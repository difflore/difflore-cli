#![allow(clippy::unwrap_used, clippy::expect_used)]
//! End-to-end shim behaviour: the `difflore-hook` binary must always produce a
//! valid hook output and exit 0 in `auto`/`never` mode even with no warm
//! daemon, and must surface a hard error in `always` mode when the daemon is
//! unreachable.
//!
//! Own test binary: drives the real shim process with a tempdir `DIFFLORE_HOME`
//! so nothing touches the developer's `~/.difflore` and no daemon from a prior
//! run is reused.

use std::io::Write as _;
use std::process::{Command, Stdio};

fn shim_bin() -> std::path::PathBuf {
    std::env::var_os("CARGO_BIN_EXE_difflore-hook")
        .map(std::path::PathBuf::from)
        .expect("difflore-hook binary path")
}

fn session_start_payload() -> String {
    serde_json::json!({
        "session_id": "test",
        "cwd": ".",
        "hook_event_name": "SessionStart",
        "source": "startup"
    })
    .to_string()
}

/// Run the shim with the given forward mode and stdin payload, returning
/// (exit_code, stdout). Uses a fresh tempdir home each call.
fn run_shim(mode: &str, payload: &str, home: &std::path::Path) -> (Option<i32>, String) {
    let mut child = Command::new(shim_bin())
        .args(["--client", "claude-code"])
        .env("DIFFLORE_HOME", home)
        .env("DIFFLORE_HOOK_FORWARD", mode)
        // Auto mode best-effort spawns a detached daemon; keep its idle window
        // tiny so the test never leaves a long-lived background process. The
        // daemon inherits this env from the shim child.
        .env("DIFFLORE_HOOK_DAEMON_IDLE_SECS", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn difflore-hook");
    child
        .stdin
        .take()
        .expect("child stdin")
        .write_all(payload.as_bytes())
        .expect("write stdin");
    let output = child.wait_with_output().expect("wait shim");
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
    )
}

#[test]
fn auto_mode_falls_back_in_process_when_no_daemon_is_running() {
    let home = tempfile::tempdir().expect("temp home");
    // Auto: warm path misses (no daemon), shim best-effort spawns one and falls
    // back in-process for THIS event. Output must be valid hook JSON, exit 0.
    let (code, stdout) = run_shim("auto", &session_start_payload(), home.path());
    assert_eq!(code, Some(0), "auto-mode hook must exit 0, stdout={stdout}");
    let json: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("auto-mode hook must emit valid JSON");
    assert!(
        json.get("continue").is_some() || json.is_object(),
        "expected a hook output object, got: {json}"
    );
}

#[test]
fn never_mode_runs_in_process_without_touching_the_socket() {
    let home = tempfile::tempdir().expect("temp home");
    let (code, stdout) = run_shim("never", &session_start_payload(), home.path());
    assert_eq!(code, Some(0), "never-mode hook must exit 0, stdout={stdout}");
    let _: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("never-mode hook must emit valid JSON");
}

#[test]
fn always_mode_fails_visibly_when_daemon_is_unreachable() {
    let home = tempfile::tempdir().expect("temp home");
    // Always: the user explicitly demanded the daemon. With none running and no
    // way to reach one, the shim must surface a visible failure (exit 2) rather
    // than silently degrading — preserving the diagnostic contract.
    let (code, _stdout) = run_shim("always", &session_start_payload(), home.path());
    assert_eq!(
        code,
        Some(2),
        "always-mode hook must exit 2 when the daemon is unreachable"
    );
}
