use crate::domain::models::SkillRecord;
use crate::error::CoreError;
use uuid::Uuid;

use super::fetch_skill_row_by_id;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CandidateSourceProof {
    pub source: Option<String>,
    pub comment_url: Option<String>,
    pub file: Option<String>,
    pub excerpt: Option<String>,
}

impl CandidateSourceProof {
    pub const fn has_any(&self) -> bool {
        self.source.is_some()
            || self.comment_url.is_some()
            || self.file.is_some()
            || self.excerpt.is_some()
    }
}

/// One row in the local candidate queue. Like `SkillRecord` minus the
/// engine flags, plus the ingest-time provenance surfaced in the UI.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CandidateRule {
    pub id: String,
    pub name: String,
    pub description: String,
    pub origin: String,
    pub installed_at: String,
    pub content_hash: Option<String>,
    pub source_repo: Option<String>,
    pub file_patterns: Vec<String>,
    pub drafted_rule: Option<String>,
    pub source_proof: Option<CandidateSourceProof>,
}

#[derive(sqlx::FromRow)]
struct CandidateRuleRow {
    id: String,
    name: String,
    description: String,
    origin: String,
    installed_at: String,
    content_hash: Option<String>,
    source_repo: Option<String>,
    file_patterns: Option<String>,
}

impl crate::domain::rule_view::RuleView for CandidateRule {
    fn id(&self) -> &str {
        &self.id
    }
    fn content(&self) -> &str {
        self.drafted_rule.as_deref().unwrap_or(&self.description)
    }
    fn origin(&self) -> &str {
        &self.origin
    }
    fn confidence(&self) -> Option<f64> {
        None
    }
}

impl From<CandidateRuleRow> for CandidateRule {
    fn from(row: CandidateRuleRow) -> Self {
        let file_patterns = parse_candidate_file_patterns(row.file_patterns.as_deref());
        let drafted_rule = parse_candidate_drafted_rule(&row.description);
        let source_proof = parse_candidate_source_proof(&row.description);
        Self {
            id: row.id,
            name: row.name,
            description: row.description,
            origin: row.origin,
            installed_at: row.installed_at,
            content_hash: row.content_hash,
            source_repo: row.source_repo,
            file_patterns,
            drafted_rule,
            source_proof,
        }
    }
}

fn candidate_actionability_rank(file_patterns: Option<&str>) -> u8 {
    let patterns = parse_candidate_file_patterns(file_patterns);
    if patterns.is_empty() {
        return 2;
    }
    u8::from(!patterns.iter().any(|pattern| {
        let lower = pattern.to_ascii_lowercase();
        lower.contains(".github/")
            || lower.contains("go.mod")
            || lower.contains("go.sum")
            || lower.contains("cargo.toml")
            || lower.contains("cargo.lock")
            || lower.contains("package.json")
            || lower.contains("package-lock.json")
            || lower.contains("pnpm-lock.yaml")
            || lower.contains("yarn.lock")
            || lower.contains("dockerfile")
    }))
}

/// List pending candidates, high-leverage file-scoped work first.
///
/// `repo` filters (at the SQL layer) to rows whose `source_repo` matches
/// the `owner/repo` slug. `limit` caps the result in Rust afterwards;
/// `None` means no cap.
pub async fn list_candidates(
    db: &sqlx::SqlitePool,
    repo: Option<&str>,
    limit: Option<usize>,
) -> crate::Result<Vec<CandidateRule>> {
    // Two static SQL variants: sqlx macros need a literal SQL string, so
    // the conditional repo filter is branched here.
    let mut rows: Vec<CandidateRuleRow> = if let Some(r) = repo {
        sqlx::query_as(
            "SELECT id, name, description, origin, installed_at, content_hash, source_repo, file_patterns FROM skills \
             WHERE status = 'pending' \
             AND lower(source_repo) = lower(?1) \
             ORDER BY installed_at DESC",
        )
        .bind(r)
        .fetch_all(db)
        .await?
    } else {
        sqlx::query_as(
            "SELECT id, name, description, origin, installed_at, content_hash, source_repo, file_patterns FROM skills \
             WHERE status = 'pending' ORDER BY installed_at DESC",
        )
        .fetch_all(db)
        .await?
    };
    rows.sort_by(|a, b| {
        candidate_actionability_rank(a.file_patterns.as_deref())
            .cmp(&candidate_actionability_rank(b.file_patterns.as_deref()))
            .then_with(|| b.installed_at.cmp(&a.installed_at))
            .then_with(|| a.id.cmp(&b.id))
    });
    let mut out: Vec<CandidateRule> = rows.into_iter().map(Into::into).collect();
    if let Some(cap) = limit {
        out.truncate(cap);
    }
    Ok(out)
}

/// Count pending candidates, optionally filtered to a repo. Cheaper than
/// `list_candidates(...).len()` when only the total is needed.
pub async fn count_pending_candidates(
    db: &sqlx::SqlitePool,
    repo: Option<&str>,
) -> crate::Result<u64> {
    let count: i64 = if let Some(r) = repo {
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM skills \
             WHERE status = 'pending' \
             AND lower(source_repo) = lower(?1)",
        )
        .bind(r)
        .fetch_one(db)
        .await?
    } else {
        sqlx::query_scalar!("SELECT COUNT(*) FROM skills WHERE status = 'pending'")
            .fetch_one(db)
            .await?
    };
    Ok(u64::try_from(count.max(0)).unwrap_or(0))
}

pub async fn list_candidate_ids(db: &sqlx::SqlitePool) -> crate::Result<Vec<String>> {
    let ids = sqlx::query_scalar!("SELECT id FROM skills WHERE status = 'pending'")
        .fetch_all(db)
        .await?;
    Ok(ids)
}

pub async fn rule_status(db: &sqlx::SqlitePool, id: &str) -> crate::Result<Option<String>> {
    let status = sqlx::query_scalar!("SELECT status FROM skills WHERE id = ?1", id)
        .fetch_optional(db)
        .await?;
    Ok(status)
}

pub async fn promote_candidate(db: &sqlx::SqlitePool, id: &str) -> crate::Result<SkillRecord> {
    let candidate_row: Option<(String, Option<String>)> = sqlx::query_as(
        "SELECT description, content_hash FROM skills WHERE id = ?1 AND status = 'pending'",
    )
    .bind(id)
    .fetch_optional(db)
    .await?;
    let Some((candidate_description, candidate_content_hash)) = candidate_row else {
        let existing = rule_status(db, id).await?;
        return match existing.as_deref() {
            Some("active") => Err(CoreError::Validation(format!(
                "rule '{id}' is already active; nothing to promote. Inspect local memory with `difflore status --json`."
            ))),
            _ => Err(CoreError::NotFound(format!(
                "memory draft '{id}' not found. Run `difflore status` for the next action."
            ))),
        };
    };

    if let Some(hash) = candidate_content_hash
        .as_deref()
        .map(str::trim)
        .filter(|hash| !hash.is_empty())
    {
        let active_duplicate: Option<String> = sqlx::query_scalar(
            "SELECT id FROM skills
             WHERE status = 'active' AND content_hash = ?1
             ORDER BY installed_at ASC, id ASC LIMIT 1",
        )
        .bind(hash)
        .fetch_optional(db)
        .await?;
        if let Some(active_id) = active_duplicate {
            return Err(CoreError::Validation(format!(
                "memory draft '{id}' duplicates active rule '{active_id}'. Inspect both with `difflore memory show` before approving."
            )));
        }
    }

    let source_proof = parse_candidate_source_proof(&candidate_description);
    let mut tx = db.begin().await?;
    let updated = sqlx::query!(
        "UPDATE skills SET status = 'active' WHERE id = ?1 AND status = 'pending'",
        id
    )
    .execute(&mut *tx)
    .await?;
    if updated.rows_affected() == 0 {
        tx.rollback().await?;
        // Disambiguate "already active" from "not found" so the user
        // isn't sent looking on the candidates list for an active rule.
        let existing = rule_status(db, id).await?;
        return match existing.as_deref() {
            Some("active") => Err(CoreError::Validation(format!(
                "rule '{id}' is already active; nothing to promote. Inspect local memory with `difflore status --json`."
            ))),
            _ => Err(CoreError::NotFound(format!(
                "memory draft '{id}' not found. Run `difflore status` for the next action."
            ))),
        };
    }
    if let Some(proof) = source_proof {
        record_candidate_source_proof(&mut tx, id, &proof).await?;
    }
    let skill = fetch_skill_row_by_id(&mut *tx, id).await?;
    tx.commit().await?;
    Ok(skill)
}

pub async fn reject_candidate(db: &sqlx::SqlitePool, id: &str) -> crate::Result<()> {
    let mut tx = db.begin().await?;
    // Read the row's tombstone provenance before deleting it. Runtime-checked
    // (non-macro) query because `content_hash`/`source`/`description` feed the
    // `rejected_signatures` write and the table has no `.sqlx/` cache entry.
    let candidate_row: Option<(Option<String>, String, Option<String>)> = sqlx::query_as(
        "SELECT content_hash, description, source FROM skills \
         WHERE id = ?1 AND status = 'pending'",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;

    let Some((content_hash, description, _source)) = candidate_row else {
        tx.rollback().await?;
        // Disambiguate "doesn't exist" vs "already active" — both hit the
        // not-found branch but the user-facing fix differs.
        let existing = rule_status(db, id).await?;
        return match existing.as_deref() {
            Some("active") => Err(CoreError::Validation(format!(
                "rule '{id}' is already an active rule, not a pending memory draft."
            ))),
            _ => Err(CoreError::NotFound(format!(
                "memory draft '{id}' not found. Run `difflore status` for the next action."
            ))),
        };
    };

    // Tombstone the rejected content so the import pipeline can't resurrect it
    // on the next `difflore import-reviews`. Legacy rows with no content_hash
    // can't be matched against re-derived candidates, so skip the tombstone
    // there and preserve the old delete-only behaviour.
    if let Some(hash) = content_hash
        .as_deref()
        .map(str::trim)
        .filter(|hash| !hash.is_empty())
    {
        let proof = parse_candidate_source_proof(&description);
        let source_repo = proof.as_ref().and_then(|p| p.source.clone());
        let comment_url = proof.as_ref().and_then(|p| p.comment_url.clone());
        sqlx::query(
            "INSERT OR REPLACE INTO rejected_signatures \
             (content_hash, source_repo, comment_url, reason) \
             VALUES (?1, ?2, ?3, ?4)",
        )
        .bind(hash)
        .bind(source_repo)
        .bind(comment_url)
        .bind("rejected via reject_candidate")
        .execute(&mut *tx)
        .await?;
    }

    sqlx::query("DELETE FROM skills WHERE id = ?1 AND status = 'pending'")
        .bind(id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok(())
}

pub fn parse_candidate_source_proof(description: &str) -> Option<CandidateSourceProof> {
    let proof = CandidateSourceProof {
        source: description_field(description, "Source:"),
        comment_url: description_field(description, "Comment:"),
        file: description_field(description, "File:"),
        excerpt: reviewer_excerpt(description),
    };
    proof.has_any().then_some(proof)
}

pub fn parse_candidate_drafted_rule(description: &str) -> Option<String> {
    let after = description_section_after(description, "Rule:")?;
    let drafted = after
        .split_once("Source evidence:")
        .map_or(after, |(drafted, _)| drafted)
        .trim();
    if drafted.is_empty() {
        return None;
    }
    Some(normalize_legacy_path_prefixed_rule(
        &drafted.lines().collect::<Vec<_>>().join(" "),
    ))
}

/// Older imported review rules embedded their file glob directly in the rule
/// text (`When touching <path>, ...`). File patterns are now path hints, so
/// displays and exports should surface the rule obligation without that legacy
/// path prefix.
#[must_use]
pub fn normalize_legacy_path_prefixed_rule(statement: &str) -> String {
    let statement = statement.trim();
    let lower = statement.to_ascii_lowercase();
    let Some(rest) = lower
        .starts_with("when touching ")
        .then(|| &statement["When touching ".len()..])
    else {
        return statement.to_owned();
    };
    let Some(after_scope) = legacy_when_touching_remainder(rest) else {
        return statement.to_owned();
    };
    let stripped = after_scope.trim();
    if stripped.is_empty() {
        return statement.to_owned();
    }
    capitalize_first_ascii(stripped)
}

fn legacy_when_touching_remainder(rest: &str) -> Option<&str> {
    if let Some(rest) = rest.strip_prefix('`') {
        let (scope, after_scope) = rest.split_once('`')?;
        if scope.trim().is_empty() {
            return None;
        }
        return after_scope.trim_start().strip_prefix(',');
    }
    let (scope, after_scope) = rest.split_once(',')?;
    (!scope.trim().is_empty()).then_some(after_scope)
}

fn capitalize_first_ascii(value: &str) -> String {
    let Some(first) = value.chars().next() else {
        return String::new();
    };
    let mut out = String::with_capacity(value.len());
    out.push(if first.is_ascii_lowercase() {
        first.to_ascii_uppercase()
    } else {
        first
    });
    out.push_str(&value[first.len_utf8()..]);
    out
}

fn description_section_after<'a>(description: &'a str, label: &str) -> Option<&'a str> {
    if let Some(rest) = description.trim_start().strip_prefix(label) {
        return Some(rest);
    }
    let needle = format!("\n{label}");
    description
        .split_once(&needle)
        .map(|(_, after)| after.trim_start())
}

fn parse_candidate_file_patterns(raw: Option<&str>) -> Vec<String> {
    let Some(raw) = raw.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<String>>(raw).unwrap_or_default()
}

async fn record_candidate_source_proof(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    skill_id: &str,
    proof: &CandidateSourceProof,
) -> crate::Result<()> {
    let event_id = format!("rule-event-{}", Uuid::new_v4());
    let metadata = serde_json::json!({
        "sourceProof": proof,
    })
    .to_string();
    let reason = source_proof_reason(proof);
    sqlx::query!(
        "INSERT INTO rule_events
         (id, skill_id, kind, source, confidence_before, confidence_after, reason, metadata)
         VALUES (?1, ?2, 'source_proof', 'candidate_promotion', NULL, NULL, ?3, ?4)",
        event_id,
        skill_id,
        reason,
        metadata,
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn source_proof_reason(proof: &CandidateSourceProof) -> String {
    match (
        proof.source.as_deref(),
        proof.comment_url.as_deref(),
        proof.file.as_deref(),
    ) {
        (Some(source), _, Some(file)) => {
            format!("Promoted review-memory candidate from {source} on {file}")
        }
        (Some(source), _, None) => {
            format!("Promoted review-memory candidate from {source}")
        }
        (None, Some(comment_url), Some(file)) => {
            format!("Promoted review-memory candidate from {comment_url} on {file}")
        }
        (None, Some(comment_url), None) => {
            format!("Promoted review-memory candidate from {comment_url}")
        }
        (None, None, Some(file)) => {
            format!("Promoted review-memory candidate for {file}")
        }
        (None, None, None) => "Promoted review-memory candidate with source proof".to_owned(),
    }
}

fn description_field(description: &str, prefix: &str) -> Option<String> {
    description
        .lines()
        .find_map(|line| line.trim().strip_prefix(prefix).map(str::trim))
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn reviewer_excerpt(description: &str) -> Option<String> {
    let excerpt = description
        .split_once("Reviewer said:")
        .map(|(_, body)| body.trim())
        .filter(|body| !body.is_empty())?;
    Some(truncate_chars(excerpt, 500))
}

fn truncate_chars(value: &str, limit: usize) -> String {
    let mut chars = value.chars();
    let truncated: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CandidateRule, candidate_actionability_rank, parse_candidate_drafted_rule,
        parse_candidate_file_patterns,
    };
    use crate::domain::rule_view::RuleView;

    #[test]
    fn candidate_rule_implements_rule_view() {
        let c = CandidateRule {
            id: "c1".into(),
            name: "n".into(),
            description: "desc".into(),
            origin: "agent-memory".into(),
            installed_at: String::new(),
            content_hash: None,
            source_repo: None,
            file_patterns: vec![],
            drafted_rule: None,
            source_proof: None,
        };
        assert_eq!(c.id(), "c1");
        assert_eq!(c.content(), "desc");
        assert_eq!(c.origin(), "agent-memory");
        assert_eq!(c.confidence(), None);

        let c2 = CandidateRule {
            drafted_rule: Some("the drafted body".into()),
            ..c
        };
        assert_eq!(c2.content(), "the drafted body");
    }

    #[test]
    fn drafted_rule_is_extracted_without_source_evidence() {
        let body = "Rule:\nWhen touching `src/**/*.rs`, prefer structured parsing.\n\nSource evidence:\nSource: acme/widgets#7\n\nReviewer said:\nPlease prefer structured parsing.";

        assert_eq!(
            parse_candidate_drafted_rule(body).as_deref(),
            Some("Prefer structured parsing.")
        );
    }

    #[test]
    fn drafted_rule_normalizes_legacy_path_prefix_without_backticks() {
        let body = "Rule:\nWhen touching src/http/handler.rs, never unwrap request payloads.\n\nSource evidence:\nSource: acme/widgets#7";

        assert_eq!(
            parse_candidate_drafted_rule(body).as_deref(),
            Some("Never unwrap request payloads.")
        );
    }

    #[test]
    fn drafted_rule_parser_rejects_retired_label() {
        let body = "Imported from review.\n\nDrafted rule:\nWhen touching `src/**/*.rs`, prefer structured parsing.\n\nSource evidence:\nSource: acme/widgets#7\n\nReviewer said:\nPlease prefer structured parsing.";

        assert_eq!(parse_candidate_drafted_rule(body).as_deref(), None);
    }

    #[test]
    fn candidate_file_patterns_parse_json_list() {
        assert_eq!(
            parse_candidate_file_patterns(Some(r#"["src/**/*.rs","**/go.mod"]"#)),
            vec!["src/**/*.rs".to_owned(), "**/go.mod".to_owned()]
        );
        assert!(parse_candidate_file_patterns(Some("not-json")).is_empty());
    }

    #[test]
    fn candidate_actionability_rank_only_checks_pattern_values() {
        assert_eq!(candidate_actionability_rank(Some(r#"["**/go.mod"]"#)), 0);
        assert_eq!(
            candidate_actionability_rank(Some(
                r#"[{"note":"cargo.toml appears in metadata, not a pattern"}]"#
            )),
            2
        );
    }
}
