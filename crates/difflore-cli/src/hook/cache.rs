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
}

pub fn should_skip_recent(file_path: &str, purpose: &str) -> bool {
    let project_root = difflore_core::infra::db::current_project_root();
    let project_hash = difflore_core::infra::db::project_hash_from_root(&project_root);
    should_skip_recent_for_project_hash(file_path, purpose, &project_hash)
}

pub fn should_skip_recent_for_project_hash(
    file_path: &str,
    purpose: &str,
    project_hash: &str,
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
    now_ms().saturating_sub(entry.ts_ms) < ttl && entry.rules_injected > 0
}

pub fn remember_injection(file_path: &str, purpose: &str, rules_injected: usize) {
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
        cache_key(file_path, purpose),
        CacheEntry {
            ts_ms: now,
            rules_injected,
        },
    );
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(&cache) {
        let _ = fs::write(path, json);
    }
}

fn cache_key(file_path: &str, purpose: &str) -> String {
    let project_root = difflore_core::infra::db::current_project_root();
    let project_hash = difflore_core::infra::db::project_hash_from_root(&project_root);
    cache_key_for_project_hash(file_path, purpose, &project_hash)
}

fn cache_key_for_project_hash(file_path: &str, purpose: &str, project_hash: &str) -> String {
    let normalized = file_path.trim().replace('\\', "/");
    format!("{project_hash}:{purpose}:{normalized}")
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
        .map_or(0, |d| d.as_millis() as i64)
}
