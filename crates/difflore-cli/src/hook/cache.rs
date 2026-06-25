//! Cache that suppresses repeated rule injections for the same file and
//! event kind within a short window. Advisory only: any IO or parse
//! failure returns "do not skip" so hooks keep working.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

const DEFAULT_TTL_MS: i64 = 120_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheFile {
    version: u32,
    entries: BTreeMap<String, CacheEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEntry {
    ts_ms: i64,
    rules_injected: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    signal_hash: Option<String>,
}

pub(crate) fn should_skip_recent_with_signal(
    file_path: &str,
    purpose: &str,
    signal: Option<&str>,
) -> bool {
    let project_root = difflore_core::infra::db::current_project_root();
    let project_hash = difflore_core::infra::db::project_hash_from_root(&project_root);
    should_skip_recent_for_project_hash_with_signal(file_path, purpose, &project_hash, signal)
}

pub(crate) fn should_skip_recent_for_project_hash_with_signal(
    file_path: &str,
    purpose: &str,
    project_hash: &str,
    signal: Option<&str>,
) -> bool {
    let ttl = ttl_ms();
    if ttl <= 0 {
        return false;
    }
    let Some(path) = cache_path() else {
        return false;
    };
    let Ok(raw) = fs::read_to_string(path) else {
        return false;
    };
    let Ok(cache) = serde_json::from_str::<CacheFile>(&raw) else {
        return false;
    };
    let key = cache_key_for_project_hash(file_path, purpose, project_hash);
    let Some(entry) = cache.entries.get(&key) else {
        return false;
    };
    cache_entry_should_skip(entry, ttl, now_ms(), signal_hash(signal))
}

pub(crate) fn remember_injection(
    file_path: &str,
    purpose: &str,
    rules_injected: usize,
    signal: Option<&str>,
) {
    let project_root = difflore_core::infra::db::current_project_root();
    let project_hash = difflore_core::infra::db::project_hash_from_root(&project_root);
    remember_injection_for_project_hash_with_signal(
        file_path,
        purpose,
        rules_injected,
        &project_hash,
        signal,
    );
}

pub fn remember_injection_for_project_hash_with_signal(
    file_path: &str,
    purpose: &str,
    rules_injected: usize,
    project_hash: &str,
    signal: Option<&str>,
) {
    let Some(path) = cache_path() else {
        return;
    };
    let mut cache = fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str::<CacheFile>(&raw).ok())
        .unwrap_or_else(|| CacheFile {
            version: 1,
            entries: BTreeMap::new(),
        });
    let now = now_ms();
    let ttl = ttl_ms().max(DEFAULT_TTL_MS);
    cache
        .entries
        .retain(|_, entry| now.saturating_sub(entry.ts_ms) <= ttl * 4);
    cache.entries.insert(
        cache_key_for_project_hash(file_path, purpose, project_hash),
        CacheEntry {
            ts_ms: now,
            rules_injected,
            signal_hash: signal_hash(signal),
        },
    );
    if let Ok(json) = serde_json::to_string_pretty(&cache) {
        // Atomic write so a torn `fs::write` can't corrupt the dedup cache and
        // make the next hook-path read fail to parse it.
        let _ = difflore_core::infra::files::write_atomic(&path, json.as_bytes());
    }
}

fn cache_key_for_project_hash(file_path: &str, purpose: &str, project_hash: &str) -> String {
    let normalized = file_path.trim().replace('\\', "/");
    format!("{project_hash}:{purpose}:{normalized}")
}

fn cache_entry_should_skip(
    entry: &CacheEntry,
    ttl: i64,
    now: i64,
    current_signal_hash: Option<String>,
) -> bool {
    if now.saturating_sub(entry.ts_ms) >= ttl || entry.rules_injected == 0 {
        return false;
    }
    match current_signal_hash {
        Some(current) => entry.signal_hash.as_deref() == Some(current.as_str()),
        None => true,
    }
}

fn signal_hash(signal: Option<&str>) -> Option<String> {
    let signal = signal?.trim();
    if signal.is_empty() {
        return None;
    }
    Some(difflore_core::infra::crypto::sha256_block_hex(
        signal.as_bytes(),
    ))
}

fn cache_path() -> Option<PathBuf> {
    difflore_core::infra::paths::data_home()
        .ok()
        .map(|dir| dir.join("hook-cache.json"))
}

fn ttl_ms() -> i64 {
    difflore_core::infra::env::var(difflore_core::infra::env::DIFFLORE_HOOK_CACHE_TTL_MS)
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(DEFAULT_TTL_MS)
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(i64::MAX, |d| d.as_millis() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_aware_skip_only_suppresses_unchanged_context() {
        let entry = CacheEntry {
            ts_ms: 1_000,
            rules_injected: 2,
            signal_hash: signal_hash(Some("post-edit\n+same")),
        };

        assert!(cache_entry_should_skip(
            &entry,
            120_000,
            2_000,
            signal_hash(Some("post-edit\n+same")),
        ));
        assert!(
            !cache_entry_should_skip(
                &entry,
                120_000,
                2_000,
                signal_hash(Some("post-edit\n+changed")),
            ),
            "a changed diff/query must reopen the retrieval gate"
        );
    }

    #[test]
    fn legacy_skip_without_signal_preserves_file_ttl_behavior() {
        let entry = CacheEntry {
            ts_ms: 1_000,
            rules_injected: 1,
            signal_hash: None,
        };

        assert!(cache_entry_should_skip(&entry, 120_000, 2_000, None));
        assert!(
            !cache_entry_should_skip(&entry, 120_000, 2_000, signal_hash(Some("new signal")),),
            "once callers provide a signal, old file-only cache entries must not suppress changed content"
        );
    }
}
