//! Shared types for the `agent_exec` module.

use gate4agent::CliTool;

/// Which gate4agent-backed CLI to call.
///
/// Matches (case- and separator-insensitively) the `client_name`
/// strings the hook adapters use, so callers that know their IDE can
/// route gate work without a second mapping table.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum AgentKind {
    ClaudeCode,
    Codex,
    GeminiCli,
    OpenCode,
}

impl AgentKind {
    /// The [`ClientId`](crate::clients::ClientId) this agent kind executes for.
    #[must_use]
    pub const fn client_id(self) -> crate::clients::ClientId {
        use crate::clients::ClientId;
        match self {
            Self::ClaudeCode => ClientId::ClaudeCode,
            Self::Codex => ClientId::Codex,
            Self::GeminiCli => ClientId::GeminiCli,
            Self::OpenCode => ClientId::OpenCode,
        }
    }

    /// The gate runner for a client, if gate4agent supports its headless pipe
    /// transport. Exhaustive over [`ClientId`](crate::clients::ClientId) so
    /// adding a client forces a decision about its gate support.
    #[must_use]
    pub const fn for_client(id: crate::clients::ClientId) -> Option<Self> {
        use crate::clients::ClientId;
        match id {
            ClientId::ClaudeCode => Some(Self::ClaudeCode),
            ClientId::Codex => Some(Self::Codex),
            ClientId::GeminiCli => Some(Self::GeminiCli),
            ClientId::OpenCode => Some(Self::OpenCode),
            ClientId::Cursor
            | ClientId::Windsurf
            | ClientId::CopilotCli
            | ClientId::Antigravity
            | ClientId::Goose
            | ClientId::Crush
            | ClientId::RooCode
            | ClientId::Warp => None,
        }
    }

    /// gate4agent tool backing this gate runner.
    #[must_use]
    pub const fn cli_tool(self) -> CliTool {
        match self {
            Self::ClaudeCode => CliTool::ClaudeCode,
            Self::Codex => CliTool::Codex,
            Self::GeminiCli => CliTool::Gemini,
            Self::OpenCode => CliTool::OpenCode,
        }
    }

    /// Stable display label, matching the hook adapters' `name()` — the
    /// client's wire name from the single source of truth.
    #[must_use]
    pub const fn label(self) -> &'static str {
        self.client_id().wire_name()
    }

    /// Parse the hook dispatcher's alias set (via
    /// [`ClientId::from_wire`](crate::clients::ClientId::from_wire)). Returns
    /// `None` on unknown input so callers fall back to a configured default
    /// rather than silently routing to the wrong agent.
    #[must_use]
    pub fn from_client_name(name: &str) -> Option<Self> {
        Self::for_client(crate::clients::ClientId::from_wire(name)?)
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
    /// `true` if the binary was missing, spawn/exit failed, or timed out.
    /// Callers then skip the gate and ignore `stdout`.
    pub errored: bool,
    /// Short reason set when `errored == true`. Separate from `stderr`
    /// so we can attach our own context (e.g. "timeout after 30s").
    pub error_message: String,
}

impl GateResult {
    /// Error result with no stdout/stderr, for failures before or during spawn.
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
            ("cursor", None),
            ("gemini", Some(AgentKind::GeminiCli)),
            ("gemini-cli", Some(AgentKind::GeminiCli)),
            ("gemini_cli", Some(AgentKind::GeminiCli)),
            ("windsurf", None),
            ("Windsurf", None),
            ("definitely-not-an-agent", None),
            ("", None),
        ];
        for (input, want) in cases {
            assert_eq!(AgentKind::from_client_name(input), *want, "input {input:?}");
        }
    }

    #[test]
    fn label_round_trips_through_from_client_name() {
        // Guards against a label rename desyncing the parser.
        for kind in [
            AgentKind::ClaudeCode,
            AgentKind::Codex,
            AgentKind::GeminiCli,
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
    fn cli_tool_maps_to_gate4agent_variants() {
        assert_eq!(AgentKind::ClaudeCode.cli_tool(), CliTool::ClaudeCode);
        assert_eq!(AgentKind::Codex.cli_tool(), CliTool::Codex);
        assert_eq!(AgentKind::GeminiCli.cli_tool(), CliTool::Gemini);
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
