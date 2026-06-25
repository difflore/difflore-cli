//! Table-driven sources for plain file / directory-glob ingest.

use std::path::{Path, PathBuf};

use crate::error::CoreError;

use super::{MemoryDoc, Source, read_dir_docs_with_ext, read_file_doc};

struct SingleFileSpec {
    id: &'static str,
    label: &'static str,
    file_name: &'static str,
}

impl SingleFileSpec {
    fn detect(&self, repo_root: &Path) -> bool {
        repo_root.join(self.file_name).is_file()
    }
    fn read(&self, repo_root: &Path) -> Result<Vec<MemoryDoc>, CoreError> {
        let path = repo_root.join(self.file_name);
        if !path.is_file() {
            return Ok(Vec::new());
        }
        Ok(vec![read_file_doc(self.id, path)?])
    }
}

struct DirGlobSpec {
    id: &'static str,
    label: &'static str,
    dir: &'static [&'static str],
    ext: &'static str,
}

impl DirGlobSpec {
    fn resolve_dir(&self, repo_root: &Path) -> PathBuf {
        let mut p = repo_root.to_path_buf();
        for seg in self.dir {
            p.push(seg);
        }
        p
    }
    fn detect(&self, repo_root: &Path) -> bool {
        self.resolve_dir(repo_root).is_dir()
    }
    fn read(&self, repo_root: &Path) -> Result<Vec<MemoryDoc>, CoreError> {
        let dir = self.resolve_dir(repo_root);
        if !dir.is_dir() {
            return Ok(Vec::new());
        }
        read_dir_docs_with_ext(self.id, &dir, self.ext)
    }
}

const CLAUDE_MD: SingleFileSpec = SingleFileSpec {
    id: "claude-md",
    label: "CLAUDE.md",
    file_name: "CLAUDE.md",
};

const AGENTS_MD: SingleFileSpec = SingleFileSpec {
    id: "agents-md",
    label: "AGENTS.md",
    file_name: "AGENTS.md",
};

const CURSOR_RULES: DirGlobSpec = DirGlobSpec {
    id: "cursor-rules",
    label: "Cursor rules",
    dir: &[".cursor", "rules"],
    ext: "mdc",
};

const GEMINI_MD: SingleFileSpec = SingleFileSpec {
    id: "gemini-md",
    label: "GEMINI.md",
    file_name: "GEMINI.md",
};

const WINDSURF_RULES_FILE: SingleFileSpec = SingleFileSpec {
    id: "windsurf-rules-file",
    label: ".windsurfrules",
    file_name: ".windsurfrules",
};

const WINDSURF_RULES_DIR: DirGlobSpec = DirGlobSpec {
    id: "windsurf-rules",
    label: "Windsurf rules",
    dir: &[".windsurf", "rules"],
    ext: "md",
};

const CLINE_RULES: SingleFileSpec = SingleFileSpec {
    id: "cline-rules",
    label: ".clinerules",
    file_name: ".clinerules",
};

const COPILOT_INSTRUCTIONS: SingleFileSpec = SingleFileSpec {
    id: "copilot-instructions",
    label: "GitHub Copilot instructions",
    file_name: ".github/copilot-instructions.md",
};

pub struct ClaudeMdSource;
pub struct AgentsMdSource;
pub struct CursorRulesSource;
pub struct GeminiMdSource;
pub struct WindsurfRulesFileSource;
pub struct WindsurfRulesDirSource;
pub struct ClineRulesSource;
pub struct CopilotInstructionsSource;

impl Source for ClaudeMdSource {
    fn id(&self) -> &'static str {
        CLAUDE_MD.id
    }
    fn label(&self) -> &'static str {
        CLAUDE_MD.label
    }
    fn detect(&self, repo_root: &Path) -> bool {
        CLAUDE_MD.detect(repo_root)
    }
    fn read(&self, repo_root: &Path) -> Result<Vec<MemoryDoc>, CoreError> {
        CLAUDE_MD.read(repo_root)
    }
}

impl Source for AgentsMdSource {
    fn id(&self) -> &'static str {
        AGENTS_MD.id
    }
    fn label(&self) -> &'static str {
        AGENTS_MD.label
    }
    fn detect(&self, repo_root: &Path) -> bool {
        AGENTS_MD.detect(repo_root)
    }
    fn read(&self, repo_root: &Path) -> Result<Vec<MemoryDoc>, CoreError> {
        AGENTS_MD.read(repo_root)
    }
}

impl Source for CursorRulesSource {
    fn id(&self) -> &'static str {
        CURSOR_RULES.id
    }
    fn label(&self) -> &'static str {
        CURSOR_RULES.label
    }
    fn detect(&self, repo_root: &Path) -> bool {
        CURSOR_RULES.detect(repo_root)
    }
    fn read(&self, repo_root: &Path) -> Result<Vec<MemoryDoc>, CoreError> {
        CURSOR_RULES.read(repo_root)
    }
}

impl Source for GeminiMdSource {
    fn id(&self) -> &'static str {
        GEMINI_MD.id
    }
    fn label(&self) -> &'static str {
        GEMINI_MD.label
    }
    fn detect(&self, repo_root: &Path) -> bool {
        GEMINI_MD.detect(repo_root)
    }
    fn read(&self, repo_root: &Path) -> Result<Vec<MemoryDoc>, CoreError> {
        GEMINI_MD.read(repo_root)
    }
}

impl Source for WindsurfRulesFileSource {
    fn id(&self) -> &'static str {
        WINDSURF_RULES_FILE.id
    }
    fn label(&self) -> &'static str {
        WINDSURF_RULES_FILE.label
    }
    fn detect(&self, repo_root: &Path) -> bool {
        WINDSURF_RULES_FILE.detect(repo_root)
    }
    fn read(&self, repo_root: &Path) -> Result<Vec<MemoryDoc>, CoreError> {
        WINDSURF_RULES_FILE.read(repo_root)
    }
}

impl Source for WindsurfRulesDirSource {
    fn id(&self) -> &'static str {
        WINDSURF_RULES_DIR.id
    }
    fn label(&self) -> &'static str {
        WINDSURF_RULES_DIR.label
    }
    fn detect(&self, repo_root: &Path) -> bool {
        WINDSURF_RULES_DIR.detect(repo_root)
    }
    fn read(&self, repo_root: &Path) -> Result<Vec<MemoryDoc>, CoreError> {
        WINDSURF_RULES_DIR.read(repo_root)
    }
}

impl Source for ClineRulesSource {
    fn id(&self) -> &'static str {
        CLINE_RULES.id
    }
    fn label(&self) -> &'static str {
        CLINE_RULES.label
    }
    fn detect(&self, repo_root: &Path) -> bool {
        CLINE_RULES.detect(repo_root)
    }
    fn read(&self, repo_root: &Path) -> Result<Vec<MemoryDoc>, CoreError> {
        CLINE_RULES.read(repo_root)
    }
}

impl Source for CopilotInstructionsSource {
    fn id(&self) -> &'static str {
        COPILOT_INSTRUCTIONS.id
    }
    fn label(&self) -> &'static str {
        COPILOT_INSTRUCTIONS.label
    }
    fn detect(&self, repo_root: &Path) -> bool {
        COPILOT_INSTRUCTIONS.detect(repo_root)
    }
    fn read(&self, repo_root: &Path) -> Result<Vec<MemoryDoc>, CoreError> {
        COPILOT_INSTRUCTIONS.read(repo_root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn detects_claude_md_when_present() {
        let dir = TempDir::new().unwrap();
        assert!(!ClaudeMdSource.detect(dir.path()));
        std::fs::write(dir.path().join("CLAUDE.md"), "Project memory.").unwrap();
        assert!(ClaudeMdSource.detect(dir.path()));

        let docs = ClaudeMdSource.read(dir.path()).unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].content.trim(), "Project memory.");
    }

    #[test]
    fn detects_agents_md_when_present() {
        let dir = TempDir::new().unwrap();
        assert!(!AgentsMdSource.detect(dir.path()));
        std::fs::write(dir.path().join("AGENTS.md"), "# Agents\nuse rust").unwrap();
        assert!(AgentsMdSource.detect(dir.path()));

        let docs = AgentsMdSource.read(dir.path()).unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].source_id, "agents-md");
        assert!(docs[0].content.contains("use rust"));
    }

    #[test]
    fn detects_cursor_rules_dir() {
        let dir = TempDir::new().unwrap();
        assert!(!CursorRulesSource.detect(dir.path()));
        let rules = dir.path().join(".cursor").join("rules");
        std::fs::create_dir_all(&rules).unwrap();
        std::fs::write(rules.join("a.mdc"), "---\nname: a\n---\nbody").unwrap();
        std::fs::write(rules.join("README.txt"), "skip").unwrap();
        assert!(CursorRulesSource.detect(dir.path()));

        let docs = CursorRulesSource.read(dir.path()).unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].source_id, "cursor-rules");
        assert!(docs[0].content.contains("name: a"));
    }

    #[test]
    fn cursor_rules_are_returned_in_stable_path_order() {
        let dir = TempDir::new().unwrap();
        let rules = dir.path().join(".cursor").join("rules");
        std::fs::create_dir_all(&rules).unwrap();
        std::fs::write(rules.join("b.mdc"), "rule b").unwrap();
        std::fs::write(rules.join("a.mdc"), "rule a").unwrap();

        let docs = CursorRulesSource.read(dir.path()).unwrap();

        assert_eq!(docs.len(), 2);
        assert!(docs[0].path.ends_with("a.mdc"));
        assert!(docs[1].path.ends_with("b.mdc"));
    }

    #[test]
    fn detects_additional_agent_file_sources() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("GEMINI.md"), "Use pnpm.").unwrap();
        std::fs::write(dir.path().join(".windsurfrules"), "Never commit secrets.").unwrap();
        std::fs::write(dir.path().join(".clinerules"), "Prefer small patches.").unwrap();
        let github = dir.path().join(".github");
        std::fs::create_dir_all(&github).unwrap();
        std::fs::write(github.join("copilot-instructions.md"), "Use Rust 2024.").unwrap();

        assert!(GeminiMdSource.detect(dir.path()));
        assert!(WindsurfRulesFileSource.detect(dir.path()));
        assert!(ClineRulesSource.detect(dir.path()));
        assert!(CopilotInstructionsSource.detect(dir.path()));
        assert_eq!(CopilotInstructionsSource.read(dir.path()).unwrap().len(), 1);
    }

    #[test]
    fn windsurf_dir_reads_markdown_rules_in_stable_order() {
        let dir = TempDir::new().unwrap();
        let rules = dir.path().join(".windsurf").join("rules");
        std::fs::create_dir_all(&rules).unwrap();
        std::fs::write(rules.join("b.md"), "rule b").unwrap();
        std::fs::write(rules.join("a.md"), "rule a").unwrap();
        std::fs::write(rules.join("skip.txt"), "skip").unwrap();

        let docs = WindsurfRulesDirSource.read(dir.path()).unwrap();

        assert_eq!(docs.len(), 2);
        assert!(docs[0].path.ends_with("a.md"));
        assert!(docs[1].path.ends_with("b.md"));
    }
}
