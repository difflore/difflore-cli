//! Per-project "last session start" watermark.
//!
//! Stored at `~/.difflore/projects/{hash}/last-session-start.json` as a JSON
//! blob `{ "ts_ms": …, "client": "…" }`. The read path stays permissive about
//! missing/malformed input (silent fallback to `None`, never panics).
//!
//! Concurrent SessionStart fires from two agent windows can race the write;
//! that's fine — last writer wins, and the only consequence is a slightly
//! older `prev_ts` in one banner. No lock is taken: a contended file lock
//! would defeat this hot path's 50 ms budget.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One row in the watermark file. `client` is diagnostic only — the banner
/// pipeline doesn't branch on it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Watermark {
    /// Unix epoch milliseconds (UTC) of the watermark write.
    pub ts_ms: i64,
    /// Adapter the SessionStart came from (`"claude-code"`, `"cursor"`, …).
    pub client: String,
}

/// Watermark file path under `~/.difflore/projects/{hash}/`. Does not create
/// the parent directory (the write helper handles that).
fn watermark_path(project_hash: &str) -> PathBuf {
    difflore_core::infra::db::project_index_dir(project_hash).join("last-session-start.json")
}

/// Read the watermark for the given project hash. Returns `None` if the file
/// is missing, unreadable, or unparseable — all of which the banner treats as
/// a fresh repo (shows everything learned to date, capped at the row limit).
pub fn read_watermark(project_hash: &str) -> Option<Watermark> {
    read_watermark_at(&watermark_path(project_hash))
}

/// Write the watermark for the given project hash, creating the parent dir on
/// demand. Best-effort: the caller can ignore the `Result`.
pub fn write_watermark(project_hash: &str, wm: &Watermark) -> Result<(), String> {
    write_watermark_at(&watermark_path(project_hash), wm)
}

/// Pure-path variant of [`read_watermark`] so tests can pass a tempdir path
/// instead of mutating `DIFFLORE_HOME` (which needs `unsafe` and races other
/// tests reading the env var).
fn read_watermark_at(path: &Path) -> Option<Watermark> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<Watermark>(&raw).ok()
}

/// Pure-path variant of [`write_watermark`]. See [`read_watermark_at`]
/// for the testability rationale.
fn write_watermark_at(path: &Path, wm: &Watermark) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create dir: {e}"))?;
    }
    let body = serde_json::to_string(wm).map_err(|e| format!("serialize: {e}"))?;
    // Atomic-ish write via tempfile + rename: prevents a crashed write
    // from leaving a half-written JSON that the next read would treat
    // as "fresh repo" and re-show every rule.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body).map_err(|e| format!("write tmp: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("rename: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_then_read_returns_same_value() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("last-session-start.json");
        let wm = Watermark {
            ts_ms: 1_700_000_000_000,
            client: "claude-code".to_owned(),
        };
        write_watermark_at(&path, &wm).expect("write ok");
        let back = read_watermark_at(&path).expect("read ok");
        assert_eq!(back.ts_ms, wm.ts_ms);
        assert_eq!(back.client, wm.client);
    }

    #[test]
    fn read_missing_returns_none() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("never-written.json");
        assert!(read_watermark_at(&path).is_none());
    }

    #[test]
    fn read_garbage_json_returns_none() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("garbage.json");
        std::fs::write(&path, "not json at all").expect("write");
        assert!(read_watermark_at(&path).is_none());
    }

    #[test]
    fn write_creates_missing_parent_dirs() {
        // A fresh repo's `~/.difflore/projects/{hash}/` doesn't exist until
        // the first write, so `write` must mkdir-p its parent.
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp
            .path()
            .join("projects")
            .join("abc123")
            .join("last-session-start.json");
        assert!(!path.parent().expect("parent").exists(), "precondition");
        let wm = Watermark {
            ts_ms: 1,
            client: "cursor".to_owned(),
        };
        write_watermark_at(&path, &wm).expect("write ok");
        assert!(path.exists(), "watermark file missing post-write");
    }
}
