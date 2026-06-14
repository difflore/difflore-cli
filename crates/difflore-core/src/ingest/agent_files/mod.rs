//! Cross-vendor ingest sources: detect + read agent memory / rule files.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use crate::error::CoreError;

mod claude_code_memory;
mod simple_files;

pub use claude_code_memory::ClaudeCodeMemorySource;
pub use simple_files::{AgentsMdSource, ClaudeMdSource, CursorRulesSource};

#[derive(Debug, Clone)]
pub struct MemoryDoc {
    pub source_id: &'static str,
    pub path: PathBuf,
    pub content: String,
    pub modified_at: Option<DateTime<Utc>>,
}

pub trait Source: Send + Sync {
    fn id(&self) -> &'static str;
    fn label(&self) -> &'static str;
    fn detect(&self, repo_root: &Path) -> bool;
    fn read(&self, repo_root: &Path) -> Result<Vec<MemoryDoc>, CoreError>;
}

pub fn registered_sources() -> &'static [&'static dyn Source] {
    static SOURCES: &[&dyn Source] = &[
        &AgentsMdSource,
        &ClaudeMdSource,
        &ClaudeCodeMemorySource,
        &CursorRulesSource,
    ];
    SOURCES
}

pub(crate) fn read_file_doc(
    source_id: &'static str,
    path: PathBuf,
) -> Result<MemoryDoc, CoreError> {
    let content = std::fs::read_to_string(&path)?;
    let modified_at = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok()
        .map(DateTime::<Utc>::from);
    Ok(MemoryDoc {
        source_id,
        path,
        content,
        modified_at,
    })
}
