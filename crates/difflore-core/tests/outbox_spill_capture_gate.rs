#![allow(clippy::expect_used, clippy::unwrap_used)]
#![allow(unsafe_code)]
//! Regression coverage for replaying hook spill files while capture is disabled.
//!
//! This test binary intentionally contains one test because it mutates process
//! environment variables.

use difflore_core::cloud::capture::DIFFLORE_CAPTURE_ENV;
use difflore_core::cloud::outbox::{
    OutboxQueue, hook_spill_stats, replay_spilled_observations, spill_observation_payload,
};
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use tempfile::TempDir;

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
async fn replay_preserves_spill_file_when_capture_is_disabled() {
    let home = TempDir::new().expect("temp DIFFLORE_HOME");

    // SAFETY: this integration test binary contains exactly one test, so no
    // sibling test in this process can observe the mutated environment.
    unsafe {
        std::env::set_var("DIFFLORE_HOME", home.path());
        std::env::set_var(DIFFLORE_CAPTURE_ENV, "false");
    }

    let pool = fresh_pool().await;
    let queue = OutboxQueue::new(pool.clone());
    let path =
        spill_observation_payload(r#"{"session_id":"s1"}"#, "db locked").expect("spill payload");

    let report = replay_spilled_observations(&queue, 8)
        .await
        .expect("replay spill");

    assert_eq!(report.attempted, 1);
    assert_eq!(report.replayed, 0);
    assert_eq!(report.failed, 1);
    assert!(
        path.exists(),
        "disabled capture must not delete durable spill"
    );
    assert_eq!(hook_spill_stats().expect("spill stats").count, 1);

    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cloud_outbox")
        .fetch_one(&pool)
        .await
        .expect("count rows");
    assert_eq!(rows, 0);

    // SAFETY: same single-test isolation as above.
    unsafe {
        std::env::remove_var(DIFFLORE_CAPTURE_ENV);
        std::env::remove_var("DIFFLORE_HOME");
    }
}
