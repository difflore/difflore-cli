#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(unsafe_code)]
//! End-to-end gate test for `DIFFLORE_CAPTURE=false`.
//!
//! In its own integration-test binary so the env mutation can't race
//! sibling tests: Cargo gives each `tests/*.rs` a dedicated process, and
//! this file holds exactly one test.
//!
//! Verifies that with the env var set to `"false"`, `OutboxQueue::enqueue`
//! is a no-op and no row enters `cloud_outbox`.

use difflore_core::cloud::capture::DIFFLORE_CAPTURE_ENV;
use difflore_core::cloud::outbox::{OutboxQueue, kind};
use sqlx::SqlitePool;
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

#[tokio::test]
async fn enqueue_is_noop_when_capture_disabled() {
    let pool = fresh_pool().await;
    let queue = OutboxQueue::new(pool.clone());

    // SAFETY: this integration test binary contains exactly one test,
    // so no sibling test can observe the mutated env. The `remove_var`
    // at the end restores process state for downstream Drop logic.
    unsafe {
        std::env::set_var(DIFFLORE_CAPTURE_ENV, "false");
    }

    let skipped_id = queue
        .enqueue(kind::TRAJECTORY, r#"{"skipped":true}"#)
        .await
        .unwrap();
    let count_after_skip: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cloud_outbox")
        .fetch_one(&pool)
        .await
        .unwrap();

    assert_eq!(
        skipped_id, 0,
        "enqueue must return sentinel 0 when capture is disabled",
    );
    assert_eq!(
        count_after_skip, 0,
        "no row may enter cloud_outbox when capture is disabled",
    );

    // SAFETY: same single-test isolation as above.
    unsafe {
        std::env::remove_var(DIFFLORE_CAPTURE_ENV);
    }

    // With the gate cleared, the same queue inserts normally —
    // confirms the gate is the only thing that suppressed the insert.
    let real_id = queue
        .enqueue(kind::TRAJECTORY, r#"{"skipped":false}"#)
        .await
        .unwrap();
    let count_after_enabled: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cloud_outbox")
        .fetch_one(&pool)
        .await
        .unwrap();

    assert!(real_id > 0, "enqueue must insert when capture is enabled");
    assert_eq!(
        count_after_enabled, 1,
        "exactly one row inserted once the gate is cleared",
    );
}
