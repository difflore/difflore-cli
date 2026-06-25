//! Export emitters: one per static context-file convention. v1 ships
//! `agents-md` and `claude-md`; cursor/copilot emitters wait for design
//! partner pull.

mod agents_md;
mod claude_md;

pub(crate) use agents_md::AGENTS_MD;
pub(crate) use claude_md::CLAUDE_MD;

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
    }
}
