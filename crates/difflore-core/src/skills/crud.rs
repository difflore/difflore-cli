use std::collections::{HashMap, HashSet};

use crate::domain::models::{
    InstallSkillInput, RemoveSkillInput, SkillRecord, ToggleSkillEngineInput,
};
use crate::error::CoreError;

use super::{SkillRow, fetch_skill_row_by_id, fetch_skill_row_by_id_optional};

/// Map `skill_id → source_repo` for every active rule. Lets local memory
/// surfaces default to the current repo without widening the stable
/// `SkillRecord` serde surface.
pub async fn list_source_repos(
    db: &sqlx::SqlitePool,
) -> crate::Result<HashMap<String, Option<String>>> {
    let rows = sqlx::query!("SELECT id, source_repo FROM skills WHERE status = 'active'")
        .fetch_all(db)
        .await?;
    let mut out = HashMap::with_capacity(rows.len());
    for row in rows {
        out.insert(row.id, row.source_repo);
    }
    Ok(out)
}

/// Expand git remotes with a conservative source-repo alias.
///
/// A fork's local `origin` (e.g. `difflore-fixtures/fastapi`) may differ from
/// the upstream repo the imported memory is scoped to (e.g. `fastapi/fastapi`).
/// When exactly one active source repo shares the same repository name, add it
/// as an extra recall scope. With zero or multiple candidates, keep only the
/// original remotes so unrelated repos never receive global memory.
pub async fn expand_repo_scopes_with_source_aliases(
    db: &sqlx::SqlitePool,
    repo_full_names: &[String],
) -> crate::Result<Vec<String>> {
    let mut scopes = Vec::new();
    let mut seen = HashSet::new();
    let mut repo_names = Vec::new();

    for raw in repo_full_names {
        let Some(repo) = crate::infra::git::normalize_canonical_repo_scope(raw) else {
            continue;
        };
        if seen.insert(repo.clone()) {
            if let Some((_, name)) = legacy_github_scope(&repo)
                && !repo_names.iter().any(|existing| existing == name)
            {
                repo_names.push(name.to_owned());
            }
            scopes.push(repo);
        }
    }

    for repo_name in repo_names {
        let pattern = format!("%/{repo_name}");
        let rows: Vec<String> = sqlx::query_scalar(
            "SELECT LOWER(source_repo)
             FROM skills
             WHERE status = 'active'
               AND source_repo IS NOT NULL
               AND TRIM(source_repo) <> ''
               AND LOWER(source_repo) LIKE ?1
             GROUP BY LOWER(source_repo)",
        )
        .bind(pattern)
        .fetch_all(db)
        .await?;

        let candidates: Vec<String> = rows
            .into_iter()
            .filter_map(|repo| crate::infra::git::normalize_canonical_repo_scope(&repo))
            .filter(|repo| {
                legacy_github_scope(repo).is_some_and(|(_, name)| name == repo_name)
                    && !seen.contains(repo)
            })
            .collect();

        if candidates.len() == 1 {
            let Some(alias) = candidates.into_iter().next() else {
                continue;
            };
            seen.insert(alias.clone());
            scopes.push(alias);
        }
    }

    Ok(scopes)
}

fn legacy_github_scope(scope: &str) -> Option<(&str, &str)> {
    let (owner, repo) = scope.split_once('/')?;
    if owner.contains('.') || repo.contains('/') || repo.is_empty() {
        return None;
    }
    Some((owner, repo))
}

pub async fn list(db: &sqlx::SqlitePool) -> crate::Result<Vec<SkillRecord>> {
    let rows = sqlx::query_as!(
        SkillRow,
        "SELECT id, name, source, directory, version, description, type, \
         engines, tags, trigger, check_prompt, repo_owner, repo_name, repo_branch, readme_url, \
         enabled_for_codex, enabled_for_claude, enabled_for_gemini, enabled_for_cursor, \
         installed_at, updated_at, origin FROM skills WHERE status = 'active' \
         ORDER BY installed_at DESC",
    )
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(SkillRecord::from).collect())
}

/// Like `list()` but without the `status='active'` filter, so callers can
/// see pending rows alongside active ones.
pub async fn list_all(db: &sqlx::SqlitePool) -> crate::Result<Vec<SkillRecord>> {
    let rows = sqlx::query_as!(
        SkillRow,
        "SELECT id, name, source, directory, version, description, type, \
         engines, tags, trigger, check_prompt, repo_owner, repo_name, repo_branch, readme_url, \
         enabled_for_codex, enabled_for_claude, enabled_for_gemini, enabled_for_cursor, \
         installed_at, updated_at, origin FROM skills ORDER BY installed_at DESC",
    )
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(SkillRecord::from).collect())
}

pub async fn add(db: &sqlx::SqlitePool, input: InstallSkillInput) -> crate::Result<SkillRecord> {
    if !crate::skills::fs::is_safe_skill_component(&input.owner)
        || !crate::skills::fs::is_safe_skill_component(&input.directory)
    {
        return Err(CoreError::Validation(
            "skill owner and directory must be safe single path components".to_owned(),
        ));
    }
    let id = format!("skill-{}-{}", input.owner, input.directory);
    let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let engines_json = serde_json::to_string(&["claude"])?;
    let tags_json = serde_json::to_string(&["github", "imported"])?;
    let name = input.directory.replace('-', " ");
    let description = format!("Imported from {}/{}", input.owner, input.repo);

    sqlx::query!(
        "INSERT OR IGNORE INTO skills
         (id, name, source, directory, version, description, type, engines, tags,
          repo_owner, repo_name, repo_branch, enabled_for_claude, installed_at, updated_at)
         VALUES (?1, ?2, 'github', ?3, '1.0.0', ?4, 'skill', ?5, ?6,
                 ?7, ?8, ?9, 1, ?10, ?10)",
        id,
        name,
        input.directory,
        description,
        engines_json,
        tags_json,
        input.owner,
        input.repo,
        input.branch,
        now
    )
    .execute(db)
    .await?;

    fetch_skill_row_by_id(db, &id).await
}

pub async fn remove(db: &sqlx::SqlitePool, input: RemoveSkillInput) -> crate::Result<()> {
    let skill: Option<SkillRecord> = fetch_skill_row_by_id_optional(db, &input.id).await?;

    // Fail loud on an unknown id; otherwise a typo returns a phantom
    // "Removed rule: X" that's impossible to debug.
    let Some(skill) = skill else {
        return Err(CoreError::NotFound(format!(
            "rule '{}' not found. Inspect local memory with `difflore status --json`.",
            input.id
        )));
    };

    sqlx::query!("DELETE FROM skills WHERE id = ?1", input.id)
        .execute(db)
        .await?;

    {
        for engine in &["codex", "claude", "gemini", "cursor"] {
            if let Err(e) =
                crate::skills::fs::sync_engine_link(&skill.source, &skill.directory, engine, false)
            {
                eprintln!("warning: sync_engine_link failed for engine {engine}: {e}");
            }
        }
        // Confine the recursive delete to the skills root: a crafted
        // `source`/`directory` (e.g. containing `..`) must never let
        // `remove_dir_all` escape and delete an arbitrary directory.
        if crate::skills::fs::is_safe_skill_component(&skill.source)
            && crate::skills::fs::is_safe_skill_component(&skill.directory)
        {
            let skill_dir = crate::skills::fs::skills_base_dir()?
                .join(&skill.source)
                .join(&skill.directory);
            if skill_dir.exists() {
                let _ = std::fs::remove_dir_all(&skill_dir);
            }
        }
    }

    Ok(())
}

pub async fn toggle_engine(
    db: &sqlx::SqlitePool,
    input: ToggleSkillEngineInput,
) -> crate::Result<()> {
    let val = i32::from(input.enabled);
    match input.engine.as_str() {
        "codex" => {
            sqlx::query!("UPDATE skills SET enabled_for_codex = ?1, updated_at = datetime('now') WHERE id = ?2",
                val, input.id).execute(db).await?;
        }
        "claude" => {
            sqlx::query!("UPDATE skills SET enabled_for_claude = ?1, updated_at = datetime('now') WHERE id = ?2",
                val, input.id).execute(db).await?;
        }
        "gemini" => {
            sqlx::query!("UPDATE skills SET enabled_for_gemini = ?1, updated_at = datetime('now') WHERE id = ?2",
                val, input.id).execute(db).await?;
        }
        "cursor" => {
            sqlx::query!("UPDATE skills SET enabled_for_cursor = ?1, updated_at = datetime('now') WHERE id = ?2",
                val, input.id).execute(db).await?;
        }
        other => return Err(CoreError::Internal(format!("unknown engine: {other}"))),
    }

    if let Some(skill) = fetch_skill_row_by_id_optional(db, &input.id).await? {
        let enabled =
            input.enabled && crate::skills::fs::skill_type_allows_engine_link(&skill.r#type);
        if let Err(e) = crate::skills::fs::sync_engine_link(
            &skill.source,
            &skill.directory,
            &input.engine,
            enabled,
        ) {
            eprintln!(
                "warning: sync_engine_link failed for engine {}: {e}",
                input.engine
            );
        }
    }

    Ok(())
}

pub async fn sync_links(db: &sqlx::SqlitePool) -> crate::Result<()> {
    #[derive(sqlx::FromRow)]
    struct SkillLinkRow {
        source: String,
        directory: String,
        r#type: String,
        enabled_for_codex: i64,
        enabled_for_claude: i64,
        enabled_for_gemini: i64,
        enabled_for_cursor: i64,
        status: String,
    }

    for engine in &["codex", "claude", "gemini", "cursor"] {
        if let Err(e) = crate::skills::fs::purge_review_standard_engine_links(engine, false) {
            eprintln!("warning: purge review-standard links failed for engine {engine}: {e}");
        }
    }

    let rows = sqlx::query_as::<_, SkillLinkRow>(
        "SELECT source, directory, type AS \"type\", enabled_for_codex, \
         enabled_for_claude, enabled_for_gemini, enabled_for_cursor, status \
         FROM skills",
    )
    .fetch_all(db)
    .await?;

    for skill in &rows {
        let allow_engine_link = skill.status == "active"
            && crate::skills::fs::skill_type_allows_engine_link(&skill.r#type);
        let engines = [
            ("codex", skill.enabled_for_codex != 0),
            ("claude", skill.enabled_for_claude != 0),
            ("gemini", skill.enabled_for_gemini != 0),
            ("cursor", skill.enabled_for_cursor != 0),
        ];
        for (engine, enabled) in engines {
            if let Err(e) = crate::skills::fs::sync_engine_link(
                &skill.source,
                &skill.directory,
                engine,
                allow_engine_link && enabled,
            ) {
                eprintln!("warning: sync_engine_link failed for engine {engine}: {e}");
            }
        }
    }

    Ok(())
}
