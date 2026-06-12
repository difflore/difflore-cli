//! Codex hook adapter.
//!
//! Codex invokes lifecycle hooks with a JSON object on stdin. The shape is
//! close to Claude Code's hook payloads, but file edits are reported as
//! `tool_name: "apply_patch"` with the patch text under
//! `tool_input.command`. DiffLore's core hook logic already understands
//! Claude-style edit tool names (`Edit`, `Write`, `MultiEdit`), so this adapter
//! translates Codex's `apply_patch` tool into that canonical edit vocabulary.

use serde::Deserialize;
use serde_json::{Value, json};

use super::synth;
use super::types::{HookEvent, HookResult};
use super::{PayloadAdapter, PlatformAdapter};

pub struct CodexAdapter;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) struct CodexHookPayload {
    #[serde(default)]
    hook_event_name: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    transcript_path: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    tool_input: Option<Value>,
    #[serde(default)]
    tool_response: Option<Value>,
    #[serde(default)]
    prompt: Option<String>,
}

impl CodexHookPayload {
    fn into_canonical(self) -> Result<HookEvent, String> {
        let event_name = self
            .hook_event_name
            .as_deref()
            .ok_or_else(|| "missing hook_event_name".to_owned())?;
        match event_name {
            "PostToolUse" => Ok(self.into_post_tool_use()),
            "PreToolUse" => self.into_pre_tool_use(),
            "SessionStart" => Ok(HookEvent::SessionStart {
                cwd: self.cwd.unwrap_or_default(),
                session_id: self.session_id,
            }),
            "UserPromptSubmit" => Ok(HookEvent::UserPromptSubmit {
                prompt: self.prompt.unwrap_or_default(),
                session_id: self.session_id,
                transcript_path: self.transcript_path,
                cwd: self.cwd,
            }),
            "Stop" => Ok(HookEvent::Stop {
                session_id: self.session_id,
                transcript_path: self.transcript_path,
                cwd: self.cwd,
            }),
            // Codex does not currently document SessionEnd, but accepting it is
            // harmless if a future build sends it.
            "SessionEnd" => Ok(HookEvent::SessionEnd {
                session_id: self.session_id,
                transcript_path: self.transcript_path,
                cwd: self.cwd,
            }),
            other => Err(format!("unsupported Codex hook event: {other}")),
        }
    }

    fn into_pre_tool_use(self) -> Result<HookEvent, String> {
        let tool_name = self.tool_name.as_deref().unwrap_or_default();
        if tool_name != "Read" && !tool_name.ends_with("__read") {
            return Err(format!(
                "PreToolUse for `{tool_name}` not wired - Read only",
            ));
        }
        let file_path = self
            .tool_input
            .as_ref()
            .and_then(extract_file_path_from_json)
            .ok_or_else(|| format!("PreToolUse:{tool_name} missing readable file path"))?;
        Ok(HookEvent::PreToolUseRead {
            file_path,
            session_id: self.session_id,
        })
    }

    fn into_post_tool_use(self) -> HookEvent {
        let tool_name = self.tool_name.clone().unwrap_or_default();
        if tool_name == "Bash" {
            let command = command_from_input(self.tool_input.as_ref());
            return HookEvent::PostToolUse {
                tool_name: "Bash".to_owned(),
                file_path: None,
                diff: synth::diff_shell(
                    command.as_deref(),
                    shell_output_text(self.tool_response.as_ref()).as_deref(),
                ),
                session_id: self.session_id,
                new_text: None,
                old_text: None,
            };
        }

        if tool_name == "apply_patch" {
            let command = command_from_input(self.tool_input.as_ref()).unwrap_or_default();
            let file_path = first_patch_file_path(&command);
            let (old_text, new_text) = patch_edit_strings(&command);
            return HookEvent::PostToolUse {
                tool_name: codex_patch_tool_name(&command).to_owned(),
                file_path,
                diff: (!command.trim().is_empty()).then(|| command.trim().to_owned()),
                session_id: self.session_id,
                new_text,
                old_text,
            };
        }

        HookEvent::PostToolUse {
            tool_name,
            file_path: self
                .tool_input
                .as_ref()
                .and_then(extract_file_path_from_json),
            diff: None,
            session_id: self.session_id,
            new_text: None,
            old_text: None,
        }
    }
}

fn command_from_input(input: Option<&Value>) -> Option<String> {
    input?
        .get("command")
        .and_then(|v| v.as_str())
        .map(String::from)
}

fn shell_output_text(value: Option<&Value>) -> Option<String> {
    let value = value?;
    if let Some(text) = value.as_str() {
        return Some(text.to_owned());
    }
    for key in ["output", "stdout", "stderr", "content"] {
        if let Some(text) = value.get(key).and_then(|v| v.as_str()) {
            return Some(text.to_owned());
        }
    }
    None
}

fn extract_file_path_from_json(value: &Value) -> Option<String> {
    for key in ["file_path", "path", "absolute_path"] {
        if let Some(path) = value.get(key).and_then(|v| v.as_str()) {
            return Some(path.to_owned());
        }
    }
    None
}

fn first_patch_file_path(command: &str) -> Option<String> {
    for line in command.lines() {
        for prefix in ["*** Update File: ", "*** Add File: ", "*** Delete File: "] {
            if let Some(path) = line.strip_prefix(prefix) {
                let path = path.trim();
                if !path.is_empty() {
                    return Some(path.to_owned());
                }
            }
        }
    }
    None
}

fn codex_patch_tool_name(command: &str) -> &'static str {
    let file_ops = command
        .lines()
        .filter(|line| {
            line.starts_with("*** Update File: ")
                || line.starts_with("*** Add File: ")
                || line.starts_with("*** Delete File: ")
        })
        .count();
    if file_ops > 1 {
        "MultiEdit"
    } else if command
        .lines()
        .any(|line| line.starts_with("*** Add File: "))
    {
        "Write"
    } else {
        "Edit"
    }
}

fn patch_edit_strings(command: &str) -> (Option<String>, Option<String>) {
    let mut old_acc = String::new();
    let mut new_acc = String::new();
    for line in command.lines() {
        if line.starts_with("***")
            || line.starts_with("@@")
            || line.starts_with("---")
            || line.starts_with("+++")
        {
            continue;
        }
        if let Some(removed) = line.strip_prefix('-') {
            if !old_acc.is_empty() {
                old_acc.push('\n');
            }
            old_acc.push_str(removed);
        } else if let Some(added) = line.strip_prefix('+') {
            if !new_acc.is_empty() {
                new_acc.push('\n');
            }
            new_acc.push_str(added);
        }
    }
    (
        (!old_acc.is_empty()).then_some(old_acc),
        (!new_acc.is_empty()).then_some(new_acc),
    )
}

impl PayloadAdapter for CodexAdapter {
    type Raw = CodexHookPayload;
    const PARSE_LABEL: &'static str = "Codex";

    fn into_canonical(raw: Self::Raw) -> Result<HookEvent, String> {
        raw.into_canonical()
    }
}

impl PlatformAdapter for CodexAdapter {
    fn name(&self) -> &'static str {
        "codex"
    }

    fn parse_stdin(&self, raw: &str) -> Result<HookEvent, String> {
        Self::parse_stdin_default(raw)
    }

    fn format_output(&self, result: HookResult) -> String {
        let mut obj = json!({
            "continue": result.continue_,
        });
        if let Some(msg) = result.system_message {
            obj["systemMessage"] = Value::String(msg);
        }
        if let Some(ctx) = result.additional_context {
            let event_name = result.event_name.as_deref().unwrap_or("PostToolUse");
            obj["hookSpecificOutput"] = json!({
                "hookEventName": event_name,
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
    fn parse_post_tool_use_apply_patch_as_edit() {
        let adapter = CodexAdapter;
        let raw = r#"{
            "hook_event_name": "PostToolUse",
            "session_id": "sess",
            "cwd": "/tmp/proj",
            "tool_name": "apply_patch",
            "tool_input": {
                "command": "*** Begin Patch\n*** Update File: src/foo.rs\n@@\n-let x = 1;\n+let x = 2;\n*** End Patch"
            },
            "tool_response": {"stdout": "Done"}
        }"#;

        let event = adapter.parse_stdin(raw).expect("parse ok");
        match event {
            HookEvent::PostToolUse {
                tool_name,
                file_path,
                diff,
                old_text,
                new_text,
                session_id,
            } => {
                assert_eq!(tool_name, "Edit");
                assert_eq!(file_path.as_deref(), Some("src/foo.rs"));
                assert_eq!(session_id.as_deref(), Some("sess"));
                let diff = diff.expect("patch command becomes diff");
                assert!(diff.contains("*** Update File: src/foo.rs"));
                assert_eq!(old_text.as_deref(), Some("let x = 1;"));
                assert_eq!(new_text.as_deref(), Some("let x = 2;"));
            }
            other => panic!("expected PostToolUse, got {other:?}"),
        }
    }

    #[test]
    fn parse_add_file_patch_as_write() {
        let adapter = CodexAdapter;
        let raw = r#"{
            "hook_event_name": "PostToolUse",
            "tool_name": "apply_patch",
            "tool_input": {
                "command": "*** Begin Patch\n*** Add File: README.md\n+hello\n*** End Patch"
            }
        }"#;

        let event = adapter.parse_stdin(raw).expect("parse ok");
        if let HookEvent::PostToolUse {
            tool_name,
            file_path,
            new_text,
            ..
        } = event
        {
            assert_eq!(tool_name, "Write");
            assert_eq!(file_path.as_deref(), Some("README.md"));
            assert_eq!(new_text.as_deref(), Some("hello"));
        } else {
            panic!("expected PostToolUse");
        }
    }

    #[test]
    fn parse_bash_synthesises_shell_diff() {
        let adapter = CodexAdapter;
        let raw = r#"{
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": "cargo test"},
            "tool_response": {"stdout": "failed\n"}
        }"#;

        let event = adapter.parse_stdin(raw).expect("parse ok");
        if let HookEvent::PostToolUse {
            tool_name, diff, ..
        } = event
        {
            assert_eq!(tool_name, "Bash");
            let diff = diff.expect("shell diff");
            assert!(diff.contains("$ cargo test"));
            assert!(diff.contains("+failed"));
        } else {
            panic!("expected PostToolUse");
        }
    }

    #[test]
    fn parse_user_prompt_submit_preserves_codex_common_fields() {
        let adapter = CodexAdapter;
        let raw = r#"{
            "hook_event_name": "UserPromptSubmit",
            "session_id": "sess",
            "transcript_path": "/tmp/transcript.jsonl",
            "cwd": "/tmp/proj",
            "prompt": "fix this"
        }"#;

        let event = adapter.parse_stdin(raw).expect("parse ok");
        assert_eq!(
            event,
            HookEvent::UserPromptSubmit {
                prompt: "fix this".into(),
                session_id: Some("sess".into()),
                transcript_path: Some("/tmp/transcript.jsonl".into()),
                cwd: Some("/tmp/proj".into()),
            }
        );
    }

    #[test]
    fn format_output_nests_additional_context_for_codex() {
        let adapter = CodexAdapter;
        let mut result = HookResult::with_context("Memory 1: keep routes explicit");
        result.event_name = Some("UserPromptSubmit".into());

        let out = adapter.format_output(result);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["continue"], true);
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "UserPromptSubmit");
        assert_eq!(
            v["hookSpecificOutput"]["additionalContext"],
            "Memory 1: keep routes explicit"
        );
    }
}
