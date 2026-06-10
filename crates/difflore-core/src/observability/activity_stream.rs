//! Real-time "memory pipeline" event stream powering the TUI's Activity tab.
//!
//! Each retrieval / injection / reinforcement emits a small typed record to a
//! JSONL file at `$DIFFLORE_HOME/activity.jsonl`, which the TUI tail-reads to
//! render the last N events.
//!
//! A JSONL file (rather than an in-process channel) is used because the MCP
//! server, CLI fix command, and TUI run as separate processes; a file is the
//! cheapest cross-process bus that survives a TUI restart with no daemon.
//! Capped at 1000 lines via tail-rotation.

use std::fs;
use std::io::Write;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Hard cap on retained events. A write that would exceed this truncates the
/// file to the last `MAX_EVENTS - 1` lines plus the new one.
pub const MAX_EVENTS: usize = 1000;

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
    crate::paths::data_home()
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

fn append_with_cap(path: &std::path::Path, line: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

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

    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(f, "{line}")?;
    Ok(())
}

/// Read the last `n` events from disk, newest-first. Best-effort: a
/// missing or unreadable file yields an empty Vec so the TUI can render
/// an empty state without erroring.
pub fn tail(n: usize) -> Vec<ActivityEvent> {
    let Some(path) = log_path() else {
        return Vec::new();
    };
    let Ok(raw) = fs::read_to_string(&path) else {
        return Vec::new();
    };
    raw.lines()
        .rev()
        .take(n)
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// Test-friendly variant of `record` that writes to an explicit path
/// instead of `$DIFFLORE_HOME/activity.jsonl`. Production callers should
/// use `record`; tests use this to avoid a shared-env-var race against
/// the workspace-wide `shared_test_home()`.
pub fn record_to(path: &std::path::Path, payload: ActivityPayload) -> std::io::Result<()> {
    let event = ActivityEvent {
        ts_ms: now_ms(),
        payload,
    };
    let line = serde_json::to_string(&event)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    append_with_cap(path, &line)
}

/// Test-friendly variant of `tail`.
pub fn tail_from(path: &std::path::Path, n: usize) -> Vec<ActivityEvent> {
    let Ok(raw) = fs::read_to_string(path) else {
        return Vec::new();
    };
    raw.lines()
        .rev()
        .take(n)
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
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
}
