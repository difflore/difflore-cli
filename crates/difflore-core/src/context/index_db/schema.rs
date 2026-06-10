use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};
use std::path::PathBuf;

use crate::errors::CoreError;

pub(super) const INDEX_DB_NAME: &str = "context-index.db";

pub(super) const RULE_INDEX_META_VERSION: &str = "2";

#[allow(dead_code)]
pub struct IndexedRuleChunk {
    pub id: String,
    pub skill_id: String,
    pub content: String,
    pub embedding: Vec<f32>,
    /// JSON-serialised glob list, NULL = universal. Used by the
    /// `retrieve_rules_with_confidence` cascade to drop rules whose
    /// patterns don't match the file the agent is editing.
    pub file_patterns: Option<String>,
    /// Denormalised from the `skills` row's tags so retrieval can filter on
    /// language without joining back to data.db.
    pub language: Option<String>,
    /// Denormalised from `skills.source_repo`. NULL is unattributed metadata,
    /// not a runtime cross-repo/global rule.
    pub repo_scope: Option<String>,
}

/// Metadata pre-filter for `query_rule_chunks` / `fts_search`. Each set field
/// is an AND clause; `repo_scope` matches exactly. Callers injecting rules
/// into agents should pass the current repo and avoid unscoped fallback.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueryFilter {
    pub language: Option<String>,
    pub repo_scope: Option<String>,
}

impl QueryFilter {
    /// True when the filter has no effect (both fields unset).
    pub const fn is_empty(&self) -> bool {
        self.language.is_none() && self.repo_scope.is_none()
    }
}

/// Per-project path: `~/.difflore/projects/{hash}/context-index.db`.
/// Public so supporting tools can target the same file the runtime opens.
pub fn index_db_path_for_project(project_hash: &str) -> PathBuf {
    crate::db::project_index_dir(project_hash).join(INDEX_DB_NAME)
}

/// Retired global path: `~/.difflore/context-index.db`. Used only by the
/// startup guard to fail closed when a pre-split DB is present.
pub(crate) fn retired_global_index_db_path() -> Result<PathBuf, CoreError> {
    Ok(crate::paths::data_home()
        .map_err(CoreError::Internal)?
        .join(INDEX_DB_NAME))
}

pub(super) fn embedding_to_blob(emb: &[f32]) -> Vec<u8> {
    emb.iter().flat_map(|f| f.to_le_bytes()).collect()
}

pub(super) fn blob_to_embedding(blob: &[u8]) -> Result<Vec<f32>, CoreError> {
    if !blob.len().is_multiple_of(4) {
        return Err(CoreError::Internal(format!(
            "embedding blob length {} is not a multiple of 4",
            blob.len()
        )));
    }
    Ok(blob
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect())
}

/// Idempotent ALTER that swallows the "duplicate column" error but surfaces
/// everything else, for nullable columns added after the initial schema. The
/// per-project index DB is created programmatically, not via numbered
/// migrations.
// Uses runtime `query` (not `query!`): SQLite has no `ADD COLUMN IF NOT
// EXISTS`, and the macro would prepare against the migration DB that already
// has these columns and always fail.
async fn ensure_column(
    pool: &SqlitePool,
    table: &str,
    column: &str,
    ddl_type: &str,
) -> Result<(), CoreError> {
    let sql = format!("ALTER TABLE {table} ADD COLUMN {column} {ddl_type}");
    if let Err(e) = sqlx::query(&sql).execute(pool).await {
        let msg = e.to_string();
        if !msg.contains("duplicate column name") {
            return Err(CoreError::Internal(format!(
                "{table}.{column} migration failed: {msg}"
            )));
        }
    }
    Ok(())
}

/// Open a fresh pool at an arbitrary path and run the chunk-table DDL. Shared
/// by `get_pool_for_project` (cached) and the migration utility (one-shot) so
/// both use the same journal mode, columns, and idempotent ALTERs.
pub(crate) async fn open_pool_at(path: &std::path::Path) -> Result<SqlitePool, CoreError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let opts = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .busy_timeout(std::time::Duration::from_secs(30));

    let pool = SqlitePoolOptions::new()
        .max_connections(3)
        .connect_with(opts)
        .await
        .map_err(|e| CoreError::Internal(format!("failed to open index db: {e}")))?;

    // Index-specific tables (separate DB from the main app DB). Metadata
    // columns stay nullable so backfill is optional.
    sqlx::query!(
        "CREATE TABLE IF NOT EXISTS rule_chunks (
            id TEXT PRIMARY KEY,
            skill_id TEXT NOT NULL,
            content TEXT NOT NULL,
            embedding BLOB,
            file_patterns TEXT,
            language TEXT,
            repo_scope TEXT
        )"
    )
    .execute(&pool)
    .await
    .map_err(|e| CoreError::Internal(format!("index db migration failed: {e}")))?;

    // Idempotent ALTERs for pre-existing index DBs; `ensure_column` swallows
    // the "duplicate column" error and propagates the rest.
    ensure_column(&pool, "rule_chunks", "file_patterns", "TEXT").await?;
    ensure_column(&pool, "rule_chunks", "language", "TEXT").await?;
    ensure_column(&pool, "rule_chunks", "repo_scope", "TEXT").await?;

    // FTS5 virtual table for keyword retrieval. Porter stemmer + unicode61
    // folding collapse `parsing`/`parsed`/`parses` to one token.
    //
    // `rule_chunks.id` is a TEXT primary key, not an INTEGER rowid, so it
    // can't be FTS5's rowid; carry it in `chunk_id UNINDEXED` and join back
    // to `rule_chunks` at query time.
    sqlx::query!(
        "CREATE VIRTUAL TABLE IF NOT EXISTS rule_chunks_fts USING fts5(
            chunk_id UNINDEXED,
            content,
            tokenize='porter unicode61'
        )"
    )
    .execute(&pool)
    .await
    .map_err(|e| CoreError::Internal(format!("fts5 virtual table creation failed: {e}")))?;

    sqlx::query!(
        "CREATE TABLE IF NOT EXISTS rule_index_meta (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        )"
    )
    .execute(&pool)
    .await
    .map_err(|e| CoreError::Internal(format!("index meta table creation failed: {e}")))?;

    sqlx::query!(
        "CREATE TRIGGER IF NOT EXISTS rule_chunks_ai AFTER INSERT ON rule_chunks BEGIN
            INSERT INTO rule_chunks_fts(chunk_id, content) VALUES (new.id, new.content);
        END"
    )
    .execute(&pool)
    .await
    .map_err(|e| CoreError::Internal(format!("fts5 AI trigger failed: {e}")))?;

    sqlx::query!(
        "CREATE TRIGGER IF NOT EXISTS rule_chunks_au AFTER UPDATE ON rule_chunks BEGIN
            DELETE FROM rule_chunks_fts WHERE chunk_id = old.id;
            INSERT INTO rule_chunks_fts(chunk_id, content) VALUES (new.id, new.content);
        END"
    )
    .execute(&pool)
    .await
    .map_err(|e| CoreError::Internal(format!("fts5 AU trigger failed: {e}")))?;

    sqlx::query!(
        "CREATE TRIGGER IF NOT EXISTS rule_chunks_ad AFTER DELETE ON rule_chunks BEGIN
            DELETE FROM rule_chunks_fts WHERE chunk_id = old.id;
        END"
    )
    .execute(&pool)
    .await
    .map_err(|e| CoreError::Internal(format!("fts5 AD trigger failed: {e}")))?;

    let fts_count: i64 =
        sqlx::query_scalar!(r#"SELECT COUNT(*) as "n!: i64" FROM rule_chunks_fts"#)
            .fetch_one(&pool)
            .await
            .unwrap_or(0);
    let base_count: i64 = sqlx::query_scalar!(r#"SELECT COUNT(*) as "n!: i64" FROM rule_chunks"#)
        .fetch_one(&pool)
        .await
        .unwrap_or(0);
    if fts_count == 0 && base_count > 0 {
        sqlx::query!(
            "INSERT INTO rule_chunks_fts(chunk_id, content) \
             SELECT id, content FROM rule_chunks"
        )
        .execute(&pool)
        .await
        .map_err(|e| CoreError::Internal(format!("fts5 back-fill failed: {e}")))?;
    }

    Ok(pool)
}

pub(super) async fn read_meta(pool: &SqlitePool, key: &str) -> Result<Option<String>, CoreError> {
    let value = sqlx::query_scalar!("SELECT value FROM rule_index_meta WHERE key = ?1", key)
        .fetch_optional(pool)
        .await?;
    Ok(value)
}

pub(super) async fn write_meta(pool: &SqlitePool, key: &str, value: &str) -> Result<(), CoreError> {
    sqlx::query!(
        "INSERT INTO rule_index_meta (key, value)
         VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        key,
        value
    )
    .execute(pool)
    .await?;
    Ok(())
}
