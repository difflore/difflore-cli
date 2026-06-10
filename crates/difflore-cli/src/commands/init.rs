use crate::commands::providers::setup as providers_setup;
use crate::commands::util::{ensure_project, exit_err};
use crate::mcp_install;
use crate::runtime::CommandContext;
use crate::style::{self, sym};

/// Options for `difflore init`.
///
/// `init` is the first-time local setup path. `--check` is a readiness
/// preview that never writes.
///
/// Cloud login is handled by the explicit `difflore cloud login`
/// command, so `init` does not surprise-open a browser.
#[derive(Default, Clone, Copy)]
pub(crate) struct InitOptions {
    pub check: bool,
}

impl InitOptions {
    const fn run_agents(self) -> bool {
        !self.check
    }

    const fn run_provider(self) -> bool {
        !self.check
    }
}

/// `difflore init` — readiness summary + a single next best action.
///
/// Per the CLI redesign brief, `init` is the one safe command that
/// gets a user to local value. Output is shaped as:
///   `OK DiffLore initialized for <repo>`
///   `Readiness` block (repo / memory / agents / provider / cloud)
///   `Next best action` — one command.
pub(crate) async fn handle_init(ctx: &CommandContext, opts: InitOptions) {
    let cwd = match std::env::current_dir() {
        Ok(p) => p,
        Err(e) => exit_err(&format!("could not read current directory: {e}")),
    };
    let cwd_str = cwd.to_string_lossy().to_string();

    let git_dir = cwd.join(".git");
    let is_git = git_dir.is_dir() || git_dir.is_file();
    if !is_git {
        eprintln!(
            "{} {} `difflore init` expects to run inside a git repo (no .git found at {}).",
            style::warn(sym::WARN),
            style::warn("warning"),
            cwd.display(),
        );
    }

    let remote_url = crate::commands::util::git_str(&["config", "--get", "remote.origin.url"]);
    // [fork, upstream(s)…] — the same alias chain `fix --preview` uses, so the
    // memory preview resolves to upstream for a fork the user hasn't imported
    // reviews under yet.
    let repo_aliases = difflore_core::infra::git::detect_github_repo_full_names(&cwd.to_string_lossy());

    let db = &ctx.db;
    // Called for its side effect: registering the cwd in the projects table so
    // later commands have a project_id to bind to.
    let _project = ensure_project(db, &cwd_str).await;

    let repo_label = repo_aliases.first().cloned().unwrap_or_else(|| {
        // Fallback when detect failed and remote_url didn't parse: the
        // directory name keeps the header non-empty.
        cwd.file_name().map_or_else(
            || "this repo".to_owned(),
            |s| s.to_string_lossy().into_owned(),
        )
    });

    // Run setup steps before collecting snapshots so the readiness block
    // reflects the post-init state.
    if opts.run_agents() {
        mcp_install::install_all(false);
    }
    if opts.run_provider() {
        let has_active = difflore_core::domain::providers::list(db)
            .await
            .is_ok_and(|ps| ps.iter().any(|p| p.is_active));
        if !has_active {
            providers_setup::run_setup(db).await;
        }
    }
    if opts.check {
        println!(
            "{} {} DiffLore would initialize for {}",
            style::pewter(sym::BULLET),
            style::pewter("[--check]"),
            style::title(&repo_label),
        );
    } else {
        println!(
            "{} DiffLore initialized for {}",
            style::ok(sym::OK),
            style::title(&repo_label),
        );
    }
    println!();

    println!("{}", style::pewter("Readiness"));

    println!(
        "  {:<10} {}",
        style::pewter("repo"),
        style::title(&repo_label),
    );
    if let Some(url) = &remote_url {
        let safe_url = redact_remote_url(url);
        println!(
            "  {:<10} {}",
            style::pewter(""),
            style::pewter(&format!("origin: {safe_url}")),
        );
    }

    let cloud_client = ctx.cloud().await;
    let cloud_logged_in = cloud_client.is_logged_in();

    let total_rules = match difflore_core::skills::stats(db).await {
        Ok(s) => s.total,
        Err(_) => 0,
    };
    let memory_value = if total_rules == 0 {
        style::amber(&format!(
            "0 rules - run `{}`",
            memory_import_command(cloud_logged_in)
        ))
        .to_string()
    } else {
        style::title(&format!(
            "{} rule{}",
            total_rules,
            if total_rules == 1 { "" } else { "s" }
        ))
        .to_string()
    };
    println!("  {:<10} {}", style::pewter("memory"), memory_value);

    // Print a top-3 sample so the user sees concrete review judgments. Each
    // line ends with `<- from <repo>` (same framing as `fix --preview` and the
    // TUI) so the memory source is visible.
    if total_rules > 0 {
        let top = top_rules_preview(db, &repo_aliases, 3).await;
        for sample in &top {
            let suffix = sample.source_repo.as_deref().map_or_else(String::new, |r| {
                format!("  {}", style::pewter(&format!("<- from {r}")))
            });
            println!(
                "  {:<10} {} {}{suffix}",
                style::pewter(""),
                style::pewter(sym::BULLET),
                sample.name,
            );
        }
    }

    let snapshot = mcp_install::collect_status_snapshot();
    let installed = snapshot
        .clients
        .iter()
        .filter(|c| matches!(c.state, mcp_install::InstallState::Installed))
        .count();
    let detected = snapshot.clients.iter().filter(|c| c.detected).count();
    // Denominator = detected agents on this machine, not the full probe list.
    let agents_value = format!("{installed}/{detected} wired");
    println!(
        "  {:<10} {}",
        style::pewter("agents"),
        if installed > 0 {
            style::title(&agents_value).to_string()
        } else {
            style::amber(&agents_value).to_string()
        }
    );

    let providers = difflore_core::domain::providers::list(db).await.unwrap_or_default();
    let active = providers.iter().find(|p| p.is_active);
    let provider_value = match active {
        Some(p) => style::title(&format!("{} active", p.name)).to_string(),
        None => style::amber("not configured").to_string(),
    };
    println!("  {:<10} {}", style::pewter("provider"), provider_value);

    // Tier badge making the OSS/Cloud split visible — the OSS line is the only
    // place a casual user sees a pointer to what cloud unlocks.
    let cloud_status = fetch_cloud_status_for_init(cloud_client).await;
    let on_cloud_team = is_cloud_team(&cloud_status);
    let cloud_value = tier_badge_line(&cloud_status);
    let styled_cloud = if on_cloud_team {
        style::title(&cloud_value).to_string()
    } else {
        style::pewter(&cloud_value).to_string()
    };
    println!("  {:<10} {}", style::pewter("cloud"), styled_cloud);

    // OSS-mode "what cloud adds" block. Skipped on Cloud Team (already
    // converted).
    if !on_cloud_team {
        let pricing = difflore_core::cloud::endpoints::pricing_url();
        println!();
        println!("{}", style::pewter("Cloud Team adds (paid):"));
        println!(
            "  {} GitHub App team review history",
            style::pewter(sym::BULLET),
        );
        println!(
            "  {} Hot team rules + multi-device sync",
            style::pewter(sym::BULLET),
        );
        println!(
            "  {} Managed embeddings + accepted-edit dashboards",
            style::pewter(sym::BULLET),
        );
        println!("  {}", style::pewter(&pricing));
    }

    println!();
    println!("{}", style::pewter("Why this matters"));
    println!(
        "  {} Agents recall team review judgment before they edit, so fewer comments repeat.",
        style::pewter(sym::BULLET),
    );
    println!(
        "  {} Use {} to inspect accepted edits, then {} to see exact recall.",
        style::pewter(sym::BULLET),
        style::cmd("difflore status"),
        style::cmd("difflore recall --diff"),
    );

    let next = pick_next_best_action(total_rules, installed, active.is_some(), cloud_logged_in);
    println!();
    println!("{}", style::pewter("Next best action"));
    println!("  {}", style::cmd(next));
}

/// Return true when the user is on a paid Cloud Team plan. Unknown plan
/// slugs deliberately fall back to OSS/free messaging so a typo or
/// unreleased slug cannot suppress the upgrade prompt.
pub(crate) fn is_cloud_team(status: &difflore_core::cloud::sync::CloudStatus) -> bool {
    if !status.logged_in {
        return false;
    }
    matches!(
        status.plan.as_deref(),
        Some("team" | "team_plus" | "pro" | "business" | "enterprise")
    )
}

async fn fetch_cloud_status_for_init(
    client: &difflore_core::cloud::client::CloudClient,
) -> difflore_core::cloud::sync::CloudStatus {
    if !client.is_logged_in() {
        return difflore_core::cloud::sync::fetch_cloud_status(client).await;
    }
    match tokio::time::timeout(
        std::time::Duration::from_secs(2),
        difflore_core::cloud::sync::fetch_cloud_status(client),
    )
    .await
    {
        Ok(status) if status.logged_in => status,
        Ok(_) | Err(_) => difflore_core::cloud::sync::CloudStatus {
            logged_in: true,
            email: None,
            plan: None,
            team_id: None,
            team_name: None,
        },
    }
}

/// Render the one-line tier badge used in the `cloud:` row of the
/// readiness block (and the doctor cloud reachability section).
///
/// Two states, both fit in a single readiness row:
///   - OSS local mode → highlights what the user already has locally
///   - Cloud Team active → highlights what they're paying for
pub(crate) fn tier_badge_line(status: &difflore_core::cloud::sync::CloudStatus) -> String {
    if is_cloud_team(status) {
        "Cloud Team | multi-device sync + GitHub App team review history".to_owned()
    } else if status.logged_in {
        "Cloud Free | logged in | local runtime + upgrade path".to_owned()
    } else {
        "OSS | local-only | agent recall + on-device fix".to_owned()
    }
}

/// Pick the single highest-leverage next command for this user state.
///
/// Priority order:
///   1. No memory: import local candidates, uploading PR history if already logged in.
///   2. Memory but no agents wired: wire an agent so recall is reachable.
///   3. Memory + agents but no provider: set up a provider (unblocks fix).
///   4. All set: preview recall on the current diff.
const fn pick_next_best_action(
    total_rules: i64,
    installed_agents: usize,
    has_active_provider: bool,
    cloud_logged_in: bool,
) -> &'static str {
    if total_rules == 0 {
        memory_import_command(cloud_logged_in)
    } else if installed_agents == 0 {
        "difflore agents install"
    } else if !has_active_provider {
        "difflore providers setup"
    } else {
        "difflore recall --diff"
    }
}

const fn memory_import_command(cloud_logged_in: bool) -> &'static str {
    if cloud_logged_in {
        "difflore import-reviews --max-prs 50 --upload"
    } else {
        "difflore import-reviews --max-prs 50"
    }
}

/// One rule preview row used by `init`'s memory section. Lightweight
/// because the readiness block only needs the user-facing name and the
/// source_repo provenance.
struct RulePreview {
    name: String,
    source_repo: Option<String>,
}

/// Pick the top N rules for the `init` memory section. Prefers rules whose
/// `source_repo` matches one of `repo_aliases`; falls back to the
/// highest-confidence active rules corpus-wide (common for a fresh,
/// not-yet-imported fork) so the section is never empty.
async fn top_rules_preview(
    db: &difflore_core::SqlitePool,
    repo_aliases: &[String],
    limit: usize,
) -> Vec<RulePreview> {
    if limit == 0 {
        return Vec::new();
    }
    let limit_i = i64::try_from(limit).unwrap_or(3);
    let candidates: Vec<&str> = repo_aliases
        .iter()
        .map(String::as_str)
        .filter(|s| !s.trim().is_empty())
        .collect();

    if !candidates.is_empty() {
        let placeholders = std::iter::repeat_n("?", candidates.len())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT name, source_repo FROM skills \
             WHERE source_repo IN ({placeholders}) \
               AND COALESCE(status, 'active') = 'active' \
             ORDER BY confidence_score DESC, name ASC \
             LIMIT ?"
        );
        let mut q = sqlx::query_as::<_, (String, Option<String>)>(&sql);
        for repo in &candidates {
            q = q.bind(*repo);
        }
        q = q.bind(limit_i);
        if let Ok(rows) = q.fetch_all(db).await
            && !rows.is_empty()
        {
            return rows
                .into_iter()
                .map(|(name, source_repo)| RulePreview { name, source_repo })
                .collect();
        }
    }

    let global: Result<Vec<(String, Option<String>)>, sqlx::Error> = sqlx::query_as(
        "SELECT name, source_repo FROM skills \
         WHERE COALESCE(status, 'active') = 'active' \
         ORDER BY confidence_score DESC, name ASC \
         LIMIT ?1",
    )
    .bind(limit_i)
    .fetch_all(db)
    .await;
    global
        .unwrap_or_default()
        .into_iter()
        .map(|(name, source_repo)| RulePreview { name, source_repo })
        .collect()
}

/// Parse owner/repo from git remote URLs. Supports https + ssh forms.
pub(crate) fn parse_owner_repo_from_url(url: &str) -> Option<String> {
    let trimmed = url.trim().trim_end_matches(".git");
    if let Some(rest) = trimmed.split_once(':').map(|(_, r)| r)
        && rest.contains('/')
        && !rest.contains("://")
    {
        return Some(rest.to_owned());
    }
    if let Some(without_scheme) = trimmed.split("://").nth(1) {
        let mut parts = without_scheme.splitn(2, '/');
        parts.next()?;
        let path = parts.next()?;
        if path.contains('/') {
            return Some(path.to_owned());
        }
    }
    None
}

fn redact_remote_url(url: &str) -> String {
    let trimmed = url.trim();
    let Some((scheme, rest)) = trimmed.split_once("://") else {
        return trimmed.to_owned();
    };
    let Some((userinfo, host_and_path)) = rest.split_once('@') else {
        return trimmed.to_owned();
    };
    if userinfo.is_empty() || host_and_path.is_empty() {
        return trimmed.to_owned();
    }
    format!("{scheme}://***@{host_and_path}")
}

#[cfg(test)]
mod tests {
    use super::{
        InitOptions, is_cloud_team, memory_import_command, redact_remote_url, tier_badge_line,
    };
    use difflore_core::cloud::sync::CloudStatus;

    fn status(logged_in: bool, plan: Option<&str>) -> CloudStatus {
        CloudStatus {
            logged_in,
            email: None,
            plan: plan.map(String::from),
            team_id: None,
            team_name: None,
        }
    }

    #[test]
    fn tier_badge_oss_when_not_logged_in() {
        let s = status(false, None);
        assert!(!is_cloud_team(&s));
        let line = tier_badge_line(&s);
        assert!(line.starts_with("OSS"), "unexpected: {line}");
        assert!(line.contains("local-only"));
        assert!(line.contains("agent recall"));
    }

    #[test]
    fn tier_badge_oss_when_logged_in_but_free() {
        // Logged in to a free / self-host plan is still OSS-tier; the
        // conversion line must still appear in the init block.
        for plan in ["free", "self_host", "typo_future_plan"] {
            let s = status(true, Some(plan));
            assert!(!is_cloud_team(&s), "plan {plan} should not be team-tier");
            let line = tier_badge_line(&s);
            assert!(line.starts_with("Cloud Free"), "unexpected: {line}");
            assert!(line.contains("logged in"));
        }
    }

    #[test]
    fn tier_badge_team_when_paid_plan() {
        for plan in ["team", "team_plus", "pro", "business", "enterprise"] {
            let s = status(true, Some(plan));
            assert!(is_cloud_team(&s), "plan {plan} should be team-tier");
            let line = tier_badge_line(&s);
            assert!(line.starts_with("Cloud Team"), "unexpected: {line}");
            assert!(line.contains("multi-device sync"));
            assert!(line.contains("GitHub App team review history"));
        }
    }

    #[test]
    fn init_runs_local_setup_steps_by_default() {
        let opts = InitOptions::default();
        assert!(opts.run_agents());
        assert!(opts.run_provider());
    }

    #[test]
    fn memory_import_command_is_single_source_for_zero_rule_next_step() {
        assert_eq!(
            memory_import_command(false),
            "difflore import-reviews --max-prs 50"
        );
        assert_eq!(
            memory_import_command(true),
            "difflore import-reviews --max-prs 50 --upload"
        );
    }

    #[test]
    fn redact_remote_url_masks_https_userinfo() {
        assert_eq!(
            redact_remote_url("https://oauth2:secret@github.com/org/repo.git"),
            "https://***@github.com/org/repo.git"
        );
        assert_eq!(
            redact_remote_url("git@github.com:org/repo.git"),
            "git@github.com:org/repo.git"
        );
    }
}
