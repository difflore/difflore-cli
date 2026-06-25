#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(unsafe_code)]
//! Regression coverage for internal agent sessions.
//!
//! Local agent CLI subprocesses set `DIFFLORE_CAPTURE=false` so their own
//! Codex/Claude hooks do not recursively mine DiffLore's curator prompts. This
//! test lives in its own integration-test binary because it mutates process
//! environment variables.

use std::path::Path;

use difflore_cli::hook::runtime as hook_runtime;

fn contains_session_mine_state(path: &Path) -> bool {
    if !path.exists() {
        return false;
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return false;
    };
    for entry in entries.flatten() {
        let entry_path = entry.path();
        if entry_path.file_name().and_then(|name| name.to_str()) == Some("session-mine-state.json")
        {
            return true;
        }
        if entry_path.is_dir() && contains_session_mine_state(&entry_path) {
            return true;
        }
    }
    false
}

#[tokio::test]
async fn lifecycle_hook_skips_capture_side_effects_when_capture_is_disabled() {
    let home = tempfile::tempdir().expect("temp DIFFLORE_HOME");
    let cwd = tempfile::tempdir().expect("temp cwd");

    // SAFETY: this integration-test binary contains one test. Cargo runs each
    // tests/*.rs file in its own process, so these env mutations cannot race a
    // sibling test in the same binary.
    unsafe {
        std::env::set_var("DIFFLORE_HOME", home.path());
        std::env::set_var(difflore_core::cloud::capture::DIFFLORE_CAPTURE_ENV, "false");
    }

    let raw = serde_json::json!({
        "hook_event_name": "Stop",
        "session_id": "internal-curator-session",
        "cwd": cwd.path(),
    })
    .to_string();

    let output = hook_runtime::output_for_raw("codex", &raw, false)
        .await
        .expect("hook output");
    let json: serde_json::Value = serde_json::from_str(&output).expect("parse output");
    assert_eq!(
        json.get("continue").and_then(serde_json::Value::as_bool),
        Some(true),
        "capture-disabled lifecycle hook must remain a quiet noop: {json}"
    );

    assert!(
        !home.path().join("data.db").exists(),
        "capture-disabled Stop must not open the app DB to schedule autopilot"
    );
    assert!(
        !home.path().join("observations_outbox.db").exists(),
        "capture-disabled Stop must not open the observations outbox"
    );
    assert!(
        !contains_session_mine_state(&home.path().join("projects")),
        "capture-disabled Stop must not write the session-mine watermark"
    );
}
