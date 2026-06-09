#![allow(clippy::exit)] // reason: this module hosts the centralized exit helpers (`exit_err`, `confirm_destructive`).
#![allow(clippy::items_after_test_module)] // reason: test module is intentionally inline; downstream items are tightly related helpers.

use std::process;

use colored::Colorize;

use difflore_core::models::{AddProjectInput, ProjectRecord};

pub(crate) async fn ensure_project(db: &difflore_core::SqlitePool, path: &str) -> ProjectRecord {
    let input = AddProjectInput {
        path: path.to_owned(),
    };
    match difflore_core::projects::add(db, input).await {
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

/// Resolve the active project root for CLI commands. Routes through
/// `difflore_core::paths::current_project_root` so we get the git
/// toplevel with cwd as a safe fallback.
pub(crate) fn project_path() -> String {
    difflore_core::paths::current_project_root()
        .to_string_lossy()
        .into_owned()
}

pub(crate) fn exit_err(msg: &str) -> ! {
    eprintln!("{} {}", "error:".red().bold(), msg);
    process::exit(1);
}

pub(crate) fn format_recall_edit_proof_breakdown(
    rule_recall: i64,
    mcp_rule_serve: i64,
    edit_attribution: i64,
) -> String {
    let mut parts = Vec::new();
    push_counted(&mut parts, rule_recall, "rule recall", "rule recalls");
    push_counted(&mut parts, mcp_rule_serve, "MCP serve", "MCP serves");
    push_counted(
        &mut parts,
        edit_attribution,
        "edit attribution",
        "edit attributions",
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

/// Reject obviously-malformed OWNER/REPO values before they're pasted
/// into a GitHub query or used as a SQL filter. Format: exactly one
/// slash, both halves non-empty, characters limited to alphanumeric /
/// `.` / `-` / `_` (matching what GitHub allows in repo names).
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

#[cfg(test)]
mod project_path_tests {
    use super::project_path;

    #[test]
    fn project_path_matches_core_resolver() {
        // `project_path()` must stay a thin wrapper over the core resolver.
        let got = project_path();
        let expected = difflore_core::paths::current_project_root()
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

/// Gate a destructive command per the Destructive Command Policy in the
/// CLI redesign brief: skip the prompt when `--yes` is passed, otherwise
/// require an interactive y/N. Non-TTY without `--yes` is a hard error so
/// scripts don't accidentally hang or auto-proceed.
pub(crate) fn confirm_destructive(yes: bool, prompt: &str) {
    if yes {
        return;
    }
    use std::io::{BufRead, IsTerminal, Write};
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        // Previously hinted at "or `--dry-run` (preview only)" too, but
        // `--dry-run` is only wired on a subset of destructive commands
        // (`ingest`, `rules dedup`, etc.) — `candidates accept` and
        // others don't accept it, so following the hint produced
        // `error: unexpected argument '--dry-run' found`. Sticking to
        // the universally-available flag keeps the message truthful for
        // every caller.
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
    match difflore_core::db::init_db().await {
        Ok(pool) => pool,
        Err(e) => {
            // Detect the stale-DB-across-versions case: sqlx surfaces a
            // very opaque "migration NNNN was previously applied but is
            // missing in the resolved migrations" error when the on-disk
            // DB has a migration history a fresh binary doesn't know
            // about. Translate to actionable copy instead of dumping the
            // raw sqlx wording.
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

/// Clamp a numeric CLI arg into `[lo, hi]` and emit a one-line stderr
/// warning when the value was actually adjusted. Skipped under `--json`
/// so structured consumers don't choke on the extra line. Phrasing
/// matches the original ad-hoc blocks in `search.rs` / `import_reviews`.
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

/// Pretty-print `value` as JSON, falling back to `fallback` on serializer error.
/// JSON serialization of types in this crate effectively never fails — the
/// fallback exists for the rare case (NaN floats, etc.) and to keep call
/// sites concise.
pub(crate) fn json_or(value: &impl serde::Serialize, fallback: &'static str) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| fallback.to_owned())
}

/// Compact-JSON sibling of [`json_or`] — uses `serde_json::to_string` (no
/// indentation), used by hook adapters whose output is consumed by host
/// tooling line-by-line.
pub(crate) fn json_compact_or(value: &impl serde::Serialize, fallback: &'static str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| fallback.to_owned())
}

/// Flatten `DiffContentRecord` into unified-diff text shared by `fix`
/// and `rules audit`.
pub(crate) fn diff_records_to_string(
    records: &[difflore_core::models::DiffContentRecord],
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
