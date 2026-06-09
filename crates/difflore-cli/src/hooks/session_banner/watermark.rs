//! Per-project "last session start" watermark.
//!
//! Stored at `~/.difflore/projects/{hash}/last-session-start.json` as a
//! one-shot JSON blob `{ "ts_ms": …, "client": "…" }`. Owning this file
//! is a single-purpose responsibility — no other code reads or writes
//! it — so the format can change freely as long as the read path stays
//! permissive about missing/malformed input (silent fallback to `None`,
//! never panics).
//!
//! Concurrent SessionStart fires from two agent windows in the same repo
//! could race the write here. That's fine: whichever fires last wins,
//! and the only consequence is one of the two banners may show a
//! slightly older `prev_ts`. We deliberately do NOT take a lock — the
//! whole helper is on the hot path and a contended file lock would
//! defeat the 50 ms budget.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One row in the watermark file. `client` is purely diagnostic — the
/// banner pipeline doesn't branch on it today, but storing it lets a
/// future audit answer "which agent last opened this repo?" without
/// crawling the fire log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Watermark {
    /// Unix epoch milliseconds (UTC) of the watermark write.
    pub ts_ms: i64,
    /// Adapter name the SessionStart came from (`"claude-code"`,
    /// `"cursor"`, …). Diagnostic only.
    pub client: String,
}

/// Resolve the watermark file path under the canonical
/// `~/.difflore/projects/{hash}/` layout. Does NOT create the parent
/// directory — the write helper handles that, and the read helper is
/// satisfied with a missing path (returns `None`).
fn watermark_path(project_hash: &str) -> PathBuf {
    difflore_core::db::project_index_dir(project_hash).join("last-session-start.json")
}

/// Read the watermark for the given project hash. Returns `None` when:
///   * the file doesn't exist (first session on this repo),
///   * the file exists but is unreadable,
///   * the JSON fails to parse.
///
/// All three collapse into "treat this like a fresh repo" — the banner
/// then shows everything learned to date, capped at the row limit.
pub fn read_watermark(project_hash: &str) -> Option<Watermark> {
    read_watermark_at(&watermark_path(project_hash))
}

/// Write the watermark for the given project hash. Best-effort:
/// caller can ignore the `Result` via `let _ = …`. Creates the parent
/// dir on demand — first-ever SessionStart in a repo finds
/// `~/.difflore/projects/{hash}/` missing.
pub fn write_watermark(project_hash: &str, wm: &Watermark) -> Result<(), String> {
    write_watermark_at(&watermark_path(project_hash), wm)
}

/// Pure-path variant of [`read_watermark`]. Tests call this with a
/// tempdir-rooted path instead of mutating `DIFFLORE_HOME`, which would
/// require an `unsafe` block (forbidden by the workspace's
/// `unsafe_code = "deny"` lint) and would race other tests reading the
/// same env var.
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

    /// End-to-end roundtrip against a tempdir-rooted path.
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
        // A fresh repo's `~/.difflore/projects/{hash}/` directory
        // doesn't exist until the watermark write runs. Production
        // would surface this as a hot-path stall if `write` didn't
        // mkdir-p — regression-guard the behaviour here.
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
