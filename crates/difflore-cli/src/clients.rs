//! `ClientId`: the compile-time single source of truth for every AI client
//! DiffLore knows about.
//!
//! Three subsystems used to keep their own `'claude-code'`-style string
//! tables — the installer registry (`installer/registry.rs`), the hook
//! adapter dispatch (`hook/adapters/mod.rs`), and the agent-CLI gate runner
//! (`agent_exec/types.rs`). Each now matches exhaustively over this enum, so
//! adding a client is: add a variant here, follow the compile errors. A new
//! client can no longer be wired into one table and silently missed in the
//! others.

/// Every AI client DiffLore integrates with, across all surfaces (MCP entry,
/// lifecycle hooks, headless gate CLI).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ClientId {
    ClaudeCode,
    Codex,
    Cursor,
    GeminiCli,
    Windsurf,
    CopilotCli,
    Antigravity,
    Goose,
    Crush,
    RooCode,
    Warp,
}

impl ClientId {
    /// Every known client, in installer display order (Claude Code first).
    pub const ALL: [Self; 11] = [
        Self::ClaudeCode,
        Self::Codex,
        Self::Cursor,
        Self::GeminiCli,
        Self::Windsurf,
        Self::CopilotCli,
        Self::Antigravity,
        Self::Goose,
        Self::Crush,
        Self::RooCode,
        Self::Warp,
    ];

    /// Stable machine identifier: hook `--client` argument, telemetry client
    /// label, and the canonical alias every other spelling normalises to.
    #[must_use]
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
            Self::Cursor => "cursor",
            Self::GeminiCli => "gemini-cli",
            Self::Windsurf => "windsurf",
            Self::CopilotCli => "copilot-cli",
            Self::Antigravity => "antigravity",
            Self::Goose => "goose",
            Self::Crush => "crush",
            Self::RooCode => "roo-code",
            Self::Warp => "warp",
        }
    }

    /// Human display name, as shown by `difflore agents status` and used for
    /// the installer's surface→client roll-up.
    #[must_use]
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::ClaudeCode => "Claude Code",
            Self::Codex => "Codex",
            Self::Cursor => "Cursor",
            Self::GeminiCli => "Gemini CLI",
            Self::Windsurf => "Windsurf",
            Self::CopilotCli => "Copilot CLI",
            Self::Antigravity => "Antigravity",
            Self::Goose => "Goose",
            Self::Crush => "Crush",
            Self::RooCode => "Roo Code",
            Self::Warp => "Warp",
        }
    }

    /// Parse any accepted spelling of a client name (wire names plus the
    /// alias sets the hook configs and gate callers historically used),
    /// case- and separator-insensitively. Returns `None` for unknown names so
    /// callers choose their own fallback policy (the hook adapter dispatch
    /// falls back to Claude Code; the gate runner skips).
    #[must_use]
    pub fn from_wire(name: &str) -> Option<Self> {
        let normalized = name.to_ascii_lowercase();
        Some(match normalized.as_str() {
            "claude" | "claude-code" | "claude_code" | "claude-cli" => Self::ClaudeCode,
            "codex" | "codex-cli" => Self::Codex,
            "cursor" | "cursor-agent" => Self::Cursor,
            "gemini" | "gemini-cli" | "gemini_cli" => Self::GeminiCli,
            "windsurf" => Self::Windsurf,
            "copilot" | "copilot-cli" => Self::CopilotCli,
            "antigravity" => Self::Antigravity,
            "goose" => Self::Goose,
            "crush" => Self::Crush,
            "roo" | "roo-code" | "roo_code" => Self::RooCode,
            "warp" => Self::Warp,
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_names_round_trip_through_from_wire() {
        // A wire-name rename that forgets the parser would silently unroute
        // that client everywhere; pin the round-trip for every variant.
        for id in ClientId::ALL {
            assert_eq!(
                ClientId::from_wire(id.wire_name()),
                Some(id),
                "wire name {} did not round-trip",
                id.wire_name()
            );
        }
    }

    #[test]
    fn wire_names_are_unique_and_kebab_case() {
        let mut seen = std::collections::BTreeSet::new();
        for id in ClientId::ALL {
            assert!(seen.insert(id.wire_name()), "duplicate {}", id.wire_name());
            assert!(
                id.wire_name()
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c == '-'),
                "{} is not kebab-case",
                id.wire_name()
            );
        }
    }

    #[test]
    fn from_wire_accepts_legacy_alias_spellings() {
        // These exact aliases appear in user hook configs and gate callers in
        // the wild; dropping one would break installed configurations.
        let cases: &[(&str, ClientId)] = &[
            ("claude", ClientId::ClaudeCode),
            ("claude_code", ClientId::ClaudeCode),
            ("CLAUDE-CODE", ClientId::ClaudeCode),
            ("codex-cli", ClientId::Codex),
            ("cursor-agent", ClientId::Cursor),
            ("gemini", ClientId::GeminiCli),
            ("gemini_cli", ClientId::GeminiCli),
            ("Windsurf", ClientId::Windsurf),
        ];
        for (input, want) in cases {
            assert_eq!(ClientId::from_wire(input), Some(*want), "alias {input}");
        }
        assert_eq!(ClientId::from_wire("definitely-not-a-client"), None);
        assert_eq!(ClientId::from_wire(""), None);
    }
}
