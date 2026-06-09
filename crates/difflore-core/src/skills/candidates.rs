use crate::errors::CoreError;
use crate::models::SkillRecord;
use uuid::Uuid;

use super::SkillRow;

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

/// One row in the local candidate queue. Mirrors `SkillRecord` but
/// drops the engine flags that aren't actionable for a pending rule
/// and adds the ingest-time provenance we surface in the UI.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CandidateRule {
    pub id: String,
    pub name: String,
    pub description: String,
    pub origin: String,
    pub installed_at: String,
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
            file_patterns,
            drafted_rule,
            source_proof,
        }
    }
}

fn candidate_actionability_rank(file_patterns: Option<&str>) -> u8 {
    let Some(patterns) = file_patterns
        .map(str::trim)
        .filter(|v| !v.is_empty() && *v != "[]")
    else {
        return 2;
    };
    let lower = patterns.to_ascii_lowercase();
    u8::from(
        !(lower.contains(".github/")
            || lower.contains("go.mod")
            || lower.contains("go.sum")
            || lower.contains("cargo.toml")
            || lower.contains("cargo.lock")
            || lower.contains("package.json")
            || lower.contains("package-lock.json")
            || lower.contains("pnpm-lock.yaml")
            || lower.contains("yarn.lock")
            || lower.contains("dockerfile")),
    )
}

/// List pending candidates, with high-leverage file-scoped work first.
///
/// `repo` filters to rows whose canonical `source_repo` matches the given
/// `owner/repo` slug. `limit` caps the number of returned rows after that
/// filter; `None` means no cap. The filter happens at the SQL layer where it's
/// cheap and the cap is applied in Rust so a missing cap doesn't change the
/// generated query shape.
pub async fn list_candidates(
    db: &sqlx::SqlitePool,
    repo: Option<&str>,
    limit: Option<usize>,
) -> crate::Result<Vec<CandidateRule>> {
    // Two static SQL variants — the repo filter must be conditional, and
    // sqlx macros need a literal SQL string, so we branch at the call site.
    let mut rows: Vec<CandidateRuleRow> = if let Some(r) = repo {
        sqlx::query_as(
            "SELECT id, name, description, origin, installed_at, file_patterns FROM skills \
             WHERE status = 'pending' \
             AND source_repo = ?1 \
             ORDER BY installed_at DESC",
        )
        .bind(r)
        .fetch_all(db)
        .await?
    } else {
        sqlx::query_as!(
            CandidateRuleRow,
            "SELECT id, name, description, origin, installed_at, file_patterns FROM skills \
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
/// `list_candidates(...).len()` when the caller only needs the total
/// (e.g. the "+N more — `--limit 0` to see all" hint in `candidates list`).
pub async fn count_pending_candidates(
    db: &sqlx::SqlitePool,
    repo: Option<&str>,
) -> crate::Result<u64> {
    let count: i64 = if let Some(r) = repo {
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM skills \
             WHERE status = 'pending' \
             AND source_repo = ?1",
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
    let candidate_description = sqlx::query_scalar!(
        "SELECT description FROM skills WHERE id = ?1 AND status = 'pending'",
        id,
    )
    .fetch_optional(db)
    .await?;
    let Some(candidate_description) = candidate_description else {
        let existing = rule_status(db, id).await?;
        return match existing.as_deref() {
            Some("active") => Err(CoreError::Validation(format!(
                "rule '{id}' is already active — nothing to promote. Inspect local memory with `difflore status --json`."
            ))),
            _ => Err(CoreError::NotFound(format!(
                "memory draft '{id}' not found. Run `difflore status` for the next action."
            ))),
        };
    };

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
        // Same disambiguation as `reject_candidate`: tell the user when
        // they're trying to promote something already active so they
        // don't go looking for it on the candidates list.
        let existing = rule_status(db, id).await?;
        return match existing.as_deref() {
            Some("active") => Err(CoreError::Validation(format!(
                "rule '{id}' is already active — nothing to promote. Inspect local memory with `difflore status --json`."
            ))),
            _ => Err(CoreError::NotFound(format!(
                "memory draft '{id}' not found. Run `difflore status` for the next action."
            ))),
        };
    }
    if let Some(proof) = source_proof {
        record_candidate_source_proof(&mut tx, id, &proof).await?;
    }
    let row = sqlx::query_as!(
        SkillRow,
        "SELECT id, name, source, directory, version, description, type, \
         engines, tags, trigger, check_prompt, repo_owner, repo_name, repo_branch, readme_url, \
         enabled_for_codex, enabled_for_claude, enabled_for_gemini, enabled_for_cursor, \
         installed_at, updated_at, origin FROM skills WHERE id = ?1",
        id
    )
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(SkillRecord::from(row))
}

pub async fn reject_candidate(db: &sqlx::SqlitePool, id: &str) -> crate::Result<()> {
    let result = sqlx::query!(
        "DELETE FROM skills WHERE id = ?1 AND status = 'pending'",
        id
    )
    .execute(db)
    .await?;
    if result.rows_affected() == 0 {
        // Disambiguate "doesn't exist" vs "is already active" — both
        // hit `rows_affected == 0` but the user-facing fix differs.
        // The previous wording told both cases to look in
        // `candidates list`, where an active rule won't appear.
        let existing = rule_status(db, id).await?;
        return match existing.as_deref() {
            Some("active") => Err(CoreError::Validation(format!(
                "rule '{id}' is already an active rule, not a pending memory draft."
            ))),
            _ => Err(CoreError::NotFound(format!(
                "memory draft '{id}' not found. Run `difflore status` for the next action."
            ))),
        };
    }
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
    (!drafted.is_empty()).then(|| drafted.lines().collect::<Vec<_>>().join(" "))
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
    use super::{CandidateRule, parse_candidate_drafted_rule, parse_candidate_file_patterns};
    use crate::domain::rule_view::RuleView;

    #[test]
    fn candidate_rule_implements_rule_view() {
        let c = CandidateRule {
            id: "c1".into(),
            name: "n".into(),
            description: "desc".into(),
            origin: "agent-memory".into(),
            installed_at: String::new(),
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
            Some("When touching `src/**/*.rs`, prefer structured parsing.")
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
}
