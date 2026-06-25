use sqlx::sqlite::SqlitePool;
use std::collections::HashMap;
use std::sync::OnceLock;
use tokio::sync::Mutex;

use crate::context::rule_source::RuleIndexState;
use crate::error::CoreError;

use super::schema::{
    RULE_INDEX_META_VERSION, index_db_path_for_project, open_pool_at, read_meta, write_meta,
};

pub async fn rule_index_is_current(
    pool: &SqlitePool,
    state: &RuleIndexState,
) -> Result<bool, CoreError> {
    let chunk_count: i64 = sqlx::query_scalar!(r#"SELECT COUNT(*) as "n!: i64" FROM rule_chunks"#)
        .fetch_one(pool)
        .await?;
    let fts_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM rule_chunks_fts")
        .fetch_one(pool)
        .await?;
    if fts_count != chunk_count {
        return Ok(false);
    }
    if state.rule_count > 0 && chunk_count == 0 {
        return Ok(false);
    }
    let version = read_meta(pool, "rule_index_version").await?;
    if version.as_deref() != Some(RULE_INDEX_META_VERSION) {
        return Ok(false);
    }
    let count = read_meta(pool, "skills_count").await?;
    if count.as_deref() != Some(&state.rule_count.to_string()) {
        return Ok(false);
    }
    let embedding_profile = read_meta(pool, "embedding_profile").await?;
    if embedding_profile.as_deref() != Some(state.embedding_profile.as_str()) {
        return Ok(false);
    }
    // When the corpus was scoped to a repo (`scope_signature` is `Some`), the
    // persisted signature must match exactly; otherwise a scope swap with the
    // same count/timestamp would serve the wrong scope's chunks. `None` is
    // scope-agnostic (whole-corpus) and skips this gate.
    if let Some(expected) = state.scope_signature.as_deref() {
        let stored = read_meta(pool, "skills_scope_signature").await?;
        if stored.as_deref() != Some(expected) {
            return Ok(false);
        }
    }
    let max_updated_at = read_meta(pool, "skills_max_updated_at").await?;
    Ok(max_updated_at == state.max_updated_at)
}

pub async fn mark_rule_index_current(
    pool: &SqlitePool,
    state: &RuleIndexState,
) -> Result<(), CoreError> {
    write_meta(pool, "rule_index_version", RULE_INDEX_META_VERSION).await?;
    write_meta(pool, "skills_count", &state.rule_count.to_string()).await?;
    write_meta(pool, "embedding_profile", &state.embedding_profile).await?;
    // Scope-agnostic callers persist an empty marker instead of deleting the
    // row: the freshness check only consults this key when the incoming state
    // is `Some(..)`, so an empty marker is inert. Reusing `write_meta` avoids
    // a second SQL string for the offline sqlx cache to track.
    let scope_marker = state.scope_signature.as_deref().unwrap_or_default();
    write_meta(pool, "skills_scope_signature", scope_marker).await?;
    match &state.max_updated_at {
        Some(ts) => write_meta(pool, "skills_max_updated_at", ts).await?,
        None => {
            sqlx::query!("DELETE FROM rule_index_meta WHERE key = 'skills_max_updated_at'")
                .execute(pool)
                .await?;
        }
    }
    Ok(())
}

/// Process-wide cache of per-project index pools, keyed by `project_hash`.
/// Lazily populated; pools live for the process lifetime.
fn pool_cache() -> &'static Mutex<HashMap<String, SqlitePool>> {
    static CACHE: OnceLock<Mutex<HashMap<String, SqlitePool>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Get or lazily create the `index_db` pool for a project. Creation is
/// serialised on the cache mutex so racing callers produce exactly one pool.
pub async fn get_pool_for_project(project_hash: &str) -> Result<SqlitePool, CoreError> {
    let mut cache = pool_cache().lock().await;
    if let Some(existing) = cache.get(project_hash) {
        return Ok(existing.clone());
    }
    let path = index_db_path_for_project(project_hash);
    let pool = open_pool_at(&path).await?;
    cache.insert(project_hash.to_owned(), pool.clone());
    Ok(pool)
}

/// Open a standalone, fully-migrated index-DB pool at an explicit path.
///
/// Unlike [`get_pool_for_project`], this is neither cached nor tied to
/// `~/.difflore/projects/{hash}/` — the caller owns the path. For ephemeral
/// indexes (e.g. a `TempDir` backing `difflore try`) that must leave no trace.
pub async fn open_index_pool_at(path: &std::path::Path) -> Result<SqlitePool, CoreError> {
    open_pool_at(path).await
}

/// Resolve the per-project pool for the current working directory. Callers
/// that already hold a hash should call `get_pool_for_project` directly.
pub async fn get_pool_for_cwd() -> Result<SqlitePool, CoreError> {
    let root = crate::infra::db::current_project_root();
    let hash = crate::infra::db::project_hash_from_root(&root);
    get_pool_for_project(&hash).await
}
