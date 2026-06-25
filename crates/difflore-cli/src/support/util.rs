#![allow(clippy::exit)]

use std::process;

use colored::Colorize;

use difflore_core::domain::models::{AddProjectInput, ProjectRecord};

pub(crate) async fn ensure_project(
    db: &difflore_core::SqlitePool,
    path: &str,
) -> anyhow::Result<ProjectRecord> {
    let input = AddProjectInput {
        path: path.to_owned(),
    };
    difflore_core::domain::projects::add(db, input)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to register project: {e}"))
}

pub(crate) fn git_str(args: &[&str]) -> Option<String> {
    let out = difflore_core::infra::git::git_command(".")
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

pub(crate) fn git_str_in(cwd: &str, args: &[&str]) -> Option<String> {
    let out = difflore_core::infra::git::git_command(cwd)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Resolve the active project root for CLI commands: the git toplevel, with
/// cwd as fallback (via `difflore_core::infra::paths::current_project_root`).
pub(crate) fn project_path() -> String {
    difflore_core::infra::paths::current_project_root()
        .to_string_lossy()
        .into_owned()
}

pub(crate) fn exit_err(msg: &str) -> ! {
    eprintln!("{} {}", "error:".red().bold(), msg);
    process::exit(1);
}

/// The single error boundary for `Result`-returning command handlers: render
/// the error once on stderr and exit with code 1. Dispatch arms apply this via
/// `handler(...).await.unwrap_or_else(render_cli_error)`, so handler logic stays
/// composable (returns `anyhow::Result<()>`) while error rendering and the exit
/// code live in exactly one place.
#[allow(clippy::needless_pass_by_value)] // `-> !` terminal sink; by-value is fine
pub(crate) fn render_cli_error(err: anyhow::Error) -> ! {
    // `{:#}` includes the anyhow context chain on one line, matching how the
    // former inline `exit_err(&format!("…: {e}"))` sites rendered context.
    exit_err(&format!("{err:#}"));
}

pub(crate) fn exit_code(code: i32) -> ! {
    process::exit(code);
}

pub(crate) fn format_recall_edit_proof_breakdown(
    rule_recall: i64,
    mcp_rule_serve: i64,
    edit_attribution: i64,
) -> String {
    let mut parts = Vec::new();
    push_counted(&mut parts, rule_recall, "rule recall", "rule recalls");
    push_counted(&mut parts, mcp_rule_serve, "agent recall", "agent recalls");
    push_counted(
        &mut parts,
        edit_attribution,
        "accepted edit",
        "accepted edits",
    );
    if parts.is_empty() {
        String::new()
    } else {
        format!(" ({})", parts.join(" + "))
    }
}

fn push_counted(parts: &mut Vec<String>, count: i64, singular: &str, plural: &str) {
    if count <= 0 {
        return;
    }
    parts.push(format!(
        "{count} {}",
        if count == 1 { singular } else { plural }
    ));
}

/// Canonicalize a repo full name for case- and `.git`-insensitive comparison:
/// trim, drop a trailing `.git`, lowercase. Shared by `status` (scope/value
/// loop) and `doctor` (probes) so both agree on repo identity.
pub(crate) fn normalize_repo(repo: &str) -> String {
    repo.trim().trim_end_matches(".git").to_ascii_lowercase()
}

/// Count active rules whose canonical `source_repo` matches `repo`. `rules`
/// carry their id; `source_repos` maps rule id -> canonical source repo.
/// Returns 0 for an absent or empty `repo`. Shared by `status` and `doctor`.
pub(crate) fn count_rules_for_repo(
    rules: &[difflore_core::domain::models::SkillRecord],
    source_repos: &std::collections::HashMap<String, Option<String>>,
    repo: Option<&str>,
) -> i64 {
    let Some(repo) = repo.map(normalize_repo).filter(|repo| !repo.is_empty()) else {
        return 0;
    };

    rules
        .iter()
        .filter(|rule| {
            source_repos
                .get(&rule.id)
                .and_then(|repo| repo.as_deref())
                .map(str::trim)
                .filter(|repo| !repo.is_empty())
                .map(normalize_repo)
                .as_deref()
                == Some(repo.as_str())
        })
        .count() as i64
}

/// Reject malformed OWNER/REPO before it reaches a GitHub query or SQL
/// filter. Requires exactly one slash, non-empty halves, and characters
/// limited to alphanumeric / `.` / `-` / `_` (what GitHub allows).
pub(crate) fn validate_owner_repo(s: &str) -> Result<(), &'static str> {
    let (owner, repo) = s
        .split_once('/')
        .ok_or("expected OWNER/REPO with one '/'")?;
    if owner.is_empty() || repo.is_empty() {
        return Err("owner and repo must both be non-empty");
    }
    if repo.contains('/') {
        return Err("expected exactly one '/' between owner and repo");
    }
    let ok = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.');
    if !owner.chars().all(ok) || !repo.chars().all(ok) {
        return Err("only alphanumeric, '-', '_', '.' are allowed");
    }
    Ok(())
}

/// Gate a destructive command: skip the prompt with `--yes`, otherwise
/// require an interactive y/N. Non-TTY without `--yes` is a hard error so
/// scripts don't hang or auto-proceed.
pub(crate) fn confirm_destructive(yes: bool, prompt: &str) -> anyhow::Result<()> {
    if yes {
        return Ok(());
    }
    use std::io::{BufRead, IsTerminal, Write};
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        // Don't hint `--dry-run`: only some destructive commands accept it.
        anyhow::bail!(
            "destructive action requires confirmation. \
             Re-run with `--yes` to skip the prompt (non-interactive)."
        );
    }
    print!("{} {} [y/N] ", "?".yellow().bold(), prompt);
    std::io::stdout()
        .flush()
        .map_err(|e| anyhow::anyhow!("failed to write confirmation prompt: {e}"))?;
    let mut buf = String::new();
    if stdin.lock().read_line(&mut buf).is_err() {
        anyhow::bail!("failed to read confirmation");
    }
    let ans = buf.trim().to_ascii_lowercase();
    if ans != "y" && ans != "yes" {
        // Aborting at a confirmation prompt is a user decision, not an error:
        // exit non-zero quietly (matching the prior `eprintln!("aborted.")`
        // + `process::exit(1)`), bypassing the `error:`-prefixed boundary.
        eprintln!("aborted.");
        process::exit(1);
    }
    Ok(())
}

pub(crate) async fn init_db() -> difflore_core::SqlitePool {
    match difflore_core::infra::db::init_db().await {
        Ok(pool) => pool,
        Err(e) => {
            let err = e.to_string();
            // Stale-DB-across-versions case: sqlx surfaces an opaque
            // "migration NNNN was previously applied but is missing" when the
            // on-disk DB has migrations this binary doesn't know about.
            // Translate to actionable copy.
            if err.contains("was previously applied but is missing") {
                exit_err(
                    "DiffLore's local database was created by a different version and can't be \
                     opened by this binary.\n  \
                     Fix: back up `~/.difflore/data.db`, then remove it and re-run this command. \
                     Local-only memory in that file will be lost; cloud-synced memory restores via \
                     `difflore cloud sync` after login.",
                );
            }
            exit_err(&format!("Failed to initialize database: {err}"));
        }
    }
}

/// Clamp a numeric CLI arg into `[lo, hi]`, warning on stderr when adjusted.
/// Skipped under `--json` so structured consumers don't see the extra line.
pub(crate) fn clamp_with_warn<T: Copy + Ord + std::fmt::Display>(
    name: &str,
    value: T,
    lo: T,
    hi: T,
    json: bool,
) -> T {
    let clamped = value.clamp(lo, hi);
    if clamped != value && !json {
        eprintln!(
            "{} {name} capped at {clamped} (requested {value}; valid range {lo}..={hi})",
            crate::style::amber(crate::style::sym::WARN),
        );
    }
    clamped
}

/// Pretty-print `value` as JSON, falling back to `fallback` on serializer
/// error (rare: NaN floats, etc.).
pub(crate) fn json_or(value: &impl serde::Serialize, fallback: &'static str) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| fallback.to_owned())
}

/// Compact-JSON sibling of [`json_or`] (no indentation), for hook adapters
/// whose output is consumed line-by-line.
pub(crate) fn json_compact_or(value: &impl serde::Serialize, fallback: &'static str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| fallback.to_owned())
}

/// Flatten `DiffContentRecord` into unified-diff text shared by `fix`
/// and `rules audit`.
pub(crate) fn diff_records_to_string(
    records: &[difflore_core::domain::models::DiffContentRecord],
) -> String {
    let mut out = String::new();
    for file in records {
        out.push_str(&format!(
            "--- a/{}\n+++ b/{}\n",
            file.file_path, file.file_path
        ));
        for hunk in &file.hunks {
            out.push_str(&hunk.header);
            out.push('\n');
            out.push_str(&hunk.body);
        }
    }
    out
}

#[cfg(test)]
mod project_path_tests {
    use super::project_path;

    #[test]
    fn project_path_matches_core_resolver() {
        let got = project_path();
        let expected = difflore_core::infra::paths::current_project_root()
            .to_string_lossy()
            .into_owned();
        assert_eq!(got, expected);
    }
}

#[cfg(test)]
mod owner_repo_tests {
    use super::validate_owner_repo;

    #[test]
    fn accepts_normal_repo() {
        assert!(validate_owner_repo("cli/cli").is_ok());
        assert!(validate_owner_repo("Org-Name/repo.name_v2").is_ok());
    }

    #[test]
    fn rejects_no_slash() {
        assert!(validate_owner_repo("not-a-repo").is_err());
    }

    #[test]
    fn rejects_extra_slash() {
        assert!(validate_owner_repo("owner/repo/extra").is_err());
    }

    #[test]
    fn rejects_empty_halves() {
        assert!(validate_owner_repo("/missing").is_err());
        assert!(validate_owner_repo("missing/").is_err());
    }

    #[test]
    fn rejects_bad_chars() {
        assert!(validate_owner_repo("owner/repo space").is_err());
        assert!(validate_owner_repo("owner/repo$").is_err());
    }
}
