use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InjectionEntry {
    ts_ms: i64,
    path: String,
    rules_injected: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    file_path: Option<String>,
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
    crate::paths::data_home()
        .ok()
        .map(|dir| dir.join("injection-paths.json"))
}

pub fn record(path_name: &str, rules_injected: usize, file_path: Option<&str>) {
    let Some(path) = log_path() else {
        return;
    };
    let cutoff = now_ms().saturating_sub(24 * 60 * 60 * 1000);
    let mut log = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str::<InjectionLog>(&raw).ok())
        .unwrap_or(InjectionLog {
            version: 1,
            entries: Vec::new(),
        });
    log.entries.retain(|entry| entry.ts_ms >= cutoff);
    log.entries.push(InjectionEntry {
        ts_ms: now_ms(),
        path: path_name.to_owned(),
        rules_injected,
        file_path: file_path.map(|p| {
            if p.len() > 200 {
                p.chars().take(200).collect()
            } else {
                p.to_owned()
            }
        }),
    });
    if log.entries.len() > 2_000 {
        let keep_from = log.entries.len().saturating_sub(2_000);
        log.entries = log.entries.split_off(keep_from);
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(&log) {
        let _ = std::fs::write(path, json);
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
    for entry in log
        .entries
        .into_iter()
        .filter(|entry| entry.ts_ms >= cutoff)
    {
        summary.count_24h += 1;
        *summary.by_path.entry(entry.path.clone()).or_insert(0) += 1;
        if entry.rules_injected > 0 {
            *summary.injected_by_path.entry(entry.path).or_insert(0) += 1;
            summary.total_rules_injected += entry.rules_injected;
        }
    }
    summary
}
