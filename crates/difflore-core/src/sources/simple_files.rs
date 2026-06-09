//! Table-driven sources for plain file / directory-glob ingest.

use std::path::{Path, PathBuf};

use crate::errors::CoreError;

use super::{MemoryDoc, Source, read_file_doc};

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
        let mut docs = Vec::new();
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some(self.ext) {
                docs.push(read_file_doc(self.id, path)?);
            }
        }
        Ok(docs)
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

pub struct ClaudeMdSource;
pub struct AgentsMdSource;
pub struct CursorRulesSource;

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
}
