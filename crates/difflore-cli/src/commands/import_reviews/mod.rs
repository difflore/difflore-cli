use difflore_core::infra::git::RepoScope;
use difflore_core::ingest::github::{ImportOptions, ImportProgress};
use difflore_core::ingest::provider::ReviewProvider;
use sqlx::SqlitePool;

use crate::cli::ImportProviderArg;
use crate::runtime::CommandContext;
use crate::style;
use crate::support::util::{ensure_project, exit_err, project_path, validate_owner_repo};

#[cfg(test)]
mod fixtures;
mod github;
mod gitlab;
mod local_candidates;
mod scope;
mod upload;

pub(crate) use github::format_github_import_err;
use github::verify_source_repo_access;
use gitlab::{run_gitlab_import, verify_gitlab_project_access};
use local_candidates::{
    LocalCandidateProgress, local_candidate_budget, print_local_candidate_next_steps,
    run_local_candidates,
};
use upload::run_upload;

/// Args bundle for `difflore import-reviews`; keeps dispatcher calls from
/// growing a long positional parameter list.
pub(crate) struct ImportArgs {
    pub repo: Option<String>,
    pub from_upstream: Option<String>,
    /// Explicit `--provider` override; `None` → detect from the git remote.
    pub provider: Option<ImportProviderArg>,
    /// Explicit `--gitlab-host` for self-managed instances; implies the
    /// gitlab provider.
    pub gitlab_host: Option<String>,
    pub max_prs: usize,
    pub pr_numbers: Vec<i32>,
    /// PR numbers to exclude from import (parsed from `--exclude-prs`). Any PR
    /// whose number is in this set contributes zero rules. Used for leak-free
    /// recall evaluation.
    pub exclude_prs: Vec<i32>,
    pub since: Option<String>,
    pub include_open: bool,
    pub upload: bool,
    pub dry_run: bool,
    pub json: bool,
}

impl From<crate::cli::ImportReviewsCliArgs> for ImportArgs {
    fn from(args: crate::cli::ImportReviewsCliArgs) -> Self {
        Self {
            repo: args.repo,
            from_upstream: args.from_upstream,
            provider: args.provider,
            gitlab_host: args.gitlab_host,
            max_prs: args.max_prs,
            pr_numbers: args.pr_numbers,
            exclude_prs: args.exclude_prs,
            since: args.since,
            include_open: args.include_open,
            upload: args.upload,
            dry_run: args.dry_run,
            json: args.json,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ImportRunOutcome {
    pub(crate) cloud_upload_queued: bool,
}

struct ValidatedArgs {
    repo: Option<String>,
    from_upstream: Option<String>,
    provider: Option<ImportProviderArg>,
    gitlab_host: Option<String>,
    max_prs: usize,
    pr_numbers: Vec<i32>,
    /// PR numbers to exclude from import, deduped into a set. Any PR whose
    /// number is in this set contributes zero rules.
    exclude_prs: std::collections::HashSet<i32>,
    since: Option<String>,
    include_open: bool,
    upload: bool,
    local_candidates: bool,
    dry_run: bool,
    json: bool,
}

/// Provider-neutral argument validation. `--repo` / `--from-upstream` shape
/// checks are deliberately NOT here: their grammar differs per provider
/// (GitHub `owner/repo` vs GitLab nested namespace paths), so they run after
/// provider resolution.
fn validate_args(args: ImportArgs) -> ValidatedArgs {
    let ImportArgs {
        repo,
        from_upstream,
        provider,
        gitlab_host,
        max_prs,
        pr_numbers,
        exclude_prs,
        since,
        include_open,
        upload,
        dry_run,
        json,
    } = args;

    let requested_max_prs = max_prs;
    let max_prs = max_prs.clamp(1, 1000);
    if !json && requested_max_prs != max_prs {
        eprintln!(
            "{} --max-prs capped at {max_prs} (requested {requested_max_prs}; valid range 1..=1000)",
            style::amber(style::sym::WARN)
        );
    }
    if let Some(s) = since.as_deref()
        && chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").is_err()
    {
        exit_err(&format!(
            "--since '{s}' is not a valid YYYY-MM-DD date (e.g. 2026-01-15)."
        ));
    }
    if pr_numbers.iter().any(|n| *n <= 0) {
        exit_err("--pr must be a positive PR number.");
    }
    if exclude_prs.iter().any(|n| *n <= 0) {
        exit_err("--exclude-prs must list positive PR numbers.");
    }
    let exclude_prs: std::collections::HashSet<i32> = exclude_prs.into_iter().collect();

    let local_candidates = !upload;

    ValidatedArgs {
        repo,
        from_upstream,
        provider,
        gitlab_host,
        max_prs,
        pr_numbers,
        exclude_prs,
        since,
        include_open,
        upload,
        local_candidates,
        dry_run,
        json,
    }
}

/// Which provider this run targets, after flags + remote detection settle.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ResolvedProvider {
    Github,
    Gitlab { host: String },
}

/// Resolve the review provider from explicit flags first, then the git
/// remote. Detection is conservative (`ingest::provider`): github.com →
/// GitHub, gitlab.com or a PAT-configured host → GitLab, any other host →
/// error demanding an explicit `--provider` instead of guessing — a wrong
/// guess would import review history from the wrong API surface.
fn resolve_provider(
    provider: Option<ImportProviderArg>,
    gitlab_host: Option<&str>,
    remote_url: Option<&str>,
    configured_gitlab_hosts: &[String],
) -> Result<ResolvedProvider, String> {
    use difflore_core::ingest::gitlab::auth as gitlab_auth;
    use difflore_core::ingest::provider as ingest_provider;

    if let Some(host_input) = gitlab_host {
        if provider == Some(ImportProviderArg::Github) {
            return Err(
                "--gitlab-host conflicts with --provider github; drop one of the two.".to_owned(),
            );
        }
        let host = gitlab_auth::normalize_gitlab_host(host_input)?;
        return Ok(ResolvedProvider::Gitlab { host });
    }

    match provider {
        Some(ImportProviderArg::Github) => Ok(ResolvedProvider::Github),
        Some(ImportProviderArg::Gitlab) => {
            // Forced GitLab without --gitlab-host: adopt the remote's host
            // when it plausibly is the instance, else default to gitlab.com.
            let host = remote_url
                .and_then(ingest_provider::remote_url_host)
                .filter(|host| host != "github.com")
                .unwrap_or_else(|| gitlab_auth::DEFAULT_GITLAB_HOST.to_owned());
            Ok(ResolvedProvider::Gitlab { host })
        }
        None => {
            // No remote (or a local-path remote): keep the long-standing
            // GitHub default — that path already reports a clear "--repo
            // required" error when nothing is detectable.
            let Some(url) = remote_url else {
                return Ok(ResolvedProvider::Github);
            };
            match ingest_provider::detect_provider_from_remote_url(url, configured_gitlab_hosts) {
                Some(ReviewProvider::Github) => Ok(ResolvedProvider::Github),
                Some(ReviewProvider::Gitlab) => {
                    let host = ingest_provider::remote_url_host(url)
                        .unwrap_or_else(|| gitlab_auth::DEFAULT_GITLAB_HOST.to_owned());
                    Ok(ResolvedProvider::Gitlab { host })
                }
                None => ingest_provider::remote_url_host(url).map_or(
                    Ok(ResolvedProvider::Github),
                    |host| {
                        Err(format!(
                            "Could not infer the review provider for remote host '{host}'.\n  \
                             Pass --provider github, or --provider gitlab --gitlab-host {host} for a self-managed GitLab.\n  \
                             Tip: `difflore auth gitlab --host {host}` stores a PAT and makes detection automatic next time."
                        ))
                    },
                ),
            }
        }
    }
}

fn resolve_local_repo(
    repo: Option<String>,
    from_upstream: Option<&str>,
    pp: &str,
) -> Result<String, String> {
    repo.or_else(|| difflore_core::ingest::github::detect_repo_from_remote(pp).ok())
        .ok_or_else(|| {
            let from_upstream_hint = if from_upstream.is_some() {
                "\n  · `--from-upstream` is set, but --repo is still required to declare the local target. \
                 Pass --repo to the same value if you want this repo to inherit the upstream's memory directly."
            } else {
                ""
            };
            format!(
                "Could not detect GitHub repo from git remote. \
                 Pass `--repo owner/repo` (the local repo to attach memory to).{from_upstream_hint}"
            )
        })
}

fn run_dry_run(v: &ValidatedArgs, local_repo: &str, source_repo: &str) {
    if v.json {
        println!(
            "{}",
            crate::support::util::json_or(&dry_run_payload(v, local_repo, source_repo), "{}")
        );
        return;
    }

    let label = if v.from_upstream.is_some() {
        format!("{source_repo} -> attach to {local_repo}")
    } else {
        local_repo.to_owned()
    };
    let open_part = if v.include_open {
        " (including open PRs)"
    } else {
        ""
    };
    style::println_wrapped(&format!(
        "{} Dry run | would import up to {} PRs from {label}{open_part}.",
        style::ok(style::sym::TIP),
        v.max_prs,
    ));
    if v.upload {
        style::println_wrapped(
            "  Would upload to cloud for extraction; `difflore cloud sync` then pulls rules down.",
        );
    }
    if v.local_candidates {
        style::println_wrapped(
            "  Would draft local rule candidates from high-signal review comments; no cloud needed.",
        );
        println!(
            "  Up to {} rule drafts would be created.",
            local_candidate_budget(v)
        );
    }
    println!(
        "  {}",
        style::pewter("(no DB writes, no network calls performed)")
    );
}

/// Deterministically order the exclude set for JSON output. The set itself is
/// unordered, so sorting keeps `--json` payloads stable for snapshot tests and
/// for an eval harness that diffs successive runs.
fn sorted_exclude_prs(exclude_prs: &std::collections::HashSet<i32>) -> Vec<i32> {
    let mut out: Vec<i32> = exclude_prs.iter().copied().collect();
    out.sort_unstable();
    out
}

fn dry_run_payload(v: &ValidatedArgs, local_repo: &str, source_repo: &str) -> serde_json::Value {
    serde_json::json!({
        "dryRun": true,
        "provider": "github",
        "repo": local_repo,
        "sourceRepo": source_repo,
        "fromUpstream": v.from_upstream.as_deref(),
        "maxPrs": v.max_prs,
        "prNumbers": v.pr_numbers,
        "excludePrs": sorted_exclude_prs(&v.exclude_prs),
        "includeOpen": v.include_open,
        "upload": v.upload,
        "localCandidates": v.local_candidates,
        "localCandidateBudget": if v.local_candidates {
            Some(local_candidate_budget(v))
        } else {
            None
        },
        "writes": false,
        "networkCalls": false,
    })
}

fn run_gitlab_dry_run(v: &ValidatedArgs, host: &str, gitlab_project: &str) {
    if v.json {
        println!(
            "{}",
            crate::support::util::json_or(&gitlab_dry_run_payload(v, host, gitlab_project), "{}")
        );
        return;
    }
    style::println_wrapped(&format!(
        "{} Dry run | would import up to {} merged MRs from {host}/{gitlab_project}.",
        style::ok(style::sym::TIP),
        v.max_prs,
    ));
    if v.upload {
        style::println_wrapped(
            "  Would upload to cloud for extraction; `difflore cloud sync` then pulls rules down.",
        );
    }
    if v.local_candidates {
        style::println_wrapped(
            "  Would draft local rule candidates from high-signal review comments; no cloud needed.",
        );
        println!(
            "  Up to {} rule drafts would be created.",
            local_candidate_budget(v)
        );
    }
    println!(
        "  {}",
        style::pewter("(no DB writes, no network calls performed)")
    );
}

fn gitlab_dry_run_payload(
    v: &ValidatedArgs,
    host: &str,
    gitlab_project: &str,
) -> serde_json::Value {
    serde_json::json!({
        "dryRun": true,
        "provider": "gitlab",
        "gitlabHost": host,
        "repo": gitlab_project,
        "sourceRepo": gitlab_project,
        "maxPrs": v.max_prs,
        "prNumbers": v.pr_numbers,
        "excludePrs": sorted_exclude_prs(&v.exclude_prs),
        "upload": v.upload,
        "localCandidates": v.local_candidates,
        "localCandidateBudget": if v.local_candidates {
            Some(local_candidate_budget(v))
        } else {
            None
        },
        "writes": false,
        "networkCalls": false,
    })
}

fn print_gitlab_import_plan(v: &ValidatedArgs, host: &str, gitlab_project: &str) {
    if v.json {
        return;
    }
    style::println_wrapped(&format!(
        "{} Import plan: scan {} from {host}/{gitlab_project}.",
        style::ok(style::sym::TIP),
        if v.pr_numbers.is_empty() {
            format!("up to {} merged MRs", v.max_prs)
        } else {
            format!(
                "MR {}",
                v.pr_numbers
                    .iter()
                    .map(|n| format!("!{n}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        },
    ));
    if !v.exclude_prs.is_empty() {
        let excluded = sorted_exclude_prs(&v.exclude_prs)
            .iter()
            .map(|n| format!("!{n}"))
            .collect::<Vec<_>>()
            .join(", ");
        style::println_wrapped(&format!(
            "  {} excluding {} (contributes zero rules)",
            style::pewter(style::sym::BULLET),
            excluded,
        ));
    }
    style::println_wrapped(&format!(
        "  {} preview only: {}",
        style::pewter(style::sym::BULLET),
        style::cmd("difflore import-reviews --dry-run"),
    ));
    style::println_wrapped(&format!(
        "  {} recovery: if GitLab throttles, retry with {} or {}.",
        style::pewter(style::sym::BULLET),
        style::cmd("--max-prs 20"),
        style::cmd("--since YYYY-MM-DD"),
    ));
}

fn print_import_plan(v: &ValidatedArgs, local_repo: &str, source_repo: &str) {
    if v.json {
        return;
    }
    let label = if v.from_upstream.is_some() {
        format!("{source_repo} -> attach to {local_repo}")
    } else {
        local_repo.to_owned()
    };
    style::println_wrapped(&format!(
        "{} Import plan: scan {} from {label}.",
        style::ok(style::sym::TIP),
        if v.pr_numbers.is_empty() {
            let pr_kind = if v.include_open {
                "merged/open PRs"
            } else {
                "merged PRs"
            };
            format!("up to {} {pr_kind}", v.max_prs)
        } else {
            format!(
                "PR {}",
                v.pr_numbers
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        },
    ));
    if !v.exclude_prs.is_empty() {
        let excluded = sorted_exclude_prs(&v.exclude_prs)
            .iter()
            .map(|n| format!("#{n}"))
            .collect::<Vec<_>>()
            .join(", ");
        style::println_wrapped(&format!(
            "  {} excluding {} (contributes zero rules)",
            style::pewter(style::sym::BULLET),
            excluded,
        ));
    }
    style::println_wrapped(&format!(
        "  {} preview only: {}",
        style::pewter(style::sym::BULLET),
        style::cmd("difflore import-reviews --dry-run"),
    ));
    style::println_wrapped(&format!(
        "  {} recovery: if GitHub throttles, retry with {} or {}.",
        style::pewter(style::sym::BULLET),
        style::cmd("--max-prs 20"),
        style::cmd("--since YYYY-MM-DD"),
    ));
}

async fn run_import(
    db: &SqlitePool,
    opts: ImportOptions,
    repo: &str,
    source_repo: &str,
    upload: bool,
    json: bool,
) -> Result<ImportProgress, String> {
    if json {
        let result = match difflore_core::ingest::github::import_pr_reviews(db, opts, None).await {
            Ok(r) => r,
            Err(e) => return Err(format_github_import_err("Import failed", &e.to_string())),
        };
        return Ok(result);
    }

    let spinner_label = format!("Importing PR reviews from {source_repo}");
    let spinner = style::Spinner::new(&spinner_label);
    let spinner_progress = spinner.handle();

    let empty_pr_kind = if opts.include_open {
        "merged/open PRs"
    } else {
        "merged PRs"
    };
    let direct_pr_mode = !opts.pr_numbers.is_empty();
    let progress_cb: Box<dyn Fn(&ImportProgress) + Send> = Box::new(move |p| {
        if p.prs_total > 0 && p.prs_fetched > 0 {
            let skipped_part = if p.comments_skipped > 0 {
                format!(" ({} skipped)", p.comments_skipped)
            } else {
                String::new()
            };
            spinner_progress.println(&format!(
                "  [{}/{}] {} comments imported{}",
                p.prs_fetched, p.prs_total, p.comments_imported, skipped_part
            ));
        } else if p.prs_total > 0 {
            spinner_progress.println(&format!(
                "  {} PRs with review activity to import",
                p.prs_total
            ));
        } else if direct_pr_mode && p.prs_missing > 0 {
            spinner_progress.println(&format!(
                "  No requested PRs with review activity found ({} missing/inaccessible).",
                p.prs_missing
            ));
        } else {
            spinner_progress.println(&format!("  No {empty_pr_kind} with review activity found."));
        }
    });

    let result =
        match difflore_core::ingest::github::import_pr_reviews(db, opts, Some(progress_cb)).await {
            Ok(r) => r,
            Err(e) => {
                spinner.finish_err("Import failed");
                return Err(format_github_import_err("Import failed", &e.to_string()));
            }
        };

    spinner.finish_ok(&format!(
        "Imported {} PRs from {}",
        result.prs_fetched, source_repo,
    ));
    if source_repo != repo {
        println!("  attached to local repo: {}", style::pewter(repo));
    }
    println!("  review comments:        {}", result.comments_imported);
    if result.comments_skipped > 0 {
        println!("  skipped:                {}", result.comments_skipped);
    }
    if result.prs_missing > 0 {
        let missing = result
            .missing_pr_numbers
            .iter()
            .map(|n| format!("#{n}"))
            .collect::<Vec<_>>()
            .join(", ");
        println!("  missing PRs:            {missing}");
    }
    // Phrase as "requested": upload runs after this summary, so a later
    // failure must not contradict an earlier "uploaded: yes".
    println!(
        "  upload requested:       {}",
        if upload { "yes" } else { "no" }
    );
    println!();
    if upload {
        println!(
            "  {} Uploading imported comments for extraction...",
            style::emerald(style::sym::TIP),
        );
    } else if result.comments_imported > 0 {
        println!(
            "  {} Imports stayed local.",
            style::emerald(style::sym::TIP),
        );
        style::println_wrapped("    Drafting review candidates from high-signal comments...");
    }
    Ok(result)
}

#[allow(clippy::too_many_arguments)]
// reason: flat JSON-reporting glue; bundling into a struct would only move the field list.
fn print_import_json(
    provider: &str,
    gitlab_host: Option<&str>,
    repo: &str,
    source_repo: &str,
    result: &ImportProgress,
    local_candidates: Option<&LocalCandidateProgress>,
    uploaded_reviews: usize,
) {
    let payload = import_json_payload(
        provider,
        gitlab_host,
        repo,
        source_repo,
        result,
        local_candidates,
        uploaded_reviews,
    );
    println!("{}", crate::support::util::json_or(&payload, "{}"));
}

#[allow(clippy::too_many_arguments)]
// reason: flat JSON-reporting glue; bundling into a struct would only move the field list.
fn import_json_payload(
    provider: &str,
    gitlab_host: Option<&str>,
    repo: &str,
    source_repo: &str,
    result: &ImportProgress,
    local_candidates: Option<&LocalCandidateProgress>,
    uploaded_reviews: usize,
) -> serde_json::Value {
    serde_json::json!({
        "provider": provider,
        "gitlabHost": gitlab_host,
        "repo": repo,
        "sourceRepo": source_repo,
        "prsFetched": result.prs_fetched,
        "prsTotal": result.prs_total,
        "commentsImported": result.comments_imported,
        "commentsSkipped": result.comments_skipped,
        "prsMissing": result.prs_missing,
        "missingPrNumbers": &result.missing_pr_numbers,
        "uploadedReviews": uploaded_reviews,
        "cloudUploadQueued": uploaded_reviews > 0,
        "localCandidates": local_candidates.map(|p| serde_json::json!({
            "commentsConsidered": p.comments_considered,
            "candidatesCreated": p.candidates_created,
            "candidatesActivated": p.candidates_activated,
            "candidatesPending": p.candidates_pending,
            "candidatesDeduped": p.candidates_deduped,
            "candidateBudget": p.budget,
            "commentsSkipped": p.comments_skipped,
            "capped": p.capped,
        })),
    })
}

pub(crate) async fn handle(ctx: &CommandContext, args: ImportArgs) {
    if let Err(e) = try_handle(ctx, args).await {
        exit_err(&e);
    }
}

pub(crate) async fn try_handle(
    ctx: &CommandContext,
    args: ImportArgs,
) -> Result<ImportRunOutcome, String> {
    let v = validate_args(args);

    let pp = project_path();
    let remote_url = difflore_core::ingest::provider::git_remote_origin_url(&pp);
    // PAT-configured self-managed hosts only matter for auto-detection, so
    // skip the auth-db read when an explicit flag already decides.
    let configured_hosts = if v.provider.is_none() && v.gitlab_host.is_none() {
        difflore_core::ingest::gitlab::auth::configured_hosts().await
    } else {
        Vec::new()
    };
    let provider = resolve_provider(
        v.provider,
        v.gitlab_host.as_deref(),
        remote_url.as_deref(),
        &configured_hosts,
    )?;

    match provider {
        ResolvedProvider::Github => try_handle_github(ctx, &pp, v).await,
        ResolvedProvider::Gitlab { host } => {
            try_handle_gitlab(ctx, &pp, remote_url.as_deref(), &host, v).await
        }
    }
}

async fn try_handle_github(
    ctx: &CommandContext,
    pp: &str,
    v: ValidatedArgs,
) -> Result<ImportRunOutcome, String> {
    if let Some(r) = v.repo.as_deref()
        && let Err(msg) = validate_owner_repo(r)
    {
        return Err(format!("--repo '{r}' is invalid: {msg}"));
    }
    if let Some(r) = v.from_upstream.as_deref()
        && let Err(msg) = validate_owner_repo(r)
    {
        return Err(format!("--from-upstream '{r}' is invalid: {msg}"));
    }

    let db = &ctx.db;
    let project = ensure_project(db, pp).await;

    let local_repo = resolve_local_repo(v.repo.clone(), v.from_upstream.as_deref(), pp)?;
    let source_repo = v
        .from_upstream
        .clone()
        .unwrap_or_else(|| local_repo.clone());
    let source_repo_scope = RepoScope::github(&source_repo)
        .ok_or_else(|| format!("--from-upstream '{source_repo}' is invalid for GitHub"))?;

    if v.dry_run {
        run_dry_run(&v, &local_repo, &source_repo);
        return Ok(ImportRunOutcome::default());
    }

    print_import_plan(&v, &local_repo, &source_repo);

    if let Err(e) = verify_source_repo_access(&source_repo) {
        return Err(format_github_import_err("Import failed", &e));
    }

    let opts = ImportOptions {
        repo: local_repo.clone(),
        source_repo: source_repo.clone(),
        project_id: project.id,
        max_prs: v.max_prs,
        pr_numbers: v.pr_numbers.clone(),
        exclude_prs: v.exclude_prs.clone(),
        since: v.since.clone(),
        upload_to_cloud: v.upload,
        include_open: v.include_open,
    };

    let import_result = run_import(db, opts, &local_repo, &source_repo, v.upload, v.json).await?;

    let local_candidate_progress = if v.local_candidates {
        // Budget scales with import scope so bulk imports don't silently drop
        // most high-signal review evidence.
        let budget = local_candidate_budget(&v);
        let progress = run_local_candidates(
            db,
            "github",
            &local_repo,
            &source_repo_scope,
            budget,
            &v.pr_numbers,
            &v.exclude_prs,
        )
        .await;
        if !v.json {
            print_local_candidate_next_steps(&progress);
        }
        Some(progress)
    } else {
        None
    };

    let uploaded_reviews = if v.upload {
        run_upload(ctx, db, "github", &local_repo, None, &import_result, v.json).await?
    } else {
        0
    };

    if v.json {
        print_import_json(
            "github",
            None,
            &local_repo,
            &source_repo,
            &import_result,
            local_candidate_progress.as_ref(),
            uploaded_reviews,
        );
    }
    Ok(ImportRunOutcome {
        cloud_upload_queued: uploaded_reviews > 0,
    })
}

async fn try_handle_gitlab(
    ctx: &CommandContext,
    pp: &str,
    remote_url: Option<&str>,
    host: &str,
    v: ValidatedArgs,
) -> Result<ImportRunOutcome, String> {
    // Flags whose semantics don't exist in the GitLab v1 importer fail loud
    // instead of being silently ignored.
    if v.from_upstream.is_some() {
        return Err(
            "--from-upstream is GitHub-only for now; GitLab imports read the project directly \
             (pass --repo group/project)."
                .to_owned(),
        );
    }
    if v.include_open {
        return Err(
            "--include-open is not supported for GitLab yet; the GitLab importer reads merged \
             MRs only."
                .to_owned(),
        );
    }

    let gitlab_project = resolve_gitlab_project_path(v.repo.clone(), remote_url, host)?;
    difflore_core::ingest::provider::validate_gitlab_project_path(&gitlab_project)
        .map_err(|msg| format!("--repo is invalid for GitLab: {msg}"))?;
    let gitlab_repo_scope = RepoScope::gitlab(host, &gitlab_project)
        .ok_or_else(|| format!("--repo is invalid for GitLab: {gitlab_project}"))?;

    if v.dry_run {
        run_gitlab_dry_run(&v, host, &gitlab_project);
        return Ok(ImportRunOutcome::default());
    }

    print_gitlab_import_plan(&v, host, &gitlab_project);

    let Some((token, _source)) = difflore_core::ingest::gitlab::auth::resolve_token(host).await
    else {
        return Err(missing_gitlab_token_error(host));
    };

    verify_gitlab_project_access(host, &token, &gitlab_project).await?;

    let db = &ctx.db;
    let project = ensure_project(db, pp).await;
    let opts = difflore_core::ingest::gitlab::ImportOptions {
        host: host.to_owned(),
        project_path: gitlab_project.clone(),
        project_id: project.id,
        token,
        max_mrs: v.max_prs,
        // `--pr` carries MR IIDs in the GitLab context (flag reuse by design).
        mr_iids: v.pr_numbers.clone(),
        exclude_mrs: v.exclude_prs.clone(),
        since: v.since.clone(),
    };

    let import_result = run_gitlab_import(db, opts, v.upload, v.json).await?;

    let local_candidate_progress = if v.local_candidates {
        let budget = local_candidate_budget(&v);
        let progress = run_local_candidates(
            db,
            "gitlab",
            &gitlab_project,
            &gitlab_repo_scope,
            budget,
            &v.pr_numbers,
            &v.exclude_prs,
        )
        .await;
        if !v.json {
            print_local_candidate_next_steps(&progress);
        }
        Some(progress)
    } else {
        None
    };

    let uploaded_reviews = if v.upload {
        run_upload(
            ctx,
            db,
            "gitlab",
            &gitlab_project,
            Some(host),
            &import_result,
            v.json,
        )
        .await?
    } else {
        0
    };

    if v.json {
        print_import_json(
            "gitlab",
            Some(host),
            &gitlab_project,
            gitlab_repo_scope.as_str(),
            &import_result,
            local_candidate_progress.as_ref(),
            uploaded_reviews,
        );
    }
    Ok(ImportRunOutcome {
        cloud_upload_queued: uploaded_reviews > 0,
    })
}

/// `--repo` wins; otherwise the namespace path comes from the git remote,
/// but only when the remote actually points at the resolved host — silently
/// importing `group/project` from the wrong instance would be worse than
/// asking for the flag.
fn resolve_gitlab_project_path(
    repo: Option<String>,
    remote_url: Option<&str>,
    host: &str,
) -> Result<String, String> {
    if let Some(repo) = repo {
        return Ok(repo);
    }
    remote_url
        .and_then(difflore_core::ingest::provider::remote_url_host_and_path)
        .filter(|(remote_host, _)| remote_host.eq_ignore_ascii_case(host))
        .map(|(_, path)| path)
        .filter(|path| !path.is_empty())
        .ok_or_else(|| {
            format!(
                "Could not detect the GitLab project path from the git remote for {host}.\n  \
                 Pass `--repo group/project` (subgroups allowed, e.g. group/subgroup/project)."
            )
        })
}

fn missing_gitlab_token_error(host: &str) -> String {
    format!(
        "No GitLab token found for {host}.\n  \
         Store one first: echo \"<TOKEN>\" | difflore auth gitlab --host {host}\n  \
         Or set DIFFLORE_GITLAB_TOKEN / GITLAB_TOKEN in this shell.\n  \
         Mint a PAT with the read_api scope at https://{host}/-/user_settings/personal_access_tokens"
    )
}

#[cfg(test)]
#[allow(unsafe_code)] // reason: test home is pinned once so remember_as_candidate never writes to the user's real home.
mod tests {

    use std::collections::HashSet;

    use crate::support::review_text::strip_review_markdown_noise;
    use difflore_core::infra::git::RepoScope;
    use difflore_core::ingest::github::ImportProgress;

    use super::fixtures::{
        fresh_import_pool, imported_item, review, seed_gitlab_pr_with_directive,
        seed_imported_review_comments, seed_imported_review_comments_with_resolution,
        seed_pr_with_directive,
    };
    use super::github::{format_github_import_err, gh_repo_view_failure_detail};
    use super::gitlab::format_gitlab_import_err;
    use super::local_candidates::{
        CAPTURE_CONFIDENCE_HIGH, CAPTURE_CONFIDENCE_LOW, CaptureRoute, candidate_title,
        clean_review_comment, distilled_rule_statement, is_high_signal_review_comment_for_paths,
        local_candidate_budget, local_candidate_budget_reached, local_candidate_input,
        local_candidate_next_step_commands, pending_drafts_review_hint, route_for_confidence,
        run_local_candidates,
    };
    use super::scope::file_pattern_from_path;
    use super::upload::{
        build_upload_batches, cloud_upload_next_step_commands, comment_event_type,
        comment_file_path_from_metadata, gitlab_host_from_metadata, imported_review_upload,
        matches_upload_target,
    };
    use super::{ImportArgs, dry_run_payload, import_json_payload, validate_args};

    fn github_scope(repo: &str) -> RepoScope {
        RepoScope::github(repo).expect("test GitHub repo scope")
    }

    fn gitlab_scope(host: &str, repo: &str) -> RepoScope {
        RepoScope::gitlab(host, repo).expect("test GitLab repo scope")
    }

    #[test]
    fn strip_review_markdown_noise_drops_severity_banners_and_emphasis() {
        let raw = "_⚠️ Potential issue_ | _🟡 Minor_ Wait for the async submit \
                   path before asserting state.";
        let out = strip_review_markdown_noise(raw);
        assert!(!out.contains('_'), "underscores remain: {out}");
        assert!(!out.contains('⚠'), "emoji remain: {out}");
        assert!(
            !out.to_ascii_lowercase().contains("potential issue"),
            "banner: {out}"
        );
        assert!(
            !out.to_ascii_lowercase().contains("minor"),
            "severity: {out}"
        );
        assert!(out.starts_with("Wait for the async submit"), "got: {out}");
    }

    #[test]
    fn strip_review_markdown_noise_keeps_real_prose() {
        let raw = "**Use** `errors.Is` rather than `==` when comparing wrapped errors.";
        let out = strip_review_markdown_noise(raw);
        assert!(out.contains("Use"));
        assert!(out.contains("errors.Is"));
        assert!(!out.contains('*'));
    }

    #[test]
    fn clean_review_comment_strips_coderabbit_summary_wrappers() {
        let raw = "<details>\n<summary>Actionable comments posted: 3</summary>\n\n\
                   _⚠️ Potential issue_ | _🟡 Minor_\n\n\
                   Wait for the async submit path before asserting state.\n\
                   </details>";
        let out = clean_review_comment(raw);
        assert!(!out.contains("details"), "html residue: {out}");
        assert!(!out.contains("Actionable"), "banner: {out}");
        assert!(!out.contains('_'), "emphasis: {out}");
        assert!(out.starts_with("Wait for the async submit"), "got: {out}");
    }

    #[test]
    fn clean_review_comment_strips_outside_diff_platform_warning_lines() {
        let raw = "[!CAUTION]\n\
                   Some comments are outside the diff and cannot be posted inline.\n\
                   Outside diff range comments (14)\n\
                   Prefer checking the parsed header before indexing into it.";
        let out = clean_review_comment(raw);

        assert!(!out.contains("[!CAUTION]"), "caution residue: {out}");
        assert!(!out.contains("outside the diff"), "platform residue: {out}");
        assert!(
            out.starts_with("Prefer checking the parsed header"),
            "got: {out}"
        );
    }

    #[test]
    fn candidate_title_uses_clean_first_sentence() {
        let raw = "_⚠️ Potential issue_ | _🟡 Minor_ Wait for the async submit \
                   path before asserting state. The current code races.";
        let title = candidate_title(raw, "form-core/src/index.ts");
        assert!(
            title.starts_with("Review: Wait for the async submit"),
            "got: {title}"
        );
        assert!(!title.contains('⚠'));
        assert!(!title.contains('_'));
    }

    #[test]
    fn candidate_title_normalizes_review_chatter_for_dedup() {
        let a = candidate_title(
            "Please prefer Mapping[str, str] here instead of dict[str, str]. It keeps callers flexible.",
            "src/http/headers.py",
        );
        let b = candidate_title(
            "We should prefer Mapping[str, str] here instead of dict[str, str]. It keeps callers flexible.",
            "src/http/headers.py",
        );

        assert_eq!(
            a,
            "Review: Prefer Mapping[str, str] here instead of dict[str, str]"
        );
        assert_eq!(a, b);
    }

    #[test]
    fn format_github_import_err_classifies_known_and_falls_through_unknown() {
        // (raw, must-contain): one row per branch in format_github_import_err.
        let cases: &[(&str, &str)] = &[
            ("GitHub CLI (gh) is not installed", "cli.github.com"),
            (
                "gh api graphql error: HTTP 401: Bad credentials",
                "auth missing or expired",
            ),
            (
                "GraphQL errors: Could not resolve to a Repository with the name 'foo/bar'.",
                "gh repo view",
            ),
            (
                "GraphQL errors: Resource not accessible by personal access token",
                "gh auth refresh",
            ),
            (
                "gh api graphql error: API rate limit exceeded",
                "rate limit",
            ),
        ];
        for (raw, expect) in cases {
            let out = format_github_import_err("Import failed", raw);
            assert!(
                out.contains(expect),
                "want {expect:?} for {raw:?}, got: {out}"
            );
        }

        let rate_limited =
            format_github_import_err("Import failed", "gh api graphql error: rate limit exceeded");
        assert!(rate_limited.contains("--max-prs 20"));
        assert!(rate_limited.contains("--dry-run"));

        // All branches except the trivially-actionable "gh not installed" one
        // must retain the raw stderr at the tail. The actionable framing is
        // the prefix; raw is the suffix that keeps bug reports debuggable.
        let raw_required: &[&str] = &[
            "gh api graphql error: HTTP 401: Bad credentials",
            "GraphQL errors: Could not resolve to a Repository with the name 'foo/bar'.",
            "GraphQL errors: Resource not accessible by personal access token",
            "gh api graphql error: API rate limit exceeded",
            "request failed: connection refused",
            "request timed out after 30s",
        ];
        for raw in raw_required {
            let out = format_github_import_err("Import failed", raw);
            assert!(
                out.contains(raw) && out.contains("raw:"),
                "raw input {raw:?} not retained at tail in: {out}"
            );
        }

        // Unknown errors fall through verbatim — never silently swallowed.
        assert_eq!(
            format_github_import_err("Import failed", "novel github error xyz"),
            "Import failed: novel github error xyz"
        );
    }

    #[test]
    fn format_gitlab_import_err_classifies_statuses_with_actionable_recovery() {
        let host = "gitlab.corp.example";
        // (raw, must-contain): one row per branch in format_gitlab_import_err.
        let cases: &[(&str, &str)] = &[
            (
                "GitLab API error: HTTP 401 Unauthorized for GET /api/v4/projects/g%2Fp",
                "read_api",
            ),
            (
                "GitLab API error: HTTP 404 Not Found for GET /api/v4/projects/g%2Fp",
                "404 — not 403",
            ),
            (
                "GitLab API error: HTTP 403 Forbidden for GET /api/v4/projects/g%2Fp",
                "IP allowlists",
            ),
            (
                "GitLab API error: HTTP 429 Too Many Requests for GET /api/v4/projects/g%2Fp",
                "--max-prs 20",
            ),
            (
                "GitLab API error: HTTP 503 Service Unavailable for GET /api/v4/projects/g%2Fp",
                "rerun the same command",
            ),
            (
                "GitLab API request failed for GET /api/v4/projects/g%2Fp: invalid peer certificate",
                "trusted at the OS level",
            ),
            (
                "GitLab API request failed for GET /api/v4/projects/g%2Fp: operation timed out",
                "VPN/proxy",
            ),
            (
                "GitLab API request failed for GET /api/v4/projects/g%2Fp: dns error: failed to lookup address",
                "--gitlab-host",
            ),
            // Windows connection reset, rendered through the reqwest source
            // chain (observed live against an unreachable host).
            (
                "GitLab API request failed for GET /api/v4/projects/g%2Fp: error sending request for url (https://h/): client error (Connect): An existing connection was forcibly closed by the remote host. (os error 10054)",
                "--gitlab-host",
            ),
        ];
        for (raw, expect) in cases {
            let out = format_gitlab_import_err("Import failed", host, raw);
            assert!(
                out.contains(expect),
                "want {expect:?} for {raw:?}, got: {out}"
            );
            assert!(
                out.contains(raw) && out.contains("raw:"),
                "raw input {raw:?} not retained at tail in: {out}"
            );
        }

        // Auth-shaped errors must point at the instance's PAT settings page.
        let unauthorized = format_gitlab_import_err(
            "Import failed",
            host,
            "GitLab API error: HTTP 401 Unauthorized for GET /api/v4/projects/g%2Fp",
        );
        assert!(unauthorized.contains(&format!(
            "https://{host}/-/user_settings/personal_access_tokens"
        )));
        assert!(unauthorized.contains(&format!("difflore auth gitlab --host {host}")));

        // The 404 path must explain the GitLab quirk (no-access private
        // projects answer 404, not 403) so users check BOTH path and scope.
        let missing = format_gitlab_import_err(
            "Import failed",
            host,
            "GitLab API error: HTTP 404 Not Found for GET /api/v4/projects/g%2Fp",
        );
        assert!(missing.contains("wrong project path or a permission gap"));

        // Unknown errors fall through to the generic core mapper, never
        // silently swallowed.
        assert_eq!(
            format_gitlab_import_err("Import failed", host, "novel gitlab error xyz"),
            "Import failed: novel gitlab error xyz"
        );
    }

    #[test]
    fn provider_resolution_prefers_flags_then_remote_then_errors_on_unknown_hosts() {
        use super::{ImportProviderArg, ResolvedProvider, resolve_provider};

        let gitlab = |host: &str| ResolvedProvider::Gitlab {
            host: host.to_owned(),
        };

        // Explicit --gitlab-host wins and implies the gitlab provider.
        assert_eq!(
            resolve_provider(
                None,
                Some("GitLab.Corp.Example"),
                Some("https://github.com/o/r.git"),
                &[],
            ),
            Ok(gitlab("gitlab.corp.example"))
        );
        // ...but conflicts with an explicit github provider.
        assert!(
            resolve_provider(
                Some(ImportProviderArg::Github),
                Some("gitlab.corp.example"),
                None,
                &[],
            )
            .is_err()
        );

        // Explicit provider flags decide without a remote.
        assert_eq!(
            resolve_provider(Some(ImportProviderArg::Github), None, None, &[]),
            Ok(ResolvedProvider::Github)
        );
        assert_eq!(
            resolve_provider(Some(ImportProviderArg::Gitlab), None, None, &[]),
            Ok(gitlab("gitlab.com"))
        );
        // Forced gitlab adopts a plausible remote host...
        assert_eq!(
            resolve_provider(
                Some(ImportProviderArg::Gitlab),
                None,
                Some("git@gitlab.corp.example:group/project.git"),
                &[],
            ),
            Ok(gitlab("gitlab.corp.example"))
        );
        // ...but never adopts github.com as a GitLab instance.
        assert_eq!(
            resolve_provider(
                Some(ImportProviderArg::Gitlab),
                None,
                Some("https://github.com/o/r.git"),
                &[],
            ),
            Ok(gitlab("gitlab.com"))
        );

        // Auto-detection: known hosts map to their provider.
        assert_eq!(
            resolve_provider(None, None, Some("git@github.com:o/r.git"), &[]),
            Ok(ResolvedProvider::Github)
        );
        assert_eq!(
            resolve_provider(None, None, Some("https://gitlab.com/g/p.git"), &[]),
            Ok(gitlab("gitlab.com"))
        );
        // PAT-configured self-managed hosts detect automatically.
        assert_eq!(
            resolve_provider(
                None,
                None,
                Some("https://gitlab.corp.example/g/p.git"),
                &["gitlab.corp.example".to_owned()],
            ),
            Ok(gitlab("gitlab.corp.example"))
        );

        // Unknown host: refuse to guess, demand an explicit provider.
        let err = resolve_provider(None, None, Some("https://gitea.corp.example/o/r.git"), &[])
            .expect_err("unknown host must not be guessed");
        assert!(err.contains("--provider gitlab --gitlab-host gitea.corp.example"));
        assert!(err.contains("--provider github"));

        // No remote / local-path remote: legacy GitHub default.
        assert_eq!(
            resolve_provider(None, None, None, &[]),
            Ok(ResolvedProvider::Github)
        );
        assert_eq!(
            resolve_provider(None, None, Some("/srv/git/repo.git"), &[]),
            Ok(ResolvedProvider::Github)
        );
    }

    #[test]
    fn gitlab_project_path_comes_from_flag_or_matching_remote_only() {
        use super::resolve_gitlab_project_path;

        // --repo wins unconditionally.
        assert_eq!(
            resolve_gitlab_project_path(
                Some("group/sub/project".to_owned()),
                Some("https://elsewhere.example/x/y.git"),
                "gitlab.com",
            ),
            Ok("group/sub/project".to_owned())
        );
        // Remote path is used when the remote host matches the instance.
        assert_eq!(
            resolve_gitlab_project_path(
                None,
                Some("git@gitlab.com:group/sub/project.git"),
                "gitlab.com",
            ),
            Ok("group/sub/project".to_owned())
        );
        // Host mismatch → require the flag instead of importing from the
        // wrong instance.
        let err = resolve_gitlab_project_path(
            None,
            Some("git@gitlab.com:group/project.git"),
            "gitlab.corp.example",
        )
        .expect_err("mismatched remote host must not leak a project path");
        assert!(err.contains("--repo group/project"));
        assert!(
            resolve_gitlab_project_path(None, None, "gitlab.com").is_err(),
            "no remote and no flag must error"
        );
    }

    #[test]
    fn gh_repo_view_failure_detail_ignores_stdout_warnings() {
        let detail = gh_repo_view_failure_detail(
            "acme/widgets",
            "exit status: 1",
            b"warning: extension update available\n{\"nameWithOwner\":\"acme/widgets\"}\n",
            b"GraphQL: Could not resolve to a Repository with the name 'acme/widgets'.\n",
        );

        assert_eq!(
            detail,
            "GraphQL: Could not resolve to a Repository with the name 'acme/widgets'."
        );

        let fallback = gh_repo_view_failure_detail(
            "acme/widgets",
            "exit status: 1",
            b"warning: extension update available\n",
            b"",
        );
        assert_eq!(
            fallback,
            "gh repo view acme/widgets failed with status exit status: 1"
        );
    }

    #[test]
    fn upload_batches_split_large_reviews_by_comment_count() {
        let batches = build_upload_batches(&[review(1, 181)]);
        let counts: Vec<usize> = batches
            .iter()
            .flat_map(|batch| batch.iter().map(|r| r.comments.len()))
            .collect();
        assert_eq!(counts, vec![20, 20, 20, 20, 20, 20, 20, 20, 20, 1]);
    }

    #[test]
    fn upload_batches_keep_small_reviews_under_batch_limits() {
        let reviews = (1..=25).map(|pr| review(pr, 1)).collect::<Vec<_>>();
        let batches = build_upload_batches(&reviews);
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].len(), 20);
        assert_eq!(batches[1].len(), 5);
    }

    #[test]
    fn import_upload_payload_attaches_to_local_repo_and_keeps_upstream_source() {
        let item = imported_item(
            Some("user/fork"),
            Some(r#"{"sourceRepoFullName":"upstream/project","attachedRepoFullName":"user/fork"}"#),
        );

        let upload = imported_review_upload(&item).expect("review with comments should upload");

        assert_eq!(upload.repo_full_name, "user/fork");
        assert_eq!(
            upload.source_repo_full_name.as_deref(),
            Some("upstream/project")
        );
        assert_eq!(upload.pr_number, 7);
        assert_eq!(upload.comments.len(), 1);
    }

    #[test]
    fn import_upload_payload_preserves_gitlab_provider_and_host() {
        let mut item = imported_item(
            Some("group/sub/project"),
            Some(
                r#"{"gitlabHost":"GitLab.Corp.Example","sourceRepoFullName":"group/sub/project"}"#,
            ),
        );
        item.item.source = "gitlab".to_owned();

        let upload = imported_review_upload(&item).expect("review with comments should upload");

        assert_eq!(upload.provider.as_deref(), Some("gitlab"));
        assert_eq!(upload.provider_host.as_deref(), Some("gitlab.corp.example"));
        assert_eq!(
            upload.repo_full_name,
            "gitlab.corp.example/group/sub/project"
        );
        assert_eq!(upload.source_repo_full_name, None);
        assert_eq!(
            gitlab_host_from_metadata(Some(r#"{"gitlabHost":"https://gitlab.corp.example/"}"#))
                .as_deref(),
            Some("gitlab.corp.example")
        );
        assert_eq!(
            gitlab_host_from_metadata(Some(r#"{"gitlabHost":"https://gitlab.corp.example/g/p"}"#)),
            None
        );
    }

    #[test]
    fn upload_target_filters_gitlab_reviews_by_host_dimension() {
        let mut corp_item = imported_item(
            Some("group/project"),
            Some(r#"{"gitlabHost":"gitlab.corp.example"}"#),
        );
        corp_item.item.source = "gitlab".to_owned();
        let mut dotcom_item = imported_item(
            Some("group/project"),
            Some(r#"{"gitlabHost":"gitlab.com"}"#),
        );
        dotcom_item.item.source = "gitlab".to_owned();

        assert!(matches_upload_target(
            &corp_item,
            "gitlab",
            "group/project",
            Some("gitlab.corp.example"),
        ));
        assert!(!matches_upload_target(
            &dotcom_item,
            "gitlab",
            "group/project",
            Some("gitlab.corp.example"),
        ));
        assert!(matches_upload_target(
            &dotcom_item,
            "gitlab",
            "group/project",
            Some("https://gitlab.com/"),
        ));
        assert!(
            !matches_upload_target(&corp_item, "gitlab", "group/project", None),
            "GitLab upload filtering must fail closed without the requested host"
        );
    }

    #[test]
    fn import_upload_payload_does_not_invent_source_repo_without_metadata() {
        let item = imported_item(Some("user/fork"), None);

        let upload = imported_review_upload(&item).expect("review with comments should upload");

        assert_eq!(upload.repo_full_name, "user/fork");
        assert_eq!(upload.source_repo_full_name, None);
    }

    #[test]
    fn import_upload_payload_canonicalizes_gitlab_source_repo() {
        let mut item = imported_item(
            Some("user/fork"),
            Some(r#"{"gitlabHost":"gitlab.com","sourceRepoFullName":"upstream/project"}"#),
        );
        item.item.source = "gitlab".to_owned();

        let upload = imported_review_upload(&item).expect("review with comments should upload");

        assert_eq!(upload.repo_full_name, "gitlab.com/user/fork");
        assert_eq!(
            upload.source_repo_full_name.as_deref(),
            Some("gitlab.com/upstream/project")
        );
    }

    #[test]
    fn import_upload_payload_preserves_inline_comment_file_path() {
        let mut item = imported_item(Some("user/fork"), None);
        item.comments[0].metadata = Some(
            r#"{"filePath":"src/http/request.rs","sourceRepoFullName":"user/fork","attachedRepoFullName":"user/fork","resolved":true}"#
                .to_owned(),
        );

        let upload = imported_review_upload(&item).expect("review with comments should upload");

        assert_eq!(
            upload.comments[0].file_path.as_deref(),
            Some("src/http/request.rs"),
            "the inline comment's path must flow into the cloud payload so extraction can derive file_patterns"
        );
    }

    #[test]
    fn import_upload_payload_leaves_file_path_none_without_a_recorded_path() {
        // Legacy rows / top-level review bodies carry no per-comment metadata.
        let item = imported_item(Some("user/fork"), None);
        let upload = imported_review_upload(&item).expect("review with comments should upload");
        assert_eq!(upload.comments[0].file_path, None);

        // Explicit null, blank, and unparseable metadata all degrade to None
        // rather than erroring or uploading an empty-string path.
        assert_eq!(
            comment_file_path_from_metadata(Some(r#"{"filePath":null}"#)),
            None
        );
        assert_eq!(
            comment_file_path_from_metadata(Some(r#"{"filePath":"  "}"#)),
            None
        );
        assert_eq!(comment_file_path_from_metadata(Some("not json")), None);
        assert_eq!(comment_file_path_from_metadata(None), None);
    }

    #[test]
    fn import_upload_payload_labels_inline_comments_as_review_comments() {
        use difflore_core::contract::ImportedCommentEventType;

        let mut item = imported_item(Some("user/fork"), None);
        item.comments[0].metadata = Some(
            r#"{"filePath":"src/http/request.rs","sourceRepoFullName":"user/fork","attachedRepoFullName":"user/fork"}"#
                .to_owned(),
        );

        let upload = imported_review_upload(&item).expect("review with comments should upload");

        assert_eq!(
            upload.comments[0].event_type,
            Some(ImportedCommentEventType::PullRequestReviewComment),
            "a recorded filePath means the comment was inline on a diff"
        );
    }

    #[test]
    fn import_upload_payload_labels_discussion_comments_as_issue_comments() {
        use difflore_core::contract::ImportedCommentEventType;

        // GitHub PR discussion comments are anchored to the first changed
        // file at import time, so they carry BOTH `sourceKind: issue_comment`
        // and a borrowed filePath. The sourceKind must win, or the cloud
        // would label them as inline review comments.
        let mut item = imported_item(Some("user/fork"), None);
        item.comments[0].metadata = Some(
            r#"{"filePath":"src/lib.rs","sourceRepoFullName":"user/fork","attachedRepoFullName":"user/fork","sourceKind":"issue_comment"}"#
                .to_owned(),
        );
        item.comments[0].comment_url =
            Some("https://github.com/user/fork/pull/7#issuecomment-99".to_owned());

        let upload = imported_review_upload(&item).expect("review with comments should upload");
        assert_eq!(
            upload.comments[0].event_type,
            Some(ImportedCommentEventType::IssueComment),
            "sourceKind=issue_comment outranks the anchored filePath"
        );
        assert_eq!(
            upload.comments[0].file_path.as_deref(),
            Some("src/lib.rs"),
            "the anchored path still flows into the payload for file-pattern extraction"
        );

        // GitLab MR-level discussion notes carry `sourceKind: mr_comment`
        // and map to the same webhook bucket.
        assert_eq!(
            comment_event_type(
                Some(r#"{"filePath":null,"sourceKind":"mr_comment"}"#),
                None,
                "https://gitlab.com/group/project/-/merge_requests/4#note_200",
            ),
            Some(ImportedCommentEventType::IssueComment),
        );
    }

    #[test]
    fn import_upload_payload_labels_review_bodies_as_pull_request_review() {
        use difflore_core::contract::ImportedCommentEventType;

        // Top-level review bodies carry no filePath/sourceKind metadata
        // (durability signal only, or nothing) — the GitHub URL fragment is
        // the local marker.
        let mut item = imported_item(Some("user/fork"), None);
        item.comments[0].metadata = Some(r#"{"resolved":true,"reactionsTotal":1}"#.to_owned());
        item.comments[0].comment_url =
            Some("https://github.com/user/fork/pull/7#pullrequestreview-42".to_owned());
        item.comments[0].line_number = None;

        let upload = imported_review_upload(&item).expect("review with comments should upload");
        assert_eq!(
            upload.comments[0].event_type,
            Some(ImportedCommentEventType::PullRequestReview),
        );
    }

    #[test]
    fn comment_event_type_is_omitted_when_locally_unknown() {
        use difflore_core::contract::ImportedCommentEventType;

        // No metadata + unrecognized URL: stay silent so the cloud's own
        // derivation (which sees the same inputs) decides — explicit wrong
        // labels would override it.
        assert_eq!(
            comment_event_type(
                None,
                None,
                "https://gitlab.example/p/-/merge_requests/4#note_7"
            ),
            None,
        );
        assert_eq!(comment_event_type(Some("not json"), None, ""), None);
        // Legacy inline rows that lost filePath metadata are still recognized
        // by their GitHub fragment, mirroring the cloud's derivation.
        assert_eq!(
            comment_event_type(None, None, "https://github.com/u/r/pull/7#discussion_r100"),
            Some(ImportedCommentEventType::PullRequestReviewComment),
        );

        // The optional field must vanish from the wire payload (not serialize
        // as null) so pre-eventType cloud deployments see an unchanged shape,
        // and the serialized values must match the cloud zod enum exactly.
        let mut wire = review(7, 1);
        wire.comments[0].event_type = None;
        let json = serde_json::to_string(&wire).expect("serialize upload payload");
        assert!(!json.contains("eventType"), "omitted, got: {json}");

        wire.comments[0].event_type = Some(ImportedCommentEventType::IssueComment);
        let json = serde_json::to_string(&wire).expect("serialize upload payload");
        assert!(
            json.contains(r#""eventType":"issue_comment""#),
            "got: {json}"
        );
        assert_eq!(
            serde_json::to_string(&ImportedCommentEventType::PullRequestReviewComment).unwrap(),
            r#""pull_request_review_comment""#
        );
        assert_eq!(
            serde_json::to_string(&ImportedCommentEventType::PullRequestReview).unwrap(),
            r#""pull_request_review""#
        );
    }

    #[test]
    fn local_candidate_gate_keeps_review_rules_and_skips_chatter() {
        assert!(is_high_signal_review_comment_for_paths(
            "We should validate the header before parsing because otherwise malformed requests panic.",
            &[],
        ));
        assert!(!is_high_signal_review_comment_for_paths("LGTM", &[]));
        assert!(!is_high_signal_review_comment_for_paths(
            "nit: spacing",
            &[]
        ));
        assert!(!is_high_signal_review_comment_for_paths(
            "Thanks for fixing this.",
            &[],
        ));
        assert!(!is_high_signal_review_comment_for_paths(
            "Agree with u. If we add some conditions to check the param in advance, there should be a little slowdown than before.",
            &[],
        ));
        assert!(!is_high_signal_review_comment_for_paths(
            "Because this operation removes indices to prevent prefix checking.",
            &[],
        ));
        assert!(!is_high_signal_review_comment_for_paths(
            "// Copyright 2026 Gin Core Team. All rights reserved. // Use of this source code is governed by a MIT style license.",
            &[],
        ));
        assert!(!is_high_signal_review_comment_for_paths(
            "## Pull request overview This PR updates CI workflows to use newer versions of tools and standardizes YAML string formatting.",
            &[".github/workflows/gin.yml".to_owned()],
        ));
    }

    #[test]
    fn local_candidate_title_and_file_pattern_are_stable() {
        let title = candidate_title(
            "Please prefer Mapping[str, str] here instead of dict[str, str]. It keeps callers flexible.",
            "src/http/headers.py",
        );
        assert_eq!(
            title,
            "Review: Prefer Mapping[str, str] here instead of dict[str, str]"
        );
        assert_eq!(
            file_pattern_from_path("src/http/headers.py").as_deref(),
            Some("src/http/**/*.py")
        );
        assert_eq!(
            file_pattern_from_path("README.md").as_deref(),
            Some("**/README.md")
        );
        assert_eq!(
            file_pattern_from_path("UPGRADE-6.4.md").as_deref(),
            Some("**/UPGRADE-6.4.md")
        );
        assert_eq!(
            file_pattern_from_path("acceptance/testdata/workflow/run-view.txtar").as_deref(),
            Some("acceptance/testdata/workflow/**/*.txtar")
        );
        assert_eq!(
            file_pattern_from_path("go.mod").as_deref(),
            Some("**/go.mod")
        );
        assert_eq!(
            file_pattern_from_path("package-lock.json").as_deref(),
            Some("**/package-lock.json")
        );
        assert_eq!(file_pattern_from_path("Context.PDF"), None);
        assert_eq!(file_pattern_from_path("maps.Copy"), None);
        assert_eq!(file_pattern_from_path("Handle body-size errors"), None);
    }

    #[test]
    fn local_candidate_body_starts_with_distilled_rule_before_raw_review() {
        let mut item = imported_item(
            Some("user/fork"),
            Some(r#"{"sourceRepoFullName":"upstream/project","attachedRepoFullName":"user/fork"}"#),
        );
        item.comments[0].content =
            "Please prefer Mapping[str, str] here instead of dict[str, str]. It keeps callers flexible."
                .to_owned();
        item.comments[0].metadata = Some(r#"{"filePath":"src/http/headers.py"}"#.to_owned());

        let input =
            local_candidate_input(&item, &item.comments[0], &github_scope("upstream/project"))
                .expect("candidate")
                .input;

        assert!(input.body.starts_with(
            "Rule:\nWhen touching `src/http/**/*.py`, prefer Mapping[str, str] here instead of dict[str, str]."
        ));
        assert!(
            input
                .body
                .contains("Source evidence:\nSource: upstream/project#7")
        );
        assert!(input.body.contains("Reviewer said:\n"));
    }

    #[test]
    fn local_candidate_skips_pr_overview_bot_summary() {
        let mut item = imported_item(
            Some("user/fork"),
            Some(r#"{"sourceRepoFullName":"upstream/project","attachedRepoFullName":"user/fork"}"#),
        );
        item.item.file_path = "ci: update workflows and dependencies".to_owned();
        item.comments[0].content = "## Pull request overview\nThis PR should ensure CI uses current action versions and dependency manifests stay in sync.\n\n| File | Description |\n| ---- | ----------- |\n| .github/workflows/gin.yml | Updates the lint action version. |\n| `go.mod` | Bumps module dependencies. |"
            .to_owned();
        item.comments[0].metadata = None;

        assert!(
            local_candidate_input(&item, &item.comments[0], &github_scope("upstream/project"))
                .is_none()
        );
    }

    #[test]
    fn local_candidate_skips_coverage_and_ai_review_reports() {
        for (author, content) in [
            (
                Some("codecov[bot]"),
                "## Codecov Report\nPatch coverage is 72.31% and project coverage changed by -0.03%.",
            ),
            (
                None,
                "Codecov Report: patch coverage should improve before merge because uncovered lines changed.",
            ),
            (
                Some("coderabbitai"),
                "## Walkthrough\nThis automated review should ensure the new route handler validates input.",
            ),
            (
                None,
                "Actionable comments posted: 0. Review skipped because this PR only updates generated files.",
            ),
        ] {
            let mut item = imported_item(
                Some("user/fork"),
                Some(
                    r#"{"sourceRepoFullName":"upstream/project","attachedRepoFullName":"user/fork"}"#,
                ),
            );
            item.comments[0].author = author.map(str::to_owned);
            item.comments[0].metadata = Some(r#"{"filePath":"src/lib.rs"}"#.to_owned());
            item.comments[0].content = content.to_owned();

            assert!(
                local_candidate_input(&item, &item.comments[0], &github_scope("upstream/project"))
                    .is_none(),
                "content should be skipped: {content}"
            );
        }
    }

    #[test]
    fn local_candidate_skips_acknowledgements_and_weak_questions() {
        for content in [
            "I updated the test to use msw and verify the request body.",
            "Fixed in the latest push; the regression test now covers this.",
            "I tested this in the beta.6 version now and can confirm it works. Nice work.",
            "I don't have any suggestions for fixes, etc. Thanks for the great work.",
            "In the end, we use `any`, but it's good. Thank you for your contribution.",
            "Do we need to support this edge case?",
            "Is there a reason this should live in the public API?",
        ] {
            let mut item = imported_item(
                Some("user/fork"),
                Some(
                    r#"{"sourceRepoFullName":"upstream/project","attachedRepoFullName":"user/fork"}"#,
                ),
            );
            item.comments[0].metadata = Some(r#"{"filePath":"src/lib.rs"}"#.to_owned());
            item.comments[0].content = content.to_owned();

            assert!(
                local_candidate_input(&item, &item.comments[0], &github_scope("upstream/project"))
                    .is_none(),
                "content should be skipped: {content}"
            );
        }
    }

    #[test]
    fn local_candidate_keeps_directive_questions() {
        let mut item = imported_item(
            Some("user/fork"),
            Some(r#"{"sourceRepoFullName":"upstream/project","attachedRepoFullName":"user/fork"}"#),
        );
        item.comments[0].metadata = Some(r#"{"filePath":"src/lib.rs"}"#.to_owned());
        item.comments[0].content =
            "Could you add a regression test that verifies malformed headers return 400 instead of panicking?"
                .to_owned();

        let input =
            local_candidate_input(&item, &item.comments[0], &github_scope("upstream/project"))
                .expect("candidate")
                .input;

        assert!(input.body.contains("add a regression test"));
    }

    #[test]
    fn local_candidate_keeps_copilot_as_product_or_path_name() {
        let mut item = imported_item(
            Some("user/fork"),
            Some(r#"{"sourceRepoFullName":"upstream/project","attachedRepoFullName":"user/fork"}"#),
        );
        item.comments[0].author = Some("human-reviewer".to_owned());
        item.comments[0].metadata = Some(r#"{"filePath":"pkg/cmd/copilot/copilot.go"}"#.to_owned());
        item.comments[0].content =
            "Also `copilot` should be replaced with the const to keep this command consistent."
                .to_owned();

        let input =
            local_candidate_input(&item, &item.comments[0], &github_scope("upstream/project"))
                .expect("candidate")
                .input;

        assert!(input.body.contains("`pkg/cmd/copilot/**/*.go`"));
        assert!(
            input
                .body
                .contains("`copilot` should be replaced with the const")
        );
    }

    #[test]
    fn local_candidate_skips_non_english_docs_translation_wording() {
        for content in [
            "This should be translated as a more natural Korean sentence for this paragraph.",
            "This sentence reads awkwardly and should be rewritten by a native speaker.",
        ] {
            let mut item = imported_item(
                Some("user/fork"),
                Some(
                    r#"{"sourceRepoFullName":"upstream/project","attachedRepoFullName":"user/fork"}"#,
                ),
            );
            item.comments[0].metadata =
                Some(r#"{"filePath":"docs/ko/docs/tutorial/response-status-code.md"}"#.to_owned());
            item.comments[0].content = content.to_owned();

            assert!(
                local_candidate_input(&item, &item.comments[0], &github_scope("upstream/project"))
                    .is_none(),
                "content should be skipped: {content}"
            );
        }
    }

    #[test]
    fn local_candidate_keeps_localized_docs_api_symbol_rule() {
        let mut item = imported_item(
            Some("user/fork"),
            Some(r#"{"sourceRepoFullName":"upstream/project","attachedRepoFullName":"user/fork"}"#),
        );
        item.comments[0].metadata =
            Some(r#"{"filePath":"docs/ko/docs/tutorial/response-status-code.md"}"#.to_owned());
        item.comments[0].content =
            "Please keep `HTTPException` untranslated because it is a FastAPI API symbol."
                .to_owned();
        assert!(
            is_high_signal_review_comment_for_paths(
                &item.comments[0].content,
                &["docs/ko/docs/tutorial/response-status-code.md".to_owned()]
            ),
            "clean: {}",
            clean_review_comment(&item.comments[0].content)
        );

        let input =
            local_candidate_input(&item, &item.comments[0], &github_scope("upstream/project"))
                .expect("candidate")
                .input;

        assert!(input.body.contains("keep `HTTPException` untranslated"));
    }

    #[test]
    fn local_candidate_extracts_later_directive_after_greeting() {
        let mut item = imported_item(
            Some("user/fork"),
            Some(r#"{"sourceRepoFullName":"upstream/project","attachedRepoFullName":"user/fork"}"#),
        );
        item.comments[0].metadata = Some(r#"{"filePath":"src/jsx/streaming.test.tsx"}"#.to_owned());
        item.comments[0].content = "Hi @alice, thank you for the correction. That's a great help. Please add the following test for the fallback path."
            .to_owned();

        let input =
            local_candidate_input(&item, &item.comments[0], &github_scope("upstream/project"))
                .expect("candidate")
                .input;

        assert!(input.title.contains("Add the following test"));
        assert!(input.body.contains(
            "When touching `src/jsx/**/*.tsx`, add the following test for the fallback path."
        ));
        assert!(!input.body.contains("thank you for the correction."));
    }

    #[test]
    fn local_candidate_extracts_directive_after_positive_ack() {
        let mut item = imported_item(
            Some("user/fork"),
            Some(r#"{"sourceRepoFullName":"upstream/project","attachedRepoFullName":"user/fork"}"#),
        );
        item.comments[0].metadata =
            Some(r#"{"filePath":"packages/vite/src/node/cli.ts"}"#.to_owned());
        item.comments[0].content =
            "This works great! As suggested, we should add the `-w` option as webpack does."
                .to_owned();

        let input =
            local_candidate_input(&item, &item.comments[0], &github_scope("upstream/project"))
                .expect("candidate")
                .input;

        assert!(input.body.contains("add the `-w` option as webpack does."));
        assert!(!input.title.contains("This works great"));
    }

    #[test]
    fn local_candidate_skips_pr_process_chatter() {
        for content in [
            "@airhorns would you merge main to this branch? Tests should be green after that.",
            "Can I make changes to this PR? Or should I fork your repo?",
            "Please don't comment on years old PRs, open a new issue with a minimal reproduction.",
            "A test is failing (+ rebase needed).",
            ":/ Could you update the PR base branch before merging this?",
        ] {
            let mut item = imported_item(
                Some("user/fork"),
                Some(
                    r#"{"sourceRepoFullName":"upstream/project","attachedRepoFullName":"user/fork"}"#,
                ),
            );
            item.comments[0].metadata = Some(r#"{"filePath":"src/lib.rs"}"#.to_owned());
            item.comments[0].content = content.to_owned();

            assert!(
                local_candidate_input(&item, &item.comments[0], &github_scope("upstream/project"))
                    .is_none(),
                "content should be skipped: {content}"
            );
        }
    }

    #[test]
    fn local_candidate_ignores_bare_code_filenames_from_review_tables() {
        let mut item = imported_item(
            Some("user/fork"),
            Some(r#"{"sourceRepoFullName":"upstream/project","attachedRepoFullName":"user/fork"}"#),
        );
        item.item.file_path = "ci summary".to_owned();
        item.comments[0].metadata = None;
        item.comments[0].content =
            "Please ensure workflow versions stay consistent across CI files.\n\n\
| File | Description |\n\
| ---- | ----------- |\n\
| .github/workflows/gin.yml | Updates the lint action version. |\n\
| ConsumerGroup.java | Bare generated table filename without a directory. |"
                .to_owned();

        let input =
            local_candidate_input(&item, &item.comments[0], &github_scope("upstream/project"))
                .expect("candidate")
                .input;
        let patterns = input.file_patterns.expect("file patterns");

        assert_eq!(patterns, vec![".github/workflows/**/*.yml".to_owned()]);
        assert!(!input.body.contains("Related files: ConsumerGroup.java"));
    }

    #[test]
    fn local_candidate_caps_large_pr_summary_file_patterns() {
        let mut item = imported_item(
            Some("user/fork"),
            Some(r#"{"sourceRepoFullName":"upstream/project","attachedRepoFullName":"user/fork"}"#),
        );
        item.item.file_path = "module00/src/Foo00.java".to_owned();
        item.comments[0].metadata = Some(r#"{"filePath":"module00/src/Foo00.java"}"#.to_owned());
        let rows = (0..40)
            .map(|n| format!("| module{n:02}/src/Foo{n:02}.java | keep validation aligned |"))
            .collect::<Vec<_>>()
            .join("\n");
        item.comments[0].content = format!(
            "Please validate serializer state and keep behavior consistent across these modules.\n\n| File | Comment |\n| ---- | ------- |\n{rows}"
        );

        let input =
            local_candidate_input(&item, &item.comments[0], &github_scope("upstream/project"))
                .expect("candidate")
                .input;
        let patterns = input.file_patterns.expect("file patterns");

        assert_eq!(
            patterns.len(),
            difflore_core::skills::REMEMBER_FILE_PATTERN_LIMIT
        );
        assert_eq!(patterns[0], "module00/src/**/*.java");
    }

    #[test]
    fn local_candidate_caps_related_files_body_line() {
        let mut item = imported_item(
            Some("user/fork"),
            Some(r#"{"sourceRepoFullName":"upstream/project","attachedRepoFullName":"user/fork"}"#),
        );
        item.item.file_path = "module00/src/Foo00.java".to_owned();
        item.comments[0].metadata = Some(r#"{"filePath":"module00/src/Foo00.java"}"#.to_owned());
        let rows = (0..48)
            .map(|n| format!("| module{n:02}/src/Foo{n:02}.java | keep validation aligned |"))
            .collect::<Vec<_>>()
            .join("\n");
        item.comments[0].content = format!(
            "Please validate serializer state and keep behavior consistent across these modules.\n\n| File | Comment |\n| ---- | ------- |\n{rows}"
        );

        let input =
            local_candidate_input(&item, &item.comments[0], &github_scope("upstream/project"))
                .expect("candidate")
                .input;

        assert!(
            input
                .body
                .contains("Related files: module01/src/Foo01.java")
        );
        assert!(input.body.contains("and 35 more"));
        assert!(!input.body.contains("module47/src/Foo47.java"));
        assert!(
            input.body.chars().count() <= difflore_core::skills::REMEMBER_BODY_CHAR_LIMIT,
            "candidate body should fit remember_rule limit"
        );
    }

    #[test]
    fn local_candidate_skips_coderabbit_outside_diff_aggregate() {
        let mut item = imported_item(
            Some("user/fork"),
            Some(r#"{"sourceRepoFullName":"upstream/project","attachedRepoFullName":"user/fork"}"#),
        );
        item.item.file_path = "review summary".to_owned();
        item.comments[0].metadata = None;
        item.comments[0].content = "[!CAUTION]\n\
Some comments are outside the diff and cannot be posted inline due to platform limitations.\n\n\
<details>\n\
<summary>Outside diff range comments (14)</summary>\n\n\
| File | Comment |\n\
| ---- | ------- |\n\
| `src/lib.rs` | We should validate the header before parsing because malformed requests panic. |\n\
| +14 more | Additional outside-diff comments. |\n\
</details>"
            .to_owned();

        assert!(
            local_candidate_input(&item, &item.comments[0], &github_scope("upstream/project"))
                .is_none()
        );
    }

    #[test]
    fn local_candidate_skips_platform_review_table_wrapper() {
        let mut item = imported_item(
            Some("user/fork"),
            Some(r#"{"sourceRepoFullName":"upstream/project","attachedRepoFullName":"user/fork"}"#),
        );
        item.comments[0].metadata = Some(r#"{"filePath":"src/lib.rs"}"#.to_owned());
        item.comments[0].content = "<details>\n\
<summary>Review details</summary>\n\n\
| Reviewable files | 18 |\n\
| Additional comments | 14 |\n\n\
We should validate the header before parsing because malformed requests panic.\n\
</details>"
            .to_owned();

        assert!(
            local_candidate_input(&item, &item.comments[0], &github_scope("upstream/project"))
                .is_none()
        );
    }

    #[test]
    fn local_candidate_ignores_plus_more_scope_markers() {
        let mut item = imported_item(
            Some("user/fork"),
            Some(r#"{"sourceRepoFullName":"upstream/project","attachedRepoFullName":"user/fork"}"#),
        );
        item.item.file_path = "ci summary".to_owned();
        item.comments[0].metadata = None;
        item.comments[0].content =
            "Please ensure workflow versions stay consistent across CI files.\n\n\
| File | Description |\n\
| ---- | ----------- |\n\
| .github/workflows/gin.yml | Updates the lint action version. |\n\
| +14 more | Additional files hidden by the review UI. |"
                .to_owned();

        let input =
            local_candidate_input(&item, &item.comments[0], &github_scope("upstream/project"))
                .expect("candidate")
                .input;
        let patterns = input.file_patterns.expect("file patterns");

        assert_eq!(patterns, vec![".github/workflows/**/*.yml".to_owned()]);
        assert!(input.body.contains("File: .github/workflows/gin.yml"));
        assert!(!input.body.contains("+14 more"));
    }

    #[test]
    fn local_candidate_skips_pr_author_thread_replies() {
        let mut item = imported_item(
            Some("user/fork"),
            Some(r#"{"sourceRepoFullName":"upstream/project","attachedRepoFullName":"user/fork"}"#),
        );
        item.item.author = Some("alice".to_owned());
        item.comments[0].author = Some("Alice".to_owned());
        item.comments[0].content =
            "Fixed - now asserting found=false for all the non-matching paths, not just checking for panics."
                .to_owned();
        item.comments[0].metadata = Some(r#"{"filePath":"tree_test.go"}"#.to_owned());

        assert!(
            local_candidate_input(&item, &item.comments[0], &github_scope("upstream/project"))
                .is_none()
        );
    }

    #[test]
    fn local_candidate_uses_pr_discussion_comment_with_changed_file_scope() {
        let mut item = imported_item(
            Some("difflore-fixtures/terminal"),
            Some(
                r#"{"sourceRepoFullName":"microsoft/terminal","attachedRepoFullName":"difflore-fixtures/terminal"}"#,
            ),
        );
        item.item.file_path = "tools/ReleaseEngineering/Draft-TerminalReleases.ps1".to_owned();
        item.comments[0].author = Some("DHowett".to_owned());
        item.comments[0].comment_url = Some(
            "https://github.com/microsoft/terminal/pull/13629#issuecomment-1644692454".to_owned(),
        );
        item.comments[0].metadata = Some(
            r#"{"filePath":"tools/ReleaseEngineering/Draft-TerminalReleases.ps1","sourceKind":"issue_comment"}"#
                .to_owned(),
        );
        item.comments[0].content =
            "This is great and amazing, but it needs to be fixed for portable/zip builds and stuff too."
                .to_owned();

        let input = local_candidate_input(
            &item,
            &item.comments[0],
            &github_scope("microsoft/terminal"),
        )
        .expect("candidate")
        .input;

        assert_eq!(
            input.file_patterns.as_deref(),
            Some(&["tools/ReleaseEngineering/**/*.ps1".to_owned()][..])
        );
        assert!(input.body.contains("Source: microsoft/terminal#7"));
        assert!(input.body.contains(
            "When touching `tools/ReleaseEngineering/**/*.ps1`, fixed for portable/zip builds"
        ));
    }

    #[test]
    fn local_candidate_does_not_auto_activate_unadopted_bot_directive() {
        // A bot directive with no adoption signal (unresolved, no reactions)
        // must land as a medium-confidence pending draft: not dropped, not
        // auto-active.
        let mut item = imported_item(
            Some("user/fork"),
            Some(r#"{"sourceRepoFullName":"upstream/project","attachedRepoFullName":"user/fork"}"#),
        );
        item.comments[0].author = Some("github-actions[bot]".to_owned());
        item.comments[0].metadata = Some(r#"{"filePath":"src/lib.rs"}"#.to_owned());
        item.comments[0].content =
            "Please ensure workflow versions stay consistent across CI files.".to_owned();

        let candidate =
            local_candidate_input(&item, &item.comments[0], &github_scope("upstream/project"))
                .expect("unadopted bot directive should draft a candidate, not be vetoed");
        assert_eq!(
            candidate.route,
            CaptureRoute::Candidate,
            "unadopted bot directive must stay pending, got confidence {}",
            candidate.confidence,
        );
        assert!(candidate.confidence < CAPTURE_CONFIDENCE_HIGH);
        assert!(candidate.confidence >= CAPTURE_CONFIDENCE_LOW);
    }

    #[test]
    fn local_candidate_auto_activates_resolved_bot_directive() {
        // A bot directive that WAS adopted (resolved thread) earns the
        // resolved bonus and clears the HIGH threshold, so it auto-activates.
        let mut item = imported_item(
            Some("user/fork"),
            Some(r#"{"sourceRepoFullName":"upstream/project","attachedRepoFullName":"user/fork"}"#),
        );
        item.comments[0].author = Some("coderabbitai[bot]".to_owned());
        item.comments[0].metadata = Some(
            r#"{"filePath":"src/http/request.rs","resolved":true,"thumbsUp":1,"thumbsDown":0,"reactionsTotal":1}"#
                .to_owned(),
        );
        item.comments[0].content =
            "Please validate the header before parsing because otherwise malformed requests panic."
                .to_owned();

        let candidate =
            local_candidate_input(&item, &item.comments[0], &github_scope("upstream/project"))
                .expect("resolved bot directive should draft a candidate");
        assert_eq!(
            candidate.route,
            CaptureRoute::Active,
            "resolved+approved bot directive must auto-activate, got confidence {}",
            candidate.confidence,
        );
        assert!(candidate.confidence >= CAPTURE_CONFIDENCE_HIGH);
    }

    #[test]
    fn local_candidate_auto_activates_resolved_human_directive() {
        let mut item = imported_item(
            Some("user/fork"),
            Some(r#"{"sourceRepoFullName":"upstream/project","attachedRepoFullName":"user/fork"}"#),
        );
        item.comments[0].author = Some("human-reviewer".to_owned());
        item.comments[0].metadata =
            Some(r#"{"filePath":"src/http/request.rs","resolved":true}"#.to_owned());
        item.comments[0].content =
            "We should validate the header before parsing because otherwise malformed requests panic."
                .to_owned();

        let candidate =
            local_candidate_input(&item, &item.comments[0], &github_scope("upstream/project"))
                .expect("resolved human directive should draft a candidate");
        assert_eq!(candidate.route, CaptureRoute::Active);
        assert!(candidate.confidence >= CAPTURE_CONFIDENCE_HIGH);
    }

    #[test]
    fn local_candidate_leaves_unadopted_human_directive_pending() {
        // A strong human directive with no adoption signal becomes a pending
        // candidate the user must accept, not auto-active.
        let mut item = imported_item(
            Some("user/fork"),
            Some(r#"{"sourceRepoFullName":"upstream/project","attachedRepoFullName":"user/fork"}"#),
        );
        item.comments[0].metadata = Some(r#"{"filePath":"src/http/request.rs"}"#.to_owned());
        item.comments[0].content =
            "We should validate the header before parsing because otherwise malformed requests panic."
                .to_owned();

        let candidate =
            local_candidate_input(&item, &item.comments[0], &github_scope("upstream/project"))
                .expect("unadopted human directive should still draft a candidate");
        assert_eq!(candidate.route, CaptureRoute::Candidate);
        assert!(candidate.confidence < CAPTURE_CONFIDENCE_HIGH);
        assert!(candidate.confidence >= CAPTURE_CONFIDENCE_LOW);
    }

    #[test]
    fn local_candidate_drops_contradicted_directive() {
        // A later reply retracting the suggestion is a strong negative —
        // even a strong directive must be dropped (route below LOW → None).
        let mut item = imported_item(
            Some("user/fork"),
            Some(r#"{"sourceRepoFullName":"upstream/project","attachedRepoFullName":"user/fork"}"#),
        );
        item.comments[0].metadata = Some(
            r#"{"filePath":"src/http/request.rs","laterReplies":["Actually no, disregard that — the framework already handles it."]}"#
                .to_owned(),
        );
        item.comments[0].content =
            "We should validate the header before parsing because otherwise malformed requests panic."
                .to_owned();

        assert!(
            local_candidate_input(&item, &item.comments[0], &github_scope("upstream/project"))
                .is_none(),
            "a contradicted directive must be dropped, not drafted"
        );
    }

    #[test]
    fn local_candidate_demotes_resolved_but_downvoted_directive_to_pending() {
        // A resolved thread alone clears HIGH, but a strict 👎-majority is a
        // correctness signal: the directive demotes to a pending candidate
        // (still ≥ LOW so it is reviewed, not dropped), not auto-active.
        let mut item = imported_item(
            Some("user/fork"),
            Some(r#"{"sourceRepoFullName":"upstream/project","attachedRepoFullName":"user/fork"}"#),
        );
        item.comments[0].author = Some("human-reviewer".to_owned());
        item.comments[0].metadata = Some(
            r#"{"filePath":"src/http/request.rs","resolved":true,"thumbsUp":1,"thumbsDown":4,"reactionsTotal":5}"#
                .to_owned(),
        );
        item.comments[0].content =
            "We should validate the header before parsing because otherwise malformed requests panic."
                .to_owned();

        let candidate =
            local_candidate_input(&item, &item.comments[0], &github_scope("upstream/project"))
                .expect(
                    "resolved-but-downvoted directive should draft a candidate, not be dropped",
                );
        assert_eq!(
            candidate.route,
            CaptureRoute::Candidate,
            "net-downvoted resolved directive must stay pending, got confidence {}",
            candidate.confidence,
        );
        assert!(candidate.confidence < CAPTURE_CONFIDENCE_HIGH);
        assert!(candidate.confidence >= CAPTURE_CONFIDENCE_LOW);
    }

    #[test]
    fn local_candidate_keeps_resolved_directive_active_despite_single_downvote() {
        // A tied 👍/👎 is not a veto: only a strict 👎-majority penalizes, so
        // the resolved bonus still carries the directive to active.
        let mut item = imported_item(
            Some("user/fork"),
            Some(r#"{"sourceRepoFullName":"upstream/project","attachedRepoFullName":"user/fork"}"#),
        );
        item.comments[0].author = Some("human-reviewer".to_owned());
        item.comments[0].metadata = Some(
            r#"{"filePath":"src/http/request.rs","resolved":true,"thumbsUp":1,"thumbsDown":1,"reactionsTotal":2}"#
                .to_owned(),
        );
        item.comments[0].content =
            "We should validate the header before parsing because otherwise malformed requests panic."
                .to_owned();

        let candidate =
            local_candidate_input(&item, &item.comments[0], &github_scope("upstream/project"))
                .expect("resolved directive should draft a candidate");
        assert_eq!(
            candidate.route,
            CaptureRoute::Active,
            "a tied 👍/👎 must not penalize a resolved directive, got confidence {}",
            candidate.confidence,
        );
        assert!(candidate.confidence >= CAPTURE_CONFIDENCE_HIGH);
    }

    #[test]
    fn capture_confidence_routes_at_named_thresholds() {
        assert_eq!(
            route_for_confidence(CAPTURE_CONFIDENCE_HIGH),
            CaptureRoute::Active
        );
        assert_eq!(
            route_for_confidence(CAPTURE_CONFIDENCE_HIGH - 0.01),
            CaptureRoute::Candidate
        );
        assert_eq!(
            route_for_confidence(CAPTURE_CONFIDENCE_LOW),
            CaptureRoute::Candidate
        );
        assert_eq!(
            route_for_confidence(CAPTURE_CONFIDENCE_LOW - 0.01),
            CaptureRoute::Drop
        );
    }

    #[test]
    fn local_candidate_skips_greeting_only_review_verdicts() {
        for content in [
            "Hi, thanks for the PR.",
            "@m1a2st : Thanks for the updated PR.",
            "Hi @matt-welch, thanks for working on this.",
            "@junrao You're right.",
            "Overall LGTM",
            "Be fine as-is",
        ] {
            let mut item = imported_item(
                Some("user/fork"),
                Some(
                    r#"{"sourceRepoFullName":"upstream/project","attachedRepoFullName":"user/fork"}"#,
                ),
            );
            item.comments[0].metadata = Some(r#"{"filePath":"src/lib.rs"}"#.to_owned());
            item.comments[0].content = content.to_owned();

            assert!(
                local_candidate_input(&item, &item.comments[0], &github_scope("upstream/project"))
                    .is_none(),
                "content should be skipped: {content}"
            );
        }
    }

    #[test]
    fn distilled_rule_statement_removes_review_chatter_prefixes() {
        assert_eq!(
            distilled_rule_statement(
                "We should validate the header before parsing because otherwise malformed requests panic.",
                "src/http/request.rs",
            ),
            "When touching `src/http/**/*.rs`, validate the header before parsing because otherwise malformed requests panic."
        );
    }

    #[test]
    fn distilled_rule_statement_keeps_dotted_code_identifiers_intact() {
        assert_eq!(
            distilled_rule_statement(
                "The test should verify that `http.ErrAbortHandler` is actually being treated as a broken pipe error by asserting that the output does NOT contain \"panic recovered\".",
                "recovery_test.go",
            ),
            "When touching `**/*.go`, the test should verify that `http.ErrAbortHandler` is actually being treated as a broken pipe error by asserting that the output does NOT contain \"panic recovered\"."
        );
    }

    #[test]
    fn gitlab_same_repo_local_candidate_does_not_widen_file_patterns() {
        let mut item = imported_item(Some("group/project"), None);
        item.item.source = "gitlab".to_owned();
        item.comments[0].metadata = Some(
            serde_json::json!({
                "filePath": "src/http/request.rs",
                "gitlabHost": "gitlab.com",
                "sourceRepoFullName": "group/project",
                "attachedRepoFullName": "group/project",
                "resolved": true,
            })
            .to_string(),
        );
        item.comments[0].content =
            "We should validate the header before parsing because otherwise malformed requests panic."
                .to_owned();

        let input = local_candidate_input(
            &item,
            &item.comments[0],
            &gitlab_scope("gitlab.com", "group/project"),
        )
        .expect("candidate")
        .input;

        assert_eq!(
            input.file_patterns.as_deref(),
            Some(
                [
                    String::from("src/http/**/*.rs"),
                    String::from("src/**/*.rs"),
                ]
                .as_slice()
            )
        );
        assert!(
            !input
                .file_patterns
                .as_deref()
                .unwrap_or_default()
                .contains(&String::from("**/*.rs"))
        );
    }

    #[test]
    fn import_next_steps_are_value_proof_first() {
        assert_eq!(
            local_candidate_next_step_commands(),
            &[
                "difflore status",
                "difflore recall --diff",
                "difflore fix --preview",
            ],
        );

        let cloud_commands = cloud_upload_next_step_commands()
            .iter()
            .map(|(cmd, _)| *cmd)
            .collect::<Vec<_>>();
        assert_eq!(
            cloud_commands,
            vec![
                "difflore cloud sync",
                "difflore status",
                "difflore recall --diff",
                "difflore cloud impact",
                "difflore fix --preview",
            ],
        );
    }

    #[test]
    fn pending_drafts_hint_points_at_status_not_removed_candidates_verb() {
        // The drafts hint must name an existing command and never resurrect
        // the removed `difflore candidates` verb.
        for count in [1usize, 8] {
            let (prefix, command, suffix) = pending_drafts_review_hint(count);
            let full = format!("{prefix}{command}{suffix}");

            assert_eq!(
                command, "difflore status",
                "hint must steer to a real command"
            );
            assert!(
                !full.contains("difflore candidates"),
                "hint must not name the removed `difflore candidates` verb: {full}"
            );
            assert!(
                !full.contains("accept"),
                "there is no manual per-id accept command; do not imply one: {full}"
            );
            assert!(
                full.contains("held for review"),
                "hint should read as a review prompt: {full}"
            );
        }

        // Plain singular/plural agreement on the draft noun.
        assert!(
            pending_drafts_review_hint(1)
                .0
                .contains("1 medium-confidence draft held")
        );
        assert!(
            pending_drafts_review_hint(8)
                .0
                .contains("8 medium-confidence drafts held")
        );
    }

    #[tokio::test]
    async fn local_candidate_budget_ignores_deduped_comments_between_new_rules() {
        let db = fresh_import_pool().await;
        seed_imported_review_comments(
            &db,
            &[
                (
                    "Please validate the header before parsing because otherwise malformed requests panic.",
                    "src/http/request.rs",
                ),
                (
                    "We should validate the header before parsing because otherwise malformed requests panic.",
                    "src/http/request.rs",
                ),
                (
                    "Please prefer Mapping[str, str] here instead of dict[str, str]. It keeps callers flexible.",
                    "src/http/headers.py",
                ),
            ],
        )
        .await;

        let source_scope = github_scope("acme/widgets");
        let progress = run_local_candidates(
            &db,
            "github",
            "acme/widgets",
            &source_scope,
            2,
            &[],
            &HashSet::new(),
        )
        .await;

        assert_eq!(progress.candidates_created, 2);
        // Seeded comments are resolved threads, so the gate auto-activates
        // both — none stay pending.
        assert_eq!(progress.candidates_activated, 2);
        assert_eq!(progress.candidates_pending, 0);
        assert_eq!(progress.candidates_deduped, 1);
        assert!(local_candidate_budget_reached(&progress));
        assert!(progress.capped);

        let memories = difflore_core::skills::list_all_skills(&db)
            .await
            .expect("list active memories");
        assert_eq!(memories.len(), 2);
        assert!(
            memories
                .iter()
                .any(|c| c.name.contains("Validate the header")),
            "memories: {memories:?}"
        );
        assert!(
            memories.iter().any(|c| c.name.contains("Prefer Mapping")),
            "memories: {memories:?}"
        );
    }

    #[tokio::test]
    async fn gitlab_local_import_recalls_and_stays_isolated_from_same_github_namespace() {
        let db = fresh_import_pool().await;
        seed_gitlab_pr_with_directive(
            &db,
            "gitlab.com",
            "group/project",
            7,
            "We should validate the header before parsing because otherwise malformed requests panic.",
            "src/http/request.rs",
        )
        .await;
        seed_pr_with_directive(
            &db,
            "group/project",
            8,
            "We should prefer Mapping[str, str] here instead of dict[str, str] to keep callers flexible.",
            "src/http/headers.py",
        )
        .await;

        let gitlab_source_scope = gitlab_scope("gitlab.com", "group/project");
        let gitlab_progress = run_local_candidates(
            &db,
            "gitlab",
            "group/project",
            &gitlab_source_scope,
            25,
            &[],
            &HashSet::new(),
        )
        .await;
        assert_eq!(gitlab_progress.candidates_created, 1);
        assert_eq!(gitlab_progress.candidates_activated, 1);

        let github_source_scope = github_scope("group/project");
        let github_progress = run_local_candidates(
            &db,
            "github",
            "group/project",
            &github_source_scope,
            25,
            &[],
            &HashSet::new(),
        )
        .await;
        assert_eq!(github_progress.candidates_created, 1);
        assert_eq!(github_progress.candidates_activated, 1);

        let source_repos = difflore_core::skills::list_source_repos(&db)
            .await
            .expect("load source repos");
        let mut source_repo_values = source_repos
            .values()
            .filter_map(|repo| repo.as_deref())
            .collect::<Vec<_>>();
        source_repo_values.sort_unstable();
        assert_eq!(
            source_repo_values,
            vec!["gitlab.com/group/project", "group/project"]
        );

        let gitlab_patterns: Option<String> =
            sqlx::query_scalar("SELECT file_patterns FROM skills WHERE source_repo = ?1")
                .bind(gitlab_source_scope.as_str())
                .fetch_one(&db)
                .await
                .expect("load gitlab file patterns");
        let gitlab_patterns: Vec<String> =
            serde_json::from_str(&gitlab_patterns.expect("gitlab file patterns"))
                .expect("parse gitlab file patterns");
        assert_eq!(gitlab_patterns, vec!["src/http/**/*.rs"]);

        let gitlab_export = difflore_core::export::collect_rules_for_export_with_scopes(
            &db,
            &[gitlab_source_scope.as_str().to_owned()],
            difflore_core::export::ExportCollectOptions::default(),
        )
        .await
        .expect("collect gitlab export");
        assert_eq!(gitlab_export.rules.len(), 1);
        assert!(
            gitlab_export.rules[0].name.contains("Validate the header"),
            "gitlab export: {:?}",
            gitlab_export.rules
        );

        let github_export = difflore_core::export::collect_rules_for_export_with_scopes(
            &db,
            &[github_source_scope.as_str().to_owned()],
            difflore_core::export::ExportCollectOptions::default(),
        )
        .await
        .expect("collect github export");
        assert_eq!(github_export.rules.len(), 1);
        assert!(
            github_export.rules[0].name.contains("Prefer Mapping"),
            "github export: {:?}",
            github_export.rules
        );

        let gitlab_project_path: String = sqlx::query_scalar(
            "SELECT p.path FROM projects p \
             INNER JOIN review_items ri ON ri.project_id = p.id \
             WHERE ri.source = ?1 LIMIT 1",
        )
        .bind("gitlab")
        .fetch_one(&db)
        .await
        .expect("load gitlab project path");
        let project_hash = difflore_core::infra::db::project_hash_from_root(std::path::Path::new(
            &gitlab_project_path,
        ));
        let index_pool = difflore_core::context::index_db::get_pool_for_project(&project_hash)
            .await
            .expect("open test index pool");
        let repo_scopes = vec![
            gitlab_source_scope.as_str().to_owned(),
            github_source_scope.as_str().to_owned(),
        ];
        difflore_core::context::orchestrator::ensure_rules_indexed_for_repo_scopes_with_embedding_timeout(
            &db,
            &index_pool,
            &repo_scopes,
            Some(std::time::Duration::from_millis(0)),
        )
        .await
        .expect("index scoped rules");

        let gitlab_filter = difflore_core::context::index_db::QueryFilter {
            language: Some("rust".to_owned()),
            repo_scope: Some(gitlab_source_scope.as_str().to_owned()),
        };
        let gitlab_hits = difflore_core::context::retrieval::retrieve_rules_with_confidence(
            &index_pool,
            "validate header parsing malformed requests",
            difflore_core::context::retrieval::RetrievalOptions {
                top_k: Some(5),
                target_scope: Some(difflore_core::context::retrieval::TargetScope::File(
                    "src/http/request.rs",
                )),
                filter: Some(&gitlab_filter),
                ann_enabled: false,
                ..Default::default()
            },
        )
        .await
        .expect("retrieve gitlab rules");
        assert!(
            gitlab_hits
                .iter()
                .any(|rule| rule.content.contains("Validate the header")),
            "gitlab recall hits: {gitlab_hits:?}"
        );
        assert!(
            !gitlab_hits
                .iter()
                .any(|rule| rule.content.contains("Prefer Mapping")),
            "gitlab recall leaked github rule: {gitlab_hits:?}"
        );

        let github_filter = difflore_core::context::index_db::QueryFilter {
            language: Some("python".to_owned()),
            repo_scope: Some(github_source_scope.as_str().to_owned()),
        };
        let github_hits = difflore_core::context::retrieval::retrieve_rules_with_confidence(
            &index_pool,
            "prefer mapping strings flexible callers",
            difflore_core::context::retrieval::RetrievalOptions {
                top_k: Some(5),
                target_scope: Some(difflore_core::context::retrieval::TargetScope::File(
                    "src/http/headers.py",
                )),
                filter: Some(&github_filter),
                ann_enabled: false,
                ..Default::default()
            },
        )
        .await
        .expect("retrieve github rules");
        assert!(
            github_hits
                .iter()
                .any(|rule| rule.content.contains("Prefer Mapping")),
            "github recall hits: {github_hits:?}"
        );
        assert!(
            !github_hits
                .iter()
                .any(|rule| rule.content.contains("Validate the header")),
            "github recall leaked gitlab rule: {github_hits:?}"
        );
    }

    #[tokio::test]
    async fn run_local_candidates_leaves_unresolved_directives_pending() {
        // End-to-end routing: an unresolved (un-adopted) directive must NOT
        // be served by the MCP active-rule path; it lands as a pending
        // candidate the user can review and accept.
        let db = fresh_import_pool().await;
        seed_imported_review_comments_with_resolution(
            &db,
            &[(
                "We should validate the header before parsing because otherwise malformed requests panic.",
                "src/http/request.rs",
            )],
            false,
        )
        .await;

        let source_scope = github_scope("acme/widgets");
        let progress = run_local_candidates(
            &db,
            "github",
            "acme/widgets",
            &source_scope,
            5,
            &[],
            &HashSet::new(),
        )
        .await;

        assert_eq!(progress.candidates_created, 1);
        assert_eq!(progress.candidates_activated, 0);
        assert_eq!(progress.candidates_pending, 1);

        // Not active → not surfaced by list_all_skills.
        let active = difflore_core::skills::list_all_skills(&db)
            .await
            .expect("list active memories");
        assert!(
            active.is_empty(),
            "unresolved directive must not auto-activate"
        );

        // But it IS a pending candidate awaiting review.
        let pending = difflore_core::skills::count_pending_candidates(&db, None)
            .await
            .expect("count pending");
        assert_eq!(pending, 1);
    }

    #[test]
    fn import_local_candidate_budget_scales_with_pr_window() {
        let defaults = validate_args(import_args_with_budget(10));
        assert_eq!(local_candidate_budget(&defaults), 25);

        let larger_window = validate_args(import_args_with_budget(100));
        assert_eq!(local_candidate_budget(&larger_window), 200);
    }

    #[test]
    fn import_dry_run_json_describes_plan_without_side_effects() {
        let args = validate_args(ImportArgs {
            repo: Some("acme/fork".to_owned()),
            from_upstream: Some("acme/upstream".to_owned()),
            provider: None,
            gitlab_host: None,
            max_prs: 2,
            pr_numbers: vec![7, 8],
            exclude_prs: vec![9, 9, 10],
            since: None,
            include_open: true,
            upload: false,
            dry_run: true,
            json: true,
        });

        let payload = dry_run_payload(&args, "acme/fork", "acme/upstream");

        assert_eq!(payload["dryRun"], true);
        assert_eq!(payload["provider"], "github");
        assert_eq!(payload["repo"], "acme/fork");
        assert_eq!(payload["sourceRepo"], "acme/upstream");
        assert_eq!(payload["fromUpstream"], "acme/upstream");
        assert_eq!(payload["maxPrs"], 2);
        assert_eq!(payload["prNumbers"], serde_json::json!([7, 8]));
        // Deduped (the two 9s collapse) and sorted for stable JSON output.
        assert_eq!(payload["excludePrs"], serde_json::json!([9, 10]));
        assert_eq!(payload["includeOpen"], true);
        assert_eq!(payload["upload"], false);
        assert_eq!(payload["localCandidates"], true);
        assert_eq!(payload["localCandidateBudget"], 25);
        assert_eq!(payload["writes"], false);
        assert_eq!(payload["networkCalls"], false);
    }

    #[test]
    fn import_json_payload_reports_cloud_upload_queue_result() {
        let progress = ImportProgress {
            prs_total: 2,
            prs_fetched: 1,
            comments_imported: 13,
            comments_skipped: 0,
            prs_missing: 2,
            missing_pr_numbers: vec![404, 405],
        };
        let payload = import_json_payload(
            "github",
            None,
            "acme/fork",
            "acme/upstream",
            &progress,
            None,
            7,
        );

        assert_eq!(payload["provider"], "github");
        assert!(payload["gitlabHost"].is_null());
        assert_eq!(payload["repo"], "acme/fork");
        assert_eq!(payload["sourceRepo"], "acme/upstream");
        assert_eq!(payload["prsFetched"], 1);
        assert_eq!(payload["commentsImported"], 13);
        assert_eq!(payload["prsMissing"], 2);
        assert_eq!(payload["missingPrNumbers"], serde_json::json!([404, 405]));
        assert_eq!(payload["uploadedReviews"], 7);
        assert_eq!(payload["cloudUploadQueued"], true);
    }

    #[test]
    fn import_json_payload_names_the_gitlab_provider_and_host() {
        let progress = ImportProgress {
            prs_total: 1,
            prs_fetched: 1,
            comments_imported: 4,
            comments_skipped: 0,
            prs_missing: 0,
            missing_pr_numbers: Vec::new(),
        };
        let payload = import_json_payload(
            "gitlab",
            Some("gitlab.corp.example"),
            "group/sub/project",
            "group/sub/project",
            &progress,
            None,
            0,
        );

        assert_eq!(payload["provider"], "gitlab");
        assert_eq!(payload["gitlabHost"], "gitlab.corp.example");
        assert_eq!(payload["repo"], "group/sub/project");
        assert_eq!(payload["cloudUploadQueued"], false);
    }

    fn import_args_with_budget(max_prs: usize) -> ImportArgs {
        ImportArgs {
            repo: None,
            from_upstream: None,
            provider: None,
            gitlab_host: None,
            max_prs,
            pr_numbers: Vec::new(),
            exclude_prs: Vec::new(),
            since: None,
            include_open: false,
            upload: false,
            dry_run: false,
            json: true,
        }
    }

    #[tokio::test]
    async fn exclude_prs_yields_no_rules_from_the_excluded_pr() {
        // Leak-free recall eval relies on this: import a repo's review memory
        // while withholding the exact PR recall will be tested on. Seed two
        // PRs with distinct, high-signal directives; exclude one and assert it
        // contributes zero rules while the other still does.
        let db = fresh_import_pool().await;
        seed_pr_with_directive(
            &db,
            "acme/widgets",
            7,
            "We should validate the header before parsing because otherwise malformed requests panic.",
            "src/http/request.rs",
        )
        .await;
        seed_pr_with_directive(
            &db,
            "acme/widgets",
            8,
            "We should prefer Mapping[str, str] here instead of dict[str, str] to keep callers flexible.",
            "src/http/headers.py",
        )
        .await;

        let exclude: HashSet<i32> = std::iter::once(8).collect();
        let source_scope = github_scope("acme/widgets");
        let progress = run_local_candidates(
            &db,
            "github",
            "acme/widgets",
            &source_scope,
            25,
            &[],
            &exclude,
        )
        .await;

        // Only PR #7's directive survives — PR #8 produced no candidate.
        assert_eq!(
            progress.candidates_created, 1,
            "excluded PR #8 must contribute zero rules"
        );

        let memories = difflore_core::skills::list_all_skills(&db)
            .await
            .expect("list active memories");
        assert_eq!(memories.len(), 1, "memories: {memories:?}");
        assert!(
            memories
                .iter()
                .any(|m| m.name.contains("Validate the header")),
            "PR #7's rule should be present: {memories:?}"
        );
        assert!(
            !memories.iter().any(|m| m.name.contains("Prefer Mapping")),
            "PR #8 was excluded, so its rule must not appear: {memories:?}"
        );
    }

    #[tokio::test]
    async fn empty_exclude_set_keeps_every_prs_rules() {
        // Control for the exclude test: with no exclusions both seeded PRs
        // contribute their directive.
        let db = fresh_import_pool().await;
        seed_pr_with_directive(
            &db,
            "acme/widgets",
            7,
            "We should validate the header before parsing because otherwise malformed requests panic.",
            "src/http/request.rs",
        )
        .await;
        seed_pr_with_directive(
            &db,
            "acme/widgets",
            8,
            "We should prefer Mapping[str, str] here instead of dict[str, str] to keep callers flexible.",
            "src/http/headers.py",
        )
        .await;

        let source_scope = github_scope("acme/widgets");
        let progress = run_local_candidates(
            &db,
            "github",
            "acme/widgets",
            &source_scope,
            25,
            &[],
            &HashSet::new(),
        )
        .await;

        assert_eq!(
            progress.candidates_created, 2,
            "no exclusions means both PRs contribute rules"
        );
    }
}
