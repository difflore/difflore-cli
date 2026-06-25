#![allow(clippy::unwrap_used)]
#![allow(unsafe_code)]
//! Regression tests for retired local persistence migrations.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use difflore_core::context::index_db;
use difflore_core::infra::db;
use difflore_core::migration;
use tempfile::TempDir;

static ENV_LOCK: Mutex<()> = Mutex::new(());

struct EnvGuard<'a> {
    _lock: std::sync::MutexGuard<'a, ()>,
    _tmp: TempDir,
    home: PathBuf,
}

impl EnvGuard<'_> {
    fn new() -> Self {
        let lock = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().to_path_buf();
        // SAFETY: serialized by ENV_LOCK and restored in Drop.
        unsafe {
            std::env::set_var("DIFFLORE_HOME", &home);
        }
        Self {
            _lock: lock,
            _tmp: tmp,
            home,
        }
    }

    fn home(&self) -> &Path {
        &self.home
    }
}

impl Drop for EnvGuard<'_> {
    fn drop(&mut self) {
        // SAFETY: ENV_LOCK is held.
        unsafe {
            std::env::remove_var("DIFFLORE_HOME");
        }
    }
}

#[tokio::test]
async fn retired_split_migration_is_noop_on_fresh_home() {
    let guard = EnvGuard::new();

    migration::run_if_needed().await.unwrap();

    assert!(
        !guard.home().join(".migrated_v1_per_project_index").exists(),
        "retired migration must not write historical sentinels"
    );
}

#[tokio::test]
async fn retired_split_migration_refuses_retired_global_index() {
    let guard = EnvGuard::new();
    let retired_index = guard.home().join("context-index.db");
    std::fs::write(&retired_index, b"retired index").unwrap();

    let result = migration::run_if_needed().await;
    assert!(
        result.as_ref().err().is_some_and(|e| e
            .to_string()
            .contains("retired context-index split migration")),
        "retired global index must fail closed: {result:?}"
    );
    assert!(
        retired_index.exists(),
        "retired global index must be left untouched"
    );
    assert!(
        !guard.home().join("backups").exists(),
        "retired migration must not create historical backups"
    );
}

#[tokio::test]
async fn memory_conflicts_migration_applies() {
    use sqlx::sqlite::SqlitePoolOptions;

    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    db::run_migrations(&pool).await.unwrap();

    // The embedded migration must create the reviewable-conflict table; a
    // fresh DB starts with zero records.
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM memory_conflicts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "fresh memory_conflicts table starts empty");
}

#[tokio::test]
async fn get_pool_for_project_roundtrip_in_home() {
    let _guard = EnvGuard::new();

    let hash = db::project_hash_from_root(Path::new("/tmp/example-project-root"));
    let pool = index_db::get_pool_for_project(&hash).await.unwrap();
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM rule_chunks")
        .fetch_one(&pool)
        .await
        .unwrap();

    assert_eq!(n, 0, "fresh per-project DB starts empty");
}
