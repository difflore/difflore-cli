use openapi_contract::api;
use uuid::Uuid;

use crate::cloud::api_types::RuleDetail;
use crate::cloud::client::CloudClient;
use crate::errors::CoreError;

use super::types::LocalRuleUploadRow;

/// True when `s` parses as a canonical UUID. Local rules created via
/// `remember_rule` (id `conv-{slug}-{8hex}`) and `rules add` (id
/// `local-{slug}`) fail this check; cloud-synced rules pass.
pub(super) fn looks_like_cloud_uuid(s: &str) -> bool {
    Uuid::parse_str(s).is_ok()
}

pub(super) fn rule_cloud_mapping_key(local_id: &str) -> String {
    format!("rule_cloud_id:{local_id}")
}

fn validate_cloud_rule_id(source: &str, value: Option<String>) -> crate::Result<Option<String>> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.trim().is_empty() {
        return Ok(None);
    }
    if looks_like_cloud_uuid(&value) {
        return Ok(Some(value));
    }
    Err(CoreError::Internal(format!(
        "{source} contains non-UUID cloud rule id `{value}`"
    )))
}

async fn lookup_skills_cloud_id(
    pool: &sqlx::SqlitePool,
    local_id: &str,
) -> crate::Result<Option<String>> {
    let cloud_id: Option<String> =
        sqlx::query_scalar!("SELECT cloud_id FROM skills WHERE id = ?1", local_id)
            .fetch_optional(pool)
            .await
            .map_err(|e| CoreError::Internal(format!("read skills.cloud_id for {local_id}: {e}")))?
            .flatten();
    validate_cloud_rule_id("skills.cloud_id", cloud_id)
}

async fn lookup_remembered_cloud_rule_id(
    pool: &sqlx::SqlitePool,
    local_id: &str,
) -> crate::Result<Option<String>> {
    let key = rule_cloud_mapping_key(local_id);
    let cloud_id: Option<String> =
        sqlx::query_scalar!("SELECT value FROM auth WHERE key = ?1", key)
            .fetch_optional(pool)
            .await
            .map_err(|e| {
                CoreError::Internal(format!("read rule cloud id mapping for {local_id}: {e}"))
            })?;
    validate_cloud_rule_id("auth rule cloud id mapping", cloud_id)
}

async fn lookup_existing_cloud_rule_id(
    pool: &sqlx::SqlitePool,
    local_id: &str,
) -> crate::Result<Option<String>> {
    if looks_like_cloud_uuid(local_id) {
        return Ok(Some(local_id.to_owned()));
    }
    if let Some(mapped) = lookup_remembered_cloud_rule_id(pool, local_id).await? {
        return Ok(Some(mapped));
    }
    lookup_skills_cloud_id(pool, local_id).await
}

pub(super) async fn resolve_existing_cloud_rule_id(
    pool: &sqlx::SqlitePool,
    rule_id: &str,
) -> crate::Result<Option<String>> {
    lookup_existing_cloud_rule_id(pool, rule_id).await
}

async fn remember_cloud_rule_id(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    local_id: &str,
    cloud_id: &str,
) -> crate::Result<()> {
    let mapping_key = rule_cloud_mapping_key(local_id);
    sqlx::query!(
        "INSERT INTO auth (key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        mapping_key,
        cloud_id
    )
    .execute(&mut **tx)
    .await
    .map_err(|e| CoreError::Internal(format!("remember cloud id mapping: {e}")))?;

    sqlx::query!(
        "UPDATE skills SET cloud_id = ?1 WHERE id = ?2",
        cloud_id,
        local_id
    )
    .execute(&mut **tx)
    .await
    .map_err(|e| CoreError::Internal(format!("update skills.cloud_id: {e}")))?;

    Ok(())
}

pub(super) async fn resolve_cloud_rule_id_for_unpublish(
    pool: &sqlx::SqlitePool,
    rule_id: &str,
) -> crate::Result<String> {
    if let Some(cloud_id) = resolve_existing_cloud_rule_id(pool, rule_id).await? {
        return Ok(cloud_id);
    }
    Err(CoreError::NotFound(format!(
        "cloud id mapping for rule {rule_id}; sync or publish it from DiffLore Cloud before unpublishing"
    )))
}

pub(super) fn build_rule_create_body(row: &LocalRuleUploadRow) -> serde_json::Value {
    // Cloud requires `content >= 1 char`. Conversation rules store the
    // full body in `description` already; deliberate `rules add` rules
    // may have an empty description, so fall back to the rule name
    // (always non-empty by validation).
    let content = if row.description.trim().is_empty() {
        row.name.clone()
    } else {
        row.description.clone()
    };
    let engines: Vec<String> = serde_json::from_str(&row.engines_json).unwrap_or_default();
    let tags: Vec<String> = serde_json::from_str(&row.tags_json).unwrap_or_default();
    let file_patterns: Vec<String> = row
        .file_patterns_json
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default();

    // Cloud's `type` enum is `'skill' | 'review_standard'`; our local
    // rules use either, so pass through as-is.
    serde_json::json!({
        "name": row.name,
        "type": row.rule_type,
        "description": row.description,
        "content": content,
        "version": row.version,
        "engines": engines,
        "tags": tags,
        "trigger": row.trigger,
        "checkPrompt": row.check_prompt,
        // This path is called only as part of an explicit team publish.
        // Mark the cloud copy team-visible up front so the following
        // `/rules/team/publish` promotion can attach it to the team.
        "visibility": "team",
        "filePatterns": file_patterns,
        "origin": row.origin,
        "sourceRepo": row.source_repo,
    })
}

/// Bridge a local-only rule into the cloud's UUID-keyed `rules_cloud`
/// table so it can subsequently be promoted to a team. The cloud's
/// `/rules/team/publish` endpoint only knows how to *promote* an
/// existing-on-cloud rule — it can't ingest a new rule body. So when the
/// caller hands us a slug-form local id, we:
///   1. POST `/rules` with the local row's fields → cloud assigns a UUID
///   2. Rewrite the local row's id (and child `rule_examples.skill_id`)
///      to the new UUID inside a single transaction with deferred FK
///      checks so a crash mid-rewrite leaves the DB consistent
///   3. Return the UUID to the caller for the subsequent publish call
///
/// UUID-form ids pass through unchanged — useful both for cloud-synced
/// rules being re-promoted and for hypothetical future code paths that
/// pre-mint UUIDs locally.
pub(super) async fn ensure_cloud_rule_id(
    pool: &sqlx::SqlitePool,
    client: &CloudClient,
    local_id: &str,
) -> crate::Result<String> {
    if looks_like_cloud_uuid(local_id) {
        return Ok(local_id.to_owned());
    }
    if let Some(existing) = lookup_existing_cloud_rule_id(pool, local_id).await? {
        return Ok(existing);
    }

    let row: Option<LocalRuleUploadRow> = sqlx::query_as::<_, LocalRuleUploadRow>(
        r"SELECT name, type as rule_type, description, version,
           engines as engines_json, tags as tags_json, trigger, check_prompt,
           file_patterns as file_patterns_json, origin, source_repo
           FROM skills WHERE id = ?1",
    )
    .bind(local_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| CoreError::Internal(format!("read local rule {local_id}: {e}")))?;

    let row = row.ok_or_else(|| CoreError::NotFound(format!("rule {local_id}")))?;
    let body = build_rule_create_body(&row);

    let created_json: serde_json::Value = api!(POST "/rules", body = &body).fetch(client).await?;
    let created: RuleDetail = serde_json::from_value(created_json)?;
    let new_id = created.id;
    if !looks_like_cloud_uuid(&new_id) {
        return Err(CoreError::Internal(format!(
            "cloud returned non-UUID rule id `{new_id}` from POST /rules"
        )));
    }

    // Migrate the local row to the cloud-assigned UUID inside a single
    // transaction. Defer FK checks until commit so we can rewrite the
    // child `rule_examples.skill_id` and the parent `skills.id` in
    // either order without tripping the FK constraint mid-transaction.
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| CoreError::Internal(format!("begin tx: {e}")))?;
    sqlx::query!("PRAGMA defer_foreign_keys = ON")
        .execute(&mut *tx)
        .await
        .map_err(|e| CoreError::Internal(format!("defer FKs: {e}")))?;
    remember_cloud_rule_id(&mut tx, local_id, &new_id).await?;
    sqlx::query!(
        "UPDATE rule_examples SET skill_id = ?1 WHERE skill_id = ?2",
        new_id,
        local_id
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| CoreError::Internal(format!("update rule_examples: {e}")))?;
    sqlx::query!("UPDATE skills SET id = ?1 WHERE id = ?2", new_id, local_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| CoreError::Internal(format!("update skills.id: {e}")))?;
    tx.commit()
        .await
        .map_err(|e| CoreError::Internal(format!("commit id rewrite: {e}")))?;

    Ok(new_id)
}
