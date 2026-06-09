//! Windsurf hook adapter.
//!
//! Windsurf wraps every hook payload in a common envelope keyed by
//! `agent_action_name` with a `tool_info` object carrying per-event
//! detail. The envelope also carries `trajectory_id` / `execution_id`
//! as session IDs and (for command-like events) a `cwd`.
//!
//! Example stdin (`post_write_code`):
//!
//! ```json
//! {
//!   "agent_action_name": "post_write_code",
//!   "trajectory_id": "...",
//!   "execution_id": "...",
//!   "timestamp": "...",
//!   "tool_info": {
//!     "file_path": "src/foo.ts",
//!     "edits": [{ "old_string": "a", "new_string": "b" }]
//!   }
//! }
//! ```
//!
//! Event mapping:
//!
//!   | Windsurf action          | Canonical event           |
//!   |--------------------------|---------------------------|
//!   | `pre_user_prompt`        | `UserPromptSubmit`        |
//!   | `post_write_code`        | `PostToolUse { Write }`   |
//!   | `post_run_command`       | `PostToolUse { Bash }`    |
//!   | `post_mcp_tool_use`      | `PostToolUse { mcp_*  }`  |
//!   | `post_cascade_response`  | `Stop`                    |
//!   | `session_start` / `beforeAgentResponse` | `SessionStart` |
//!   | anything else            | error (CLI no-ops)        |
//!
//! Windsurf exit codes: 0 = success, 2 = block (pre-hooks only). We
//! never block, so the adapter output is always consumed with exit 0.
//! Output contract: Windsurf ignores extra fields on advisory hooks,
//! so we ship `{ "continue": true }` plus optional `context` / message
//! fields for downstream builds that honour them.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::synth;
use super::types::{HookEvent, HookResult};
use super::{PayloadAdapter, PlatformAdapter};

/// Zero-sized marker — no adapter state.
pub struct WindsurfAdapter;

/// Typed view of Windsurf's stdin envelope. All fields optional; we
/// reject only when `agent_action_name` is absent (malformed payload).
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub(crate) struct WindsurfHookPayload {
    #[serde(default)]
    agent_action_name: Option<String>,
    #[serde(default)]
    trajectory_id: Option<String>,
    #[serde(default)]
    execution_id: Option<String>,
    #[serde(default)]
    tool_info: Option<Value>,
}

impl WindsurfHookPayload {
    fn into_canonical(self) -> Result<HookEvent, String> {
        let action = self
            .agent_action_name
            .as_deref()
            .ok_or_else(|| "missing agent_action_name".to_owned())?;
        let info = self.tool_info.as_ref();
        match action {
            // SessionStart variants: Windsurf has historically used
            // both names. Either triggers our warmup path.
            "session_start" | "beforeAgentResponse" => Ok(HookEvent::SessionStart {
                cwd: extract_cwd(info),
                session_id: None,
            }),
            "pre_user_prompt" => Ok(HookEvent::UserPromptSubmit {
                prompt: info
                    .and_then(|v| v.get("user_prompt"))
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_owned(),
                session_id: None,
            }),
            "post_write_code" => {
                let new_text = info
                    .and_then(|v| v.get("new_code").or_else(|| v.get("content")))
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let old_text = info
                    .and_then(|v| v.get("old_code"))
                    .and_then(|v| v.as_str())
                    .map(String::from);
                Ok(HookEvent::PostToolUse {
                    tool_name: "Write".to_owned(),
                    file_path: info
                        .and_then(|v| v.get("file_path"))
                        .and_then(|v| v.as_str())
                        .map(String::from),
                    diff: synthesise_write_diff(info),
                    session_id: None,
                    new_text,
                    old_text,
                })
            }
            "post_run_command" => Ok(HookEvent::PostToolUse {
                tool_name: "Bash".to_owned(),
                file_path: None,
                diff: synthesise_command_diff(info),
                session_id: None,
                new_text: None,
                old_text: None,
            }),
            "post_mcp_tool_use" => Ok(HookEvent::PostToolUse {
                tool_name: info
                    .and_then(|v| v.get("mcp_tool_name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("mcp_tool")
                    .to_owned(),
                file_path: None,
                diff: synthesise_mcp_diff(info),
                session_id: None,
                new_text: None,
                old_text: None,
            }),
            "post_cascade_response" => Ok(HookEvent::Stop {
                session_id: None,
                transcript_path: None,
                cwd: None,
            }),
            other => Err(format!("unsupported Windsurf hook action: {other}")),
        }
    }
}

fn extract_cwd(info: Option<&Value>) -> String {
    info.and_then(|v| v.get("cwd"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_owned()
}

/// Synthesise a diff from `post_write_code`'s edits array. Falls
/// back to `content` for whole-file writes if edits is missing.
fn synthesise_write_diff(info: Option<&Value>) -> Option<String> {
    let info = info?;
    if let Some(edits) = info.get("edits").and_then(|v| v.as_array()) {
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
    if let Some(content) = info.get("content").and_then(|v| v.as_str()) {
        return Some(synth::diff_content(content));
    }
    None
}

/// Diff-like summary for `post_run_command`. Windsurf ships the
/// command under `command_line` and no output, so this is just `$ cmd`.
fn synthesise_command_diff(info: Option<&Value>) -> Option<String> {
    let cmd = info?.get("command_line").and_then(|v| v.as_str())?;
    synth::diff_shell(Some(cmd), None)
}

/// Summary for `post_mcp_tool_use`: we flatten the tool arguments and
/// the result into a text blob so the retriever can match on keywords
/// mentioned inside MCP tool I/O.
fn synthesise_mcp_diff(info: Option<&Value>) -> Option<String> {
    let info = info?;
    let mut out = String::new();
    if let Some(args) = info.get("mcp_tool_arguments") {
        out.push_str("+ mcp_tool_arguments: ");
        out.push_str(&args.to_string());
        out.push('\n');
    }
    if let Some(res) = info.get("mcp_result") {
        out.push_str("+ mcp_result: ");
        out.push_str(&res.to_string());
        out.push('\n');
    }
    if out.is_empty() { None } else { Some(out) }
}

impl PayloadAdapter for WindsurfAdapter {
    type Raw = WindsurfHookPayload;
    const PARSE_LABEL: &'static str = "Windsurf";

    fn into_canonical(raw: Self::Raw) -> Result<HookEvent, String> {
        raw.into_canonical()
    }
}

impl PlatformAdapter for WindsurfAdapter {
    fn name(&self) -> &'static str {
        "windsurf"
    }

    fn parse_stdin(&self, raw: &str) -> Result<HookEvent, String> {
        Self::parse_stdin_default(raw)
    }

    fn format_output(&self, result: HookResult) -> String {
        // Advisory hooks in Windsurf ignore extra keys; we keep the
        // body minimal and include `continue` so future Windsurf
        // builds that treat absence as "block" still pass through.
        let mut obj = json!({ "continue": result.continue_ });
        if let Some(ctx) = result.additional_context {
            // Matches the key surface we use for Cursor — if a future
            // Windsurf build picks up `context`, we're already emitting it.
            obj["context"] = Value::String(ctx);
        }
        if let Some(msg) = result.system_message {
            obj["systemMessage"] = Value::String(msg);
        }
        crate::commands::util::json_compact_or(&obj, "{\"continue\":true}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_before_agent_response_also_maps_to_session_start() {
        let adapter = WindsurfAdapter;
        let raw = r#"{"agent_action_name":"beforeAgentResponse","tool_info":{}}"#;
        if let HookEvent::SessionStart { .. } = adapter.parse_stdin(raw).unwrap() {
            // pass — cwd may be empty string, that's fine
        } else {
            panic!("expected SessionStart");
        }
    }

    #[test]
    fn parse_pre_user_prompt_extracts_prompt() {
        let adapter = WindsurfAdapter;
        let raw =
            r#"{"agent_action_name":"pre_user_prompt","tool_info":{"user_prompt":"hi there"}}"#;
        assert_eq!(
            adapter.parse_stdin(raw).unwrap(),
            HookEvent::UserPromptSubmit {
                prompt: "hi there".into(),
                session_id: None,
            }
        );
    }

    #[test]
    fn parse_post_write_code_collects_edits_into_diff() {
        let adapter = WindsurfAdapter;
        let raw = r#"{
            "agent_action_name": "post_write_code",
            "tool_info": {
                "file_path": "src/a.ts",
                "edits": [
                    { "old_string": "x", "new_string": "y" },
                    { "old_string": "1", "new_string": "2" }
                ]
            }
        }"#;
        if let HookEvent::PostToolUse {
            tool_name,
            file_path,
            diff,
            ..
        } = adapter.parse_stdin(raw).unwrap()
        {
            assert_eq!(tool_name, "Write");
            assert_eq!(file_path.as_deref(), Some("src/a.ts"));
            let d = diff.unwrap();
            assert!(d.contains("-x") && d.contains("+y"));
            assert!(d.contains("-1") && d.contains("+2"));
        } else {
            panic!("expected PostToolUse");
        }
    }

    #[test]
    fn parse_post_run_command_maps_to_bash() {
        let adapter = WindsurfAdapter;
        let raw = r#"{
            "agent_action_name": "post_run_command",
            "tool_info": { "command_line": "npm test", "cwd": "/w/p" }
        }"#;
        if let HookEvent::PostToolUse {
            tool_name,
            file_path,
            diff,
            ..
        } = adapter.parse_stdin(raw).unwrap()
        {
            assert_eq!(tool_name, "Bash");
            assert!(file_path.is_none());
            assert_eq!(diff.as_deref(), Some("$ npm test\n"));
        } else {
            panic!("expected PostToolUse");
        }
    }

    #[test]
    fn parse_post_mcp_tool_use_preserves_tool_name() {
        let adapter = WindsurfAdapter;
        let raw = r#"{
            "agent_action_name": "post_mcp_tool_use",
            "tool_info": {
                "mcp_server_name": "difflore",
                "mcp_tool_name": "search_rules",
                "mcp_tool_arguments": {"diff": "foo"},
                "mcp_result": {"rules": []}
            }
        }"#;
        if let HookEvent::PostToolUse {
            tool_name, diff, ..
        } = adapter.parse_stdin(raw).unwrap()
        {
            assert_eq!(tool_name, "search_rules");
            let d = diff.unwrap();
            assert!(d.contains("mcp_tool_arguments"));
            assert!(d.contains("mcp_result"));
        } else {
            panic!("expected PostToolUse");
        }
    }

    #[test]
    fn parse_unknown_action_errors() {
        let adapter = WindsurfAdapter;
        let err = adapter
            .parse_stdin(r#"{"agent_action_name":"post_future_thing","tool_info":{}}"#)
            .unwrap_err();
        assert!(err.contains("unsupported"), "got: {err}");
    }

    #[test]
    fn parse_missing_action_errors() {
        let adapter = WindsurfAdapter;
        let err = adapter.parse_stdin(r"{}").unwrap_err();
        assert!(err.contains("missing"), "got: {err}");
    }

    #[test]
    fn format_output_noop_emits_continue() {
        let adapter = WindsurfAdapter;
        let out = adapter.format_output(HookResult::noop());
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["continue"], true);
    }

    #[test]
    fn format_output_with_context_adds_context_field() {
        let adapter = WindsurfAdapter;
        let out = adapter.format_output(HookResult::with_context("rule"));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["context"], "rule");
    }
}
