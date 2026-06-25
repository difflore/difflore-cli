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

use std::{
    io,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

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
    /// True when the hook shim reached this runtime via cold fallback instead
    /// of a warm forwarder. Used only for a short Windows SessionStart hint.
    pub forward_miss: bool,
}

/// Wall-clock p99 budget for the entire banner pipeline; exceeding it yields
/// `None` so a slow disk doesn't stall the agent.
const BANNER_BUDGET: Duration = Duration::from_millis(50);
const GIT_ROOT_TIMEOUT: Duration = Duration::from_millis(20);
const GIT_REMOTE_TIMEOUT: Duration = Duration::from_millis(15);
const GIT_ROOT_POLL_INTERVAL: Duration = Duration::from_millis(5);

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
    let project_scope = project_scope_for_banner(&ctx.cwd).await?;
    let project_root = project_scope.root;
    let project_hash = difflore_core::infra::db::project_hash_from_root(&project_root);
    let capture_paused_reason = capture_paused_reason_for_project_hash(&project_hash);

    let repo_aliases = project_scope.repo_aliases;
    if repo_aliases.is_empty() {
        // No repo identity → no way to filter `source_repo`, so we'd spam every
        // rule from every repo. Bail rather than mislead.
        return capture_paused_reason
            .as_deref()
            .map(render::format_capture_paused_banner);
    }

    let prev_ts = watermark::read_watermark(&project_hash).map(|w| w.ts_ms);

    let Ok(db) = difflore_core::infra::db::init_db().await else {
        return None;
    };

    let rows = query::new_rules_since(&db, prev_ts, &repo_aliases, MAX_RULES_IN_BANNER)
        .await
        .ok()?;
    let pulse = query::memory_pulse_since(&db, prev_ts, &repo_aliases)
        .await
        .unwrap_or_default();

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

    if !pulse.should_render(rows.len()) {
        if ctx.forward_miss && cfg!(windows) {
            return Some(render::format_windows_forwarder_cold_banner());
        }
        return capture_paused_reason
            .as_deref()
            .map(render::format_capture_paused_banner);
    }

    let prev_label = prev_ts
        .and_then(timestamp_to_rfc3339)
        .unwrap_or_else(|| "the start of this repo".to_owned());

    Some(render::format_banner_with_memory_pulse(
        &rows,
        &pulse,
        &prev_label,
        capture_paused_reason.as_deref(),
        ctx.forward_miss && cfg!(windows),
    ))
}

#[derive(Debug)]
struct BannerProjectScope {
    root: PathBuf,
    repo_aliases: Vec<String>,
}

async fn project_scope_for_banner(cwd: &str) -> Option<BannerProjectScope> {
    let configured_gitlab_hosts = difflore_core::ingest::gitlab::auth::configured_hosts().await;
    let cwd = cwd.to_owned();
    tokio::task::spawn_blocking(move || {
        let root = resolve_project_root(&cwd);
        let repo_aliases = repo_aliases_for(&root, &configured_gitlab_hosts);
        BannerProjectScope { root, repo_aliases }
    })
    .await
    .ok()
}

fn capture_paused_reason_for_project_hash(project_hash: &str) -> Option<String> {
    let mut state_path = difflore_core::infra::db::project_index_dir(project_hash);
    state_path.push("session-mine-state.json");
    let status = crate::session_mine::trigger::gate_capture_status(&state_path);
    match status {
        crate::session_mine::trigger::GateCaptureStatus::Ready => None,
        crate::session_mine::trigger::GateCaptureStatus::Paused { reason, .. } => {
            Some(reason).filter(|reason| !reason.trim().is_empty())
        }
    }
}

/// Resolve the project root for `cwd` via `git rev-parse --show-toplevel`,
/// scoped to the agent's reported cwd. Falls back to `cwd` when git isn't
/// available.
fn resolve_project_root(cwd: &str) -> PathBuf {
    let cwd_path = if cwd.is_empty() {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    } else {
        PathBuf::from(cwd)
    };
    let output = run_command_with_timeout(
        &cwd_path,
        "git",
        &["rev-parse", "--show-toplevel"],
        GIT_ROOT_TIMEOUT,
    );
    if let Ok(out) = output
        && out.status.success()
    {
        let s = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        if !s.is_empty() {
            return PathBuf::from(s);
        }
    }
    cwd_path
}

fn run_command_with_timeout(
    cwd: &Path,
    program: &str,
    args: &[&str],
    timeout: Duration,
) -> io::Result<Output> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Route through the core no-window builder so this kill/timeout helper does
    // not flash a transient console window on Windows.
    difflore_core::infra::git::apply_no_window(&mut cmd);
    let mut child = cmd.spawn()?;
    let started = Instant::now();
    loop {
        if child.try_wait()?.is_some() {
            return child.wait_with_output();
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "`{} {}` timed out after {}ms",
                    program,
                    args.join(" "),
                    timeout.as_millis()
                ),
            ));
        }
        std::thread::sleep(GIT_ROOT_POLL_INTERVAL);
    }
}

/// Normalized lower-case `owner/repo` aliases for the project, matching
/// `commands::status::queries::normalized_repo_aliases` so the SQL filter joins
/// cleanly with `source_repo` values (also lowercased on write).
fn repo_aliases_for(project_root: &Path, configured_gitlab_hosts: &[String]) -> Vec<String> {
    let Ok(out) =
        run_command_with_timeout(project_root, "git", &["remote", "-v"], GIT_REMOTE_TIMEOUT)
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    repo_aliases_from_remote_verbose(&stdout, configured_gitlab_hosts)
}

fn repo_aliases_from_remote_verbose(
    stdout: &str,
    configured_gitlab_hosts: &[String],
) -> Vec<String> {
    let mut repos = Vec::new();
    for remote in ["origin", "upstream"] {
        for line in stdout.lines() {
            let mut fields = line.split_whitespace();
            let Some(name) = fields.next() else {
                continue;
            };
            let Some(url) = fields.next() else {
                continue;
            };
            if name != remote {
                continue;
            }
            let Some(repo) = difflore_core::infra::git::parse_repo_remote_url_with_gitlab_hosts(
                url,
                configured_gitlab_hosts,
            ) else {
                continue;
            };
            if !repos.iter().any(|existing| existing == &repo) {
                repos.push(repo);
            }
            break;
        }
    }
    repos
}

/// Render a unix-ms timestamp as RFC 3339 for the banner header. `None` if the
/// timestamp is outside chrono's representable range, where the caller falls
/// back to a generic phrase.
fn timestamp_to_rfc3339(ts_ms: i64) -> Option<String> {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ts_ms).map(|dt| dt.to_rfc3339())
}
