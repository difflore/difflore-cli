//! Cursor hook adapter.
//!
//! Cursor IDE invokes hooks with a JSON object on stdin whose shape
//! differs from Claude Code's in three important ways:
//!
//!   1. The event discriminator is `hook_event_name` with *camelCase*
//!      values (`afterFileEdit`, `afterShellExecution`, …) rather than
//!      Claude's `PostToolUse` naming.
//!   2. Cursor exposes a `workspace_roots` array instead of a single
//!      `cwd` string. We use `workspace_roots[0]` as the effective CWD.
//!   3. Shell-command events come as `command`/`output` pairs rather
//!      than `tool_input`/`tool_response`. We synthesise a `Bash`-shaped
//!      tool call from those so the downstream rule retriever sees a
//!      uniform `PostToolUse` shape regardless of origin.
//!
//! Example stdin (from claude-mem's cursor adapter reference):
//!
//! ```json
//! {
//!   "conversation_id": "...",
//!   "workspace_roots": ["/path/to/repo"],
//!   "hook_event_name": "afterFileEdit",
//!   "tool_name": "Edit",
//!   "tool_input": { "file_path": "src/foo.rs", "edits": [...] },
//!   "result_json": { ... }
//! }
//! ```
//!
//! Cursor's output expectation is minimal — it honours a single
//! `{ "continue": true|false }` flag. For advisory context we include
//! a `context` string alongside so Cursor's newer builds can surface
//! it; older Cursor builds just ignore the extra field.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::synth;
use super::types::{HookEvent, HookResult};
use super::{PayloadAdapter, PlatformAdapter};

pub struct CursorAdapter;

/// Typed view of Cursor's hook stdin payload. Every field is optional: Cursor
/// ships different subsets per event, and a missing field should no-op rather
/// than reject a structurally-valid payload.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub(crate) struct CursorHookPayload {
    #[serde(default)]
    hook_event_name: Option<String>,
    #[serde(default)]
    conversation_id: Option<String>,
    #[serde(default)]
    workspace_roots: Option<Vec<String>>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    tool_input: Option<Value>,
    /// Cursor's analogue of `tool_response` (kept under the Cursor spelling).
    #[serde(default)]
    result_json: Option<Value>,
    /// Shell-command events ship these two in place of `tool_input` /
    /// `tool_response`. We fold them into a synthetic Bash tool call.
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    output: Option<String>,
    /// `beforeSubmitPrompt` payloads carry the prompt under one of several keys
    /// depending on Cursor version; all are probed.
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    input: Option<String>,
    #[serde(default)]
    message: Option<String>,
    /// Some Cursor events ship the edited path at the top level rather than
    /// nested under `tool_input`.
    #[serde(default)]
    file_path: Option<String>,
}

impl CursorHookPayload {
    /// Map the parsed Cursor payload into our canonical `HookEvent`. See module
    /// docs for the event-name mapping.
    fn into_canonical(self) -> Result<HookEvent, String> {
        let event_name = self
            .hook_event_name
            .as_deref()
            .ok_or_else(|| "missing hook_event_name".to_owned())?;
        match event_name {
            "afterFileEdit" => Ok(post_tool_use_for_file_edit(self)),
            "afterMCPExecution" => Ok(HookEvent::PostToolUse {
                tool_name: "afterMCPExecution".to_owned(),
                file_path: None,
                diff: None,
                session_id: None,
                new_text: None,
                old_text: None,
            }),
            "afterShellExecution" => Ok(HookEvent::PostToolUse {
                // Synthesise a Bash-shaped entry so downstream logic recognises
                // shell activity uniformly across clients.
                tool_name: "Bash".to_owned(),
                file_path: None,
                diff: synth::diff_shell(self.command.as_deref(), self.output.as_deref()),
                session_id: None,
                new_text: None,
                old_text: None,
            }),
            "beforeSubmitPrompt" => {
                let prompt = self
                    .prompt
                    .or(self.query)
                    .or(self.input)
                    .or(self.message)
                    .unwrap_or_default();
                Ok(HookEvent::UserPromptSubmit {
                    prompt,
                    session_id: None,
                })
            }
            "stop" => Ok(HookEvent::Stop {
                session_id: None,
                transcript_path: None,
                cwd: None,
            }),
            other => Err(format!("unsupported Cursor hook event: {other}")),
        }
    }
}

/// Extract a `PostToolUse` for Cursor's `afterFileEdit`.
///
/// The file path may live at the top level, under `tool_input.file_path`, or
/// under `tool_input.path` depending on Cursor release; all three are probed so
/// hooks survive Cursor updates without a `DiffLore` release.
fn post_tool_use_for_file_edit(p: CursorHookPayload) -> HookEvent {
    let file_path = p.file_path.clone().or_else(|| {
        p.tool_input
            .as_ref()
            .and_then(|v| v.get("file_path").or_else(|| v.get("path")))
            .and_then(|v| v.as_str())
            .map(String::from)
    });
    let diff = synthesise_edit_diff(p.tool_input.as_ref());
    let (old_text, new_text) = synth::extract_edit_strings(p.tool_input.as_ref());
    HookEvent::PostToolUse {
        tool_name: p.tool_name.unwrap_or_else(|| "Edit".to_owned()),
        file_path,
        diff,
        session_id: None,
        new_text,
        old_text,
    }
}

/// Synthesise a text diff from Cursor's edit payload.
///
/// Cursor ships edits in a few shapes across versions:
///   - `{ "edits": [{ "old_string", "new_string" }, ...] }` (array)
///   - `{ "old_string": "...", "new_string": "..." }` (flat)
///   - `{ "content": "..." }` (whole-file write)
///
/// Line-prefix mechanics live in `synth`.
fn synthesise_edit_diff(tool_input: Option<&Value>) -> Option<String> {
    let input = tool_input?;
    if let Some(edits) = input.get("edits").and_then(|v| v.as_array()) {
        let mut out = String::new();
        for edit in edits {
            if let (Some(old), Some(new)) = (
                edit.get("old_string").and_then(|v| v.as_str()),
                edit.get("new_string").and_then(|v| v.as_str()),
            ) {
                synth::append_old_new(&mut out, old, new);
            }
        }
        if !out.is_empty() {
            return Some(out);
        }
    }
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

impl PayloadAdapter for CursorAdapter {
    type Raw = CursorHookPayload;
    const PARSE_LABEL: &'static str = "Cursor";

    fn into_canonical(raw: Self::Raw) -> Result<HookEvent, String> {
        raw.into_canonical()
    }
}

impl PlatformAdapter for CursorAdapter {
    fn name(&self) -> &'static str {
        "cursor"
    }

    fn parse_stdin(&self, raw: &str) -> Result<HookEvent, String> {
        Self::parse_stdin_default(raw)
    }

    fn format_output(&self, result: HookResult) -> String {
        // Cursor's minimum contract is `{ "continue": bool }`. Newer builds also
        // pick up a `context` string for advisory injection; older builds ignore
        // it, so include it whenever present rather than version-sniffing.
        let mut obj = json!({
            "continue": result.continue_,
        });
        if let Some(ctx) = result.additional_context {
            obj["context"] = Value::String(ctx);
        }
        let _ = result.system_message;
        crate::commands::util::json_compact_or(&obj, "{\"continue\":true}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_after_file_edit_flat_form() {
        // Old Cursor shape: old_string/new_string at the top of tool_input.
        let adapter = CursorAdapter;
        let raw = r#"{
            "hook_event_name": "afterFileEdit",
            "workspace_roots": ["/tmp/proj"],
            "tool_name": "Edit",
            "tool_input": {
                "file_path": "src/foo.rs",
                "old_string": "let x = 1;",
                "new_string": "let x = 2;"
            }
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
                let d = diff.expect("diff synthesised");
                assert!(d.contains("-let x = 1;"));
                assert!(d.contains("+let x = 2;"));
            }
            other => panic!("expected PostToolUse, got {other:?}"),
        }
    }

    #[test]
    fn parse_after_file_edit_array_form_with_edits() {
        // Newer Cursor packs edits as an array of old/new pairs; all must be
        // collected into one diff so an Edit with N hunks isn't dropped.
        let adapter = CursorAdapter;
        let raw = r#"{
            "hook_event_name": "afterFileEdit",
            "tool_input": {
                "path": "src/bar.rs",
                "edits": [
                    { "old_string": "A", "new_string": "B" },
                    { "old_string": "C", "new_string": "D" }
                ]
            }
        }"#;
        let event = adapter.parse_stdin(raw).expect("parse ok");
        if let HookEvent::PostToolUse {
            file_path, diff, ..
        } = event
        {
            // Must find path under `tool_input.path` (not `file_path`).
            assert_eq!(file_path.as_deref(), Some("src/bar.rs"));
            let d = diff.expect("array form diff synthesised");
            assert!(d.contains("-A") && d.contains("+B"));
            assert!(d.contains("-C") && d.contains("+D"));
        } else {
            panic!("expected PostToolUse");
        }
    }

    #[test]
    fn parse_after_shell_execution_synthesises_bash_diff() {
        let adapter = CursorAdapter;
        let raw = r#"{
            "hook_event_name": "afterShellExecution",
            "command": "echo hi",
            "output": "hi\n"
        }"#;
        let event = adapter.parse_stdin(raw).expect("parse ok");
        if let HookEvent::PostToolUse {
            tool_name,
            file_path,
            diff,
            ..
        } = event
        {
            assert_eq!(tool_name, "Bash");
            assert!(file_path.is_none());
            let d = diff.expect("shell diff");
            assert!(d.contains("$ echo hi"));
            assert!(d.contains("+hi"));
        } else {
            panic!("expected PostToolUse");
        }
    }

    #[test]
    fn parse_before_submit_prompt_probes_alt_keys() {
        // A Cursor version that ships `query` instead of `prompt` must not drop
        // the payload.
        let adapter = CursorAdapter;
        let raw = r#"{"hook_event_name":"beforeSubmitPrompt","query":"hello"}"#;
        let event = adapter.parse_stdin(raw).expect("parse ok");
        assert_eq!(
            event,
            HookEvent::UserPromptSubmit {
                prompt: "hello".into(),
                session_id: None,
            }
        );
    }

    #[test]
    fn parse_unsupported_event_errors() {
        let adapter = CursorAdapter;
        let err = adapter
            .parse_stdin(r#"{"hook_event_name":"someNewCursorEvent"}"#)
            .unwrap_err();
        assert!(err.contains("unsupported"), "got: {err}");
    }

    #[test]
    fn format_output_noop_emits_continue_only() {
        let adapter = CursorAdapter;
        let out = adapter.format_output(HookResult::noop());
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["continue"], true);
        assert!(v.get("context").is_none());
    }

    #[test]
    fn format_output_omits_system_message() {
        let adapter = CursorAdapter;
        let mut result = HookResult::noop();
        result.system_message = Some("DiffLore lifecycle note".to_owned());

        let out = adapter.format_output(result);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(v.get("systemMessage").is_none());
    }

    #[test]
    fn format_output_with_context_includes_context_field() {
        // Cursor's newer builds honour `context` at the top level; always
        // include it when there is advisory context to surface.
        let adapter = CursorAdapter;
        let out = adapter.format_output(HookResult::with_context("Rule 1: X"));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["continue"], true);
        assert_eq!(v["context"], "Rule 1: X");
    }
}
