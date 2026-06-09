use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::errors::CoreError;
use crate::models::{FileReadRecord, FileSearchResult, FilesReadInput, FilesSearchInput};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSearchResponse {
    pub results: Vec<FileSearchResult>,
    pub warnings: Vec<String>,
}

const MAX_DEPTH: usize = 4;

const SKIP_DIR_NAMES: &[&str] = &[
    "node_modules",
    ".git",
    "target",
    ".cursor",
    "dist",
    "build",
    ".next",
    ".svn",
    "__pycache__",
    ".idea",
    ".vscode",
];

fn should_skip_dir(name: &str) -> bool {
    SKIP_DIR_NAMES.contains(&name)
}

fn name_matches_query(file_name: &str, query: &str) -> bool {
    let q = query.trim();
    if q.is_empty() {
        return true;
    }
    let ql = q.to_lowercase();
    let nl = file_name.to_lowercase();
    nl.contains(&ql) || fuzzy_subsequence(&nl, &ql)
}

fn fuzzy_subsequence(name: &str, query: &str) -> bool {
    let mut it = name.chars();
    for qc in query.chars() {
        let mut found = false;
        for c in it.by_ref() {
            if c == qc {
                found = true;
                break;
            }
        }
        if !found {
            return false;
        }
    }
    true
}

async fn resolve_registered_project_root(
    db: &sqlx::SqlitePool,
    project_path: &str,
) -> crate::Result<PathBuf> {
    let raw_root = PathBuf::from(project_path);
    if !raw_root.exists() {
        return Err(CoreError::Validation("project path does not exist".into()));
    }

    let root = raw_root
        .canonicalize()
        .map_err(|e| CoreError::Validation(format!("invalid project path: {e}")))?;

    if !root.is_dir() {
        return Err(CoreError::Validation(
            "project path must be a directory".into(),
        ));
    }

    let normalized_root = root.to_string_lossy().replace('\\', "/");
    let exists: i64 = sqlx::query_scalar!(
        r#"SELECT COUNT(1) as "n!: i64" FROM projects WHERE path = ?1"#,
        normalized_root
    )
    .fetch_one(db)
    .await
    .map_err(|e| CoreError::Internal(format!("failed to validate project path: {e}")))?;

    if exists == 0 {
        return Err(CoreError::Validation(
            "project path must belong to a registered project".into(),
        ));
    }

    Ok(root)
}

fn walk(
    root: &Path,
    rel: &Path,
    depth: usize,
    query: &str,
    out: &mut Vec<FileSearchResult>,
    warnings: &mut Vec<String>,
    limit: usize,
) -> crate::Result<()> {
    if out.len() >= limit || depth > MAX_DEPTH {
        return Ok(());
    }

    let dir = root.join(rel);
    let read = match std::fs::read_dir(&dir) {
        Ok(r) => r,
        Err(e) => {
            warnings.push(format!(
                "Could not read directory {}: {}",
                dir.to_string_lossy(),
                e
            ));
            return Ok(());
        }
    };

    for entry in read {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if should_skip_dir(&name) {
            continue;
        }

        let rel_path: PathBuf = if rel.as_os_str().is_empty() {
            PathBuf::from(&name)
        } else {
            rel.join(&name)
        };
        let rel_display = rel_path.to_string_lossy().replace('\\', "/");

        let is_dir = entry.file_type()?.is_dir();

        if name_matches_query(&name, query) {
            out.push(FileSearchResult {
                path: rel_display.clone(),
                relative_path: rel_display.clone(),
                is_directory: is_dir,
            });
            if out.len() >= limit {
                return Ok(());
            }
        }

        if is_dir && depth < MAX_DEPTH {
            walk(root, &rel_path, depth + 1, query, out, warnings, limit)?;
            if out.len() >= limit {
                return Ok(());
            }
        }
    }
    Ok(())
}

pub async fn search(
    db: &sqlx::SqlitePool,
    input: FilesSearchInput,
) -> crate::Result<FileSearchResponse> {
    let root = resolve_registered_project_root(db, &input.project_path).await?;
    let limit = usize::try_from(input.limit.unwrap_or(100).max(0)).unwrap_or(0);
    if limit == 0 {
        return Ok(FileSearchResponse {
            results: vec![],
            warnings: vec![],
        });
    }
    let mut out = Vec::new();
    let mut warnings = Vec::new();
    walk(
        &root,
        Path::new(""),
        0,
        &input.query,
        &mut out,
        &mut warnings,
        limit,
    )?;
    Ok(FileSearchResponse {
        results: out,
        warnings,
    })
}

pub async fn read(db: &sqlx::SqlitePool, input: FilesReadInput) -> crate::Result<FileReadRecord> {
    let root = resolve_registered_project_root(db, &input.project_path).await?;

    let rel = PathBuf::from(&input.relative_path);
    if rel.is_absolute() {
        return Err(CoreError::Validation(
            "relativePath must be relative".into(),
        ));
    }
    if rel
        .components()
        .any(|c| matches!(c, Component::ParentDir | Component::Prefix(_)))
    {
        return Err(CoreError::Validation(
            "relativePath contains invalid components".into(),
        ));
    }

    let abs = root.join(&rel);
    let abs = abs
        .canonicalize()
        .map_err(|e| CoreError::Validation(format!("file not found: {e}")))?;
    if !abs.starts_with(&root) {
        return Err(CoreError::Validation("path escapes project root".into()));
    }
    if !abs.is_file() {
        return Err(CoreError::Validation("path is not a file".into()));
    }

    let max_bytes = usize::try_from(
        input
            .max_bytes
            .unwrap_or(256 * 1024)
            .clamp(1, 2 * 1024 * 1024),
    )
    .unwrap_or(256 * 1024);
    let bytes = std::fs::read(&abs)?;
    let truncated = bytes.len() > max_bytes;
    let bytes = if truncated {
        &bytes[..max_bytes]
    } else {
        &bytes[..]
    };

    // Best-effort UTF-8. If invalid, we still return lossy text so UI can display.
    let content = String::from_utf8_lossy(bytes).to_string();

    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let sha256 = {
        use std::fmt::Write as _;
        digest
            .iter()
            .fold(String::with_capacity(digest.len() * 2), |mut acc, b| {
                let _ = write!(&mut acc, "{b:02x}");
                acc
            })
    };

    let all_lines: Vec<&str> = content.lines().collect();
    let total_lines = i32::try_from(all_lines.len()).unwrap_or(i32::MAX);

    let start = input.start_line.unwrap_or(1).max(1);
    let end = input.end_line.unwrap_or(total_lines.max(1)).max(start);

    let start_idx = (start - 1) as usize;
    let end_idx_exclusive = end.min(total_lines) as usize;

    let sliced = if start_idx >= all_lines.len() {
        String::new()
    } else {
        all_lines[start_idx..end_idx_exclusive].join("\n")
    };

    let language = abs
        .extension()
        .and_then(|e| e.to_str())
        .map(ToOwned::to_owned);

    Ok(FileReadRecord {
        absolute_path: abs.to_string_lossy().to_string(),
        relative_path: input.relative_path.replace('\\', "/"),
        content: sliced,
        language,
        line_count: total_lines,
        truncated,
        sha256: Some(sha256),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_skip_well_known_build_dirs() {
        assert!(should_skip_dir("node_modules"));
        assert!(should_skip_dir("target"));
        assert!(should_skip_dir(".git"));
        assert!(!should_skip_dir("src"));
        assert!(!should_skip_dir("crates"));
    }

    #[test]
    fn name_matches_empty_query_is_always_true() {
        assert!(name_matches_query("anything.rs", ""));
        assert!(name_matches_query("", ""));
    }

    #[test]
    fn name_matches_exact_substring() {
        assert!(name_matches_query("UserService.ts", "user"));
        assert!(name_matches_query("UserService.ts", "Service"));
        assert!(!name_matches_query("UserService.ts", "admin"));
    }

    #[test]
    fn fuzzy_subsequence_matches_scattered_chars() {
        assert!(fuzzy_subsequence("usrservice", "usrvc"));
        assert!(fuzzy_subsequence("abcde", "ace"));
        assert!(!fuzzy_subsequence("abcde", "aec"));
        assert!(!fuzzy_subsequence("abc", "abcd"));
    }

    #[test]
    fn name_matches_falls_back_to_fuzzy_when_substring_fails() {
        // "usvc" is not a substring of "UserService.ts" (case-normalized "userservice.ts")
        // but is a subsequence: u→s→v...wait, "v" is not in "userservice.ts". Use a real case.
        assert!(name_matches_query("UserServiceImpl.ts", "usi"));
    }
}
