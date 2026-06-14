//! Shared primitives for the two SQLite-backed outbox queues.
//!
//! `cloud_outbox` and `observation_events` share retry math, clocks, and
//! error truncation, but keep their own SQL and recovery strategies.
//!
//! * different tables / columns,
//! * `OutboxQueue` recovers stuck rows via a stale-`claimed_at` window
//!   plus a process-wide circuit breaker; the observations emitter
//!   instead schedules an exponential-backoff `next_attempt_at_ms`,
//! * `OutboxQueue` uses the compile-time-checked `sqlx::query!` macro
//!   bound to `cloud_outbox`; the observations emitter uses runtime
//!   `sqlx::query` against a hand-migrated table.
//!
//! Queue-specific delivery semantics stay in their callers.

/// Maximum delivery attempts per outbox row, shared by both queues.
pub(crate) const MAX_RETRY_COUNT: i64 = 8;

/// What a queue should do with a row whose upload just failed.
///
/// This encodes the *decision* both `OutboxQueue::mark_failed` and the
/// observations emitter's `mark_failed` make from the row's current
/// `retry_count`. It deliberately does **not** prescribe *how* the row
/// is retried (bounce to `pending`, vs. schedule a backoff
/// `next_attempt_at_ms`) — that is the per-queue behaviour the callers
/// keep owning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetryDecision {
    /// The row has not exhausted its attempts: it should be retried
    /// (each queue applies its own re-schedule). `next_count` is the
    /// incremented `retry_count` to persist.
    Retry { next_count: i64 },
    /// The row has hit `MAX_RETRY_COUNT`: it must transition to
    /// `abandoned` and never be re-claimed. `next_count` is still the
    /// incremented count so callers that persist it on abandon stay
    /// consistent with the persisted retry count.
    Abandon { next_count: i64 },
}

/// Decide whether a row on its `retry_count`-th recorded failure should
/// be retried or abandoned.
///
/// Centralizing this decision keeps the two queues on the same bound.
pub(crate) const fn decide_retry(retry_count: i64) -> RetryDecision {
    let next_count = retry_count + 1;
    if next_count >= MAX_RETRY_COUNT {
        RetryDecision::Abandon { next_count }
    } else {
        RetryDecision::Retry { next_count }
    }
}

/// Exponential-backoff delay (ms) for the `next_count`-th attempt.
///
/// Formula: `60_000ms * 2^clamp(next_count, 0, 5)`, with overflow
/// guarded by `checked_shl` and `saturating_mul`.
///
/// `OutboxQueue` does not call this; it retries through the
/// stale-`claimed_at` window and circuit breaker instead.
pub(crate) fn backoff_delay_ms(next_count: i64) -> i64 {
    let shift = u32::try_from(next_count.clamp(0, 5)).unwrap_or(0);
    60_000_i64.saturating_mul(1_i64.checked_shl(shift).unwrap_or(32))
}

/// Wall-clock now in unix milliseconds, saturating on overflow or
/// pre-epoch clocks.
pub(crate) fn now_unix_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Trim an unbounded error string to at most `max_chars` characters so
/// a cascade of failures cannot blow up the row's `last_error` column.
///
/// Returns the input when it is already short enough, otherwise the
/// first `max_chars` Unicode scalar values.
pub(crate) fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_owned();
    }
    s.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decide_retry_matches_prior_inline_arithmetic() {
        // retry_count 0..MAX-2 -> Retry with incremented count.
        for rc in 0..(MAX_RETRY_COUNT - 1) {
            assert_eq!(
                decide_retry(rc),
                RetryDecision::Retry { next_count: rc + 1 },
                "retry_count {rc} should still retry"
            );
        }
        // The attempt that pushes next_count to MAX abandons.
        assert_eq!(
            decide_retry(MAX_RETRY_COUNT - 1),
            RetryDecision::Abandon {
                next_count: MAX_RETRY_COUNT
            }
        );
        // And anything already at/above the bound stays abandoned.
        assert_eq!(
            decide_retry(MAX_RETRY_COUNT),
            RetryDecision::Abandon {
                next_count: MAX_RETRY_COUNT + 1
            }
        );
    }

    #[test]
    fn max_retry_count_is_unified_to_eight() {
        assert_eq!(MAX_RETRY_COUNT, 8);
    }

    #[test]
    fn backoff_delay_matches_observations_formula() {
        // 60s base, doubling, clamped at 2^5.
        assert_eq!(backoff_delay_ms(0), 60_000);
        assert_eq!(backoff_delay_ms(1), 120_000);
        assert_eq!(backoff_delay_ms(2), 240_000);
        assert_eq!(backoff_delay_ms(3), 480_000);
        assert_eq!(backoff_delay_ms(4), 960_000);
        assert_eq!(backoff_delay_ms(5), 1_920_000);
        // Clamp holds beyond shift 5.
        assert_eq!(backoff_delay_ms(6), 1_920_000);
        assert_eq!(backoff_delay_ms(99), 1_920_000);
        // Negative counts clamp to shift 0.
        assert_eq!(backoff_delay_ms(-1), 60_000);
    }

    #[test]
    fn truncate_returns_short_input_verbatim_and_clips_long_input() {
        assert_eq!(truncate("abc", 2048), "abc");
        let long: String = "x".repeat(5000);
        let clipped = truncate(&long, 2048);
        assert_eq!(clipped.chars().count(), 2048);
        let inline: String = long.chars().take(2048).collect();
        assert_eq!(clipped, inline);
    }

    #[test]
    fn now_unix_ms_is_monotonic_nonzero() {
        let a = now_unix_ms();
        assert!(a > 0);
        let b = now_unix_ms();
        assert!(b >= a);
    }
}
