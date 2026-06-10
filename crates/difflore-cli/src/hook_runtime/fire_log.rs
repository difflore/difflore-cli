use std::path::PathBuf;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct HookFireEntry {
    ts_ms: i64,
    client: String,
    event: String,
    /// Number of rules surfaced into the agent context for this fire.
    /// Defaults to 0 for log entries predating the instrumentation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rules_injected: Option<usize>,
    /// File path the agent was about to read/edit, if known. Truncated to 200
    /// chars to keep the JSON log small.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    file_path: Option<String>,
    /// Per-fire wall-clock spent inside the `DiffLore` hook handler. `None` for
    /// log entries predating the instrumentation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    elapsed_ms: Option<i64>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct HookFireLog {
    version: u32,
    entries: Vec<HookFireEntry>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct HookFireSummary {
    pub(crate) count_24h: usize,
    pub(crate) by_client: std::collections::BTreeMap<String, usize>,
    pub(crate) by_event: std::collections::BTreeMap<String, usize>,
    /// Number of 24h fires that surfaced ≥1 rule. When `injected_fires` ≪
    /// `count_24h`, the corpus is cold and rule coverage / `file_patterns`
    /// need attention.
    pub(crate) injected_fires: usize,
    /// Total rules surfaced across all 24h fires.
    pub(crate) total_rules_injected: usize,
    /// Median per-fire wall-clock in the hook handler over timed fires.
    /// `None` until at least one timed fire lands.
    pub(crate) median_elapsed_ms: Option<i64>,
    /// Number of 24h fires that carry timing data (the median's sample size).
    pub(crate) timed_fires: usize,
    pub(crate) path: Option<PathBuf>,
    pub(crate) detail: Option<String>,
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

fn hook_fire_log_path() -> Option<PathBuf> {
    difflore_core::paths::data_home()
        .ok()
        .map(|dir| dir.join("hook-fires.json"))
}

fn remember_hook_fire_full(
    client: &str,
    event: &str,
    rules_injected: Option<usize>,
    file_path: Option<String>,
    elapsed_ms: Option<i64>,
) {
    difflore_core::injection_log::record("hook", rules_injected.unwrap_or(0), file_path.as_deref());
    let Some(path) = hook_fire_log_path() else {
        return;
    };
    let cutoff = now_ms().saturating_sub(24 * 60 * 60 * 1000);
    let mut log = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str::<HookFireLog>(&raw).ok())
        .unwrap_or(HookFireLog {
            version: 1,
            entries: Vec::new(),
        });
    log.entries.retain(|entry| entry.ts_ms >= cutoff);
    log.entries.push(HookFireEntry {
        ts_ms: now_ms(),
        client: client.to_owned(),
        event: event.to_owned(),
        rules_injected,
        file_path: file_path.map(|p| {
            if p.len() > 200 {
                p.chars().take(200).collect()
            } else {
                p
            }
        }),
        elapsed_ms,
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

pub(super) fn remember_hook_fire_maybe_deferred(
    client: String,
    event: String,
    rules_injected: Option<usize>,
    file_path: Option<String>,
    elapsed_ms: Option<i64>,
    defer: bool,
) {
    if defer {
        let _ = std::thread::Builder::new()
            .name("difflore-hook-log".to_owned())
            .spawn(move || {
                remember_hook_fire_full(&client, &event, rules_injected, file_path, elapsed_ms);
            });
    } else {
        remember_hook_fire_full(&client, &event, rules_injected, file_path, elapsed_ms);
    }
}

pub(crate) fn hook_fire_summary_24h() -> HookFireSummary {
    let Some(path) = hook_fire_log_path() else {
        return HookFireSummary {
            detail: Some("could not resolve DIFFLORE_HOME".into()),
            ..HookFireSummary::default()
        };
    };
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return HookFireSummary {
            path: Some(path),
            detail: Some("no hook fire log yet".into()),
            ..HookFireSummary::default()
        };
    };
    let log = match serde_json::from_str::<HookFireLog>(&raw) {
        Ok(log) => log,
        Err(e) => {
            return HookFireSummary {
                path: Some(path),
                detail: Some(format!("hook fire log is unreadable: {e}")),
                ..HookFireSummary::default()
            };
        }
    };
    let cutoff = now_ms().saturating_sub(24 * 60 * 60 * 1000);
    let mut summary = HookFireSummary {
        path: Some(path),
        ..HookFireSummary::default()
    };
    let mut elapsed_samples: Vec<i64> = Vec::new();
    for entry in log
        .entries
        .into_iter()
        .filter(|entry| entry.ts_ms >= cutoff)
    {
        summary.count_24h += 1;
        *summary.by_client.entry(entry.client).or_insert(0) += 1;
        *summary.by_event.entry(entry.event).or_insert(0) += 1;
        if let Some(n) = entry.rules_injected
            && n > 0
        {
            summary.injected_fires += 1;
            summary.total_rules_injected += n;
        }
        if let Some(ms) = entry.elapsed_ms {
            elapsed_samples.push(ms);
        }
    }
    if !elapsed_samples.is_empty() {
        elapsed_samples.sort_unstable();
        let mid = elapsed_samples.len() / 2;
        // Even-count median: average the two middle values. Cheap to
        // implement and matches what users intuit from "median".
        let median = if elapsed_samples.len().is_multiple_of(2) {
            i64::midpoint(elapsed_samples[mid - 1], elapsed_samples[mid])
        } else {
            elapsed_samples[mid]
        };
        summary.median_elapsed_ms = Some(median);
        summary.timed_fires = elapsed_samples.len();
    }
    summary
}

#[cfg(test)]
mod median_elapsed_tests {
    use super::*;

    fn entry(ts: i64, elapsed: Option<i64>) -> HookFireEntry {
        HookFireEntry {
            ts_ms: ts,
            client: "claude-code".into(),
            event: "post_tool_use".into(),
            rules_injected: None,
            file_path: None,
            elapsed_ms: elapsed,
        }
    }

    fn summarise(entries: Vec<HookFireEntry>) -> HookFireSummary {
        let mut summary = HookFireSummary::default();
        let mut samples = Vec::new();
        for e in entries {
            summary.count_24h += 1;
            if let Some(ms) = e.elapsed_ms {
                samples.push(ms);
            }
        }
        if !samples.is_empty() {
            samples.sort_unstable();
            let mid = samples.len() / 2;
            let median = if samples.len() % 2 == 0 {
                i64::midpoint(samples[mid - 1], samples[mid])
            } else {
                samples[mid]
            };
            summary.median_elapsed_ms = Some(median);
            summary.timed_fires = samples.len();
        }
        summary
    }

    #[test]
    fn summarise_computes_median_only_over_timed_entries() {
        // (timings, expected_median, expected_timed_fires, expected_count_24h)
        // Each row exercises one branch: odd, even, all-untimed, mixed.
        type SummaryCase<'a> = (&'a [Option<i64>], Option<i64>, usize, usize);
        let cases: &[SummaryCase<'_>] = &[
            (&[Some(10), Some(50), Some(30)], Some(30), 3, 3), // odd → middle
            (&[Some(10), Some(40), Some(20), Some(50)], Some(30), 4, 4), // even → mean of middle pair
            (&[None, None], None, 0, 2),                                 // no timings → None
            (&[None, Some(100), None, Some(300)], Some(200), 2, 4), // mixed → only timed contribute
        ];
        for (timings, want_median, want_timed, want_count) in cases {
            let entries = timings
                .iter()
                .enumerate()
                .map(|(i, t)| entry(i as i64 + 1, *t))
                .collect();
            let s = summarise(entries);
            assert_eq!(s.median_elapsed_ms, *want_median, "median for {timings:?}");
            assert_eq!(s.timed_fires, *want_timed, "timed for {timings:?}");
            assert_eq!(s.count_24h, *want_count, "count for {timings:?}");
        }
    }
}
