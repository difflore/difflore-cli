//! Full-body rendering of recalled rules for the `difflore recall --json`
//! surface.
//!
//! `recall` retrieves rule *chunks* (`ScoredRuleChunk`: id + indexed body +
//! score) and lightweight display metadata (`file_patterns`, `source_repo`).
//! That is enough to rank and headline a hit, but the indexed body text rarely
//! carries the rule's bad/good code examples — those live in the
//! `rule_examples` table, which recall never read. As a result
//! `recall --json` surfaced only titles/previews with the fix/bad/good bodies
//! NULL, so an agent consuming recall saw headlines but not the actual team
//! memory.
//!
//! This module closes that gap by reusing the SAME public code-spec renderer
//! the MCP `get_rules` detail path uses
//! ([`crate::context::rule_render::render_code_spec`]) plus the public example
//! loader ([`crate::context::rule_source::load_rule_examples_batch`]). It is
//! the DB-backed counterpart to the DB-free `rule_render` module: it fetches
//! the renderable skill columns + examples and projects them into a
//! [`RenderedRuleBody`] the CLI can serialise directly.

use std::collections::HashMap;

use sqlx::SqlitePool;

use crate::context::rule_render::{RuleRenderInput, render_code_spec};
use crate::context::rule_source::{RuleExample, load_rule_examples_batch};
use crate::errors::CoreError;

/// A single bad/good example pair surfaced on a recalled rule. Mirrors the
/// `rule_examples` row shape the MCP `get_rules` tool already returns, so the
/// recall `--json` and `get_rules` example surfaces stay aligned.
#[derive(Debug, Clone)]
pub struct RenderedRuleExample {
    pub bad_code: String,
    pub good_code: String,
    pub description: Option<String>,
}

impl From<&RuleExample> for RenderedRuleExample {
    fn from(ex: &RuleExample) -> Self {
        Self {
            bad_code: ex.bad_code.clone(),
            good_code: ex.good_code.clone(),
            description: ex.description.clone(),
        }
    }
}

/// The full, agent-consumable body of a recalled rule: the rendered code-spec
/// `body` (the same template `get_rules` emits), the structured examples, and
/// the supplementary `check`/`trigger`/`origin`/`confidence` fields that the
/// chunk-only recall path could not see.
#[derive(Debug, Clone)]
pub struct RenderedRuleBody {
    /// Full code-spec markdown body (contract / validation matrix / cases /
    /// self-check / provenance), rendered by `render_code_spec`.
    pub body: String,
    pub origin: String,
    pub confidence: f64,
    /// `skills.trigger`, when populated.
    pub trigger: Option<String>,
    /// `skills.check_prompt`, when populated.
    pub check: Option<String>,
    /// Structured bad/good example rows from `rule_examples`.
    pub examples: Vec<RenderedRuleExample>,
}

impl RenderedRuleBody {
    /// First example's `bad_code`, trimmed, when present and non-empty. This is
    /// the authoritative "bad" snippet (straight from the `rule_examples`
    /// table) that the chunk-only heuristic could not reliably recover.
    #[must_use]
    pub fn first_bad_code(&self) -> Option<String> {
        self.examples
            .iter()
            .map(|ex| ex.bad_code.trim())
            .find(|code| !code.is_empty())
            .map(ToOwned::to_owned)
    }

    /// First example's `good_code` (the fix), trimmed, when present.
    #[must_use]
    pub fn first_good_code(&self) -> Option<String> {
        self.examples
            .iter()
            .map(|ex| ex.good_code.trim())
            .find(|code| !code.is_empty())
            .map(ToOwned::to_owned)
    }
}

/// Renderable columns for a single skill. Runtime `sqlx::query_as` (not the
/// `query!` macro) so adding this read doesn't require regenerating the offline
/// `.sqlx/` cache — the same escape hatch the age-decay loader in
/// `rule_source` documents. `trigger` is backtick-quoted (reserved word),
/// mirroring the MCP `fetch_skills_by_ids` SELECT.
#[derive(sqlx::FromRow)]
struct RenderRow {
    id: String,
    name: String,
    r#type: String,
    description: String,
    confidence_score: f64,
    file_patterns: Option<String>,
    origin: String,
    source_repo: Option<String>,
    trigger: Option<String>,
    check_prompt: Option<String>,
}

fn parse_file_patterns(raw: Option<&str>) -> Vec<String> {
    let Some(raw) = raw.map(str::trim).filter(|r| !r.is_empty()) else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<String>>(raw).unwrap_or_default()
}

/// Fetch and render the full body (code-spec + structured examples + fix/check
/// fields) for each active skill id, keyed by id. Ids that don't resolve to an
/// active skill are simply absent from the returned map, so the caller can fall
/// back to its chunk-only display for stale index entries.
///
/// Reuses the public renderers so recall and MCP `get_rules` show the same
/// code spec and rule examples.
pub async fn render_full_rule_bodies(
    pool: &SqlitePool,
    ids: &[String],
) -> Result<HashMap<String, RenderedRuleBody>, CoreError> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let ids_json = serde_json::to_string(ids)
        .map_err(|e| CoreError::Internal(format!("encode skill ids: {e}")))?;
    // Mirror the MCP serve boundary: only active skills are served, never a
    // pending candidate.
    let rows = sqlx::query_as::<_, RenderRow>(
        "SELECT id, name, type, description, confidence_score, file_patterns, \
                origin, source_repo, `trigger`, check_prompt \
         FROM skills WHERE id IN (SELECT value FROM json_each(?1)) AND status = 'active'",
    )
    .bind(ids_json)
    .fetch_all(pool)
    .await
    .map_err(|e| CoreError::Internal(format!("skills body lookup failed: {e}")))?;

    let present_ids: Vec<String> = rows.iter().map(|row| row.id.clone()).collect();
    let examples_map = load_rule_examples_batch(pool, &present_ids)
        .await
        .unwrap_or_default();

    let mut out = HashMap::with_capacity(rows.len());
    for row in rows {
        let file_patterns = parse_file_patterns(row.file_patterns.as_deref());
        let examples = examples_map.get(&row.id);
        let input = RuleRenderInput {
            id: &row.id,
            name: &row.name,
            r#type: &row.r#type,
            confidence: row.confidence_score,
            origin: &row.origin,
            source_repo: row.source_repo.as_deref(),
            file_patterns: &file_patterns,
            description: &row.description,
            trigger: row.trigger.as_deref(),
            check_prompt: row.check_prompt.as_deref(),
            examples: examples.map(Vec::as_slice),
        };
        let body = render_code_spec(&input);
        let rendered = RenderedRuleBody {
            body,
            origin: row.origin,
            confidence: row.confidence_score,
            trigger: row
                .trigger
                .map(|t| t.trim().to_owned())
                .filter(|t| !t.is_empty()),
            check: row
                .check_prompt
                .map(|c| c.trim().to_owned())
                .filter(|c| !c.is_empty()),
            examples: examples
                .map(|ex| ex.iter().map(RenderedRuleExample::from).collect())
                .unwrap_or_default(),
        };
        out.insert(row.id, rendered);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_file_patterns_tolerates_missing_and_malformed() {
        assert!(parse_file_patterns(None).is_empty());
        assert!(parse_file_patterns(Some("")).is_empty());
        assert!(parse_file_patterns(Some("not-json")).is_empty());
        assert_eq!(
            parse_file_patterns(Some(r#"["**/*.go"]"#)),
            vec!["**/*.go".to_owned()]
        );
    }

    #[test]
    fn first_bad_and_good_code_pick_first_non_empty() {
        let body = RenderedRuleBody {
            body: String::new(),
            origin: "pr_review".to_owned(),
            confidence: 0.8,
            trigger: None,
            check: None,
            examples: vec![
                RenderedRuleExample {
                    bad_code: "   ".to_owned(),
                    good_code: String::new(),
                    description: None,
                },
                RenderedRuleExample {
                    bad_code: "io.ReadAll(r.Body)".to_owned(),
                    good_code: "http.MaxBytesReader(w, r.Body, max)".to_owned(),
                    description: Some("cap the body".to_owned()),
                },
            ],
        };
        assert_eq!(body.first_bad_code().as_deref(), Some("io.ReadAll(r.Body)"));
        assert_eq!(
            body.first_good_code().as_deref(),
            Some("http.MaxBytesReader(w, r.Body, max)")
        );
    }

    #[test]
    fn first_code_is_none_when_no_examples() {
        let body = RenderedRuleBody {
            body: String::new(),
            origin: "conversation".to_owned(),
            confidence: 0.5,
            trigger: None,
            check: None,
            examples: Vec::new(),
        };
        assert!(body.first_bad_code().is_none());
        assert!(body.first_good_code().is_none());
    }

    async fn test_db() -> SqlitePool {
        use std::str::FromStr;
        let _home = crate::db::shared_test_home();
        let opts = sqlx::sqlite::SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .foreign_keys(true);
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        crate::db::run_migrations(&pool).await.unwrap();
        pool
    }

    #[tokio::test]
    async fn render_full_rule_bodies_includes_example_and_renders_cases() {
        // A recalled rule whose bad/good code lives in the `rule_examples`
        // table (NOT in the indexed body prose) must still render a full body
        // with the Cases block and surface the example bad/good code. This is
        // the exact gap that made `recall --json` return NULL fix/bad/good.
        let db = test_db().await;
        let now = chrono::Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO skills
             (id, name, source, directory, version, description, type, engines, tags,
              enabled_for_claude, installed_at, updated_at, status, origin, source_repo,
              confidence_score, file_patterns)
             VALUES (?1, ?2, 'local', ?3, '1.0.0', ?4, 'review_standard', '[]', '[]',
                     1, ?5, ?5, 'active', 'pr_review', 'acme/widgets', 0.82, ?6)",
        )
        .bind("rule-cap-bodies")
        .bind("Cap request bodies")
        .bind("cap-request-bodies")
        .bind("When touching `**/*.go`, cap request bodies with MaxBytesReader.")
        .bind(&now)
        .bind(r#"["**/*.go"]"#)
        .execute(&db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO rule_examples (id, skill_id, bad_code, good_code, description, source, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'test', ?6)",
        )
        .bind("ex-cap-bodies")
        .bind("rule-cap-bodies")
        .bind("data, _ := io.ReadAll(r.Body)")
        .bind("r.Body = http.MaxBytesReader(w, r.Body, max)")
        .bind("reviewer flagged unbounded read")
        .bind(&now)
        .execute(&db)
        .await
        .unwrap();

        let bodies = render_full_rule_bodies(&db, &["rule-cap-bodies".to_owned()])
            .await
            .unwrap();
        let rendered = bodies
            .get("rule-cap-bodies")
            .expect("rendered body present");

        // The body is the full code-spec, including the Cases block built from
        // the example — not a one-line preview.
        assert!(
            rendered
                .body
                .contains("## Rule rule-cap-bodies — Cap request bodies")
        );
        assert!(rendered.body.contains("### Cases"));
        assert!(rendered.body.contains("data, _ := io.ReadAll(r.Body)"));
        assert!(
            rendered
                .body
                .contains("http.MaxBytesReader(w, r.Body, max)")
        );
        // Structured example + authoritative bad/good code surfaced.
        assert_eq!(rendered.examples.len(), 1);
        assert_eq!(
            rendered.first_bad_code().as_deref(),
            Some("data, _ := io.ReadAll(r.Body)")
        );
        assert_eq!(
            rendered.first_good_code().as_deref(),
            Some("r.Body = http.MaxBytesReader(w, r.Body, max)")
        );
        assert_eq!(rendered.origin, "pr_review");
        assert!((rendered.confidence - 0.82).abs() < 1e-9);
    }

    #[tokio::test]
    async fn render_full_rule_bodies_skips_unknown_or_pending_ids() {
        let db = test_db().await;
        let now = chrono::Utc::now().to_rfc3339();
        // A pending candidate must never be served back through the body
        // renderer (mirrors the MCP serve boundary).
        sqlx::query(
            "INSERT INTO skills
             (id, name, source, directory, version, description, type, engines, tags,
              enabled_for_claude, installed_at, updated_at, status)
             VALUES (?1, ?2, 'local', ?3, '1.0.0', '', 'review_standard', '[]', '[]',
                     1, ?4, ?4, 'pending')",
        )
        .bind("pending-rule")
        .bind("Pending rule")
        .bind("pending-rule")
        .bind(&now)
        .execute(&db)
        .await
        .unwrap();

        let bodies = render_full_rule_bodies(
            &db,
            &["pending-rule".to_owned(), "does-not-exist".to_owned()],
        )
        .await
        .unwrap();
        assert!(
            bodies.is_empty(),
            "pending and unknown ids must not render a body"
        );
    }
}
