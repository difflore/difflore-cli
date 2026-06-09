#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(unsafe_code)]
//! End-to-end gate test for `DIFFLORE_CAPTURE=false` on the
//! observations outbox.
//!
//! Sibling of `cloud_capture_gate.rs`: when the env var is set to the
//! literal `"false"`, `ObservationEmitter::enqueue` must be a no-op —
//! no row enters `observation_events`. The product has two outbox
//! queues (`cloud_outbox` for trajectory/mcp telemetry and
//! `observation_events` for PostToolUse observations); both have to
//! gate at enqueue so the privacy promise the CLI's
//! `difflore cloud privacy` notice makes stays whole.
//!
//! Lives in its own integration-test binary for the same reason the
//! sibling does: setting an env var inside a unit test would race with
//! parallel-running siblings. Cargo gives every `tests/*.rs` file a
//! dedicated process, and this file holds exactly one test, so the env
//! mutation cannot leak.

use chrono::{TimeZone, Utc};
use difflore_core::cloud::capture::DIFFLORE_CAPTURE_ENV;
use difflore_core::cloud::observations::{ObservationEmitter, ObservationEvent};
use sqlx::Row;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::time::Duration;
use tempfile::TempDir;

async fn row_count(db_path: &std::path::Path) -> i64 {
    // Open a separate read-only connection to the same DB file so we
    // don't have to crack open the emitter's pool accessor (which is
    // crate-internal). Mirrors how the sibling cloud_capture_gate test
    // uses a directly-built `:memory:` pool.
    let opts = SqliteConnectOptions::new()
        .filename(db_path)
        .create_if_missing(false)
        .busy_timeout(Duration::from_secs(2));
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .expect("open observation_events for read");
    let row = sqlx::query("SELECT COUNT(*) FROM observation_events")
        .fetch_one(&pool)
        .await
        .expect("count observation_events");
    row.get::<i64, _>(0)
}

#[tokio::test]
async fn enqueue_is_noop_when_capture_disabled() {
    let tmp = TempDir::new().expect("tmp dir");
    let db_path = tmp.path().join("observations_outbox.db");
    let emitter = ObservationEmitter::open_at(&db_path)
        .await
        .expect("open observations outbox");

    let event = ObservationEvent::RuleFired {
        rule_ids: vec![String::from("rule-test-1")],
        file_path: Some(String::from("src/main.rs")),
        intent: Some(String::from("test capture gate")),
        session_id: String::from("session-test"),
        fired_at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
    };

    // SAFETY: this integration test binary contains exactly one test,
    // so no sibling test can observe the mutated env. The `remove_var`
    // at the end restores process state for downstream Drop logic.
    unsafe {
        std::env::set_var(DIFFLORE_CAPTURE_ENV, "false");
    }

    let skipped_id = emitter.enqueue(&event).await.expect("enqueue (skipped)");
    let count_after_skip = row_count(&db_path).await;

    assert_eq!(
        skipped_id, 0,
        "enqueue must return sentinel 0 when capture is disabled",
    );
    assert_eq!(
        count_after_skip, 0,
        "no row may enter observation_events when capture is disabled",
    );

    // SAFETY: same single-test isolation as above.
    unsafe {
        std::env::remove_var(DIFFLORE_CAPTURE_ENV);
    }

    // With the gate cleared, the same emitter inserts normally —
    // confirms the gate is the only thing that suppressed the insert.
    let real_id = emitter.enqueue(&event).await.expect("enqueue (enabled)");
    let count_after_enabled = row_count(&db_path).await;

    assert!(real_id > 0, "enqueue must insert when capture is enabled");
    assert_eq!(
        count_after_enabled, 1,
        "exactly one row inserted once the gate is cleared",
    );
}
