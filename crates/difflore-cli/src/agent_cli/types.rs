//! Shared types for the `agent_cli` module.

/// Which agent CLI to shell out to.
///
/// Matches (case- and separator-insensitively) the `client_name`
/// strings the hook adapters use, so callers that know their IDE can
/// route gate work without a second mapping table.
///
/// `Windsurf` ships no headless CLI today; `dispatch_gate` returns an
/// errored `GateResult` for it rather than panicking. See `runner::run`
/// for the error text.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum AgentKind {
    ClaudeCode,
    Codex,
    Cursor,
    GeminiCli,
    Windsurf,
}

impl AgentKind {
    /// Stable display label, matching the hook adapters' `name()`.
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

    /// Parse the hook dispatcher's alias set. Returns `None` on unknown
    /// input so callers fall back to a configured default rather than
    /// silently routing to the wrong agent.
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
/// Not a `Result`: the caller wants stdout AND whether the CLI exited
/// non-zero, so it can downgrade an errored gate to a best-effort skip
/// without losing the partial output. Mirrors hivemind's
/// `gate-runner.ts` shape for a shared fallback flow.
#[derive(Clone, Debug, Default)]
pub struct GateResult {
    /// CLI stdout, trimmed by the runner. Empty string (never `None`)
    /// on total failure so JSON-parsing paths fail with a clear
    /// "expected JSON, got empty" error.
    pub stdout: String,
    /// CLI stderr, useful for surfacing the CLI's error text when
    /// `errored == true`.
    pub stderr: String,
    /// `true` if the binary was missing, spawn/exit failed, timed out,
    /// or the agent is Windsurf. Callers then skip the gate and ignore
    /// `stdout`.
    pub errored: bool,
    /// Short reason set when `errored == true`. Separate from `stderr`
    /// so we can attach our own context (e.g. "timeout after 30s").
    pub error_message: String,
}

impl GateResult {
    /// Error result with no stdout/stderr, for failures before spawning
    /// the CLI (binary not found, unsupported agent).
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
        // Guards against a label rename desyncing the parser.
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
