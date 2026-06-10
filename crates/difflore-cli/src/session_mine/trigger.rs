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

use std::path::{Path, PathBuf};

/// How many turns may pass between mid-session fires before the turn-watermark
/// trigger kicks in. High enough that idle sessions don't churn the gate, low
/// enough that a long debug session doesn't go un-mined.
pub const TURNS_PER_FIRE: i64 = 20;

/// On-disk state file shape. Plain JSON so the user can `cat` or `rm` it during
/// debugging without breaking invariants (next fire just starts a new file).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SessionMineState {
    /// Unix-ms of the last successful fire. `0` for "never".
    #[serde(default)]
    pub last_fire_ts: i64,
    /// Number of turns since the last fire. Caller bumps this from
    /// the hook on `PostToolUse` / `UserPromptSubmit`; the trigger
    /// resets it to zero when it fires.
    #[serde(default)]
    pub turns_since_fire: i64,
    /// Last session id that fired. Lets the hook detect "user
    /// switched to a new session" and reset the counter without
    /// firing prematurely.
    #[serde(default)]
    pub last_session_id: String,
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

/// Generalised entry point. Per-turn callers pass `force_session_end = false`;
/// the hook dispatcher's SessionEnd / Stop branch passes `true` to skip the
/// watermark.
pub fn should_trigger_now_with_force(
    state_path: &Path,
    turn_count: i64,
    force_session_end: bool,
) -> bool {
    let mut state = read_state(state_path).unwrap_or_default();
    state.turns_since_fire = turn_count.max(state.turns_since_fire);

    let fire = force_session_end || state.turns_since_fire >= TURNS_PER_FIRE;
    if fire {
        state.last_fire_ts = now_unix_ms();
        state.turns_since_fire = 0;
    }
    let _ = write_state(state_path, &state);
    fire
}

/// Canonical state-file path for the current project, sharing the per-project
/// namespace via `difflore_core::infra::db::project_index_dir`. Errors
/// propagate so callers can drop the trigger silently when the data home isn't
/// resolvable (e.g. a test sandbox with no DIFFLORE_HOME).
pub fn state_file_for_cwd() -> Result<PathBuf, String> {
    let root = difflore_core::infra::db::current_project_root();
    let hash = difflore_core::infra::db::project_hash_from_root(&root);
    let mut path = difflore_core::infra::db::project_index_dir(&hash);
    path.push("session-mine-state.json");
    Ok(path)
}

fn read_state(path: &Path) -> Option<SessionMineState> {
    let body = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&body).ok()
}

fn write_state(path: &Path, state: &SessionMineState) -> Result<(), std::io::Error> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body =
        serde_json::to_string_pretty(state).map_err(|e| std::io::Error::other(e.to_string()))?;
    std::fs::write(path, body)
}

fn now_unix_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_state_path() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("session-mine-state.json");
        (dir, path)
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
