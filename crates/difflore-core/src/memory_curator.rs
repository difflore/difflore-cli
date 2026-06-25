//! Shared local-memory curation pipeline.
//!
//! Source-specific collectors (PR review import, session mining, explicit
//! remember requests) should produce candidate memories. This module owns the
//! common local-AI step: batch prompt shape, JSON parsing, confidence threshold
//! defaults, safety filtering helpers, and graceful "leave for review" failure
//! behavior.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Result;

pub const DEFAULT_CURATOR_BATCH_LIMIT: usize = 20;
pub const DEFAULT_CURATOR_MIN_CONFIDENCE: f32 = 0.85;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryCuratorSource {
    PrReview,
    SessionMined,
    Conversation,
}

impl MemoryCuratorSource {
    const fn label(self) -> &'static str {
        match self {
            Self::PrReview => "PR review comments",
            Self::SessionMined => "coding-agent sessions",
            Self::Conversation => "explicit user memory captures",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MemoryCuratorOptions {
    pub max_candidates: usize,
    pub min_confidence: f32,
}

impl Default for MemoryCuratorOptions {
    fn default() -> Self {
        Self {
            max_candidates: DEFAULT_CURATOR_BATCH_LIMIT,
            min_confidence: DEFAULT_CURATOR_MIN_CONFIDENCE,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MemoryCuratorCandidate {
    pub group_id: String,
    pub current_title: String,
    pub current_rule: String,
    pub source: MemoryCuratorSource,
    pub source_repo: Option<String>,
    pub file_patterns: Vec<String>,
    pub source_evidence: String,
}

impl MemoryCuratorCandidate {
    pub const fn new(
        group_id: String,
        current_title: String,
        current_rule: String,
        source: MemoryCuratorSource,
    ) -> Self {
        Self {
            group_id,
            current_title,
            current_rule,
            source,
            source_repo: None,
            file_patterns: Vec::new(),
            source_evidence: String::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryCuratorAction {
    Enable,
    Review,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryCuratorScope {
    Universal,
    LanguageWide,
    PathScoped,
}

impl MemoryCuratorScope {
    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "universal" => Some(Self::Universal),
            "language_wide" | "language-wide" | "languagewide" => Some(Self::LanguageWide),
            "path_scoped" | "path-scoped" | "pathscoped" => Some(Self::PathScoped),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct MemoryCuratorDecision {
    pub group_id: String,
    pub action: MemoryCuratorAction,
    pub confidence: f32,
    pub title: Option<String>,
    pub rule: Option<String>,
    pub reason: Option<String>,
    pub scope: Option<MemoryCuratorScope>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MemoryCuratorOutcome {
    pub decisions: Vec<MemoryCuratorDecision>,
    pub unavailable_reason: Option<String>,
}

impl MemoryCuratorOutcome {
    fn unavailable(detail: impl Into<String>) -> Self {
        Self {
            decisions: Vec::new(),
            unavailable_reason: Some(truncate_chars(&detail.into(), 240)),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AiCuratorDecision {
    group_id: String,
    action: String,
    confidence: f32,
    title: Option<String>,
    rule: Option<String>,
    reason: Option<String>,
    scope: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AiCuratorEnvelope {
    decisions: Vec<AiCuratorDecision>,
}

impl From<AiCuratorDecision> for MemoryCuratorDecision {
    fn from(value: AiCuratorDecision) -> Self {
        let action = if value.action.trim().eq_ignore_ascii_case("enable") {
            MemoryCuratorAction::Enable
        } else {
            MemoryCuratorAction::Review
        };
        Self {
            group_id: value.group_id,
            action,
            // Reject out-of-range / non-finite model confidence by mapping it to
            // 0 (NOT clamping to 1.0). A legitimate model returns 0..=1; a value
            // like 1e9 is a malformed/poisoned response, and clamping it to 1.0
            // would still clear the `>= min_confidence` auto-enable gate
            // (memory_autopilot.rs) and silently promote a draft to an active
            // rule. Treating it as invalid (0.0) keeps it out of auto-enable.
            confidence: if value.confidence.is_finite() && (0.0..=1.0).contains(&value.confidence) {
                value.confidence
            } else {
                0.0
            },
            title: value.title,
            rule: value.rule,
            reason: value.reason,
            scope: value.scope.as_deref().and_then(MemoryCuratorScope::parse),
        }
    }
}

pub fn file_patterns_for_curator_scope(
    scope: MemoryCuratorScope,
    anchors: &[String],
) -> Option<Vec<String>> {
    match scope {
        MemoryCuratorScope::PathScoped => None,
        MemoryCuratorScope::Universal => Some(Vec::new()),
        MemoryCuratorScope::LanguageWide => {
            let patterns = language_wide_file_patterns(anchors);
            (!patterns.is_empty()).then_some(patterns)
        }
    }
}

pub async fn curate_memory_candidates_with_local_ai(
    candidates: &[MemoryCuratorCandidate],
    options: MemoryCuratorOptions,
) -> Result<MemoryCuratorOutcome> {
    let candidates = candidates
        .iter()
        .take(options.max_candidates)
        .cloned()
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return Ok(MemoryCuratorOutcome {
            decisions: Vec::new(),
            unavailable_reason: None,
        });
    }

    let prompt_json = serde_json::to_string_pretty(&candidates)?;
    let source_summary = summarize_sources(&candidates);
    let system_prompt = "You are DiffLore's local memory curator. You turn raw coding evidence \
        into durable coding-agent rules only when the evidence contains a clear, reusable team \
        preference. Return JSON only. Never approve vague comments, one-off questions, jokes, \
        broad taste, or evidence that needs missing context.";
    let user_prompt = format!(
        "Review these candidate memories from {source_summary} and decide whether each should \
         become an active local coding-agent rule.\n\n\
         For each candidate return one decision:\n\
         - action: \"enable\" only if the rule is durable, actionable, repo-specific, and safe \
           for an agent to apply without more context.\n\
         - action: \"review\" for vague, low-context, one-off, subjective, question-shaped, or \
           conflict-prone evidence.\n\
         - confidence: 0.0 to 1.0.\n\
         - title: short imperative title when enabling.\n\
         - rule: rewritten rule text when enabling. The rule must be explicit, imperative, and \
           not quote the source wording.\n\
         - scope: \"path_scoped\", \"language_wide\", or \"universal\" when enabling. Default to \
           \"path_scoped\". Use \"language_wide\" only for cross-file team conventions such as \
           shared APIs/components (`$api`, `~/base/*`), framework primitives (TanStack, \
           useMemo), or evidence that says always/never/strictly forbidden/project rule/all \
           files/anywhere. Use \"universal\" only for clearly repo-wide, language-independent \
           rules. If unsure, choose the narrower scope.\n\
         - reason: concise reason.\n\n\
         JSON schema:\n\
         {{\"decisions\":[{{\"groupId\":\"...\",\"action\":\"enable|review\",\"confidence\":0.0,\
         \"title\":\"...\",\"rule\":\"...\",\"scope\":\"path_scoped|language_wide|universal\",\
         \"reason\":\"...\"}}]}}\n\n\
         Candidates:\n{prompt_json}"
    );

    let raw = match crate::review_engine::complete_with_local_agent_cli(system_prompt, &user_prompt)
        .await
    {
        Ok(raw) => raw,
        Err(err) => return Ok(MemoryCuratorOutcome::unavailable(err.to_string())),
    };
    let decisions = match parse_curator_decisions(&raw) {
        Ok(decisions) => decisions,
        Err(err) => return Ok(MemoryCuratorOutcome::unavailable(err.to_string())),
    };

    Ok(MemoryCuratorOutcome {
        decisions,
        unavailable_reason: None,
    })
}

pub fn curator_decisions_by_group(
    decisions: Vec<MemoryCuratorDecision>,
) -> BTreeMap<String, MemoryCuratorDecision> {
    decisions
        .into_iter()
        .map(|decision| (decision.group_id.clone(), decision))
        .collect()
}

pub fn curator_rule_is_safe(title: &str, rule: &str) -> bool {
    let title = title.trim();
    let rule = rule.trim();
    if !(8..=140).contains(&title.chars().count()) {
        return false;
    }
    if !(40..=1_200).contains(&rule.chars().count()) {
        return false;
    }
    let text = format!("{title}\n{rule}").to_ascii_lowercase();
    if text.contains('?')
        || text.contains("could we")
        || text.contains("do we really")
        || text.contains("after all")
        || text.contains("reviewer said")
        || text.contains("source evidence")
    {
        return false;
    }
    [
        "use ",
        "avoid ",
        "prefer ",
        "do not ",
        "don't ",
        "replace ",
        "extract ",
        "inline ",
        "centralize ",
        "preserve ",
        "keep ",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

fn parse_curator_decisions(raw: &str) -> Result<Vec<MemoryCuratorDecision>> {
    let json_text = extract_json_object(raw)
        .ok_or_else(|| crate::CoreError::Internal("local AI CLI did not return JSON".to_owned()))?;
    let value: Value = serde_json::from_str(json_text).map_err(|err| {
        crate::CoreError::Internal(format!("local AI CLI returned invalid JSON: {err}"))
    })?;
    let raw_decisions = if value.is_array() {
        serde_json::from_value::<Vec<AiCuratorDecision>>(value).map_err(|err| {
            crate::CoreError::Internal(format!("local AI CLI decision parse failed: {err}"))
        })?
    } else {
        serde_json::from_value::<AiCuratorEnvelope>(value)
            .map_err(|err| {
                crate::CoreError::Internal(format!("local AI CLI decision parse failed: {err}"))
            })?
            .decisions
    };
    Ok(raw_decisions.into_iter().map(Into::into).collect())
}

/// Defensive JSON extractor shared with `memory_autopilot`: return the outermost
/// JSON object/array slice, tolerating code fences or prose around it.
pub(crate) fn extract_json_object(raw: &str) -> Option<&str> {
    let trimmed = raw.trim();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        return Some(trimmed);
    }
    let start_object = trimmed.find('{');
    let start_array = trimmed.find('[');
    let start = match (start_object, start_array) {
        (Some(object), Some(array)) => object.min(array),
        (Some(object), None) => object,
        (None, Some(array)) => array,
        (None, None) => return None,
    };
    let end = trimmed.rfind('}').or_else(|| trimmed.rfind(']'))?;
    (end > start).then_some(&trimmed[start..=end])
}

fn summarize_sources(candidates: &[MemoryCuratorCandidate]) -> String {
    let mut labels = candidates
        .iter()
        .map(|candidate| match candidate.source {
            MemoryCuratorSource::PrReview => MemoryCuratorSource::PrReview.label(),
            MemoryCuratorSource::SessionMined => MemoryCuratorSource::SessionMined.label(),
            MemoryCuratorSource::Conversation => MemoryCuratorSource::Conversation.label(),
        })
        .collect::<Vec<_>>();
    labels.sort_unstable();
    labels.dedup();
    labels.join(", ")
}

fn language_wide_file_patterns(anchors: &[String]) -> Vec<String> {
    let mut source_exts = BTreeSet::new();
    let mut fallback_exts = BTreeSet::new();
    for anchor in anchors {
        for ext in scope_anchor_extensions(anchor) {
            if !is_language_wide_extension(&ext) {
                continue;
            }
            if is_manifest_like_scope_anchor(anchor) {
                fallback_exts.insert(ext);
            } else {
                source_exts.insert(ext);
            }
        }
    }

    let exts = if source_exts.is_empty() {
        fallback_exts
    } else {
        source_exts
    };
    exts.into_iter()
        .take(crate::skills::REMEMBER_FILE_PATTERN_LIMIT)
        .map(|ext| format!("**/*.{ext}"))
        .collect()
}

fn scope_anchor_extensions(anchor: &str) -> Vec<String> {
    let normalized = anchor
        .trim()
        .trim_matches('`')
        .trim_matches('"')
        .trim_matches('\'')
        .trim_start_matches("./")
        .replace('\\', "/");
    if normalized.is_empty() {
        return Vec::new();
    }
    if let Some(start) = normalized.rfind(".{")
        && let Some(end_offset) = normalized[start + 2..].find('}')
    {
        return normalized[start + 2..start + 2 + end_offset]
            .split(',')
            .filter_map(normalize_extension)
            .collect();
    }
    normalized
        .rsplit_once('.')
        .and_then(|(_, ext)| normalize_extension(ext))
        .into_iter()
        .collect()
}

fn normalize_extension(raw: &str) -> Option<String> {
    let ext = raw
        .trim()
        .trim_start_matches('{')
        .trim_end_matches('}')
        .trim_end_matches(|ch: char| !ch.is_ascii_alphanumeric())
        .to_ascii_lowercase();
    if ext.is_empty() || ext.len() > 12 || !ext.chars().all(|ch| ch.is_ascii_alphanumeric()) {
        return None;
    }
    Some(ext)
}

fn is_manifest_like_scope_anchor(anchor: &str) -> bool {
    let lower = anchor.replace('\\', "/").to_ascii_lowercase();
    let file = lower.rsplit('/').next().unwrap_or(lower.as_str());
    lower.starts_with(".github/workflows/")
        || lower.contains("/.github/workflows/")
        || matches!(
            file,
            "go.mod"
                | "go.sum"
                | "cargo.toml"
                | "cargo.lock"
                | "package.json"
                | "package-lock.json"
                | "pnpm-lock.yaml"
                | "yarn.lock"
                | "bun.lockb"
                | "uv.lock"
                | "poetry.lock"
                | "requirements.txt"
                | "dockerfile"
        )
}

fn is_language_wide_extension(ext: &str) -> bool {
    matches!(
        ext,
        "c" | "cc"
            | "cpp"
            | "cxx"
            | "h"
            | "hpp"
            | "cs"
            | "go"
            | "java"
            | "js"
            | "jsx"
            | "mjs"
            | "cjs"
            | "ts"
            | "tsx"
            | "mts"
            | "cts"
            | "py"
            | "rb"
            | "rs"
            | "swift"
            | "kt"
            | "kts"
            | "php"
            | "vue"
            | "svelte"
            | "css"
            | "scss"
            | "less"
            | "mdx"
    )
}

/// Truncate `value` to at most `limit` characters, appending `...` when content
/// was dropped. Shared with `memory_autopilot`.
pub(crate) fn truncate_chars(value: &str, limit: usize) -> String {
    let mut chars = value.chars();
    let truncated = chars.by_ref().take(limit).collect::<String>();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_local_ai_curator_json_from_code_fence() {
        let raw = r#"```json
        {"decisions":[{"groupId":"g1","action":"enable","confidence":0.94,"title":"Prefer CSS layout over JavaScript sizing","rule":"Prefer CSS layout primitives over JavaScript calculations when sizing Universal Viewer layout regions.","reason":"durable styling rule"}]}
        ```"#;

        let decisions = parse_curator_decisions(raw).expect("parse");

        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].group_id, "g1");
        assert_eq!(decisions[0].action, MemoryCuratorAction::Enable);
        assert_eq!(decisions[0].scope, None);
    }

    #[test]
    fn parses_local_ai_curator_scope() {
        let raw = r#"
        {"decisions":[{"groupId":"g1","action":"enable","confidence":0.96,"title":"Use base media components","rule":"Use base media components for images and video in UI code instead of raw media tags.","scope":"language_wide","reason":"cross-file UI convention"}]}
        "#;

        let decisions = parse_curator_decisions(raw).expect("parse");

        assert_eq!(decisions[0].scope, Some(MemoryCuratorScope::LanguageWide));
    }

    #[test]
    fn ignores_unknown_local_ai_curator_scope() {
        let raw = r#"
        {"decisions":[{"groupId":"g1","action":"enable","confidence":0.96,"title":"Use base media components","rule":"Use base media components for images and video in UI code instead of raw media tags.","scope":"everywhereish","reason":"unknown scope"}]}
        "#;

        let decisions = parse_curator_decisions(raw).expect("parse");

        assert_eq!(decisions[0].scope, None);
    }

    #[test]
    fn out_of_range_curator_confidence_is_rejected_not_clamped() {
        // A malformed/poisoned response with confidence far outside 0..=1 must
        // NOT become max confidence (which would still clear the auto-enable
        // `>= min_confidence` gate). It is mapped to 0.0; in-range passes through.
        let raw = r#"
        {"decisions":[
          {"groupId":"hi","action":"enable","confidence":1000000000.0,"title":"x","rule":"r"},
          {"groupId":"neg","action":"enable","confidence":-5.0,"title":"y","rule":"r"},
          {"groupId":"ok","action":"enable","confidence":0.94,"title":"z","rule":"r"}
        ]}
        "#;
        let decisions = parse_curator_decisions(raw).expect("parse");
        let conf = |id: &str| {
            decisions
                .iter()
                .find(|d| d.group_id == id)
                .unwrap()
                .confidence
        };
        assert!(
            conf("hi").abs() < 1e-6,
            "out-of-range high must reject to 0, not clamp to 1"
        );
        assert!(conf("neg").abs() < 1e-6, "negative must reject to 0");
        assert!((conf("ok") - 0.94).abs() < 1e-6, "in-range value preserved");
    }

    #[test]
    fn language_wide_scope_derives_repo_wide_source_patterns() {
        let anchors = vec![
            "src/components/developers/hero/**/*.tsx".to_owned(),
            "**/package.json".to_owned(),
            "src/api/client.ts".to_owned(),
        ];

        assert_eq!(
            file_patterns_for_curator_scope(MemoryCuratorScope::LanguageWide, &anchors),
            Some(vec!["**/*.ts".to_owned(), "**/*.tsx".to_owned()])
        );
    }

    #[test]
    fn universal_and_path_scoped_scope_translate_without_guesswork() {
        let anchors = vec!["src/components/Hero.tsx".to_owned()];

        assert_eq!(
            file_patterns_for_curator_scope(MemoryCuratorScope::Universal, &anchors),
            Some(Vec::new())
        );
        assert_eq!(
            file_patterns_for_curator_scope(MemoryCuratorScope::PathScoped, &anchors),
            None
        );
    }

    #[test]
    fn curator_rule_safety_rejects_review_questions() {
        assert!(!curator_rule_is_safe(
            "Do we really need unknown here",
            "When touching `src/**/*.ts`, do we really need to use unknown here?"
        ));
        assert!(curator_rule_is_safe(
            "Avoid unknown for typed event payloads",
            "Avoid `unknown` for event payloads when the expected payload shape is known. Define or reuse a typed payload interface instead, and reserve `unknown` for truly opaque external data."
        ));
    }

    #[test]
    fn summarizes_mixed_sources_without_duplicates() {
        let candidates = vec![
            MemoryCuratorCandidate::new(
                "a".to_owned(),
                "Title".to_owned(),
                "Rule".to_owned(),
                MemoryCuratorSource::PrReview,
            ),
            MemoryCuratorCandidate::new(
                "b".to_owned(),
                "Title".to_owned(),
                "Rule".to_owned(),
                MemoryCuratorSource::SessionMined,
            ),
            MemoryCuratorCandidate::new(
                "c".to_owned(),
                "Title".to_owned(),
                "Rule".to_owned(),
                MemoryCuratorSource::PrReview,
            ),
        ];

        assert_eq!(
            summarize_sources(&candidates),
            "PR review comments, coding-agent sessions"
        );
    }
}
