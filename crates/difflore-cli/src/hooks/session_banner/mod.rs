//! Since-last-session banner.
//!
//! Emits a short, agent-visible note when the current repo has gained
//! rules since the last `SessionStart` fired for it. Plugs into every
//! platform adapter (Claude Code, Cursor, Gemini CLI, Windsurf) via the
//! `additional_context` field on `HookResult` — adapters just append the
//! banner string to whatever context they already produce.
//!
//! Design notes
//! ────────────
//! * **Hot path discipline.** The helper runs inside `SessionStart`
//!   dispatch, so total wall time is budgeted to 50ms p99. Any DB read
//!   that crosses that budget bails out via `tokio::time::timeout` and
//!   the caller sees `None` — never a panic, never an error bubble.
//! * **Zero-noise on quiet sessions.** When no new rules exist for this
//!   repo since the last fire, the helper returns `None`. The adapter
//!   then appends nothing, preserving the exact existing transcript.
//! * **Per-repo watermark.** Each repo gets its own JSON watermark at
//!   `~/.difflore/projects/{hash}/last-session-start.json`. New repos
//!   start with `prev_ts = None`, so the first session shows everything
//!   learned to date (capped at 5). The watermark is then advanced to
//!   "now" so subsequent fires only show genuinely-new rules.
//! * **DB read failures are swallowed.** Hooks must never block the
//!   agent. If `data.db` is locked, missing, or rejects the query, the
//!   helper returns `None`.

pub mod query;
pub mod render;
pub mod watermark;

#[cfg(test)]
mod tests;

use std::time::Duration;

/// What the banner helper needs to do its job. The integration site in
/// `hook_runtime::dispatch` builds this from the `HookEvent::SessionStart`
/// payload it already has — no new plumbing through the rest of the
/// pipeline. `client_name` is recorded into the watermark so a future
/// follow-up can per-client-scope the "since last session" wording (e.g.
/// "since last Claude Code session"); the current banner just uses it
/// for the watermark file's debug trail.
#[derive(Debug, Clone)]
pub struct BannerContext {
    /// Absolute path to the agent's working directory. Used to resolve
    /// the project root → project hash → watermark file, and to detect
    /// the current GitHub repo aliases that filter the query.
    pub cwd: String,
    /// Platform adapter name (`"claude-code"`, `"cursor"`, …). Stored
    /// in the watermark JSON for debug only; the query treats every
    /// client identically.
    pub client_name: String,
}

/// Wall-clock budget for the entire banner pipeline. p99 ceiling — if
/// the DB read or watermark IO exceeds this, the helper returns `None`
/// so the adapter doesn't stall the agent on a slow disk.
const BANNER_BUDGET: Duration = Duration::from_millis(50);

/// Max rules listed inside the banner. Above this we'd push the banner
/// past the 6-line / 400-char shape the spec calls for, and the agent's
/// context window would notice. Most repos accumulate <5 new rules
/// between sessions anyway.
const MAX_RULES_IN_BANNER: usize = 5;

/// Build the "since-last-session" banner for the given context. Returns
/// `None` when there is nothing new to show OR when any step in the
/// pipeline failed — callers should treat both as "emit nothing".
///
/// The pipeline:
///   1. Resolve the project root + repo aliases from `ctx.cwd`.
///   2. Read the watermark (`prev_ts`) — `None` on first session.
///   3. Open `data.db` and query for rules with `installed_at > prev_ts`
///      whose `source_repo` matches one of this repo's aliases.
///   4. Advance the watermark to "now" (best-effort).
///   5. Format the rows into the banner string.
///
/// Each step is fenced by `tokio::time::timeout` against `BANNER_BUDGET`
/// so a stuck DB never stalls the agent's session start.
pub async fn render_since_last_session_banner(ctx: &BannerContext) -> Option<String> {
    // `Result::ok()` collapses an `Err(Elapsed)` from `timeout` into
    // `None`, which is exactly the "swallow on stall" semantics the
    // hot path needs. We don't care to distinguish a timeout from a
    // genuine no-rules-found case — both render as "emit nothing".
    let result: Result<Option<String>, tokio::time::error::Elapsed> =
        tokio::time::timeout(BANNER_BUDGET, render_inner(ctx)).await;
    result.unwrap_or_default()
}

async fn render_inner(ctx: &BannerContext) -> Option<String> {
    let project_root = resolve_project_root(&ctx.cwd);
    let project_hash = difflore_core::db::project_hash_from_root(&project_root);

    let repo_aliases = repo_aliases_for(&project_root);
    if repo_aliases.is_empty() {
        // No repo identity — we'd have no way to filter `source_repo`
        // and would spam every rule from every repo. Bail rather than
        // produce a misleading banner.
        return None;
    }

    let prev_ts = watermark::read_watermark(&project_hash).map(|w| w.ts_ms);

    let Ok(db) = difflore_core::db::init_db().await else {
        return None;
    };

    let rows = query::new_rules_since(&db, prev_ts, &repo_aliases, MAX_RULES_IN_BANNER)
        .await
        .ok()?;

    // Advance the watermark BEFORE we early-return on "no rows": even an
    // empty quiet session counts as a session, otherwise the next fire
    // would still see `prev_ts = None` and show every rule learned to
    // date. Watermark write failures are swallowed.
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

/// Resolve the project root for `cwd`. Tries `git rev-parse
/// --show-toplevel` (matching `current_project_root`'s logic) but
/// scoped to the agent's reported cwd rather than the CLI's own. Falls
/// back to `cwd` itself when git isn't available.
fn resolve_project_root(cwd: &str) -> std::path::PathBuf {
    if cwd.is_empty() {
        return difflore_core::db::current_project_root();
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

/// Normalized lower-case `owner/repo` aliases for the project. Matches
/// the convention used by `commands::status::queries::normalized_repo_aliases`
/// so the SQL filter joins cleanly with already-stored `source_repo`
/// values (which were also lowercased on write).
fn repo_aliases_for(project_root: &std::path::Path) -> Vec<String> {
    let raw = difflore_core::git::detect_github_repo_full_names(&project_root.to_string_lossy());
    raw.into_iter()
        .map(|r| r.trim().to_ascii_lowercase())
        .filter(|r| !r.is_empty())
        .collect()
}

/// Render a unix-ms timestamp as RFC 3339 for the banner header. Returns
/// `None` if the timestamp is outside chrono's representable range —
/// in that case the caller falls back to a generic phrase.
fn timestamp_to_rfc3339(ts_ms: i64) -> Option<String> {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ts_ms).map(|dt| dt.to_rfc3339())
}
