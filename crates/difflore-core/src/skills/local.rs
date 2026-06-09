use crate::errors::CoreError;
use crate::models::{CreateLocalSkillInput, SkillRecord};
use uuid::Uuid;

use super::SkillRow;

async fn record_engine_link_failure(
    db: &sqlx::SqlitePool,
    skill_id: &str,
    engine: &str,
    error: &std::io::Error,
) {
    let event_id = format!("rule-event-{}", Uuid::new_v4());
    let reason = format!("sync_engine_link failed for engine {engine}: {error}");
    let metadata = serde_json::json!({
        "engine": engine,
        "enabled": true,
        "error": error.to_string(),
    })
    .to_string();
    if let Err(insert_err) = sqlx::query(
        "INSERT INTO rule_events
         (id, skill_id, kind, source, reason, metadata)
         VALUES (?1, ?2, 'engine_link_failed', 'local_rule_create', ?3, ?4)",
    )
    .bind(event_id)
    .bind(skill_id)
    .bind(reason)
    .bind(metadata)
    .execute(db)
    .await
    {
        eprintln!("warning: failed to audit sync_engine_link failure: {insert_err}");
    }
}

pub async fn create_local(
    db: &sqlx::SqlitePool,
    input: CreateLocalSkillInput,
) -> crate::Result<SkillRecord> {
    let slug: String = input
        .name
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if slug.is_empty() {
        return Err(CoreError::Internal(
            "skill name produces an empty slug after sanitization".into(),
        ));
    }
    let id = format!("local-{slug}");
    let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let engines = input.engines.unwrap_or_default();
    let tags = input.tags.unwrap_or_default();
    let engines_json = serde_json::to_string(&engines)?;
    let tags_json = serde_json::to_string(&tags)?;
    let skill_type = input.r#type.unwrap_or_else(|| "skill".into());
    let description = input.description.unwrap_or_default();

    let base_dir = crate::skill_fs::skills_base_dir()
        .map_err(CoreError::Internal)?
        .join("local");
    let skill_dir = base_dir.join(&slug);

    // Ensure base_dir exists BEFORE canonicalize so both sides end up with
    // the same prefix form (on Windows, `canonicalize()` returns `\\?\C:\...`
    // when the dir exists, but falls back to the relative form when it
    // doesn't — the asymmetry caused `starts_with` to spuriously fail).
    std::fs::create_dir_all(&base_dir)
        .map_err(|e| CoreError::Internal(format!("failed to create skills base directory: {e}")))?;

    let canonical_base = base_dir
        .canonicalize()
        .map_err(|e| CoreError::Internal(format!("failed to resolve skills base dir: {e}")))?;
    let skill_dir_for_check = canonical_base.join(&slug);
    if !skill_dir_for_check.starts_with(&canonical_base) {
        return Err(CoreError::Internal("invalid skill name".into()));
    }

    let mut skill_md = String::new();
    skill_md.push_str("---\n");
    skill_md.push_str(&format!("type: {}\n", &skill_type));
    if !engines.is_empty() {
        skill_md.push_str(&format!("engines: [{}]\n", engines.join(", ")));
    }
    if !tags.is_empty() {
        skill_md.push_str(&format!("tags: [{}]\n", tags.join(", ")));
    }
    if let Some(ref trigger) = input.trigger
        && !trigger.is_empty()
    {
        skill_md.push_str(&format!("trigger: {trigger}\n"));
    }
    skill_md.push_str("---\n\n");
    skill_md.push_str(&format!("# {}\n\n", &input.name));
    if !description.is_empty() {
        skill_md.push_str(&format!("{}\n", &description));
    }
    if let Some(ref content) = input.content
        && !content.is_empty()
    {
        skill_md.push_str(&format!("\n{content}\n"));
    }

    // Friendly duplicate check BEFORE writing SKILL.md / hitting SQLite's
    // UNIQUE constraint. Skills are keyed by `id = local-<slug>`, so two
    // rules with the same sanitized name collide on `skills.id`. Raw sqlx
    // errors ("UNIQUE constraint failed: skills.id (code: 1555)") confuse
    // end users — give them the actionable version.
    let existing_id = sqlx::query_scalar!("SELECT id FROM skills WHERE id = ?1", id)
        .fetch_optional(db)
        .await?;
    if existing_id.is_some() {
        return Err(CoreError::Validation(format!(
            "a rule with id '{id}' already exists. Remove it first with \
             the memory management UI or pick a different name."
        )));
    }

    std::fs::create_dir_all(&skill_dir)
        .map_err(|e| CoreError::Internal(format!("failed to create skill directory: {e}")))?;
    let canonical_skill = skill_dir
        .canonicalize()
        .map_err(|e| CoreError::Internal(format!("failed to resolve skill directory: {e}")))?;
    if !canonical_skill.starts_with(&canonical_base) {
        return Err(CoreError::Internal("invalid skill name".into()));
    }

    std::fs::write(skill_dir.join("SKILL.md"), &skill_md)
        .map_err(|e| CoreError::Internal(format!("failed to write SKILL.md: {e}")))?;

    let insert_result = sqlx::query!(
        "INSERT INTO skills
         (id, name, source, directory, version, description, type, engines, tags,
          trigger, check_prompt, enabled_for_claude, installed_at, updated_at)
         VALUES (?1, ?2, 'local', ?3, '1.0.0', ?4, ?5, ?6, ?7, ?8, ?9, 1, ?10, ?10)",
        id,
        input.name,
        slug,
        description,
        skill_type,
        engines_json,
        tags_json,
        input.trigger,
        input.check_prompt,
        now
    )
    .execute(db)
    .await;
    if let Err(e) = insert_result {
        let _ = std::fs::remove_dir_all(&skill_dir);
        return Err(e.into());
    }

    for engine_name in &engines {
        if let Err(e) = crate::skill_fs::sync_engine_link("local", &slug, engine_name, true) {
            eprintln!("warning: sync_engine_link failed for engine {engine_name}: {e}");
            record_engine_link_failure(db, &id, engine_name, &e).await;
        }
    }

    let row = sqlx::query_as!(
        SkillRow,
        "SELECT id, name, source, directory, version, description, type, \
         engines, tags, trigger, check_prompt, repo_owner, repo_name, repo_branch, readme_url, \
         enabled_for_codex, enabled_for_claude, enabled_for_gemini, enabled_for_cursor, \
         installed_at, updated_at, origin FROM skills WHERE id = ?1",
        id
    )
    .fetch_one(db)
    .await?;
    Ok(SkillRecord::from(row))
}
