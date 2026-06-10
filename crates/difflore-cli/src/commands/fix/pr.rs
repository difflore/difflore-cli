use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use difflore_core::models::{DiffContentRecord, GitDiffInput};
use serde::Deserialize;

use crate::commands::util::validate_owner_repo;
use crate::style::{self, sym};

use super::fix_debug;

const PR_PREVIEW_COMMAND_TIMEOUT: Duration = Duration::from_secs(8);
const PR_PREVIEW_DIFF_TIMEOUT: Duration = Duration::from_secs(5);
const PR_COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(25);

fn preview_command_timeout(preview: bool) -> Option<Duration> {
    preview.then_some(PR_PREVIEW_COMMAND_TIMEOUT)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct PrSpec {
    pub(super) repo_full_name: String,
    pub(super) number: u64,
}

#[derive(Clone, Debug)]
pub(super) struct PrMetadata {
    pub(super) repo_full_name: String,
    pub(super) number: u64,
    pub(super) title: String,
    pub(super) base_ref: String,
    pub(super) base_sha: String,
    pub(super) head_ref: String,
    pub(super) head_sha: String,
    pub(super) head_repo_full_name: String,
    pub(super) head_clone_url: String,
    pub(super) is_fork: bool,
}

#[derive(Clone, Debug)]
pub(super) struct PreparedPrFix {
    pub(super) repo_full_name: String,
    pub(super) pr_number: u64,
    pub(super) title: String,
    pub(super) base_ref: String,
    pub(super) head_sha: String,
    pub(super) work_branch: String,
    pub(super) merge_base: String,
    diff_head_ref: String,
    pub(super) scope_label: String,
    pub(super) review_id: String,
    pub(super) repo_full_name_aliases: Vec<String>,
    pub(super) repo_root: PathBuf,
    pub(super) diff_records: Vec<DiffContentRecord>,
    pub(super) checked_out: bool,
}

#[derive(Debug)]
pub(super) struct PreparePrOptions<'a> {
    pub(super) raw_pr: &'a str,
    pub(super) repo_hint: Option<&'a str>,
    pub(super) base_override: Option<&'a str>,
    pub(super) work_branch: Option<&'a str>,
    pub(super) no_checkout: bool,
    pub(super) allow_dirty: bool,
    pub(super) yes: bool,
    pub(super) preview: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhPrView {
    number: u64,
    title: Option<String>,
    base_ref_name: String,
    base_ref_oid: String,
    head_ref_name: String,
    head_ref_oid: String,
    head_repository: Option<GhRepo>,
    head_repository_owner: Option<GhRepoOwner>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhRepo {
    name: Option<String>,
    name_with_owner: Option<String>,
    url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GhRepoOwner {
    login: String,
}

pub(super) async fn prepare_pr_fix(
    cwd: &Path,
    options: PreparePrOptions<'_>,
) -> anyhow::Result<PreparedPrFix> {
    let repo_root = git_repo_root(cwd)?;
    let spec = parse_pr_spec(options.raw_pr, options.repo_hint, &repo_root)?;
    let preview_command_timeout = preview_command_timeout(options.preview);
    let mut meta = fetch_pr_metadata(&repo_root, &spec, preview_command_timeout)?;
    if let Some(base) = options.base_override {
        // Reject option-looking base overrides before git can parse them as
        // flags on the --no-checkout path.
        difflore_core::infra::git::reject_option_like_revision(base, "PR base override")
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        meta.base_ref = base.to_owned();
    }

    let prepared = if options.no_checkout {
        ensure_clean_worktree(&repo_root, options.allow_dirty)?;
        prepare_current_head_pr(&repo_root, &meta)?
    } else if options.preview {
        prepare_preview_pr(&repo_root, &meta, preview_command_timeout)?
    } else {
        ensure_clean_worktree(&repo_root, options.allow_dirty)?;
        let work_branch = options
            .work_branch
            .map_or_else(|| default_work_branch(meta.number), str::to_owned);
        if !prompt_pr_checkout(&meta, &work_branch, options.yes)? {
            bail!("aborted");
        }
        fetch_and_checkout_pr(&repo_root, &meta, &work_branch)?
    };

    let diff_started = Instant::now();
    let diff_records = if options.preview {
        match tokio::time::timeout(
            PR_PREVIEW_DIFF_TIMEOUT,
            collect_pr_diff(&repo_root, &prepared),
        )
        .await
        {
            Ok(result) => result?,
            Err(_) => bail!(
                "preview PR diff collection timed out after {}ms; fetch the PR locally or retry without --preview to use the full apply-path budget",
                duration_ms(PR_PREVIEW_DIFF_TIMEOUT)
            ),
        }
    } else {
        collect_pr_diff(&repo_root, &prepared).await?
    };
    fix_debug!(
        "pr_prepare diff_collection elapsed={}ms timeout={}",
        duration_ms(diff_started.elapsed()),
        if options.preview {
            format!("{}ms", duration_ms(PR_PREVIEW_DIFF_TIMEOUT))
        } else {
            "none".to_owned()
        }
    );
    Ok(PreparedPrFix {
        diff_records,
        ..prepared
    })
}

pub(super) fn parse_pr_spec(
    raw: &str,
    repo_hint: Option<&str>,
    cwd: &Path,
) -> anyhow::Result<PrSpec> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("--pr cannot be empty");
    }

    if let Some((repo, number)) = parse_pr_url(raw).or_else(|| parse_owner_repo_number(raw)) {
        return Ok(PrSpec {
            repo_full_name: normalize_repo(&repo)?,
            number,
        });
    }

    if raw.chars().all(|ch| ch.is_ascii_digit()) {
        let number = raw.parse::<u64>().context("failed to parse PR number")?;
        let repo = repo_hint
            .map(str::to_owned)
            .or_else(|| repo_from_remote(cwd, "upstream"))
            .or_else(|| {
                difflore_core::git::detect_github_repo_full_names(&cwd.to_string_lossy())
                    .into_iter()
                    .next()
            })
            .context("could not infer GitHub repo for --pr; pass --repo OWNER/REPO")?;
        return Ok(PrSpec {
            repo_full_name: normalize_repo(&repo)?,
            number,
        });
    }

    bail!("invalid --pr `{raw}`; expected number, OWNER/REPO#NUMBER, or GitHub PR URL")
}

fn parse_pr_url(raw: &str) -> Option<(String, u64)> {
    let after_host = raw
        .strip_prefix("https://github.com/")
        .or_else(|| raw.strip_prefix("http://github.com/"))?;
    let parts: Vec<&str> = after_host
        .split(['/', '?', '#'])
        .filter(|part| !part.is_empty())
        .collect();
    if parts.len() < 4 || parts[2] != "pull" {
        return None;
    }
    let number = parts[3].parse().ok()?;
    Some((format!("{}/{}", parts[0], parts[1]), number))
}

fn parse_owner_repo_number(raw: &str) -> Option<(String, u64)> {
    let (repo, number) = raw.split_once('#')?;
    let number = number.parse().ok()?;
    Some((repo.to_owned(), number))
}

fn normalize_repo(repo: &str) -> anyhow::Result<String> {
    validate_owner_repo(repo).map_err(|msg| anyhow::anyhow!("invalid repo `{repo}`: {msg}"))?;
    Ok(repo.to_ascii_lowercase())
}

fn repo_from_remote(repo_root: &Path, remote: &str) -> Option<String> {
    let url = git_stdout(repo_root, &["remote", "get-url", remote], None).ok()?;
    difflore_core::git::parse_github_remote_url(&url)
}

fn head_repo_full_name(view: &GhPrView) -> Option<String> {
    let name_with_owner = view
        .head_repository
        .as_ref()
        .and_then(|repo| repo.name_with_owner.as_deref())
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if let Some(repo) = name_with_owner {
        return Some(repo.to_ascii_lowercase());
    }

    let owner = view
        .head_repository_owner
        .as_ref()
        .map(|owner| owner.login.trim())
        .filter(|value| !value.is_empty());
    let name = view
        .head_repository
        .as_ref()
        .and_then(|repo| repo.name.as_deref())
        .map(str::trim)
        .filter(|value| !value.is_empty());
    match (owner, name) {
        (Some(owner), Some(name)) => Some(format!("{owner}/{name}").to_ascii_lowercase()),
        _ => None,
    }
}

fn fetch_pr_metadata(
    repo_root: &Path,
    spec: &PrSpec,
    timeout: Option<Duration>,
) -> anyhow::Result<PrMetadata> {
    which::which("gh").context(
        "GitHub CLI (`gh`) is required for `difflore fix --pr`.\n  Install: https://cli.github.com/\n  Then authenticate: gh auth login",
    )?;
    let json = run_command(
        repo_root,
        "gh",
        &[
            "pr",
            "view",
            &spec.number.to_string(),
            "--repo",
            &spec.repo_full_name,
            "--json",
            "number,title,baseRefName,baseRefOid,headRefName,headRefOid,headRepository,headRepositoryOwner",
        ],
        timeout,
    )?;
    let view: GhPrView =
        serde_json::from_str(&json).context("failed to parse `gh pr view` metadata")?;
    let head_repo = head_repo_full_name(&view).unwrap_or_else(|| spec.repo_full_name.clone());
    let head_url = view
        .head_repository
        .as_ref()
        .and_then(|repo| repo.url.as_deref())
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .map_or_else(|| format!("https://github.com/{head_repo}"), str::to_owned);
    let head_clone_url = if head_url.ends_with(".git") {
        head_url
    } else {
        format!("{head_url}.git")
    };

    // GitHub-supplied refs/OIDs flow into git as revisions. Reject values
    // git could misparse as options before any downstream call sees them.
    for (value, what) in [
        (&view.base_ref_name, "PR base ref"),
        (&view.base_ref_oid, "PR base SHA"),
        (&view.head_ref_name, "PR head ref"),
        (&view.head_ref_oid, "PR head SHA"),
    ] {
        difflore_core::infra::git::reject_option_like_revision(value, what)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    // Fork clone URLs are also git arguments; reject option-looking values.
    difflore_core::infra::git::reject_option_like_revision(&head_clone_url, "PR head clone URL")
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    Ok(PrMetadata {
        repo_full_name: spec.repo_full_name.clone(),
        number: view.number,
        title: view.title.unwrap_or_default(),
        base_ref: view.base_ref_name,
        base_sha: view.base_ref_oid,
        head_ref: view.head_ref_name,
        head_sha: view.head_ref_oid,
        head_repo_full_name: head_repo.clone(),
        head_clone_url,
        is_fork: head_repo != spec.repo_full_name,
    })
}

fn ensure_clean_worktree(repo_root: &Path, allow_dirty: bool) -> anyhow::Result<()> {
    if allow_dirty {
        return Ok(());
    }
    let status = git_stdout(repo_root, &["status", "--porcelain"], None)?;
    if !status.trim().is_empty() {
        bail!(
            "Your working tree has uncommitted changes.\n\nDifflore needs a clean tree before fixing a PR locally.\nCommit/stash your work, or rerun with --allow-dirty if you know what you are doing.\n\nSuggested:\n  git status\n  git stash push -u -m \"before difflore pr fix\""
        );
    }
    Ok(())
}

fn prompt_pr_checkout(meta: &PrMetadata, work_branch: &str, yes: bool) -> anyhow::Result<bool> {
    if yes {
        return Ok(true);
    }
    if !io::stdin().is_terminal() {
        bail!(
            "`difflore fix --pr` needs confirmation before switching branches; rerun with --yes in non-interactive mode"
        );
    }

    println!();
    println!("DiffLore will fix PR #{} locally.", meta.number);
    println!();
    println!("Repository: {}", meta.repo_full_name);
    println!("Title:      {}", meta.title);
    println!("Base:       {}", meta.base_ref);
    println!("PR head:    {}:{}", meta.head_repo_full_name, meta.head_ref);
    println!("Work branch: {work_branch}");
    println!();
    println!("This will:");
    println!("  - fetch the PR branch");
    println!("  - switch your working tree to {work_branch}");
    println!("  - analyze the PR diff against {}", meta.base_ref);
    println!("  - apply local fixes only");
    println!("  - leave review, commit, and push to you");
    println!();
    println!("It will NOT:");
    println!("  - push commits");
    println!("  - open a PR");
    println!("  - post GitHub comments");
    println!("  - modify the remote branch");
    println!();
    print!("Continue? [y/N] ");
    io::stdout().flush().ok();

    let mut input = String::new();
    io::stdin().lock().read_line(&mut input)?;
    let answer = input.trim().to_ascii_lowercase();
    Ok(answer == "y" || answer == "yes")
}

fn fetch_and_checkout_pr(
    repo_root: &Path,
    meta: &PrMetadata,
    work_branch: &str,
) -> anyhow::Result<PreparedPrFix> {
    let refs = fetch_pr_refs(repo_root, meta, None)?;

    run_git(
        repo_root,
        &["switch", "-C", work_branch, &refs.head_ref],
        None,
    )
    .with_context(|| format!("failed to switch to {work_branch}"))?;
    let merge_base = git_stdout(repo_root, &["merge-base", "HEAD", &refs.base_ref], None)
        .context("failed to compute PR merge-base")?;

    Ok(prepared_from_meta(
        repo_root,
        meta,
        work_branch.to_owned(),
        merge_base,
        "HEAD".to_owned(),
        true,
    ))
}

struct FetchedPrRefs {
    base_ref: String,
    head_ref: String,
}

fn fetch_pr_refs(
    repo_root: &Path,
    meta: &PrMetadata,
    timeout: Option<Duration>,
) -> anyhow::Result<FetchedPrRefs> {
    let base_remote =
        remote_for_repo(repo_root, &meta.repo_full_name)?.unwrap_or_else(|| "origin".to_owned());
    let base_remote_ref = format!("refs/remotes/{base_remote}/{}", meta.base_ref);
    run_git(
        repo_root,
        &[
            "fetch",
            &base_remote,
            &format!("+refs/heads/{}:{base_remote_ref}", meta.base_ref),
        ],
        timeout,
    )
    .with_context(|| format!("failed to fetch base ref {}", meta.base_ref))?;

    let head_remote_ref = if meta.is_fork {
        format!("refs/remotes/difflore/pr/{}", meta.number)
    } else {
        format!("refs/remotes/{base_remote}/pr/{}", meta.number)
    };
    if meta.is_fork {
        run_git(
            repo_root,
            &[
                "fetch",
                &meta.head_clone_url,
                &format!("+refs/heads/{}:{head_remote_ref}", meta.head_ref),
            ],
            timeout,
        )
        .with_context(|| format!("failed to fetch fork PR head {}", meta.head_ref))?;
    } else {
        run_git(
            repo_root,
            &[
                "fetch",
                &base_remote,
                &format!("+refs/pull/{}/head:{head_remote_ref}", meta.number),
            ],
            timeout,
        )
        .with_context(|| format!("failed to fetch PR #{} head", meta.number))?;
    }

    Ok(FetchedPrRefs {
        base_ref: base_remote_ref,
        head_ref: head_remote_ref,
    })
}

fn prepare_preview_pr(
    repo_root: &Path,
    meta: &PrMetadata,
    timeout: Option<Duration>,
) -> anyhow::Result<PreparedPrFix> {
    let refs = fetch_pr_refs(repo_root, meta, timeout)?;
    let merge_base = git_stdout(
        repo_root,
        &["merge-base", &refs.head_ref, &refs.base_ref],
        timeout,
    )
    .context("failed to compute PR merge-base")?;

    Ok(prepared_from_meta(
        repo_root,
        meta,
        format!("{}:{}", meta.head_repo_full_name, meta.head_ref),
        merge_base,
        refs.head_ref,
        false,
    ))
}

fn prepare_current_head_pr(repo_root: &Path, meta: &PrMetadata) -> anyhow::Result<PreparedPrFix> {
    let work_branch = git_stdout(repo_root, &["branch", "--show-current"], None)
        .unwrap_or_else(|_| "HEAD".to_owned());
    ensure_current_head_contains_pr_head(repo_root, meta)?;
    let merge_base = merge_base_for_current_head(repo_root, meta)?;
    Ok(prepared_from_meta(
        repo_root,
        meta,
        work_branch,
        merge_base,
        "HEAD".to_owned(),
        false,
    ))
}

fn ensure_current_head_contains_pr_head(repo_root: &Path, meta: &PrMetadata) -> anyhow::Result<()> {
    if run_git(
        repo_root,
        &["merge-base", "--is-ancestor", &meta.head_sha, "HEAD"],
        None,
    )
    .is_err()
    {
        bail!(
            "--no-checkout requires the current checkout to contain PR #{} head {}.\nRemove --no-checkout so DiffLore can checkout the PR, or switch to the branch/commit that contains the target PR head.",
            meta.number,
            meta.head_sha
        );
    }
    Ok(())
}

fn prepared_from_meta(
    repo_root: &Path,
    meta: &PrMetadata,
    work_branch: String,
    merge_base: String,
    diff_head_ref: String,
    checked_out: bool,
) -> PreparedPrFix {
    let project_path = repo_root.to_string_lossy().to_string();
    let scope_label = if diff_head_ref == "HEAD" {
        format!("PR #{} ({}...HEAD)", meta.number, meta.base_ref)
    } else {
        format!(
            "PR #{} ({}...{})",
            meta.number, meta.base_ref, meta.head_ref
        )
    };
    let mut aliases = vec![meta.repo_full_name.clone()];
    if !aliases.iter().any(|repo| repo == &meta.head_repo_full_name) {
        aliases.push(meta.head_repo_full_name.clone());
    }
    for repo in difflore_core::git::detect_github_repo_full_names(&project_path) {
        if !aliases.iter().any(|existing| existing == &repo) {
            aliases.push(repo);
        }
    }

    PreparedPrFix {
        repo_full_name: meta.repo_full_name.clone(),
        pr_number: meta.number,
        title: meta.title.clone(),
        base_ref: meta.base_ref.clone(),
        head_sha: meta.head_sha.clone(),
        work_branch,
        merge_base,
        diff_head_ref,
        scope_label,
        review_id: format!("github-pr:{}#{}", meta.repo_full_name, meta.number),
        repo_full_name_aliases: aliases,
        repo_root: repo_root.to_path_buf(),
        diff_records: Vec::new(),
        checked_out,
    }
}

async fn collect_pr_diff(
    repo_root: &Path,
    prepared: &PreparedPrFix,
) -> anyhow::Result<Vec<DiffContentRecord>> {
    difflore_core::git::diff(GitDiffInput {
        project_path: repo_root.to_string_lossy().to_string(),
        staged: None,
        ref1: Some(prepared.merge_base.clone()),
        ref2: Some(prepared.diff_head_ref.clone()),
    })
    .await
    .context("failed to collect PR diff")
}

fn merge_base_for_current_head(repo_root: &Path, meta: &PrMetadata) -> anyhow::Result<String> {
    let mut candidates = Vec::new();
    candidates.push(meta.base_ref.clone());
    if !meta.base_ref.contains('/') {
        if let Some(remote) = remote_for_repo(repo_root, &meta.repo_full_name)? {
            candidates.push(format!("{remote}/{}", meta.base_ref));
        }
        candidates.push(format!("origin/{}", meta.base_ref));
    }
    candidates.push(meta.base_sha.clone());

    for candidate in candidates {
        if let Ok(merge_base) = git_stdout(repo_root, &["merge-base", "HEAD", &candidate], None)
            && !merge_base.trim().is_empty()
        {
            return Ok(merge_base);
        }
    }
    bail!(
        "could not compute merge-base for PR #{} against `{}`. Fetch the base branch or omit --no-checkout.",
        meta.number,
        meta.base_ref,
    )
}

fn default_work_branch(pr_number: u64) -> String {
    format!("difflore/pr-{pr_number}-fix")
}

fn git_repo_root(cwd: &Path) -> anyhow::Result<PathBuf> {
    let root = git_stdout(cwd, &["rev-parse", "--show-toplevel"], None)
        .context("`difflore fix --pr` must be run inside a git repository")?;
    Ok(PathBuf::from(root))
}

fn remote_for_repo(repo_root: &Path, repo_full_name: &str) -> anyhow::Result<Option<String>> {
    let remotes = git_stdout(repo_root, &["remote"], None)?;
    for remote in remotes
        .lines()
        .map(str::trim)
        .filter(|remote| !remote.is_empty())
    {
        let Ok(url) = git_stdout(repo_root, &["remote", "get-url", remote], None) else {
            continue;
        };
        let Some(remote_repo) = difflore_core::git::parse_github_remote_url(&url) else {
            continue;
        };
        if remote_repo == repo_full_name {
            return Ok(Some(remote.to_owned()));
        }
    }
    Ok(None)
}

fn git_stdout(
    repo_root: &Path,
    args: &[&str],
    timeout: Option<Duration>,
) -> anyhow::Result<String> {
    run_command(repo_root, "git", args, timeout)
}

fn run_git(repo_root: &Path, args: &[&str], timeout: Option<Duration>) -> anyhow::Result<()> {
    run_command(repo_root, "git", args, timeout).map(|_| ())
}

fn run_command(
    repo_root: &Path,
    program: &str,
    args: &[&str],
    timeout: Option<Duration>,
) -> anyhow::Result<String> {
    ensure_pr_fix_command_allowed(program, args)?;
    let started = Instant::now();
    let output = if let Some(timeout) = timeout {
        run_command_with_timeout(repo_root, program, args, timeout)?
    } else {
        Command::new(program)
            .args(args)
            .current_dir(repo_root)
            .output()
            .with_context(|| format!("failed to run `{program}`"))?
    };
    fix_debug!(
        "pr_prepare command=`{} {}` elapsed={}ms timeout={}",
        program,
        args.join(" "),
        duration_ms(started.elapsed()),
        timeout.map_or_else(
            || "none".to_owned(),
            |duration| format!("{}ms", duration_ms(duration))
        )
    );
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let detail = if stderr.is_empty() { stdout } else { stderr };
        bail!("`{} {}` failed: {detail}", program, args.join(" "));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn run_command_with_timeout(
    repo_root: &Path,
    program: &str,
    args: &[&str],
    timeout: Duration,
) -> anyhow::Result<Output> {
    let mut child = Command::new(program)
        .args(args)
        .current_dir(repo_root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to run `{program}`"))?;
    let started = Instant::now();
    loop {
        if child.try_wait()?.is_some() {
            return child
                .wait_with_output()
                .with_context(|| format!("failed to collect `{program}` output"));
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "`{} {}` timed out after {}ms in preview PR preparation",
                program,
                args.join(" "),
                duration_ms(timeout)
            );
        }
        std::thread::sleep(PR_COMMAND_POLL_INTERVAL);
    }
}

fn duration_ms(duration: Duration) -> u64 {
    duration.as_secs().saturating_mul(1000) + u64::from(duration.subsec_millis())
}

fn ensure_pr_fix_command_allowed(program: &str, args: &[&str]) -> anyhow::Result<()> {
    match program {
        "git" => {
            let Some(subcommand) = args.first().copied() else {
                bail!("internal error: empty git command in PR fix flow");
            };
            match subcommand {
                "branch" | "fetch" | "merge-base" | "remote" | "rev-parse" | "status"
                | "switch" => Ok(()),
                _ => bail!(
                    "internal guard blocked `git {}` in PR fix flow; PR fixes must stay local and never commit, push, merge, rebase, or reset for the user",
                    args.join(" ")
                ),
            }
        }
        "gh" => match args {
            ["pr", "view", ..] => Ok(()),
            _ => bail!(
                "internal guard blocked `gh {}` in PR fix flow; PR fixes may read PR metadata but must not post comments or mutate GitHub state",
                args.join(" ")
            ),
        },
        _ => bail!(
            "internal guard blocked `{program}` in PR fix flow; only git and gh metadata commands are allowed"
        ),
    }
}

pub(super) fn print_pr_review_instructions(prepared: &PreparedPrFix) {
    println!();
    println!(
        "{} PR #{} local fix stayed in your working tree.",
        style::ok(sym::OK),
        prepared.pr_number,
    );
    println!("  Repo: {}", prepared.repo_full_name);
    println!("  Branch: {}", prepared.work_branch);
    println!("  Base: {}", prepared.base_ref);
    println!("  Head SHA: {}", prepared.head_sha);
    if !prepared.checked_out {
        println!("  Checkout: skipped via --no-checkout");
    }
    println!();
    println!("Review locally:");
    println!("  git diff");
    println!("  cargo test");
    println!("  git add -p");
    println!("  git commit -m \"Apply Difflore review-memory fixes\"");
    println!("  git push origin HEAD");
    println!();
    println!("Nothing was pushed or commented by Difflore.");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_number_with_repo_hint() {
        let spec = parse_pr_spec("123", Some("Acme/API"), Path::new(".")).unwrap();
        assert_eq!(spec.repo_full_name, "acme/api");
        assert_eq!(spec.number, 123);
    }

    #[test]
    fn preview_pr_mode_has_short_command_budget() {
        assert_eq!(
            preview_command_timeout(true),
            Some(PR_PREVIEW_COMMAND_TIMEOUT)
        );
        assert_eq!(preview_command_timeout(false), None);
        assert_eq!(duration_ms(PR_PREVIEW_COMMAND_TIMEOUT), 8_000);
        assert_eq!(duration_ms(PR_PREVIEW_DIFF_TIMEOUT), 5_000);
    }

    #[test]
    fn parses_number_from_upstream_remote_in_fork_clone() {
        let dir = tempfile::tempdir().unwrap();
        let status = Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success());
        for (name, url) in [
            ("origin", "https://github.com/difflore-fixtures/vite.git"),
            ("upstream", "https://github.com/vitejs/vite.git"),
        ] {
            let status = Command::new("git")
                .args(["remote", "add", name, url])
                .current_dir(dir.path())
                .status()
                .unwrap();
            assert!(status.success());
        }

        let spec = parse_pr_spec("22297", None, dir.path()).unwrap();

        assert_eq!(spec.repo_full_name, "vitejs/vite");
        assert_eq!(spec.number, 22297);
    }

    #[test]
    fn parses_owner_repo_number() {
        let spec = parse_pr_spec("acme/api#42", None, Path::new(".")).unwrap();
        assert_eq!(spec.repo_full_name, "acme/api");
        assert_eq!(spec.number, 42);
    }

    #[test]
    fn parses_pr_url() {
        let spec = parse_pr_spec(
            "https://github.com/acme/api/pull/42/files",
            None,
            Path::new("."),
        )
        .unwrap();
        assert_eq!(spec.repo_full_name, "acme/api");
        assert_eq!(spec.number, 42);
    }

    #[test]
    fn rejects_invalid_pr_spec() {
        assert!(parse_pr_spec("acme/api/pull/42", None, Path::new(".")).is_err());
    }

    #[test]
    fn pr_fix_command_guard_allows_only_metadata_and_local_prep() {
        for (program, args) in [
            ("gh", &["pr", "view", "123", "--repo", "acme/api"][..]),
            ("git", &["rev-parse", "--show-toplevel"][..]),
            ("git", &["status", "--porcelain"][..]),
            ("git", &["remote", "get-url", "origin"][..]),
            (
                "git",
                &[
                    "fetch",
                    "origin",
                    "+refs/heads/main:refs/remotes/origin/main",
                ][..],
            ),
            (
                "git",
                &[
                    "switch",
                    "-C",
                    "difflore/pr-123-fix",
                    "refs/remotes/origin/pr/123",
                ][..],
            ),
            ("git", &["merge-base", "HEAD", "origin/main"][..]),
            ("git", &["branch", "--show-current"][..]),
        ] {
            ensure_pr_fix_command_allowed(program, args).unwrap();
        }
    }

    #[test]
    fn pr_fix_command_guard_rejects_commits_pushes_and_github_writes() {
        for (program, args) in [
            ("git", &["commit", "-m", "apply fixes"][..]),
            ("git", &["push", "origin", "HEAD"][..]),
            ("git", &["merge", "origin/main"][..]),
            ("git", &["rebase", "origin/main"][..]),
            ("git", &["reset", "--hard"][..]),
            ("gh", &["pr", "comment", "123", "--body", "fixed"][..]),
            ("gh", &["api", "repos/acme/api/pulls/123/comments"][..]),
        ] {
            let err = ensure_pr_fix_command_allowed(program, args)
                .expect_err("command should be blocked");
            let msg = format!("{err:#}");
            assert!(
                msg.contains("blocked"),
                "unexpected error for {program} {}: {msg}",
                args.join(" ")
            );
        }
    }

    #[test]
    fn head_repo_falls_back_when_name_with_owner_is_empty() {
        let view = GhPrView {
            number: 22297,
            title: None,
            base_ref_name: "main".to_owned(),
            base_ref_oid: "base".to_owned(),
            head_ref_name: "feature".to_owned(),
            head_ref_oid: "head".to_owned(),
            head_repository: Some(GhRepo {
                name: Some("vite".to_owned()),
                name_with_owner: Some(String::new()),
                url: None,
            }),
            head_repository_owner: Some(GhRepoOwner {
                login: "sapphi-red".to_owned(),
            }),
        };

        assert_eq!(
            head_repo_full_name(&view).as_deref(),
            Some("sapphi-red/vite")
        );
    }
}
