use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{Row, SqlitePool};

use crate::memory_autopilot::{
    AutopilotEventInput, DEFAULT_AUTOPILOT_LIMIT, MemoryAutopilotOptions,
    ensure_autopilot_events_table, record_autopilot_event, run_memory_autopilot,
};
use crate::{CoreError, Result};

pub const BACKGROUND_AUTOPILOT_LIMIT: usize = DEFAULT_AUTOPILOT_LIMIT;
pub const BACKGROUND_CURATOR_LIMIT: usize = DEFAULT_AUTOPILOT_LIMIT;
pub const SESSION_END_AUTOPILOT_COOLDOWN_SECS: i64 = 300;
pub const EXPLICIT_AUTOPILOT_COOLDOWN_SECS: i64 = 60;

const BACKGROUND_LEASE_SECS: i64 = 600;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutopilotScheduleRequest<'a> {
    pub reason: &'a str,
    pub cooldown_secs: i64,
    pub lease_owner: &'a str,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MemoryAutopilotScheduleStatus {
    pub enabled: bool,
    pub dirty: bool,
    pub last_dirty_at: Option<String>,
    pub last_dirty_reason: Option<String>,
    pub last_trigger_at: Option<String>,
    pub last_trigger_reason: Option<String>,
    pub last_run_at: Option<String>,
    pub lease_owner: Option<String>,
    pub lease_expires_at: Option<String>,
    pub last_result: Value,
    pub trigger_count: i64,
    pub dirty_mark_count: i64,
    pub spawn_attempt_count: i64,
    pub spawn_success_count: i64,
    pub run_count: i64,
    pub productive_run_count: i64,
    pub skip_count: i64,
    pub last_skip_reason: Option<String>,
}

impl Default for MemoryAutopilotScheduleStatus {
    fn default() -> Self {
        Self {
            enabled: true,
            dirty: false,
            last_dirty_at: None,
            last_dirty_reason: None,
            last_trigger_at: None,
            last_trigger_reason: None,
            last_run_at: None,
            lease_owner: None,
            lease_expires_at: None,
            last_result: json!({}),
            trigger_count: 0,
            dirty_mark_count: 0,
            spawn_attempt_count: 0,
            spawn_success_count: 0,
            run_count: 0,
            productive_run_count: 0,
            skip_count: 0,
            last_skip_reason: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MemoryAutopilotBackgroundRun {
    pub lease_owner: String,
    pub productive: bool,
    pub enabled_count: usize,
    pub skipped_count: usize,
    pub remaining_auto_enable_groups: usize,
    pub needs_review_groups: usize,
    pub started_at: String,
    pub result: Value,
}

pub async fn ensure_autopilot_schedule_table(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS memory_autopilot_schedule (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            enabled INTEGER NOT NULL DEFAULT 1,
            dirty INTEGER NOT NULL DEFAULT 0,
            last_dirty_at TEXT,
            last_dirty_reason TEXT,
            last_trigger_at TEXT,
            last_trigger_reason TEXT,
            last_run_at TEXT,
            lease_owner TEXT,
            lease_expires_at TEXT,
            last_result TEXT NOT NULL DEFAULT '{}',
            trigger_count INTEGER NOT NULL DEFAULT 0,
            dirty_mark_count INTEGER NOT NULL DEFAULT 0,
            spawn_attempt_count INTEGER NOT NULL DEFAULT 0,
            spawn_success_count INTEGER NOT NULL DEFAULT 0,
            run_count INTEGER NOT NULL DEFAULT 0,
            productive_run_count INTEGER NOT NULL DEFAULT 0,
            skip_count INTEGER NOT NULL DEFAULT 0,
            last_skip_reason TEXT
        )",
    )
    .execute(pool)
    .await?;
    sqlx::query("INSERT OR IGNORE INTO memory_autopilot_schedule (id) VALUES (1)")
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn mark_autopilot_dirty(pool: &SqlitePool, reason: &str) -> Result<()> {
    ensure_autopilot_schedule_table(pool).await?;
    sqlx::query(
        "UPDATE memory_autopilot_schedule
         SET dirty = 1,
             last_dirty_at = strftime('%Y-%m-%dT%H:%M:%f', 'now'),
             last_dirty_reason = ?1,
             last_trigger_at = strftime('%Y-%m-%dT%H:%M:%f', 'now'),
             last_trigger_reason = ?1,
             trigger_count = trigger_count + 1,
             dirty_mark_count = dirty_mark_count + 1
         WHERE id = 1",
    )
    .bind(reason)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn try_acquire_autopilot_lease(
    pool: &SqlitePool,
    request: AutopilotScheduleRequest<'_>,
) -> Result<bool> {
    ensure_autopilot_schedule_table(pool).await?;
    let cooldown = format!("-{} seconds", request.cooldown_secs.max(0));
    let lease_expiry = format!("+{BACKGROUND_LEASE_SECS} seconds");
    let result = sqlx::query(
        "UPDATE memory_autopilot_schedule
         SET lease_owner = ?1,
             lease_expires_at = strftime('%Y-%m-%dT%H:%M:%f', 'now', ?2),
             last_trigger_at = strftime('%Y-%m-%dT%H:%M:%f', 'now'),
             last_trigger_reason = ?3,
             trigger_count = trigger_count + 1,
             spawn_attempt_count = spawn_attempt_count + 1,
             last_skip_reason = NULL
         WHERE id = 1
           AND enabled = 1
           AND dirty = 1
           AND (lease_owner IS NULL OR julianday(lease_expires_at) < julianday('now'))
           AND (last_run_at IS NULL OR julianday(last_run_at) < julianday('now', ?4))",
    )
    .bind(request.lease_owner)
    .bind(lease_expiry)
    .bind(request.reason)
    .bind(&cooldown)
    .execute(pool)
    .await?;

    if result.rows_affected() == 1 {
        return Ok(true);
    }

    record_autopilot_skip(pool, request.reason, &cooldown).await?;
    Ok(false)
}

pub async fn try_acquire_manual_autopilot_lease(
    pool: &SqlitePool,
    lease_owner: &str,
) -> Result<bool> {
    ensure_autopilot_schedule_table(pool).await?;
    let lease_expiry = format!("+{BACKGROUND_LEASE_SECS} seconds");
    let result = sqlx::query(
        "UPDATE memory_autopilot_schedule
         SET lease_owner = ?1,
             lease_expires_at = strftime('%Y-%m-%dT%H:%M:%f', 'now', ?2),
             last_trigger_at = strftime('%Y-%m-%dT%H:%M:%f', 'now'),
             last_trigger_reason = 'manual',
             trigger_count = trigger_count + 1
         WHERE id = 1
           AND (lease_owner IS NULL OR julianday(lease_expires_at) < julianday('now'))",
    )
    .bind(lease_owner)
    .bind(lease_expiry)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

pub async fn note_autopilot_spawn_success(pool: &SqlitePool, lease_owner: &str) -> Result<()> {
    ensure_autopilot_schedule_table(pool).await?;
    sqlx::query(
        "UPDATE memory_autopilot_schedule
         SET spawn_success_count = spawn_success_count + 1
         WHERE id = 1 AND lease_owner = ?1",
    )
    .bind(lease_owner)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn release_autopilot_lease(
    pool: &SqlitePool,
    lease_owner: &str,
    reason: &str,
) -> Result<()> {
    ensure_autopilot_schedule_table(pool).await?;
    sqlx::query(
        "UPDATE memory_autopilot_schedule
         SET lease_owner = NULL,
             lease_expires_at = NULL,
             last_skip_reason = ?2
         WHERE id = 1 AND lease_owner = ?1",
    )
    .bind(lease_owner)
    .bind(reason)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn run_background_memory_autopilot(
    pool: &SqlitePool,
    lease_owner: &str,
) -> Result<MemoryAutopilotBackgroundRun> {
    ensure_autopilot_schedule_table(pool).await?;
    ensure_autopilot_events_table(pool).await?;
    let started_at = sqlite_now(pool).await?;

    let owns_lease = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM memory_autopilot_schedule
         WHERE id = 1 AND lease_owner = ?1 AND julianday(lease_expires_at) >= julianday('now')",
    )
    .bind(lease_owner)
    .fetch_one(pool)
    .await?;
    if owns_lease == 0 {
        return Err(CoreError::Validation(
            "autopilot background lease is not held".to_owned(),
        ));
    }

    let report = run_memory_autopilot(
        pool,
        MemoryAutopilotOptions {
            dry_run: false,
            max_auto_enable: BACKGROUND_AUTOPILOT_LIMIT,
            curator_max_candidates: Some(BACKGROUND_CURATOR_LIMIT),
        },
    )
    .await;

    let run = match report {
        Ok(report) => {
            let enabled_count = report.auto_enabled.len();
            let productive = enabled_count > 0;
            let result = json!({
                "status": "ok",
                "productive": productive,
                "enabledCount": enabled_count,
                "skippedCount": report.skipped.len(),
                "remainingAutoEnableGroups": report.digest.counts.auto_enable_groups,
                "needsReviewGroups": report.digest.counts.needs_review_groups,
                "maxAutoEnable": report.max_auto_enable,
                "startedAt": started_at,
            });
            finish_autopilot_run(pool, lease_owner, &started_at, productive, &result).await?;
            record_autopilot_event(
                pool,
                AutopilotEventInput {
                    event_type: "background_run_finished",
                    rule_id: None,
                    item_ids: &[],
                    group_id: None,
                    title: "Background memory autopilot",
                    reason: if productive {
                        "enabled high-confidence memory"
                    } else {
                        "no high-confidence memory enabled"
                    },
                    payload: result.clone(),
                },
            )
            .await?;
            MemoryAutopilotBackgroundRun {
                lease_owner: lease_owner.to_owned(),
                productive,
                enabled_count,
                skipped_count: report.skipped.len(),
                remaining_auto_enable_groups: report.digest.counts.auto_enable_groups,
                needs_review_groups: report.digest.counts.needs_review_groups,
                started_at,
                result,
            }
        }
        Err(err) => {
            let error_message = err.to_string();
            let result = json!({
                "status": "error",
                "productive": false,
                "error": error_message,
                "startedAt": started_at,
            });
            finish_autopilot_run(pool, lease_owner, &started_at, false, &result).await?;
            record_autopilot_event(
                pool,
                AutopilotEventInput {
                    event_type: "background_run_failed",
                    rule_id: None,
                    item_ids: &[],
                    group_id: None,
                    title: "Background memory autopilot",
                    reason: result
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("background autopilot failed"),
                    payload: result.clone(),
                },
            )
            .await?;
            return Err(err);
        }
    };

    Ok(run)
}

pub async fn load_autopilot_schedule_status(
    pool: &SqlitePool,
) -> Result<MemoryAutopilotScheduleStatus> {
    ensure_autopilot_schedule_table(pool).await?;
    let row = sqlx::query(
        "SELECT enabled, dirty, last_dirty_at, last_dirty_reason,
                last_trigger_at, last_trigger_reason, last_run_at,
                lease_owner, lease_expires_at, last_result,
                trigger_count, dirty_mark_count, spawn_attempt_count, spawn_success_count,
                run_count, productive_run_count, skip_count, last_skip_reason
         FROM memory_autopilot_schedule
         WHERE id = 1",
    )
    .fetch_one(pool)
    .await?;
    let last_result_raw: String = row.try_get("last_result")?;
    Ok(MemoryAutopilotScheduleStatus {
        enabled: row.try_get::<i64, _>("enabled")? != 0,
        dirty: row.try_get::<i64, _>("dirty")? != 0,
        last_dirty_at: row.try_get("last_dirty_at").ok(),
        last_dirty_reason: row.try_get("last_dirty_reason").ok(),
        last_trigger_at: row.try_get("last_trigger_at").ok(),
        last_trigger_reason: row.try_get("last_trigger_reason").ok(),
        last_run_at: row.try_get("last_run_at").ok(),
        lease_owner: row.try_get("lease_owner").ok(),
        lease_expires_at: row.try_get("lease_expires_at").ok(),
        last_result: serde_json::from_str(&last_result_raw).unwrap_or_else(|_| json!({})),
        trigger_count: row.try_get("trigger_count")?,
        dirty_mark_count: row.try_get("dirty_mark_count")?,
        spawn_attempt_count: row.try_get("spawn_attempt_count")?,
        spawn_success_count: row.try_get("spawn_success_count")?,
        run_count: row.try_get("run_count")?,
        productive_run_count: row.try_get("productive_run_count")?,
        skip_count: row.try_get("skip_count")?,
        last_skip_reason: row.try_get("last_skip_reason").ok(),
    })
}

async fn record_autopilot_skip(pool: &SqlitePool, reason: &str, cooldown: &str) -> Result<()> {
    // The CASE mirrors the acquire predicate in `try_acquire_autopilot_lease` so the recorded
    // reason reflects the same predicate that just failed. The explicit cooldown arm uses the
    // negation of the acquire cooldown clause (last_run_at >= now+cooldown) instead of relying on
    // a catch-all ELSE, so unmodeled failures fall through to 'unknown' rather than mislabelling
    // them as cooldown.
    sqlx::query(
        "UPDATE memory_autopilot_schedule
         SET trigger_count = trigger_count + 1,
             spawn_attempt_count = spawn_attempt_count + 1,
             skip_count = skip_count + 1,
             last_trigger_at = strftime('%Y-%m-%dT%H:%M:%f', 'now'),
             last_trigger_reason = ?1,
             last_skip_reason =
                CASE
                    WHEN enabled = 0 THEN 'disabled'
                    WHEN dirty = 0 THEN 'not_dirty'
                    WHEN lease_owner IS NOT NULL AND julianday(lease_expires_at) >= julianday('now') THEN 'lease_held'
                    WHEN last_run_at IS NOT NULL AND julianday(last_run_at) >= julianday('now', ?2) THEN 'cooldown'
                    ELSE 'unknown'
                END
         WHERE id = 1",
    )
    .bind(reason)
    .bind(cooldown)
    .execute(pool)
    .await?;
    Ok(())
}

async fn finish_autopilot_run(
    pool: &SqlitePool,
    lease_owner: &str,
    started_at: &str,
    productive: bool,
    result: &Value,
) -> Result<()> {
    let result_json = serde_json::to_string(result)?;
    sqlx::query(
        "UPDATE memory_autopilot_schedule
         SET last_run_at = strftime('%Y-%m-%dT%H:%M:%f', 'now'),
             dirty = CASE
                WHEN last_dirty_at IS NULL OR julianday(last_dirty_at) < julianday(?2) THEN 0
                ELSE dirty
             END,
             lease_owner = NULL,
             lease_expires_at = NULL,
             last_result = ?3,
             run_count = run_count + 1,
             productive_run_count = productive_run_count + ?4,
             last_skip_reason = NULL
         WHERE id = 1 AND lease_owner = ?1",
    )
    .bind(lease_owner)
    .bind(started_at)
    .bind(result_json)
    .bind(i64::from(productive))
    .execute(pool)
    .await?;
    Ok(())
}

async fn sqlite_now(pool: &SqlitePool) -> Result<String> {
    Ok(
        sqlx::query_scalar::<_, String>("SELECT strftime('%Y-%m-%dT%H:%M:%f', 'now')")
            .fetch_one(pool)
            .await?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;

    async fn pool() -> SqlitePool {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")
            .expect("sqlite opts")
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .expect("connect");
        ensure_autopilot_schedule_table(&pool)
            .await
            .expect("ensure table");
        pool
    }

    #[tokio::test]
    async fn lease_acquire_is_atomic_for_dirty_schedule() {
        let pool = pool().await;
        mark_autopilot_dirty(&pool, "test").await.expect("dirty");

        let first = try_acquire_autopilot_lease(
            &pool,
            AutopilotScheduleRequest {
                reason: "session_end",
                cooldown_secs: 0,
                lease_owner: "owner-a",
            },
        )
        .await
        .expect("first");
        let second = try_acquire_autopilot_lease(
            &pool,
            AutopilotScheduleRequest {
                reason: "session_end",
                cooldown_secs: 0,
                lease_owner: "owner-b",
            },
        )
        .await
        .expect("second");

        assert!(first);
        assert!(!second);
        let status = load_autopilot_schedule_status(&pool).await.expect("status");
        assert_eq!(status.lease_owner.as_deref(), Some("owner-a"));
        assert_eq!(status.spawn_attempt_count, 2);
        assert_eq!(status.skip_count, 1);
    }

    #[tokio::test]
    async fn run_started_after_dirty_does_not_clear_new_dirty() {
        let pool = pool().await;
        mark_autopilot_dirty(&pool, "before").await.expect("dirty");
        let acquired = try_acquire_autopilot_lease(
            &pool,
            AutopilotScheduleRequest {
                reason: "session_end",
                cooldown_secs: 0,
                lease_owner: "owner",
            },
        )
        .await
        .expect("acquire");
        assert!(acquired);
        let started_at = sqlite_now(&pool).await.expect("now");
        sqlx::query(
            "UPDATE memory_autopilot_schedule
             SET dirty = 1, last_dirty_at = datetime(?1, '+1 second')
             WHERE id = 1",
        )
        .bind(&started_at)
        .execute(&pool)
        .await
        .expect("new dirty");
        finish_autopilot_run(&pool, "owner", &started_at, false, &json!({"status":"ok"}))
            .await
            .expect("finish");

        let status = load_autopilot_schedule_status(&pool).await.expect("status");
        assert!(status.dirty);
        assert_eq!(status.run_count, 1);
        assert_eq!(status.productive_run_count, 0);
    }

    #[tokio::test]
    async fn run_started_at_same_instant_as_dirty_preserves_dirty() {
        let pool = pool().await;
        mark_autopilot_dirty(&pool, "before").await.expect("dirty");
        let acquired = try_acquire_autopilot_lease(
            &pool,
            AutopilotScheduleRequest {
                reason: "session_end",
                cooldown_secs: 0,
                lease_owner: "owner",
            },
        )
        .await
        .expect("acquire");
        assert!(acquired);
        let started_at = "2026-01-01T00:00:00.500";
        sqlx::query(
            "UPDATE memory_autopilot_schedule
             SET dirty = 1, last_dirty_at = ?1
             WHERE id = 1",
        )
        .bind(started_at)
        .execute(&pool)
        .await
        .expect("same-instant dirty");
        finish_autopilot_run(&pool, "owner", started_at, false, &json!({"status":"ok"}))
            .await
            .expect("finish");

        let status = load_autopilot_schedule_status(&pool).await.expect("status");
        assert!(status.dirty);
        assert_eq!(status.run_count, 1);
    }

    #[tokio::test]
    async fn skip_during_cooldown_records_cooldown_reason() {
        let pool = pool().await;
        mark_autopilot_dirty(&pool, "test").await.expect("dirty");
        let acquired = try_acquire_autopilot_lease(
            &pool,
            AutopilotScheduleRequest {
                reason: "session_end",
                cooldown_secs: 0,
                lease_owner: "owner",
            },
        )
        .await
        .expect("acquire");
        assert!(acquired);
        // Finish the run and re-arm dirty so only the cooldown predicate can block the next
        // acquire. last_run_at is now (set by finish), so a long cooldown keeps us blocked.
        let started_at = sqlite_now(&pool).await.expect("now");
        finish_autopilot_run(&pool, "owner", &started_at, false, &json!({"status":"ok"}))
            .await
            .expect("finish");
        mark_autopilot_dirty(&pool, "again").await.expect("dirty");

        let acquired_again = try_acquire_autopilot_lease(
            &pool,
            AutopilotScheduleRequest {
                reason: "session_end",
                cooldown_secs: 3600,
                lease_owner: "owner-2",
            },
        )
        .await
        .expect("acquire-2");
        assert!(!acquired_again);

        let status = load_autopilot_schedule_status(&pool).await.expect("status");
        assert_eq!(status.last_skip_reason.as_deref(), Some("cooldown"));
    }

    #[tokio::test]
    async fn skip_while_lease_held_records_lease_held_reason() {
        let pool = pool().await;
        mark_autopilot_dirty(&pool, "test").await.expect("dirty");
        let first = try_acquire_autopilot_lease(
            &pool,
            AutopilotScheduleRequest {
                reason: "session_end",
                cooldown_secs: 0,
                lease_owner: "owner-a",
            },
        )
        .await
        .expect("first");
        assert!(first);
        let second = try_acquire_autopilot_lease(
            &pool,
            AutopilotScheduleRequest {
                reason: "session_end",
                cooldown_secs: 0,
                lease_owner: "owner-b",
            },
        )
        .await
        .expect("second");
        assert!(!second);

        let status = load_autopilot_schedule_status(&pool).await.expect("status");
        assert_eq!(status.last_skip_reason.as_deref(), Some("lease_held"));
    }

    #[tokio::test]
    async fn status_surfaces_corrupt_not_null_counters() {
        let pool = pool().await;
        sqlx::query("UPDATE memory_autopilot_schedule SET trigger_count = 'broken' WHERE id = 1")
            .execute(&pool)
            .await
            .expect("corrupt counter");

        let err = load_autopilot_schedule_status(&pool)
            .await
            .expect_err("corrupt counters should not be treated as zero");
        assert!(
            matches!(err, CoreError::Database(_)),
            "expected database decode error, got {err}"
        );
    }

    #[tokio::test]
    async fn expired_lease_can_be_acquired_again() {
        let pool = pool().await;
        mark_autopilot_dirty(&pool, "test").await.expect("dirty");
        let first = try_acquire_autopilot_lease(
            &pool,
            AutopilotScheduleRequest {
                reason: "session_end",
                cooldown_secs: 0,
                lease_owner: "old",
            },
        )
        .await
        .expect("first");
        assert!(first);
        sqlx::query(
            "UPDATE memory_autopilot_schedule
             SET lease_expires_at = datetime('now', '-1 second')
             WHERE id = 1",
        )
        .execute(&pool)
        .await
        .expect("expire");

        let second = try_acquire_autopilot_lease(
            &pool,
            AutopilotScheduleRequest {
                reason: "session_end",
                cooldown_secs: 0,
                lease_owner: "new",
            },
        )
        .await
        .expect("second");
        assert!(second);
        let status = load_autopilot_schedule_status(&pool).await.expect("status");
        assert_eq!(status.lease_owner.as_deref(), Some("new"));
    }
}
