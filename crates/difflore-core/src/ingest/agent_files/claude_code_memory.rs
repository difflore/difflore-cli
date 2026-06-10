//! Claude Code per-project memory at `~/.claude/projects/<slug>/memory/`.

use std::path::{Path, PathBuf};

use crate::error::CoreError;

use super::{MemoryDoc, Source, read_file_doc};

pub struct ClaudeCodeMemorySource;

const ID: &str = "claude-code-memory";

/// Convert an absolute repo path to Claude Code's project slug, replacing
/// path separators and filename-hostile chars (including the Windows drive
/// separator: `C:\Users\alice` -> `C--Users-alice`).
fn project_slug(repo_root: &Path) -> Option<String> {
    let canonical = repo_root.canonicalize().ok()?;
    Some(project_slug_from_path_text(&canonical.to_string_lossy()))
}

fn project_slug_from_path_text(path: &str) -> String {
    let path = path.strip_prefix(r"\\?\").unwrap_or(path);
    path.chars()
        .map(|ch| match ch {
            '\\' | '/' | ':' | '<' | '>' | '"' | '|' | '?' | '*' => '-',
            _ => ch,
        })
        .collect()
}

fn memory_dir(repo_root: &Path) -> Option<PathBuf> {
    let slug = project_slug(repo_root)?;
    let home = claude_home_dir()?;
    Some(
        home.join(".claude")
            .join("projects")
            .join(slug)
            .join("memory"),
    )
}

fn claude_home_dir() -> Option<PathBuf> {
    if let Some(home) = crate::infra::env::var_os(crate::infra::env::DIFFLORE_CLAUDE_HOME)
        && !home.is_empty()
    {
        return Some(PathBuf::from(home));
    }
    dirs::home_dir()
}

impl Source for ClaudeCodeMemorySource {
    fn id(&self) -> &'static str {
        ID
    }
    fn label(&self) -> &'static str {
        "Claude Code memory"
    }
    fn detect(&self, repo_root: &Path) -> bool {
        memory_dir(repo_root).is_some_and(|p| p.is_dir())
    }
    fn read(&self, repo_root: &Path) -> Result<Vec<MemoryDoc>, CoreError> {
        let Some(dir) = memory_dir(repo_root) else {
            return Ok(Vec::new());
        };
        if !dir.is_dir() {
            return Ok(Vec::new());
        }
        let mut docs = Vec::new();
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("md") {
                docs.push(read_file_doc(ID, path)?);
            }
        }
        Ok(docs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn project_slug_sanitizes_windows_drive_paths() {
        assert_eq!(
            project_slug_from_path_text(r"C:\Users\alice\repo"),
            "C--Users-alice-repo"
        );
        assert_eq!(
            project_slug_from_path_text(r"\\?\C:\Users\alice\repo"),
            "C--Users-alice-repo"
        );
    }

    #[test]
    fn detects_memory_dir_via_overridden_home() {
        let home = TempDir::new().unwrap();
        let repo = TempDir::new().unwrap();
        let slug = project_slug(repo.path()).expect("slug");
        let memory = home
            .path()
            .join(".claude")
            .join("projects")
            .join(&slug)
            .join("memory");
        std::fs::create_dir_all(&memory).unwrap();
        std::fs::write(memory.join("a.md"), "rule a").unwrap();
        std::fs::write(memory.join("b.md"), "rule b").unwrap();
        std::fs::write(memory.join("ignored.txt"), "skip").unwrap();

        let (detected, docs) = temp_env::with_var(
            "DIFFLORE_CLAUDE_HOME",
            Some(home.path().as_os_str()),
            || {
                let detected = ClaudeCodeMemorySource.detect(repo.path());
                let mut docs = ClaudeCodeMemorySource.read(repo.path()).unwrap();
                docs.sort_by(|a, b| a.path.cmp(&b.path));
                (detected, docs)
            },
        );

        assert!(detected);
        assert_eq!(docs.len(), 2);
        assert!(docs.iter().all(|d| d.source_id == ID));
        assert!(docs.iter().any(|d| d.content == "rule a"));
        assert!(docs.iter().any(|d| d.content == "rule b"));
    }
}
