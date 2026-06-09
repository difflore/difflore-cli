//! Gemini CLI hook adapter.
//!
//! Gemini CLI supports 11 lifecycle hooks; `DiffLore` maps 4 to canonical
//! `HookEvent` variants and ignores the remaining 7 as not actionable:
//!
//!   | Gemini event   | Canonical event              |
//!   |----------------|------------------------------|
//!   | `SessionStart`   | `SessionStart { cwd }`       |
//!   | `BeforeAgent`    | `SessionStart { cwd }` *    |
//!   | `AfterAgent`     | `Stop`                       |
//!   | `AfterTool`      | `PostToolUse { … }`          |
//!   | `SessionEnd`     | `SessionEnd`                 |
//!   | `BeforeTool`     | no-op (pre-execution noise)  |
//!   | `PreCompress`    | no-op                        |
//!   | Notification   | no-op                        |
//!
//! \* `BeforeAgent` is treated as a session-start so `DiffLore`'s
//! per-session warmup also fires when users resume sessions; this
//! costs nothing extra — `ensure_ready` is cached per process.
//!
//! Example stdin (verified against Gemini CLI's published hook schema):
//!
//! ```json
//! {
//!   "session_id": "...",
//!   "cwd": "/path/to/repo",
//!   "hook_event_name": "AfterTool",
//!   "tool_name": "WriteFile",
//!   "tool_input":  { "path": "src/foo.py", "content": "..." },
//!   "tool_response": { "success": true, "output": "…" },
//!   "transcript_path": "/abs/path/to/transcript.jsonl"
//! }
//! ```
//!
//! **ANSI escape stripping**: Gemini CLI is known to leak raw ANSI
//! color sequences through tool output into system messages. Our
//! `format_output` path strips them out of `systemMessage` before
//! shipping so the user doesn't see `\x1b[31m` garbage in their UI.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::synth;
use super::types::{HookEvent, HookResult};
use super::{PayloadAdapter, PlatformAdapter};

/// Zero-sized marker — no adapter state.
pub struct GeminiCliAdapter;

/// Typed view of Gemini CLI's hook stdin. Every field is optional:
/// Gemini ships different subsets per event, and we reject only when
/// `hook_event_name` itself is absent (structurally invalid).
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub(crate) struct GeminiHookPayload {
    #[serde(default)]
    hook_event_name: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    transcript_path: Option<String>,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    tool_input: Option<Value>,
    #[serde(default)]
    tool_response: Option<Value>,
    #[serde(default)]
    prompt: Option<String>,
    /// `AfterAgent` carries the full assistant text here.
    #[serde(default)]
    prompt_response: Option<String>,
}

impl GeminiHookPayload {
    fn into_canonical(self) -> Result<HookEvent, String> {
        let event_name = self
            .hook_event_name
            .as_deref()
            .ok_or_else(|| "missing hook_event_name".to_owned())?;
        match event_name {
            // SessionStart and BeforeAgent both signal "new session/turn
            // starting" — collapse both into SessionStart so warmup logic
            // in the CLI runs once either way.
            "SessionStart" | "BeforeAgent" => Ok(HookEvent::SessionStart {
                cwd: self.cwd.unwrap_or_default(),
                session_id: None,
            }),
            "AfterAgent" => Ok(HookEvent::Stop {
                session_id: None,
                transcript_path: None,
                cwd: None,
            }),
            "AfterTool" => Ok(after_tool_event(self)),
            "SessionEnd" => Ok(HookEvent::SessionEnd {
                session_id: None,
                transcript_path: None,
                cwd: None,
            }),
            // BeforeTool / PreCompress / Notification: Gemini fires
            // these for every tool call / every compaction / every
            // permission prompt — way too chatty for rule retrieval,
            // which wants "after the code actually changed" signals.
            "BeforeTool" | "PreCompress" | "Notification" => Err(format!(
                "Gemini CLI event {event_name} is intentionally ignored"
            )),
            other => Err(format!("unsupported Gemini CLI hook event: {other}")),
        }
    }
}

/// Build a `PostToolUse` from Gemini's `AfterTool` payload.
///
/// The `tool_input` shape is tool-specific (Gemini's built-ins include
/// `WriteFile`, `Edit`, `ReadFile`, `ShellCommand`, …). We probe the
/// common path keys so typical file-mutation tools flow through with a
/// `file_path` set.
///
/// Tool-name normalisation: the dispatch layer only acts on the
/// canonical Claude Code names (`Edit`/`Write`/`MultiEdit`); Gemini's
/// `WriteFile` would otherwise get noop'd and Gemini users would
/// silently miss rule injection on every file write. Map it here so
/// downstream callers don't need a per-platform allowlist.
fn after_tool_event(p: GeminiHookPayload) -> HookEvent {
    let raw_tool_name = p.tool_name.clone().unwrap_or_default();
    let tool_name = match raw_tool_name.as_str() {
        "WriteFile" => "Write".to_owned(),
        _ => raw_tool_name,
    };
    let file_path = p
        .tool_input
        .as_ref()
        .and_then(|v| {
            v.get("file_path")
                .or_else(|| v.get("path"))
                .or_else(|| v.get("file"))
        })
        .and_then(|v| v.as_str())
        .map(String::from);
    let diff = synthesise_diff(p.tool_input.as_ref(), p.tool_response.as_ref());
    let (old_text, new_text) = synth::extract_edit_strings(p.tool_input.as_ref());
    HookEvent::PostToolUse {
        tool_name,
        file_path,
        diff,
        session_id: p.session_id,
        new_text,
        old_text,
    }
}

/// Best-effort diff synthesis for Gemini tool payloads.
///
/// Handles three common shapes:
///   - `{ "old_string", "new_string" }` (Edit)
///   - `{ "content" }` (`WriteFile`)
///   - `{ "command", "output" }` (`ShellCommand`) — surfaces both so
///     rule retrievers keying off "npm install" or "curl" still match.
///
/// Any tool output that carries ANSI escape sequences (common with
/// `ShellCommand`) is stripped before being folded into the diff;
/// downstream text matching shouldn't have to care about terminal
/// colour codes.
fn synthesise_diff(tool_input: Option<&Value>, tool_response: Option<&Value>) -> Option<String> {
    let input = tool_input?;
    if let (Some(old), Some(new)) = (
        input.get("old_string").and_then(|v| v.as_str()),
        input.get("new_string").and_then(|v| v.as_str()),
    ) {
        return Some(synth::diff_old_new(old, new));
    }
    if let Some(content) = input.get("content").and_then(|v| v.as_str()) {
        return Some(synth::diff_content(content));
    }
    if let Some(cmd) = input.get("command").and_then(|v| v.as_str()) {
        // Gemini stuffs shell output into `tool_response.output` — we
        // sanitise ANSI before it lands in the retriever's text index.
        let cleaned = tool_response
            .and_then(|v| v.get("output"))
            .and_then(|v| v.as_str())
            .map(strip_ansi);
        return synth::diff_shell(Some(cmd), cleaned.as_deref());
    }
    None
}

/// Strip ANSI escape sequences (CSI / OSC / ESC-prefixed control codes)
/// from `s`. Implemented as a tiny state machine rather than via the
/// `regex` crate so the CLI doesn't pick up an extra dependency just
/// for this one call site.
///
/// Covers the cases claude-mem's regex targets:
///   - CSI sequences `ESC [ … final-byte` where final-byte is
///     `@ … ~` (0x40..=0x7E).
///   - 8-bit CSI prefix (0x9B) with the same body.
///   - Simple two-byte ESC sequences (e.g. `ESC 7`, `ESC M`).
///
/// Returns `s` unchanged on the fast path when neither ESC (0x1B) nor
/// 8-bit CSI (0x9B) appears at all.
pub(crate) fn strip_ansi(s: &str) -> String {
    if !s.contains('\x1b') && !s.contains('\u{009b}') {
        return s.to_owned();
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        // 8-bit CSI (U+009B, encoded in UTF-8 as 0xC2 0x9B).
        if b == 0xC2 && i + 1 < bytes.len() && bytes[i + 1] == 0x9B {
            i += 2;
            i = skip_csi_body(bytes, i);
            continue;
        }
        // ESC-prefixed sequence.
        if b == 0x1B {
            if i + 1 < bytes.len() && bytes[i + 1] == b'[' {
                // CSI: ESC [ … final-byte
                i += 2;
                i = skip_csi_body(bytes, i);
                continue;
            }
            // Two-byte ESC sequences (e.g. ESC M, ESC 7) — skip the
            // ESC plus one more byte when present.
            if i + 1 < bytes.len() {
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        out.push(b);
        i += 1;
    }
    // Safety: input was valid UTF-8, and every byte we drop is a full
    // ASCII escape sequence — we never split a multi-byte char.
    String::from_utf8(out).unwrap_or_else(|_| s.to_owned())
}

/// Skip the body of a CSI sequence starting at `i`, returning the
/// index just past the final byte. The body consists of any mix of
/// parameter bytes (0x30..=0x3F) and intermediate bytes (0x20..=0x2F),
/// terminated by a single final byte (0x40..=0x7E).
fn skip_csi_body(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() {
        let c = bytes[i];
        i += 1;
        if (0x40..=0x7E).contains(&c) {
            return i;
        }
    }
    i
}

impl PayloadAdapter for GeminiCliAdapter {
    type Raw = GeminiHookPayload;
    const PARSE_LABEL: &'static str = "Gemini CLI";

    fn into_canonical(raw: Self::Raw) -> Result<HookEvent, String> {
        raw.into_canonical()
    }
}

impl PlatformAdapter for GeminiCliAdapter {
    fn name(&self) -> &'static str {
        "gemini-cli"
    }

    fn parse_stdin(&self, raw: &str) -> Result<HookEvent, String> {
        Self::parse_stdin_default(raw)
    }

    fn format_output(&self, result: HookResult) -> String {
        // Gemini CLI's documented output shape:
        //   { continue, suppressOutput, systemMessage, hookSpecificOutput }
        // `continue` is always included to prevent accidental agent
        // termination on future Gemini builds that treat absence as
        // "stop".
        let mut obj = json!({
            "continue": result.continue_,
            "suppressOutput": false,
        });
        if let Some(msg) = result.system_message {
            obj["systemMessage"] = Value::String(strip_ansi(&msg));
        }
        if let Some(ctx) = result.additional_context {
            // Gemini CLI pipes `hookSpecificOutput.additionalContext`
            // back into the conversation transcript for SessionStart
            // and AfterTool events — the natural place for advisory
            // rule injection.
            obj["hookSpecificOutput"] = json!({
                "additionalContext": ctx,
            });
        }
        crate::commands::util::json_compact_or(&obj, "{\"continue\":true}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_session_start_reads_cwd() {
        let adapter = GeminiCliAdapter;
        let raw = r#"{"hook_event_name":"SessionStart","cwd":"/tmp/x"}"#;
        assert_eq!(
            adapter.parse_stdin(raw).unwrap(),
            HookEvent::SessionStart {
                cwd: "/tmp/x".into(),
                session_id: None,
            }
        );
    }

    #[test]
    fn parse_before_agent_maps_to_session_start() {
        // Deliberate collapse: BeforeAgent triggers session warmup too.
        let adapter = GeminiCliAdapter;
        let raw = r#"{"hook_event_name":"BeforeAgent","cwd":"/home/me/p"}"#;
        assert_eq!(
            adapter.parse_stdin(raw).unwrap(),
            HookEvent::SessionStart {
                cwd: "/home/me/p".into(),
                session_id: None,
            }
        );
    }

    #[test]
    fn parse_after_agent_maps_to_stop() {
        let adapter = GeminiCliAdapter;
        assert_eq!(
            adapter
                .parse_stdin(r#"{"hook_event_name":"AfterAgent"}"#)
                .unwrap(),
            HookEvent::Stop {
                session_id: None,
                transcript_path: None,
                cwd: None
            }
        );
    }

    #[test]
    fn parse_after_tool_extracts_file_path_and_diff() {
        let adapter = GeminiCliAdapter;
        let raw = r#"{
            "hook_event_name": "AfterTool",
            "tool_name": "Edit",
            "tool_input": {
                "file_path": "src/foo.py",
                "old_string": "a=1",
                "new_string": "a=2"
            }
        }"#;
        if let HookEvent::PostToolUse {
            tool_name,
            file_path,
            diff,
            ..
        } = adapter.parse_stdin(raw).unwrap()
        {
            assert_eq!(tool_name, "Edit");
            assert_eq!(file_path.as_deref(), Some("src/foo.py"));
            let d = diff.unwrap();
            assert!(d.contains("-a=1") && d.contains("+a=2"));
        } else {
            panic!("expected PostToolUse");
        }
    }

    #[test]
    fn parse_after_tool_normalises_writefile_to_write() {
        // Regression: Gemini's `WriteFile` tool was passed through
        // unmapped, so the hook dispatcher's `Edit|Write|MultiEdit`
        // allowlist noop'd every Gemini file write — silently skipping
        // rule injection on the canonical Gemini editing surface.
        let adapter = GeminiCliAdapter;
        let raw = r#"{
            "hook_event_name": "AfterTool",
            "tool_name": "WriteFile",
            "tool_input": {
                "file_path": "src/new.py",
                "content": "print('hi')"
            }
        }"#;
        if let HookEvent::PostToolUse { tool_name, .. } = adapter.parse_stdin(raw).unwrap() {
            assert_eq!(tool_name, "Write");
        } else {
            panic!("expected PostToolUse");
        }
    }

    #[test]
    fn parse_after_tool_shell_strips_ansi_from_output() {
        // Regression: Gemini's ShellCommand often leaks ANSI colour
        // codes — we must strip them before they reach rule retrieval.
        // The JSON input uses `` unicode escapes (legal JSON) so
        // serde accepts the payload; strip_ansi then cleans them up.
        let adapter = GeminiCliAdapter;
        // Build the JSON via serde_json::json! so the ESC byte (0x1B)
        // lives only in the produced string; the source file stays
        // ASCII-clean and survives Edit / Write tools that strip C0
        // control bytes from string literals.
        let output = format!("{esc}[31mred{esc}[0m plain", esc = '\u{001b}');
        let payload = json!({
            "hook_event_name": "AfterTool",
            "tool_name": "ShellCommand",
            "tool_input": { "command": "ls" },
            "tool_response": { "output": output },
        });
        let raw = serde_json::to_string(&payload).unwrap();
        if let HookEvent::PostToolUse { diff, .. } = adapter.parse_stdin(&raw).unwrap() {
            let d = diff.unwrap();
            assert!(d.contains("$ ls"));
            assert!(d.contains("+red plain"), "got: {d:?}");
            assert!(!d.contains('\x1b'), "ANSI escape leaked into diff: {d:?}");
        } else {
            panic!("expected PostToolUse");
        }
    }

    #[test]
    fn parse_ignored_events_error_loudly_so_cli_noops() {
        // BeforeTool / PreCompress / Notification are deliberately
        // not modelled. They must error (so the CLI no-ops) rather
        // than silently returning Stop or anything else actionable.
        let adapter = GeminiCliAdapter;
        for ev in ["BeforeTool", "PreCompress", "Notification"] {
            let raw = format!(r#"{{"hook_event_name":"{ev}"}}"#);
            let err = adapter.parse_stdin(&raw).unwrap_err();
            assert!(err.contains("ignored"), "for {ev}: {err}");
        }
    }

    #[test]
    fn format_output_includes_continue_and_suppress_output() {
        let adapter = GeminiCliAdapter;
        let out = adapter.format_output(HookResult::noop());
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["continue"], true);
        assert_eq!(v["suppressOutput"], false);
    }

    #[test]
    fn format_output_nests_additional_context_under_hook_specific_output() {
        let adapter = GeminiCliAdapter;
        let out = adapter.format_output(HookResult::with_context("R1"));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["hookSpecificOutput"]["additionalContext"], "R1");
    }

    #[test]
    fn format_output_strips_ansi_from_system_message() {
        let adapter = GeminiCliAdapter;
        let mut r = HookResult::noop();
        r.system_message = Some("\u{001b}[31mred\u{001b}[0m OK".into());
        let out = adapter.format_output(r);
        let v: Value = serde_json::from_str(&out).unwrap();
        let msg = v["systemMessage"].as_str().unwrap();
        assert!(!msg.contains('\x1b'), "ANSI leaked: {msg:?}");
        assert!(msg.contains("red OK"), "content lost: {msg:?}");
    }
}
