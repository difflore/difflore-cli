#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(unsafe_code)]
//! Behavioural regression test for W1-B hot-path fix.
//!
//! Lives in its own integration-test binary on purpose: the test
//! mutates `DIFFLORE_HOME` so all on-disk side effects (`data.db`,
//! `observations_outbox.db`, `hook-cache.json`) land under a
//! tempdir instead of the user's real `~/.difflore`. Cargo gives
//! every `tests/*.rs` file its own process, so that env mutation
//! cannot leak into sibling test binaries.
//!
//! The fix being guarded: `PostToolUse:Edit` must consult
//! `hook_cache::should_skip_recent` BEFORE opening the SQLite
//! pool or running `observation::classify` + outbox `enqueue`.
//! Before the fix the skip path still paid all of those costs
//! (175ms p99); after it, a duplicate edit short-circuits to a
//! noop without writing any observation row or any
//! `cloud_outbox` row.
//!
//! The proceed half asserts the non-skip branch still runs the
//! classifier + outbox enqueue, so the fix didn't accidentally
//! suppress capture. Both halves run in the same `#[tokio::test]`
//! to avoid racing on the shared process-wide `DIFFLORE_HOME` env
//! var.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use difflore_cli::hook_runtime;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use sqlx::sqlite::SqliteConnectOptions;

#[derive(Serialize, Deserialize)]
struct CacheFile {
    version: u32,
    entries: BTreeMap<String, CacheEntry>,
}

#[derive(Serialize, Deserialize)]
struct CacheEntry {
    ts_ms: i64,
    rules_injected: usize,
}

/// Seed `hook-cache.json` so `should_skip_recent` returns true for
/// the given file path. Mirrors the on-disk layout
/// `hook_cache::remember_injection` would have produced; we write
/// it by hand to avoid having to actually run a successful
/// injection just to set up the test.
fn seed_skip_cache(home: &Path, file_path: &str) {
    let project_root = difflore_core::db::current_project_root();
    let project_hash = difflore_core::db::project_hash_from_root(&project_root);
    let normalized = file_path.trim().replace('\\', "/");
    let key = format!("{project_hash}:post-edit:{normalized}");

    let ts_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let mut entries = BTreeMap::new();
    entries.insert(
        key,
        CacheEntry {
            ts_ms,
            rules_injected: 3,
        },
    );
    let cache = CacheFile {
        version: 1,
        entries,
    };
    std::fs::create_dir_all(home).expect("create home");
    let payload = serde_json::to_string_pretty(&cache).expect("serialize cache");
    std::fs::write(home.join("hook-cache.json"), payload).expect("write hook-cache");
}

fn post_tool_use_payload(file_path: &str) -> String {
    serde_json::json!({
        "session_id": "test-session",
        "cwd": ".",
        "hook_event_name": "PostToolUse",
        "tool_name": "Edit",
        "tool_input": {
            "file_path": file_path,
            "old_string": "let x = 1;\n",
            "new_string": "let x = 2;\n"
        },
        "tool_response": {}
    })
    .to_string()
}

async fn count_cloud_outbox_rows(data_db: &Path) -> i64 {
    if !data_db.exists() {
        // Skip path never opened the pool, so the file was never
        // created. Treat that as the strongest possible "zero rows"
        // signal.
        return 0;
    }
    let opts = SqliteConnectOptions::new()
        .filename(data_db)
        .create_if_missing(false);
    let pool: SqlitePool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .expect("open data.db");
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cloud_outbox")
        .fetch_one(&pool)
        .await
        .unwrap_or(0);
    pool.close().await;
    count
}

#[tokio::test]
async fn post_tool_use_dedup_skip_short_circuits_before_db_and_outbox() {
    // SAFETY: this test mutates DIFFLORE_HOME, and is the only test
    // in this integration-test binary, so cargo's per-file process
    // isolation guarantees no sibling test can observe the mutation.
    // Reading via the difflore_core helpers happens inside the same
    // task that set the env var, so there's no cross-thread race.
    let home = tempfile::tempdir().expect("temp DIFFLORE_HOME");
    unsafe {
        std::env::set_var("DIFFLORE_HOME", home.path());
    }
    // Force capture on so an *un-skipped* enqueue would actually
    // insert a row (the proceed assertion below relies on this).
    unsafe {
        std::env::remove_var("DIFFLORE_CAPTURE");
    }

    // -- skip arm -----------------------------------------------------
    // Seed the cache so should_skip_recent returns true. The fast
    // path should then never touch data.db or observations_outbox.db.
    let skip_file = "src/example_skip.rs";
    seed_skip_cache(home.path(), skip_file);

    let raw = post_tool_use_payload(skip_file);
    let out = hook_runtime::output_for_raw("claude-code", &raw, false)
        .await
        .expect("hook output");
    let json: serde_json::Value = serde_json::from_str(&out).expect("parseable output");
    assert_eq!(
        json.get("continue").and_then(serde_json::Value::as_bool),
        Some(true),
        "skip path must return a continue:true noop, got: {json}"
    );

    let data_db = home.path().join("data.db");
    let obs_db = home.path().join("observations_outbox.db");
    assert!(
        !data_db.exists(),
        "skip path must not init data.db (was {})",
        data_db.display()
    );
    assert!(
        !obs_db.exists(),
        "skip path must not open observations_outbox.db (was {})",
        obs_db.display()
    );
    // Belt-and-braces: even if a future change pre-creates data.db
    // for some other reason, cloud_outbox must still be empty on
    // the skip path.
    let rows_after_skip = count_cloud_outbox_rows(&data_db).await;
    assert_eq!(
        rows_after_skip, 0,
        "skip path must not enqueue any cloud_outbox row, found {rows_after_skip}"
    );

    // -- proceed arm --------------------------------------------------
    // Different file → no cache entry → should_skip_recent returns
    // false → dispatch runs classify + enqueue.
    let proceed_file = "src/example_proceed.rs";
    let raw = post_tool_use_payload(proceed_file);
    let out = hook_runtime::output_for_raw("claude-code", &raw, false)
        .await
        .expect("hook output");
    let _: serde_json::Value = serde_json::from_str(&out).expect("parseable output");

    assert!(
        data_db.exists(),
        "non-skip path must init data.db (was {})",
        data_db.display()
    );
    let rows_after_proceed = count_cloud_outbox_rows(&data_db).await;
    assert!(
        rows_after_proceed >= 1,
        "non-skip path must enqueue at least one cloud_outbox row, found {rows_after_proceed}"
    );

    unsafe {
        std::env::remove_var("DIFFLORE_HOME");
    }
    drop(home);
}
