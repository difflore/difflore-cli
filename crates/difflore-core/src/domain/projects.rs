use uuid::Uuid;

use crate::error::CoreError;
use crate::domain::models::{AddProjectInput, ProjectRecord, RemoveProjectInput};

#[derive(sqlx::FromRow)]
struct ProjectRow {
    id: String,
    name: String,
    path: String,
    git_branch: Option<String>,
    active_sessions: i64,
    created_at: String,
}

impl From<ProjectRow> for ProjectRecord {
    fn from(r: ProjectRow) -> Self {
        Self {
            id: r.id,
            name: r.name,
            path: r.path,
            git_branch: r.git_branch,
            active_sessions: i32::try_from(r.active_sessions).unwrap_or(i32::MAX),
            total_sessions: None,
            created_at: r.created_at,
        }
    }
}

pub async fn list(db: &sqlx::SqlitePool) -> crate::Result<Vec<ProjectRecord>> {
    let rows = sqlx::query_as!(
        ProjectRow,
        "SELECT id, name, path, git_branch, active_sessions, created_at
         FROM projects ORDER BY created_at DESC"
    )
    .fetch_all(db)
    .await?;

    Ok(rows.into_iter().map(ProjectRecord::from).collect())
}

pub async fn get(
    db: &sqlx::SqlitePool,
    input: RemoveProjectInput,
) -> crate::Result<Option<ProjectRecord>> {
    if input.id.trim().is_empty() {
        return Err(CoreError::Validation("id is required".into()));
    }
    let row = sqlx::query_as!(
        ProjectRow,
        "SELECT id, name, path, git_branch, active_sessions, created_at
         FROM projects WHERE id = ?1",
        input.id
    )
    .fetch_optional(db)
    .await?;

    Ok(row.map(ProjectRecord::from))
}

pub async fn add(db: &sqlx::SqlitePool, input: AddProjectInput) -> crate::Result<ProjectRecord> {
    let path = normalize_project_path(&input.path)?;
    let path_str = path.to_string_lossy().to_string();

    if path_str.trim().is_empty() {
        return Err(CoreError::Validation("path is required".into()));
    }
    let existing = sqlx::query_as!(
        ProjectRow,
        "SELECT id, name, path, git_branch, active_sessions, created_at
         FROM projects WHERE path = ?1",
        path_str
    )
    .fetch_optional(db)
    .await?;

    if let Some(p) = existing {
        return Ok(ProjectRecord::from(p));
    }

    let name = path
        .file_name()
        .and_then(|v| v.to_str())
        .filter(|v| !v.is_empty())
        .unwrap_or("project")
        .to_owned();

    let id = format!("project-{}", Uuid::new_v4());
    let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();

    sqlx::query!(
        "INSERT INTO projects (id, name, path, active_sessions, created_at) VALUES (?1, ?2, ?3, 0, ?4)",
        id,
        name,
        path_str,
        now
    )
    .execute(db)
    .await?;

    Ok(ProjectRecord {
        id,
        name,
        path: path_str,
        git_branch: None,
        active_sessions: 0,
        total_sessions: Some(0),
        created_at: now,
    })
}

fn normalize_project_path(raw: &str) -> crate::Result<std::path::PathBuf> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(CoreError::Validation("path is required".into()));
    }

    let path = std::path::Path::new(trimmed);
    let canonical = path.canonicalize().map_err(|e| {
        CoreError::Validation(format!("project path must be an existing directory: {e}"))
    })?;
    if !canonical.is_dir() {
        return Err(CoreError::Validation(format!(
            "project path must be a directory: {}",
            canonical.display()
        )));
    }
    if canonical.parent().is_none() {
        return Err(CoreError::Validation(
            "refusing to register a filesystem root as a project".into(),
        ));
    }
    Ok(canonical)
}

pub async fn remove(db: &sqlx::SqlitePool, input: RemoveProjectInput) -> crate::Result<()> {
    if input.id.trim().is_empty() {
        return Err(CoreError::Validation("id is required".into()));
    }
    let result = sqlx::query!("DELETE FROM projects WHERE id = ?1", input.id)
        .execute(db)
        .await?;
    if result.rows_affected() == 0 {
        return Err(CoreError::NotFound(format!(
            "project '{}' not found.",
            input.id
        )));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_row(
        id: &str,
        name: &str,
        path: &str,
        branch: Option<&str>,
        sessions: i64,
    ) -> ProjectRow {
        ProjectRow {
            id: id.into(),
            name: name.into(),
            path: path.into(),
            git_branch: branch.map(String::from),
            active_sessions: sessions,
            created_at: "2026-04-10 12:00:00".into(),
        }
    }

    #[test]
    fn project_row_into_record_copies_all_fields() {
        let row = make_row("p-1", "my-proj", "/home/me/code", Some("main"), 3);
        let rec = ProjectRecord::from(row);
        assert_eq!(rec.id, "p-1");
        assert_eq!(rec.name, "my-proj");
        assert_eq!(rec.path, "/home/me/code");
        assert_eq!(rec.git_branch.as_deref(), Some("main"));
        assert_eq!(rec.active_sessions, 3);
        assert_eq!(rec.total_sessions, None);
        assert_eq!(rec.created_at, "2026-04-10 12:00:00");
    }

    #[test]
    fn normalize_project_path_rejects_missing_file_and_root() {
        let err = normalize_project_path("").unwrap_err().to_string();
        assert!(err.contains("path is required"), "unexpected: {err}");

        let missing = normalize_project_path("/definitely/not/difflore")
            .unwrap_err()
            .to_string();
        assert!(
            missing.contains("existing directory"),
            "unexpected: {missing}"
        );

        let root = std::path::Path::new(std::path::MAIN_SEPARATOR_STR);
        if root.exists() {
            let err = normalize_project_path(root.to_string_lossy().as_ref())
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("filesystem root"),
                "root should be rejected: {err}"
            );
        }
    }
}
