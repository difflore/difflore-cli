#![cfg_attr(windows, windows_subsystem = "windows")]

//! `difflore-hook`: the thin shim every client's hook config invokes.
//!
//! Fast path: forward the raw event to the warm hook forwarder over the local
//! socket. Fallback: run the hook runtime in-process. The wire protocol —
//! request/response shapes, endpoint, blocking transport — is defined once in
//! `difflore_cli::hook::forward::protocol` and only consumed here.

use std::io::Read;
use std::process::ExitCode;

use difflore_cli::hook::forward::protocol;

fn main() -> ExitCode {
    let mode = protocol::Mode::from_env();
    let client = parse_client_arg().unwrap_or_else(|| {
        difflore_core::infra::env::var(difflore_core::infra::env::DIFFLORE_HOOK_CLIENT)
            .unwrap_or_else(|| "claude-code".to_owned())
    });

    // Cap stdin so a hostile or runaway hook producer cannot OOM the hook.
    // Reading the ceiling + 1 lets us no-op on oversized payloads instead of
    // processing truncated events.
    let mut raw = String::new();
    let read = std::io::stdin()
        .take(protocol::MAX_IPC_BYTES + 1)
        .read_to_string(&mut raw);
    if read.is_err() || raw.len() as u64 > protocol::MAX_IPC_BYTES {
        if difflore_core::infra::env::flag_set(difflore_core::infra::env::DIFFLORE_DEBUG_HOOKS) {
            eprintln!(
                "[difflore-hook] stdin ignored: read failed or exceeded {} bytes",
                protocol::MAX_IPC_BYTES
            );
        }
        println!("{}", protocol::NOOP_OUTPUT);
        return ExitCode::SUCCESS;
    }

    if mode != protocol::Mode::Always
        && fast_noop_enabled()
        && let Some(output) = fast_noop_output(&client, &raw)
    {
        if difflore_core::infra::env::flag_set(difflore_core::infra::env::DIFFLORE_HOOK_SHIM_TRACE)
        {
            eprintln!("[difflore-hook.trace] fast_noop=1");
        }
        println!("{output}");
        return ExitCode::SUCCESS;
    }

    if mode != protocol::Mode::Never {
        match forward_once(&client, &raw) {
            Ok(output) => {
                println!("{output}");
                return ExitCode::SUCCESS;
            }
            Err(e) if mode == protocol::Mode::Always => {
                eprintln!("DiffLore hook could not start its background helper: {e}");
                return ExitCode::from(2);
            }
            Err(_) => {
                // Auto mode, warm path missed: best-effort spawn a detached
                // daemon so the *next* hook hits the warm path, then fall back
                // in-process for *this* event (we never block waiting for the
                // daemon to bind). Spawn failure is swallowed — it must never
                // turn a working fallback into a hook error.
                maybe_spawn_daemon();
            }
        }
    }

    // Only the cold fallback path needs an async runtime. The fast-noop and
    // warm-forward paths above are fully synchronous and return early, so the
    // common case never pays for runtime construction.
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            if difflore_core::infra::env::flag_set(difflore_core::infra::env::DIFFLORE_DEBUG_HOOKS)
            {
                eprintln!("[difflore-hook] could not build fallback runtime: {e}");
            }
            println!("{}", protocol::NOOP_OUTPUT);
            return ExitCode::SUCCESS;
        }
    };
    runtime.block_on(fallback_to_runtime(
        &client,
        &raw,
        mode != protocol::Mode::Never,
    ));
    ExitCode::SUCCESS
}

/// Best-effort detached daemon spawn for the current project. Only logged
/// under `DIFFLORE_DEBUG_HOOKS`; the caller proceeds to fallback regardless.
fn maybe_spawn_daemon() {
    #[cfg(windows)]
    {
        if !windows_hook_self_warm_enabled() {
            return;
        }
    }

    let hash = protocol::current_project_hash();
    if let Err(e) = difflore_cli::hook::forward::spawn::spawn_daemon_detached(&hash) {
        if difflore_core::infra::env::flag_set(difflore_core::infra::env::DIFFLORE_DEBUG_HOOKS) {
            eprintln!("[difflore-hook] daemon spawn skipped: {e}");
        }
    }
}

#[cfg(windows)]
fn windows_hook_self_warm_enabled() -> bool {
    match std::env::var("DIFFLORE_WINDOWS_HOOK_SELF_WARM") {
        Ok(value) => !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "never" | "no"
        ),
        Err(_) => true,
    }
}

fn fast_noop_enabled() -> bool {
    match std::env::var("DIFFLORE_HOOK_SHIM_FAST_NOOP") {
        Ok(value) => !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "never" | "no"
        ),
        Err(_) => true,
    }
}

fn parse_client_arg() -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--client" {
            return args.next();
        }
        if let Some(value) = arg.strip_prefix("--client=") {
            return Some(value.to_owned());
        }
    }
    None
}

fn fast_noop_output(client: &str, raw: &str) -> Option<&'static str> {
    let payload: serde_json::Value = serde_json::from_str(raw).ok()?;
    match fast_post_tool_kind(client, &payload)? {
        FastPostToolKind::Mutating => None,
        FastPostToolKind::NonMutating => Some(protocol::NOOP_OUTPUT),
        FastPostToolKind::Bash => {
            if bash_payload_needs_runtime(&payload) {
                None
            } else {
                Some(protocol::NOOP_OUTPUT)
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FastPostToolKind {
    Mutating,
    Bash,
    NonMutating,
}

fn fast_post_tool_kind(client: &str, payload: &serde_json::Value) -> Option<FastPostToolKind> {
    if client.trim().eq_ignore_ascii_case("windsurf") {
        return fast_windsurf_post_tool_kind(payload);
    }

    if payload
        .get("hook_event_name")
        .and_then(|v| v.as_str())
        .is_some_and(|event| event != "PostToolUse")
    {
        return None;
    }

    let tool_name = payload
        .get("tool_name")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    Some(tool_kind_from_name(tool_name))
}

fn fast_windsurf_post_tool_kind(payload: &serde_json::Value) -> Option<FastPostToolKind> {
    match payload.get("agent_action_name").and_then(|v| v.as_str())? {
        "post_write_code" => Some(FastPostToolKind::Mutating),
        "post_run_command" => Some(FastPostToolKind::Bash),
        "post_mcp_tool_use" => Some(FastPostToolKind::NonMutating),
        _ => None,
    }
}

fn tool_kind_from_name(tool_name: &str) -> FastPostToolKind {
    match tool_name {
        "Edit" | "Write" | "MultiEdit" | "apply_patch" => FastPostToolKind::Mutating,
        "Bash" => FastPostToolKind::Bash,
        _ => FastPostToolKind::NonMutating,
    }
}

fn bash_payload_needs_runtime(payload: &serde_json::Value) -> bool {
    let Some(response) = bash_response_payload(payload) else {
        return false;
    };
    if command_failed(response) {
        return true;
    }
    let output = shell_output_text(response);
    if output.trim().len() < difflore_core::hook_signal::BASH_MIN_ERROR_OUTPUT_CHARS {
        return false;
    }
    difflore_core::hook_signal::bash_output_is_high_signal_failure(&output)
}

fn bash_response_payload(payload: &serde_json::Value) -> Option<&serde_json::Value> {
    payload
        .get("tool_response")
        .or_else(|| payload.get("tool_result"))
        .or_else(|| payload.get("result"))
        .or_else(|| payload.get("tool_info"))
}

fn command_failed(value: &serde_json::Value) -> bool {
    for key in ["exit_code", "exitCode", "status_code", "statusCode"] {
        if let Some(code) = value.get(key).and_then(serde_json::Value::as_i64) {
            return code != 0;
        }
    }
    if let Some(success) = value.get("success").and_then(serde_json::Value::as_bool) {
        return !success;
    }
    false
}

fn shell_output_text(value: &serde_json::Value) -> String {
    if let Some(text) = value.as_str() {
        return text.to_owned();
    }
    let mut out = String::new();
    for key in ["output", "stdout", "stderr", "content"] {
        if let Some(text) = value.get(key).and_then(|v| v.as_str()) {
            out.push_str(text);
            out.push('\n');
        }
    }
    out
}

/// One warm-path attempt: encode, blocking socket round-trip, decode. Trace
/// timings stay here (shim-only concern); the wire mechanics live in
/// [`protocol`].
fn forward_once(client: &str, raw: &str) -> Result<String, String> {
    let trace =
        difflore_core::infra::env::flag_set(difflore_core::infra::env::DIFFLORE_HOOK_SHIM_TRACE);
    let started = std::time::Instant::now();
    let request = protocol::encode_request_line(client, raw)?;
    if trace {
        eprintln!(
            "[difflore-hook.trace] encode={}ms",
            started.elapsed().as_millis()
        );
    }
    let response = protocol::ipc_roundtrip_blocking(&request)?;
    if trace {
        eprintln!(
            "[difflore-hook.trace] ipc={}ms",
            started.elapsed().as_millis()
        );
    }
    let output = match protocol::decode_response_line(&response) {
        Ok(output) => output,
        Err(e) => {
            if protocol::is_incompatible_forwarder_error(&e) {
                protocol::remove_current_project_socket_best_effort();
            }
            return Err(e);
        }
    };
    if trace {
        eprintln!(
            "[difflore-hook.trace] decode={}ms",
            started.elapsed().as_millis()
        );
    }
    Ok(output)
}

async fn fallback_to_runtime(client: &str, raw: &str, forward_miss: bool) {
    let debug =
        difflore_core::infra::env::flag_set(difflore_core::infra::env::DIFFLORE_DEBUG_HOOKS);
    match difflore_cli::hook::runtime::output_for_raw_with_forward_miss(
        client,
        raw,
        debug,
        forward_miss,
    )
    .await
    {
        Ok(output) => println!("{output}"),
        Err(e) => {
            if debug {
                eprintln!("[difflore-hook] runtime fallback failed: {e:#}");
            }
            println!("{}", protocol::NOOP_OUTPUT);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_noop_skips_non_mutating_post_tool_use() {
        let raw = r#"{"hook_event_name":"PostToolUse","tool_name":"Read"}"#;
        assert_eq!(fast_noop_output("codex", raw), Some(protocol::NOOP_OUTPUT));
    }

    #[test]
    fn fast_noop_keeps_mutating_tools_on_regular_path() {
        let write = r#"{"hook_event_name":"PostToolUse","tool_name":"Write"}"#;
        assert_eq!(fast_noop_output("claude-code", write), None);

        let patch = r#"{"hook_event_name":"PostToolUse","tool_name":"apply_patch"}"#;
        assert_eq!(fast_noop_output("codex", patch), None);
    }

    #[test]
    fn fast_noop_skips_successful_short_bash() {
        let raw = r#"{
          "hook_event_name":"PostToolUse",
          "tool_name":"Bash",
          "tool_response":{"stdout":"ok\n","stderr":"","exit_code":0}
        }"#;
        assert_eq!(fast_noop_output("codex", raw), Some(protocol::NOOP_OUTPUT));
    }

    #[test]
    fn fast_noop_keeps_failed_bash_on_regular_path() {
        let raw = r#"{
          "hook_event_name":"PostToolUse",
          "tool_name":"Bash",
          "tool_response":{"stdout":"","stderr":"Error: failed\n","exit_code":1}
        }"#;
        assert_eq!(fast_noop_output("codex", raw), None);
    }

    #[test]
    fn fast_noop_keeps_high_signal_bash_on_regular_path() {
        let raw = r#"{
          "hook_event_name":"PostToolUse",
          "tool_name":"Bash",
          "tool_response":{"stdout":"Traceback (most recent call last):\n  File \"src/app.py\", line 1, in <module>\nValueError: typed parser exploded with enough detail\n","exit_code":0}
        }"#;
        assert_eq!(fast_noop_output("claude-code", raw), None);
    }

    #[test]
    fn fast_noop_handles_windsurf_mcp_as_non_mutating() {
        let raw =
            r#"{"agent_action_name":"post_mcp_tool_use","tool_info":{"mcp_tool_name":"search"}}"#;
        assert_eq!(
            fast_noop_output("windsurf", raw),
            Some(protocol::NOOP_OUTPUT)
        );
    }

    #[tokio::test]
    async fn fast_noop_events_match_runtime_noop_behavior() {
        let cases = [
            (
                "codex",
                r#"{"hook_event_name":"PostToolUse","tool_name":"Read"}"#,
            ),
            (
                "codex",
                r#"{
                  "hook_event_name":"PostToolUse",
                  "tool_name":"Bash",
                  "tool_response":{"stdout":"ok\n","stderr":"","exit_code":0}
                }"#,
            ),
            (
                "windsurf",
                r#"{"agent_action_name":"post_mcp_tool_use","tool_info":{"mcp_tool_name":"search"}}"#,
            ),
        ];

        for (client, raw) in cases {
            assert_eq!(
                fast_noop_output(client, raw),
                Some(protocol::NOOP_OUTPUT),
                "{client} payload should be shim-fast-noop eligible",
            );
            let runtime_output = difflore_cli::hook::runtime::output_for_raw(client, raw, false)
                .await
                .unwrap_or_else(|error| format!("runtime-error: {error:#}"));
            assert_eq!(
                runtime_output,
                protocol::NOOP_OUTPUT,
                "{client} fast-noop payload must also be a runtime noop",
            );
        }
    }
}
