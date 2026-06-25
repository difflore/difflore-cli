//! Session-mine fire watermark.
//!
//! Two fire conditions, OR-ed:
//!
//! 1. **Session-end** — the hook dispatcher passes
//!    `force_session_end = true` on `SessionEnd` / `Stop` so the last
//!    few pairs of an expiring conversation get a chance to mine.
//! 2. **Turn watermark** — fire every [`TURNS_PER_FIRE`] turns inside
//!    a long-running session so we don't lose mid-session learnings
//!    if the user never cleanly closes the conversation.
//!
//! State is persisted at
//! `<DIFFLORE_HOME>/projects/<project-hash>/session-mine-state.json`
//! so the watermark survives process exits. Two concurrent hook processes
//! racing on the same file is benign: worst case is a double-fire, handled
//! cheaply on the worker side via the gate's MERGE verdict.

use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

/// How many turns may pass between mid-session fires before the turn-watermark
/// trigger kicks in. High enough that idle sessions don't churn the gate, low
/// enough that a long debug session doesn't go un-mined.
pub const TURNS_PER_FIRE: i64 = 20;

const DIFFLORE_MINE_TURNS_ENV: &str = "DIFFLORE_MINE_TURNS";
const DIFFLORE_MINE_COOLDOWN_MS_ENV: &str = "DIFFLORE_MINE_COOLDOWN_MS";
const DIFFLORE_MINE_CORRECTION_COOLDOWN_MS_ENV: &str = "DIFFLORE_MINE_CORRECTION_COOLDOWN_MS";
const GATE_STALL_BACKOFF_MS: i64 = 60 * 60 * 1000;

const GIT_ROOT_TIMEOUT: Duration = Duration::from_millis(100);
const GIT_ROOT_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// On-disk state file shape. Plain JSON so the user can `cat` or `rm` it during
/// debugging without breaking invariants (next fire just starts a new file).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SessionMineState {
    /// Unix-ms of the last successful fire. `0` for "never".
    #[serde(default)]
    pub last_fire_ts: i64,
    /// Number of turns since the last fire. Prompt hooks bump this via
    /// [`should_trigger_after_user_prompt`]; the trigger resets it to
    /// zero when it fires.
    #[serde(default)]
    pub turns_since_fire: i64,
    /// Last non-empty session id seen by the prompt watermark. Lets the
    /// hook detect "user switched to a new session" and reset the counter
    /// without firing prematurely.
    #[serde(default)]
    pub last_session_id: String,
    /// Unix-ms when local gate capture was paused by a persistent agent-CLI
    /// failure. `0` means capture is not paused.
    #[serde(default)]
    pub gate_stalled_since_ts: i64,
    /// Short human-readable persistent gate failure reason.
    #[serde(default)]
    pub gate_stall_reason: String,
    /// Unix-ms of the last targeted correction mine. Kept separate from
    /// `last_fire_ts` so bursty correction prompts do not suppress normal
    /// session/recipe mining cadence.
    #[serde(default)]
    pub last_correction_fire_ts: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateCaptureStatus {
    Ready,
    Paused {
        since_ts: i64,
        reason: String,
        retry_after_ms: i64,
    },
}

/// Decide whether the session-mine worker should fire right now.
///
/// Reads `state_path`, applies the bump rule, persists the new state, and
/// returns the decision in one shot so callers can't fire without updating
/// the watermark. `turn_count` is the caller's current `turns_since_fire`;
/// when it is `>= TURNS_PER_FIRE` the trigger fires and resets the counter.
///
/// Use [`should_trigger_now_with_force`] for session-end signals.
pub fn should_trigger_now(state_path: &Path, turn_count: i64) -> bool {
    should_trigger_now_with_force(state_path, turn_count, false)
}

/// Bump the persisted turn watermark for a `UserPromptSubmit` event and
/// decide whether periodic session mining should fire. A new non-empty
/// `session_id` resets the counter before counting this prompt, so one
/// long session cannot make the next session fire immediately.
pub fn should_trigger_after_user_prompt(state_path: &Path, session_id: Option<&str>) -> bool {
    let mut state = read_state(state_path).unwrap_or_default();
    let now = now_unix_ms();
    let session_id = session_id.map(str::trim).filter(|s| !s.is_empty());
    if let Some(session_id) = session_id
        && state.last_session_id != session_id
    {
        session_id.clone_into(&mut state.last_session_id);
        state.turns_since_fire = 0;
    }

    state.turns_since_fire = state.turns_since_fire.saturating_add(1);
    let turns_per_fire = configured_turns_per_fire();
    let paused = gate_stall_retry_after_ms(&state, now).is_some();
    let fire = !paused && state.turns_since_fire >= turns_per_fire;
    if fire {
        state.last_fire_ts = now;
        state.turns_since_fire = 0;
    }
    let _ = write_state(state_path, &state);
    fire
}

/// Generalised entry point. Per-turn callers pass `force_session_end = false`;
/// the hook dispatcher's SessionEnd / Stop branch passes `true` to skip the
/// watermark.
pub fn should_trigger_now_with_force(
    state_path: &Path,
    turn_count: i64,
    force_session_end: bool,
) -> bool {
    let mut state = read_state(state_path).unwrap_or_default();
    let now = now_unix_ms();
    state.turns_since_fire = turn_count.max(state.turns_since_fire);

    let turns_per_fire = configured_turns_per_fire();
    let paused = gate_stall_retry_after_ms(&state, now).is_some();
    let fire = !paused && (force_session_end || state.turns_since_fire >= turns_per_fire);
    if fire {
        state.last_fire_ts = now;
        state.turns_since_fire = 0;
    }
    let _ = write_state(state_path, &state);
    fire
}

/// Cooldown for the session-end (Stop / SessionEnd) mining gate.
///
/// Some hosts fire `Stop` at the end of *every* assistant turn (Claude Code),
/// so an unconditional force-mine would spawn a gate agent-CLI call per turn —
/// burning tokens and adding turn-end work. Collapse those into at most one
/// mine per window. Mirrors the autopilot's session-end cooldown.
pub const SESSION_END_MINE_COOLDOWN_MS: i64 = 300_000;
pub const CORRECTION_MINE_COOLDOWN_MS: i64 = 60_000;

/// Session-end mining gate, throttled by [`SESSION_END_MINE_COOLDOWN_MS`].
///
/// Unlike [`should_trigger_now_with_force`] (which fires unconditionally on the
/// force path), this fires only when the previous mine is older than the
/// cooldown — so per-turn `Stop` events on hosts that emit them don't spawn a
/// gate every turn, and a `Stop`+`SessionEnd` pair within the window collapses
/// to a single mine. On fire it resets the watermark; when throttled it leaves
/// state untouched so the mid-session prompt cadence keeps counting.
pub fn should_trigger_session_end(state_path: &Path) -> bool {
    let mut state = read_state(state_path).unwrap_or_default();
    let now = now_unix_ms();
    let paused = gate_stall_retry_after_ms(&state, now).is_some();
    let cooldown_ms = configured_session_end_cooldown_ms();
    let fire = !paused && (state.last_fire_ts == 0 || now - state.last_fire_ts >= cooldown_ms);
    if fire {
        state.last_fire_ts = now;
        state.turns_since_fire = 0;
        let _ = write_state(state_path, &state);
    }
    fire
}

/// Targeted correction mining gate, throttled separately from session mining.
///
/// A correction prompt is high-signal, but a rapid back-and-forth correction
/// burst should not spawn one gate child per user message before the persistent
/// failure backoff has a chance to engage.
pub fn should_trigger_correction(state_path: &Path) -> bool {
    let mut state = read_state(state_path).unwrap_or_default();
    let now = now_unix_ms();
    let paused = gate_stall_retry_after_ms(&state, now).is_some();
    let cooldown_ms = configured_correction_cooldown_ms();
    let fire = !paused
        && (state.last_correction_fire_ts == 0
            || now - state.last_correction_fire_ts >= cooldown_ms);
    if fire {
        state.last_correction_fire_ts = now;
        let _ = write_state(state_path, &state);
    }
    fire
}

pub fn record_gate_capture_stall_for_project(
    cwd: Option<&str>,
    reason: &str,
) -> Result<(), String> {
    let path = state_file_for_project(cwd)?;
    record_gate_capture_stall(&path, reason)
}

pub fn clear_gate_capture_stall_for_project(cwd: Option<&str>) -> Result<(), String> {
    let path = state_file_for_project(cwd)?;
    clear_gate_capture_stall(&path)
}

pub fn gate_capture_status_for_project(cwd: Option<&str>) -> Result<GateCaptureStatus, String> {
    let path = state_file_for_project(cwd)?;
    Ok(gate_capture_status(&path))
}

pub fn gate_capture_status(state_path: &Path) -> GateCaptureStatus {
    let state = read_state(state_path).unwrap_or_default();
    gate_capture_status_from_state(&state, now_unix_ms())
}

/// Canonical state-file path for the current project, sharing the per-project
/// namespace via `difflore_core::infra::db::project_index_dir`. Errors
/// propagate so callers can drop the trigger silently when the data home isn't
/// resolvable (e.g. a test sandbox with no DIFFLORE_HOME).
pub fn state_file_for_cwd() -> Result<PathBuf, String> {
    state_file_for_project(None)
}

/// Canonical state-file path for a hook-supplied cwd. When `cwd` is
/// present, resolve its git toplevel first so prompt and lifecycle hooks
/// share the same per-project watermark even when the host invokes hooks
/// from a subdirectory.
pub fn state_file_for_project(cwd: Option<&str>) -> Result<PathBuf, String> {
    let root = project_root_for_cwd(cwd);
    let hash = difflore_core::infra::db::project_hash_from_root(&root);
    let mut path = difflore_core::infra::db::project_index_dir(&hash);
    path.push("session-mine-state.json");
    Ok(path)
}

fn project_root_for_cwd(cwd: Option<&str>) -> PathBuf {
    let Some(cwd) = cwd.map(str::trim).filter(|s| !s.is_empty()) else {
        return difflore_core::infra::db::current_project_root();
    };
    let path = PathBuf::from(cwd);
    cached_project_root_for_path(path, discover_git_root)
}

fn cached_project_root_for_path<F>(path: PathBuf, discover: F) -> PathBuf
where
    F: FnOnce(&Path) -> Option<PathBuf>,
{
    if let Ok(cache) = project_root_cache().lock()
        && let Some(root) = cache.get(&path)
    {
        return root.clone();
    }

    let root = discover(&path).unwrap_or_else(|| path.clone());
    if let Ok(mut cache) = project_root_cache().lock() {
        cache.insert(path, root.clone());
    }
    root
}

fn project_root_cache() -> &'static Mutex<HashMap<PathBuf, PathBuf>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, PathBuf>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn discover_git_root(path: &Path) -> Option<PathBuf> {
    let out = run_command_with_timeout(
        path,
        "git",
        &["rev-parse", "--show-toplevel"],
        GIT_ROOT_TIMEOUT,
    )
    .ok()?;
    if out.status.success() {
        let root = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        if !root.is_empty() {
            return Some(PathBuf::from(root));
        }
    }
    None
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
    // not flash a transient console window on Windows (same flag git.rs applies).
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

fn configured_turns_per_fire() -> i64 {
    positive_env_i64(DIFFLORE_MINE_TURNS_ENV, TURNS_PER_FIRE)
}

fn configured_session_end_cooldown_ms() -> i64 {
    positive_env_i64(DIFFLORE_MINE_COOLDOWN_MS_ENV, SESSION_END_MINE_COOLDOWN_MS)
}

fn configured_correction_cooldown_ms() -> i64 {
    positive_env_i64(
        DIFFLORE_MINE_CORRECTION_COOLDOWN_MS_ENV,
        CORRECTION_MINE_COOLDOWN_MS,
    )
}

fn positive_env_i64(key: &str, fallback: i64) -> i64 {
    positive_i64(std::env::var(key).ok().as_deref(), fallback)
}

fn positive_i64(raw: Option<&str>, fallback: i64) -> i64 {
    raw.and_then(|value| value.trim().parse::<i64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(fallback)
}

fn record_gate_capture_stall(path: &Path, reason: &str) -> Result<(), String> {
    let mut state = read_state(path).unwrap_or_default();
    state.gate_stalled_since_ts = now_unix_ms();
    state.gate_stall_reason = truncate_reason(reason);
    write_state(path, &state).map_err(|e| e.to_string())
}

fn clear_gate_capture_stall(path: &Path) -> Result<(), String> {
    let mut state = read_state(path).unwrap_or_default();
    if state.gate_stalled_since_ts == 0 && state.gate_stall_reason.is_empty() {
        return Ok(());
    }
    state.gate_stalled_since_ts = 0;
    state.gate_stall_reason.clear();
    write_state(path, &state).map_err(|e| e.to_string())
}

fn gate_capture_status_from_state(state: &SessionMineState, now: i64) -> GateCaptureStatus {
    let Some(retry_after_ms) = gate_stall_retry_after_ms(state, now) else {
        return GateCaptureStatus::Ready;
    };
    GateCaptureStatus::Paused {
        since_ts: state.gate_stalled_since_ts,
        reason: state.gate_stall_reason.trim().to_owned(),
        retry_after_ms,
    }
}

const fn gate_stall_retry_after_ms(state: &SessionMineState, now: i64) -> Option<i64> {
    if state.gate_stalled_since_ts <= 0 {
        return None;
    }
    let elapsed = now.saturating_sub(state.gate_stalled_since_ts);
    if elapsed >= GATE_STALL_BACKOFF_MS {
        None
    } else {
        Some(GATE_STALL_BACKOFF_MS - elapsed)
    }
}

fn truncate_reason(reason: &str) -> String {
    const MAX: usize = 240;
    let first_line = reason.lines().next().unwrap_or("").trim();
    let mut chars = first_line.chars();
    let truncated: String = chars.by_ref().take(MAX).collect();
    if chars.next().is_none() {
        first_line.to_owned()
    } else {
        format!("{truncated}...")
    }
}

fn read_state(path: &Path) -> Option<SessionMineState> {
    let body = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&body).ok()
}

fn write_state(path: &Path, state: &SessionMineState) -> Result<(), io::Error> {
    let body = serde_json::to_string_pretty(state).map_err(|e| io::Error::other(e.to_string()))?;
    difflore_core::infra::files::write_atomic(path, body.as_bytes())
}

fn now_unix_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn tmp_state_path() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("session-mine-state.json");
        (dir, path)
    }

    #[test]
    fn project_root_cache_reuses_discovered_root_for_cwd() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = dir.path().join("work");
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&cwd).expect("create cwd");
        std::fs::create_dir_all(&root).expect("create root");
        let calls = Cell::new(0);

        let first = cached_project_root_for_path(cwd.clone(), |_| {
            calls.set(calls.get() + 1);
            Some(root.clone())
        });
        let second = cached_project_root_for_path(cwd, |_| {
            calls.set(calls.get() + 1);
            Some(dir.path().join("unexpected"))
        });

        assert_eq!(first, root);
        assert_eq!(second, root);
        assert_eq!(calls.get(), 1, "second lookup must use the cache");
    }

    #[test]
    fn project_root_for_cwd_falls_back_to_cwd_outside_git() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = dir.path().to_string_lossy().into_owned();

        assert_eq!(project_root_for_cwd(Some(&cwd)), dir.path());
    }

    #[test]
    fn project_root_for_cwd_falls_back_when_git_cannot_start_in_cwd() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("missing");
        let cwd = missing.to_string_lossy().into_owned();

        assert_eq!(project_root_for_cwd(Some(&cwd)), missing);
    }

    #[cfg(unix)]
    #[test]
    fn run_command_with_timeout_rejects_slow_command() {
        let dir = tempfile::tempdir().expect("tempdir");
        let started = Instant::now();

        let err = run_command_with_timeout(
            dir.path(),
            "sh",
            &["-c", "sleep 2"],
            Duration::from_millis(20),
        )
        .expect_err("slow command must time out");

        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "timeout should kill the command promptly"
        );
    }

    #[test]
    fn first_call_under_threshold_does_not_fire() {
        // A fresh session under the threshold must not pay LLM gate cost: read
        // the missing file as "never fired", bump the counter, and hold off.
        let (_dir, path) = tmp_state_path();
        assert!(!should_trigger_now(&path, 5));
        let state = read_state(&path).expect("state must persist");
        assert_eq!(state.turns_since_fire, 5);
        assert_eq!(state.last_fire_ts, 0);
    }

    #[test]
    fn reaching_turn_threshold_fires_and_resets() {
        // Crossing TURNS_PER_FIRE must fire and reset the counter to zero, or
        // every subsequent hook would re-fire and pay gate cost.
        let (_dir, path) = tmp_state_path();
        assert!(should_trigger_now(&path, TURNS_PER_FIRE));
        let state = read_state(&path).expect("state must persist");
        assert_eq!(state.turns_since_fire, 0);
        assert!(state.last_fire_ts > 0);
    }

    #[test]
    fn user_prompt_submit_bumps_turn_watermark() {
        let (_dir, path) = tmp_state_path();

        assert!(!should_trigger_after_user_prompt(&path, Some("sess-a")));

        let state = read_state(&path).expect("state must persist");
        assert_eq!(state.turns_since_fire, 1);
        assert_eq!(state.last_session_id, "sess-a");
        assert_eq!(state.last_fire_ts, 0);
    }

    #[test]
    fn twentieth_user_prompt_submit_fires_and_resets() {
        let (_dir, path) = tmp_state_path();

        for _ in 0..(TURNS_PER_FIRE - 1) {
            assert!(!should_trigger_after_user_prompt(&path, Some("sess-a")));
        }
        assert!(should_trigger_after_user_prompt(&path, Some("sess-a")));

        let state = read_state(&path).expect("state must persist");
        assert_eq!(state.turns_since_fire, 0);
        assert_eq!(state.last_session_id, "sess-a");
        assert!(state.last_fire_ts > 0);
    }

    #[test]
    fn cadence_env_parser_accepts_positive_values_only() {
        assert_eq!(positive_i64(Some("7"), TURNS_PER_FIRE), 7);
        assert_eq!(positive_i64(Some(" 9000 "), TURNS_PER_FIRE), 9000);
        assert_eq!(positive_i64(Some("0"), TURNS_PER_FIRE), TURNS_PER_FIRE);
        assert_eq!(positive_i64(Some("-1"), TURNS_PER_FIRE), TURNS_PER_FIRE);
        assert_eq!(positive_i64(Some("nope"), TURNS_PER_FIRE), TURNS_PER_FIRE);
        assert_eq!(positive_i64(None, TURNS_PER_FIRE), TURNS_PER_FIRE);
    }

    #[test]
    fn gate_capture_stall_blocks_triggers_until_backoff_expires() {
        let (_dir, path) = tmp_state_path();

        record_gate_capture_stall(&path, "codex unauthorized").expect("record stall");

        assert!(
            !should_trigger_after_user_prompt(&path, Some("sess-a")),
            "paused capture must not fire on prompt watermark"
        );
        assert!(
            !should_trigger_session_end(&path),
            "paused capture must not fire on session end"
        );
        assert!(
            !should_trigger_correction(&path),
            "paused capture must not fire targeted correction mining"
        );

        let mut state = read_state(&path).expect("state persists");
        state.gate_stalled_since_ts -= GATE_STALL_BACKOFF_MS + 1;
        state.turns_since_fire = TURNS_PER_FIRE;
        write_state(&path, &state).expect("write backdated state");

        assert!(
            should_trigger_now(&path, TURNS_PER_FIRE),
            "after the pause backoff, normal trigger cadence should resume"
        );
    }

    #[test]
    fn gate_capture_status_reports_pause_and_clear() {
        let (_dir, path) = tmp_state_path();

        assert_eq!(gate_capture_status(&path), GateCaptureStatus::Ready);
        record_gate_capture_stall(&path, "claude-code forbidden").expect("record stall");

        let status = gate_capture_status(&path);
        let GateCaptureStatus::Paused {
            since_ts,
            reason,
            retry_after_ms,
        } = status
        else {
            panic!("expected paused capture status");
        };
        assert!(since_ts > 0);
        assert_eq!(reason, "claude-code forbidden");
        assert!(retry_after_ms > 0);

        clear_gate_capture_stall(&path).expect("clear stall");
        assert_eq!(gate_capture_status(&path), GateCaptureStatus::Ready);
    }

    #[test]
    fn user_prompt_submit_resets_counter_for_new_session() {
        let (_dir, path) = tmp_state_path();

        let _ = should_trigger_now(&path, TURNS_PER_FIRE - 1);
        assert!(!should_trigger_after_user_prompt(&path, Some("sess-b")));

        let state = read_state(&path).expect("state must persist");
        assert_eq!(state.turns_since_fire, 1);
        assert_eq!(state.last_session_id, "sess-b");
    }

    #[test]
    fn force_session_end_fires_regardless_of_turn_count() {
        // force_session_end=true (set on SessionEnd / Stop) must mine the last
        // turns of an expiring conversation even below the watermark.
        let (_dir, path) = tmp_state_path();
        let fired = should_trigger_now_with_force(&path, 1, true);
        assert!(fired, "session-end must short-circuit the turn watermark");
        let state = read_state(&path).expect("state must persist");
        assert_eq!(state.turns_since_fire, 0);
    }

    #[test]
    fn session_end_mine_is_throttled_by_cooldown() {
        // Hosts that fire Stop every turn (Claude Code) must not spawn a gate
        // per turn. First mine fires; an immediate repeat (next turn, or the
        // Stop+SessionEnd pair) is throttled; once the cooldown elapses it fires
        // again.
        let (_dir, path) = tmp_state_path();
        assert!(
            should_trigger_session_end(&path),
            "first session-end must mine"
        );
        assert!(
            !should_trigger_session_end(&path),
            "a repeat within the cooldown must not re-mine"
        );
        // Backdate the last fire past the cooldown window.
        let mut state = read_state(&path).expect("state persists");
        state.last_fire_ts -= SESSION_END_MINE_COOLDOWN_MS + 1;
        write_state(&path, &state).expect("write backdated state");
        assert!(
            should_trigger_session_end(&path),
            "after the cooldown elapses, session-end must mine again"
        );
    }

    #[test]
    fn correction_mine_is_throttled_by_cooldown() {
        let (_dir, path) = tmp_state_path();
        assert!(
            should_trigger_correction(&path),
            "first correction should mine"
        );
        assert!(
            !should_trigger_correction(&path),
            "a repeated correction inside the cooldown must not mine"
        );

        let mut state = read_state(&path).expect("state persists");
        state.last_correction_fire_ts -= CORRECTION_MINE_COOLDOWN_MS + 1;
        write_state(&path, &state).expect("write backdated state");
        assert!(
            should_trigger_correction(&path),
            "after the cooldown elapses, correction mining should resume"
        );
    }

    #[test]
    fn malformed_state_file_is_treated_as_fresh() {
        // A garbage state file must fall back to default state, not panic;
        // otherwise one bad file would brick session-mine for that project.
        let (_dir, path) = tmp_state_path();
        std::fs::write(&path, "{ not json").unwrap();
        assert!(!should_trigger_now(&path, 1));
    }

    #[test]
    fn turn_count_below_persisted_value_does_not_decrement() {
        // The watermark tracks the highest reported turn count. If the hook
        // restarts and forgets its counter, a fresh `turn_count = 1` must not
        // overwrite a persisted `5` and lose accumulated turns.
        let (_dir, path) = tmp_state_path();
        let _ = should_trigger_now(&path, 5);
        let _ = should_trigger_now(&path, 1);
        let state = read_state(&path).expect("state must persist");
        assert_eq!(
            state.turns_since_fire, 5,
            "watermark must monotonically rise inside a session"
        );
    }
}
