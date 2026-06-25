//! Shared embedding-degradation window and classification logic.
//!
//! Both the default doctor table (`table.rs`) and the `--report` env-probe /
//! formatter sections count and classify `EmbeddingFallback` activity events.
//! Hoisting the window, threshold, and predicates here keeps every surface from
//! silently disagreeing about degradation state if one side is edited alone.

pub(in crate::commands::doctor) const EMBEDDING_DEGRADATION_WINDOW_MS: i64 = 10 * 60 * 1000;
pub(in crate::commands::doctor) const SUSTAINED_TRANSIENT_FALLBACK_THRESHOLD: usize = 5;

pub(in crate::commands::doctor) fn should_count_embedding_degradation(
    ts_ms: i64,
    reason: &str,
    now_ms: i64,
) -> bool {
    is_persistent_embedding_degradation(reason)
        || now_ms.saturating_sub(ts_ms) <= EMBEDDING_DEGRADATION_WINDOW_MS
}

pub(in crate::commands::doctor) fn is_persistent_embedding_degradation(reason: &str) -> bool {
    matches!(reason, "cap" | "forbidden" | "scope" | "unauthorized")
}

#[cfg(test)]
mod tests {
    use super::{
        EMBEDDING_DEGRADATION_WINDOW_MS, is_persistent_embedding_degradation,
        should_count_embedding_degradation,
    };

    #[test]
    fn persistent_reasons_are_classified_as_persistent() {
        for reason in ["cap", "forbidden", "scope", "unauthorized"] {
            assert!(is_persistent_embedding_degradation(reason), "{reason}");
        }
        assert!(!is_persistent_embedding_degradation("timeout"));
    }

    #[test]
    fn persistent_reasons_count_regardless_of_age() {
        let now_ms = 1_000_000_000;
        let stale_ts = now_ms - EMBEDDING_DEGRADATION_WINDOW_MS - 1;
        assert!(should_count_embedding_degradation(stale_ts, "cap", now_ms));
    }

    #[test]
    fn transient_reasons_count_only_within_window() {
        let now_ms = 1_000_000_000;
        let inside = now_ms - EMBEDDING_DEGRADATION_WINDOW_MS;
        let outside = now_ms - EMBEDDING_DEGRADATION_WINDOW_MS - 1;
        assert!(should_count_embedding_degradation(
            inside, "timeout", now_ms
        ));
        assert!(!should_count_embedding_degradation(
            outside, "timeout", now_ms
        ));
    }
}
