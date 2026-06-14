//! Serve-proof evidence helpers shared by the MCP tool handlers (split out
//! of the former `tools/util.rs`): strict file-pattern matching, evidence
//! records, and the rule-body rendering used to back them.

use globset::Glob;
use sqlx::SqlitePool;
use std::collections::{HashMap, HashSet};

use crate::context::types::{EvidenceKind, EvidenceRecord};
use crate::error::CoreError;

/// Row shape for the skills lookup used by `search_rules` / `get_rules`.
/// Kept private to the MCP layer so the surface stays narrow — no other
/// caller needs `origin+title+description+file_patterns` in one shot.
#[derive(sqlx::FromRow)]
pub(crate) struct SkillDetailRow {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) r#type: String,
    /// JSON-encoded tag list. Selected so the row deserialises cleanly; read
    /// only by the `FromRow` derive today since the code-spec body no longer
    /// renders a `Tags:` line.
    #[allow(dead_code)]
    pub(crate) tags: String,
    pub(crate) confidence_score: f64,
    pub(crate) file_patterns: Option<String>,
    pub(crate) origin: String,
    /// Source repo attribution. Pulled so `render_full_rule_with_examples` can
    /// emit a `Source: owner/repo` line that the agent can cite downstream.
    pub(crate) source_repo: Option<String>,
    /// Free-text "when to apply" hint, rendered as the code-spec `### Trigger`
    /// slot when present. Nullable — most local rules carry none.
    pub(crate) trigger: Option<String>,
    /// Free-text self-check prompt, rendered as the `### Self-check` slot when
    /// present. Same nullability as `trigger`.
    pub(crate) check_prompt: Option<String>,
}

/// Build the full markdown rule body for callers who pair
/// `search_rules` → `get_rules`.
///
/// Re-projects the row's stored fields into the code-spec template (contract /
/// validation matrix / cases / self-check / provenance) via the shared,
/// DB-free `context::rule_render` helpers, so a published pack renders
/// identically.
pub(crate) fn render_full_rule_with_examples(
    row: &SkillDetailRow,
    examples: Option<&Vec<crate::context::rule_source::RuleExample>>,
) -> String {
    let file_patterns = parse_file_patterns(row.file_patterns.as_deref());
    let input = crate::context::rule_render::RuleRenderInput {
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
    crate::context::rule_render::render_code_spec(&input)
}

/// Extract a short preview (<= 120 chars) of a rule body. Prefers the
/// `description` field (skips the generated header). Strips leading
/// whitespace and collapses inner newlines so the preview stays on one
/// line in typical tool-call rendering.
pub(crate) fn rule_preview(description: &str, limit: usize) -> String {
    let flat: String = description
        .trim()
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    let mut preview = String::with_capacity(limit.min(flat.len()));
    for ch in flat.chars() {
        if preview.chars().count() >= limit {
            break;
        }
        preview.push(ch);
    }
    preview
}

/// Parse the JSON-encoded `file_patterns` column into a `Vec<String>`.
/// Malformed / missing → empty vec. Matches the permissive behaviour of
/// `retrieval::pattern_allows` — a rule with a broken pattern list never
/// disappears silently, it just shows up with no patterns in the index.
pub fn parse_file_patterns(raw: Option<&str>) -> Vec<String> {
    let Some(raw) = raw else {
        return Vec::new();
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    serde_json::from_str::<Vec<String>>(trimmed).unwrap_or_default()
}

pub(crate) fn first_matching_pattern(patterns: &[String], target_file: &str) -> Option<String> {
    let normalised = target_file.trim_start_matches('/').replace('\\', "/");
    patterns.iter().find_map(|pattern| {
        Glob::new(pattern).ok().and_then(|glob| {
            glob.compile_matcher()
                .is_match(&normalised)
                .then(|| pattern.clone())
        })
    })
}

pub(crate) fn has_strict_file_patterns_match(file_patterns: &[String], target_file: &str) -> bool {
    let target_file = target_file.trim();
    if target_file.is_empty() || target_file == "unknown" {
        return false;
    }
    first_matching_pattern(file_patterns, target_file).is_some()
}

/// Buyer-grade serve proof should mean "this rule's explicit file scope
/// matched the file the agent was touching." Universal/no-pattern rules
/// are still eligible for recall, but they are not strict file proof.
pub(crate) fn has_strict_file_scope_match(
    file_patterns_raw: Option<&str>,
    target_file: &str,
) -> bool {
    let target_file = target_file.trim();
    let patterns = parse_file_patterns(file_patterns_raw);
    has_strict_file_patterns_match(&patterns, target_file)
}

pub(crate) fn strict_file_match_count_for_ids(
    meta_map: &HashMap<String, SkillDetailRow>,
    ids: &[String],
    target_file: Option<&str>,
) -> i64 {
    let Some(target_file) = target_file else {
        return 0;
    };
    let count = ids
        .iter()
        .filter(|id| {
            meta_map.get(id.as_str()).is_some_and(|row| {
                has_strict_file_scope_match(row.file_patterns.as_deref(), target_file)
            })
        })
        .count();
    i64::try_from(count).unwrap_or(i64::MAX)
}

pub(crate) fn strict_file_match_ids_for_rules(
    rules: &[crate::context::rule_source::RuleDocument],
    target_file: Option<&str>,
) -> HashSet<String> {
    let Some(target_file) = target_file else {
        return HashSet::new();
    };
    rules
        .iter()
        .filter(|rule| has_strict_file_scope_match(rule.file_patterns.as_deref(), target_file))
        .map(|rule| rule.skill_id.clone())
        .collect()
}

pub(crate) fn strict_file_match_ids_for_meta(
    meta_map: &HashMap<String, SkillDetailRow>,
    target_file: Option<&str>,
) -> HashSet<String> {
    let Some(target_file) = target_file else {
        return HashSet::new();
    };
    meta_map
        .iter()
        .filter(|(_, row)| has_strict_file_scope_match(row.file_patterns.as_deref(), target_file))
        .map(|(id, _)| id.clone())
        .collect()
}

pub(crate) fn build_match_evidence(
    file: &str,
    similarity: f64,
    file_patterns: &[String],
    confidence: f64,
) -> Vec<EvidenceRecord> {
    let mut evidence = Vec::new();

    if file != "unknown" {
        if let Some(pattern) = first_matching_pattern(file_patterns, file) {
            evidence.push(
                EvidenceRecord::new(
                    EvidenceKind::FilePatternMatch,
                    format!("target file `{file}` matches file_patterns via `{pattern}`"),
                )
                .with_source("search_rules")
                .with_target(file.to_owned())
                .with_matched_value(pattern),
            );
        } else if file_patterns.is_empty() {
            evidence.push(
                EvidenceRecord::new(
                    EvidenceKind::FilePatternMatch,
                    format!(
                        "target file `{file}` is eligible because the rule has no file_patterns"
                    ),
                )
                .with_source("search_rules")
                .with_target(file.to_owned())
                .with_matched_value("universal"),
            );
        }
    }

    evidence.push(
        EvidenceRecord::new(
            EvidenceKind::RetrievalMatch,
            format!("retrieval match score {similarity:.3} with confidence {confidence:.2}"),
        )
        .with_source("search_rules")
        .with_score(similarity)
        .with_target(file.to_owned()),
    );

    evidence
}

pub(crate) fn build_timeline_evidence(
    kind: EvidenceKind,
    source: &str,
    ts: &str,
    preview: &str,
) -> EvidenceRecord {
    let reason = match kind {
        EvidenceKind::RuleCreated => format!("rule created from {source} at {ts}"),
        EvidenceKind::RuleUpdated => format!("rule updated from {source} at {ts}"),
        EvidenceKind::RuleExample => format!("example captured from {source} at {ts}"),
        EvidenceKind::TriggerMatch => format!("trigger text carried forward from {source} at {ts}"),
        EvidenceKind::FilePatternMatch => format!("file-pattern match at {ts}"),
        EvidenceKind::RetrievalMatch => format!("retrieval match at {ts}"),
        EvidenceKind::SemanticSimilarity => format!("semantic match at {ts}"),
        EvidenceKind::PastVerdictRecall => format!("past verdict recall at {ts}"),
    };

    EvidenceRecord::new(kind, reason)
        .with_source(source.to_owned())
        .with_ts(ts.to_owned())
        .with_matched_value(preview.to_owned())
}

/// Look up skills metadata for the given IDs in one query. Returns a
/// `HashMap` so the caller can preserve the input order when rendering.
pub(crate) async fn fetch_skills_by_ids(
    db: &SqlitePool,
    ids: &[String],
) -> Result<HashMap<String, SkillDetailRow>, CoreError> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    // MCP-serve boundary: never hand a pending candidate back through
    // get_rules / search_rules. Index chunks for these are filtered at
    // load time, but a stale chunk could still resolve here.
    let ids_json =
        serde_json::to_string(ids).map_err(|e| CoreError::Internal(format!("encode ids: {e}")))?;
    let rows = sqlx::query_as::<_, SkillDetailRow>(
        "SELECT id, name, description, type, tags, confidence_score, file_patterns, origin, \
                source_repo, `trigger`, check_prompt \
         FROM skills WHERE id IN (SELECT value FROM json_each(?1)) AND status = 'active'",
    )
    .bind(ids_json)
    .fetch_all(db)
    .await
    .map_err(|e| CoreError::Internal(format!("skills lookup failed: {e}")))?;
    let mut map = HashMap::with_capacity(rows.len());
    for row in rows {
        map.insert(row.id.clone(), row);
    }
    Ok(map)
}

/// Truncate a string to at most `limit` chars without splitting inside a
/// grapheme. Used for preview fallbacks in `rule_timeline`; the global
/// `rule_preview` helper already handles the rule-description case but
/// `names/bad_code` snippets need a tighter cap.
pub(crate) fn truncate_chars(s: &str, limit: usize) -> String {
    s.chars().take(limit).collect()
}

/// Classify a skill's `origin` column into a timeline `kind`. The mapping
/// is the same one `rule_hits_by_origin` uses — one source of truth so the
/// telemetry aggregate and the timeline stream stay aligned.
pub fn origin_to_kind(origin: &str) -> &'static str {
    match origin {
        "conversation" => "remember",
        "pr_review" => "pr_review",
        "extracted" => "extracted",
        "manual" => "manual",
        "cloud" => "cloud",
        "team" => "team",
        _ => "created",
    }
}
