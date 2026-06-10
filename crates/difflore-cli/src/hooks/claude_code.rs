//! Claude Code hook adapter.
//!
//! Claude Code (the official Anthropic CLI) invokes hooks with a JSON
//! object on stdin that looks like:
//!
//! ```json
//! {
//!   "session_id": "...",
//!   "cwd": "/abs/path/to/repo",
//!   "hook_event_name": "PostToolUse",
//!   "tool_name": "Edit",
//!   "tool_input": { "file_path": "src/foo.rs", "old_string": "...", "new_string": "..." },
//!   "tool_response": { ... },
//!   "transcript_path": "/abs/path/to/session.jsonl"
//! }
//! ```
//!
//! Not every field is present on every event — `SessionStart` for
//! instance only carries `session_id` + `cwd`. The adapter is
//! permissive on absent fields (returns `None` / falls through) and
//! strict only on the ones it needs for a given event kind.
//!
//! The output shape Claude Code expects is:
//!
//! ```json
//! {
//!   "continue": true,
//!   "hookSpecificOutput": { "additionalContext": "optional long string" }
//! }
//! ```
//!
//! We camelCase the JSON keys on output (matching Claude Code's
//! convention) while keeping our internal `HookResult` `snake_case`.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::synth;
use super::types::{HookEvent, HookResult};
use super::{PayloadAdapter, PlatformAdapter};

pub struct ClaudeCodeAdapter;

/// Typed view of Claude Code's hook stdin payload. Everything except
/// `hook_event_name` is optional because Claude Code sends different subsets of
/// fields per event. The parse stays permissive so a future hook event lands in
/// `Err(...)` from `to_canonical` and the CLI no-ops rather than breaking.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) struct ClaudeHookPayload {
    #[serde(default)]
    hook_event_name: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    tool_input: Option<Value>,
    #[serde(default)]
    tool_response: Option<Value>,
    #[serde(default)]
    transcript_path: Option<String>,
    /// `UserPromptSubmit` carries the prompt under this key.
    #[serde(default)]
    prompt: Option<String>,
}

impl ClaudeHookPayload {
    /// Map the parsed payload into our canonical `HookEvent`. Unknown and
    /// missing event names both return `Err` so the CLI can log + no-op.
    fn into_canonical(self) -> Result<HookEvent, String> {
        let event_name = self
            .hook_event_name
            .as_deref()
            .ok_or_else(|| "missing hook_event_name".to_owned())?;
        match event_name {
            "PreToolUse" => {
                // Only Read is wired for rule pre-injection; other tools fall
                // through to an Err so the hook stays advisory, not blocking.
                let tool_name = self.tool_name.clone().unwrap_or_default();
                if tool_name != "Read" {
                    return Err(format!(
                        "PreToolUse for `{tool_name}` not wired — Read only",
                    ));
                }
                let file_path = self
                    .tool_input
                    .as_ref()
                    .and_then(|v| v.get("file_path"))
                    .and_then(|v| v.as_str())
                    .map(String::from)
                    .ok_or_else(|| "PreToolUse:Read missing tool_input.file_path".to_owned())?;
                Ok(HookEvent::PreToolUseRead {
                    file_path,
                    session_id: self.session_id.clone(),
                })
            }
            "PostToolUse" => {
                let tool_name = self.tool_name.clone().unwrap_or_default();
                // Edit/Write nest the edited path under `tool_input.file_path`,
                // the only shape we act on. Everything else flows through with
                // `file_path = None` so upstream logic can ignore it.
                let file_path = self
                    .tool_input
                    .as_ref()
                    .and_then(|v| v.get("file_path"))
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let diff = synthesise_diff(self.tool_input.as_ref(), self.tool_response.as_ref());
                let (old_text, new_text) = synth::extract_edit_strings(self.tool_input.as_ref());
                Ok(HookEvent::PostToolUse {
                    tool_name,
                    file_path,
                    diff,
                    session_id: self.session_id.clone(),
                    new_text,
                    old_text,
                })
            }
            "SessionStart" => Ok(HookEvent::SessionStart {
                cwd: self.cwd.unwrap_or_default(),
                session_id: self.session_id.clone(),
            }),
            "UserPromptSubmit" => Ok(HookEvent::UserPromptSubmit {
                prompt: self.prompt.unwrap_or_default(),
                session_id: self.session_id.clone(),
            }),
            "Stop" => Ok(HookEvent::Stop {
                session_id: self.session_id.clone(),
                transcript_path: self.transcript_path.clone(),
                cwd: self.cwd.clone(),
            }),
            "SessionEnd" => Ok(HookEvent::SessionEnd {
                session_id: self.session_id.clone(),
                transcript_path: self.transcript_path.clone(),
                cwd: self.cwd.clone(),
            }),
            other => Err(format!("unsupported Claude Code hook event: {other}")),
        }
    }
}

/// Best-effort diff synthesis from Claude Code's tool payloads. Claude Code
/// gives raw tool input, not a unified diff: `Edit` carries
/// `old_string`/`new_string`; `Write` carries just `content`. Line-prefix
/// mechanics live in `synth::diff_old_new` / `synth::diff_content`.
fn synthesise_diff(tool_input: Option<&Value>, _tool_response: Option<&Value>) -> Option<String> {
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
    None
}

impl PayloadAdapter for ClaudeCodeAdapter {
    type Raw = ClaudeHookPayload;
    const PARSE_LABEL: &'static str = "Claude Code";

    fn into_canonical(raw: Self::Raw) -> Result<HookEvent, String> {
        raw.into_canonical()
    }
}

impl PlatformAdapter for ClaudeCodeAdapter {
    fn name(&self) -> &'static str {
        "claude-code"
    }

    fn parse_stdin(&self, raw: &str) -> Result<HookEvent, String> {
        Self::parse_stdin_default(raw)
    }

    fn format_output(&self, result: HookResult) -> String {
        // Claude Code uses camelCase keys and validates that `hookEventName`
        // matches the event that fired the hook — a mismatch drops the entire
        // injection with "Hook returned incorrect event name". Echo the
        // dispatcher's event name, falling back to `PostToolUse` for legacy
        // callers that didn't thread one through.
        let mut obj = json!({
            "continue": result.continue_,
        });
        let _ = result.system_message;
        if let Some(ctx) = result.additional_context {
            let event_name = result.event_name.as_deref().unwrap_or("PostToolUse");
            obj["hookSpecificOutput"] = json!({
                "hookEventName": event_name,
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
    fn parse_post_tool_use_edit_extracts_file_path_and_diff() {
        // The file path and synthesised diff are the signal the rule retriever
        // uses to scope its cascade, so both must come through.
        let adapter = ClaudeCodeAdapter;
        let raw = r#"{
            "hook_event_name": "PostToolUse",
            "session_id": "abc",
            "cwd": "/home/user/proj",
            "tool_name": "Edit",
            "tool_input": {
                "file_path": "src/foo.rs",
                "old_string": "let x = 1;",
                "new_string": "let x = 2;"
            },
            "tool_response": {}
        }"#;
        let event = adapter.parse_stdin(raw).expect("parse ok");
        match event {
            HookEvent::PostToolUse {
                tool_name,
                file_path,
                diff,
                ..
            } => {
                assert_eq!(tool_name, "Edit");
                assert_eq!(file_path.as_deref(), Some("src/foo.rs"));
                let diff = diff.expect("Edit events always carry a synthesised diff");
                assert!(
                    diff.contains("-let x = 1;"),
                    "diff missing old line: {diff}"
                );
                assert!(
                    diff.contains("+let x = 2;"),
                    "diff missing new line: {diff}"
                );
            }
            other => panic!("expected PostToolUse, got {other:?}"),
        }
    }

    #[test]
    fn parse_write_event_synthesises_diff_from_content() {
        // Write events carry `content` instead of old/new; the synthesiser
        // emits a `+`-prefixed block so the retriever has something to match.
        let adapter = ClaudeCodeAdapter;
        let raw = r#"{
            "hook_event_name": "PostToolUse",
            "tool_name": "Write",
            "tool_input": {
                "file_path": "new.rs",
                "content": "fn main() {}\n"
            }
        }"#;
        let event = adapter.parse_stdin(raw).expect("parse ok");
        if let HookEvent::PostToolUse { diff, .. } = event {
            let diff = diff.expect("Write must synthesise a diff");
            assert!(diff.contains("+fn main() {}"), "got: {diff}");
        } else {
            panic!("expected PostToolUse");
        }
    }

    #[test]
    fn parse_unsupported_event_errors_without_panicking() {
        // An unmodelled hook event must return `Err` (CLI logs + no-ops) rather
        // than panic and take the assistant down.
        let adapter = ClaudeCodeAdapter;
        let raw = r#"{"hook_event_name":"SomeFutureEventWeHaventHeardOf"}"#;
        let err = adapter.parse_stdin(raw).unwrap_err();
        assert!(err.contains("unsupported"), "got: {err}");
    }

    #[test]
    fn parse_missing_event_name_errors() {
        // A payload without `hook_event_name` is invalid — reject, don't assume
        // a default.
        let adapter = ClaudeCodeAdapter;
        let raw = r#"{"session_id":"abc"}"#;
        let err = adapter.parse_stdin(raw).unwrap_err();
        assert!(err.contains("missing"), "got: {err}");
    }

    #[test]
    fn format_output_noop_emits_continue_true_only() {
        // The empty result must emit minimal-but-valid JSON so Claude Code
        // doesn't render a spurious empty system message.
        let adapter = ClaudeCodeAdapter;
        let out = adapter.format_output(HookResult::noop());
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["continue"], true);
        assert!(v.get("systemMessage").is_none());
        assert!(v.get("hookSpecificOutput").is_none());
    }

    #[test]
    fn format_output_omits_system_message() {
        let adapter = ClaudeCodeAdapter;
        let mut result = HookResult::noop();
        result.system_message = Some("DiffLore lifecycle note".to_owned());

        let out = adapter.format_output(result);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(v.get("systemMessage").is_none());
    }

    #[test]
    fn format_output_with_context_nests_additional_context() {
        // Claude Code expects the extra context inside
        // `hookSpecificOutput.additionalContext`, not at the top level.
        let adapter = ClaudeCodeAdapter;
        let out = adapter.format_output(HookResult::with_context("Rule 1: X"));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["continue"], true);
        assert_eq!(v["hookSpecificOutput"]["additionalContext"], "Rule 1: X");
    }

    #[test]
    fn format_output_echoes_event_name_so_pretooluse_injection_lands() {
        // Regression for "Hook returned incorrect event name": Claude Code
        // drops the injection if `hookEventName` doesn't match the firing
        // event, so PreToolUse responses must echo PreToolUse.
        let adapter = ClaudeCodeAdapter;
        let mut r = HookResult::with_context("Rule 1: cap log volume");
        r.event_name = Some("PreToolUse".into());
        let out = adapter.format_output(r);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v["hookSpecificOutput"]["hookEventName"], "PreToolUse",
            "PreToolUse responses must echo the firing event name, not the legacy PostToolUse default; got: {out}"
        );

        // Backwards-compat: callers that didn't thread an event name through
        // keep the PostToolUse default.
        let r2 = HookResult::with_context("legacy");
        let v2: Value = serde_json::from_str(&adapter.format_output(r2)).unwrap();
        assert_eq!(v2["hookSpecificOutput"]["hookEventName"], "PostToolUse");
    }
}
