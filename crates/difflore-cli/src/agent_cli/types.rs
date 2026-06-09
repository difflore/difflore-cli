//! Shared types for the `agent_cli` module.
//!
//! Why split: keeps `mod.rs` focused on the public `dispatch_gate` entry
//! point while `runner.rs` / `binary_finder.rs` own behaviour. Tests in
//! each sub-module can import these without pulling in the runner.

/// Which agent CLI to shell out to.
///
/// Matches (case-insensitively, separator-insensitively) the
/// `client_name` strings used by the hook adapters in
/// `crate::hooks::get_platform_adapter` so callers that already know
/// which IDE they're in can route gate work to the matching CLI without
/// a second mapping table.
///
/// `Windsurf` is included for symmetry with the hook adapters even
/// though Windsurf ships no headless CLI today — `dispatch_gate` will
/// return an errored `GateResult` for it rather than panicking, so the
/// caller can decide whether to fall back to a different agent or BYOK
/// provider. See `runner::run` for the exact error text.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum AgentKind {
    ClaudeCode,
    Codex,
    Cursor,
    GeminiCli,
    Windsurf,
}

impl AgentKind {
    /// Stable display label (matches the hook adapters' `name()`).
    /// Kept `&'static` so error messages can avoid an allocation.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
            Self::Cursor => "cursor",
            Self::GeminiCli => "gemini-cli",
            Self::Windsurf => "windsurf",
        }
    }

    /// Parse the same alias set the hook dispatcher accepts. Returns
    /// `None` on unknown input so callers can fall back to a configured
    /// default rather than silently routing to a wrong agent (which is
    /// what `get_platform_adapter` does — appropriate for hooks where
    /// blocking the assistant is worse than a wrong parse, but not for
    /// LLM dispatch where the wrong CLI just wastes the user's time).
    #[must_use]
    pub fn from_client_name(name: &str) -> Option<Self> {
        let normalized = name.to_ascii_lowercase();
        Some(match normalized.as_str() {
            "claude" | "claude-code" | "claude_code" | "claude-cli" => Self::ClaudeCode,
            "codex" | "codex-cli" => Self::Codex,
            "cursor" | "cursor-agent" => Self::Cursor,
            "gemini" | "gemini-cli" | "gemini_cli" => Self::GeminiCli,
            "windsurf" => Self::Windsurf,
            _ => return None,
        })
    }
}

/// Outcome of a single `dispatch_gate` call.
///
/// We deliberately do NOT return `Result<String, anyhow::Error>` here:
/// the caller almost always wants stdout AND knowledge of whether the
/// CLI exited non-zero (so it can downgrade an "errored gate" to a
/// best-effort skip without losing the partial output the CLI may have
/// printed before failing). Mirrors hivemind's `gate-runner.ts` return
/// shape so prompts shared across that codebase and DiffLore can use
/// the same fallback flow.
#[derive(Clone, Debug, Default)]
pub struct GateResult {
    /// Whatever the CLI wrote to stdout. Trimmed by the runner so
    /// callers don't need to re-trim. Never `None` — empty string on
    /// total failure so JSON-parsing code paths can fail with a
    /// "expected JSON, got empty" error rather than a null deref.
    pub stdout: String,
    /// Whatever the CLI wrote to stderr. Useful for surfacing the
    /// CLI's own error text up to the user when `errored == true`.
    pub stderr: String,
    /// `true` if: binary not found, spawn failed, exit code non-zero,
    /// timed out, or agent is Windsurf (no headless CLI). Callers
    /// treat this as "skip the gate, don't trust `stdout`".
    pub errored: bool,
    /// Short human-readable reason populated when `errored == true`.
    /// Empty otherwise. Kept separate from `stderr` so we can attach
    /// our own context (e.g. "timeout after 30s") that the CLI itself
    /// never wrote.
    pub error_message: String,
}

impl GateResult {
    /// Construct a quick error result with no stdout/stderr. Used
    /// when we fail before even spawning the CLI (binary not found,
    /// unsupported agent, etc).
    pub(super) fn errored_with(message: impl Into<String>) -> Self {
        Self {
            stdout: String::new(),
            stderr: String::new(),
            errored: true,
            error_message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_client_name_accepts_alias_variants() {
        let cases: &[(&str, Option<AgentKind>)] = &[
            ("claude", Some(AgentKind::ClaudeCode)),
            ("claude-code", Some(AgentKind::ClaudeCode)),
            ("claude_code", Some(AgentKind::ClaudeCode)),
            ("CLAUDE-CODE", Some(AgentKind::ClaudeCode)),
            ("codex", Some(AgentKind::Codex)),
            ("codex-cli", Some(AgentKind::Codex)),
            ("cursor", Some(AgentKind::Cursor)),
            ("cursor-agent", Some(AgentKind::Cursor)),
            ("gemini", Some(AgentKind::GeminiCli)),
            ("gemini-cli", Some(AgentKind::GeminiCli)),
            ("gemini_cli", Some(AgentKind::GeminiCli)),
            ("windsurf", Some(AgentKind::Windsurf)),
            ("Windsurf", Some(AgentKind::Windsurf)),
            ("definitely-not-an-agent", None),
            ("", None),
        ];
        for (input, want) in cases {
            assert_eq!(
                AgentKind::from_client_name(input),
                *want,
                "input {input:?}"
            );
        }
    }

    #[test]
    fn label_round_trips_through_from_client_name() {
        // Every kind's label must parse back to itself. Guards against
        // a future label rename quietly desyncing the parser.
        for kind in [
            AgentKind::ClaudeCode,
            AgentKind::Codex,
            AgentKind::Cursor,
            AgentKind::GeminiCli,
            AgentKind::Windsurf,
        ] {
            assert_eq!(
                AgentKind::from_client_name(kind.label()),
                Some(kind),
                "label {} did not round-trip",
                kind.label()
            );
        }
    }

    #[test]
    fn errored_with_populates_only_error_fields() {
        let result = GateResult::errored_with("nope");
        assert!(result.errored);
        assert_eq!(result.error_message, "nope");
        assert!(result.stdout.is_empty());
        assert!(result.stderr.is_empty());
    }
}
