//! `CLAUDE.md` emitter: Claude Code's project instructions file. Gated on
//! `skills.enabled_for_claude` so the static file carries the same rule set
//! the claude engine receives from the hook/MCP path.

pub(crate) static CLAUDE_MD: super::Emitter = super::Emitter {
    format: "claude-md",
    file_name: "CLAUDE.md",
    engine: Some("claude"),
};
