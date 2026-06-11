use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::domain::models::{
    AddExampleInput, ListExamplesInput, RemoveExampleInput, RuleExampleRecord, SkillRecord,
    SkillRepoAddInput, SkillRepoRecord, SkillRepoRemoveInput, UpdateConfidenceInput,
};
use crate::error::CoreError;

use super::{SkillRepoRow, SkillRow};

/// Pick the text for the local `skills.description` column. The local schema
/// only stores `description`, so store the full cloud `content` when present:
/// this keeps the desktop hash matching cloud's `/rules/sync` contract
/// (`sha256(rule.content)`) and gives retrieval the richest rule text.
fn effective_description(rule: &crate::cloud::sync::SyncedRule) -> String {
    if !rule.content.trim().is_empty() {
        return rule.content.clone();
    }
    rule.description.clone()
}

/// Derive `confidence_score` for a synced cloud rule from its
/// `cluster-size:N` / `severity:X` tags. Returns `None` when neither signal is
/// present so the caller leaves the column at the DB default (0.7). The mapping
/// lives in `crate::context::rule_source::confidence_from_tags`.
fn effective_confidence(rule: &crate::cloud::sync::SyncedRule) -> Option<f64> {
    let tags_json = serde_json::to_string(&rule.tags).ok()?;
    crate::context::rule_source::confidence_from_tags(&tags_json)
}

pub(crate) fn cloud_rule_directory_name(rule_id: &str) -> String {
    let mut slug = String::with_capacity(rule_id.len().min(96));
    let mut last_dash = false;
    for ch in rule_id.trim().chars() {
        let safe = if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            ch.to_ascii_lowercase()
        } else {
            '-'
        };
        if safe == '-' {
            if !last_dash {
                slug.push('-');
                last_dash = true;
            }
        } else {
            slug.push(safe);
            last_dash = false;
        }
    }
    let slug = slug.trim_matches('-');
    let needs_hash = slug.is_empty()
        || slug != rule_id
        || slug.chars().count() > 96
        || is_windows_reserved_path_name(slug);
    let base = if slug.is_empty() { "rule" } else { slug };
    if !needs_hash {
        return base.to_owned();
    }

    let head: String = base.chars().take(80).collect();
    format!("{head}-{}", short_rule_id_hash(rule_id))
}

fn short_rule_id_hash(rule_id: &str) -> String {
    let digest = Sha256::digest(rule_id.as_bytes());
    let mut hex = String::with_capacity(12);
    for byte in digest.iter().take(6) {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

fn is_windows_reserved_path_name(name: &str) -> bool {
    let lower = name.trim_end_matches('.').to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "con"
            | "prn"
            | "aux"
            | "nul"
            | "com1"
            | "com2"
            | "com3"
            | "com4"
            | "com5"
            | "com6"
            | "com7"
            | "com8"
            | "com9"
            | "lpt1"
            | "lpt2"
            | "lpt3"
            | "lpt4"
            | "lpt5"
            | "lpt6"
            | "lpt7"
            | "lpt8"
            | "lpt9"
    )
}

/// Re-stamp `skills.confidence_score` for every row whose tags carry a
/// `cluster-size:N` / `severity:X` evidence signal but whose score is
/// still at the historic flat default (0.7). Returns the number of
/// rows updated. Tight tolerance (`±0.001`) so user-customized scores
/// like 0.65 / 0.75 are left alone.
pub async fn backfill_skills_confidence_from_tags(db: &sqlx::SqlitePool) -> crate::Result<i64> {
    let rows =
        sqlx::query!("SELECT id, tags, confidence_score FROM skills WHERE status = 'active'")
            .fetch_all(db)
            .await?;

    let mut updated = 0_i64;
    for row in rows {
        let id = row.id;
        let tags = row.tags;
        let current = row.confidence_score;
        if (current - 0.7).abs() > 0.001 {
            continue;
        }
        let Some(new_score) = crate::context::rule_source::confidence_from_tags(&tags) else {
            continue;
        };
        if (new_score - current).abs() < 0.001 {
            continue;
        }
        let _ = sqlx::query!(
            "UPDATE skills SET confidence_score = ?1 WHERE id = ?2",
            new_score,
            id
        )
        .execute(db)
        .await;
        updated += 1;
    }
    Ok(updated)
}

/// Enable every synced cloud rule for all agents. Cloud review memory is
/// agent-agnostic: once a team rule is synced, Codex/Cursor/Gemini should get
/// the same protection Claude gets.
pub async fn backfill_cloud_rules_enabled_for_all_agents(
    db: &sqlx::SqlitePool,
) -> crate::Result<u64> {
    let result = sqlx::query!(
        "UPDATE skills \
         SET enabled_for_codex = 1, \
             enabled_for_claude = 1, \
             enabled_for_gemini = 1, \
             enabled_for_cursor = 1, \
             updated_at = datetime('now') \
         WHERE source = 'cloud' \
           AND status = 'active' \
           AND (enabled_for_codex = 0 \
                OR enabled_for_claude = 0 \
                OR enabled_for_gemini = 0 \
                OR enabled_for_cursor = 0)",
    )
    .execute(db)
    .await?;
    Ok(result.rows_affected())
}

async fn apply_cloud_source_repo(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    skill_id: &str,
    incoming_repo: Option<&str>,
) -> crate::Result<()> {
    let Some(incoming_repo) = incoming_repo.map(str::trim).filter(|repo| !repo.is_empty()) else {
        return Ok(());
    };

    let updated = sqlx::query(
        "UPDATE skills
         SET source_repo = ?1
         WHERE id = ?2
           AND source = 'cloud'
           AND (source_repo IS NULL OR trim(source_repo) = '' OR source_repo = ?1)",
    )
    .bind(incoming_repo)
    .bind(skill_id)
    .execute(&mut **tx)
    .await?;
    if updated.rows_affected() > 0 {
        return Ok(());
    }

    let existing = sqlx::query_as::<_, (Option<String>, String)>(
        "SELECT source_repo, source FROM skills WHERE id = ?1",
    )
    .bind(skill_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((Some(existing_repo), source)) = existing else {
        return Ok(());
    };
    if source != "cloud" || existing_repo.trim().is_empty() || existing_repo == incoming_repo {
        return Ok(());
    }

    let event_id = format!("rule-event-{}", Uuid::new_v4());
    let reason = format!(
        "cloud sync kept existing source_repo '{existing_repo}' and ignored incoming '{incoming_repo}'"
    );
    let metadata = serde_json::json!({
        "existingSourceRepo": existing_repo,
        "incomingSourceRepo": incoming_repo,
    })
    .to_string();
    sqlx::query(
        "INSERT INTO rule_events
         (id, skill_id, kind, source, reason, metadata)
         VALUES (?1, ?2, 'source_repo_conflict', 'cloud_sync', ?3, ?4)",
    )
    .bind(event_id)
    .bind(skill_id)
    .bind(reason)
    .bind(metadata)
    .execute(&mut **tx)
    .await?;

    Ok(())
}

async fn refresh_rule_index_after_sync(db: &sqlx::SqlitePool) {
    let project_hash =
        crate::infra::db::project_hash_from_root(&crate::infra::db::current_project_root());
    let index_pool = match crate::context::index_db::get_pool_for_project(&project_hash).await {
        Ok(pool) => pool,
        Err(e) => {
            if crate::infra::env::debug_cloud() {
                eprintln!("[difflore] cloud sync rule-index refresh skipped: {e}");
            }
            return;
        }
    };
    if let Err(e) = crate::context::orchestrator::ensure_rules_indexed(db, &index_pool).await {
        if crate::infra::env::debug_cloud() {
            eprintln!("[difflore] cloud sync rule-index refresh failed: {e}");
        }
    }
}

pub async fn apply_sync_result(
    db: &sqlx::SqlitePool,
    result: &crate::cloud::sync::SyncResult,
) -> crate::Result<()> {
    let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let sync_changed =
        !result.created.is_empty() || !result.updated.is_empty() || !result.deleted.is_empty();
    let mut tx = db.begin().await?;

    for rule in &result.created {
        let engines_json = serde_json::to_string(&rule.engines)?;
        let tags_json = serde_json::to_string(&rule.tags)?;
        let directory = cloud_rule_directory_name(&rule.id);
        let file_patterns_json = if rule.file_patterns.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&rule.file_patterns)?)
        };
        let description = effective_description(rule);
        // Fall back to "cloud" so audit pages distinguish remotely-fetched
        // rules from locally-typed ones (the migration default of `manual`
        // would mislabel them).
        let origin = rule.origin.clone().unwrap_or_else(|| "cloud".to_owned());
        let directory_param = directory.as_str();
        sqlx::query(
            "INSERT INTO skills
             (id, name, source, directory, version, description, type, engines, tags,
              trigger, check_prompt, file_patterns,
              enabled_for_codex, enabled_for_claude, enabled_for_gemini, enabled_for_cursor,
              installed_at, updated_at, origin)
             VALUES (?1, ?2, 'cloud', ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11,
                     1, 1, 1, 1, ?12, ?12, ?13)
             ON CONFLICT(id) DO UPDATE SET
                name = excluded.name,
                directory = excluded.directory,
                version = excluded.version,
                description = excluded.description,
                type = excluded.type,
                engines = excluded.engines,
                tags = excluded.tags,
                trigger = excluded.trigger,
                check_prompt = excluded.check_prompt,
                file_patterns = excluded.file_patterns,
                updated_at = excluded.updated_at,
                origin = excluded.origin,
                status = 'active'
             WHERE skills.source = 'cloud'",
        )
        .bind(&rule.id)
        .bind(&rule.name)
        .bind(directory_param)
        .bind(&rule.version)
        .bind(&description)
        .bind(&rule.r#type)
        .bind(&engines_json)
        .bind(&tags_json)
        .bind(rule.trigger.as_deref())
        .bind(rule.check_prompt.as_deref())
        .bind(file_patterns_json.as_deref())
        .bind(&now)
        .bind(&origin)
        .execute(&mut *tx)
        .await?;
        apply_cloud_source_repo(&mut tx, &rule.id, rule.source_repo.as_deref()).await?;
        // Stamp confidence_score from cluster-size/severity tags only when the
        // row is still at the DB default (0.7 ± 0.001), so user-customized
        // scores like 0.1 (rejected) or 0.85 (boosted) survive resync.
        if let Some(conf) = effective_confidence(rule) {
            let _ = sqlx::query!(
                "UPDATE skills SET confidence_score = ?1 \
                 WHERE id = ?2 AND ABS(confidence_score - 0.7) < 0.001",
                conf,
                rule.id
            )
            .execute(&mut *tx)
            .await;
        }
    }

    for rule in &result.updated {
        let engines_json = serde_json::to_string(&rule.engines)?;
        let tags_json = serde_json::to_string(&rule.tags)?;
        let directory = cloud_rule_directory_name(&rule.id);
        let file_patterns_json = if rule.file_patterns.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&rule.file_patterns)?)
        };
        let description = effective_description(rule);
        sqlx::query(
            "UPDATE skills SET name = ?1, description = ?2, type = ?3, version = ?4,
             engines = ?5, tags = ?6, trigger = ?7, check_prompt = ?8, file_patterns = ?9,
             directory = ?10,
             updated_at = ?11,
             origin = COALESCE(?12, origin),
             status = 'active'
             WHERE id = ?13 AND source = 'cloud'",
        )
        .bind(&rule.name)
        .bind(&description)
        .bind(&rule.r#type)
        .bind(&rule.version)
        .bind(&engines_json)
        .bind(&tags_json)
        .bind(rule.trigger.as_deref())
        .bind(rule.check_prompt.as_deref())
        .bind(file_patterns_json.as_deref())
        .bind(&directory)
        .bind(&now)
        .bind(rule.origin.as_deref())
        .bind(&rule.id)
        .execute(&mut *tx)
        .await?;
        apply_cloud_source_repo(&mut tx, &rule.id, rule.source_repo.as_deref()).await?;
        // Same default-only guard as the INSERT path: don't clobber
        // user-customized scores like 0.1 (rejected) or 0.85 (boosted)
        // when re-syncing rules.
        if let Some(conf) = effective_confidence(rule) {
            let _ = sqlx::query!(
                "UPDATE skills SET confidence_score = ?1 \
                 WHERE id = ?2 AND ABS(confidence_score - 0.7) < 0.001",
                conf,
                rule.id
            )
            .execute(&mut *tx)
            .await;
        }
    }

    for id in &result.deleted {
        sqlx::query("DELETE FROM skills WHERE id = ?1 AND source = 'cloud'")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }

    tx.commit().await?;

    // Self-heal: re-stamp confidence_score on rows still at the 0.7 default but
    // with cluster-size/severity tags. Idempotent; runs once per sync so
    // already-synced corpora get the new ranking weights without waiting for a
    // cloud-side updated_at bump.
    let _ = backfill_skills_confidence_from_tags(db).await;
    if sync_changed {
        refresh_rule_index_after_sync(db).await;
    }

    Ok(())
}

/// Metadata explaining why a rule surfaced in recall/search:
/// `file_patterns` plus canonical `source_repo`.
#[derive(Debug, Clone, Default)]
pub struct SearchSkillMeta {
    pub file_patterns: Vec<String>,
    pub source_repo: Option<String>,
}

pub async fn fetch_search_meta(
    pool: &sqlx::SqlitePool,
    ids: &[String],
) -> std::collections::HashMap<String, SearchSkillMeta> {
    let mut out = std::collections::HashMap::new();
    if ids.is_empty() {
        return out;
    }
    let Ok(ids_json) = serde_json::to_string(ids) else {
        return out;
    };
    let Ok(rows) = sqlx::query_as::<_, (String, Option<String>, Option<String>)>(
        "SELECT id, file_patterns, source_repo
           FROM skills WHERE id IN (SELECT value FROM json_each(?1))",
    )
    .bind(ids_json)
    .fetch_all(pool)
    .await
    else {
        return out;
    };
    for (id, file_patterns_raw, source_repo) in rows {
        let file_patterns: Vec<String> = file_patterns_raw
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default();
        out.insert(
            id,
            SearchSkillMeta {
                file_patterns,
                source_repo,
            },
        );
    }
    out
}

pub async fn list_review_standards(db: &sqlx::SqlitePool) -> crate::Result<Vec<SkillRecord>> {
    let rows = sqlx::query_as!(
        SkillRow,
        "SELECT id, name, source, directory, version, description, type, \
         engines, tags, trigger, check_prompt, repo_owner, repo_name, repo_branch, readme_url, \
         enabled_for_codex, enabled_for_claude, enabled_for_gemini, enabled_for_cursor, \
         installed_at, updated_at, origin FROM skills \
         WHERE type = 'review_standard' AND status = 'active' \
         ORDER BY installed_at DESC",
    )
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(SkillRecord::from).collect())
}

/// List skills across all `type`s but active status only; pending candidates
/// and soft-deleted rows are filtered out in SQL. Several callers rely on this
/// active-only filter and pair it with their own pending checks, so audit them
/// if you change the WHERE clause.
pub async fn list_all_skills(db: &sqlx::SqlitePool) -> crate::Result<Vec<SkillRecord>> {
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

pub async fn export_rules_markdown(db: &sqlx::SqlitePool) -> crate::Result<String> {
    let skills = list_all_skills(db).await?;
    if skills.is_empty() {
        return Ok("# Project Rules\n\n_No rules found._\n".to_owned());
    }

    let skill_ids: Vec<String> = skills.iter().map(|s| s.id.clone()).collect();
    let examples_map =
        crate::context::rule_source::load_rule_examples_batch(db, &skill_ids).await?;

    let mut md = String::from("# Project Rules\n");

    for (i, skill) in skills.iter().enumerate() {
        md.push_str(&format!("\n## {}\n\n", skill.name));
        md.push_str(&format!("{}\n", skill.description));

        if let Some(cp) = &skill.check_prompt
            && !cp.is_empty()
        {
            md.push_str(&format!("\n**Check prompt:** {cp}\n"));
        }

        if let Some(examples) = examples_map.get(&skill.id)
            && !examples.is_empty()
        {
            md.push_str("\n### Examples\n");
            for ex in examples {
                md.push_str("\n❌ Bad:\n```\n");
                md.push_str(&ex.bad_code);
                md.push_str("\n```\n\n✅ Good:\n```\n");
                md.push_str(&ex.good_code);
                md.push_str("\n```\n");
                if let Some(desc) = &ex.description
                    && !desc.is_empty()
                {
                    md.push_str(&format!("\n{desc}\n"));
                }
            }
        }

        if i < skills.len() - 1 {
            md.push_str("\n---\n");
        }
    }

    Ok(md)
}

pub async fn repos_list(db: &sqlx::SqlitePool) -> crate::Result<Vec<SkillRepoRecord>> {
    let rows = sqlx::query_as!(SkillRepoRow,
        "SELECT id, owner, name, branch, enabled, created_at FROM skill_repos ORDER BY created_at DESC"
    )
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(SkillRepoRecord::from).collect())
}

pub async fn repos_add(
    db: &sqlx::SqlitePool,
    input: SkillRepoAddInput,
) -> crate::Result<SkillRepoRecord> {
    let id = format!("repo-{}-{}", Uuid::new_v4(), input.name);
    let branch = input.branch.unwrap_or_else(|| "main".into());
    let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();

    sqlx::query!(
        "INSERT INTO skill_repos (id, owner, name, branch, enabled, created_at) VALUES (?1, ?2, ?3, ?4, 1, ?5)",
        id, input.owner, input.name, branch, now
    )
    .execute(db)
    .await?;

    Ok(SkillRepoRecord {
        id,
        owner: input.owner,
        name: input.name,
        branch,
        enabled: true,
        created_at: now,
    })
}

pub async fn repos_remove(db: &sqlx::SqlitePool, input: SkillRepoRemoveInput) -> crate::Result<()> {
    let result = sqlx::query!("DELETE FROM skill_repos WHERE id = ?1", input.id)
        .execute(db)
        .await?;
    if result.rows_affected() == 0 {
        return Err(CoreError::NotFound(format!(
            "skill repo '{}' not found.",
            input.id
        )));
    }
    Ok(())
}

/// Return value of `update_confidence` so the caller can render a before/after
/// message ("rule X: 0.65 → 0.70").
#[derive(Debug, Clone)]
pub struct ConfidenceChange {
    pub before: f64,
    pub after: f64,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfidenceSignal {
    Accept,
    Reject,
}

impl ConfidenceSignal {
    const ALLOWED: &'static [&'static str] = &["accept", "reject"];

    fn parse(raw: &str) -> crate::Result<Self> {
        match raw {
            "accept" => Ok(Self::Accept),
            "reject" => Ok(Self::Reject),
            _ => Err(CoreError::Validation(format!(
                "signal must be one of: {}",
                Self::ALLOWED.join(", ")
            ))),
        }
    }

    const fn delta(self) -> f64 {
        match self {
            Self::Accept => 0.05,
            Self::Reject => -0.1,
        }
    }

    const fn event_kind(self) -> &'static str {
        match self {
            Self::Accept => "feedback_accept",
            Self::Reject => "feedback_dismiss",
        }
    }
}

pub async fn update_confidence(
    db: &sqlx::SqlitePool,
    input: UpdateConfidenceInput,
) -> crate::Result<ConfidenceChange> {
    let signal = ConfidenceSignal::parse(input.signal.as_str())?;
    let delta = signal.delta();

    let mut tx = db.begin().await?;
    let existing = sqlx::query!(
        "SELECT confidence_score, name FROM skills WHERE id = ?1",
        input.skill_id
    )
    .fetch_optional(&mut *tx)
    .await?;
    let row = existing.ok_or_else(|| {
        CoreError::NotFound(format!(
            "rule '{}' not found; cannot apply {} feedback. Run `difflore status --json` to inspect current local memory ids.",
            input.skill_id, input.signal
        ))
    })?;
    let before = row.confidence_score;
    let name = row.name;
    let after = (before + delta).clamp(0.0, 1.0);

    sqlx::query!(
        "UPDATE skills SET confidence_score = ?1 WHERE id = ?2",
        after,
        input.skill_id
    )
    .execute(&mut *tx)
    .await?;

    let event_id = format!("rule-event-{}", Uuid::new_v4());
    let kind = signal.event_kind();
    let metadata = serde_json::json!({
        "signal": input.signal,
        "delta": delta,
    })
    .to_string();
    sqlx::query!(
        "INSERT INTO rule_events
         (id, skill_id, kind, source, confidence_before, confidence_after, reason, metadata)
         VALUES (?1, ?2, ?3, 'local_feedback', ?4, ?5, NULL, ?6)",
        event_id,
        input.skill_id,
        kind,
        before,
        after,
        metadata
    )
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(ConfidenceChange {
        before,
        after,
        name,
    })
}

pub async fn add_example(
    db: &sqlx::SqlitePool,
    input: AddExampleInput,
) -> crate::Result<RuleExampleRecord> {
    let id = format!("example-{}", Uuid::new_v4());
    let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let source = input.source.unwrap_or_else(|| "manual".into());

    sqlx::query!(
        "INSERT INTO rule_examples (id, skill_id, bad_code, good_code, description, source, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        id,
        input.skill_id,
        input.bad_code,
        input.good_code,
        input.description,
        source,
        now
    )
    .execute(db)
    .await?;

    Ok(RuleExampleRecord {
        id,
        skill_id: input.skill_id,
        bad_code: input.bad_code,
        good_code: input.good_code,
        description: input.description,
        source,
        created_at: now,
    })
}

pub async fn list_examples(
    db: &sqlx::SqlitePool,
    input: ListExamplesInput,
) -> crate::Result<Vec<RuleExampleRecord>> {
    let rows = sqlx::query_as!(
        ExampleRow,
        "SELECT id, skill_id, bad_code, good_code, description, source, created_at FROM rule_examples WHERE skill_id = ?1 ORDER BY created_at DESC",
        input.skill_id
    )
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| RuleExampleRecord {
            id: r.id,
            skill_id: r.skill_id,
            bad_code: r.bad_code,
            good_code: r.good_code,
            description: r.description,
            source: r.source,
            created_at: r.created_at,
        })
        .collect())
}

pub async fn remove_example(db: &sqlx::SqlitePool, input: RemoveExampleInput) -> crate::Result<()> {
    let result = sqlx::query!("DELETE FROM rule_examples WHERE id = ?1", input.id)
        .execute(db)
        .await?;
    // SQLite's DELETE silently succeeds with 0 rows affected when the id
    // doesn't exist. Surface that as NotFound so the CLI can tell the
    // user their id was wrong instead of claiming a phantom success.
    if result.rows_affected() == 0 {
        return Err(CoreError::NotFound(format!(
            "example '{}' not found. Run `difflore status --json` to inspect current local memory ids.",
            input.id
        )));
    }
    Ok(())
}

#[derive(sqlx::FromRow)]
struct ExampleRow {
    id: String,
    skill_id: String,
    bad_code: String,
    good_code: String,
    description: Option<String>,
    source: String,
    created_at: String,
}
