// The warm hook-forward daemon binds a filesystem-path local socket
// (`interprocess` `GenericFilePath` listener), which is a Unix capability;
// Windows named pipes live in a separate namespace and cannot bind these
// paths. On Windows the shim falls back to the in-process hook path (covered
// by `hook_shim_fallback.rs`), so these daemon lifecycle/isolation scenarios
// are Unix-only by construction.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(unsafe_code)]
//! Lifecycle + isolation tests for the warm hook-forward daemon.
//!
//! Its own integration-test binary on purpose: the daemon's socket, `data.db`,
//! and per-project index DBs land under one tempdir `DIFFLORE_HOME`. Cargo
//! gives each `tests/*.rs` file its own process, so the env mutation cannot
//! leak into sibling binaries.
//!
//! All scenarios run inside a *single* `#[tokio::test]` driver, sequentially:
//! `DIFFLORE_HOME` is process-global, so running scenarios concurrently would
//! let one test's home clobber another's. The runtime is `multi_thread` so the
//! daemon's accept loop makes progress on a worker thread while the driver
//! thread does blocking probe/round-trip connects. Each scenario uses a
//! distinct project hash so their sockets never collide within the one home.

use std::io::{Read as _, Write as _};
use std::time::{Duration, Instant};

use difflore_cli::hook::forward::{self, protocol};

/// Poll until a daemon for `hash` is connectable, or time out.
fn wait_for_daemon(hash: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if protocol::connect_blocking_for_hash(hash).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

/// Send one SessionStart request straight to the daemon serving `hash` and
/// return the decoded hook output. Connects to the explicit per-hash socket
/// (not the current-project one) so a test can target a chosen daemon.
fn roundtrip_to(hash: &str, raw: &str) -> Result<String, String> {
    let mut stream = protocol::connect_blocking_for_hash(hash).map_err(|e| e.to_string())?;
    let line = protocol::encode_request_line("claude-code", raw)?;
    stream
        .write_all(line.as_bytes())
        .map_err(|e| e.to_string())?;
    stream.flush().map_err(|e| e.to_string())?;
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|e| e.to_string())?;
    protocol::decode_response_line(&response)
}

fn session_start_payload() -> String {
    serde_json::json!({
        "session_id": "test",
        "cwd": ".",
        "hook_event_name": "SessionStart",
        "source": "startup"
    })
    .to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hook_daemon_lifecycle_and_isolation() {
    let home = tempfile::tempdir().expect("temp DIFFLORE_HOME");
    // SAFETY: single driver test in this process owns the env for its whole
    // run; cargo's per-file process isolation bounds the blast radius.
    unsafe {
        std::env::set_var("DIFFLORE_HOME", home.path());
        // Long idle so daemons under test stay up until aborted; the idle
        // scenario overrides this locally.
        std::env::set_var("DIFFLORE_HOOK_DAEMON_IDLE_SECS", "120");
    }

    index_pools_are_isolated_per_project_hash().await;
    cross_library_retrieval_is_isolated_per_hash().await;
    second_daemon_for_same_hash_yields().await;
    stale_leftover_socket_is_reclaimed().await;
    concurrent_daemons_settle_to_one().await;
    idle_timeout_exits_and_removes_socket().await;

    drop(home);
}

/// The core correctness property of socket-per-hash: a daemon launched for
/// hash A serves *only* A's index DB, never B's. Proven at the layer the
/// daemon freezes — `State.index_pool` comes from `get_pool_for_project(hash)`.
async fn index_pools_are_isolated_per_project_hash() {
    let hash_a = "1111aaaabbbb";
    let hash_b = "2222ccccdddd";

    let pool_a = difflore_core::context::index_db::get_pool_for_project(hash_a)
        .await
        .expect("open index pool A");
    let pool_b = difflore_core::context::index_db::get_pool_for_project(hash_b)
        .await
        .expect("open index pool B");

    sqlx::query(
        "INSERT INTO rule_chunks (id, skill_id, content, embedding, file_patterns) \
         VALUES ('rule-a', 'skill-a', 'RULE-A only matches rust files', NULL, '[\"**/*.rs\"]')",
    )
    .execute(&pool_a)
    .await
    .expect("seed A");
    sqlx::query(
        "INSERT INTO rule_chunks (id, skill_id, content, embedding, file_patterns) \
         VALUES ('rule-b', 'skill-b', 'RULE-B only matches python files', NULL, '[\"**/*.py\"]')",
    )
    .execute(&pool_b)
    .await
    .expect("seed B");

    let a_sees_a: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM rule_chunks WHERE id = 'rule-a'")
        .fetch_one(&pool_a)
        .await
        .expect("count a in A");
    let a_sees_b: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM rule_chunks WHERE id = 'rule-b'")
        .fetch_one(&pool_a)
        .await
        .expect("count b in A");
    assert_eq!(a_sees_a, 1, "daemon A's index must contain RULE-A");
    assert_eq!(
        a_sees_b, 0,
        "daemon A's index must NOT contain RULE-B (cross-library leak)"
    );

    let b_sees_b: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM rule_chunks WHERE id = 'rule-b'")
        .fetch_one(&pool_b)
        .await
        .expect("count b in B");
    let b_sees_a: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM rule_chunks WHERE id = 'rule-a'")
        .fetch_one(&pool_b)
        .await
        .expect("count a in B");
    assert_eq!(b_sees_b, 1, "daemon B's index must contain RULE-B");
    assert_eq!(
        b_sees_a, 0,
        "daemon B's index must NOT contain RULE-A (cross-library leak)"
    );

    // The global data.db is shared: both daemons resolve init_db to the same
    // file, so a write through one is visible through the other.
    let db1 = difflore_core::infra::db::init_db()
        .await
        .expect("init db 1");
    let db2 = difflore_core::infra::db::init_db()
        .await
        .expect("init db 2");
    sqlx::query("CREATE TABLE IF NOT EXISTS _xrepo_probe (k TEXT PRIMARY KEY)")
        .execute(&db1)
        .await
        .expect("create probe table");
    sqlx::query("INSERT OR REPLACE INTO _xrepo_probe (k) VALUES ('shared')")
        .execute(&db1)
        .await
        .expect("write via db1");
    let seen: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM _xrepo_probe WHERE k = 'shared'")
        .fetch_one(&db2)
        .await
        .expect("read via db2");
    assert_eq!(
        seen, 1,
        "global data.db must be shared across daemons (one pool per data.db path)"
    );
}

/// End-to-end cross-library guard — the most important correctness property of
/// the whole socket-per-hash design. The sibling test above proves the *pool
/// identity* is isolated; this proves the consequence that actually matters:
/// the `hash -> pool -> retrieval` chain never serves one repo's rule when a
/// daemon launched for another hash answers a file edit.
///
/// We exercise the real chain `run_server_for_hash` builds:
///   1. Start a live daemon for hash A (so the pool is genuinely the one the
///      daemon freezes at `State.index_pool`, not a pool we conjured), then take
///      the same cached per-project pool via `get_pool_for_project(hash)` — the
///      exact call the daemon makes internally.
///   2. Seed each per-project index with a distinguishable, file-pattern-scoped
///      rule (A matches only `*.rs`, B matches only `*.py`) through the same
///      `upsert_rule_chunks` indexing path production uses.
///   3. Drive the retrieval entrypoint with a `*.rs` edit against pool A and a
///      `*.py` edit against pool B, asserting each daemon's pool returns ONLY
///      its own rule — A's rule is present, B's rule is provably absent from A's
///      pool, and vice-versa.
///
/// Retrieval reads strictly from the supplied `index_pool` and applies the
/// strict file-pattern cascade, so a hit for A's `*.rs` rule on a `*.rs` edit
/// that never surfaces B's `*.py` rule directly demonstrates the isolation that
/// keeps repo A's library out of repo B's edits.
async fn cross_library_retrieval_is_isolated_per_hash() {
    use difflore_core::context::index_db::{get_pool_for_project, upsert_rule_chunks};
    use difflore_core::context::retrieval::{RetrievalOptions, retrieve_rules_with_confidence};
    use difflore_core::context::rule_source::RuleDocument;

    let hash_a = "3333eeeeffff";
    let hash_b = "4444aaaabbbb";

    // Start a live daemon for hash A so the pool we query below is the one the
    // daemon actually froze at startup (`get_pool_for_project` is pool-cached
    // per hash, so the daemon's `State.index_pool` and ours are the same DB).
    let h_a = hash_a.to_owned();
    let daemon_a = tokio::spawn(async move {
        forward::run_server_for_hash(&h_a)
            .await
            .expect("daemon A run");
    });
    assert!(
        wait_for_daemon(hash_a, Duration::from_secs(10)),
        "daemon A should become connectable before we probe its frozen pool"
    );

    // The same cached pools the daemons serve.
    let pool_a = get_pool_for_project(hash_a)
        .await
        .expect("open index pool A (daemon A's frozen pool)");
    let pool_b = get_pool_for_project(hash_b)
        .await
        .expect("open index pool B");

    // A rule that should only ever surface on Rust edits, indexed into A's pool.
    let rule_a = RuleDocument {
        skill_id: "rust-only-rule-a".to_owned(),
        title: "rust-only-rule-a".to_owned(),
        content: "When editing rust handlers prefer the zorptangle pattern over raw unwrap"
            .to_owned(),
        confidence: 0.7,
        file_patterns: Some(r#"["**/*.rs"]"#.to_owned()),
        language: None,
        repo_scope: None,
    };
    // A rule that should only ever surface on Python edits, indexed into B's pool.
    let rule_b = RuleDocument {
        skill_id: "python-only-rule-b".to_owned(),
        title: "python-only-rule-b".to_owned(),
        content: "When editing python handlers prefer the quafflenibble pattern over bare except"
            .to_owned(),
        confidence: 0.7,
        file_patterns: Some(r#"["**/*.py"]"#.to_owned()),
        language: None,
        repo_scope: None,
    };
    upsert_rule_chunks(&pool_a, std::slice::from_ref(&rule_a))
        .await
        .expect("index rule A into pool A");
    upsert_rule_chunks(&pool_b, std::slice::from_ref(&rule_b))
        .await
        .expect("index rule B into pool B");

    // A `*.rs` edit answered by daemon A's pool: A's rule must surface and B's
    // rule must NOT (it does not exist in pool A at all). The distinctive token
    // ("zorptangle") clears the relevance floor deterministically under the
    // offline SHA1 + FTS retrieval lane.
    let a_hits = retrieve_rules_with_confidence(
        &pool_a,
        "post-edit zorptangle pattern in rust handler",
        RetrievalOptions {
            top_k: Some(5),
            target_file: Some("src/main.rs"),
            ..Default::default()
        },
    )
    .await
    .expect("retrieve from pool A");
    let a_ids: Vec<&str> = a_hits.iter().map(|h| h.skill_id.as_str()).collect();
    assert!(
        a_ids.contains(&"rust-only-rule-a"),
        "daemon A's pool must serve its own *.rs rule for a *.rs edit, got {a_ids:?}"
    );
    assert!(
        !a_ids.contains(&"python-only-rule-b"),
        "daemon A's pool must NEVER surface repo B's rule (cross-library leak), got {a_ids:?}"
    );

    // Symmetric check: a `*.py` edit answered by daemon B's pool surfaces B's
    // rule only, never A's.
    let b_hits = retrieve_rules_with_confidence(
        &pool_b,
        "post-edit quafflenibble pattern in python handler",
        RetrievalOptions {
            top_k: Some(5),
            target_file: Some("src/main.py"),
            ..Default::default()
        },
    )
    .await
    .expect("retrieve from pool B");
    let b_ids: Vec<&str> = b_hits.iter().map(|h| h.skill_id.as_str()).collect();
    assert!(
        b_ids.contains(&"python-only-rule-b"),
        "daemon B's pool must serve its own *.py rule for a *.py edit, got {b_ids:?}"
    );
    assert!(
        !b_ids.contains(&"rust-only-rule-a"),
        "daemon B's pool must NEVER surface repo A's rule (cross-library leak), got {b_ids:?}"
    );

    // Cross-pattern guard on the isolated pool: a `*.py` edit against A's pool
    // (which holds only the `*.rs` rule) must drop it under the strict
    // file-pattern cascade rather than leaking it onto the wrong language.
    let a_wrong_ext = retrieve_rules_with_confidence(
        &pool_a,
        "post-edit zorptangle pattern in rust handler",
        RetrievalOptions {
            top_k: Some(5),
            target_file: Some("src/main.py"),
            ..Default::default()
        },
    )
    .await
    .expect("retrieve from pool A with a *.py target");
    let a_wrong_ids: Vec<&str> = a_wrong_ext.iter().map(|h| h.skill_id.as_str()).collect();
    assert!(
        !a_wrong_ids.contains(&"rust-only-rule-a"),
        "the strict file-pattern cascade must drop A's *.rs rule on a *.py edit, got {a_wrong_ids:?}"
    );

    daemon_a.abort();
}

/// A second daemon for a hash already served by a live daemon must detect it
/// and return `Ok` quickly without binding or disrupting the incumbent.
async fn second_daemon_for_same_hash_yields() {
    let hash = "aaaa11112222";

    let h1 = hash.to_owned();
    let daemon1 = tokio::spawn(async move {
        forward::run_server_for_hash(&h1)
            .await
            .expect("daemon1 run");
    });
    assert!(
        wait_for_daemon(hash, Duration::from_secs(10)),
        "daemon1 should become connectable"
    );

    let started = Instant::now();
    forward::run_server_for_hash(hash)
        .await
        .expect("second daemon should yield cleanly, not error");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "yield should be near-instant, took {:?}",
        started.elapsed()
    );

    let out = roundtrip_to(hash, &session_start_payload()).expect("daemon1 still serves");
    let _: serde_json::Value = serde_json::from_str(&out).expect("valid hook output");

    daemon1.abort();
}

/// A dead daemon's residual file at the socket path (plain file, no listener)
/// must be reclaimed: the connect-probe fails (no live peer), so the daemon
/// clears it and binds rather than erroring or splitting.
async fn stale_leftover_socket_is_reclaimed() {
    let hash = "bbbb33334444";

    let socket = protocol::endpoint_for_hash(hash).expect("endpoint");
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent).expect("create data home");
    }
    std::fs::write(&socket, b"stale").expect("write leftover file");

    let h = hash.to_owned();
    let daemon = tokio::spawn(async move {
        forward::run_server_for_hash(&h)
            .await
            .expect("daemon should reclaim stale socket and run");
    });
    assert!(
        wait_for_daemon(hash, Duration::from_secs(10)),
        "daemon should bind after clearing the stale file and accept connections"
    );
    let out = roundtrip_to(hash, &session_start_payload()).expect("reclaimed daemon serves");
    let _: serde_json::Value = serde_json::from_str(&out).expect("valid hook output");

    daemon.abort();
}

/// Several daemons fired at one hash at once must settle to a single server:
/// exactly one wins the bind and keeps accepting; the rest yield.
async fn concurrent_daemons_settle_to_one() {
    let hash = "dddd77778888";

    let mut handles = Vec::new();
    for _ in 0..5 {
        let h = hash.to_owned();
        handles.push(tokio::spawn(async move {
            forward::run_server_for_hash(&h).await
        }));
    }

    assert!(
        wait_for_daemon(hash, Duration::from_secs(10)),
        "at least one daemon should bind and accept"
    );

    let out = roundtrip_to(hash, &session_start_payload()).expect("single survivor serves");
    let _: serde_json::Value = serde_json::from_str(&out).expect("valid hook output");

    // Eventually 4 of 5 must yield, leaving exactly one survivor in its accept
    // loop. The losers' futures resolve once polled; under full-suite thread
    // contention that can take a few seconds, so allow a generous budget rather
    // than asserting an instantaneous count.
    let mut finished = 0;
    let deadline = Instant::now() + Duration::from_secs(30);
    while finished < 4 && Instant::now() < deadline {
        finished = handles.iter().filter(|h| h.is_finished()).count();
        if finished < 4 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    assert_eq!(
        finished, 4,
        "exactly one daemon should keep serving; {finished} of 5 yielded (expected 4)"
    );
    // The lone survivor must still be serving requests.
    let out = roundtrip_to(hash, &session_start_payload()).expect("survivor still serves");
    let _: serde_json::Value = serde_json::from_str(&out).expect("valid hook output");

    for h in handles {
        h.abort();
    }
}

/// With a tiny idle window and no requests, the daemon reaps itself and removes
/// its socket so a later connect fails.
async fn idle_timeout_exits_and_removes_socket() {
    let hash = "cccc55556666";
    unsafe {
        std::env::set_var("DIFFLORE_HOOK_DAEMON_IDLE_SECS", "1");
    }
    let socket = protocol::endpoint_for_hash(hash).expect("endpoint");

    let h = hash.to_owned();
    let daemon = tokio::spawn(async move {
        forward::run_server_for_hash(&h).await.expect("daemon run");
    });
    assert!(
        wait_for_daemon(hash, Duration::from_secs(10)),
        "daemon should be connectable before idling out"
    );

    let joined = tokio::time::timeout(Duration::from_secs(10), daemon).await;
    assert!(
        joined.is_ok(),
        "daemon should exit on idle timeout within the window"
    );

    let cleaned = {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if !socket.exists() && protocol::connect_blocking_for_hash(hash).is_err() {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    assert!(
        cleaned,
        "idle exit must remove the socket; still present at {}",
        socket.display()
    );

    // Restore the long idle for any later use of the env in this process.
    unsafe {
        std::env::set_var("DIFFLORE_HOOK_DAEMON_IDLE_SECS", "120");
    }
}
