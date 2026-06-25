use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::domain::models::{FileReadRecord, FileSearchResult, FilesReadInput, FilesSearchInput};
use crate::error::CoreError;

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

    let normalized_root = project_path_lookup_key(&root.to_string_lossy());
    let project_paths = sqlx::query_scalar::<_, String>("SELECT path FROM projects")
        .fetch_all(db)
        .await
        .map_err(|e| CoreError::Internal(format!("failed to validate project path: {e}")))?;

    let exists = project_paths
        .iter()
        .any(|path| project_path_lookup_key(path) == normalized_root);

    if !exists {
        return Err(CoreError::Validation(
            "project path must belong to a registered project".into(),
        ));
    }

    Ok(root)
}

fn project_path_lookup_key(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    let without_verbatim = normalized
        .strip_prefix("//?/")
        .or_else(|| normalized.strip_prefix("/?/"))
        .unwrap_or(&normalized);
    let without_unc = without_verbatim
        .strip_prefix("UNC/")
        .map_or_else(|| without_verbatim.to_owned(), |rest| format!("//{rest}"));
    if cfg!(windows) {
        without_unc.to_ascii_lowercase()
    } else {
        without_unc
    }
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

/// Atomically write `contents` to `path` via a temp file in the same directory
/// followed by a rename, so a crash or concurrent reader can never observe a
/// truncated/half-written file. Parent directories are created; the fsync
/// before the rename is best-effort (some filesystems reject it). `tempfile` is
/// only a dev-dependency of this crate, so the temp file is rolled by hand with
/// a per-write-unique name (pid + counter) to avoid same-process collisions.
///
/// Note: this guarantees *integrity* (no torn writes), not serialization — two
/// concurrent read-modify-write callers still race to last-writer-wins, which
/// is acceptable for the best-effort telemetry/cache files that use it.
pub fn write_atomic(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;

    let file_name = path.file_name().map_or_else(
        || "difflore".to_owned(),
        |n| n.to_string_lossy().into_owned(),
    );
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(
        ".{file_name}.difflore-tmp-{}-{seq}",
        std::process::id()
    ));

    let result = std::fs::File::create(&tmp)
        .and_then(|mut file| {
            file.write_all(contents)?;
            let _ = file.sync_all();
            Ok(())
        })
        .and_then(|()| std::fs::rename(&tmp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
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
    fn write_atomic_creates_replaces_and_leaves_no_temp_litter() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Writes into a not-yet-existing nested dir.
        let path = dir.path().join("nested").join("log.json");
        write_atomic(&path, b"first").expect("first write");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "first");

        // Overwrites in place.
        write_atomic(&path, b"second").expect("second write");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "second");

        // No `.difflore-tmp-` files left behind in the target dir.
        let leftover = std::fs::read_dir(path.parent().unwrap())
            .expect("read_dir")
            .filter_map(Result::ok)
            .any(|e| e.file_name().to_string_lossy().contains("difflore-tmp-"));
        assert!(!leftover, "atomic write must not leave temp files");
    }

    #[test]
    fn write_atomic_is_concurrency_safe_for_temp_names() {
        // Many threads writing the same target must never collide on a temp
        // path or corrupt the destination (last-writer-wins is fine).
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("c.json");
        std::thread::scope(|s| {
            for n in 0..16 {
                let path = path.clone();
                s.spawn(move || {
                    let body = format!("{{\"n\":{n}}}");
                    write_atomic(&path, body.as_bytes()).expect("write");
                });
            }
        });
        // Whatever won, it must be one valid, complete payload (not torn).
        let raw = std::fs::read_to_string(&path).expect("read");
        serde_json::from_str::<serde_json::Value>(&raw).expect("valid json");
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
        // "usi" is not a substring of "userserviceimpl.ts" but is a subsequence.
        assert!(name_matches_query("UserServiceImpl.ts", "usi"));
    }
}
