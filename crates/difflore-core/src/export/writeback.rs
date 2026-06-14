//! Marker-block writeback engine.
//!
//! Generalises the installer's `upsert_gemini_md_context` tag-upsert into a
//! reusable BEGIN/END section engine with the safety rails a user-owned file
//! needs:
//!
//!   - the section between [`BEGIN_MARKER`] and [`END_MARKER`] is regenerated;
//!     every byte outside it (including CRLF line endings) is preserved
//!     verbatim,
//!   - rewrites are atomic (temp file + rename) so a crash can't truncate the
//!     user's `AGENTS.md`,
//!   - an unchanged content hash short-circuits to [`WriteAction::Unchanged`]
//!     without touching the file,
//!   - a `BEGIN` without its `END` (or vice versa) is treated as corruption
//!     and refused with a warning instead of guessing at a splice point,
//!   - symlinked targets are refused: real-world fixtures have `CLAUDE.md`
//!     symlinked to another agent's instructions file, and writing through it
//!     would silently edit that other file.

use std::io::Write as _;
use std::path::Path;

use crate::error::CoreError;

pub const BEGIN_MARKER: &str = "<!-- BEGIN DIFFLORE RULES -->";
pub const END_MARKER: &str = "<!-- END DIFFLORE RULES -->";

/// Header field carrying the rules-body hash inside the generated block.
const CONTENT_HASH_FIELD: &str = "content-hash:";

/// What the upsert did (or would do, under `dry_run`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteAction {
    /// Target file did not exist; it was created with just the block.
    Created,
    /// Target file existed; the block was inserted or regenerated.
    Updated,
    /// Existing block already carries the same content hash; nothing written.
    Unchanged,
    /// Refused to write (symlink, corrupted markers, unreadable file).
    Skipped,
}

impl WriteAction {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Updated => "updated",
            Self::Unchanged => "unchanged",
            Self::Skipped => "skipped",
        }
    }
}

#[derive(Debug, Clone)]
pub struct WriteOutcome {
    pub action: WriteAction,
    /// Human-readable refusal reason; set only for [`WriteAction::Skipped`].
    pub reason: Option<String>,
}

impl WriteOutcome {
    const fn ok(action: WriteAction) -> Self {
        Self {
            action,
            reason: None,
        }
    }

    const fn skipped(reason: String) -> Self {
        Self {
            action: WriteAction::Skipped,
            reason: Some(reason),
        }
    }
}

/// One upsert request.
pub struct MarkerBlockWrite<'a> {
    pub path: &'a Path,
    /// Full marker-delimited block (BEGIN..END inclusive, `\n` line endings).
    pub block: &'a str,
    /// Stable hash of the export payload, as embedded in the block header.
    pub content_hash: &'a str,
    /// Plan without touching disk.
    pub dry_run: bool,
}

/// Whether `path` already contains a DiffLore marker block. Used by callers to
/// decide if an empty export should refresh an existing block or skip creating
/// a new file.
#[must_use]
pub fn has_marker_block(path: &Path) -> bool {
    std::fs::read_to_string(path).is_ok_and(|content| content.contains(BEGIN_MARKER))
}

/// Insert or regenerate the DiffLore marker block in `req.path`.
///
/// IO failures surface as `Err`; policy refusals (symlink / corrupted
/// markers) come back as `Ok` with [`WriteAction::Skipped`] and a reason so a
/// multi-target caller can keep going and report.
pub fn upsert_marker_block(req: &MarkerBlockWrite<'_>) -> Result<WriteOutcome, CoreError> {
    let path = req.path;

    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Ok(WriteOutcome::skipped(format!(
                "{} is a symlink; refusing to write through it (it may point at another \
                 agent's instructions file). Re-point or remove the symlink and re-run.",
                path.display()
            )));
        }
        Ok(meta) if meta.is_dir() => {
            return Ok(WriteOutcome::skipped(format!(
                "{} is a directory, not a file",
                path.display()
            )));
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if !req.dry_run {
                write_atomic(path, format!("{}\n", req.block).as_bytes())?;
            }
            return Ok(WriteOutcome::ok(WriteAction::Created));
        }
        Err(e) => {
            return Err(CoreError::Internal(format!(
                "failed to stat {}: {e}",
                path.display()
            )));
        }
    }

    let existing = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) => {
            return Ok(WriteOutcome::skipped(format!(
                "could not read {} as UTF-8 text ({e}); refusing to rewrite it",
                path.display()
            )));
        }
    };

    let begin = existing.find(BEGIN_MARKER);
    let end = existing.find(END_MARKER);
    let new_content = match (begin, end) {
        (None, None) => {
            // No block yet: append, preserving the user's trailing-newline
            // state (mirrors the installer's GEMINI.md upsert).
            let sep = if existing.ends_with('\n') || existing.is_empty() {
                ""
            } else {
                "\n"
            };
            format!("{existing}{sep}\n{}\n", req.block)
        }
        (Some(begin_idx), Some(end_idx)) if end_idx > begin_idx => {
            let block_end = end_idx + END_MARKER.len();
            let existing_block = &existing[begin_idx..block_end];
            if embedded_content_hash(existing_block) == Some(req.content_hash) {
                return Ok(WriteOutcome::ok(WriteAction::Unchanged));
            }
            format!(
                "{}{}{}",
                &existing[..begin_idx],
                req.block,
                &existing[block_end..]
            )
        }
        _ => {
            return Ok(WriteOutcome::skipped(format!(
                "{} has a corrupted DiffLore section (BEGIN/END marker mismatch); \
                 fix or delete the `{BEGIN_MARKER}` / `{END_MARKER}` lines and re-run",
                path.display()
            )));
        }
    };

    if !req.dry_run {
        write_atomic(path, new_content.as_bytes())?;
    }
    Ok(WriteOutcome::ok(WriteAction::Updated))
}

/// Pull the `content-hash: <hex>` field out of an existing block header.
fn embedded_content_hash(block: &str) -> Option<&str> {
    let start = block.find(CONTENT_HASH_FIELD)? + CONTENT_HASH_FIELD.len();
    let rest = block[start..].trim_start();
    let hash = rest
        .split(|c: char| c.is_whitespace() || c == '|')
        .next()?
        .trim();
    (!hash.is_empty()).then_some(hash)
}

/// Atomic `std::fs::write`: temp file in the same directory, flushed, then
/// renamed over the target so a crash or power loss leaves the original file
/// intact rather than truncated. (The installer has a sibling helper; core
/// keeps its own copy because the installer's is crate-private to the CLI.)
fn write_atomic(path: &Path, contents: &[u8]) -> Result<(), CoreError> {
    let io_err = |stage: &str, e: std::io::Error| {
        CoreError::Internal(format!("{stage} {} failed: {e}", path.display()))
    };
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir).map_err(|e| io_err("creating parent dir for", e))?;
    let file_name = path.file_name().map_or_else(
        || "difflore-export".to_owned(),
        |n| n.to_string_lossy().into_owned(),
    );
    let tmp = dir.join(format!(".{file_name}.difflore-tmp-{}", std::process::id()));

    let write_result = std::fs::File::create(&tmp)
        .and_then(|mut file| {
            file.write_all(contents)?;
            // Best-effort flush to disk before the rename; some filesystems
            // reject fsync and that must not fail the export.
            let _ = file.sync_all();
            Ok(())
        })
        .and_then(|()| std::fs::rename(&tmp, path));
    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&tmp);
        return Err(io_err("writing", e));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block_with_hash(hash: &str, body: &str) -> String {
        format!(
            "{BEGIN_MARKER}\n<!-- generated-at: now | rules: 1 | {CONTENT_HASH_FIELD} {hash} | repo-scope: a/b -->\n{body}\n{END_MARKER}"
        )
    }

    fn upsert(path: &Path, hash: &str, body: &str, dry_run: bool) -> WriteOutcome {
        let block = block_with_hash(hash, body);
        upsert_marker_block(&MarkerBlockWrite {
            path,
            block: &block,
            content_hash: hash,
            dry_run,
        })
        .expect("upsert must not error")
    }

    #[test]
    fn creates_missing_file_with_block() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("AGENTS.md");
        let outcome = upsert(&path, "aaa111", "body", false);
        assert_eq!(outcome.action, WriteAction::Created);
        let written = std::fs::read_to_string(&path).expect("read");
        assert!(written.starts_with(BEGIN_MARKER));
        assert!(written.trim_end().ends_with(END_MARKER));
    }

    #[test]
    fn dry_run_never_touches_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("AGENTS.md");
        let outcome = upsert(&path, "aaa111", "body", true);
        assert_eq!(outcome.action, WriteAction::Created);
        assert!(!path.exists());
    }

    #[test]
    fn appends_block_to_existing_file_without_touching_user_content() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("AGENTS.md");
        std::fs::write(&path, "# My agents file\n\nuser notes\n").expect("seed");
        let outcome = upsert(&path, "aaa111", "body", false);
        assert_eq!(outcome.action, WriteAction::Updated);
        let written = std::fs::read_to_string(&path).expect("read");
        assert!(written.starts_with("# My agents file\n\nuser notes\n"));
        assert!(written.contains(BEGIN_MARKER));
    }

    #[test]
    fn rewrite_is_idempotent_via_content_hash() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("AGENTS.md");
        assert_eq!(
            upsert(&path, "aaa111", "body", false).action,
            WriteAction::Created
        );
        // Same hash -> short-circuit, even if cosmetic header text changed.
        let other_header = format!(
            "{BEGIN_MARKER}\n<!-- generated-at: LATER | rules: 1 | {CONTENT_HASH_FIELD} aaa111 -->\nbody\n{END_MARKER}"
        );
        let outcome = upsert_marker_block(&MarkerBlockWrite {
            path: &path,
            block: &other_header,
            content_hash: "aaa111",
            dry_run: false,
        })
        .expect("upsert");
        assert_eq!(outcome.action, WriteAction::Unchanged);
        // Different hash -> regenerated.
        assert_eq!(
            upsert(&path, "bbb222", "body2", false).action,
            WriteAction::Updated
        );
        let written = std::fs::read_to_string(&path).expect("read");
        assert!(written.contains("body2"));
        assert!(!written.contains("\nbody\n"));
    }

    #[test]
    fn update_preserves_user_content_around_the_block() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("AGENTS.md");
        let seeded = format!(
            "# Header kept\n\n{}\n\n## Footer kept\ntail line\n",
            block_with_hash("aaa111", "old body")
        );
        std::fs::write(&path, &seeded).expect("seed");
        let outcome = upsert(&path, "bbb222", "new body", false);
        assert_eq!(outcome.action, WriteAction::Updated);
        let written = std::fs::read_to_string(&path).expect("read");
        assert!(written.starts_with("# Header kept\n\n"));
        assert!(written.ends_with("\n\n## Footer kept\ntail line\n"));
        assert!(written.contains("new body"));
        assert!(!written.contains("old body"));
    }

    #[test]
    fn update_preserves_crlf_user_content_outside_the_block() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("CLAUDE.md");
        let seeded = format!(
            "# CRLF file\r\nuser line\r\n\n{}\n\r\ntrailer\r\n",
            block_with_hash("aaa111", "old body")
        );
        std::fs::write(&path, &seeded).expect("seed");
        let outcome = upsert(&path, "bbb222", "new body", false);
        assert_eq!(outcome.action, WriteAction::Updated);
        let written = std::fs::read_to_string(&path).expect("read");
        assert!(written.starts_with("# CRLF file\r\nuser line\r\n"));
        assert!(written.ends_with("\r\ntrailer\r\n"));
        assert!(written.contains("new body"));
    }

    #[test]
    fn refuses_corrupted_markers() {
        let dir = tempfile::tempdir().expect("tempdir");
        // BEGIN without END.
        let begin_only = dir.path().join("a.md");
        std::fs::write(&begin_only, format!("{BEGIN_MARKER}\nstuff\n")).expect("seed");
        let outcome = upsert(&begin_only, "aaa111", "body", false);
        assert_eq!(outcome.action, WriteAction::Skipped);
        assert!(
            outcome
                .reason
                .as_deref()
                .is_some_and(|r| r.contains("marker"))
        );
        assert_eq!(
            std::fs::read_to_string(&begin_only).expect("read"),
            format!("{BEGIN_MARKER}\nstuff\n"),
            "refusal must leave the file untouched"
        );

        // END without BEGIN.
        let end_only = dir.path().join("b.md");
        std::fs::write(&end_only, format!("notes\n{END_MARKER}\n")).expect("seed");
        assert_eq!(
            upsert(&end_only, "aaa111", "body", false).action,
            WriteAction::Skipped
        );

        // END before BEGIN.
        let inverted = dir.path().join("c.md");
        std::fs::write(&inverted, format!("{END_MARKER}\n{BEGIN_MARKER}\n")).expect("seed");
        assert_eq!(
            upsert(&inverted, "aaa111", "body", false).action,
            WriteAction::Skipped
        );
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlinked_target() {
        // Real fixture shape: CLAUDE.md symlinked to another agent's
        // instructions file; writing through it would edit that other file.
        let dir = tempfile::tempdir().expect("tempdir");
        let real = dir.path().join("copilot-instructions.md");
        std::fs::write(&real, "# copilot file\n").expect("seed");
        let link = dir.path().join("CLAUDE.md");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");

        let outcome = upsert(&link, "aaa111", "body", false);
        assert_eq!(outcome.action, WriteAction::Skipped);
        assert!(
            outcome
                .reason
                .as_deref()
                .is_some_and(|r| r.contains("symlink"))
        );
        assert_eq!(
            std::fs::read_to_string(&real).expect("read"),
            "# copilot file\n",
            "the symlink target must stay untouched"
        );
    }

    #[test]
    fn has_marker_block_detects_existing_section() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("AGENTS.md");
        assert!(!has_marker_block(&path));
        std::fs::write(&path, "no block\n").expect("seed");
        assert!(!has_marker_block(&path));
        std::fs::write(&path, block_with_hash("aaa111", "body")).expect("seed");
        assert!(has_marker_block(&path));
    }

    #[test]
    fn embedded_content_hash_parses_header_field() {
        let block = block_with_hash("deadbeef1234", "body");
        assert_eq!(embedded_content_hash(&block), Some("deadbeef1234"));
        assert_eq!(embedded_content_hash("no field here"), None);
    }
}
