//! Since-last-session banner.
//!
//! Emits a short, agent-visible note when the current repo has gained rules
//! since the last `SessionStart` fired for it. Plugs into every platform adapter
//! via the `additional_context` field on `HookResult`.
//!
//! Constraints:
//! * Runs in the `SessionStart` hot path, budgeted to 50ms p99 via
//!   `tokio::time::timeout`; any step crossing that budget yields `None`.
//! * Returns `None` when no new rules exist, so quiet sessions stay noise-free.
//! * Per-repo JSON watermark at
//!   `~/.difflore/projects/{hash}/last-session-start.json`. A new repo starts
//!   with `prev_ts = None` (first session shows everything to date, capped at
//!   5), then the watermark advances to "now".
//! * DB read failures are swallowed (return `None`) — hooks must never block
//!   the agent.

pub mod query;
pub mod render;
pub mod watermark;

#[cfg(test)]
mod tests;

use std::time::Duration;

/// Inputs the banner helper needs, built from the `HookEvent::SessionStart`
/// payload at the dispatch site.
#[derive(Debug, Clone)]
pub struct BannerContext {
    /// Absolute path to the agent's working directory. Resolves the project
    /// root → project hash → watermark file and the GitHub repo aliases that
    /// filter the query.
    pub cwd: String,
    /// Platform adapter name. Stored in the watermark JSON for debug only; the
    /// query treats every client identically.
    pub client_name: String,
}

/// Wall-clock p99 budget for the entire banner pipeline; exceeding it yields
/// `None` so a slow disk doesn't stall the agent.
const BANNER_BUDGET: Duration = Duration::from_millis(50);

/// Max rules listed inside the banner, to stay within the 6-line / 400-char
/// shape. Most repos accumulate <5 new rules between sessions anyway.
const MAX_RULES_IN_BANNER: usize = 5;

/// Build the "since-last-session" banner. Returns `None` when there is nothing
/// new to show OR any pipeline step failed — both mean "emit nothing".
///
/// The pipeline:
///   1. Resolve the project root + repo aliases from `ctx.cwd`.
///   2. Read the watermark (`prev_ts`) — `None` on first session.
///   3. Open `data.db` and query for rules with `installed_at > prev_ts`
///      whose `source_repo` matches one of this repo's aliases.
///   4. Advance the watermark to "now" (best-effort).
///   5. Format the rows into the banner string.
///
/// Fenced by `tokio::time::timeout` against `BANNER_BUDGET`.
pub async fn render_since_last_session_banner(ctx: &BannerContext) -> Option<String> {
    // `Result::ok()` collapses a timeout `Err(Elapsed)` into `None` — the
    // swallow-on-stall semantics the hot path needs. A timeout and a genuine
    // no-rules case both render as "emit nothing".
    let result: Result<Option<String>, tokio::time::error::Elapsed> =
        tokio::time::timeout(BANNER_BUDGET, render_inner(ctx)).await;
    result.unwrap_or_default()
}

async fn render_inner(ctx: &BannerContext) -> Option<String> {
    let project_root = resolve_project_root(&ctx.cwd);
    let project_hash = difflore_core::infra::db::project_hash_from_root(&project_root);

    let repo_aliases = repo_aliases_for(&project_root);
    if repo_aliases.is_empty() {
        // No repo identity → no way to filter `source_repo`, so we'd spam every
        // rule from every repo. Bail rather than mislead.
        return None;
    }

    let prev_ts = watermark::read_watermark(&project_hash).map(|w| w.ts_ms);

    let Ok(db) = difflore_core::infra::db::init_db().await else {
        return None;
    };

    let rows = query::new_rules_since(&db, prev_ts, &repo_aliases, MAX_RULES_IN_BANNER)
        .await
        .ok()?;

    // Advance the watermark BEFORE the "no rows" early-return: a quiet session
    // still counts, otherwise the next fire would see `prev_ts = None` and show
    // every rule learned to date. Write failures are swallowed.
    let now_ms = chrono::Utc::now().timestamp_millis();
    let _ = watermark::write_watermark(
        &project_hash,
        &watermark::Watermark {
            ts_ms: now_ms,
            client: ctx.client_name.clone(),
        },
    );

    if rows.is_empty() {
        return None;
    }

    let prev_label = prev_ts
        .and_then(timestamp_to_rfc3339)
        .unwrap_or_else(|| "the start of this repo".to_owned());

    Some(render::format_banner(&rows, &prev_label))
}

/// Resolve the project root for `cwd` via `git rev-parse --show-toplevel`,
/// scoped to the agent's reported cwd. Falls back to `cwd` when git isn't
/// available.
fn resolve_project_root(cwd: &str) -> std::path::PathBuf {
    if cwd.is_empty() {
        return difflore_core::infra::db::current_project_root();
    }
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output();
    if let Ok(out) = output
        && out.status.success()
    {
        let s = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        if !s.is_empty() {
            return std::path::PathBuf::from(s);
        }
    }
    std::path::PathBuf::from(cwd)
}

/// Normalized lower-case `owner/repo` aliases for the project, matching
/// `commands::status::queries::normalized_repo_aliases` so the SQL filter joins
/// cleanly with `source_repo` values (also lowercased on write).
fn repo_aliases_for(project_root: &std::path::Path) -> Vec<String> {
    let raw =
        difflore_core::infra::git::detect_github_repo_full_names(&project_root.to_string_lossy());
    raw.into_iter()
        .map(|r| r.trim().to_ascii_lowercase())
        .filter(|r| !r.is_empty())
        .collect()
}

/// Render a unix-ms timestamp as RFC 3339 for the banner header. `None` if the
/// timestamp is outside chrono's representable range, where the caller falls
/// back to a generic phrase.
fn timestamp_to_rfc3339(ts_ms: i64) -> Option<String> {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ts_ms).map(|dt| dt.to_rfc3339())
}
