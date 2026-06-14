//! Integration tests for the since-last-session banner SQL path, end to end
//! against a temporary SQLite DB. We avoid `init_db()` (a global pool cache)
//! and build a fresh `:memory:` pool with the exact schema columns the query
//! touches.
//!
//! Watermark IO is tested separately in `watermark.rs::tests`. The end-to-end
//! `render_since_last_session_banner` helper isn't exercised here because it
//! touches the real `init_db()` pool cache, which would race other tests; the
//! inner pipeline is covered transitively by the query + render tests.

use super::query::{NewRule, new_rules_since};

/// Spin up an in-memory SQLite pool with the minimal `skills`
/// columns the query reads. Mirrors the production schema exactly so
/// the query string is bit-for-bit the same one shipping in prod.
async fn fresh_skills_pool() -> sqlx::SqlitePool {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("open in-memory sqlite");
    sqlx::query(
        r"CREATE TABLE skills (
            id            TEXT PRIMARY KEY NOT NULL,
            name          TEXT NOT NULL,
            origin        TEXT NOT NULL DEFAULT 'manual',
            source_repo   TEXT,
            status        TEXT NOT NULL DEFAULT 'active',
            installed_at  TEXT DEFAULT (datetime('now')) NOT NULL
        )",
    )
    .execute(&pool)
    .await
    .expect("create skills");
    pool
}

async fn insert(
    pool: &sqlx::SqlitePool,
    id: &str,
    name: &str,
    origin: &str,
    source_repo: Option<&str>,
    status: &str,
    installed_at_iso: &str,
) {
    sqlx::query(
        r"INSERT INTO skills (id, name, origin, source_repo, status, installed_at)
          VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )
    .bind(id)
    .bind(name)
    .bind(origin)
    .bind(source_repo)
    .bind(status)
    .bind(installed_at_iso)
    .execute(pool)
    .await
    .expect("insert skill");
}

#[tokio::test]
async fn first_session_with_none_watermark_returns_recent_rules_for_repo() {
    // No prior watermark → we should see every active rule for this
    // repo, capped at the limit, newest-first. Rules for OTHER repos
    // must be filtered out.
    let pool = fresh_skills_pool().await;
    insert(
        &pool,
        "r1",
        "Return 413 for body size limit errors",
        "pr_review",
        Some("acme/billing"),
        "active",
        "2026-05-21T12:00:00Z",
    )
    .await;
    insert(
        &pool,
        "r2",
        "Wrap context cancellation in errgroup",
        "extracted",
        Some("acme/billing"),
        "active",
        "2026-05-22T13:00:00Z",
    )
    .await;
    // Different repo — must be excluded.
    insert(
        &pool,
        "r3",
        "Irrelevant rule from another repo",
        "pr_review",
        Some("other-org/other-repo"),
        "active",
        "2026-05-23T14:00:00Z",
    )
    .await;

    let aliases = vec!["acme/billing".to_owned()];
    let rows = new_rules_since(&pool, None, &aliases, 5)
        .await
        .expect("query ok");
    assert_eq!(
        rows.len(),
        2,
        "expected 2 rows scoped to acme/billing, got {rows:?}"
    );
    // Newest first.
    assert_eq!(rows[0].title, "Wrap context cancellation in errgroup");
    assert_eq!(rows[1].title, "Return 413 for body size limit errors");
}

#[tokio::test]
async fn watermark_filters_to_only_rules_newer_than_prev_ts() {
    let pool = fresh_skills_pool().await;
    insert(
        &pool,
        "r1",
        "Old rule",
        "manual",
        Some("acme/billing"),
        "active",
        "2026-05-20T10:00:00Z",
    )
    .await;
    insert(
        &pool,
        "r2",
        "New rule",
        "pr_review",
        Some("acme/billing"),
        "active",
        "2026-05-22T10:00:00Z",
    )
    .await;

    // Watermark = 2026-05-21T00:00:00Z → only `r2` should surface.
    let prev_ms = chrono::DateTime::parse_from_rfc3339("2026-05-21T00:00:00Z")
        .expect("parse")
        .timestamp_millis();
    let aliases = vec!["acme/billing".to_owned()];
    let rows = new_rules_since(&pool, Some(prev_ms), &aliases, 5)
        .await
        .expect("query ok");
    assert_eq!(rows.len(), 1, "got: {rows:?}");
    assert_eq!(rows[0].title, "New rule");
}

#[tokio::test]
async fn pending_and_no_source_repo_rules_are_excluded() {
    let pool = fresh_skills_pool().await;
    // Pending → excluded.
    insert(
        &pool,
        "r1",
        "Unverified rule",
        "conversation",
        Some("acme/billing"),
        "pending",
        "2026-05-22T10:00:00Z",
    )
    .await;
    // No source_repo → excluded (can't be attributed to a repo).
    insert(
        &pool,
        "r2",
        "Orphan rule",
        "manual",
        None,
        "active",
        "2026-05-22T11:00:00Z",
    )
    .await;
    // Active + scoped → included.
    insert(
        &pool,
        "r3",
        "Good rule",
        "pr_review",
        Some("acme/billing"),
        "active",
        "2026-05-22T12:00:00Z",
    )
    .await;

    let aliases = vec!["acme/billing".to_owned()];
    let rows = new_rules_since(&pool, None, &aliases, 5)
        .await
        .expect("query ok");
    assert_eq!(rows.len(), 1, "got: {rows:?}");
    assert_eq!(rows[0].title, "Good rule");
}

#[tokio::test]
async fn empty_alias_list_returns_empty_without_querying() {
    // Defensive: a repo with no detectable supported origin shouldn't
    // pull every rule on the user's machine. The query function
    // early-outs so even if `data.db` has a million skills, we do no
    // I/O and emit nothing.
    let pool = fresh_skills_pool().await;
    insert(
        &pool,
        "r1",
        "Should never show up",
        "pr_review",
        Some("acme/billing"),
        "active",
        "2026-05-22T10:00:00Z",
    )
    .await;
    let rows: Vec<NewRule> = new_rules_since(&pool, None, &[], 5)
        .await
        .expect("query ok");
    assert!(
        rows.is_empty(),
        "empty aliases must yield empty result; got {rows:?}"
    );
}

#[tokio::test]
async fn case_insensitive_alias_matching() {
    // `source_repo` was stored in mixed case during the cloud sync,
    // but the alias list arrives lower-cased. The `LOWER()` in the
    // query covers the mismatch.
    let pool = fresh_skills_pool().await;
    insert(
        &pool,
        "r1",
        "Cased rule",
        "pr_review",
        Some("Acme/Billing"),
        "active",
        "2026-05-22T10:00:00Z",
    )
    .await;

    let aliases = vec!["acme/billing".to_owned()];
    let rows = new_rules_since(&pool, None, &aliases, 5)
        .await
        .expect("query ok");
    assert_eq!(rows.len(), 1, "case-insensitive match failed: {rows:?}");
}

#[tokio::test]
async fn limit_caps_returned_rows() {
    let pool = fresh_skills_pool().await;
    for i in 0..10 {
        insert(
            &pool,
            &format!("r{i}"),
            &format!("Rule {i}"),
            "pr_review",
            Some("acme/billing"),
            "active",
            &format!("2026-05-{:02}T10:00:00Z", i + 1),
        )
        .await;
    }
    let aliases = vec!["acme/billing".to_owned()];
    let rows = new_rules_since(&pool, None, &aliases, 5)
        .await
        .expect("query ok");
    assert_eq!(rows.len(), 5, "limit not enforced: {rows:?}");
    // Newest first → Rule 9 ranks first.
    assert_eq!(rows[0].title, "Rule 9");
}
