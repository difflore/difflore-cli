#![allow(clippy::exit)]

use std::process;

use colored::Colorize;

use difflore_core::domain::models::{AddProjectInput, ProjectRecord};

pub(crate) async fn ensure_project(db: &difflore_core::SqlitePool, path: &str) -> ProjectRecord {
    let input = AddProjectInput {
        path: path.to_owned(),
    };
    match difflore_core::domain::projects::add(db, input).await {
        Ok(p) => p,
        Err(e) => exit_err(&format!("Failed to register project: {e}")),
    }
}

pub(crate) fn git_str(args: &[&str]) -> Option<String> {
    let out = process::Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

#[cfg(test)]
pub(crate) fn git_str_in(cwd: &str, args: &[&str]) -> Option<String> {
    let out = process::Command::new("git")
        .args(args)
        .current_dir(cwd)
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
pub(crate) fn confirm_destructive(yes: bool, prompt: &str) {
    if yes {
        return;
    }
    use std::io::{BufRead, IsTerminal, Write};
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        // Don't hint `--dry-run`: only some destructive commands accept it.
        exit_err(
            "destructive action requires confirmation. \
             Re-run with `--yes` to skip the prompt (non-interactive).",
        );
    }
    print!("{} {} [y/N] ", "?".yellow().bold(), prompt);
    if let Err(e) = std::io::stdout().flush() {
        exit_err(&format!("failed to write confirmation prompt: {e}"));
    }
    let mut buf = String::new();
    if stdin.lock().read_line(&mut buf).is_err() {
        exit_err("failed to read confirmation");
    }
    let ans = buf.trim().to_ascii_lowercase();
    if ans != "y" && ans != "yes" {
        eprintln!("aborted.");
        process::exit(1);
    }
}

pub(crate) async fn init_db() -> difflore_core::SqlitePool {
    match difflore_core::infra::db::init_db().await {
        Ok(pool) => pool,
        Err(e) => {
            // Stale-DB-across-versions case: sqlx surfaces an opaque
            // "migration NNNN was previously applied but is missing" when the
            // on-disk DB has migrations this binary doesn't know about.
            // Translate to actionable copy.
            if e.contains("was previously applied but is missing") {
                exit_err(
                    "DiffLore's local database was created by a different version and can't be \
                     opened by this binary.\n  \
                     Fix: back up `~/.difflore/data.db`, then remove it and re-run this command. \
                     Local-only memory in that file will be lost; cloud-synced memory restores via \
                     `difflore cloud sync` after login.",
                );
            }
            exit_err(&format!("Failed to initialize database: {e}"));
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
