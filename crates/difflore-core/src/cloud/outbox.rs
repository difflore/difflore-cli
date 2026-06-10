//! SQLite-backed outbox queue for fire-and-forget cloud uploads.
//!
//! Every fire-and-forget cloud POST (trajectory, `review_metrics`,
//! `accepted_edit`, `mcp_query`, `imported_reviews`) is first appended as a
//! `pending` row in the global `~/.difflore/data.db`. Drain is triggered
//! synchronously from hook / CLI cold paths — there is deliberately no
//! background daemon.
//!
//! Claim/confirm semantics:
//!
//! ```text
//! enqueue()     -> INSERT status='pending'
//! claim_next()  -> UPDATE status='processing' (atomic, oldest first)
//! confirm(id)   -> DELETE
//! mark_failed() -> UPDATE retry_count++; >=MAX_RETRY_COUNT -> status='abandoned'
//! reset_stale() -> processing > threshold seconds -> pending
//! ```
//!
//! Circuit breaker: three consecutive `mark_failed` calls trip the breaker
//! for 60 s; while open, `claim_next` returns `None` so callers short-
//! circuit without hammering an unreachable cloud. Any successful
//! `confirm` resets the consecutive-failure counter.
//!
//! Idempotency contract: `claim_next` deliberately self-heals stale
//! `processing` rows after `DEFAULT_STALE_SECONDS`. A very slow upload can
//! therefore be retried by a later drain pass. Every cloud endpoint reached
//! from this queue must treat duplicate payloads as idempotent, keyed by
//! the event id / request signature carried in the payload. The queue
//! chooses at-least-once delivery over permanent local data loss.

use super::outbox_core::{self, RetryDecision, now_unix_ms};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::sync::atomic::{AtomicI64, AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};
use tokio::sync::Mutex;

/// Seconds a `processing` row may sit before `reset_stale` / `claim_next`
/// recovers it to `pending` (covers a crashed or hung drain pass).
pub const DEFAULT_STALE_SECONDS: u64 = 60;

/// How many consecutive `mark_failed` calls trip the circuit breaker.
pub const CIRCUIT_FAILURE_THRESHOLD: u32 = 3;

/// How long (ms) the circuit stays open before `claim_next` returns rows again.
pub const CIRCUIT_OPEN_DURATION_MS: i64 = 60_000;

/// Maximum delivery attempts per outbox item; afterwards the item is marked
/// `abandoned` and is no longer claimed.
pub const MAX_RETRY_COUNT: i64 = outbox_core::MAX_RETRY_COUNT;

static DRAIN_SERIALIZATION_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn drain_serialization_lock() -> &'static Mutex<()> {
    DRAIN_SERIALIZATION_LOCK.get_or_init(|| Mutex::new(()))
}

/// A `cloud_outbox` row that has been claimed for processing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboxItem {
    pub id: i64,
    pub kind: String,
    pub payload_json: String,
    pub retry_count: i64,
}

/// Breaker state. `Open` means callers should short-circuit until
/// `until_unix_ms` has passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    Closed,
    Open { until_unix_ms: i64 },
}

/// Queue handle. Cheap to clone; all state is either on disk or behind an
/// `Arc<Atomic*>` so all clones in a process share the same breaker state.
#[derive(Debug, Clone)]
pub struct OutboxQueue {
    pool: SqlitePool,
    /// Consecutive `mark_failed` calls since the last successful `confirm`.
    consecutive_failures: Arc<AtomicU32>,
    /// Unix-ms until which the circuit stays open. `0` when closed.
    circuit_open_until_ms: Arc<AtomicI64>,
}

impl OutboxQueue {
    /// Build a queue handle from an existing pool. The pool must have the
    /// `cloud_outbox` migration applied (i.e. `init_db` was called on it).
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            pool,
            consecutive_failures: Arc::new(AtomicU32::new(0)),
            circuit_open_until_ms: Arc::new(AtomicI64::new(0)),
        }
    }

    /// Insert a new fire-and-forget payload. Returns the row id; production
    /// callers usually ignore it.
    pub async fn enqueue(&self, kind: &str, payload_json: &str) -> Result<i64, sqlx::Error> {
        if !crate::cloud::capture::capture_enabled() {
            return Ok(0);
        }
        let now = now_unix_ms();
        let result = sqlx::query!(
            "INSERT INTO cloud_outbox (kind, payload_json, status, retry_count, created_at) \
             VALUES (?1, ?2, 'pending', 0, ?3)",
            kind,
            payload_json,
            now
        )
        .execute(&self.pool)
        .await?;
        Ok(result.last_insert_rowid())
    }

    /// Current breaker state. Callers should check this before building
    /// expensive payloads for bulk drains.
    pub fn circuit_state(&self) -> CircuitState {
        let until = self.circuit_open_until_ms.load(Ordering::SeqCst);
        if until == 0 {
            return CircuitState::Closed;
        }
        if now_unix_ms() >= until {
            // Expired. Don't reset the failure counter here; the first
            // successful `confirm` does that, and the next `mark_failed`
            // re-opens the breaker.
            self.circuit_open_until_ms.store(0, Ordering::SeqCst);
            CircuitState::Closed
        } else {
            CircuitState::Open {
                until_unix_ms: until,
            }
        }
    }

    /// Atomically pick the oldest `pending` row and flip it to
    /// `processing`. Returns `None` when the queue is empty or the breaker
    /// is open.
    ///
    /// The UPDATE uses `RETURNING` so claim-and-read happen in one
    /// statement, equivalent to `SELECT … FOR UPDATE` on a row-at-a-time
    /// queue (`SQLite` serialises writes per connection).
    pub async fn claim_next(&self) -> Result<Option<OutboxItem>, sqlx::Error> {
        if matches!(self.circuit_state(), CircuitState::Open { .. }) {
            return Ok(None);
        }

        let now = now_unix_ms();
        // `processing` rows whose `claimed_at` is older than
        // `DEFAULT_STALE_SECONDS` (a previous claimer crashed/froze) are
        // re-claimable here. Folding recovery into the same atomic UPDATE
        // self-heals the queue on every claim; `reset_stale` stays public
        // for startup and diagnostics.
        let stale_cutoff = now - (DEFAULT_STALE_SECONDS as i64) * 1000;
        let row = sqlx::query!(
            r#"UPDATE cloud_outbox
             SET status = 'processing', claimed_at = ?1
             WHERE id = (
                 SELECT id FROM cloud_outbox
                 WHERE status = 'pending'
                    OR (status = 'processing'
                        AND claimed_at IS NOT NULL
                        AND claimed_at < ?2)
                 ORDER BY created_at ASC, id ASC
                 LIMIT 1
             )
             RETURNING id as "id!: i64", kind, payload_json, retry_count"#,
            now,
            stale_cutoff
        )
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| OutboxItem {
            id: r.id,
            kind: r.kind,
            payload_json: r.payload_json,
            retry_count: r.retry_count,
        }))
    }

    pub async fn claim_next_kind(&self, kind: &str) -> Result<Option<OutboxItem>, sqlx::Error> {
        if matches!(self.circuit_state(), CircuitState::Open { .. }) {
            return Ok(None);
        }

        let now = now_unix_ms();
        let stale_cutoff = now - (DEFAULT_STALE_SECONDS as i64) * 1000;
        let row = sqlx::query!(
            r#"UPDATE cloud_outbox
             SET status = 'processing', claimed_at = ?1
             WHERE id = (
                 SELECT id FROM cloud_outbox
                 WHERE kind = ?3
                   AND (status = 'pending'
                        OR (status = 'processing'
                            AND claimed_at IS NOT NULL
                            AND claimed_at < ?2))
                 ORDER BY created_at ASC, id ASC
                 LIMIT 1
             )
             RETURNING id AS "id!: i64", kind AS "kind!: String", payload_json AS "payload_json!: String", retry_count AS "retry_count!: i64""#,
            now,
            stale_cutoff,
            kind,
        )
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| OutboxItem {
            id: r.id,
            kind: r.kind,
            payload_json: r.payload_json,
            retry_count: r.retry_count,
        }))
    }

    /// Upload succeeded: delete the row and reset the consecutive-failure
    /// counter so the circuit can close again.
    pub async fn confirm(&self, id: i64) -> Result<(), sqlx::Error> {
        sqlx::query!("DELETE FROM cloud_outbox WHERE id = ?1", id)
            .execute(&self.pool)
            .await?;
        self.consecutive_failures.store(0, Ordering::SeqCst);
        // Don't reset the breaker here; let expiry or the next `claim_next`
        // close it. Avoids races where one confirm sneaks between two failures.
        Ok(())
    }

    /// Upload failed. If the row has been tried fewer than
    /// `MAX_RETRY_COUNT` times, bounce it back to `pending` so a later
    /// drain can retry. Otherwise flip it to `abandoned` — we keep the
    /// row for diagnostics but will never re-claim it.
    ///
    /// This also ticks the consecutive-failure counter and, on the
    /// threshold, opens the circuit for `CIRCUIT_OPEN_DURATION_MS`.
    pub async fn mark_failed(&self, id: i64, err: &str) -> Result<(), sqlx::Error> {
        // Trim unbounded errors so cascade failures cannot bloat the DB.
        let err_trimmed: String = outbox_core::truncate(err, 2048);

        let current = sqlx::query!(
            "SELECT retry_count, status FROM cloud_outbox WHERE id = ?1",
            id
        )
        .fetch_optional(&self.pool)
        .await?;

        let Some(row) = current else {
            // Row vanished between claim and mark_failed (raced with a
            // confirm from another drain pass). No-op; don't tick the counter.
            return Ok(());
        };

        // This queue retries by bouncing rows to `pending`; no backoff delays.
        let (new_status, new_count) = match outbox_core::decide_retry(row.retry_count) {
            RetryDecision::Retry { next_count } => ("pending", next_count),
            RetryDecision::Abandon { next_count } => ("abandoned", next_count),
        };

        sqlx::query!(
            "UPDATE cloud_outbox \
             SET status = ?1, retry_count = ?2, last_error = ?3, claimed_at = NULL \
             WHERE id = ?4",
            new_status,
            new_count,
            err_trimmed,
            id
        )
        .execute(&self.pool)
        .await?;

        // Trip the circuit breaker after N consecutive failures.
        let prev = self.consecutive_failures.fetch_add(1, Ordering::SeqCst);
        if prev + 1 >= CIRCUIT_FAILURE_THRESHOLD {
            let until = now_unix_ms() + CIRCUIT_OPEN_DURATION_MS;
            self.circuit_open_until_ms.store(until, Ordering::SeqCst);
        }

        Ok(())
    }

    /// Promote `processing` rows older than `threshold_secs` back to
    /// `pending`. Called at startup to recover from crashed drains.
    pub async fn reset_stale(&self, threshold_secs: u64) -> Result<u64, sqlx::Error> {
        let cutoff = now_unix_ms() - (threshold_secs as i64) * 1000;
        let result = sqlx::query!(
            "UPDATE cloud_outbox \
             SET status = 'pending', claimed_at = NULL \
             WHERE status = 'processing' AND claimed_at IS NOT NULL AND claimed_at < ?1",
            cutoff
        )
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Per-kind breakdown of `pending` rows for lag warnings, sorted by kind
    /// for deterministic rendering.
    pub async fn pending_counts_by_kind(&self) -> Result<Vec<(String, i64)>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT kind, COUNT(*) AS c \
             FROM cloud_outbox WHERE status = 'pending' GROUP BY kind",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut out: Vec<(String, i64)> = rows
            .into_iter()
            .map(|r| {
                let kind: String = sqlx::Row::try_get(&r, "kind").unwrap_or_default();
                let count: i64 = sqlx::Row::try_get(&r, "c").unwrap_or_default();
                (kind, count)
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    /// Reset `abandoned` rows older than `cutoff_unix_ms` back to `pending`,
    /// returning a per-kind breakdown of rows that were (or, in `dry_run`,
    /// would be) reset.
    ///
    /// A row is eligible iff its most recent `claimed_at` — or `created_at`
    /// if it was abandoned before any attempt — is older than
    /// `cutoff_unix_ms`. Recently-abandoned rows are left alone: those are
    /// likely a current outage rather than a stale-auth backlog.
    ///
    /// The whole operation runs in one `BEGIN/COMMIT` so a partial drain
    /// cannot leave a half-reset queue. `dry_run = true` runs the SELECT but
    /// rolls back, keeping the same snapshot guarantees while staying
    /// read-only.
    pub async fn drain_abandoned_older_than(
        &self,
        cutoff_unix_ms: i64,
        dry_run: bool,
    ) -> Result<DrainSummary, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let rows = sqlx::query(
            "SELECT kind, COUNT(*) AS c \
             FROM cloud_outbox \
             WHERE status = 'abandoned' \
               AND COALESCE(claimed_at, created_at) < ?1 \
             GROUP BY kind",
        )
        .bind(cutoff_unix_ms)
        .fetch_all(&mut *tx)
        .await?;

        let mut summary = DrainSummary::default();
        for row in rows {
            let kind: String = sqlx::Row::try_get(&row, "kind").unwrap_or_default();
            let count: i64 = sqlx::Row::try_get(&row, "c").unwrap_or_default();
            summary.per_kind.push((kind, count));
            summary.total += count;
        }
        summary.per_kind.sort_by(|a, b| a.0.cmp(&b.0));

        if dry_run || summary.total == 0 {
            // Nothing to mutate; roll back rather than commit a no-op tx.
            tx.rollback().await?;
            return Ok(summary);
        }

        let affected = sqlx::query(
            "UPDATE cloud_outbox \
             SET status = 'pending', \
                 retry_count = 0, \
                 last_error = NULL, \
                 claimed_at = NULL \
             WHERE status = 'abandoned' \
               AND COALESCE(claimed_at, created_at) < ?1",
        )
        .bind(cutoff_unix_ms)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        // Reset in-process breaker state so the next drain pass after a
        // resurrection isn't short-circuited by a leftover counter. On-disk
        // row state is authoritative; this is just hygiene.
        self.consecutive_failures.store(0, Ordering::SeqCst);
        self.circuit_open_until_ms.store(0, Ordering::SeqCst);

        // Prefer the real affected count over the snapshot `total`: they can
        // diverge only if a concurrent writer abandoned another eligible row
        // between the SELECT and UPDATE in the same tx.
        let affected = i64::try_from(affected.rows_affected()).unwrap_or(summary.total);
        summary.total = affected;
        Ok(summary)
    }

    /// Number of rows in each status bucket. Diagnostics only (e.g.
    /// `difflore doctor` surfaces a building backlog).
    pub async fn counts(&self) -> Result<OutboxCounts, sqlx::Error> {
        let rows = sqlx::query!(
            r#"SELECT status, COUNT(*) as "c!: i64" FROM cloud_outbox GROUP BY status"#
        )
        .fetch_all(&self.pool)
        .await?;
        let mut out = OutboxCounts::default();
        for r in rows {
            let status: String = r.status;
            let count: i64 = r.c;
            match status.as_str() {
                "pending" => out.pending = count,
                "processing" => out.processing = count,
                "failed" => out.failed = count,
                "abandoned" => out.abandoned = count,
                _ => {}
            }
        }
        Ok(out)
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct OutboxCounts {
    pub pending: i64,
    pub processing: i64,
    pub failed: i64,
    pub abandoned: i64,
}

/// Result of a `drain_abandoned_older_than` call (dry-run or real).
///
/// `total` is the count of rows reset to `pending`; `per_kind` is that count
/// bucketed by `kind`, sorted ascending for deterministic rendering.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DrainSummary {
    pub total: i64,
    pub per_kind: Vec<(String, i64)>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct AcceptedEditAttributionSummary {
    pub uploaded: usize,
    pub launch_grade: usize,
    pub missing_team_workspace: usize,
    pub missing_rule_ids: usize,
    pub unlinked_rule_observations: usize,
}

impl AcceptedEditAttributionSummary {
    pub const fn warning_count(self) -> usize {
        self.missing_team_workspace + self.missing_rule_ids + self.unlinked_rule_observations
    }

    pub const fn add(&mut self, other: Self) {
        self.uploaded += other.uploaded;
        self.launch_grade += other.launch_grade;
        self.missing_team_workspace += other.missing_team_workspace;
        self.missing_rule_ids += other.missing_rule_ids;
        self.unlinked_rule_observations += other.unlinked_rule_observations;
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct OutboxDrainReport {
    pub attempted: usize,
    pub confirmed: usize,
    pub accepted_edit_attribution: AcceptedEditAttributionSummary,
}

/// Per-row dispatch result.
struct DispatchOutcome {
    ok: bool,
    accepted_edit_attribution: Option<AcceptedEditAttributionSummary>,
    /// Greppable string persisted into `cloud_outbox.last_error` when
    /// `ok == false`; `None` on success. HTTP failures use
    /// `"{status} {reason}: {body_snippet}"`, transport failures
    /// `"transport: {message}"`, and semantic rejections (2xx but
    /// `acceptance_recorded == false`) carry the cloud's `response.error`.
    last_error: Option<String>,
}

impl DispatchOutcome {
    const fn ok(ok: bool) -> Self {
        Self {
            ok,
            accepted_edit_attribution: None,
            last_error: None,
        }
    }

    /// Failure-path constructor: `ok = false` carrying the formatted error.
    const fn failed_with(last_error: String) -> Self {
        Self {
            ok: false,
            accepted_edit_attribution: None,
            last_error: Some(last_error),
        }
    }

    fn from_outbox_failure(failure: &super::client::OutboxFailure) -> Self {
        Self::failed_with(failure.format_for_outbox_last_error())
    }
}

fn accepted_edit_attribution_summary(
    expected_rule_ids: usize,
    response: &super::api_types::RecordAcceptedEditResponse,
) -> AcceptedEditAttributionSummary {
    let mut summary = AcceptedEditAttributionSummary {
        uploaded: usize::from(response.acceptance_recorded),
        launch_grade: 0,
        missing_team_workspace: 0,
        missing_rule_ids: 0,
        unlinked_rule_observations: 0,
    };
    if response.acceptance_recorded {
        if expected_rule_ids == 0 {
            summary.missing_rule_ids = 1;
        }
        if response.team_id.is_none() {
            summary.missing_team_workspace = 1;
        }
        if expected_rule_ids > 0 && response.observations_inserted == 0 {
            summary.unlinked_rule_observations = 1;
        }
        if summary.warning_count() == 0 {
            summary.launch_grade = 1;
        }
    }
    summary
}

/// Supported outbox payload kinds. Stored as TEXT in `cloud_outbox.kind`;
/// the `drain_outbox` dispatcher matches on these to pick the right POST
/// route. Keep the string literals stable — changing one means abandoning
/// every row in the queue at upgrade time.
pub mod kind {
    pub const TRAJECTORY: &str = "trajectory";
    pub const REVIEW_METRICS: &str = "review_metrics";
    pub const ACCEPTED_EDIT: &str = "accepted_edit";
    /// Pre-release rows. Drains acknowledge and discard them; they must never
    /// feed the current accepted-edit value-loop evidence endpoint.
    pub const LEGACY_FIX_ACCEPTANCE: &str = "fix_acceptance";
    pub const MCP_QUERY: &str = "mcp_query";
    pub const IMPORTED_REVIEWS: &str = "imported_reviews";
    /// `PostToolUse` observation; see `cloud::api_types::Observation`
    /// and `crate::observation` for the payload shape.
    pub const OBSERVATION: &str = "observation";
    /// Session-mined candidate rule (see
    /// [`crate::cloud::session_mined::SessionMinedCandidate`]); destination is
    /// `POST /api/cloud/session-mined-candidates`. The dispatcher still needs
    /// an explicit arm before these rows leave `pending`.
    pub const SESSION_MINED_CANDIDATE: &str = "session_mined_candidate";
}

/// Drain at most `max_items` outbox rows: for each, dispatch to the right
/// `CloudClient` method, then `confirm` on success or `mark_failed` on
/// failure. Returns `(attempted, confirmed)`.
///
/// Best-effort: SQL errors are surfaced, but upload failures are absorbed
/// into the queue's retry counters. Called from hook cold-path exits and CLI
/// commands with idle time after their main work.
pub async fn drain_outbox(
    queue: &OutboxQueue,
    client: &super::client::CloudClient,
    max_items: usize,
) -> Result<(usize, usize), sqlx::Error> {
    let report = drain_outbox_report(queue, client, max_items).await?;
    Ok((report.attempted, report.confirmed))
}

pub async fn drain_outbox_report(
    queue: &OutboxQueue,
    client: &super::client::CloudClient,
    max_items: usize,
) -> Result<OutboxDrainReport, sqlx::Error> {
    if !client.is_logged_in() {
        // Logged out — leave rows in place; a future logged-in session
        // will drain them. Treat this as "nothing to do".
        return Ok(OutboxDrainReport::default());
    }
    let _drain_guard = drain_serialization_lock().lock().await;

    let mut attempted = 0usize;
    let mut confirmed = 0usize;
    let mut accepted_edit_attribution = AcceptedEditAttributionSummary::default();
    for _ in 0..max_items {
        if matches!(queue.circuit_state(), CircuitState::Open { .. }) {
            break;
        }
        let Some(item) = queue.claim_next().await? else {
            break;
        };
        attempted += 1;
        let outcome = match dispatch(client, &item).await {
            Ok(outcome) => outcome,
            Err(err) => {
                let _ = queue.mark_failed(item.id, &err).await;
                continue;
            }
        };
        if outcome.ok {
            queue.confirm(item.id).await?;
            confirmed += 1;
            if let Some(summary) = outcome.accepted_edit_attribution {
                accepted_edit_attribution.add(summary);
            }
        } else {
            // Persist the structured dispatch error when available.
            let err_msg = outcome
                .last_error
                .as_deref()
                .unwrap_or("upload returned non-2xx (no detail)");
            let _ = queue.mark_failed(item.id, err_msg).await;
        }
    }
    Ok(OutboxDrainReport {
        attempted,
        confirmed,
        accepted_edit_attribution,
    })
}

pub async fn drain_outbox_kind(
    queue: &OutboxQueue,
    client: &super::client::CloudClient,
    kind: &str,
    max_items: usize,
) -> Result<(usize, usize), sqlx::Error> {
    let report = drain_outbox_kind_report(queue, client, kind, max_items).await?;
    Ok((report.attempted, report.confirmed))
}

pub async fn drain_outbox_kind_report(
    queue: &OutboxQueue,
    client: &super::client::CloudClient,
    kind: &str,
    max_items: usize,
) -> Result<OutboxDrainReport, sqlx::Error> {
    if !client.is_logged_in() {
        return Ok(OutboxDrainReport::default());
    }
    let _drain_guard = drain_serialization_lock().lock().await;

    let mut attempted = 0usize;
    let mut confirmed = 0usize;
    let mut accepted_edit_attribution = AcceptedEditAttributionSummary::default();
    for _ in 0..max_items {
        if matches!(queue.circuit_state(), CircuitState::Open { .. }) {
            break;
        }
        let Some(item) = queue.claim_next_kind(kind).await? else {
            break;
        };
        attempted += 1;
        let outcome = match dispatch(client, &item).await {
            Ok(outcome) => outcome,
            Err(err) => {
                let _ = queue.mark_failed(item.id, &err).await;
                continue;
            }
        };
        if outcome.ok {
            queue.confirm(item.id).await?;
            confirmed += 1;
            if let Some(summary) = outcome.accepted_edit_attribution {
                accepted_edit_attribution.add(summary);
            }
        } else {
            // Persist the structured dispatch error when available.
            let err_msg = outcome
                .last_error
                .as_deref()
                .unwrap_or("upload returned non-2xx (no detail)");
            let _ = queue.mark_failed(item.id, err_msg).await;
        }
    }
    Ok(OutboxDrainReport {
        attempted,
        confirmed,
        accepted_edit_attribution,
    })
}

/// Route a single outbox row to the correct `CloudClient` method.
///
/// Payload JSON is a versionless wrapper. Schemas:
///
/// * `trajectory`        — `{ "pr_review_id": String, "steps": Value }`
/// * `review_metrics`    — `{ "review_id": String, "req": RecordReviewMetricsRequest }`
/// * `accepted_edit`     — `RecordAcceptedEditRequest`
/// * `fix_acceptance`    — legacy pre-release rows; explicitly skipped
/// * `mcp_query`         — `{ "file", "intent", "rules_injected",
///                             "strict_match_count", "rule_titles",
///                             "client_label" }`
/// * `imported_reviews`  — `UploadImportedReviewsRequest`
async fn dispatch(
    client: &super::client::CloudClient,
    item: &OutboxItem,
) -> Result<DispatchOutcome, String> {
    use super::api_types::{
        RecordAcceptedEditRequest, RecordReviewMetricsRequest, UploadImportedReviewsRequest,
    };
    use serde_json::Value;

    match item.kind.as_str() {
        kind::TRAJECTORY => {
            let v: Value = serde_json::from_str(&item.payload_json)
                .map_err(|e| format!("trajectory parse: {e}"))?;
            let pr_review_id = v
                .get("pr_review_id")
                .and_then(|x| x.as_str())
                .ok_or_else(|| "trajectory missing pr_review_id".to_owned())?;
            let steps = v.get("steps").cloned().unwrap_or(Value::Array(Vec::new()));
            Ok(
                match client.save_trajectory_outcome(pr_review_id, steps).await {
                    Ok(()) => DispatchOutcome::ok(true),
                    Err(failure) => DispatchOutcome::from_outbox_failure(&failure),
                },
            )
        }
        kind::REVIEW_METRICS => {
            let v: Value = serde_json::from_str(&item.payload_json)
                .map_err(|e| format!("review_metrics parse: {e}"))?;
            let review_id = v
                .get("review_id")
                .and_then(|x| x.as_str())
                .ok_or_else(|| "review_metrics missing review_id".to_owned())?
                .to_owned();
            let req_val = v
                .get("req")
                .cloned()
                .unwrap_or(Value::Object(serde_json::Map::default()));
            let req: RecordReviewMetricsRequest = serde_json::from_value(req_val)
                .map_err(|e| format!("review_metrics decode req: {e}"))?;
            Ok(
                match client.record_review_metrics_outcome(&review_id, req).await {
                    Ok(()) => DispatchOutcome::ok(true),
                    Err(failure) => DispatchOutcome::from_outbox_failure(&failure),
                },
            )
        }
        kind::ACCEPTED_EDIT => {
            let req: RecordAcceptedEditRequest = serde_json::from_str(&item.payload_json)
                .map_err(|e| format!("accepted_edit parse: {e}"))?;
            let expected_rule_ids = req
                .rule_ids
                .iter()
                .filter(|rule_id| !rule_id.trim().is_empty())
                .count();
            let response = client.record_accepted_edit_response(req).await?;
            let summary = accepted_edit_attribution_summary(expected_rule_ids, &response);
            // Semantic-only failure: 2xx but acceptance not recorded (dedup,
            // payload rejection). Surface the cloud's `error`, not a `non-2xx`
            // literal that would be wrong for a 2xx response.
            let last_error = if response.acceptance_recorded {
                None
            } else {
                Some(format!(
                    "accepted_edit rejected: {}",
                    response.error.as_deref().unwrap_or("no detail")
                ))
            };
            Ok(DispatchOutcome {
                ok: response.acceptance_recorded,
                accepted_edit_attribution: Some(summary),
                last_error,
            })
        }
        kind::LEGACY_FIX_ACCEPTANCE => {
            // Legacy `fix_acceptance` rows predate the accepted-edit proof
            // contract. Confirm them so old queues stop retrying, but never
            // POST them or count them as current value-loop evidence.
            Ok(DispatchOutcome::ok(true))
        }
        kind::MCP_QUERY => {
            let v: Value = serde_json::from_str(&item.payload_json)
                .map_err(|e| format!("mcp_query parse: {e}"))?;
            let file = v
                .get("file")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_owned();
            let intent = v
                .get("intent")
                .and_then(|x| x.as_str())
                .map(ToOwned::to_owned);
            let rules_injected = v
                .get("rules_injected")
                .and_then(Value::as_u64)
                .and_then(|n| usize::try_from(n).ok())
                .unwrap_or(0);
            let strict_match_count = v
                .get("strict_match_count")
                .and_then(Value::as_u64)
                .and_then(|n| usize::try_from(n).ok())
                .unwrap_or(0);
            let rule_titles: Vec<String> = v
                .get("rule_titles")
                .and_then(|x| x.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|t| t.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let rule_ids: Vec<String> = v
                .get("rule_ids")
                .and_then(|x| x.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|t| t.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let client_label = v
                .get("client_label")
                .and_then(|x| x.as_str())
                .map(ToOwned::to_owned);
            let repo_full_name = v
                .get("repo_full_name")
                .and_then(|x| x.as_str())
                .map(ToOwned::to_owned);
            Ok(
                match client
                    .track_mcp_query_outcome(
                        &file,
                        intent.as_deref(),
                        rules_injected,
                        strict_match_count,
                        rule_titles,
                        rule_ids,
                        client_label.as_deref(),
                        repo_full_name.as_deref(),
                    )
                    .await
                {
                    Ok(()) => DispatchOutcome::ok(true),
                    Err(failure) => DispatchOutcome::from_outbox_failure(&failure),
                },
            )
        }
        kind::IMPORTED_REVIEWS => {
            let req: UploadImportedReviewsRequest = serde_json::from_str(&item.payload_json)
                .map_err(|e| format!("imported_reviews parse: {e}"))?;
            Ok(match client.upload_imported_reviews_outcome(&req).await {
                Ok(()) => DispatchOutcome::ok(true),
                Err(failure) => DispatchOutcome::from_outbox_failure(&failure),
            })
        }
        kind::OBSERVATION => {
            let obs: super::api_types::Observation = serde_json::from_str(&item.payload_json)
                .map_err(|e| format!("observation parse: {e}"))?;
            Ok(
                match client
                    .post_observations_outcome(std::slice::from_ref(&obs))
                    .await
                {
                    Ok(()) => DispatchOutcome::ok(true),
                    Err(failure) => DispatchOutcome::from_outbox_failure(&failure),
                },
            )
        }
        other => Err(format!("unknown outbox kind '{other}'")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud::api_types::RecordAcceptedEditResponse;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

    async fn fresh_pool() -> SqlitePool {
        let opts = SqliteConnectOptions::new()
            .filename(":memory:")
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .expect("pool");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("apply migrations");
        pool
    }

    async fn status_of(pool: &SqlitePool, id: i64) -> Option<String> {
        sqlx::query_scalar!("SELECT status FROM cloud_outbox WHERE id = ?1", id)
            .fetch_optional(pool)
            .await
            .unwrap()
    }

    fn accepted_edit_response(
        acceptance_recorded: bool,
        team_id: Option<&str>,
        observations_inserted: u32,
    ) -> RecordAcceptedEditResponse {
        RecordAcceptedEditResponse {
            ok: acceptance_recorded,
            acceptance_recorded,
            acceptance_id: acceptance_recorded.then(|| "acceptance-1".to_owned()),
            diff_signature: Some("diff-1".to_owned()),
            team_id: team_id.map(str::to_owned),
            attributed_rule_ids: Vec::new(),
            observations_inserted,
            memory_reinforcement_recorded: false,
            memory_reinforcement_deduped: false,
            error: None,
        }
    }

    #[test]
    fn accepted_edit_attribution_summary_counts_launch_grade_uploads() {
        let response = accepted_edit_response(true, Some("team-1"), 2);
        let summary = accepted_edit_attribution_summary(2, &response);

        assert_eq!(summary.uploaded, 1);
        assert_eq!(summary.launch_grade, 1);
        assert_eq!(summary.warning_count(), 0);
    }

    #[test]
    fn accepted_edit_attribution_summary_flags_raw_only_uploads() {
        let missing_team =
            accepted_edit_attribution_summary(2, &accepted_edit_response(true, None, 2));
        assert_eq!(missing_team.uploaded, 1);
        assert_eq!(missing_team.launch_grade, 0);
        assert_eq!(missing_team.missing_team_workspace, 1);

        let missing_rule_ids =
            accepted_edit_attribution_summary(0, &accepted_edit_response(true, Some("team-1"), 0));
        assert_eq!(missing_rule_ids.missing_rule_ids, 1);
        assert_eq!(missing_rule_ids.launch_grade, 0);

        let unlinked_observation =
            accepted_edit_attribution_summary(2, &accepted_edit_response(true, Some("team-1"), 0));
        assert_eq!(unlinked_observation.unlinked_rule_observations, 1);
        assert_eq!(unlinked_observation.launch_grade, 0);
    }

    #[test]
    fn accepted_edit_attribution_summary_ignores_failed_uploads() {
        let response = accepted_edit_response(false, None, 0);
        let summary = accepted_edit_attribution_summary(0, &response);

        assert_eq!(summary.uploaded, 0);
        assert_eq!(summary.launch_grade, 0);
        assert_eq!(summary.warning_count(), 0);
    }

    #[tokio::test]
    async fn legacy_fix_acceptance_dispatch_skips_current_accepted_edit_pipeline() {
        let client = crate::cloud::client::CloudClient::new();
        let item = OutboxItem {
            id: 1,
            kind: kind::LEGACY_FIX_ACCEPTANCE.to_owned(),
            payload_json: "not accepted-edit json".to_owned(),
            retry_count: 0,
        };

        let outcome = dispatch(&client, &item)
            .await
            .expect("legacy rows are explicitly acknowledged and skipped");

        assert!(outcome.ok);
        assert_eq!(outcome.accepted_edit_attribution, None);
    }

    #[tokio::test]
    async fn enqueue_then_claim_moves_to_processing() {
        let pool = fresh_pool().await;
        let q = OutboxQueue::new(pool.clone());
        let id = q.enqueue("trajectory", "{}").await.unwrap();
        assert_eq!(status_of(&pool, id).await.as_deref(), Some("pending"));

        let item = q.claim_next().await.unwrap().expect("row claimed");
        assert_eq!(item.id, id);
        assert_eq!(status_of(&pool, id).await.as_deref(), Some("processing"));
    }

    #[tokio::test]
    async fn claim_next_kind_prioritizes_matching_kind() {
        let pool = fresh_pool().await;
        let q = OutboxQueue::new(pool.clone());
        let old_fix = q.enqueue(kind::ACCEPTED_EDIT, "{}").await.unwrap();
        let mcp = q.enqueue(kind::MCP_QUERY, "{}").await.unwrap();

        let item = q
            .claim_next_kind(kind::MCP_QUERY)
            .await
            .unwrap()
            .expect("mcp row claimed");

        assert_eq!(item.id, mcp);
        assert_eq!(status_of(&pool, mcp).await.as_deref(), Some("processing"));
        assert_eq!(status_of(&pool, old_fix).await.as_deref(), Some("pending"));
    }

    #[tokio::test]
    async fn drain_serialization_lock_is_process_wide() {
        let guard = drain_serialization_lock()
            .try_lock()
            .expect("first drain lock");
        assert!(
            drain_serialization_lock().try_lock().is_err(),
            "concurrent drainers must share the same in-process lock"
        );
        drop(guard);
        assert!(drain_serialization_lock().try_lock().is_ok());
    }

    #[tokio::test]
    async fn confirm_deletes_row() {
        let pool = fresh_pool().await;
        let q = OutboxQueue::new(pool.clone());
        let id = q.enqueue("trajectory", "{}").await.unwrap();
        let item = q.claim_next().await.unwrap().unwrap();
        q.confirm(item.id).await.unwrap();
        assert!(status_of(&pool, id).await.is_none());
    }

    #[tokio::test]
    async fn mark_failed_eight_times_abandons() {
        // A row survives 7 retried failures and is abandoned on the 8th
        // (`next_count == 8`, the `outbox_core::MAX_RETRY_COUNT` bound).
        let pool = fresh_pool().await;
        let q = OutboxQueue::new(pool.clone());
        let id = q.enqueue("trajectory", "{}").await.unwrap();

        // Attempts 1..=7 bounce the row back to `pending`. The breaker trips
        // after 3 consecutive failures (before the abandon at 8), so reset it
        // between attempts to keep `claim_next` returning the row; the on-disk
        // retry_count under test is unaffected by this reset.
        for attempt in 1..=7 {
            q.circuit_open_until_ms.store(0, Ordering::SeqCst);
            q.consecutive_failures.store(0, Ordering::SeqCst);
            let item = q.claim_next().await.unwrap().unwrap();
            q.mark_failed(item.id, &format!("net {attempt}"))
                .await
                .unwrap();
            assert_eq!(
                status_of(&pool, id).await.as_deref(),
                Some("pending"),
                "attempt {attempt}: retry_count {attempt} (< 8) must stay pending"
            );
        }

        // Attempt 8 — retry_count becomes 8, should transition to
        // abandoned.
        q.circuit_open_until_ms.store(0, Ordering::SeqCst);
        q.consecutive_failures.store(0, Ordering::SeqCst);
        let item = q.claim_next().await.unwrap().unwrap();
        q.mark_failed(item.id, "net 8").await.unwrap();
        assert_eq!(status_of(&pool, id).await.as_deref(), Some("abandoned"));

        // Abandoned rows are NOT re-claimable. Force-close the breaker
        // first so the assertion is about abandonment, not the breaker.
        q.circuit_open_until_ms.store(0, Ordering::SeqCst);
        q.consecutive_failures.store(0, Ordering::SeqCst);
        assert!(q.claim_next().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn claim_next_auto_recovers_stale_processing_rows() {
        // Crashed drain: enqueue, claim, never confirm, then backdate
        // `claimed_at` past the stale threshold. A later `claim_next` must
        // self-heal the row without an explicit `reset_stale` call.
        let pool = fresh_pool().await;
        let q = OutboxQueue::new(pool.clone());
        let id = q.enqueue("trajectory", "{\"crashed\":true}").await.unwrap();

        // First claim — simulates the drain that subsequently dies.
        let first = q.claim_next().await.unwrap().expect("first claim");
        assert_eq!(first.id, id);
        assert_eq!(status_of(&pool, id).await.as_deref(), Some("processing"));

        // Backdate `claimed_at` to push the row past
        // `DEFAULT_STALE_SECONDS`.
        let stale = now_unix_ms() - (DEFAULT_STALE_SECONDS as i64 + 30) * 1000;
        sqlx::query!(
            "UPDATE cloud_outbox SET claimed_at = ?1 WHERE id = ?2",
            stale,
            id
        )
        .execute(&pool)
        .await
        .unwrap();

        // Second claim from the "recovered" caller — must pick up the
        // stale row without any intermediate `reset_stale` call.
        let recovered = q.claim_next().await.unwrap().expect("recovered claim");
        assert_eq!(recovered.id, id, "stale row must be re-claimable");
        assert_eq!(status_of(&pool, id).await.as_deref(), Some("processing"));
    }

    #[tokio::test]
    async fn claim_next_ignores_fresh_processing_rows() {
        // A still-fresh `processing` row (within the stale window) must
        // NOT be re-claimed — that would let two drainers race on the
        // same payload and duplicate the cloud upload.
        let pool = fresh_pool().await;
        let q = OutboxQueue::new(pool.clone());
        let _fresh = q.enqueue("trajectory", "{}").await.unwrap();
        let item = q.claim_next().await.unwrap().expect("initial claim");

        // With no pending rows left AND the only processing row still
        // fresh, claim_next must return None.
        assert!(q.claim_next().await.unwrap().is_none());
        // Sanity: confirm cleans up.
        q.confirm(item.id).await.unwrap();
    }

    #[tokio::test]
    async fn reset_stale_promotes_processing_rows() {
        let pool = fresh_pool().await;
        let q = OutboxQueue::new(pool.clone());
        let id = q.enqueue("trajectory", "{}").await.unwrap();
        let _ = q.claim_next().await.unwrap().unwrap();
        assert_eq!(status_of(&pool, id).await.as_deref(), Some("processing"));

        // Backdate claimed_at so the threshold fires.
        let backdated = now_unix_ms() - 120_000;
        sqlx::query!(
            "UPDATE cloud_outbox SET claimed_at = ?1 WHERE id = ?2",
            backdated,
            id
        )
        .execute(&pool)
        .await
        .unwrap();

        let promoted = q.reset_stale(60).await.unwrap();
        assert_eq!(promoted, 1);
        assert_eq!(status_of(&pool, id).await.as_deref(), Some("pending"));
    }

    #[tokio::test]
    async fn circuit_breaker_halts_claims_after_three_failures() {
        let pool = fresh_pool().await;
        let q = OutboxQueue::new(pool.clone());

        // Enqueue four rows so we have at least one left after tripping.
        for i in 0..4 {
            q.enqueue("trajectory", &format!("{{\"i\":{i}}}"))
                .await
                .unwrap();
        }

        for _ in 0..3 {
            let item = q.claim_next().await.unwrap().unwrap();
            q.mark_failed(item.id, "x").await.unwrap();
        }

        // Breaker is open; claim_next must return None even though a
        // pending row still exists.
        assert!(matches!(q.circuit_state(), CircuitState::Open { .. }));
        assert!(q.claim_next().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn confirm_resets_consecutive_failure_counter() {
        let pool = fresh_pool().await;
        let q = OutboxQueue::new(pool.clone());

        // Two failures tick the counter to 2 (still below the threshold
        // of 3, so the breaker stays closed).
        let _id1 = q.enqueue("trajectory", "{}").await.unwrap();
        let _id2 = q.enqueue("trajectory", "{}").await.unwrap();

        let item = q.claim_next().await.unwrap().unwrap();
        q.mark_failed(item.id, "f1").await.unwrap();
        let item = q.claim_next().await.unwrap().unwrap();
        q.mark_failed(item.id, "f2").await.unwrap();
        assert_eq!(q.consecutive_failures.load(Ordering::SeqCst), 2);

        // A successful confirm in between must reset the counter. We
        // don't care which physical row got claimed — only that a
        // successful confirm clears the consecutive-failure state.
        let item = q.claim_next().await.unwrap().unwrap();
        q.confirm(item.id).await.unwrap();
        assert_eq!(q.consecutive_failures.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn claim_next_returns_observation_kind() {
        let pool = fresh_pool().await;
        let q = OutboxQueue::new(pool.clone());
        let obs_id = q
            .enqueue(kind::OBSERVATION, r#"{"session_id":"s"}"#)
            .await
            .unwrap();
        let traj_id = q.enqueue(kind::TRAJECTORY, "{}").await.unwrap();

        let first = q.claim_next().await.unwrap().expect("claimed first");
        let second = q.claim_next().await.unwrap().expect("claimed second");
        assert_eq!(first.id, obs_id);
        assert_eq!(first.kind, kind::OBSERVATION);
        assert_eq!(second.id, traj_id);
    }

    #[tokio::test]
    async fn claim_next_returns_oldest_first() {
        let pool = fresh_pool().await;
        let q = OutboxQueue::new(pool.clone());
        let a = q.enqueue("trajectory", r#"{"n":"a"}"#).await.unwrap();
        // Tiny sleep to guarantee distinct created_at timestamps.
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        let b = q.enqueue("trajectory", r#"{"n":"b"}"#).await.unwrap();

        let first = q.claim_next().await.unwrap().unwrap();
        let second = q.claim_next().await.unwrap().unwrap();
        assert_eq!(first.id, a);
        assert_eq!(second.id, b);
    }

    /// Helper: insert a directly-abandoned row at a chosen `created_at`
    /// (in unix-ms). Bypasses the public `enqueue`/`mark_failed` path so
    /// the cutoff-window tests don't need to fake 8 round-trips per row.
    async fn insert_abandoned(
        pool: &SqlitePool,
        kind: &str,
        created_at_ms: i64,
        claimed_at_ms: Option<i64>,
    ) -> i64 {
        sqlx::query(
            "INSERT INTO cloud_outbox \
             (kind, payload_json, status, retry_count, created_at, claimed_at, last_error) \
             VALUES (?1, '{}', 'abandoned', ?2, ?3, ?4, 'upload returned non-2xx')",
        )
        .bind(kind)
        .bind(MAX_RETRY_COUNT)
        .bind(created_at_ms)
        .bind(claimed_at_ms)
        .execute(pool)
        .await
        .unwrap()
        .last_insert_rowid()
    }

    #[tokio::test]
    async fn drain_abandoned_dry_run_reports_per_kind_without_mutating() {
        let pool = fresh_pool().await;
        let q = OutboxQueue::new(pool.clone());
        let now = now_unix_ms();
        let old = now - 31 * 86_400_000; // 31 days ago
        let mcp_id = insert_abandoned(&pool, "mcp_query", old, Some(old)).await;
        let obs_id = insert_abandoned(&pool, "observation", old, Some(old)).await;
        let _other_mcp = insert_abandoned(&pool, "mcp_query", old, Some(old)).await;

        let summary = q.drain_abandoned_older_than(now, true).await.unwrap();

        assert_eq!(summary.total, 3);
        // Sorted ascending by kind for deterministic doctor output.
        assert_eq!(
            summary.per_kind,
            vec![("mcp_query".to_owned(), 2), ("observation".to_owned(), 1),]
        );
        // Dry-run MUST NOT mutate any row.
        assert_eq!(status_of(&pool, mcp_id).await.as_deref(), Some("abandoned"));
        assert_eq!(status_of(&pool, obs_id).await.as_deref(), Some("abandoned"));
    }

    #[tokio::test]
    async fn drain_abandoned_real_resets_eligible_rows_only() {
        let pool = fresh_pool().await;
        let q = OutboxQueue::new(pool.clone());
        let now = now_unix_ms();
        let old = now - 31 * 86_400_000;
        let fresh = now - 60_000; // 60s ago — must be left alone
        let cutoff = now - 7 * 86_400_000; // older-than-7d

        let old_row = insert_abandoned(&pool, "mcp_query", old, Some(old)).await;
        let fresh_row = insert_abandoned(&pool, "mcp_query", fresh, Some(fresh)).await;

        // Tick the in-process breaker into the "open" state so we can
        // assert the drain hygienically resets it on success.
        q.consecutive_failures
            .store(CIRCUIT_FAILURE_THRESHOLD, Ordering::SeqCst);
        q.circuit_open_until_ms
            .store(now + 60_000, Ordering::SeqCst);

        let summary = q.drain_abandoned_older_than(cutoff, false).await.unwrap();
        assert_eq!(summary.total, 1);
        assert_eq!(status_of(&pool, old_row).await.as_deref(), Some("pending"));
        assert_eq!(
            status_of(&pool, fresh_row).await.as_deref(),
            Some("abandoned"),
            "rows newer than cutoff must NOT be touched",
        );

        // Resurrected row must come back with retry_count cleared.
        let retry_count: i64 =
            sqlx::query_scalar("SELECT retry_count FROM cloud_outbox WHERE id = ?1")
                .bind(old_row)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(retry_count, 0);
        let last_error: Option<String> =
            sqlx::query_scalar("SELECT last_error FROM cloud_outbox WHERE id = ?1")
                .bind(old_row)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(last_error.is_none());

        // Breaker state cleared so the next drain pass isn't short-circuited
        // by a stale in-process counter from the auth-revoke storm.
        assert_eq!(q.consecutive_failures.load(Ordering::SeqCst), 0);
        assert_eq!(q.circuit_open_until_ms.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn drain_abandoned_uses_created_at_when_no_claimed_at() {
        // Rows abandoned via decode failure / bookkeeping never get a
        // `claimed_at`. The cutoff must still apply via `created_at`
        // so they don't sit around forever.
        let pool = fresh_pool().await;
        let q = OutboxQueue::new(pool.clone());
        let now = now_unix_ms();
        let old = now - 90 * 86_400_000;
        let id = insert_abandoned(&pool, "observation", old, None).await;
        let cutoff = now - 30 * 86_400_000;

        let summary = q.drain_abandoned_older_than(cutoff, false).await.unwrap();
        assert_eq!(summary.total, 1);
        assert_eq!(status_of(&pool, id).await.as_deref(), Some("pending"));
    }

    #[tokio::test]
    async fn drain_abandoned_empty_queue_is_a_noop() {
        let pool = fresh_pool().await;
        let q = OutboxQueue::new(pool.clone());
        let summary = q
            .drain_abandoned_older_than(now_unix_ms(), false)
            .await
            .unwrap();
        assert_eq!(summary.total, 0);
        assert!(summary.per_kind.is_empty());
    }

    #[tokio::test]
    async fn pending_counts_by_kind_buckets_pending_rows() {
        let pool = fresh_pool().await;
        let q = OutboxQueue::new(pool.clone());
        q.enqueue("mcp_query", "{}").await.unwrap();
        q.enqueue("mcp_query", "{}").await.unwrap();
        q.enqueue("observation", "{}").await.unwrap();

        let counts = q.pending_counts_by_kind().await.unwrap();
        assert_eq!(
            counts,
            vec![("mcp_query".to_owned(), 2), ("observation".to_owned(), 1),]
        );
    }

    // Structured `last_error` regression coverage.

    use crate::cloud::client::{HttpFailure, OutboxFailure, normalize_body_snippet};

    #[test]
    fn normalize_body_snippet_collapses_whitespace_runs_to_single_spaces() {
        let raw = "  line one  \n\nline two\t\twith\ttabs   ";
        let snippet = normalize_body_snippet(raw, 200);
        assert_eq!(snippet, "line one line two with tabs");
        assert!(!snippet.contains('\n'));
        assert!(!snippet.contains('\t'));
    }

    #[test]
    fn normalize_body_snippet_caps_to_max_chars_without_splitting_utf8() {
        // Two-codepoint emoji + ASCII so the cap lands at codepoint 5
        // (not byte 5, which would slice mid-codepoint and panic on
        // `String::from_utf8` if we'd been sloppy).
        let raw = "😀😀😀😀😀ASCII tail";
        let snippet = normalize_body_snippet(raw, 5);
        assert_eq!(snippet.chars().count(), 5);
        assert_eq!(snippet, "😀😀😀😀😀");
    }

    #[test]
    fn outbox_failure_http_with_body_matches_spec_format() {
        // HTTP failures persist `{status} {reason}: {body_snippet}`.
        let failure = OutboxFailure::Http(HttpFailure {
            status: 401,
            reason_phrase: "Unauthorized".to_owned(),
            body_snippet: r#"{"error":"session_revoked"}"#.to_owned(),
        });
        assert_eq!(
            failure.format_for_outbox_last_error(),
            r#"401 Unauthorized: {"error":"session_revoked"}"#
        );
    }

    #[test]
    fn outbox_failure_http_with_empty_body_omits_trailing_colon() {
        let failure = OutboxFailure::Http(HttpFailure {
            status: 500,
            reason_phrase: "Internal Server Error".to_owned(),
            body_snippet: String::new(),
        });
        assert_eq!(
            failure.format_for_outbox_last_error(),
            "500 Internal Server Error",
        );
    }

    #[test]
    fn outbox_failure_transport_uses_distinct_sentinel_not_non_2xx_literal() {
        let failure = OutboxFailure::Transport("dns lookup failed: timed out".to_owned());
        let formatted = failure.format_for_outbox_last_error();
        assert!(formatted.starts_with("transport: "));
        assert!(formatted.contains("dns lookup failed"));
        // Keep transport failures out of the generic non-2xx bucket.
        assert!(
            !formatted.contains("non-2xx"),
            "transport failures must not collapse to the legacy 'non-2xx' bucket"
        );
    }

    #[tokio::test]
    async fn mark_failed_persists_dispatchoutcome_last_error_verbatim() {
        // Rich dispatch errors must reach `cloud_outbox.last_error`
        // unchanged except for the 2 KB safety trim.
        let pool = fresh_pool().await;
        let q = OutboxQueue::new(pool.clone());
        let id = q.enqueue("trajectory", "{}").await.unwrap();
        let _claimed = q.claim_next().await.unwrap().expect("row claimed");

        let formatted = OutboxFailure::Http(HttpFailure {
            status: 401,
            reason_phrase: "Unauthorized".to_owned(),
            body_snippet: r#"{"error":"session_revoked"}"#.to_owned(),
        })
        .format_for_outbox_last_error();
        q.mark_failed(id, &formatted).await.unwrap();

        let stored: Option<String> =
            sqlx::query_scalar!("SELECT last_error FROM cloud_outbox WHERE id = ?1", id)
                .fetch_one(&pool)
                .await
                .unwrap();
        let stored = stored.expect("mark_failed must populate last_error");
        assert!(stored.starts_with("401 Unauthorized:"));
        assert!(stored.contains("session_revoked"));
        // Do not collapse status + body into the generic placeholder.
        assert_ne!(stored, "upload returned non-2xx");
    }

    #[test]
    fn dispatch_outcome_from_outbox_failure_propagates_spec_format() {
        // The dispatch failure builder must preserve the persisted
        // error format.
        let outcome = DispatchOutcome::from_outbox_failure(&OutboxFailure::Http(HttpFailure {
            status: 401,
            reason_phrase: "Unauthorized".to_owned(),
            body_snippet: r#"{"error":"session_revoked"}"#.to_owned(),
        }));
        assert!(!outcome.ok);
        let last = outcome
            .last_error
            .expect("failures must always carry a last_error");
        assert!(last.starts_with("401 Unauthorized:"));
        assert!(last.contains("session_revoked"));
    }
}
