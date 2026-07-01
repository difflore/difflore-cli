use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};

use crate::{CoreError, Result};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RepoAliasRecord {
    pub root_path: String,
    pub project_hash: String,
    pub repo_scope: String,
    pub source: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RepoScopeDetection {
    pub repo_scopes: Vec<String>,
    pub detected_remotes: Vec<String>,
    pub manual_aliases: Vec<RepoAliasRecord>,
}

const MANUAL_ALIAS_SOURCE: &str = "manual";

pub async fn set_manual_alias(
    db: &SqlitePool,
    root_path: &Path,
    repo_scope: &str,
) -> Result<RepoAliasRecord> {
    ensure_repo_aliases_table(db).await?;
    let root_path = normalize_local_path(root_path)?;
    let scope = canonical_repo_scope(repo_scope)?;
    let project_hash = crate::infra::db::project_hash_from_root(Path::new(&root_path));

    sqlx::query(
        "INSERT INTO repo_aliases \
            (root_path, project_hash, repo_scope, source, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, datetime('now'), datetime('now')) \
         ON CONFLICT(project_hash, repo_scope) DO UPDATE SET \
            root_path = excluded.root_path, \
            source = excluded.source, \
            updated_at = datetime('now')",
    )
    .bind(&root_path)
    .bind(&project_hash)
    .bind(&scope)
    .bind(MANUAL_ALIAS_SOURCE)
    .execute(db)
    .await?;

    get_alias(db, &project_hash, &scope)
        .await?
        .ok_or_else(|| CoreError::internal("repo alias was not persisted"))
}

pub async fn clear_manual_aliases_for_path(db: &SqlitePool, root_path: &Path) -> Result<usize> {
    ensure_repo_aliases_table(db).await?;
    let root_path = normalize_local_path(root_path)?;
    let project_hash = crate::infra::db::project_hash_from_root(Path::new(&root_path));
    let result = sqlx::query(
        "DELETE FROM repo_aliases \
         WHERE project_hash = ?1 AND source = ?2",
    )
    .bind(project_hash)
    .bind(MANUAL_ALIAS_SOURCE)
    .execute(db)
    .await?;
    Ok(usize::try_from(result.rows_affected()).unwrap_or(usize::MAX))
}

pub async fn list_aliases(db: &SqlitePool) -> Result<Vec<RepoAliasRecord>> {
    ensure_repo_aliases_table(db).await?;
    let rows = sqlx::query(
        "SELECT root_path, project_hash, repo_scope, source, created_at, updated_at \
         FROM repo_aliases \
         ORDER BY root_path ASC, repo_scope ASC",
    )
    .fetch_all(db)
    .await?;
    Ok(rows.iter().map(alias_from_row).collect())
}

pub async fn aliases_for_path(db: &SqlitePool, path: &Path) -> Result<Vec<RepoAliasRecord>> {
    ensure_repo_aliases_table(db).await?;
    let candidate = normalize_local_path(path)?;
    let mut matches = list_aliases(db)
        .await?
        .into_iter()
        .filter(|alias| path_is_inside_root(&candidate, &alias.root_path))
        .collect::<Vec<_>>();
    matches.sort_by(|a, b| {
        b.root_path
            .len()
            .cmp(&a.root_path.len())
            .then_with(|| a.repo_scope.cmp(&b.repo_scope))
    });

    let longest = matches.first().map(|alias| alias.root_path.len());
    if let Some(longest) = longest {
        matches.retain(|alias| alias.root_path.len() == longest);
    }
    Ok(matches)
}

pub async fn detect_repo_scopes_for_path(
    db: &SqlitePool,
    project_path: &str,
    configured_gitlab_hosts: &[String],
) -> Result<RepoScopeDetection> {
    let detected_remotes = crate::infra::git::detect_repo_full_names_with_gitlab_hosts(
        project_path,
        configured_gitlab_hosts,
    );
    let expanded_remotes =
        crate::skills::expand_repo_scopes_with_source_aliases(db, &detected_remotes)
            .await
            .unwrap_or_else(|_| detected_remotes.clone());
    let manual_aliases = aliases_for_path(db, Path::new(project_path)).await?;
    let alias_scopes = manual_aliases
        .iter()
        .map(|alias| alias.repo_scope.clone())
        .collect::<Vec<_>>();
    let repo_scopes = merge_repo_scopes(alias_scopes, expanded_remotes);

    Ok(RepoScopeDetection {
        repo_scopes,
        detected_remotes,
        manual_aliases,
    })
}

pub fn merge_repo_scopes(
    primary_scopes: impl IntoIterator<Item = String>,
    secondary_scopes: impl IntoIterator<Item = String>,
) -> Vec<String> {
    let mut out = Vec::new();
    for scope in primary_scopes.into_iter().chain(secondary_scopes) {
        let Some(scope) = crate::infra::git::normalize_canonical_repo_scope(&scope) else {
            continue;
        };
        if !out.iter().any(|existing| existing == &scope) {
            out.push(scope);
        }
    }
    out
}

fn canonical_repo_scope(raw: &str) -> Result<String> {
    crate::infra::git::RepoScope::canonical(raw)
        .map(crate::infra::git::RepoScope::into_string)
        .ok_or_else(|| {
            CoreError::Validation(
                "repo alias must be GitHub owner/repo or canonical GitLab host/group/project"
                    .to_owned(),
            )
        })
}

async fn get_alias(
    db: &SqlitePool,
    project_hash: &str,
    repo_scope: &str,
) -> Result<Option<RepoAliasRecord>> {
    let row = sqlx::query(
        "SELECT root_path, project_hash, repo_scope, source, created_at, updated_at \
         FROM repo_aliases \
         WHERE project_hash = ?1 AND repo_scope = ?2",
    )
    .bind(project_hash)
    .bind(repo_scope)
    .fetch_optional(db)
    .await?;
    Ok(row.map(|row| alias_from_row(&row)))
}

fn alias_from_row(row: &sqlx::sqlite::SqliteRow) -> RepoAliasRecord {
    RepoAliasRecord {
        root_path: row.try_get("root_path").unwrap_or_default(),
        project_hash: row.try_get("project_hash").unwrap_or_default(),
        repo_scope: row.try_get("repo_scope").unwrap_or_default(),
        source: row.try_get("source").unwrap_or_default(),
        created_at: row.try_get("created_at").unwrap_or_default(),
        updated_at: row.try_get("updated_at").unwrap_or_default(),
    }
}

pub fn normalize_local_path(path: &Path) -> Result<String> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let normalized = canonicalize_best_effort(&absolute);
    let raw = normalized.to_string_lossy().replace('\\', "/");
    Ok(trim_trailing_slashes(&raw))
}

fn canonicalize_best_effort(path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return canonical;
    }

    let mut suffix = PathBuf::new();
    let mut probe = path.to_path_buf();
    loop {
        if let Ok(canonical) = std::fs::canonicalize(&probe) {
            return canonical.join(suffix);
        }
        let Some(name) = probe.file_name().map(PathBuf::from) else {
            return path.to_path_buf();
        };
        suffix = name.join(suffix);
        if !probe.pop() {
            return path.to_path_buf();
        }
    }
}

fn trim_trailing_slashes(value: &str) -> String {
    if value == "/" {
        return "/".to_owned();
    }
    value.trim_end_matches('/').to_owned()
}

fn path_is_inside_root(path: &str, root: &str) -> bool {
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|rest| rest.starts_with('/'))
}

async fn ensure_repo_aliases_table(db: &SqlitePool) -> Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS repo_aliases (
            root_path TEXT NOT NULL,
            project_hash TEXT NOT NULL,
            repo_scope TEXT NOT NULL,
            source TEXT NOT NULL DEFAULT 'manual',
            created_at TEXT DEFAULT (datetime('now')) NOT NULL,
            updated_at TEXT DEFAULT (datetime('now')) NOT NULL,
            PRIMARY KEY (project_hash, repo_scope)
        )",
    )
    .execute(db)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_repo_aliases_root_path ON repo_aliases (root_path)",
    )
    .execute(db)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn memory_db() -> SqlitePool {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("memory db");
        crate::infra::db::run_migrations(&pool)
            .await
            .expect("migrations");
        pool
    }

    #[tokio::test]
    async fn aliases_for_path_uses_the_most_specific_root() {
        let pool = memory_db().await;
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("root");
        let nested = root.join("nested");
        std::fs::create_dir_all(nested.join("src")).expect("dirs");

        set_manual_alias(&pool, &root, "acme/root")
            .await
            .expect("root alias");
        set_manual_alias(&pool, &nested, "acme/nested")
            .await
            .expect("nested alias");

        let aliases = aliases_for_path(&pool, &nested.join("src/file.ts"))
            .await
            .expect("aliases");
        assert_eq!(
            aliases
                .iter()
                .map(|alias| alias.repo_scope.as_str())
                .collect::<Vec<_>>(),
            vec!["acme/nested"]
        );
    }

    #[test]
    fn merge_repo_scopes_prefers_manual_aliases_and_dedupes() {
        assert_eq!(
            merge_repo_scopes(
                vec!["Acme/App".to_owned(), "bad scope".to_owned()],
                vec!["acme/app".to_owned(), "upstream/app".to_owned()]
            ),
            vec!["acme/app".to_owned(), "upstream/app".to_owned()]
        );
    }
}
