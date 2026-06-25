//! Real-time "memory pipeline" event stream for local activity surfaces.
//!
//! Each retrieval / injection / reinforcement emits a small typed record to a
//! JSONL file at `$DIFFLORE_HOME/activity.jsonl`, which local status/activity
//! consumers can tail-read to render the last N events.
//!
//! A JSONL file (rather than an in-process channel) is used because the MCP
//! server and CLI fix command run as separate processes; a file is the
//! cheapest cross-process bus that survives CLI restarts with no daemon.
//! Capped at 1000 lines via tail-rotation.

use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

/// Hard cap on retained events. A write that would exceed this truncates the
/// file to the last `MAX_EVENTS - 1` lines plus the new one.
pub const MAX_EVENTS: usize = 1000;
const LOCK_RETRY_DELAY: Duration = Duration::from_millis(5);
const LOCK_WAIT_TIMEOUT: Duration = Duration::from_millis(500);
const STALE_LOCK_AGE: Duration = Duration::from_secs(30);

/// One line in `activity.jsonl`. Flat layout (payload fields alongside `kind`)
/// for easier ad-hoc `jq` inspection.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ActivityEvent {
    pub ts_ms: i64,
    #[serde(flatten)]
    pub payload: ActivityPayload,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ActivityPayload {
    /// One rule surfaced by retrieval, emitted once per top-K rule per MCP
    /// call.
    RuleRecalled {
        rule_id: String,
        rule_title: String,
        score: f32,
        took_ms: u64,
    },
    /// Aggregate per-MCP-call signal: how many rules were placed in the
    /// agent's context window, plus a scannable intent summary.
    RuleInjected {
        rule_count: u32,
        prompt_chars: u32,
        intent_summary: String,
    },
    /// Confidence shift on a single rule. `reason` is one of
    /// `recalled`/`cited`/`fix_accepted`/`fix_rejected`. Stored as `String`
    /// (not `&'static str`) since serde flatten is incompatible with 'static
    /// refs on round-trip.
    RuleReinforced {
        rule_id: String,
        rule_title: String,
        prev_strength: f32,
        new_strength: f32,
        reason: String,
    },
    /// ANN / hybrid retrieval pass result.
    RetrievalEmbedding { hits: u32, took_ms: u64 },
    /// Cloud-managed embedding cap hit, emitted when the cloud returns
    /// `409 embed_cap_reached`. `cap` is the tier ceiling; `used` is the
    /// cloud-reported value.
    EmbedCapReached { cap: u32, used: u32 },
    /// Semantic embedding provider fell back to local SHA1 after retry.
    /// `reason` is a short sanitized bucket such as "network", "scope",
    /// "cap", or "empty"; raw provider errors are intentionally not
    /// persisted because they can contain URLs or request context.
    EmbeddingFallback { reason: String },
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn log_path() -> Option<PathBuf> {
    crate::infra::paths::data_home()
        .ok()
        .map(|dir| dir.join("activity.jsonl"))
}

/// Append `payload` to the activity log. Best-effort: any IO failure is
/// swallowed so telemetry never breaks the caller. When the file would
/// exceed `MAX_EVENTS` lines, rotates by tail-keeping the most recent.
pub fn record(payload: ActivityPayload) {
    let Some(path) = log_path() else {
        return;
    };
    let event = ActivityEvent {
        ts_ms: now_ms(),
        payload,
    };
    let Ok(line) = serde_json::to_string(&event) else {
        return;
    };
    let _ = append_with_cap(&path, &line);
}

fn append_with_cap(path: &Path, line: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let _lock = acquire_log_lock(path)?;

    // Read once and reuse for the rotate branch: a second read could fail
    // (file deleted/renamed between calls) and silently truncate the log to
    // just the new entry.
    let existing = fs::read_to_string(path).unwrap_or_default();
    if existing.lines().count() >= MAX_EVENTS {
        // Tail-rotate: keep the last MAX_EVENTS-1 lines, then append.
        let mut kept: Vec<&str> = existing.lines().collect();
        let drop = kept.len().saturating_sub(MAX_EVENTS - 1);
        kept.drain(..drop);
        let mut out = kept.join("\n");
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(line);
        out.push('\n');
        fs::write(path, out)?;
        return Ok(());
    }

    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(f, "{line}")?;
    Ok(())
}

struct ActivityLogLock {
    path: PathBuf,
}

impl Drop for ActivityLogLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn lock_path(path: &Path) -> PathBuf {
    let mut lock = path.as_os_str().to_owned();
    lock.push(".lock");
    PathBuf::from(lock)
}

fn acquire_log_lock(path: &Path) -> io::Result<ActivityLogLock> {
    let path = lock_path(path);
    let started = Instant::now();
    loop {
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                let _ = writeln!(file, "pid={}", std::process::id());
                return Ok(ActivityLogLock { path });
            }
            // `AlreadyExists` is normal contention. On Windows, a lock file
            // held open — or pending deletion after another writer's `Drop`
            // removed it — surfaces as `PermissionDenied` (ERROR_ACCESS_DENIED)
            // on `create_new`, not `AlreadyExists`. Treat both as transient
            // contention and retry within the timeout window; otherwise
            // concurrent writers race-drop activity events on Windows.
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::AlreadyExists | io::ErrorKind::PermissionDenied
                ) =>
            {
                // Only an existing (not pending-delete) lock can be stale; the
                // metadata read is meaningless in the PermissionDenied case.
                if e.kind() == io::ErrorKind::AlreadyExists && lock_is_stale(&path) {
                    let _ = fs::remove_file(&path);
                    continue;
                }
                if started.elapsed() >= LOCK_WAIT_TIMEOUT {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "timed out waiting for activity log lock",
                    ));
                }
                thread::sleep(LOCK_RETRY_DELAY);
            }
            Err(e) => return Err(e),
        }
    }
}

fn lock_is_stale(path: &Path) -> bool {
    fs::metadata(path)
        .and_then(|meta| meta.modified())
        .and_then(|modified| modified.elapsed().map_err(io::Error::other))
        .is_ok_and(|age| age > STALE_LOCK_AGE)
}

fn parse_events(raw: &str) -> Vec<ActivityEvent> {
    serde_json::Deserializer::from_str(raw)
        .into_iter::<ActivityEvent>()
        .filter_map(Result::ok)
        .collect()
}

/// Read the last `n` events from disk, newest-first. Best-effort: a
/// missing or unreadable file yields an empty Vec so callers can render an
/// empty state without erroring.
pub fn tail(n: usize) -> Vec<ActivityEvent> {
    let Some(path) = log_path() else {
        return Vec::new();
    };
    let Ok(raw) = fs::read_to_string(&path) else {
        return Vec::new();
    };
    parse_events(&raw).into_iter().rev().take(n).collect()
}

/// Test-friendly variant of `record` that writes to an explicit path
/// instead of `$DIFFLORE_HOME/activity.jsonl`. Production callers should
/// use `record`; tests use this to avoid a shared-env-var race against
/// the workspace-wide `shared_test_home()`.
pub fn record_to(path: &Path, payload: ActivityPayload) -> io::Result<()> {
    let event = ActivityEvent {
        ts_ms: now_ms(),
        payload,
    };
    let line =
        serde_json::to_string(&event).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    append_with_cap(path, &line)
}

/// Test-friendly variant of `tail`.
pub fn tail_from(path: &Path, n: usize) -> Vec<ActivityEvent> {
    let Ok(raw) = fs::read_to_string(path) else {
        return Vec::new();
    };
    parse_events(&raw).into_iter().rev().take(n).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writer_caps_at_max_events() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("activity.jsonl");
        for i in 0..=MAX_EVENTS {
            record_to(
                &path,
                ActivityPayload::RuleRecalled {
                    rule_id: format!("r{i}"),
                    rule_title: "t".into(),
                    score: 0.1,
                    took_ms: 1,
                },
            )
            .unwrap();
        }
        let events = tail_from(&path, MAX_EVENTS + 50);
        assert_eq!(
            events.len(),
            MAX_EVENTS,
            "file should be capped at {MAX_EVENTS} entries"
        );
        if let ActivityPayload::RuleRecalled { rule_id, .. } = &events[0].payload {
            assert_eq!(rule_id, &format!("r{MAX_EVENTS}"));
        } else {
            panic!("unexpected payload kind on top");
        }
    }

    #[test]
    fn tail_returns_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("activity.jsonl");
        record_to(
            &path,
            ActivityPayload::RuleInjected {
                rule_count: 1,
                prompt_chars: 10,
                intent_summary: "first".into(),
            },
        )
        .unwrap();
        record_to(
            &path,
            ActivityPayload::RuleInjected {
                rule_count: 2,
                prompt_chars: 20,
                intent_summary: "second".into(),
            },
        )
        .unwrap();
        let events = tail_from(&path, 10);
        assert_eq!(events.len(), 2);
        if let ActivityPayload::RuleInjected { intent_summary, .. } = &events[0].payload {
            assert_eq!(intent_summary, "second");
        } else {
            panic!("expected RuleInjected on top");
        }
    }

    #[test]
    fn embedding_fallback_round_trips_sanitized_reason() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("activity.jsonl");
        record_to(
            &path,
            ActivityPayload::EmbeddingFallback {
                reason: "network".into(),
            },
        )
        .unwrap();
        let events = tail_from(&path, 10);
        assert_eq!(events.len(), 1);
        if let ActivityPayload::EmbeddingFallback { reason } = &events[0].payload {
            assert_eq!(reason, "network");
        } else {
            panic!("expected EmbeddingFallback on top");
        }
    }

    #[test]
    fn tail_recovers_concatenated_json_objects() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("activity.jsonl");
        let first = ActivityEvent {
            ts_ms: 1,
            payload: ActivityPayload::RuleInjected {
                rule_count: 1,
                prompt_chars: 10,
                intent_summary: "first".into(),
            },
        };
        let second = ActivityEvent {
            ts_ms: 2,
            payload: ActivityPayload::RuleInjected {
                rule_count: 2,
                prompt_chars: 20,
                intent_summary: "second".into(),
            },
        };
        fs::write(
            &path,
            format!(
                "{}{}",
                serde_json::to_string(&first).unwrap(),
                serde_json::to_string(&second).unwrap()
            ),
        )
        .unwrap();

        let events = tail_from(&path, 10);
        assert_eq!(events, vec![second, first]);
    }

    #[test]
    fn concurrent_writes_remain_valid_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("activity.jsonl");
        let mut handles = Vec::new();
        for worker in 0..8 {
            let path = path.clone();
            handles.push(thread::spawn(move || {
                for i in 0..25 {
                    record_to(
                        &path,
                        ActivityPayload::RuleRecalled {
                            rule_id: format!("r-{worker}-{i}"),
                            rule_title: "title".into(),
                            score: 0.1,
                            took_ms: 1,
                        },
                    )
                    .unwrap();
                }
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }

        let raw = fs::read_to_string(&path).unwrap();
        assert_eq!(raw.lines().count(), 200);
        for line in raw.lines() {
            serde_json::from_str::<ActivityEvent>(line).unwrap();
        }
    }
}
