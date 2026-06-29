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
pub const DEFAULT_CURATOR_MIN_CONFIDENCE: f32 = 0.82;

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

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MemoryCuratorCandidate {
    pub group_id: String,
    pub current_title: String,
    pub current_rule: String,
    pub source: MemoryCuratorSource,
    pub source_repo: Option<String>,
    pub file_patterns: Vec<String>,
    pub source_evidence: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub behavior_observations: Vec<MemoryCuratorBehaviorObservation>,
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
            behavior_observations: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MemoryCuratorBehaviorObservation {
    pub base_rate: f32,
    pub lift_oracle: f32,
    pub lift_e2e: f32,
    pub corrected: bool,
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

    let mut deterministic_decisions = Vec::new();
    let candidates_for_ai = candidates
        .iter()
        .filter(|candidate| {
            if matches!(
                behavior_redundancy_decision(&candidate.behavior_observations),
                BehaviorRedundancyDecision::DropRedundant
            ) {
                deterministic_decisions.push(MemoryCuratorDecision {
                    group_id: candidate.group_id.clone(),
                    action: MemoryCuratorAction::Review,
                    confidence: 0.0,
                    title: None,
                    rule: None,
                    reason: Some(
                        "local-cli behavior evidence shows this rule is already followed without memory"
                            .to_owned(),
                    ),
                    scope: None,
                });
                false
            } else {
                true
            }
        })
        .cloned()
        .collect::<Vec<_>>();

    if candidates_for_ai.is_empty() {
        return Ok(MemoryCuratorOutcome {
            decisions: deterministic_decisions,
            unavailable_reason: None,
        });
    }

    let prompt_json = serde_json::to_string_pretty(&candidates_for_ai)?;
    let source_summary = summarize_sources(&candidates_for_ai);
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
         - action: \"review\" for vague, low-context, one-off, subjective, question-shaped, \
           behavior-redundant, already-covered, or conflict-prone evidence. Also choose \
           \"review\" when a capable general coding model or existing active memory would \
           probably do the same thing by default (validate input, add tests, handle errors, \
           improve readability, remove dead code) and the evidence adds no non-obvious project \
           API/helper, generated artifact, schema/contract coupling, module boundary, version \
           constraint, or named team convention.\n\
         - behaviorObservations, when present, are local-cli before/after evidence: baseRate is \
           how often the model already followed the rule without memory, liftOracle/liftE2E are \
           improvements from injecting it, and corrected marks rescued failures. Treat >=3 \
           observations with baseRate=1.0 and no positive lift as behavior-redundant. Treat any \
           positive lift or corrected observation as evidence that the rule is not merely \
           obvious, even if it mentions a common platform habit.\n\
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
        Err(err) => {
            return Ok(MemoryCuratorOutcome {
                decisions: deterministic_decisions,
                unavailable_reason: Some(truncate_chars(&err.to_string(), 240)),
            });
        }
    };
    let mut decisions = match parse_curator_decisions(&raw) {
        Ok(decisions) => decisions,
        Err(err) => {
            return Ok(MemoryCuratorOutcome {
                decisions: deterministic_decisions,
                unavailable_reason: Some(truncate_chars(&err.to_string(), 240)),
            });
        }
    };
    decisions.extend(deterministic_decisions);

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
    curator_rule_is_safe_with_behavior(title, rule, &[])
}

pub fn curator_rule_is_safe_with_behavior(
    title: &str,
    rule: &str,
    behavior_observations: &[MemoryCuratorBehaviorObservation],
) -> bool {
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
    let behavior_decision = behavior_redundancy_decision(behavior_observations);
    if matches!(behavior_decision, BehaviorRedundancyDecision::DropRedundant) {
        return false;
    }
    if !matches!(
        behavior_decision,
        BehaviorRedundancyDecision::ProtectPositive
    ) && rule_is_obvious_low_value(title, rule)
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
        "call ",
        "extract ",
        "inline ",
        "centralize ",
        "preserve ",
        "keep ",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BehaviorRedundancyDecision {
    DropRedundant,
    ProtectPositive,
    Neutral,
}

fn behavior_redundancy_decision(
    observations: &[MemoryCuratorBehaviorObservation],
) -> BehaviorRedundancyDecision {
    const MIN_OBSERVATIONS: usize = 3;
    const REDUNDANT_BASE_RATE: f32 = 0.99;
    const POSITIVE_LIFT: f32 = 0.001;

    let usable = observations
        .iter()
        .copied()
        .filter(|observation| {
            observation.base_rate.is_finite() && (0.0..=1.0).contains(&observation.base_rate)
        })
        .collect::<Vec<_>>();

    if usable.iter().any(|observation| {
        observation.corrected
            || observation.lift_oracle > POSITIVE_LIFT
            || observation.lift_e2e > POSITIVE_LIFT
    }) {
        return BehaviorRedundancyDecision::ProtectPositive;
    }

    if usable.len() >= MIN_OBSERVATIONS
        && usable
            .iter()
            .all(|observation| observation.base_rate >= REDUNDANT_BASE_RATE)
    {
        return BehaviorRedundancyDecision::DropRedundant;
    }

    BehaviorRedundancyDecision::Neutral
}

fn rule_is_obvious_low_value(title: &str, rule: &str) -> bool {
    let text = format!("{title}\n{rule}");
    let lower = text.to_ascii_lowercase();
    let has_common_platform = has_common_platform_knowledge(&lower);
    if !has_obvious_default_action(&lower) && !has_common_platform {
        return false;
    }

    let has_named_anchor = has_named_code_anchor(&text);
    let has_project_contract = has_project_contract_signal(&lower);
    if has_common_platform && !has_project_contract {
        return true;
    }
    let has_strong_rationale = [
        "because",
        " so ",
        "otherwise",
        "break",
        "miss",
        "stale",
        "regression",
        "contract",
        "in sync",
    ]
    .iter()
    .any(|needle| lower.contains(needle));

    !(has_named_anchor && (has_project_contract || has_strong_rationale))
}

fn has_obvious_default_action(lower: &str) -> bool {
    [
        "add test",
        "write test",
        "include test",
        "cover test",
        "handle error",
        "handle exception",
        "handle edge case",
        "catch error",
        "surface error",
        "validate input",
        "validate parameter",
        "validate argument",
        "validate payload",
        "validate request",
        "input validation",
        "payload validation",
        "request validation",
        "improve readability",
        "improve clarity",
        "improve performance",
        "improve maintainability",
        "remove dead code",
        "remove unused code",
        "delete dead code",
        "delete unused code",
        "avoid duplication",
        "reduce duplication",
        "reduce complexity",
        "consistent casing",
        "consistent capitalization",
        "consistent naming",
        "matching casing",
        "group related test",
        "group similar test",
        "consolidate related test",
        "consolidate similar test",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn has_common_platform_knowledge(lower: &str) -> bool {
    lower.contains("preventdefault")
        || lower.contains("page reload")
        || lower.contains("version suffix casing")
        || (lower.contains("blocking code") && lower.contains("async"))
}

fn has_project_contract_signal(lower: &str) -> bool {
    [
        "shared",
        "generated",
        "schema",
        "contract",
        "boundary",
        "adapter",
        "router",
        "provider",
        "cache",
        "migration",
        "protocol",
        "compatibility",
        "serialization",
        "deserialization",
        "fixture",
        "in sync",
        "module's test file",
        "module test file",
        "different module",
        "module they exercise",
        "module boundary",
        "versioned schema",
        "versioned protocol",
        "versioned contract",
        "versioned compatibility",
        "version schema",
        "version protocol",
        "version contract",
        "version compatibility",
        "api version",
        "protocol version",
        "schema version",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn has_named_code_anchor(text: &str) -> bool {
    if text.contains('`') || text.contains('/') {
        return true;
    }
    for token in text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
        let mut has_lower = false;
        let mut has_upper = false;
        for ch in token.chars() {
            has_lower |= ch.is_ascii_lowercase();
            has_upper |= ch.is_ascii_uppercase();
        }
        if token.len() >= 5 && ((has_lower && has_upper) || token.contains('_')) {
            return true;
        }
    }
    false
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
    fn curator_rule_safety_rejects_obvious_default_model_rules() {
        assert!(!curator_rule_is_safe(
            "Use request validation in handlers",
            "Use request validation when changing HTTP handlers so invalid payloads do not cause errors. This is the default safe handling expected for ordinary request parsing."
        ));
        assert!(curator_rule_is_safe(
            "Use requireUser for auth loaders",
            "Use requireUser when wiring auth loaders because direct cookie reads miss refreshed sessions; the shared helper preserves the team's session refresh behavior."
        ));
    }

    #[test]
    fn curator_rule_safety_rejects_common_platform_version_casing() {
        assert!(!curator_rule_is_safe(
            "Use consistent version suffix casing in type names",
            "When naming versioned types in TypeScript, use uppercase V2 rather than lowercase v2. Mixing casing across related type names in the same file creates inconsistency."
        ));
    }

    #[test]
    fn curator_behavior_gate_rejects_stably_redundant_rules() {
        let behavior = vec![
            MemoryCuratorBehaviorObservation {
                base_rate: 1.0,
                lift_oracle: 0.0,
                lift_e2e: 0.0,
                corrected: false,
            },
            MemoryCuratorBehaviorObservation {
                base_rate: 1.0,
                lift_oracle: 0.0,
                lift_e2e: 0.0,
                corrected: false,
            },
            MemoryCuratorBehaviorObservation {
                base_rate: 1.0,
                lift_oracle: 0.0,
                lift_e2e: 0.0,
                corrected: false,
            },
        ];

        assert!(!curator_rule_is_safe_with_behavior(
            "Use requireUser for auth loaders",
            "Use requireUser when wiring auth loaders because direct cookie reads miss refreshed sessions; the shared helper preserves the team's session refresh behavior.",
            &behavior
        ));
    }

    #[tokio::test]
    async fn behavior_redundant_candidates_are_reviewed_before_ai() {
        let mut candidate = MemoryCuratorCandidate::new(
            "group-a".to_owned(),
            "Use requireUser for auth loaders".to_owned(),
            "Use requireUser when wiring auth loaders because direct cookie reads miss refreshed sessions; the shared helper preserves the team's session refresh behavior."
                .to_owned(),
            MemoryCuratorSource::PrReview,
        );
        candidate.behavior_observations = vec![
            MemoryCuratorBehaviorObservation {
                base_rate: 1.0,
                lift_oracle: 0.0,
                lift_e2e: 0.0,
                corrected: false,
            },
            MemoryCuratorBehaviorObservation {
                base_rate: 1.0,
                lift_oracle: 0.0,
                lift_e2e: 0.0,
                corrected: false,
            },
            MemoryCuratorBehaviorObservation {
                base_rate: 1.0,
                lift_oracle: 0.0,
                lift_e2e: 0.0,
                corrected: false,
            },
        ];

        let outcome =
            curate_memory_candidates_with_local_ai(&[candidate], MemoryCuratorOptions::default())
                .await
                .expect("curate");

        assert_eq!(outcome.unavailable_reason, None);
        assert_eq!(outcome.decisions.len(), 1);
        assert_eq!(outcome.decisions[0].group_id, "group-a");
        assert_eq!(outcome.decisions[0].action, MemoryCuratorAction::Review);
        assert!(
            outcome.decisions[0]
                .reason
                .as_deref()
                .unwrap_or_default()
                .contains("already followed without memory")
        );
    }

    #[test]
    fn curator_behavior_gate_protects_positive_platform_rules() {
        let behavior = vec![MemoryCuratorBehaviorObservation {
            base_rate: 0.67,
            lift_oracle: 0.33,
            lift_e2e: 0.0,
            corrected: false,
        }];

        assert!(curator_rule_is_safe_with_behavior(
            "Call preventDefault in form submit handlers",
            "When writing form submit handlers, call preventDefault before router mutations so the page does not reload.",
            &behavior
        ));
    }

    #[test]
    fn curator_rule_safety_keeps_module_boundary_test_placement() {
        assert!(curator_rule_is_safe(
            "Don't add tests for unrelated utilities in a module's test file",
            "When adding a new utility, place its tests in a test file scoped to that utility, not in an existing test file for a different module. Tests should live with the module they exercise."
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
