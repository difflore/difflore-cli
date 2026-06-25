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
//! **Host-visible messages**: keep DiffLore lifecycle notes out of
//! `systemMessage`; hosts can render that channel as event-name chatter in
//! the user's transcript.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::synth;
use super::types::{HookEvent, HookResult};
use super::{PayloadAdapter, PlatformAdapter};

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
            // Both signal a new session/turn; collapse into SessionStart so
            // CLI warmup runs once either way.
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
            // Ignored: these fire on every tool call / compaction / permission
            // prompt — too chatty for rule retrieval, which only wants
            // after-the-code-changed signals.
            "BeforeTool" | "PreCompress" | "Notification" => Err(format!(
                "Gemini CLI event {event_name} is intentionally ignored"
            )),
            other => Err(format!("unsupported Gemini CLI hook event: {other}")),
        }
    }
}

/// Build a `PostToolUse` from Gemini's `AfterTool` payload. Probes the
/// common `tool_input` path keys so file-mutation tools get a `file_path`.
///
/// Maps Gemini's `WriteFile` to the canonical `Write`: the dispatch layer
/// only acts on `Edit`/`Write`/`MultiEdit`, so without this every Gemini
/// file write would be noop'd and miss rule injection.
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
    let target_files = file_path.iter().cloned().collect();
    HookEvent::PostToolUse {
        tool_name,
        cwd: p.cwd,
        file_path,
        target_files,
        diff,
        session_id: p.session_id,
        new_text,
        old_text,
    }
}

/// Best-effort diff synthesis for Gemini tool payloads, handling three
/// shapes: `{old_string, new_string}` (Edit), `{content}` (`WriteFile`),
/// and `{command, output}` (`ShellCommand`, surfacing both so retrievers
/// keying off "npm install" or "curl" still match). ANSI escapes in shell
/// output are stripped so downstream text matching ignores colour codes.
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
        let cleaned = tool_response
            .and_then(|v| v.get("output"))
            .and_then(|v| v.as_str())
            .map(strip_ansi);
        return synth::diff_shell(Some(cmd), cleaned.as_deref());
    }
    None
}

/// Strip ANSI escape sequences from `s`. A tiny state machine (rather than
/// the `regex` crate, to avoid an extra dependency) covering CSI sequences
/// `ESC [ … final-byte`, OSC/DCS-style string escapes terminated by BEL/ST,
/// the 8-bit CSI prefix (0x9B), and two-byte ESC sequences (e.g. `ESC 7`,
/// `ESC M`). Fast-path returns `s` unchanged when neither ESC (0x1B) nor 8-bit
/// CSI (0x9B) appears.
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
        if b == 0x1B {
            if i + 1 < bytes.len() && bytes[i + 1] == b'[' {
                i += 2;
                i = skip_csi_body(bytes, i);
                continue;
            }
            if i + 1 < bytes.len() && matches!(bytes[i + 1], b']' | b'P' | b'^' | b'_') {
                i += 2;
                i = skip_string_escape_body(bytes, i);
                continue;
            }
            // Two-byte ESC sequence (e.g. ESC M, ESC 7): skip ESC plus one
            // more byte when present.
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

/// Skip an OSC/DCS/PM/APC string body, terminated by BEL or ST (`ESC \`).
fn skip_string_escape_body(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() {
        if bytes[i] == 0x07 {
            return i + 1;
        }
        if bytes[i] == 0x1B && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
            return i + 2;
        }
        i += 1;
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
        // `continue` is always emitted so future Gemini builds that treat
        // its absence as "stop" don't accidentally terminate the agent.
        let mut obj = json!({
            "continue": result.continue_,
            "suppressOutput": false,
        });
        let _ = result.system_message;
        if let Some(ctx) = result.additional_context {
            // Gemini pipes `hookSpecificOutput.additionalContext` back into
            // the transcript for SessionStart/AfterTool — where advisory
            // rule injection belongs.
            obj["hookSpecificOutput"] = json!({
                "additionalContext": ctx,
            });
        }
        crate::support::util::json_compact_or(&obj, "{\"continue\":true}")
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
        // ShellCommand output often leaks ANSI colour codes; strip them
        // before they reach rule retrieval.
        let adapter = GeminiCliAdapter;
        // Build the JSON via serde_json::json! so the ESC byte (0x1B) lives
        // only in the produced string and the source file stays ASCII-clean.
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
    fn strip_ansi_removes_osc_and_dcs_bodies() {
        let osc = format!(
            "before {esc}]8;;https://example.test{bel}link{esc}]8;;{bel} after",
            esc = '\u{001b}',
            bel = '\u{0007}'
        );
        assert_eq!(strip_ansi(&osc), "before link after");

        let dcs = format!("a{esc}Pprivate;payload{esc}\\b", esc = '\u{001b}');
        assert_eq!(strip_ansi(&dcs), "ab");
    }

    #[test]
    fn parse_ignored_events_error_loudly_so_cli_noops() {
        // These events must error (so the CLI no-ops) rather than silently
        // returning Stop or anything else actionable.
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
    fn format_output_omits_system_message() {
        let adapter = GeminiCliAdapter;
        let mut r = HookResult::noop();
        r.system_message = Some("\u{001b}[31mred\u{001b}[0m OK".into());
        let out = adapter.format_output(r);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(v.get("systemMessage").is_none());
    }
}
