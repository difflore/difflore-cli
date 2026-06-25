//! `AGENTS.md` emitter: the cross-agent context-file convention read by
//! Codex, Cursor, Amp, Jules, and others. No per-engine filter — any agent
//! may read this file, so every active in-scope rule participates.

pub(crate) static AGENTS_MD: super::Emitter = super::Emitter {
    format: "agents-md",
    file_name: "AGENTS.md",
    engine: None,
};
