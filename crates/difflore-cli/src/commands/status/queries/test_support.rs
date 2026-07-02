//! Shared in-memory SQLite fixtures for the `queries` submodule unit tests.
//!
//! Tables are minimal hand-rolled subsets of the production schema (not the
//! real migrations) so each test seeds only the rows it exercises. Every test
//! gets its own fresh `sqlite::memory:` pool, so no state leaks across tests.

pub(super) struct ProvenRuleSeed<'a> {
    pub(super) id: &'a str,
    pub(super) name: &'a str,
    pub(super) repo: &'a str,
    pub(super) file: &'a str,
    pub(super) accepted: usize,
}

pub(super) async fn proven_rule_pool() -> difflore_core::SqlitePool {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("open sqlite");
    sqlx::query(
        "CREATE TABLE skills (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            description TEXT NOT NULL DEFAULT '',
            source_repo TEXT,
            status TEXT
         )",
    )
    .execute(&pool)
    .await
    .expect("create skills");
    sqlx::query(
        "CREATE TABLE fix_outcomes (
            id TEXT PRIMARY KEY,
            rule_id TEXT,
            rule_name TEXT NOT NULL,
            file_path TEXT,
            repo_full_name TEXT,
            accepted INTEGER NOT NULL,
            applied_ok INTEGER NOT NULL,
            created_at TEXT NOT NULL
         )",
    )
    .execute(&pool)
    .await
    .expect("create fix_outcomes");
    pool
}

pub(super) async fn value_loop_pool() -> difflore_core::SqlitePool {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("open sqlite");
    sqlx::query(
        "CREATE TABLE skills (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            description TEXT NOT NULL DEFAULT '',
            source_repo TEXT,
            status TEXT
         )",
    )
    .execute(&pool)
    .await
    .expect("create skills");
    sqlx::query(
        "CREATE TABLE rule_events (
            id TEXT PRIMARY KEY,
            skill_id TEXT NOT NULL,
            kind TEXT NOT NULL,
            source TEXT,
            confidence_before REAL,
            confidence_after REAL,
            reason TEXT,
            metadata TEXT,
            created_at TEXT DEFAULT (datetime('now')) NOT NULL
         )",
    )
    .execute(&pool)
    .await
    .expect("create rule_events");
    sqlx::query(
        "CREATE TABLE rule_outcomes (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            rule_id TEXT NOT NULL,
            kind TEXT NOT NULL,
            session_id TEXT,
            repo_full_name TEXT,
            file_path TEXT,
            query_hash TEXT,
            rank INTEGER,
            top_k INTEGER,
            strict_file_match INTEGER NOT NULL DEFAULT 0,
            created_at TEXT DEFAULT (datetime('now')) NOT NULL
         )",
    )
    .execute(&pool)
    .await
    .expect("create rule_outcomes");
    sqlx::query(
        "CREATE TABLE mcp_rule_serves (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            tool TEXT NOT NULL,
            session_id TEXT,
            repo_full_name TEXT,
            file_path TEXT,
            query_hash TEXT NOT NULL,
            rule_ids_json TEXT NOT NULL DEFAULT '[]',
            rule_count INTEGER NOT NULL DEFAULT 0,
            top_k INTEGER NOT NULL DEFAULT 0,
            was_empty INTEGER NOT NULL DEFAULT 0,
            strict_match_count INTEGER NOT NULL DEFAULT 0,
            estimated_tokens INTEGER NOT NULL DEFAULT 0,
            served_at TEXT DEFAULT (datetime('now')) NOT NULL
         )",
    )
    .execute(&pool)
    .await
    .expect("create mcp_rule_serves");
    sqlx::query(
        "CREATE TABLE fix_outcomes (
            id TEXT PRIMARY KEY,
            rule_id TEXT,
            rule_name TEXT NOT NULL,
            file_path TEXT,
            repo_full_name TEXT,
            pr_number INTEGER,
            diff_signature TEXT,
            accepted INTEGER NOT NULL,
            applied_ok INTEGER NOT NULL,
            created_at TEXT NOT NULL
         )",
    )
    .execute(&pool)
    .await
    .expect("create fix_outcomes");
    sqlx::query(
        "CREATE TABLE review_items (
            id TEXT PRIMARY KEY,
            file_path TEXT NOT NULL,
            status TEXT,
            source TEXT,
            source_kind TEXT,
            repo_full_name TEXT,
            pr_number INTEGER
         )",
    )
    .execute(&pool)
    .await
    .expect("create review_items");
    sqlx::query(
        "CREATE TABLE review_comments (
            id TEXT PRIMARY KEY,
            review_item_id TEXT NOT NULL,
            external_comment_id TEXT,
            line_number INTEGER NOT NULL,
            content TEXT NOT NULL,
            comment_url TEXT,
            created_at TEXT DEFAULT (datetime('now')) NOT NULL
         )",
    )
    .execute(&pool)
    .await
    .expect("create review_comments");
    pool
}

pub(super) async fn insert_source_proof(
    pool: &difflore_core::SqlitePool,
    rule_id: &str,
    metadata: serde_json::Value,
) {
    let event_id = format!("event-{rule_id}");
    let metadata_str = metadata.to_string();
    sqlx::query!(
        "INSERT INTO rule_events
             (id, skill_id, kind, source, reason, metadata)
             VALUES (?1, ?2, 'source_proof', 'candidate_promotion',
                     'Promoted review-memory candidate', ?3)",
        event_id,
        rule_id,
        metadata_str,
    )
    .execute(pool)
    .await
    .expect("insert source proof");
}

pub(super) async fn insert_proven_rule(pool: &difflore_core::SqlitePool, seed: ProvenRuleSeed<'_>) {
    insert_skill_only(pool, seed.id, seed.name, seed.repo).await;

    for index in 0..seed.accepted {
        let id = format!("{}-{index}", seed.id);
        let created_at = format!("2026-05-0{} 00:00:00", index + 1);
        sqlx::query(
            "INSERT INTO fix_outcomes
                 (id, rule_id, rule_name, file_path, repo_full_name, accepted, applied_ok, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 1, 1, ?6)",
        )
        .bind(id)
        .bind(seed.id)
        .bind(seed.name)
        .bind(seed.file)
        .bind(seed.repo)
        .bind(created_at)
        .execute(pool)
        .await
        .expect("insert fix outcome");
    }
}

pub(super) async fn insert_skill_only(
    pool: &difflore_core::SqlitePool,
    id: &str,
    name: &str,
    repo: &str,
) {
    sqlx::query(
        "INSERT INTO skills (id, name, description, source_repo, status)
         VALUES (?1, ?2, '', ?3, 'active')",
    )
    .bind(id)
    .bind(name)
    .bind(repo)
    .execute(pool)
    .await
    .expect("insert skill");
}
