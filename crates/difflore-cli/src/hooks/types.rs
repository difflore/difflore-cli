//! Platform-agnostic hook event + result types.
//!
//! Different AI coding assistants (Claude Code, Cursor, Zed, …) invoke
//! lifecycle hooks with platform-specific JSON shapes on stdin and expect
//! platform-specific JSON on stdout. To avoid littering `main.rs` with
//! per-client branches, we normalise both directions through `HookEvent`
//! (our canonical input model) and `HookResult` (our canonical output
//! model). Each platform ships a tiny adapter that translates in and out.
//!
//! Keep this file purely *data* — no parsing, no formatting, no I/O. That
//! keeps the adapters trivial to unit-test and avoids accidental platform
//! coupling leaking into the shared layer.

use serde::{Deserialize, Serialize};

/// Severity bucket an adapter assigns to a hook-path failure so the CLI
/// can decide whether to block the user's AI session. See
/// `PlatformAdapter::classify_error` and the `Hook::Run` dispatch site
/// in `main.rs` for the exit-code mapping (Transport→0, Client/Fatal→2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// Retryable infrastructure failure we deliberately swallow so a
    /// flaky network never blocks the assistant.
    Transport,
    /// Programmer-facing bug (bad request shape, parse failure, 4xx).
    /// Surface it so it gets fixed.
    Client,
    /// Anything we can't classify. Treat conservatively as blocking.
    Fatal,
}

/// Canonical hook event. Each assistant's adapter maps its native event
/// payload into exactly one of these variants. When a platform fires an
/// event we don't yet model, the adapter returns `Err(...)` from
/// `parse_stdin` and the CLI no-ops gracefully — hooks must NEVER block
/// the user's workflow, even in the face of an unknown event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HookEvent {
    /// Assistant just finished a tool call that may have mutated code on
    /// disk (Edit, Write, …). The CLI uses this to proactively surface
    /// relevant team rules without waiting for the next user prompt.
    PostToolUse {
        tool_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        file_path: Option<String>,
        /// Best-effort diff synthesis from the assistant's tool response.
        /// `None` when we couldn't reconstruct one — downstream logic
        /// must handle that case (typically by falling back to a file-
        /// level rule query).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        diff: Option<String>,
        /// Platform-provided session identifier, propagated end-to-end
        /// so `observation::classify` can tag each enqueued observation
        /// with the Claude Code session that produced it. `None` when
        /// the adapter didn't receive a session id — the classifier
        /// falls back to an empty string for cloud-side clustering.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        /// Raw new text the assistant wrote for this edit (`new_string` /
        /// content). Captured alongside the diff so the classifier can
        /// detect new-file writes vs. comment-removal patterns without
        /// re-parsing the adapter-synthesised diff. `None` when the
        /// adapter couldn't identify a "new text" payload.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        new_text: Option<String>,
        /// Raw pre-edit text (`old_string`) for Edit / `MultiEdit`. `None`
        /// for Write (no prior content) or when the adapter couldn't
        /// extract one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        old_text: Option<String>,
    },
    /// Assistant is about to read a file. The dispatcher returns noop for
    /// this event (rule injection on Read had near-zero hit rate). The
    /// variant is kept so the adapter can still parse PreToolUse:Read
    /// payloads without erroring.
    PreToolUseRead {
        file_path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
    },
    /// Assistant started a new session. `cwd` lets the CLI scope any
    /// repo-sensitive logic to the right project without requiring the
    /// adapter to mirror every repo-detection heuristic we have.
    SessionStart {
        cwd: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
    },
    /// User sent a prompt to the assistant. Currently a noop — the
    /// dispatcher accepts and discards. Kept so adapters can parse the
    /// platform event without erroring; reserved for future use.
    UserPromptSubmit {
        prompt: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
    },
    /// Assistant finished its turn (tool loop drained, response shipped).
    Stop {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        /// Optional absolute path to the platform-native transcript JSONL.
        /// Reserved for the stated-vs-actual validator (see
        /// `difflore_core::stated_vs_actual`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        transcript_path: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
    },
    /// Session ended (user closed the client, started a new session, …).
    /// Used by the stated-vs-actual validator when both `transcript_path`
    /// and `cwd` are present.
    SessionEnd {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        transcript_path: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
    },
}

/// Canonical hook result. Adapters translate this into the platform's
/// expected stdout JSON. We deliberately keep the surface small — hooks
/// that want to do more than "show a message + inject extra context"
/// should extend this enum rather than smuggling behaviour through
/// free-form fields.
///
/// `continue_` (note the trailing underscore — `continue` is a reserved
/// keyword) controls whether the assistant should keep going after the
/// hook returns. For `DiffLore`'s MCP-style advisory hooks we always set
/// this to `true`; setting it to `false` is reserved for future "hard
/// stop" flows (e.g. enforcement blocks).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct HookResult {
    /// When false, ask the client to abort its current action. `DiffLore`
    /// never does this today — kept for future enforcement paths.
    pub continue_: bool,
    /// Short user-visible message. Rendered by the client in its own
    /// status area (Claude Code shows it as a system message).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_message: Option<String>,
    /// Free-form context to surface to the agent. Claude Code pipes this
    /// back into the conversation under a `hookSpecificOutput` block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub additional_context: Option<String>,
    /// The platform-native name of the hook event this result is
    /// answering — `"PreToolUse"`, `"PostToolUse"`, `"UserPromptSubmit"`,
    /// etc. Set by the dispatcher so `format_output` can echo the
    /// correct event name back to clients (Claude Code rejects the
    /// entire injection if `hookSpecificOutput.hookEventName` doesn't
    /// match the event that fired it). Other adapters that don't need
    /// the name simply ignore this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_name: Option<String>,
    /// How many rules ended up in `additional_context`, for the hook-fires
    /// log. Local audit field, skipped on the wire — never sent to the agent.
    #[serde(default, skip)]
    pub rules_injected: Option<usize>,
}

impl HookResult {
    /// Non-blocking pass-through with no output. The safe default when a
    /// hook event isn't actionable (e.g. a `PostToolUse` for `Read`).
    pub(crate) const fn noop() -> Self {
        Self {
            continue_: true,
            system_message: None,
            additional_context: None,
            event_name: None,
            rules_injected: None,
        }
    }

    /// Convenience constructor for the common "we have context to
    /// inject" path. Sets `continue_=true` and fills `additional_context`.
    pub(crate) fn with_context(ctx: impl Into<String>) -> Self {
        Self {
            continue_: true,
            system_message: None,
            additional_context: Some(ctx.into()),
            event_name: None,
            rules_injected: None,
        }
    }
}

impl HookEvent {
    /// Platform-native event name as the wire format spells it. Used by
    /// the dispatcher to thread the originating event identity through
    /// to `format_output` so adapters can echo it back when the client
    /// requires the response to be self-identifying (Claude Code does).
    pub(crate) const fn wire_name(&self) -> &'static str {
        match self {
            Self::PreToolUseRead { .. } => "PreToolUse",
            Self::PostToolUse { .. } => "PostToolUse",
            Self::SessionStart { .. } => "SessionStart",
            Self::UserPromptSubmit { .. } => "UserPromptSubmit",
            Self::Stop { .. } => "Stop",
            Self::SessionEnd { .. } => "SessionEnd",
        }
    }

    /// File path the agent is about to read or just edited. None for
    /// non-file events (`UserPromptSubmit`, `SessionStart`, …). Lets the
    /// dispatcher stamp the fire-log entry with the file in scope so
    /// post-mortem audits can correlate which `file_patterns` are
    /// firing rules.
    pub(crate) fn target_file_path(&self) -> Option<String> {
        match self {
            Self::PreToolUseRead { file_path, .. } => Some(file_path.clone()),
            Self::PostToolUse { file_path, .. } => file_path.clone(),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn post_tool_use_omits_absent_optional_fields() {
        // When a Write event has no diff synthesis, the JSON must not
        // carry a `"diff": null` — that would leak implementation detail
        // to the client and trip strict parsers.
        let event = HookEvent::PostToolUse {
            tool_name: "Write".into(),
            file_path: Some("README.md".into()),
            diff: None,
            session_id: None,
            new_text: None,
            old_text: None,
        };
        let s = serde_json::to_string(&event).unwrap();
        assert!(!s.contains("diff"), "expected diff omitted, got: {s}");
    }

    #[test]
    fn hook_result_noop_has_continue_true() {
        // Regression guard: the noop constructor must produce a
        // pass-through. Any change to this default would silently turn
        // every un-actionable hook into a session-blocker.
        let r = HookResult::noop();
        assert!(r.continue_);
        assert!(r.system_message.is_none());
        assert!(r.additional_context.is_none());
    }
}
