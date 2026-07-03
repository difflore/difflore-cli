//! `.cursorrules` emitter: Cursor's repo-root rules file. Only rules enabled
//! for the cursor engine participate.

pub(crate) static CURSOR_RULES: super::Emitter = super::Emitter {
    format: "cursor-md",
    file_name: ".cursorrules",
    engine: Some("cursor"),
    kind: super::EmitterKind::MarkerBlock,
};
