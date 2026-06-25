use super::events::ObservationEvent;
use super::storage::{MAX_FLUSH_BATCH, ObservationEmitter, now_unix_ms, truncate};
use crate::cloud::client::OutboxFailure;
use crate::cloud::outbox_core::{backoff_delay_ms, jitter_ms};
use sqlx::Row;
use std::future::Future;

/// Rows older than this are abandoned even while still failing transiently or
/// rate-limited, so a long cloud outage can't grow the queue without bound
/// (the 10k `cap_queue` cap is the harder backstop). 7 days.
const OBSERVATION_MAX_AGE_MS: i64 = 7 * 24 * 60 * 60 * 1000;
/// Default backpressure delay for a 429 whose body carries no parseable
/// `retryAfterSec`.
const RATE_LIMIT_DEFAULT_MS: i64 = 60 * 1000;
/// Clamp a server-provided Retry-After so a bogus huge value can't park a row
/// for an unreasonable time.
const RATE_LIMIT_RETRY_CAP_MS: i64 = 5 * 60 * 1000;
/// How long a claimed (in-flight) row is hidden from other flushers before it
/// may be reclaimed — covers a crashed sender. Reuses `next_attempt_at_ms` as
/// the lease, so no schema change is needed.
const CLAIM_LEASE_MS: i64 = 60 * 1000;
/// Max parked rows un-parked after a single successful flush, so a recovered
/// auth/team block doesn't stampede the server.
const UNPARK_ON_SUCCESS_LIMIT: i64 = 256;

/// How a failed upload should be handled, derived from the server's response.
/// Replaces the old "abandon after 8 attempts regardless of why it failed".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeliveryClass {
    /// 5xx / transport / decode: retry with exponential backoff + jitter;
    /// abandon only when the row ages out.
    Transient,
    /// HTTP 429: honor the server's Retry-After; does not consume the backoff
    /// counter (pure backpressure), but is still age-bounded.
    RateLimited { retry_after_ms: i64 },
    /// 401/403 (e.g. `no_team`): recoverable once the user logs in / joins a
    /// team. Park (never abandon) and un-park on the next success/login.
    AuthBlocked,
    /// 400/404/405/409/410/422: malformed or gone — retrying won't help.
    /// Abandon with a `permanent:` tag (kept for diagnostics, never deleted).
    PermanentInvalid,
}

/// Classify a delivery failure. `OutboxFailure` collapses transport and
/// decode errors into `Transport`, so both are treated as transient (retry,
/// age-out) — acceptable because the cloud ingest is idempotent on
/// `(team_id, content_hash)`, so a replayed event can't double-count.
fn classify(failure: &OutboxFailure) -> DeliveryClass {
    // Transport / decode errors: retry transiently.
    let OutboxFailure::Http(http) = failure else {
        return DeliveryClass::Transient;
    };
    let status = http.status;
    if status == 429 {
        return DeliveryClass::RateLimited {
            retry_after_ms: parse_retry_after_ms(&http.body_snippet)
                .unwrap_or(RATE_LIMIT_DEFAULT_MS),
        };
    }
    if status == 401 || status == 403 {
        return DeliveryClass::AuthBlocked;
    }
    // Other 4xx won't change on retry — except the timeout-ish ones (408
    // Request Timeout, 425 Too Early), which are transient.
    if (400..500).contains(&status) && status != 408 && status != 425 {
        return DeliveryClass::PermanentInvalid;
    }
    // 5xx, 408, 425, and anything unexpected: retry transiently.
    DeliveryClass::Transient
}

/// Pull a `retryAfterSec` integer out of a 429 body fragment and convert to
/// clamped milliseconds. Returns `None` when the body carries no number
/// (e.g. the `Defined` error variant drops the `data` field).
fn parse_retry_after_ms(body: &str) -> Option<i64> {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(body) {
        return retry_after_sec_from_json(&value).map(retry_after_ms_from_secs);
    }

    parse_retry_after_ms_from_fragment(body)
}

fn retry_after_sec_from_json(value: &serde_json::Value) -> Option<i64> {
    match value {
        serde_json::Value::Object(map) => map
            .get("retryAfterSec")
            .and_then(serde_json::Value::as_i64)
            .or_else(|| map.values().find_map(retry_after_sec_from_json)),
        serde_json::Value::Array(values) => values.iter().find_map(retry_after_sec_from_json),
        _ => None,
    }
}

fn parse_retry_after_ms_from_fragment(body: &str) -> Option<i64> {
    let idx = body.find(r#""retryAfterSec""#)?;
    let rest = body[idx + r#""retryAfterSec""#.len()..].trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    let digit_len = rest
        .as_bytes()
        .iter()
        .take_while(|b| b.is_ascii_digit())
        .count();
    if digit_len == 0 {
        return None;
    }
    let digits = &rest[..digit_len];
    let secs: i64 = digits.parse().ok()?;
    Some(retry_after_ms_from_secs(secs))
}

fn retry_after_ms_from_secs(secs: i64) -> i64 {
    secs.saturating_mul(1000)
        .clamp(1000, RATE_LIMIT_RETRY_CAP_MS)
}

trait ObservationUploadClient {
    fn logged_in(&self) -> bool;

    fn upload_observation_events<'a>(
        &'a self,
        batch: &'a [ObservationEvent],
    ) -> impl Future<Output = crate::Result<(), OutboxFailure>> + Send + 'a;
}

impl ObservationUploadClient for crate::cloud::client::CloudClient {
    fn logged_in(&self) -> bool {
        self.is_logged_in()
    }

    fn upload_observation_events<'a>(
        &'a self,
        batch: &'a [ObservationEvent],
    ) -> impl Future<Output = crate::Result<(), OutboxFailure>> + Send + 'a {
        self.post_observation_events_outcome(batch)
    }
}

struct ClaimedObservation {
    id: i64,
    event: ObservationEvent,
    retry_count: i64,
    created_at_ms: i64,
}

impl ObservationEmitter {
    pub async fn retry_pending_uploads_now(&self) -> crate::Result<u64> {
        let now = now_unix_ms();
        // Explicit "retry now": make scheduled-later pending rows due now AND
        // un-park auth-blocked rows. This runs right after `cloud login` /
        // `cloud sync`, when a prior `no_team`/auth block may have just been
        // resolved, so parked rows deserve another attempt. Crucially it only
        // touches `pending`/`parked`, never `sending`: an in-flight row owned
        // by an active flusher's lease must not be yanked out from under it
        // (that would drop the flusher's terminal write and re-enable an
        // immediate duplicate POST).
        let result = sqlx::query(
            "UPDATE observation_events \
             SET status = 'pending', next_attempt_at_ms = ?1 \
             WHERE (status = 'pending' AND next_attempt_at_ms > ?1) \
                OR status = 'parked'",
        )
        .bind(now)
        .execute(self.pool())
        .await
        .map_err(|e| {
            crate::CoreError::internal(format!("reset pending observation retry time: {e}"))
        })?;
        Ok(result.rows_affected())
    }

    pub async fn flush_to_cloud(
        &self,
        client: &crate::cloud::client::CloudClient,
    ) -> crate::Result<(usize, usize)> {
        self.flush_to_cloud_with(client).await
    }

    async fn flush_to_cloud_with<C: ObservationUploadClient>(
        &self,
        client: &C,
    ) -> crate::Result<(usize, usize)> {
        if !client.logged_in() {
            return Ok((0, 0));
        }

        let now = now_unix_ms();
        // Atomically CLAIM a batch by moving rows to the in-flight `sending`
        // state and leasing them (`next_attempt_at_ms = lease`). A distinct
        // state — not "pending with a future next_attempt" — is what keeps
        // `retry_pending_uploads_now` (which only touches `pending`/`parked`)
        // from yanking a row out from under an active flusher in another
        // process (this machine runs several `mcp-server` procs). A crashed
        // sender's lease expires and the row (still `sending`) becomes
        // re-claimable once `next_attempt_at_ms <= now` again.
        let lease_until = now.saturating_add(CLAIM_LEASE_MS);
        let rows = sqlx::query(
            "UPDATE observation_events SET status = 'sending', next_attempt_at_ms = ?1 \
             WHERE id IN ( \
               SELECT id FROM observation_events \
               WHERE status IN ('pending', 'sending') AND next_attempt_at_ms <= ?2 \
               ORDER BY created_at_ms ASC, id ASC LIMIT ?3 \
             ) \
             RETURNING id, payload_json, retry_count, created_at_ms",
        )
        .bind(lease_until)
        .bind(now)
        .bind(MAX_FLUSH_BATCH)
        .fetch_all(self.pool())
        .await
        .map_err(|e| crate::CoreError::internal(format!("claim observation batch: {e}")))?;

        if rows.is_empty() {
            return Ok((0, 0));
        }

        let mut claimed = Vec::with_capacity(rows.len());
        for row in rows {
            let id: i64 = row.try_get("id").map_err(|e| {
                crate::CoreError::internal(format!("decode claimed observation id: {e}"))
            })?;
            let payload: String = row.try_get("payload_json").unwrap_or_default();
            let retry_count: i64 = row.try_get("retry_count").unwrap_or_default();
            let created_at_ms: i64 = row.try_get("created_at_ms").unwrap_or_default();
            match serde_json::from_str::<ObservationEvent>(&payload) {
                Ok(event) => claimed.push(ClaimedObservation {
                    id,
                    event,
                    retry_count,
                    created_at_ms,
                }),
                Err(e) => {
                    // A row that won't decode will never decode: drop it as a
                    // permanent failure (kept as `abandoned`, not deleted).
                    self.abandon(
                        id,
                        &format!("permanent: decode observation event: {e}"),
                        lease_until,
                    )
                    .await?;
                }
            }
        }

        if claimed.is_empty() {
            return Ok((0, 0));
        }

        let events: Vec<_> = claimed.iter().map(|row| row.event.clone()).collect();
        let attempted = events.len();
        // Try the whole claimed batch in ONE request first.
        let batch_failure = match client.upload_observation_events(&events).await {
            Ok(()) => {
                let sent_at = now_unix_ms();
                for row in &claimed {
                    self.mark_sent(row.id, sent_at).await?;
                }
                // A success proves auth + team work now, so retry parked rows.
                let _ = self.unpark_after_success().await;
                let _ = self.cap_queue().await;
                return Ok((attempted, attempted));
            }
            Err(failure) => failure,
        };

        // The batch failed. Classify it ONCE. A shared failure (rate limit,
        // auth block, transient/transport) applies to every claimed row, so
        // fanning out into per-row POSTs would multiply one 429 into 65
        // requests and burn the rate budget further. Mark all claimed rows
        // with the shared verdict and stop — only a permanent/invalid batch is
        // worth per-row isolation (one malformed row can reject the batch).
        if !matches!(classify(&batch_failure), DeliveryClass::PermanentInvalid) {
            for row in &claimed {
                self.mark_failed(
                    row.id,
                    row.retry_count,
                    row.created_at_ms,
                    &batch_failure,
                    lease_until,
                )
                .await?;
            }
            let _ = self.cap_queue().await;
            return Ok((attempted, 0));
        }

        // Permanent/invalid batch: isolate the offending row(s) by retrying
        // each singly so good rows still get through.
        let sent = self
            .isolate_permanent_invalid_batch(client, &claimed, lease_until)
            .await?;
        if sent > 0 {
            let _ = self.unpark_after_success().await;
        }
        let _ = self.cap_queue().await;
        Ok((attempted, sent))
    }

    async fn isolate_permanent_invalid_batch<C: ObservationUploadClient>(
        &self,
        client: &C,
        claimed: &[ClaimedObservation],
        lease: i64,
    ) -> crate::Result<usize> {
        let mut sent = 0usize;
        let mut pending_permanent = Vec::<(usize, OutboxFailure)>::new();

        for (idx, row) in claimed.iter().enumerate() {
            match client
                .upload_observation_events(std::slice::from_ref(&row.event))
                .await
            {
                Ok(()) => {
                    self.mark_sent(row.id, now_unix_ms()).await?;
                    sent += 1;

                    if sent == 1 {
                        let proven_permanent = std::mem::take(&mut pending_permanent);
                        for (permanent_idx, failure) in proven_permanent {
                            let failed = &claimed[permanent_idx];
                            self.mark_failed(
                                failed.id,
                                failed.retry_count,
                                failed.created_at_ms,
                                &failure,
                                lease,
                            )
                            .await?;
                        }
                    }
                }
                Err(failure) => match classify(&failure) {
                    DeliveryClass::PermanentInvalid if sent > 0 || claimed.len() == 1 => {
                        self.mark_failed(
                            row.id,
                            row.retry_count,
                            row.created_at_ms,
                            &failure,
                            lease,
                        )
                        .await?;
                    }
                    DeliveryClass::PermanentInvalid => {
                        pending_permanent.push((idx, failure));
                    }
                    _ => {
                        // A singleton retry hit a shared/recoverable failure
                        // after isolation started. Stop fanning out and apply
                        // that shared verdict to every row still owned by this
                        // claim, including earlier ambiguous 4xx candidates.
                        for (permanent_idx, _) in std::mem::take(&mut pending_permanent) {
                            let failed = &claimed[permanent_idx];
                            self.mark_failed(
                                failed.id,
                                failed.retry_count,
                                failed.created_at_ms,
                                &failure,
                                lease,
                            )
                            .await?;
                        }
                        for remaining in &claimed[idx..] {
                            self.mark_failed(
                                remaining.id,
                                remaining.retry_count,
                                remaining.created_at_ms,
                                &failure,
                                lease,
                            )
                            .await?;
                        }
                        return Ok(sent);
                    }
                },
            }
        }

        for (idx, failure) in pending_permanent {
            let row = &claimed[idx];
            // If every singleton got a permanent-looking 4xx, isolation did
            // not prove row-specific invalidity. Keep rows retryable so a
            // request-level schema/route drift cannot abandon a valid batch.
            self.mark_failed_with_class(
                row.id,
                row.retry_count,
                row.created_at_ms,
                &failure,
                lease,
                DeliveryClass::Transient,
            )
            .await?;
        }

        Ok(sent)
    }

    /// Decide what to do with a row whose upload just failed, based on the
    /// classified failure rather than a blind attempt count:
    /// - `AuthBlocked` (401/403) → park (recoverable; never abandoned here)
    /// - `PermanentInvalid` (malformed/gone) → abandon, tagged `permanent:`
    /// - `RateLimited` (429) → reschedule by the server's Retry-After; does
    ///   not consume the backoff counter, but is still age-bounded
    /// - `Transient` (5xx/transport) → exponential backoff + jitter, abandon
    ///   only once the row exceeds `OBSERVATION_MAX_AGE_MS`
    pub(super) async fn mark_failed(
        &self,
        id: i64,
        retry_count: i64,
        created_at_ms: i64,
        failure: &OutboxFailure,
        lease: i64,
    ) -> crate::Result<()> {
        self.mark_failed_with_class(
            id,
            retry_count,
            created_at_ms,
            failure,
            lease,
            classify(failure),
        )
        .await
    }

    async fn mark_failed_with_class(
        &self,
        id: i64,
        retry_count: i64,
        created_at_ms: i64,
        failure: &OutboxFailure,
        lease: i64,
        class: DeliveryClass,
    ) -> crate::Result<()> {
        let now = now_unix_ms();
        let msg = failure.format_for_outbox_last_error();
        let aged_out = now.saturating_sub(created_at_ms) > OBSERVATION_MAX_AGE_MS;

        match class {
            DeliveryClass::AuthBlocked => self.park(id, &msg, lease).await,
            DeliveryClass::PermanentInvalid => {
                self.abandon(id, &truncate(&format!("permanent: {msg}"), 2048), lease)
                    .await
            }
            DeliveryClass::RateLimited { retry_after_ms } => {
                if aged_out {
                    return self
                        .abandon(
                            id,
                            &truncate(&format!("aged-out (rate-limited): {msg}"), 2048),
                            lease,
                        )
                        .await;
                }
                // Pure backpressure: honor Retry-After, keep the retry count
                // unchanged so a long 429 window can't burn the budget.
                let delay = retry_after_ms.saturating_add(jitter_ms(retry_after_ms, now ^ id));
                self.reschedule(id, retry_count, now.saturating_add(delay), &msg, lease)
                    .await
            }
            DeliveryClass::Transient => {
                if aged_out {
                    return self
                        .abandon(id, &truncate(&format!("aged-out: {msg}"), 2048), lease)
                        .await;
                }
                let next_count = retry_count + 1;
                let base = backoff_delay_ms(next_count);
                let delay = base.saturating_add(jitter_ms(base, now ^ id));
                self.reschedule(id, next_count, now.saturating_add(delay), &msg, lease)
                    .await
            }
        }
    }

    /// Re-arm a row for a later retry: persist the (possibly unchanged) retry
    /// count, the next-attempt time, and the last error, leaving it `pending`.
    ///
    /// Lease-fenced: the `WHERE next_attempt_at_ms = ?lease` clause means the
    /// write only lands if we still hold the claim we took in `flush_to_cloud`.
    /// If our lease expired and another process reclaimed the row, this no-ops
    /// instead of clobbering the new owner's state (a stale re-pending).
    async fn reschedule(
        &self,
        id: i64,
        retry_count: i64,
        next_attempt_at_ms: i64,
        err: &str,
        lease: i64,
    ) -> crate::Result<()> {
        sqlx::query(
            "UPDATE observation_events \
             SET status = 'pending', retry_count = ?1, next_attempt_at_ms = ?2, last_error = ?3 \
             WHERE id = ?4 AND status = 'sending' AND next_attempt_at_ms = ?5",
        )
        .bind(retry_count)
        .bind(next_attempt_at_ms)
        .bind(truncate(err, 2048))
        .bind(id)
        .bind(lease)
        .execute(self.pool())
        .await
        .map_err(|e| crate::CoreError::internal(format!("reschedule observation: {e}")))?;
        Ok(())
    }

    /// Move a row to the `parked` state: an auth/team block that will clear on
    /// its own (login / team assignment), so the row is held — not abandoned,
    /// not counted as a normal pending upload — until [`Self::unpark_after_success`]
    /// or [`Self::retry_pending_uploads_now`] revives it. Lease-fenced.
    async fn park(&self, id: i64, err: &str, lease: i64) -> crate::Result<()> {
        sqlx::query(
            "UPDATE observation_events SET status = 'parked', last_error = ?1 \
             WHERE id = ?2 AND status = 'sending' AND next_attempt_at_ms = ?3",
        )
        .bind(truncate(err, 2048))
        .bind(id)
        .bind(lease)
        .execute(self.pool())
        .await
        .map_err(|e| crate::CoreError::internal(format!("park observation: {e}")))?;
        Ok(())
    }

    /// After a successful upload (proof that auth + team now work), revive a
    /// bounded, spread-out batch of `parked` rows. Bounded by
    /// `UNPARK_ON_SUCCESS_LIMIT` and spread over ~5s (`id % 5000`) so a cleared
    /// block doesn't stampede the server, and never touches `abandoned`
    /// (permanent) rows.
    async fn unpark_after_success(&self) -> crate::Result<u64> {
        let now = now_unix_ms();
        let result = sqlx::query(
            "UPDATE observation_events \
             SET status = 'pending', retry_count = 0, \
                 next_attempt_at_ms = ?1 + (id % 5000), last_error = NULL \
             WHERE id IN ( \
               SELECT id FROM observation_events \
               WHERE status = 'parked' ORDER BY created_at_ms ASC, id ASC LIMIT ?2 \
             )",
        )
        .bind(now)
        .bind(UNPARK_ON_SUCCESS_LIMIT)
        .execute(self.pool())
        .await
        .map_err(|e| crate::CoreError::internal(format!("unpark observations: {e}")))?;
        Ok(result.rows_affected())
    }

    pub(super) async fn mark_sent(&self, id: i64, sent_at_ms: i64) -> crate::Result<()> {
        sqlx::query("UPDATE observation_events SET status = 'sent', sent_at_ms = ?1 WHERE id = ?2")
            .bind(sent_at_ms)
            .bind(id)
            .execute(self.pool())
            .await
            .map_err(|e| crate::CoreError::internal(format!("mark observation sent: {e}")))?;
        Ok(())
    }

    /// Resurrect `abandoned` observation_events rows older than `cutoff_unix_ms`
    /// back to `pending`. Returns the rows reset (or that would reset, in
    /// `dry_run` mode), bucketed by `event_type` and sorted ascending so doctor
    /// output is stable.
    ///
    /// Runs in a single transaction so a partial drain cannot leave the queue
    /// half-reset; `dry_run = true` rolls back instead of committing.
    ///
    /// Eligibility is by `created_at_ms`, not `next_attempt_at_ms`: the latter
    /// isn't carried forward when a row is abandoned, so `created_at_ms` is the
    /// stable age signal.
    pub async fn drain_abandoned_older_than(
        &self,
        cutoff_unix_ms: i64,
        dry_run: bool,
    ) -> crate::Result<crate::cloud::outbox::DrainSummary> {
        let mut tx = self
            .pool()
            .begin()
            .await
            .map_err(|e| crate::CoreError::internal(format!("begin drain tx: {e}")))?;

        let rows = sqlx::query(
            "SELECT event_type, COUNT(*) AS c \
             FROM observation_events \
             WHERE status = 'abandoned' AND created_at_ms < ?1 \
             GROUP BY event_type",
        )
        .bind(cutoff_unix_ms)
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| crate::CoreError::internal(format!("count abandoned observations: {e}")))?;

        let mut summary = crate::cloud::outbox::DrainSummary::default();
        for row in rows {
            let kind: String = Row::try_get(&row, "event_type").unwrap_or_default();
            let count: i64 = Row::try_get(&row, "c").unwrap_or_default();
            summary.per_kind.push((kind, count));
            summary.total += count;
        }
        summary.per_kind.sort_by(|a, b| a.0.cmp(&b.0));

        if dry_run || summary.total == 0 {
            tx.rollback()
                .await
                .map_err(|e| crate::CoreError::internal(format!("rollback drain tx: {e}")))?;
            return Ok(summary);
        }

        // Resurrected rows are due immediately (`next_attempt_at_ms` = now) and
        // cleared of prior error context. `created_at_ms` is left untouched so
        // the cap-queue trimmer's age ordering is preserved.
        let now = now_unix_ms();
        let result = sqlx::query(
            "UPDATE observation_events \
             SET status = 'pending', \
                 retry_count = 0, \
                 next_attempt_at_ms = ?1, \
                 last_error = NULL \
             WHERE status = 'abandoned' AND created_at_ms < ?2",
        )
        .bind(now)
        .bind(cutoff_unix_ms)
        .execute(&mut *tx)
        .await
        .map_err(|e| crate::CoreError::internal(format!("reset abandoned observations: {e}")))?;
        tx.commit()
            .await
            .map_err(|e| crate::CoreError::internal(format!("commit drain tx: {e}")))?;

        let affected = i64::try_from(result.rows_affected()).unwrap_or(summary.total);
        summary.total = affected;
        Ok(summary)
    }

    /// Move a row to `abandoned` (permanent failure or aged-out). Lease-fenced
    /// like the other terminal writers so a stale flusher can't resurrect or
    /// re-abandon a row a newer owner has already taken over.
    pub(super) async fn abandon(&self, id: i64, err: &str, lease: i64) -> crate::Result<()> {
        sqlx::query(
            "UPDATE observation_events \
             SET status = 'abandoned', last_error = ?1 \
             WHERE id = ?2 AND status = 'sending' AND next_attempt_at_ms = ?3",
        )
        .bind(truncate(err, 2048))
        .bind(id)
        .bind(lease)
        .execute(self.pool())
        .await
        .map_err(|e| crate::CoreError::internal(format!("abandon observation: {e}")))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud::client::HttpFailure;
    use chrono::Utc;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    struct FakeObservationClient {
        logged_in: bool,
        outcomes: Mutex<VecDeque<crate::Result<(), OutboxFailure>>>,
        batch_sizes: Mutex<Vec<usize>>,
    }

    impl FakeObservationClient {
        fn new(outcomes: impl IntoIterator<Item = crate::Result<(), OutboxFailure>>) -> Self {
            Self {
                logged_in: true,
                outcomes: Mutex::new(outcomes.into_iter().collect()),
                batch_sizes: Mutex::new(Vec::new()),
            }
        }

        fn batch_sizes(&self) -> Vec<usize> {
            self.batch_sizes.lock().unwrap().clone()
        }
    }

    impl ObservationUploadClient for FakeObservationClient {
        fn logged_in(&self) -> bool {
            self.logged_in
        }

        fn upload_observation_events<'a>(
            &'a self,
            batch: &'a [ObservationEvent],
        ) -> impl Future<Output = crate::Result<(), OutboxFailure>> + Send + 'a {
            let outcome = {
                self.batch_sizes.lock().unwrap().push(batch.len());
                self.outcomes
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("missing fake observation upload outcome")
            };
            std::future::ready(outcome)
        }
    }

    fn http(status: u16, body: &str) -> OutboxFailure {
        OutboxFailure::Http(HttpFailure {
            status,
            reason_phrase: "test".to_owned(),
            body_snippet: body.to_owned(),
        })
    }

    async fn emitter() -> (tempfile::TempDir, ObservationEmitter) {
        let dir = tempfile::tempdir().unwrap();
        let e = ObservationEmitter::open_at(&dir.path().join("obs.db"))
            .await
            .unwrap();
        (dir, e)
    }

    async fn enqueue_one(e: &ObservationEmitter) -> i64 {
        e.enqueue(&ObservationEvent::RuleFired {
            rule_ids: vec!["r1".to_owned()],
            file_path: Some("src/lib.rs".to_owned()),
            intent: Some("edit".to_owned()),
            session_id: "s".to_owned(),
            fired_at: Utc::now(),
        })
        .await
        .unwrap()
    }

    async fn row(e: &ObservationEmitter, id: i64) -> (String, i64, i64, Option<String>) {
        let r = sqlx::query(
            "SELECT status, retry_count, next_attempt_at_ms, last_error \
             FROM observation_events WHERE id = ?1",
        )
        .bind(id)
        .fetch_one(e.pool())
        .await
        .unwrap();
        (
            r.try_get("status").unwrap(),
            r.try_get("retry_count").unwrap(),
            r.try_get("next_attempt_at_ms").unwrap(),
            r.try_get("last_error").unwrap(),
        )
    }

    /// Simulate a flush claim: move the row to the in-flight `sending` state
    /// with a fresh lease and return that lease. `mark_failed` is fenced on
    /// `status = 'sending' AND next_attempt_at_ms = lease`, so a test must claim
    /// first for its write to land (there is no concurrency in tests).
    async fn claim_for_test(e: &ObservationEmitter, id: i64) -> i64 {
        let lease = now_unix_ms() + CLAIM_LEASE_MS;
        sqlx::query(
            "UPDATE observation_events SET status = 'sending', next_attempt_at_ms = ?1 WHERE id = ?2",
        )
        .bind(lease)
        .bind(id)
        .execute(e.pool())
        .await
        .unwrap();
        lease
    }

    #[test]
    fn parse_retry_after_reads_and_clamps_seconds() {
        assert_eq!(
            parse_retry_after_ms(r#"{"data":{"retryAfterSec":55}}"#),
            Some(55_000)
        );
        // No number present (the `Defined` error variant shape).
        assert_eq!(parse_retry_after_ms("RATE_LIMITED: rate_limited"), None);
        // Clamped to [1s, 5min].
        assert_eq!(
            parse_retry_after_ms(r#""retryAfterSec":99999"#),
            Some(300_000)
        );
        assert_eq!(parse_retry_after_ms(r#""retryAfterSec":0"#), Some(1_000));
        assert_eq!(
            parse_retry_after_ms(r#"{"retryAfterSec":null,"foo":7}"#),
            None
        );
        assert_eq!(parse_retry_after_ms(r#""xretryAfterSec":7"#), None);
    }

    #[test]
    fn classify_maps_status_to_delivery_class() {
        assert_eq!(
            classify(&http(429, r#"{"data":{"retryAfterSec":30}}"#)),
            DeliveryClass::RateLimited {
                retry_after_ms: 30_000
            }
        );
        assert_eq!(
            classify(&http(429, "rate_limited")),
            DeliveryClass::RateLimited {
                retry_after_ms: RATE_LIMIT_DEFAULT_MS
            }
        );
        assert_eq!(classify(&http(403, "no_team")), DeliveryClass::AuthBlocked);
        assert_eq!(
            classify(&http(401, "unauthorized")),
            DeliveryClass::AuthBlocked
        );
        assert_eq!(classify(&http(400, "bad")), DeliveryClass::PermanentInvalid);
        assert_eq!(
            classify(&http(422, "invalid")),
            DeliveryClass::PermanentInvalid
        );
        assert_eq!(classify(&http(500, "boom")), DeliveryClass::Transient);
        assert_eq!(classify(&http(408, "timeout")), DeliveryClass::Transient);
        assert_eq!(
            classify(&OutboxFailure::Transport("connection refused".to_owned())),
            DeliveryClass::Transient
        );
    }

    #[tokio::test]
    async fn auth_block_parks_without_abandoning_or_burning_retries() {
        let (_d, e) = emitter().await;
        let id = enqueue_one(&e).await;
        let lease = claim_for_test(&e, id).await;
        e.mark_failed(id, 0, now_unix_ms(), &http(403, "no_team"), lease)
            .await
            .unwrap();
        let (status, retry_count, _, _) = row(&e, id).await;
        assert_eq!(status, "parked");
        assert_eq!(retry_count, 0, "parking must not consume the retry budget");
        // Parked rows are not counted as normal pending uploads.
        assert_eq!(e.pending_upload_count().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn permanent_invalid_is_abandoned_and_tagged() {
        let (_d, e) = emitter().await;
        let id = enqueue_one(&e).await;
        let lease = claim_for_test(&e, id).await;
        e.mark_failed(id, 0, now_unix_ms(), &http(422, "invalid_batch"), lease)
            .await
            .unwrap();
        let (status, _, _, last_error) = row(&e, id).await;
        assert_eq!(status, "abandoned");
        assert!(last_error.unwrap_or_default().starts_with("permanent:"));
    }

    #[tokio::test]
    async fn rate_limited_reschedules_without_incrementing_retry_count() {
        let (_d, e) = emitter().await;
        let id = enqueue_one(&e).await;
        let before = now_unix_ms();
        let lease = claim_for_test(&e, id).await;
        e.mark_failed(
            id,
            3,
            before,
            &http(429, r#"{"data":{"retryAfterSec":40}}"#),
            lease,
        )
        .await
        .unwrap();
        let (status, retry_count, next_attempt, _) = row(&e, id).await;
        assert_eq!(status, "pending");
        assert_eq!(
            retry_count, 3,
            "rate-limiting must not consume the retry budget"
        );
        // Honors Retry-After (~40s out), give or take jitter.
        assert!(next_attempt >= before + 40_000);
        assert!(next_attempt < before + 60_000);
    }

    #[tokio::test]
    async fn transient_backs_off_and_increments_then_ages_out() {
        let (_d, e) = emitter().await;
        let id = enqueue_one(&e).await;
        let now = now_unix_ms();
        let lease = claim_for_test(&e, id).await;
        e.mark_failed(id, 1, now, &http(503, "unavailable"), lease)
            .await
            .unwrap();
        let (status, retry_count, next_attempt, _) = row(&e, id).await;
        assert_eq!(status, "pending");
        assert_eq!(
            retry_count, 2,
            "transient failures advance the backoff counter"
        );
        assert!(next_attempt > now);

        // A row older than the max age is abandoned even if still transient.
        let aged_created = now - (OBSERVATION_MAX_AGE_MS + 1);
        let lease = claim_for_test(&e, id).await;
        e.mark_failed(id, 2, aged_created, &http(503, "unavailable"), lease)
            .await
            .unwrap();
        let (status, _, _, last_error) = row(&e, id).await;
        assert_eq!(status, "abandoned");
        assert!(last_error.unwrap_or_default().contains("aged-out"));
    }

    #[tokio::test]
    async fn unpark_after_success_and_explicit_retry_revive_parked_rows() {
        let (_d, e) = emitter().await;
        let id = enqueue_one(&e).await;
        let lease = claim_for_test(&e, id).await;
        e.mark_failed(id, 0, now_unix_ms(), &http(403, "no_team"), lease)
            .await
            .unwrap();
        assert_eq!(row(&e, id).await.0, "parked");

        // Opportunistic un-park after a success.
        let revived = e.unpark_after_success().await.unwrap();
        assert_eq!(revived, 1);
        assert_eq!(row(&e, id).await.0, "pending");

        // Explicit retry (e.g. after `cloud login`) also un-parks.
        let lease = claim_for_test(&e, id).await;
        e.mark_failed(id, 0, now_unix_ms(), &http(403, "no_team"), lease)
            .await
            .unwrap();
        assert_eq!(row(&e, id).await.0, "parked");
        e.retry_pending_uploads_now().await.unwrap();
        assert_eq!(row(&e, id).await.0, "pending");
    }

    #[tokio::test]
    async fn permanent_batch_isolation_keeps_all_4xx_singleton_failures_retryable_without_success()
    {
        let (_d, e) = emitter().await;
        let first = enqueue_one(&e).await;
        let second = enqueue_one(&e).await;
        let before = now_unix_ms();
        let client = FakeObservationClient::new([
            Err(http(422, "invalid_batch")),
            Err(http(422, "invalid first")),
            Err(http(422, "invalid second")),
        ]);

        let result = e.flush_to_cloud_with(&client).await.unwrap();

        assert_eq!(result, (2, 0));
        assert_eq!(client.batch_sizes(), vec![2, 1, 1]);
        for id in [first, second] {
            let (status, retry_count, next_attempt, last_error) = row(&e, id).await;
            assert_eq!(status, "pending");
            assert_eq!(retry_count, 1);
            assert!(next_attempt > before);
            let last_error = last_error.unwrap_or_default();
            assert!(last_error.contains("422 test"));
            assert!(!last_error.starts_with("permanent:"));
        }
    }

    #[tokio::test]
    async fn permanent_batch_isolation_abandons_singleton_4xx_after_sibling_success() {
        let (_d, e) = emitter().await;
        let good = enqueue_one(&e).await;
        let bad = enqueue_one(&e).await;
        let client = FakeObservationClient::new([
            Err(http(422, "invalid_batch")),
            Ok(()),
            Err(http(422, "invalid bad row")),
        ]);

        let result = e.flush_to_cloud_with(&client).await.unwrap();

        assert_eq!(result, (2, 1));
        assert_eq!(client.batch_sizes(), vec![2, 1, 1]);
        assert_eq!(row(&e, good).await.0, "sent");
        let (status, _, _, last_error) = row(&e, bad).await;
        assert_eq!(status, "abandoned");
        assert!(last_error.unwrap_or_default().starts_with("permanent:"));
    }

    #[tokio::test]
    async fn permanent_batch_isolation_treats_row_level_rate_limit_as_shared_failure() {
        let (_d, e) = emitter().await;
        let first = enqueue_one(&e).await;
        let second = enqueue_one(&e).await;
        let third = enqueue_one(&e).await;
        let before = now_unix_ms();
        let client = FakeObservationClient::new([
            Err(http(422, "invalid_batch")),
            Err(http(422, "ambiguous first")),
            Err(http(429, r#"{"data":{"retryAfterSec":40}}"#)),
        ]);

        let result = e.flush_to_cloud_with(&client).await.unwrap();

        assert_eq!(result, (3, 0));
        assert_eq!(
            client.batch_sizes(),
            vec![3, 1, 1],
            "row-level shared failures should stop singleton fan-out"
        );
        for id in [first, second, third] {
            let (status, retry_count, next_attempt, last_error) = row(&e, id).await;
            assert_eq!(status, "pending");
            assert_eq!(retry_count, 0);
            assert!(next_attempt >= before + 40_000);
            assert!(next_attempt < before + 60_000);
            assert!(last_error.unwrap_or_default().contains("429 test"));
        }
    }
}
