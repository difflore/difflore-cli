//! Export emitters: one per static context-file convention. v1 ships
//! `agents-md` and `claude-md`; cursor/copilot emitters wait for design
//! partner pull.

mod agents_md;
mod claude_md;
mod cursor_rules;
mod open_code_review;

pub(crate) use agents_md::AGENTS_MD;
pub(crate) use claude_md::CLAUDE_MD;
pub(crate) use cursor_rules::CURSOR_RULES;
pub(crate) use open_code_review::{OPEN_CODE_REVIEW, render_ocr_rules};

use crate::cli::ExportFormatArg;

/// A static export target: which file at the repo root, and which per-engine
/// enable flag gates the rule set (`None` = every active rule).
pub(crate) struct Emitter {
    /// CLI/JSON label, matches the `--format` value.
    pub(crate) format: &'static str,
    /// Repo-root file name the marker block lives in.
    pub(crate) file_name: &'static str,
    /// `skills.enabled_for_*` gate passed to the core collector.
    pub(crate) engine: Option<&'static str>,
    /// Writeback strategy for this target.
    pub(crate) kind: EmitterKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EmitterKind {
    /// Markdown/text file containing a BEGIN/END DIFFLORE RULES block.
    MarkerBlock,
    /// Whole-file JSON owned by DiffLore.
    OwnedJson,
}

/// Expand `--format` values (repeatable, `all` is a macro) into a deduped,
/// stable-ordered emitter list.
pub(crate) fn resolve(formats: &[ExportFormatArg]) -> Vec<&'static Emitter> {
    let mut out: Vec<&'static Emitter> = Vec::new();
    let mut push = |emitter: &'static Emitter| {
        if !out.iter().any(|e| std::ptr::eq(*e, emitter)) {
            out.push(emitter);
        }
    };
    for format in formats {
        match format {
            ExportFormatArg::AgentsMd => push(&AGENTS_MD),
            ExportFormatArg::ClaudeMd => push(&CLAUDE_MD),
            ExportFormatArg::CursorMd => push(&CURSOR_RULES),
            ExportFormatArg::OpenCodeReview => push(&OPEN_CODE_REVIEW),
            ExportFormatArg::All => {
                push(&AGENTS_MD);
                push(&CLAUDE_MD);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_expands_all_and_dedupes_repeats() {
        let all = resolve(&[ExportFormatArg::All]);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].format, "agents-md");
        assert_eq!(all[1].format, "claude-md");

        let repeated = resolve(&[
            ExportFormatArg::ClaudeMd,
            ExportFormatArg::All,
            ExportFormatArg::ClaudeMd,
        ]);
        let labels: Vec<&str> = repeated.iter().map(|e| e.format).collect();
        assert_eq!(labels, vec!["claude-md", "agents-md"]);
    }

    #[test]
    fn emitters_pin_file_names_and_engine_gates() {
        assert_eq!(AGENTS_MD.file_name, "AGENTS.md");
        assert_eq!(AGENTS_MD.engine, None);
        assert_eq!(CLAUDE_MD.file_name, "CLAUDE.md");
        assert_eq!(CLAUDE_MD.engine, Some("claude"));
        assert_eq!(AGENTS_MD.kind, EmitterKind::MarkerBlock);
        assert_eq!(CLAUDE_MD.kind, EmitterKind::MarkerBlock);
    }

    #[test]
    fn cursor_emitter_pins_file_name_and_engine_gate() {
        assert_eq!(CURSOR_RULES.file_name, ".cursorrules");
        assert_eq!(CURSOR_RULES.engine, Some("cursor"));
        let resolved = resolve(&[ExportFormatArg::CursorMd]);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].format, "cursor-md");
    }

    #[test]
    fn open_code_review_emitter_pins_file_name_and_kind() {
        assert_eq!(OPEN_CODE_REVIEW.file_name, ".opencodereview/rule.json");
        assert_eq!(OPEN_CODE_REVIEW.engine, None);
        assert_eq!(OPEN_CODE_REVIEW.kind, EmitterKind::OwnedJson);
        let resolved = resolve(&[ExportFormatArg::OpenCodeReview]);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].format, "open-code-review");
    }

    #[test]
    fn resolve_all_stays_backwards_compatible() {
        let all = resolve(&[ExportFormatArg::All]);
        let labels: Vec<&str> = all.iter().map(|e| e.format).collect();
        assert_eq!(labels, vec!["agents-md", "claude-md"]);
    }
}
