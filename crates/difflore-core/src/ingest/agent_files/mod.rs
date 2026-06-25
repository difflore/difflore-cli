//! Cross-vendor ingest sources: detect + read agent memory / rule files.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use crate::error::CoreError;

mod claude_code_memory;
mod import;
mod simple_files;
mod splitter;

pub use claude_code_memory::ClaudeCodeMemorySource;
pub use import::{
    AgentFileImportOptions, AgentFileImportReport, DEFAULT_AGENT_FILE_REVIEW_RULE_CONFIDENCE,
    import_agent_files_for_repo, import_agent_files_for_repo_with_options,
};
pub use simple_files::{
    AgentsMdSource, ClaudeMdSource, ClineRulesSource, CopilotInstructionsSource, CursorRulesSource,
    GeminiMdSource, WindsurfRulesDirSource, WindsurfRulesFileSource,
};
pub use splitter::{AgentFileMemoryEntry, AgentFileMemoryKind, split_memory_doc};

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
        &GeminiMdSource,
        &WindsurfRulesFileSource,
        &WindsurfRulesDirSource,
        &ClineRulesSource,
        &CopilotInstructionsSource,
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

pub(crate) fn read_dir_docs_with_ext(
    source_id: &'static str,
    dir: &Path,
    ext: &str,
) -> Result<Vec<MemoryDoc>, CoreError> {
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let Ok(entry) = entry else {
            continue;
        };
        let path = entry.path();
        if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some(ext) {
            paths.push(path);
        }
    }
    paths.sort();

    let mut docs = Vec::new();
    for path in paths {
        if let Ok(doc) = read_file_doc(source_id, path) {
            docs.push(doc);
        }
    }
    Ok(docs)
}
