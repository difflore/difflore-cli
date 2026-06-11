//! Fast-start gate that amortises first-command checks across the CLI.
//!
//! Every `difflore` invocation used to pay the cost of:
//!   - opening the `SQLite` pool + running migrations
//!   - reading provider config (auth check probe)
//!   - (if logged in) pinging the cloud for reachability
//!
//! On an interactive shell this is ~200–600 ms wasted per command when
//! nothing has actually changed. `ensure_ready` keeps a tiny JSON file
//! (`~/.difflore/startup-cache.json`) with the last-known-good timestamp
//! for each check and short-circuits when all timestamps are fresh.
//!
//! TTL is deliberately short (5 min) so genuine outages surface within a
//! few invocations rather than being masked for the whole session.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::fs;

use crate::error::CoreError;
use crate::infra::paths;

/// Five minutes — balances "skip the probe on the next N commands after a
/// fresh one" (common case) with "surface real drift within seconds" (when
/// the user just logged out of the cloud, swapped provider keys, etc.).
pub const STARTUP_TTL_MINUTES: i64 = 5;

/// Serialized as `~/.difflore/startup-cache.json`. Every field except
/// `version` and `migrations_applied_at` is optional so the struct can
/// round-trip cleanly across future schema additions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartupStatus {
    pub version: String,
    pub migrations_applied_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_ok_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud_ok_at: Option<DateTime<Utc>>,
}

impl StartupStatus {
    /// Are all probe timestamps newer than `now - TTL`? `provider_ok_at`
    /// and `cloud_ok_at` are considered fresh when present and recent;
    /// `None` means the probe was never run (e.g. not logged in yet) and
    /// is NOT treated as stale — we don't want to re-ping the cloud on
    /// every invocation just because the user isn't signed in.
    fn is_fresh(&self, now: DateTime<Utc>) -> bool {
        let ttl = Duration::minutes(STARTUP_TTL_MINUTES);
        let migrations_fresh = (now - self.migrations_applied_at) < ttl;
        let provider_fresh = self.provider_ok_at.is_none_or(|t| (now - t) < ttl);
        let cloud_fresh = self.cloud_ok_at.is_none_or(|t| (now - t) < ttl);
        migrations_fresh && provider_fresh && cloud_fresh
    }
}

/// `~/.difflore/startup-cache.json` (overridable via `DIFFLORE_HOME`).
fn cache_path() -> Result<PathBuf, CoreError> {
    let dir = paths::data_home().map_err(CoreError::Internal)?;
    Ok(dir.join("startup-cache.json"))
}

async fn read_cache() -> Option<StartupStatus> {
    let path = cache_path().ok()?;
    let bytes = fs::read(&path).await.ok()?;
    serde_json::from_slice(&bytes).ok()
}

async fn write_cache(status: &StartupStatus) -> Result<(), CoreError> {
    let path = cache_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    let bytes = serde_json::to_vec_pretty(status)?;
    fs::write(&path, bytes).await?;
    Ok(())
}

/// In-process mirror of the on-disk cache. Avoids re-paying the file
/// roundtrip when `ensure_ready` is called many times within a single
/// CLI invocation (MCP server + hook subprocess + CLI command), and
/// sidesteps Windows NTFS write-then-read visibility races that surface
/// on CI's cold-start virtual disk. The file cache is still the source
/// of truth across processes — the memory cache only short-circuits
/// repeats within the same process.
static MEMORY_CACHE: tokio::sync::Mutex<Option<StartupStatus>> =
    tokio::sync::Mutex::const_new(None);

/// Run the full check battery: open the DB pool (running any pending
/// migrations), read the provider config, and ping the cloud if the
/// user is logged in. Returns a freshly-stamped `StartupStatus`.
///
/// Every individual probe is fault-tolerant — a cloud outage must not
/// prevent the CLI from running. We record `None` for probes that
/// failed so the next invocation will retry, rather than masking them
/// behind a stale-but-present timestamp.
async fn run_full_check() -> Result<StartupStatus, CoreError> {
    let now = Utc::now();

    // Migrations — re-running is a no-op when everything is already
    // applied, so the cost is a single metadata query.
    let _pool = crate::infra::db::init_db()
        .await
        .map_err(CoreError::Internal)?;

    // Recover any cloud-outbox rows that got stuck in 'processing' after
    // a crashed drain (e.g. SIGKILL'd hook). Anything older than 60 s is
    // bounced back to 'pending' so the next drain can retry. Failures
    // are logged but never block startup — a stale outbox row costs at
    // most one retry's worth of duplicate work on the cloud side.
    if let Ok(pool) = crate::infra::db::init_db().await {
        let queue = crate::cloud::outbox::OutboxQueue::new(pool);
        if let Err(e) = queue
            .reset_stale(crate::cloud::outbox::DEFAULT_STALE_SECONDS)
            .await
        {
            if crate::infra::env::debug_cloud() {
                eprintln!("[difflore] cloud_outbox reset_stale skipped: {e}");
            }
        }
    }

    // Provider config read. We only need to know "is there a usable
    // provider table?" — the `list()` query walks the same rows as the
    // CLI would. Failures are recorded as `None` so the next invocation
    // retries.
    let db = crate::infra::db::init_db()
        .await
        .map_err(CoreError::Internal)?;
    let provider_ok_at = match crate::domain::providers::list(&db).await {
        Ok(_) => Some(now),
        Err(_) => None,
    };

    // Cloud reachability: only exercised when the user is logged in.
    // A logged-out user has no cloud to ping, and we leave the field
    // unset so `is_fresh` treats it as "not applicable".
    let cloud_ok_at = {
        let client = crate::cloud::client::CloudClient::create().await;
        if client.is_logged_in() {
            // No dedicated ping endpoint, so probe with the cheapest
            // logged-in call we have. The `"_ping_"` sentinel satisfies
            // the server's `min(1)` validation on `queryText`; only the
            // round-trip succeeding matters, not the response body.
            let req = crate::contract::RecallPastVerdictsRequest {
                embedding: Vec::new(),
                query_text: Some("_ping_".to_owned()),
                repo_id: None,
                scope: "personal".to_owned(),
                team_id: None,
                k: 1,
                target_file: None,
            };
            match client.recall_past_verdicts(req).await {
                Ok(_) => Some(now),
                Err(_) => None,
            }
        } else {
            None
        }
    };

    let status = StartupStatus {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        migrations_applied_at: now,
        provider_ok_at,
        cloud_ok_at,
    };
    write_cache(&status).await?;
    Ok(status)
}

/// Gate every CLI command goes through on entry. When the cache is
/// fresh and `force` is false, returns the cached status without
/// touching the filesystem beyond the single read. When any probe is
/// stale, runs the full check battery and updates the cache.
///
/// `force=true` always re-runs every probe. Use it from explicit
/// "reset"-style subcommands (`difflore init`, `difflore cloud login`,
/// etc.) where we want the next command to see the post-change state
/// immediately, not up to 5 minutes later.
pub async fn ensure_ready(force: bool) -> Result<StartupStatus, CoreError> {
    let now = Utc::now();
    if !force {
        if let Some(cached) = MEMORY_CACHE.lock().await.as_ref()
            && cached.is_fresh(now)
        {
            return Ok(cached.clone());
        }
        if let Some(cached) = read_cache().await
            && cached.is_fresh(now)
        {
            *MEMORY_CACHE.lock().await = Some(cached.clone());
            return Ok(cached);
        }
    }
    let status = run_full_check().await?;
    *MEMORY_CACHE.lock().await = Some(status.clone());
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialise the two cache-dependent tests in this module. They
    /// share `startup-cache.json` and the global `data.db` migration
    /// state under the crate-wide test home; running them in parallel
    /// produced false failures where one test's force-refresh read
    /// the other's half-written cache and `migrate!` raced on the
    /// `_sqlx_migrations` table. Holding an async `Mutex` guard
    /// across `await` points is supported by `tokio::sync::Mutex`.
    static CACHE_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[test]
    fn is_fresh_handles_missing_probes() {
        let now = Utc::now();
        let status = StartupStatus {
            version: "0.1.0".into(),
            migrations_applied_at: now,
            provider_ok_at: None,
            cloud_ok_at: None,
        };
        // Missing probes shouldn't render the cache stale — a logged-
        // out user legitimately has no cloud timestamp.
        assert!(status.is_fresh(now));
    }

    #[test]
    fn is_fresh_rejects_stale_migrations() {
        let now = Utc::now();
        let status = StartupStatus {
            version: "0.1.0".into(),
            migrations_applied_at: now - Duration::minutes(STARTUP_TTL_MINUTES + 1),
            provider_ok_at: Some(now),
            cloud_ok_at: Some(now),
        };
        assert!(!status.is_fresh(now));
    }

    #[test]
    fn is_fresh_rejects_stale_cloud() {
        let now = Utc::now();
        let status = StartupStatus {
            version: "0.1.0".into(),
            migrations_applied_at: now,
            provider_ok_at: Some(now),
            cloud_ok_at: Some(now - Duration::minutes(STARTUP_TTL_MINUTES + 1)),
        };
        assert!(!status.is_fresh(now));
    }

    #[tokio::test]
    async fn ensure_ready_caches_between_calls() {
        let _guard = CACHE_SERIAL.lock().await;
        let _home = crate::infra::db::shared_test_home();

        // Force one fresh probe so we're comparing against a known
        // baseline rather than whatever the previous test left in the
        // shared cache file.
        let first = ensure_ready(true).await.expect("first call");
        let first_ts = first.migrations_applied_at;

        // Second call should return the cached status — same timestamp.
        let second = ensure_ready(false).await.expect("second call");
        assert_eq!(
            second.migrations_applied_at, first_ts,
            "second call should come from cache, not re-run"
        );
    }

    #[tokio::test]
    async fn ensure_ready_force_refreshes_cache() {
        let _guard = CACHE_SERIAL.lock().await;
        let _home = crate::infra::db::shared_test_home();

        let first = ensure_ready(false).await.expect("first call");
        let first_ts = first.migrations_applied_at;

        // Tiny sleep so the recorded timestamps are actually different.
        // We don't want this test to be timing-sensitive, so we use
        // millisecond-level resolution via tokio::time::sleep.
        tokio::time::sleep(std::time::Duration::from_millis(15)).await;

        let second = ensure_ready(true).await.expect("force call");
        assert!(
            second.migrations_applied_at > first_ts,
            "force=true must re-run the full check (got {} vs {first_ts})",
            second.migrations_applied_at
        );
    }
}
