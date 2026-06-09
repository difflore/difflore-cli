use super::events::ObservationEvent;
use super::storage::{MAX_FLUSH_BATCH, ObservationEmitter, now_unix_ms, truncate};
use crate::cloud::outbox_core::{RetryDecision, backoff_delay_ms, decide_retry};
use sqlx::Row;

impl ObservationEmitter {
    pub async fn retry_pending_uploads_now(&self) -> Result<u64, String> {
        let now = now_unix_ms();
        let result = sqlx::query(
            "UPDATE observation_events \
             SET next_attempt_at_ms = ?1 \
             WHERE status = 'pending' AND next_attempt_at_ms > ?1",
        )
        .bind(now)
        .execute(self.pool())
        .await
        .map_err(|e| format!("reset pending observation retry time: {e}"))?;
        Ok(result.rows_affected())
    }

    pub async fn flush_to_cloud(
        &self,
        client: &crate::cloud::client::CloudClient,
    ) -> Result<(usize, usize), String> {
        if !client.is_logged_in() {
            return Ok((0, 0));
        }

        let now = now_unix_ms();
        let rows = sqlx::query(
            "SELECT id, payload_json, retry_count FROM observation_events \
             WHERE status = 'pending' AND next_attempt_at_ms <= ?1 \
             ORDER BY created_at_ms ASC, id ASC LIMIT ?2",
        )
        .bind(now)
        .bind(MAX_FLUSH_BATCH)
        .fetch_all(self.pool())
        .await
        .map_err(|e| format!("select observation batch: {e}"))?;

        if rows.is_empty() {
            return Ok((0, 0));
        }

        let mut ids = Vec::with_capacity(rows.len());
        let mut events = Vec::with_capacity(rows.len());
        let mut retry_counts = Vec::with_capacity(rows.len());
        for row in rows {
            let id: i64 = row.try_get("id").unwrap_or_default();
            let payload: String = row.try_get("payload_json").unwrap_or_default();
            let retry_count: i64 = row.try_get("retry_count").unwrap_or_default();
            match serde_json::from_str::<ObservationEvent>(&payload) {
                Ok(event) => {
                    ids.push(id);
                    events.push(event);
                    retry_counts.push(retry_count);
                }
                Err(e) => {
                    self.abandon(id, &format!("decode observation event: {e}"))
                        .await?;
                }
            }
        }

        if events.is_empty() {
            return Ok((0, 0));
        }

        let attempted = events.len();
        if client.post_observation_events_result(&events).await.is_ok() {
            let sent_at = now_unix_ms();
            for id in &ids {
                self.mark_sent(*id, sent_at).await?;
            }
            let _ = self.cap_queue().await;
            return Ok((attempted, attempted));
        }

        let sent_at = now_unix_ms();
        let mut sent = 0usize;
        for ((id, event), retry_count) in ids
            .into_iter()
            .zip(events.iter())
            .zip(retry_counts.into_iter())
        {
            match client
                .post_observation_events_result(std::slice::from_ref(event))
                .await
            {
                Ok(()) => {
                    self.mark_sent(id, sent_at).await?;
                    sent += 1;
                }
                Err(err) => {
                    self.mark_failed(id, retry_count, &err).await?;
                }
            }
        }
        let _ = self.cap_queue().await;
        Ok((attempted, sent))
    }

    pub(super) async fn mark_failed(
        &self,
        id: i64,
        retry_count: i64,
        err: &str,
    ) -> Result<(), String> {
        // Shared retry/abandon decision (unified `MAX_RETRY_COUNT`).
        // Equivalent to the prior `next = retry_count + 1; next >=
        // MAX_RETRY_COUNT ? abandon : backoff` — this queue keeps its
        // exponential-backoff re-schedule for the retry case.
        let next_count = match decide_retry(retry_count) {
            RetryDecision::Abandon { .. } => return self.abandon(id, err).await,
            RetryDecision::Retry { next_count } => next_count,
        };
        // `backoff_delay_ms` reproduces the previous inline
        // `60_000 * (1 << clamp(next_count, 0, 5))` exactly, including
        // the checked-shift / saturating-mul overflow guards.
        let delay_ms = backoff_delay_ms(next_count);
        let next_attempt = now_unix_ms().saturating_add(delay_ms);
        sqlx::query(
            "UPDATE observation_events \
             SET retry_count = ?1, next_attempt_at_ms = ?2, last_error = ?3 \
             WHERE id = ?4",
        )
        .bind(next_count)
        .bind(next_attempt)
        .bind(truncate(err, 2048))
        .bind(id)
        .execute(self.pool())
        .await
        .map_err(|e| format!("mark observation failed: {e}"))?;
        Ok(())
    }

    pub(super) async fn mark_sent(&self, id: i64, sent_at_ms: i64) -> Result<(), String> {
        sqlx::query("UPDATE observation_events SET status = 'sent', sent_at_ms = ?1 WHERE id = ?2")
            .bind(sent_at_ms)
            .bind(id)
            .execute(self.pool())
            .await
            .map_err(|e| format!("mark observation sent: {e}"))?;
        Ok(())
    }

    /// Resurrect `abandoned` observation_events rows older than
    /// `cutoff_unix_ms` back to `pending`. Returns the number of rows
    /// that were (or would be, in `dry_run` mode) reset, bucketed by
    /// `event_type` (sorted ascending so doctor output is stable).
    ///
    /// Uses a single transaction so a partial drain cannot leave the
    /// queue half-reset; `dry_run = true` rolls back instead of committing.
    ///
    /// Cutoff: a row is eligible iff its `created_at_ms` is older than
    /// the provided cutoff. We deliberately don't use
    /// `next_attempt_at_ms` because it isn't carried forward when a
    /// row is abandoned (the prior `mark_failed` rewrote it for the
    /// would-be retry that never happened), so `created_at_ms` is the
    /// stable age signal.
    pub async fn drain_abandoned_older_than(
        &self,
        cutoff_unix_ms: i64,
        dry_run: bool,
    ) -> Result<crate::cloud::outbox::DrainSummary, String> {
        let mut tx = self
            .pool()
            .begin()
            .await
            .map_err(|e| format!("begin drain tx: {e}"))?;

        let rows = sqlx::query(
            "SELECT event_type, COUNT(*) AS c \
             FROM observation_events \
             WHERE status = 'abandoned' AND created_at_ms < ?1 \
             GROUP BY event_type",
        )
        .bind(cutoff_unix_ms)
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| format!("count abandoned observations: {e}"))?;

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
                .map_err(|e| format!("rollback drain tx: {e}"))?;
            return Ok(summary);
        }

        // Resurrected rows must be due immediately (`next_attempt_at_ms`
        // = now) and free of any prior error context. We deliberately do
        // NOT touch `created_at_ms` so the cap-queue trimmer's age
        // ordering is preserved.
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
        .map_err(|e| format!("reset abandoned observations: {e}"))?;
        tx.commit()
            .await
            .map_err(|e| format!("commit drain tx: {e}"))?;

        let affected = i64::try_from(result.rows_affected()).unwrap_or(summary.total);
        summary.total = affected;
        Ok(summary)
    }

    pub(super) async fn abandon(&self, id: i64, err: &str) -> Result<(), String> {
        sqlx::query(
            "UPDATE observation_events \
             SET status = 'abandoned', last_error = ?1 WHERE id = ?2",
        )
        .bind(truncate(err, 2048))
        .bind(id)
        .execute(self.pool())
        .await
        .map_err(|e| format!("abandon observation: {e}"))?;
        Ok(())
    }
}
