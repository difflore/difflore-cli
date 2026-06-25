use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InjectionDropReason {
    ParseError,
    PreReadDisabled,
    NonMutatingTool,
    MissingTargetFile,
    RecentDuplicate,
    DbUnavailable,
    IndexUnavailable,
    RetrievalEmpty,
    RetrievalError,
    ShortCircuit,
    NoRepoScope,
    NotApplicable,
    Disabled,
    Unknown,
}

impl InjectionDropReason {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ParseError => "parse_error",
            Self::PreReadDisabled => "pre_read_disabled",
            Self::NonMutatingTool => "non_mutating_tool",
            Self::MissingTargetFile => "missing_target_file",
            Self::RecentDuplicate => "recent_duplicate",
            Self::DbUnavailable => "db_unavailable",
            Self::IndexUnavailable => "index_unavailable",
            Self::RetrievalEmpty => "retrieval_empty",
            Self::RetrievalError => "retrieval_error",
            Self::ShortCircuit => "short_circuit",
            Self::NoRepoScope => "no_repo_scope",
            Self::NotApplicable => "not_applicable",
            Self::Disabled => "disabled",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InjectionEntry {
    ts_ms: i64,
    path: String,
    rules_injected: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    file_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    drop_reason: Option<InjectionDropReason>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InjectionLog {
    version: u32,
    entries: Vec<InjectionEntry>,
}

#[derive(Debug, Clone, Default)]
pub struct InjectionPathSummary {
    pub count_24h: usize,
    pub by_path: BTreeMap<String, usize>,
    pub injected_by_path: BTreeMap<String, usize>,
    pub dropped_by_reason: BTreeMap<String, usize>,
    pub total_rules_injected: usize,
    pub path: Option<PathBuf>,
    pub detail: Option<String>,
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

fn log_path() -> Option<PathBuf> {
    crate::infra::paths::data_home()
        .ok()
        .map(|dir| dir.join("injection-paths.json"))
}

pub fn record(path_name: &str, rules_injected: usize, file_path: Option<&str>) {
    record_with_reason(path_name, rules_injected, file_path, None);
}

pub fn record_with_reason(
    path_name: &str,
    rules_injected: usize,
    file_path: Option<&str>,
    drop_reason: Option<InjectionDropReason>,
) {
    let Some(path) = log_path() else {
        return;
    };
    let now = now_ms();
    let cutoff = now.saturating_sub(24 * 60 * 60 * 1000);
    let mut log = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str::<InjectionLog>(&raw).ok())
        .unwrap_or(InjectionLog {
            version: 1,
            entries: Vec::new(),
        });
    log.entries.retain(|entry| entry.ts_ms >= cutoff);
    log.entries.push(InjectionEntry {
        ts_ms: now,
        path: path_name.to_owned(),
        rules_injected,
        file_path: file_path.map(|p| {
            if p.len() > 200 {
                p.chars().take(200).collect()
            } else {
                p.to_owned()
            }
        }),
        drop_reason: if rules_injected == 0 {
            drop_reason
        } else {
            None
        },
    });
    if log.entries.len() > 2_000 {
        let keep_from = log.entries.len().saturating_sub(2_000);
        log.entries = log.entries.split_off(keep_from);
    }
    if let Ok(json) = serde_json::to_string_pretty(&log) {
        // Atomic write: a torn `fs::write` would corrupt the JSON and make the
        // next read drop the whole 24h window.
        let _ = crate::infra::files::write_atomic(&path, json.as_bytes());
    }
}

pub fn summary_24h() -> InjectionPathSummary {
    let Some(path) = log_path() else {
        return InjectionPathSummary {
            detail: Some("could not resolve DIFFLORE_HOME".into()),
            ..InjectionPathSummary::default()
        };
    };
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return InjectionPathSummary {
            path: Some(path),
            detail: Some("no injection path log yet".into()),
            ..InjectionPathSummary::default()
        };
    };
    let log = match serde_json::from_str::<InjectionLog>(&raw) {
        Ok(log) => log,
        Err(e) => {
            return InjectionPathSummary {
                path: Some(path),
                detail: Some(format!("injection path log is unreadable: {e}")),
                ..InjectionPathSummary::default()
            };
        }
    };
    let cutoff = now_ms().saturating_sub(24 * 60 * 60 * 1000);
    let mut summary = InjectionPathSummary {
        path: Some(path),
        ..InjectionPathSummary::default()
    };
    summarize_entries_into(
        &mut summary,
        log.entries
            .into_iter()
            .filter(|entry| entry.ts_ms >= cutoff),
    );
    summary
}

fn summarize_entries_into(
    summary: &mut InjectionPathSummary,
    entries: impl IntoIterator<Item = InjectionEntry>,
) {
    for entry in entries {
        summary.count_24h += 1;
        *summary.by_path.entry(entry.path.clone()).or_insert(0) += 1;
        if entry.rules_injected > 0 {
            *summary.injected_by_path.entry(entry.path).or_insert(0) += 1;
            summary.total_rules_injected += entry.rules_injected;
        } else if let Some(reason) = entry.drop_reason {
            *summary
                .dropped_by_reason
                .entry(reason.as_str().to_owned())
                .or_insert(0) += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_counts_structured_drop_reasons() {
        let mut summary = InjectionPathSummary::default();
        summarize_entries_into(
            &mut summary,
            vec![
                InjectionEntry {
                    ts_ms: 1,
                    path: "hook".to_owned(),
                    rules_injected: 0,
                    file_path: Some("src/lib.rs".to_owned()),
                    drop_reason: Some(InjectionDropReason::RecentDuplicate),
                },
                InjectionEntry {
                    ts_ms: 2,
                    path: "hook".to_owned(),
                    rules_injected: 2,
                    file_path: Some("src/lib.rs".to_owned()),
                    drop_reason: Some(InjectionDropReason::RetrievalEmpty),
                },
                InjectionEntry {
                    ts_ms: 3,
                    path: "mcp_tool".to_owned(),
                    rules_injected: 0,
                    file_path: None,
                    drop_reason: Some(InjectionDropReason::NoRepoScope),
                },
            ],
        );

        assert_eq!(summary.count_24h, 3);
        assert_eq!(summary.total_rules_injected, 2);
        assert_eq!(
            summary.dropped_by_reason.get("recent_duplicate").copied(),
            Some(1)
        );
        assert_eq!(
            summary.dropped_by_reason.get("no_repo_scope").copied(),
            Some(1)
        );
        assert!(
            !summary.dropped_by_reason.contains_key("retrieval_empty"),
            "successful injections must not also count a stale drop reason"
        );
    }
}
