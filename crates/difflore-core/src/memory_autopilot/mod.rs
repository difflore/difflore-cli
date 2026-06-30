use std::collections::{BTreeMap, BTreeSet, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{Row, SqlitePool};

use crate::cloud::session_mined::SessionMinedLocalTriageStatus;
use crate::domain::models::SkillRecord;
use crate::memory_autopilot_schedule::{
    MemoryAutopilotScheduleStatus, load_autopilot_schedule_status,
};
use crate::memory_curator::{
    MemoryCuratorAction, MemoryCuratorCandidate, MemoryCuratorDecision, MemoryCuratorOptions,
    MemoryCuratorSource, curate_memory_candidates_with_local_ai, curator_decisions_by_group,
    curator_rule_is_safe, extract_json_object, file_patterns_for_curator_scope, truncate_chars,
};
use crate::memory_inbox::{
    approve_session_mined_candidate, delete_dropped_low_signal_session_mined_candidates,
    delete_session_mined_candidates_by_content_hash, load_memory_inbox,
    mark_session_mined_candidate_approved_for_rule, set_candidate_distinct_evidence_count,
    set_candidate_triage,
};
use crate::skills::{CandidateRule, list_candidates, promote_candidate};
use crate::{CoreError, Result};

mod classify;
mod cluster;
mod conflicts;
mod digest;
mod log;
mod plan;
mod run;
mod triage;

// Internal flat namespace: bring every submodule item into the parent so the
// crate-private machinery (and the `#[cfg(test)]` module via `super::*`)
// resolves names exactly as it did when this was a single flat file. The
// explicit re-exports below set the real external visibility for the public API
// and take precedence over these globs. The `allow` covers the lib build, where
// some globs are only exercised by the test module.
#[allow(unused_imports)]
use self::{classify::*, cluster::*, conflicts::*, digest::*, log::*, plan::*, run::*, triage::*};

pub use self::cluster::session_mined_candidates_semantically_match;
pub use self::conflicts::load_memory_conflicts;
pub use self::digest::load_memory_digest;
pub use self::log::{disable_memory_rule, load_autopilot_log};
pub use self::run::{promote_candidate_with_curator_recommendation, run_memory_autopilot};

pub(crate) use self::log::{
    AutopilotEventInput, ensure_autopilot_events_table, record_autopilot_event,
};

pub const MEMORY_AUTOPILOT_SCHEMA_VERSION: &str = "2026-06-16.memory.v1";
pub const DEFAULT_AUTOPILOT_LIMIT: usize = 3;
const MAX_AUTOPILOT_LIMIT: usize = 25;
const MAX_PENDING_SCAN: usize = 1_000;
const AUTOPILOT_CONFIDENCE: &str = "high";
const RECOMMENDED_CONFIDENCE: &str = "medium";
const DEFAULT_RECOMMENDED_MIN_CONFIDENCE: f32 = 0.70;

fn default_memory_autopilot_schema_version() -> String {
    MEMORY_AUTOPILOT_SCHEMA_VERSION.to_owned()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryAutopilotOptions {
    pub dry_run: bool,
    pub max_auto_enable: usize,
    pub curator_max_candidates: Option<usize>,
}

impl Default for MemoryAutopilotOptions {
    fn default() -> Self {
        Self {
            dry_run: false,
            max_auto_enable: DEFAULT_AUTOPILOT_LIMIT,
            curator_max_candidates: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryAutopilotLogFilter {
    pub limit: usize,
}

impl Default for MemoryAutopilotLogFilter {
    fn default() -> Self {
        Self { limit: 20 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MemoryDigest {
    #[serde(default = "default_memory_autopilot_schema_version")]
    pub schema_version: String,
    pub counts: MemoryDigestCounts,
    #[serde(default)]
    pub autopilot: MemoryAutopilotScheduleStatus,
    pub active_rules: Vec<MemoryDigestRule>,
    pub candidate_groups: Vec<MemoryCandidateGroup>,
    pub next_actions: Vec<String>,
    pub note: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MemoryDigestCounts {
    pub active_rules: i64,
    pub pending_drafts: i64,
    pub pending_session_candidates: i64,
    pub auto_enable_groups: usize,
    pub recommended_groups: usize,
    pub needs_review_groups: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MemoryDigestRule {
    pub item_id: String,
    pub rule_id: String,
    pub title: String,
    pub origin: String,
    pub source_repo: Option<String>,
    pub file_patterns: Vec<String>,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MemoryCandidateGroup {
    pub group_id: String,
    pub title: String,
    pub state: MemoryCandidateGroupState,
    pub reason: String,
    pub confidence: Option<String>,
    pub item_ids: Vec<String>,
    pub source_repo: Option<String>,
    pub file_patterns: Vec<String>,
    pub origins: Vec<String>,
    pub verdicts: Vec<String>,
    pub sample: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryCandidateGroupState {
    AutoEnable,
    Recommended,
    NeedsReview,
    AlreadyActive,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MemoryAutopilotReport {
    pub dry_run: bool,
    pub max_auto_enable: usize,
    pub auto_enabled: Vec<MemoryAutopilotAction>,
    pub skipped: Vec<MemoryAutopilotSkip>,
    pub digest: MemoryDigest,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MemoryAutopilotAction {
    pub group_id: String,
    pub title: String,
    pub rule_id: Option<String>,
    pub item_ids: Vec<String>,
    pub reason: String,
    pub dry_run: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MemoryAutopilotSkip {
    pub group_id: String,
    pub title: String,
    pub item_ids: Vec<String>,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MemoryAutopilotLog {
    #[serde(default = "default_memory_autopilot_schema_version")]
    pub schema_version: String,
    pub events: Vec<MemoryAutopilotEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MemoryAutopilotEvent {
    pub id: i64,
    pub event_type: String,
    pub rule_id: Option<String>,
    pub item_ids: Vec<String>,
    pub group_id: Option<String>,
    pub title: String,
    pub reason: String,
    pub payload: Value,
    pub created_at: String,
}

/// Filter for `load_memory_conflicts`. When `status` is set, only conflict
/// records in that lifecycle state are returned (e.g. `"detected"`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemoryConflictFilter {
    pub limit: Option<usize>,
    pub status: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MemoryConflictReport {
    #[serde(default = "default_memory_autopilot_schema_version")]
    pub schema_version: String,
    pub conflicts: Vec<MemoryConflictRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MemoryConflictRecord {
    pub evidence_hash: String,
    pub candidate_group_id: String,
    pub candidate_rule_id: Option<String>,
    pub active_rule_id: String,
    pub source_repo: Option<String>,
    pub overlap_basis: String,
    pub candidate_title: String,
    pub candidate_body: String,
    pub active_title: String,
    pub active_body: String,
    pub candidate_patterns: Vec<String>,
    pub active_patterns: Vec<String>,
    pub llm_rationale: Option<String>,
    pub llm_confidence: Option<f64>,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MemoryDisableOutcome {
    pub rule_id: String,
    pub item_id: String,
    pub title: String,
    pub previous_state: String,
    pub current_state: String,
    pub reason: String,
}

#[derive(Debug, Clone)]
struct PendingMemory {
    item_id: String,
    kind: PendingMemoryKind,
    title: String,
    body: String,
    raw_description: Option<String>,
    content_hash: Option<String>,
    origin: String,
    source_repo: Option<String>,
    file_patterns: Vec<String>,
    verdict: Option<String>,
    session_id: Option<String>,
    session_created_at_ms: Option<i64>,
    distinct_evidence_count: Option<usize>,
    autopilot_disabled: bool,
}

#[derive(Debug, Clone)]
enum PendingMemoryKind {
    Draft { id: String },
    Session { content_hash: String },
}

#[derive(Debug, Clone)]
struct ActiveMemory {
    item_id: String,
    rule_id: String,
    title: String,
    body: String,
    content_hash: Option<String>,
    origin: String,
    source_repo: Option<String>,
    file_patterns: Vec<String>,
    updated_at: String,
}

struct PlannedGroup {
    digest: MemoryCandidateGroup,
    candidates: Vec<PendingMemory>,
    /// Deterministic conflict with an in-scope active rule, captured at plan
    /// time so the autopilot side-effect path can persist a reviewable record.
    conflict: Option<ActiveConflict>,
}

#[derive(Debug, Clone)]
struct CachedCuratorRecommendation {
    input_hash: String,
    state: MemoryCandidateGroupState,
    confidence: Option<String>,
    title: String,
    rule: String,
    file_patterns: Vec<String>,
    reason: String,
    prompt_version: String,
}

#[derive(Debug, Clone, Copy)]
struct BuildPlanOptions {
    local_ai_curator: bool,
    curator_max_candidates: Option<usize>,
}

fn primary_candidate(candidates: &[PendingMemory]) -> Option<&PendingMemory> {
    candidates
        .iter()
        .find(|candidate| matches!(candidate.kind, PendingMemoryKind::Draft { .. }))
        .or_else(|| candidates.first())
}

fn format_confidence(confidence: f32) -> String {
    format!("{confidence:.2}")
}

fn candidate_group_key(candidate: &PendingMemory) -> String {
    let topic = topic_key(&format!("{} {}", candidate.title, candidate.body));
    let repo = candidate
        .source_repo
        .as_deref()
        .map(normalize_token)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "any-repo".to_owned());
    let patterns = pattern_key(&candidate.file_patterns);
    format!("{repo}:{topic}:{patterns}")
}

fn active_memory_key(rule: &ActiveMemory) -> String {
    let pending = PendingMemory {
        item_id: rule.item_id.clone(),
        kind: PendingMemoryKind::Draft {
            id: rule.rule_id.clone(),
        },
        title: rule.title.clone(),
        body: rule.body.clone(),
        raw_description: None,
        content_hash: rule.content_hash.clone(),
        origin: rule.origin.clone(),
        source_repo: rule.source_repo.clone(),
        file_patterns: rule.file_patterns.clone(),
        verdict: None,
        session_id: None,
        session_created_at_ms: None,
        distinct_evidence_count: None,
        autopilot_disabled: false,
    };
    candidate_group_key(&pending)
}

fn topic_key(text: &str) -> String {
    normalize_words(text)
        .into_iter()
        .take(8)
        .collect::<Vec<_>>()
        .join("-")
}

fn normalize_words(text: &str) -> Vec<String> {
    text.split(|ch: char| !ch.is_ascii_alphanumeric())
        .map(str::trim)
        .filter(|word| word.len() >= 3)
        .map(normalize_token)
        .filter(|word| !STOP_WORDS.contains(&word.as_str()))
        .collect()
}

const STOP_WORDS: &[&str] = &[
    "the",
    "and",
    "for",
    "with",
    "from",
    "into",
    "this",
    "that",
    "when",
    "should",
    "only",
    "avoid",
    "prefer",
    "rule",
    "memory",
    "candidate",
];

fn normalize_token(text: &str) -> String {
    text.chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '-' || *ch == '_' || *ch == '/')
        .collect::<String>()
        .to_ascii_lowercase()
}

fn pattern_key(patterns: &[String]) -> String {
    let mut normalized = normalize_patterns(patterns.to_vec());
    normalized.sort();
    normalized.join("|")
}

fn normalize_patterns(patterns: Vec<String>) -> Vec<String> {
    patterns
        .into_iter()
        .map(|pattern| pattern.trim().to_owned())
        .filter(|pattern| !pattern.is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn merged_patterns(candidates: &[PendingMemory]) -> Vec<String> {
    normalize_patterns(
        candidates
            .iter()
            .flat_map(|candidate| candidate.file_patterns.iter().cloned())
            .collect(),
    )
}

fn unique_strings<'a>(values: impl Iterator<Item = &'a str>) -> Vec<String> {
    values
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn single_source_repo(candidates: &[PendingMemory]) -> Option<String> {
    let repos = unique_strings(
        candidates
            .iter()
            .filter_map(|candidate| candidate.source_repo.as_deref()),
    );
    (repos.len() == 1).then(|| repos[0].clone())
}

fn strongest_title(candidates: &[PendingMemory]) -> String {
    candidates
        .iter()
        .max_by_key(|candidate| candidate.title.len())
        .map_or_else(
            || "Untitled memory".to_owned(),
            |candidate| candidate.title.clone(),
        )
}

fn file_patterns_are_broad(patterns: &[String]) -> bool {
    if patterns.is_empty() {
        return true;
    }
    patterns.iter().any(|pattern| {
        matches!(
            pattern.trim(),
            "**/*" | "src/**/*" | "**/*.ts" | "**/*.tsx" | "src/**/*.ts" | "src/**/*.tsx"
        )
    })
}

fn has_merge_verdict(candidates: &[PendingMemory]) -> bool {
    candidates.iter().any(|candidate| {
        candidate
            .verdict
            .as_deref()
            .unwrap_or_default()
            .to_ascii_uppercase()
            .starts_with("MERGE:")
    })
}

fn has_conflicting_language(candidates: &[PendingMemory]) -> bool {
    let text = candidates
        .iter()
        .map(|candidate| format!("{} {}", candidate.title, candidate.body).to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join("\n");
    (text.contains("inline") && text.contains("central") && text.contains("route"))
        || (contains_ascii_word(&text, "allow") && contains_ascii_word(&text, "disallow"))
        || (contains_ascii_word(&text, "always") && contains_ascii_word(&text, "never"))
}

fn contains_ascii_word(text: &str, needle: &str) -> bool {
    text.split(|ch: char| !ch.is_ascii_alphanumeric())
        .any(|word| word == needle)
}

/// An active rule that gives the OPPOSITE instruction to a candidate group, on
/// an overlapping subject within the same repo. Carries snapshots of the active
/// side so a persisted conflict record stays auditable even after the live rule
/// changes or is removed.
struct ActiveConflict {
    rule_id: String,
    title: String,
    basis: String,
    active_body: String,
    active_patterns: Vec<String>,
}

const POSITIVE_DIRECTIVES: &[&str] = &[
    "always",
    "must use",
    "should use",
    "prefer",
    "require",
    "enforce",
    "enable",
];
const NEGATIVE_DIRECTIVES: &[&str] = &[
    "never",
    "avoid",
    "must not",
    "do not",
    "don't",
    "disallow",
    "forbid",
    "disable",
    "should not",
];
const CONFLICT_STOP_WORDS: &[&str] = &[
    "always",
    "never",
    "avoid",
    "prefer",
    "require",
    "should",
    "must",
    "with",
    "this",
    "that",
    "from",
    "into",
    "when",
    "where",
    "rule",
    "rules",
    "code",
    "the",
    "and",
    "for",
    "use",
    "using",
    "not",
    "than",
    "then",
    "but",
    "create",
    "even",
    "generate",
    "home",
    "keep",
    "model",
    "name",
    "names",
    "reference",
    "repo",
    "string",
    "type",
    "typed",
];
const SINGLE_TOKEN_WEAK_CONFLICT_SUBJECTS: &[&str] = &[
    "event", "events", "flag", "flags", "handler", "handlers", "route", "routes", "request",
    "requests", "name", "names", "key", "keys",
];
const SHORT_CONFLICT_SUBJECTS: &[&str] = &["api", "css", "jwt", "sql", "ssr"];

fn contains_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| text.contains(needle))
}

/// Salient subject tokens (len >= 4, or whitelisted short tech tokens) used to
/// require a SHARED subject between two rules before calling them a conflict.
fn directive_subject_tokens(text: &str) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    for raw in text.split(|c: char| !c.is_ascii_alphanumeric()) {
        let token = raw.trim();
        if (token.len() < 4 && !SHORT_CONFLICT_SUBJECTS.contains(&token))
            || CONFLICT_STOP_WORDS.contains(&token)
        {
            continue;
        }
        if !tokens.iter().any(|existing| existing == token) {
            tokens.push(token.to_owned());
        }
    }
    tokens
}

/// True when two file-pattern sets plausibly cover the same files: an exact
/// (normalized) pattern match, or a shared file extension.
///
/// An EMPTY pattern set is a universal (repo-wide) rule that covers every file,
/// so it overlaps anything — treat it as overlapping so a global active rule is
/// conservatively routed to review rather than silently skipped.
fn patterns_overlap(a: &[String], b: &[String]) -> bool {
    if a.is_empty() || b.is_empty() {
        return true;
    }
    let normalize = |pattern: &str| pattern.trim().to_ascii_lowercase();
    let extension = |pattern: &str| {
        pattern
            .rsplit('.')
            .next()
            .map(|ext| {
                ext.trim_end_matches(|c: char| !c.is_ascii_alphanumeric())
                    .to_ascii_lowercase()
            })
            .filter(|ext| !ext.is_empty() && ext.len() <= 5)
    };
    a.iter().any(|pa| {
        let na = normalize(pa);
        let ea = extension(pa);
        b.iter()
            .any(|pb| na == normalize(pb) || (ea.is_some() && ea == extension(pb)))
    })
}

/// Returns a shared salient subject when two rule texts give opposite directives
/// (one positive, one negative) AND mention the same subject token.
fn opposing_directive_subject(
    candidate_text: &str,
    active_text: &str,
    candidate_tokens: &[String],
) -> Option<String> {
    let opposed = (contains_any(candidate_text, POSITIVE_DIRECTIVES)
        && contains_any(active_text, NEGATIVE_DIRECTIVES))
        || (contains_any(candidate_text, NEGATIVE_DIRECTIVES)
            && contains_any(active_text, POSITIVE_DIRECTIVES));
    if !opposed {
        return None;
    }
    let active_tokens = directive_subject_tokens(active_text);
    let shared = candidate_tokens
        .iter()
        .filter(|token| active_tokens.iter().any(|active| active == *token))
        .cloned()
        .collect::<Vec<_>>();
    if let Some(strong_subject) = shared
        .iter()
        .find(|token| !SINGLE_TOKEN_WEAK_CONFLICT_SUBJECTS.contains(&token.as_str()))
    {
        return Some(strong_subject.clone());
    }
    match shared.as_slice() {
        [_, _, ..] => Some(shared.into_iter().take(3).collect::<Vec<_>>().join(" + ")),
        _ => None,
    }
}

/// Deterministic candidate-vs-active opposing-directive check. Returns the first
/// in-scope active rule that contradicts the candidate group, so the group is
/// routed to human review instead of auto-enabling a rule that fights one
/// already in force.
///
/// Conservative by construction — a false positive only costs a human review,
/// never a wrong auto-enable — so it gates on ALL of: same source repo,
/// overlapping file patterns, and an opposite positive/negative directive on a
/// shared subject. The local-AI curator pass refines borderline cases.
fn detect_active_conflict(
    candidates: &[PendingMemory],
    source_repo: Option<&str>,
    file_patterns: &[String],
    active_rules: &[ActiveMemory],
) -> Option<ActiveConflict> {
    let repo = source_repo?.trim();
    if repo.is_empty() {
        return None;
    }
    let candidate_text = candidates
        .iter()
        .map(|candidate| format!("{} {}", candidate.title, candidate.body))
        .collect::<Vec<_>>()
        .join("\n")
        .to_ascii_lowercase();
    let candidate_tokens = directive_subject_tokens(&candidate_text);
    if candidate_tokens.is_empty() {
        return None;
    }
    for active in active_rules {
        let Some(active_repo) = active.source_repo.as_deref() else {
            continue;
        };
        if !active_repo.trim().eq_ignore_ascii_case(repo) {
            continue;
        }
        if !patterns_overlap(file_patterns, &active.file_patterns) {
            continue;
        }
        let active_text = format!("{} {}", active.title, active.body).to_ascii_lowercase();
        if let Some(basis) =
            opposing_directive_subject(&candidate_text, &active_text, &candidate_tokens)
        {
            return Some(ActiveConflict {
                rule_id: active.rule_id.clone(),
                title: active.title.clone(),
                basis,
                active_body: active.body.clone(),
                active_patterns: active.file_patterns.clone(),
            });
        }
    }
    None
}

fn session_keep_verdict(candidate: &PendingMemory) -> bool {
    candidate
        .verdict
        .as_deref()
        .map(str::trim)
        .unwrap_or_default()
        .eq_ignore_ascii_case("KEEP")
}

const fn group_rank(group: &MemoryCandidateGroup) -> usize {
    match group.state {
        MemoryCandidateGroupState::AutoEnable => 0,
        MemoryCandidateGroupState::Recommended => 1,
        MemoryCandidateGroupState::NeedsReview => 2,
        MemoryCandidateGroupState::AlreadyActive => 3,
    }
}

fn count_pending_kind(groups: &[PlannedGroup], kind: &str) -> i64 {
    let count = groups
        .iter()
        .filter(|group| group.digest.state != MemoryCandidateGroupState::AlreadyActive)
        .flat_map(|group| &group.candidates)
        .filter(|candidate| {
            matches!(
                (&candidate.kind, kind),
                (PendingMemoryKind::Draft { .. }, "draft")
                    | (PendingMemoryKind::Session { .. }, "session")
            )
        })
        .count();
    i64::try_from(count).unwrap_or(i64::MAX)
}

fn next_actions(counts: &MemoryDigestCounts) -> Vec<String> {
    let mut actions = Vec::new();
    if counts.recommended_groups > 0 {
        actions.push("difflore memory recommended".to_owned());
    }
    if counts.needs_review_groups > 0 {
        actions.push("difflore memory review".to_owned());
    }
    if counts.auto_enable_groups > 0 {
        actions.push("difflore memory log".to_owned());
    }
    if actions.is_empty() {
        actions.push("difflore recall <question-or-file>".to_owned());
    }
    actions
}

fn parse_string_list(raw: Option<&str>) -> Vec<String> {
    raw.map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(|value| serde_json::from_str::<Vec<String>>(value).ok())
        .unwrap_or_default()
}

fn normalize_limit(limit: usize) -> usize {
    if limit == 0 {
        DEFAULT_AUTOPILOT_LIMIT
    } else {
        limit.min(MAX_PENDING_SCAN)
    }
}

fn normalize_autopilot_limit(limit: usize) -> usize {
    if limit == 0 {
        DEFAULT_AUTOPILOT_LIMIT
    } else {
        limit.min(MAX_AUTOPILOT_LIMIT)
    }
}

fn normalize_rule_id(rule_id: &str) -> String {
    rule_id
        .trim()
        .strip_prefix("rule:")
        .unwrap_or_else(|| rule_id.trim())
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud::outbox::kind;
    use crate::cloud::session_mined::{SessionMinedCandidate, SessionMinedCandidateArgs};
    use crate::infra::git::RepoScope;
    use crate::memory_curator::MemoryCuratorScope;
    use sqlx::Row;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

    async fn fresh_pool() -> SqlitePool {
        let opts = SqliteConnectOptions::new()
            .filename(":memory:")
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .expect("connect sqlite");
        crate::infra::db::run_migrations(&pool)
            .await
            .expect("migrate");
        pool
    }

    async fn insert_session_mined_candidate(
        pool: &SqlitePool,
        session_id: &str,
        created_at_ms: i64,
        title: &str,
        body: &str,
        patterns: Vec<&str>,
    ) -> String {
        insert_session_mined_candidate_with_verdict(
            pool,
            session_id,
            created_at_ms,
            title,
            body,
            patterns,
            "KEEP",
        )
        .await
    }

    async fn insert_session_mined_candidate_with_verdict(
        pool: &SqlitePool,
        session_id: &str,
        created_at_ms: i64,
        title: &str,
        body: &str,
        patterns: Vec<&str>,
        gate_verdict: &str,
    ) -> String {
        let candidate = SessionMinedCandidate::try_new(SessionMinedCandidateArgs {
            session_id: session_id.to_owned(),
            ts_ms: created_at_ms,
            source_repo: RepoScope::canonical("owner/repo").expect("repo scope"),
            title: title.to_owned(),
            body: body.to_owned(),
            file_patterns: patterns.into_iter().map(str::to_owned).collect(),
            gate_model: "claude:haiku".to_owned(),
            gate_verdict: gate_verdict.to_owned(),
        })
        .expect("candidate");
        let content_hash = candidate.content_hash.clone();
        let payload = serde_json::to_string(&candidate).expect("payload json");
        sqlx::query(
            "INSERT INTO cloud_outbox (kind, payload_json, status, created_at) \
             VALUES (?1, ?2, 'pending', ?3)",
        )
        .bind(kind::SESSION_MINED_CANDIDATE)
        .bind(payload)
        .bind(created_at_ms)
        .execute(pool)
        .await
        .expect("insert session candidate");
        content_hash
    }

    #[tokio::test]
    async fn disable_moves_active_rule_to_disabled_and_excludes_it_from_pending() {
        let pool = fresh_pool().await;
        sqlx::query(
            "INSERT INTO skills (id, name, source, directory, version, description, type, engines, tags, status, origin) \
             VALUES \
                ('rule-1', 'Prefer block modals', 'local', '', '1.0.0', 'Use block wrappers.', 'review_standard', '[]', '[]', 'active', 'manual'), \
                ('draft-1', 'Prefer semantic buttons', 'local', '', '1.0.0', 'Use semantic buttons.', 'review_standard', '[]', '[]', 'pending', 'manual')",
        )
        .execute(&pool)
        .await
        .expect("insert rules");

        let outcome = disable_memory_rule(&pool, "rule:rule-1", Some("too noisy"))
            .await
            .expect("disable");

        assert_eq!(outcome.rule_id, "rule-1");
        assert_eq!(outcome.previous_state, "active");
        assert_eq!(outcome.current_state, "disabled");
        let status: String = sqlx::query_scalar("SELECT status FROM skills WHERE id = 'rule-1'")
            .fetch_one(&pool)
            .await
            .expect("status");
        assert_eq!(status, "disabled");
        let inbox = load_memory_inbox(&pool, 5).await.expect("inbox");
        assert_eq!(inbox.local_draft_count(), 1);
        assert_eq!(inbox.local_drafts.latest[0].id, "draft-1");
        let candidates = list_candidates(&pool, None, None)
            .await
            .expect("candidates");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].id, "draft-1");
        let log = load_autopilot_log(&pool, MemoryAutopilotLogFilter { limit: 5 })
            .await
            .expect("log");
        assert_eq!(log.events[0].event_type, "disabled");
        assert_eq!(
            log.events[0].payload,
            json!({ "previousState": "active", "currentState": "disabled" })
        );
    }

    #[tokio::test]
    async fn autopilot_supersedes_duplicate_session_candidates_and_stores_evidence() {
        let pool = fresh_pool().await;
        let body = "For local desktop development, prefer running npm run tauri dev because it \
            starts both the Vite dev server and the Tauri shell together. Running the compiled \
            binary alone can leave the UI blank because frontend assets are not served in the \
            same way during debugging.";
        for idx in 0..3 {
            insert_session_mined_candidate(
                &pool,
                &format!("session-{idx}"),
                1_714_000_000_000 + i64::from(idx) * 86_400_000,
                "Tauri dev startup: npm run tauri dev, not raw binary",
                &format!("{body} Evidence variant {idx}."),
                vec!["src-tauri/**/*.rs"],
            )
            .await;
        }

        let report = run_memory_autopilot(&pool, MemoryAutopilotOptions::default())
            .await
            .expect("autopilot");

        assert!(report.auto_enabled.is_empty());
        assert_eq!(report.digest.counts.pending_session_candidates, 1);
        let superseded_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM cloud_outbox \
             WHERE json_extract(payload_json, '$.localTriage.status') = 'superseded_by'",
        )
        .fetch_one(&pool)
        .await
        .expect("superseded count");
        assert_eq!(superseded_count, 2);
        let evidence_count: i64 = sqlx::query_scalar(
            "SELECT CAST(json_extract(payload_json, '$.localEvidence.distinctEvidenceCount') AS INTEGER) \
             FROM cloud_outbox \
             WHERE json_extract(payload_json, '$.localTriage.status') IS NULL",
        )
        .fetch_one(&pool)
        .await
        .expect("evidence count");
        assert_eq!(evidence_count, 3);
        let log = load_autopilot_log(&pool, MemoryAutopilotLogFilter { limit: 10 })
            .await
            .expect("log");
        assert!(
            log.events
                .iter()
                .any(|event| event.event_type == "session_candidate_superseded")
        );
    }

    #[tokio::test]
    async fn autopilot_purges_previously_dropped_low_signal_candidate() {
        let pool = fresh_pool().await;
        let hash = insert_session_mined_candidate(
            &pool,
            "session-noise",
            1_714_000_000_000,
            "Temporary scratch helper cleanup",
            "Remove the temporary scratch helper after the local debug run.",
            vec!["tmp/scratch/helper.ts"],
        )
        .await;
        set_candidate_triage(
            &pool,
            &hash,
            SessionMinedLocalTriageStatus::DroppedLowSignal,
            "previous cleanup marked this low signal",
            None,
        )
        .await
        .expect("mark dropped");

        let report = run_memory_autopilot(&pool, MemoryAutopilotOptions::default())
            .await
            .expect("autopilot");

        assert_eq!(report.digest.counts.pending_session_candidates, 0);
        let remaining_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM cloud_outbox \
             WHERE kind = 'session_mined_candidate'",
        )
        .fetch_one(&pool)
        .await
        .expect("remaining count");
        assert_eq!(remaining_count, 0);
    }

    #[tokio::test]
    async fn autopilot_keeps_cross_session_short_lesson_after_folding() {
        let pool = fresh_pool().await;
        for idx in 0..2 {
            insert_session_mined_candidate(
                &pool,
                &format!("session-{idx}"),
                1_714_000_000_000 + i64::from(idx) * 86_400_000,
                "Use ExternalLink for cross deployment navigation",
                &format!("Use ExternalLink for routes outside the router. Variant {idx}."),
                vec!["src/modules/ExternalLink.tsx"],
            )
            .await;
        }

        run_memory_autopilot(&pool, MemoryAutopilotOptions::default())
            .await
            .expect("first autopilot");
        let report = run_memory_autopilot(&pool, MemoryAutopilotOptions::default())
            .await
            .expect("second autopilot");

        assert_eq!(report.digest.counts.pending_session_candidates, 1);
        let dropped_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM cloud_outbox \
             WHERE json_extract(payload_json, '$.localTriage.status') = 'dropped_low_signal'",
        )
        .fetch_one(&pool)
        .await
        .expect("dropped count");
        assert_eq!(dropped_count, 0);
    }

    #[tokio::test]
    async fn autopilot_does_not_reenable_disabled_imported_rule() {
        let pool = fresh_pool().await;
        sqlx::query(
            "INSERT INTO skills \
                (id, name, source, directory, version, description, type, engines, tags, status, origin, file_patterns) \
             VALUES \
                ('rule-1', 'Prefer block modals', 'local', '', '1.0.0', \
                 'Source: pr_review\nComment: https://example.test/review\nRule:\nUse block wrappers for modals.', \
                 'review_standard', '[]', '[]', 'active', 'pr_review', '[\"src/modules/**/Modal*.tsx\"]')",
        )
        .execute(&pool)
        .await
        .expect("insert rule");

        disable_memory_rule(&pool, "rule-1", Some("too noisy"))
            .await
            .expect("disable");
        let report = run_memory_autopilot(&pool, MemoryAutopilotOptions::default())
            .await
            .expect("autopilot");

        assert!(report.auto_enabled.is_empty());
        assert_eq!(report.digest.counts.active_rules, 0);
        assert_eq!(report.digest.counts.pending_drafts, 0);
        assert_eq!(report.digest.counts.needs_review_groups, 0);
        assert!(report.digest.candidate_groups.is_empty());
        let status: String = sqlx::query_scalar("SELECT status FROM skills WHERE id = 'rule-1'")
            .fetch_one(&pool)
            .await
            .expect("status");
        assert_eq!(status, "disabled");
    }

    #[tokio::test]
    async fn imported_pr_review_drafts_wait_for_human_cleanup() {
        let pool = fresh_pool().await;
        sqlx::query(
            "INSERT INTO skills \
                (id, name, source, directory, version, description, type, engines, tags, status, origin, source_repo, file_patterns) \
             VALUES \
                ('draft-pr-1', 'Review: Do we really need to use unknown here', 'local', '', '1.0.0', \
                 'Rule:\nWhen touching `src/legacy/**/*.ts`, do we really need to use unknown here.\n\nSource evidence:\nSource: owner/repo#57\nComment: https://example.test/review\nFile: src/legacy/event.ts\n\nReviewer said:\nDo we really need to use unknown here?', \
                 'review_standard', '[]', '[]', 'pending', 'pr_review', 'owner/repo', '[\"src/legacy/**/*.ts\"]')",
        )
        .execute(&pool)
        .await
        .expect("insert pr draft");

        let report = run_memory_autopilot(&pool, MemoryAutopilotOptions::default())
            .await
            .expect("autopilot");

        assert!(report.auto_enabled.is_empty());
        assert_eq!(report.digest.counts.pending_drafts, 1);
        assert_eq!(report.digest.counts.auto_enable_groups, 0);
        assert_eq!(report.digest.counts.needs_review_groups, 1);
        assert!(
            report.digest.candidate_groups[0]
                .reason
                .contains("human rule cleanup")
        );
        let status: String =
            sqlx::query_scalar("SELECT status FROM skills WHERE id = 'draft-pr-1'")
                .fetch_one(&pool)
                .await
                .expect("status");
        assert_eq!(status, "pending");
    }

    #[tokio::test]
    async fn cleaned_pr_review_draft_auto_enables_without_ai_curator() {
        let pool = fresh_pool().await;
        sqlx::query(
            "INSERT INTO skills \
                (id, name, source, directory, version, description, type, engines, tags, status, origin, source_repo, file_patterns) \
             VALUES \
                ('draft-pr-clean', 'Use base media components', 'local', '', '1.0.0', \
                 'Rule:\nUse base media components for images and videos in UI code instead of raw media tags outside the base component implementations.\n\nSource evidence:\nSource: owner/repo#57\nComment: https://example.test/review\nFile: src/components/developers/Hero.tsx', \
                 'review_standard', '[]', '[]', 'pending', 'pr_review', 'owner/repo', '[\"src/components/developers/hero/**/*.tsx\"]')",
        )
        .execute(&pool)
        .await
        .expect("insert cleaned pr draft");

        let report = run_memory_autopilot(&pool, MemoryAutopilotOptions::default())
            .await
            .expect("autopilot");

        assert_eq!(report.auto_enabled.len(), 1);
        assert_eq!(
            report.auto_enabled[0].rule_id.as_deref(),
            Some("draft-pr-clean")
        );
        assert!(
            report.auto_enabled[0]
                .reason
                .contains("cleaned PR-review rule")
        );
        let status: String =
            sqlx::query_scalar("SELECT status FROM skills WHERE id = 'draft-pr-clean'")
                .fetch_one(&pool)
                .await
                .expect("status");
        assert_eq!(status, "active");
    }

    #[tokio::test]
    async fn cleaned_pr_review_draft_obvious_default_rule_waits_for_review() {
        let pool = fresh_pool().await;
        sqlx::query(
            "INSERT INTO skills \
                (id, name, source, directory, version, description, type, engines, tags, status, origin, source_repo, file_patterns) \
             VALUES \
                ('draft-pr-obvious', 'Use request validation in handlers', 'local', '', '1.0.0', \
                 'Rule:\nUse request validation when changing HTTP handlers so invalid payloads do not cause errors. This is the default safe handling expected for ordinary request parsing.\n\nSource evidence:\nSource: owner/repo#61\nComment: https://example.test/review\nFile: src/http/user.ts', \
                 'review_standard', '[]', '[]', 'pending', 'pr_review', 'owner/repo', '[\"src/http/**/*.ts\"]')",
        )
        .execute(&pool)
        .await
        .expect("insert obvious pr draft");

        let report = run_memory_autopilot(&pool, MemoryAutopilotOptions::default())
            .await
            .expect("autopilot");

        assert!(report.auto_enabled.is_empty());
        assert_eq!(report.digest.counts.pending_drafts, 1);
        assert_eq!(report.digest.counts.auto_enable_groups, 0);
        assert_eq!(report.digest.counts.needs_review_groups, 1);
        let status: String =
            sqlx::query_scalar("SELECT status FROM skills WHERE id = 'draft-pr-obvious'")
                .fetch_one(&pool)
                .await
                .expect("status");
        assert_eq!(status, "pending");
    }

    #[tokio::test]
    async fn fallback_titled_pr_review_draft_still_waits_for_cleanup() {
        let pool = fresh_pool().await;
        sqlx::query(
            "INSERT INTO skills \
                (id, name, source, directory, version, description, type, engines, tags, status, origin, source_repo, file_patterns) \
             VALUES \
                ('draft-pr-fallback-title', 'Review rule for src/components/Hero.tsx', 'local', '', '1.0.0', \
                 'Rule:\nUse base media components for images and videos in UI code instead of raw media tags outside the base component implementations.\n\nSource evidence:\nSource: owner/repo#58\nComment: https://example.test/review\nFile: src/components/Hero.tsx', \
                 'review_standard', '[]', '[]', 'pending', 'pr_review', 'owner/repo', '[\"src/components/Hero.tsx\"]')",
        )
        .execute(&pool)
        .await
        .expect("insert fallback-title pr draft");

        let report = run_memory_autopilot(&pool, MemoryAutopilotOptions::default())
            .await
            .expect("autopilot");

        assert!(report.auto_enabled.is_empty());
        assert_eq!(report.digest.counts.needs_review_groups, 1);
        assert!(
            report.digest.candidate_groups[0]
                .reason
                .contains("human rule cleanup")
        );
        let status: String =
            sqlx::query_scalar("SELECT status FROM skills WHERE id = 'draft-pr-fallback-title'")
                .fetch_one(&pool)
                .await
                .expect("status");
        assert_eq!(status, "pending");
    }

    #[tokio::test]
    async fn active_duplicate_drafts_are_skipped_without_ai_or_failure() {
        let pool = fresh_pool().await;
        sqlx::query(
            "INSERT INTO skills \
                (id, name, source, directory, version, description, type, engines, tags, status, origin, source_repo, file_patterns, content_hash) \
             VALUES \
                ('active-media', 'Use base media components', 'local', '', '1.0.0', \
                 'Rule:\nUse base media components for images and videos.', \
                 'review_standard', '[]', '[]', 'active', 'pr_review', 'owner/repo', '[\"**/*.tsx\"]', 'same-hash'), \
                ('draft-media', 'Use base media components', 'local', '', '1.0.0', \
                 'Rule:\nUse base media components for images and videos.\n\nSource evidence:\nSource: owner/repo#32\nComment: https://example.test/review', \
                 'review_standard', '[]', '[]', 'pending', 'pr_review', 'owner/repo', '[\"**/*.tsx\"]', 'same-hash')",
        )
        .execute(&pool)
        .await
        .expect("insert duplicate active/draft");

        let report = run_memory_autopilot(&pool, MemoryAutopilotOptions::default())
            .await
            .expect("autopilot should skip active duplicates");

        assert!(report.auto_enabled.is_empty());
        assert_eq!(report.digest.counts.auto_enable_groups, 0);
        assert_eq!(report.digest.counts.needs_review_groups, 0);
        assert_eq!(
            report.digest.candidate_groups[0].state,
            MemoryCandidateGroupState::AlreadyActive
        );
        assert!(
            report.digest.candidate_groups[0]
                .reason
                .contains("matching active rule")
        );
        let status: String =
            sqlx::query_scalar("SELECT status FROM skills WHERE id = 'draft-media'")
                .fetch_one(&pool)
                .await
                .expect("status");
        assert_eq!(status, "pending");
    }

    #[test]
    fn matching_session_candidates_still_auto_enable() {
        let candidates = (0..3)
            .map(|idx| PendingMemory {
                item_id: format!("session:{idx}"),
                kind: PendingMemoryKind::Session {
                    content_hash: format!("hash-{idx}"),
                },
                title: "Use ExternalLink for cross-deployment navigation".to_owned(),
                body:
                    "Use ExternalLink when navigating to routes served outside the TanStack router."
                        .to_owned(),
                raw_description: None,
                content_hash: None,
                origin: "session_mined".to_owned(),
                source_repo: Some("owner/repo".to_owned()),
                file_patterns: vec![
                    "src/constants/routes.ts".to_owned(),
                    "src/modules/ExternalLink.tsx".to_owned(),
                ],
                verdict: Some("KEEP".to_owned()),
                session_id: Some(format!("session-{idx}")),
                session_created_at_ms: Some(1_714_000_000_000 + i64::from(idx)),
                distinct_evidence_count: None,
                autopilot_disabled: false,
            })
            .collect::<Vec<_>>();

        let (state, reason, confidence) = classify_group(
            "owner/repo:generic-topic:test",
            &candidates,
            None,
            &candidates[0].file_patterns,
            &HashSet::new(),
            &HashSet::new(),
            &[],
        );

        assert_eq!(state, MemoryCandidateGroupState::AutoEnable);
        assert!(reason.contains("3 matching session-mined discoveries"));
        assert_eq!(confidence.as_deref(), Some(AUTOPILOT_CONFIDENCE));
    }

    #[test]
    fn pending_counts_exclude_already_active_groups() {
        let visible_session = PendingMemory {
            item_id: "session:visible".to_owned(),
            kind: PendingMemoryKind::Session {
                content_hash: "visible".to_owned(),
            },
            title: "Visible candidate".to_owned(),
            body: "Keep this visible session candidate pending.".to_owned(),
            raw_description: None,
            content_hash: None,
            origin: "session_mined".to_owned(),
            source_repo: Some("owner/repo".to_owned()),
            file_patterns: vec!["src/**/*.rs".to_owned()],
            verdict: Some("KEEP".to_owned()),
            session_id: Some("s1".to_owned()),
            session_created_at_ms: Some(1_714_000_000_000),
            distinct_evidence_count: None,
            autopilot_disabled: false,
        };
        let already_active_session = PendingMemory {
            item_id: "session:covered".to_owned(),
            kind: PendingMemoryKind::Session {
                content_hash: "covered".to_owned(),
            },
            title: "Covered candidate".to_owned(),
            body: "This session candidate is already covered by an active rule.".to_owned(),
            raw_description: None,
            content_hash: None,
            origin: "session_mined".to_owned(),
            source_repo: Some("owner/repo".to_owned()),
            file_patterns: vec!["src/**/*.rs".to_owned()],
            verdict: Some("KEEP".to_owned()),
            session_id: Some("s2".to_owned()),
            session_created_at_ms: Some(1_714_000_000_000),
            distinct_evidence_count: None,
            autopilot_disabled: false,
        };
        let visible_group = PlannedGroup {
            digest: MemoryCandidateGroup {
                group_id: "owner/repo:visible:src".to_owned(),
                title: visible_session.title.clone(),
                state: MemoryCandidateGroupState::Recommended,
                reason: "pending review".to_owned(),
                confidence: None,
                item_ids: vec![visible_session.item_id.clone()],
                source_repo: Some("owner/repo".to_owned()),
                file_patterns: visible_session.file_patterns.clone(),
                origins: vec![visible_session.origin.clone()],
                verdicts: vec!["KEEP".to_owned()],
                sample: visible_session.body.clone(),
            },
            candidates: vec![visible_session],
            conflict: None,
        };
        let already_active_group = PlannedGroup {
            digest: MemoryCandidateGroup {
                group_id: "owner/repo:covered:src".to_owned(),
                title: already_active_session.title.clone(),
                state: MemoryCandidateGroupState::AlreadyActive,
                reason: "matching active rule".to_owned(),
                confidence: None,
                item_ids: vec![already_active_session.item_id.clone()],
                source_repo: Some("owner/repo".to_owned()),
                file_patterns: already_active_session.file_patterns.clone(),
                origins: vec![already_active_session.origin.clone()],
                verdicts: vec!["KEEP".to_owned()],
                sample: already_active_session.body.clone(),
            },
            candidates: vec![already_active_session],
            conflict: None,
        };

        assert_eq!(
            count_pending_kind(&[visible_group, already_active_group], "session"),
            1
        );
    }

    #[test]
    fn sparse_session_candidates_are_recommended() {
        let candidates = vec![PendingMemory {
            item_id: "session:one".to_owned(),
            kind: PendingMemoryKind::Session {
                content_hash: "hash-one".to_owned(),
            },
            title: "Use ExternalLink for cross-deployment navigation".to_owned(),
            body: "Use ExternalLink when navigating to routes served outside the TanStack router."
                .to_owned(),
            raw_description: None,
            content_hash: None,
            origin: "session_mined".to_owned(),
            source_repo: Some("owner/repo".to_owned()),
            file_patterns: vec!["src/modules/ExternalLink.tsx".to_owned()],
            verdict: Some("KEEP".to_owned()),
            session_id: Some("session-one".to_owned()),
            session_created_at_ms: Some(1_714_000_000_000),
            distinct_evidence_count: None,
            autopilot_disabled: false,
        }];

        let (state, reason, confidence) = classify_group(
            "owner/repo:generic-topic:test",
            &candidates,
            None,
            &candidates[0].file_patterns,
            &HashSet::new(),
            &HashSet::new(),
            &[],
        );

        assert_eq!(state, MemoryCandidateGroupState::Recommended);
        assert!(reason.contains("review once before enabling"));
        assert_eq!(confidence.as_deref(), Some(RECOMMENDED_CONFIDENCE));
    }

    #[test]
    fn broad_patterns_are_not_auto_enable_safe() {
        assert!(file_patterns_are_broad(&["src/**/*.tsx".to_owned()]));
        assert!(!file_patterns_are_broad(&[
            "src/modules/**/Modal*.tsx".to_owned(),
            "src/**/*.css".to_owned(),
        ]));
    }

    #[test]
    fn disallow_alone_is_not_allow_disallow_conflict() {
        let candidates = vec![conflict_candidate(
            "owner/repo",
            "Disallow unwrap in request handlers.",
            vec!["src/**/*.rs"],
        )];

        assert!(!has_conflicting_language(&candidates));
    }

    #[test]
    fn explicit_allow_and_disallow_remains_conflicting_language() {
        let candidates = vec![
            conflict_candidate(
                "owner/repo",
                "Allow unwrap in generated fixtures.",
                vec!["tests/**/*.rs"],
            ),
            conflict_candidate(
                "owner/repo",
                "Disallow unwrap in request handlers.",
                vec!["src/**/*.rs"],
            ),
        ];

        assert!(has_conflicting_language(&candidates));
    }

    fn conflict_candidate(repo: &str, directive: &str, patterns: Vec<&str>) -> PendingMemory {
        PendingMemory {
            item_id: "session:c1".to_owned(),
            kind: PendingMemoryKind::Draft {
                id: "c1".to_owned(),
            },
            title: "Candidate guidance".to_owned(),
            body: directive.to_owned(),
            raw_description: None,
            content_hash: None,
            origin: "session".to_owned(),
            source_repo: Some(repo.to_owned()),
            file_patterns: patterns.into_iter().map(ToOwned::to_owned).collect(),
            verdict: None,
            session_id: None,
            session_created_at_ms: None,
            distinct_evidence_count: None,
            autopilot_disabled: false,
        }
    }

    fn conflict_active(repo: &str, body: &str, patterns: Vec<&str>) -> ActiveMemory {
        ActiveMemory {
            item_id: "rule:r-1".to_owned(),
            rule_id: "r-1".to_owned(),
            title: "Active rule".to_owned(),
            body: body.to_owned(),
            content_hash: None,
            origin: "pr_review".to_owned(),
            source_repo: Some(repo.to_owned()),
            file_patterns: patterns.into_iter().map(ToOwned::to_owned).collect(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn detect_active_conflict_flags_opposing_directive_in_same_scope() {
        let candidate = conflict_candidate(
            "owner/repo",
            "Always use unwrap in request handlers for brevity.",
            vec!["src/http/*.rs"],
        );
        let active = conflict_active(
            "owner/repo",
            "Never use unwrap in request handlers; return a structured error.",
            vec!["src/http/handler.rs"],
        );
        let conflict = detect_active_conflict(
            std::slice::from_ref(&candidate),
            Some("owner/repo"),
            &candidate.file_patterns,
            std::slice::from_ref(&active),
        )
        .expect("opposing always/never on a shared subject, same repo, shared extension");
        assert_eq!(conflict.rule_id, "r-1");
        assert!(
            conflict.basis.contains("unwrap"),
            "basis names the shared subject: {}",
            conflict.basis,
        );
    }

    #[test]
    fn detect_active_conflict_is_quiet_without_a_real_conflict() {
        let candidate = conflict_candidate(
            "owner/repo",
            "Always use unwrap in request handlers for brevity.",
            vec!["src/http/*.rs"],
        );
        let patterns = candidate.file_patterns.clone();
        let from = std::slice::from_ref(&candidate);

        // Same directive (no opposition).
        let agreeing = conflict_active(
            "owner/repo",
            "Always use unwrap in request handlers; it is fine here.",
            vec!["src/http/handler.rs"],
        );
        assert!(
            detect_active_conflict(
                from,
                Some("owner/repo"),
                &patterns,
                std::slice::from_ref(&agreeing)
            )
            .is_none(),
            "agreement is not a conflict",
        );

        // Opposing directive but a different repo.
        let other_repo = conflict_active(
            "other/repo",
            "Never use unwrap in request handlers.",
            vec!["src/http/handler.rs"],
        );
        assert!(
            detect_active_conflict(
                from,
                Some("owner/repo"),
                &patterns,
                std::slice::from_ref(&other_repo)
            )
            .is_none(),
            "conflicts only matter within the same repo",
        );

        // Opposing directive, same repo, but non-overlapping file patterns.
        let other_files = conflict_active(
            "owner/repo",
            "Never use unwrap in request handlers.",
            vec!["docs/guide.md"],
        );
        assert!(
            detect_active_conflict(
                from,
                Some("owner/repo"),
                &patterns,
                std::slice::from_ref(&other_files)
            )
            .is_none(),
            "rules over different files do not conflict",
        );
    }

    #[test]
    fn detect_active_conflict_ignores_low_information_shared_subjects() {
        let candidate = conflict_candidate(
            "owner/repo",
            "Use intermediate event codes to route upgrade flow analytics.",
            vec!["src/analytics/events.ts"],
        );
        let active = conflict_active(
            "owner/repo",
            "Do not use event names as PostHog flag keys; keep web_ prefixed constants.",
            vec!["src/**/*.ts"],
        );

        assert!(
            detect_active_conflict(
                std::slice::from_ref(&candidate),
                Some("owner/repo"),
                &candidate.file_patterns,
                std::slice::from_ref(&active),
            )
            .is_none(),
            "generic terms like event/flag should not create a false conflict",
        );
    }

    #[test]
    fn detect_active_conflict_requires_more_than_one_weak_shared_subject() {
        let candidate = conflict_candidate(
            "owner/repo",
            "Always use event handler.",
            vec!["src/routes/checkout.ts"],
        );
        let active = conflict_active(
            "owner/repo",
            "Never use event handler.",
            vec!["src/routes/**/*.ts"],
        );
        let conflict = detect_active_conflict(
            std::slice::from_ref(&candidate),
            Some("owner/repo"),
            &candidate.file_patterns,
            std::slice::from_ref(&active),
        )
        .expect("multiple weak shared subjects still form a real conflict");
        assert!(
            conflict.basis.contains("event") && conflict.basis.contains("handler"),
            "basis should preserve the shared subject phrase, got {}",
            conflict.basis,
        );
    }

    #[test]
    fn detect_active_conflict_catches_short_tech_subjects() {
        let candidate = conflict_candidate(
            "owner/repo",
            "Always use jwt claims for request identity.",
            vec!["src/auth/session.ts"],
        );
        let active = conflict_active(
            "owner/repo",
            "Never use jwt claims for request identity; read the signed session cookie.",
            vec!["src/auth/**/*.ts"],
        );
        let conflict = detect_active_conflict(
            std::slice::from_ref(&candidate),
            Some("owner/repo"),
            &candidate.file_patterns,
            std::slice::from_ref(&active),
        )
        .expect("short tech subjects like jwt should still form conflicts");
        assert_eq!(conflict.basis, "jwt");
    }

    #[test]
    fn detect_active_conflict_catches_global_active_rule() {
        // A repo-wide active rule (no file patterns) covers every file, so it
        // must be checked for conflict, not silently skipped as "no overlap".
        let candidate = conflict_candidate(
            "owner/repo",
            "Always use unwrap in request handlers for brevity.",
            vec!["src/http/*.rs"],
        );
        let global = conflict_active(
            "owner/repo",
            "Never use unwrap anywhere in this repo.",
            vec![],
        );
        assert!(
            detect_active_conflict(
                std::slice::from_ref(&candidate),
                Some("owner/repo"),
                &candidate.file_patterns,
                std::slice::from_ref(&global),
            )
            .is_some(),
            "a global (pattern-less) active rule must be checked for conflict",
        );
    }

    fn pr_review_draft_with_patterns(patterns: Vec<&str>) -> PendingMemory {
        PendingMemory {
            item_id: "draft:draft-pr-scope".to_owned(),
            kind: PendingMemoryKind::Draft {
                id: "draft-pr-scope".to_owned(),
            },
            title: "Raw media review".to_owned(),
            body: "Reviewer asked to avoid raw media tags in a specific hero component.".to_owned(),
            raw_description: Some(
                "Rule:\nAvoid raw media tags.\n\nSource evidence:\nSource: owner/repo#1\nComment: https://example.test/review\nFile: src/components/developers/Hero.tsx\n\nReviewer said:\nUse base media components."
                    .to_owned(),
            ),
            content_hash: None,
            origin: "pr_review".to_owned(),
            source_repo: Some("owner/repo".to_owned()),
            file_patterns: patterns.into_iter().map(ToOwned::to_owned).collect(),
            verdict: None,
            session_id: None,
            session_created_at_ms: None,
            distinct_evidence_count: None,
            autopilot_disabled: false,
        }
    }

    fn planned_pr_review_group(patterns: Vec<&str>) -> PlannedGroup {
        let candidate = pr_review_draft_with_patterns(patterns);
        let group_id = candidate_group_key(&candidate);
        let digest = digest_group(
            group_id,
            std::slice::from_ref(&candidate),
            &HashSet::new(),
            &HashSet::new(),
            &[],
        );
        PlannedGroup {
            digest,
            candidates: vec![candidate],
            conflict: None,
        }
    }

    fn high_confidence_curator_decision(
        group_id: String,
        scope: Option<MemoryCuratorScope>,
    ) -> MemoryCuratorDecision {
        MemoryCuratorDecision {
            group_id,
            action: MemoryCuratorAction::Enable,
            confidence: 0.96,
            title: Some("Use base media components".to_owned()),
            rule: Some(
                "Use base media components for images and videos in UI code instead of raw media tags outside the base component implementations."
                    .to_owned(),
            ),
            reason: Some("cross-file UI convention".to_owned()),
            scope,
        }
    }

    #[test]
    fn curator_language_wide_scope_updates_group_file_patterns() {
        let mut group = planned_pr_review_group(vec![
            "src/components/developers/hero/**/*.tsx",
            "**/package.json",
        ]);
        let decision = high_confidence_curator_decision(
            group.digest.group_id.clone(),
            Some(MemoryCuratorScope::LanguageWide),
        );

        apply_curator_decision(&mut group, &decision, MemoryCuratorOptions::default());

        assert_eq!(group.digest.state, MemoryCandidateGroupState::AutoEnable);
        assert_eq!(group.digest.file_patterns, vec!["**/*.tsx"]);
        assert_eq!(group.candidates[0].file_patterns, vec!["**/*.tsx"]);
    }

    #[test]
    fn curator_path_scoped_scope_keeps_existing_file_patterns() {
        let mut group = planned_pr_review_group(vec!["src/components/developers/hero/**/*.tsx"]);
        let original_patterns = group.digest.file_patterns.clone();
        let decision = high_confidence_curator_decision(
            group.digest.group_id.clone(),
            Some(MemoryCuratorScope::PathScoped),
        );

        apply_curator_decision(&mut group, &decision, MemoryCuratorOptions::default());

        assert_eq!(group.digest.state, MemoryCandidateGroupState::AutoEnable);
        assert_eq!(group.digest.file_patterns, original_patterns);
        assert_eq!(group.candidates[0].file_patterns, original_patterns);
    }

    #[test]
    fn curator_medium_confidence_updates_group_as_recommended() {
        let mut group = planned_pr_review_group(vec!["src/components/developers/hero/**/*.tsx"]);
        let mut decision = high_confidence_curator_decision(
            group.digest.group_id.clone(),
            Some(MemoryCuratorScope::PathScoped),
        );
        decision.confidence = 0.74;

        apply_curator_decision(&mut group, &decision, MemoryCuratorOptions::default());

        assert_eq!(group.digest.state, MemoryCandidateGroupState::Recommended);
        assert!(
            group
                .digest
                .reason
                .contains("local memory curator recommends")
        );
        assert_eq!(group.digest.confidence.as_deref(), Some("0.74"));
    }

    #[test]
    fn curator_default_threshold_auto_enables_at_observed_high_confidence() {
        let mut group = planned_pr_review_group(vec!["src/components/developers/hero/**/*.tsx"]);
        let mut decision = high_confidence_curator_decision(
            group.digest.group_id.clone(),
            Some(MemoryCuratorScope::PathScoped),
        );
        decision.confidence = crate::memory_curator::DEFAULT_CURATOR_MIN_CONFIDENCE;

        apply_curator_decision(&mut group, &decision, MemoryCuratorOptions::default());

        assert_eq!(group.digest.state, MemoryCandidateGroupState::AutoEnable);
        assert_eq!(
            group.digest.confidence.as_deref(),
            Some(AUTOPILOT_CONFIDENCE)
        );
    }

    #[tokio::test]
    async fn cached_curator_recommendation_is_reused_by_digest() {
        let pool = fresh_pool().await;
        sqlx::query(
            "INSERT INTO skills \
                (id, name, source, directory, version, description, type, engines, tags, status, origin, source_repo, file_patterns) \
             VALUES \
                ('draft-pr-scope', 'Raw media review', 'local', '', '1.0.0', \
                 'Rule:\nAvoid raw media tags.\n\nSource evidence:\nSource: owner/repo#1\nComment: https://example.test/review\nFile: src/components/developers/Hero.tsx\n\nReviewer said:\nUse base media components.', \
                 'review_standard', '[]', '[]', 'pending', 'pr_review', 'owner/repo', '[\"src/components/developers/hero/**/*.tsx\"]')",
        )
        .execute(&pool)
        .await
        .expect("insert pr draft");
        let draft = list_candidates(&pool, None, Some(1))
            .await
            .expect("load candidate")
            .into_iter()
            .next()
            .expect("candidate");
        let candidate = pending_from_draft(draft, &HashSet::new());
        let group_id = candidate_group_key(&candidate);
        let digest = digest_group(
            group_id,
            std::slice::from_ref(&candidate),
            &HashSet::new(),
            &HashSet::new(),
            &[],
        );
        let mut group = PlannedGroup {
            digest,
            candidates: vec![candidate],
            conflict: None,
        };
        let input_hash = group_input_hash(&group);
        let mut decision = high_confidence_curator_decision(
            group.digest.group_id.clone(),
            Some(MemoryCuratorScope::PathScoped),
        );
        decision.confidence = 0.74;
        apply_curator_decision(&mut group, &decision, MemoryCuratorOptions::default());
        upsert_curator_recommendation(&pool, &group, &input_hash)
            .await
            .expect("cache recommendation");

        let digest = load_memory_digest(&pool, 20).await.expect("digest");

        assert_eq!(digest.counts.recommended_groups, 1);
        assert_eq!(
            digest.candidate_groups[0].state,
            MemoryCandidateGroupState::Recommended
        );
        assert_eq!(
            digest.candidate_groups[0].title,
            "Use base media components"
        );
    }

    #[tokio::test]
    async fn manual_approve_uses_cached_curator_recommendation() {
        let pool = fresh_pool().await;
        let raw_description = "Rule:\nAvoid raw media tags.\n\nSource evidence:\nSource: owner/repo#1\nComment: https://example.test/review\nFile: src/components/developers/Hero.tsx\n\nReviewer said:\nUse base media components.";
        sqlx::query(
            "INSERT INTO skills \
                (id, name, source, directory, version, description, type, engines, tags, status, origin, source_repo, file_patterns) \
             VALUES \
                ('draft-pr-scope', 'Raw media review', 'local', '', '1.0.0', ?1, \
                 'review_standard', '[]', '[]', 'pending', 'pr_review', 'owner/repo', '[\"src/components/developers/hero/**/*.tsx\",\"**/package.json\"]')",
        )
        .bind(raw_description)
        .execute(&pool)
        .await
        .expect("insert pr draft");
        let draft = list_candidates(&pool, None, Some(1))
            .await
            .expect("load candidate")
            .into_iter()
            .next()
            .expect("candidate");
        let candidate = pending_from_draft(draft, &HashSet::new());
        let group_id = candidate_group_key(&candidate);
        let digest = digest_group(
            group_id,
            std::slice::from_ref(&candidate),
            &HashSet::new(),
            &HashSet::new(),
            &[],
        );
        let mut group = PlannedGroup {
            digest,
            candidates: vec![candidate],
            conflict: None,
        };
        let input_hash = group_input_hash(&group);
        let mut decision = high_confidence_curator_decision(
            group.digest.group_id.clone(),
            Some(MemoryCuratorScope::LanguageWide),
        );
        decision.confidence = 0.74;
        apply_curator_decision(&mut group, &decision, MemoryCuratorOptions::default());
        upsert_curator_recommendation(&pool, &group, &input_hash)
            .await
            .expect("cache recommendation");

        let rule = promote_candidate_with_curator_recommendation(&pool, "draft-pr-scope")
            .await
            .expect("promote");

        assert_eq!(rule.id, "draft-pr-scope");
        let row = sqlx::query(
            "SELECT status, name, description, file_patterns FROM skills WHERE id = 'draft-pr-scope'",
        )
        .fetch_one(&pool)
        .await
        .expect("load promoted rule");
        let status: String = row.try_get("status").expect("status");
        let name: String = row.try_get("name").expect("name");
        let description: String = row.try_get("description").expect("description");
        let file_patterns_raw: Option<String> = row.try_get("file_patterns").expect("patterns");
        let file_patterns: Vec<String> =
            serde_json::from_str(file_patterns_raw.as_deref().unwrap_or("[]")).expect("json");

        assert_eq!(status, "active");
        assert_eq!(name, "Use base media components");
        assert!(description.contains("Use base media components for images and videos"));
        assert!(description.contains("Source evidence:"));
        assert_eq!(file_patterns, vec!["**/*.tsx"]);
    }

    #[tokio::test]
    async fn enable_group_persists_refined_file_patterns_for_draft() {
        let pool = fresh_pool().await;
        let raw_description = "Rule:\nAvoid raw media tags.\n\nSource evidence:\nSource: owner/repo#1\nComment: https://example.test/review\nFile: src/components/developers/Hero.tsx\n\nReviewer said:\nUse base media components.";
        sqlx::query(
            "INSERT INTO skills \
                (id, name, source, directory, version, description, type, engines, tags, status, origin, source_repo, file_patterns) \
             VALUES \
                ('draft-pr-scope', 'Raw media review', 'local', '', '1.0.0', ?1, \
                 'review_standard', '[]', '[]', 'pending', 'pr_review', 'owner/repo', '[\"src/components/developers/hero/**/*.tsx\",\"**/package.json\"]')",
        )
        .bind(raw_description)
        .execute(&pool)
        .await
        .expect("insert pr draft");
        let mut candidate = pr_review_draft_with_patterns(vec!["**/*.tsx"]);
        candidate.title = "Use base media components".to_owned();
        candidate.body = "Use base media components for images and videos in UI code instead of raw media tags outside the base component implementations.".to_owned();
        candidate.raw_description = Some(raw_description.to_owned());

        let rule = enable_group(&pool, &[candidate]).await.expect("enable");

        assert_eq!(rule.id, "draft-pr-scope");
        let row = sqlx::query(
            "SELECT status, name, description, file_patterns FROM skills WHERE id = 'draft-pr-scope'",
        )
        .fetch_one(&pool)
        .await
        .expect("load promoted rule");
        let status: String = row.try_get("status").expect("status");
        let name: String = row.try_get("name").expect("name");
        let description: String = row.try_get("description").expect("description");
        let file_patterns_raw: Option<String> = row.try_get("file_patterns").expect("patterns");
        let file_patterns: Vec<String> =
            serde_json::from_str(file_patterns_raw.as_deref().unwrap_or("[]")).expect("json");

        assert_eq!(status, "active");
        assert_eq!(name, "Use base media components");
        assert!(description.contains("Use base media components for images and videos"));
        assert_eq!(file_patterns, vec!["**/*.tsx"]);
    }

    /// Seed an active rule and an opposing pending draft in the same repo/scope
    /// so the deterministic conflict detector fires during `build_plan`.
    async fn seed_conflicting_active_and_candidate(pool: &SqlitePool) {
        sqlx::query(
            "INSERT INTO skills \
                (id, name, source, directory, version, description, type, engines, tags, status, origin, source_repo, file_patterns) \
             VALUES \
                ('active-unwrap', 'Unwrap policy', 'local', '', '1.0.0', \
                 'Never use unwrap in request handlers; return a structured error.', \
                 'review_standard', '[]', '[]', 'active', 'pr_review', 'owner/repo', '[\"src/http/handler.rs\"]')",
        )
        .execute(pool)
        .await
        .expect("insert active rule");
        sqlx::query(
            "INSERT INTO skills \
                (id, name, source, directory, version, description, type, engines, tags, status, origin, source_repo, file_patterns) \
             VALUES \
                ('draft-unwrap', 'Unwrap usage', 'local', '', '1.0.0', \
                 'Always use unwrap in request handlers for brevity.', \
                 'review_standard', '[]', '[]', 'pending', 'manual', 'owner/repo', '[\"src/http/edit.rs\"]')",
        )
        .execute(pool)
        .await
        .expect("insert opposing draft");
    }

    #[tokio::test]
    async fn autopilot_persists_detected_conflict_with_snapshots() {
        let pool = fresh_pool().await;
        seed_conflicting_active_and_candidate(&pool).await;

        run_memory_autopilot(&pool, MemoryAutopilotOptions::default())
            .await
            .expect("autopilot");

        let report = load_memory_conflicts(&pool, MemoryConflictFilter::default())
            .await
            .expect("load conflicts");
        assert_eq!(report.conflicts.len(), 1, "exactly one conflict persisted");
        let conflict = &report.conflicts[0];
        assert_eq!(conflict.active_rule_id, "active-unwrap");
        assert_eq!(conflict.candidate_rule_id.as_deref(), Some("draft-unwrap"));
        assert_eq!(conflict.source_repo.as_deref(), Some("owner/repo"));
        assert_eq!(conflict.overlap_basis, "unwrap");
        assert_eq!(conflict.status, "detected");
        // Snapshots captured for auditability.
        assert!(conflict.active_body.contains("Never use unwrap"));
        assert!(conflict.candidate_body.contains("Always use unwrap"));
        assert_eq!(conflict.active_title, "Unwrap policy");
        assert_eq!(conflict.candidate_patterns, vec!["src/http/edit.rs"]);
        assert_eq!(conflict.active_patterns, vec!["src/http/handler.rs"]);
        assert!(!conflict.evidence_hash.is_empty());
    }

    #[tokio::test]
    async fn autopilot_conflict_persistence_is_idempotent() {
        let pool = fresh_pool().await;
        seed_conflicting_active_and_candidate(&pool).await;

        for _ in 0..2 {
            run_memory_autopilot(&pool, MemoryAutopilotOptions::default())
                .await
                .expect("autopilot");
        }

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM memory_conflicts")
            .fetch_one(&pool)
            .await
            .expect("count");
        assert_eq!(count, 1, "re-running autopilot must not duplicate the row");
    }

    #[tokio::test]
    async fn autopilot_preserves_non_detected_conflict_status() {
        let pool = fresh_pool().await;
        seed_conflicting_active_and_candidate(&pool).await;

        run_memory_autopilot(&pool, MemoryAutopilotOptions::default())
            .await
            .expect("first autopilot");
        sqlx::query("UPDATE memory_conflicts SET status = 'confirmed'")
            .execute(&pool)
            .await
            .expect("confirm conflict");

        run_memory_autopilot(&pool, MemoryAutopilotOptions::default())
            .await
            .expect("second autopilot");

        let status: String = sqlx::query_scalar("SELECT status FROM memory_conflicts")
            .fetch_one(&pool)
            .await
            .expect("status");
        assert_eq!(status, "confirmed", "re-run must not reset to detected");
    }

    #[tokio::test]
    async fn load_memory_conflicts_respects_status_filter() {
        let pool = fresh_pool().await;
        seed_conflicting_active_and_candidate(&pool).await;
        run_memory_autopilot(&pool, MemoryAutopilotOptions::default())
            .await
            .expect("autopilot");

        let detected = load_memory_conflicts(
            &pool,
            MemoryConflictFilter {
                limit: None,
                status: Some("detected".to_owned()),
            },
        )
        .await
        .expect("detected conflicts");
        assert_eq!(detected.conflicts.len(), 1);
        assert_eq!(detected.schema_version, MEMORY_AUTOPILOT_SCHEMA_VERSION);

        let dismissed = load_memory_conflicts(
            &pool,
            MemoryConflictFilter {
                limit: None,
                status: Some("dismissed".to_owned()),
            },
        )
        .await
        .expect("dismissed conflicts");
        assert!(
            dismissed.conflicts.is_empty(),
            "no dismissed conflicts exist yet"
        );
    }

    #[tokio::test]
    async fn load_memory_digest_does_not_persist_conflicts() {
        let pool = fresh_pool().await;
        seed_conflicting_active_and_candidate(&pool).await;

        load_memory_digest(&pool, 20).await.expect("digest");

        let report = load_memory_conflicts(&pool, MemoryConflictFilter::default())
            .await
            .expect("load conflicts");
        assert!(
            report.conflicts.is_empty(),
            "digest is read-only and must not write conflict records"
        );
    }

    #[test]
    fn judge_parses_contradicts_verdict_from_code_fence() {
        // contradicts -> confirmed; rationale captured.
        let raw = r#"```json
        {"decisions":[{"conflictId":"sha256:abc","verdict":"contradicts","confidence":0.91,"rationale":"Both rules govern unwrap in handlers but give opposite instructions."}]}
        ```"#;

        let decisions = parse_judge_decisions(raw).expect("parse");

        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].conflict_id, "sha256:abc");
        assert_eq!(decisions[0].verdict, JudgeVerdict::Contradicts);
        assert!((decisions[0].confidence - 0.91).abs() < 1e-6);
        assert_eq!(
            judge_status_for(decisions[0].verdict, decisions[0].confidence),
            Some("confirmed")
        );
    }

    #[test]
    fn judge_parses_compatible_verdict_and_dismisses_when_confident() {
        // compatible (high confidence) -> dismissed.
        let raw = r#"
        {"decisions":[{"conflictId":"sha256:def","verdict":"compatible","confidence":0.88,"rationale":"The rules cover different files, so both can hold."}]}
        "#;

        let decisions = parse_judge_decisions(raw).expect("parse");

        assert_eq!(decisions[0].verdict, JudgeVerdict::Compatible);
        assert_eq!(
            judge_status_for(decisions[0].verdict, decisions[0].confidence),
            Some("dismissed")
        );

        // A low-confidence `compatible` is NOT authoritative enough to retire a
        // true conflict: it must leave the row `detected`.
        assert_eq!(judge_status_for(JudgeVerdict::Compatible, 0.50), None);
    }

    #[test]
    fn judge_out_of_range_confidence_is_rejected_not_clamped() {
        // A poisoned `1e9` confidence must NOT become max confidence (which would
        // clear the dismiss gate and silently retire a conflict). It maps to 0.0,
        // so a `compatible` verdict at that confidence leaves the row `detected`.
        let raw = r#"
        {"decisions":[
          {"conflictId":"hi","verdict":"compatible","confidence":1000000000.0,"rationale":"poisoned"},
          {"conflictId":"neg","verdict":"compatible","confidence":-3.0,"rationale":"negative"},
          {"conflictId":"ok","verdict":"compatible","confidence":0.83,"rationale":"in range"}
        ]}
        "#;
        let decisions = parse_judge_decisions(raw).expect("parse");
        let conf = |id: &str| {
            decisions
                .iter()
                .find(|decision| decision.conflict_id == id)
                .expect("decision")
                .confidence
        };
        assert!(
            conf("hi").abs() < 1e-6,
            "out-of-range high must reject to 0, not clamp to 1"
        );
        assert!(conf("neg").abs() < 1e-6, "negative must reject to 0");
        assert!((conf("ok") - 0.83).abs() < 1e-6, "in-range value preserved");
        assert_eq!(
            judge_status_for(JudgeVerdict::Compatible, conf("hi")),
            None,
            "rejected confidence must not dismiss a true conflict"
        );
    }

    #[test]
    fn judge_drops_unknown_verdicts() {
        let raw = r#"
        {"decisions":[
          {"conflictId":"a","verdict":"maybe","confidence":0.99},
          {"conflictId":"","verdict":"contradicts","confidence":0.99},
          {"conflictId":"b","verdict":"contradicts","confidence":0.95}
        ]}
        "#;
        let decisions = parse_judge_decisions(raw).expect("parse");
        assert_eq!(
            decisions.len(),
            1,
            "unknown verdict and empty conflictId are dropped"
        );
        assert_eq!(decisions[0].conflict_id, "b");
    }

    #[tokio::test]
    async fn update_conflict_judge_verdict_only_touches_detected_rows() {
        // The status guard keeps the judge non-authoritative: a human verdict
        // that landed before the judge ran must never be overwritten.
        let pool = fresh_pool().await;
        seed_conflicting_active_and_candidate(&pool).await;
        run_memory_autopilot(&pool, MemoryAutopilotOptions::default())
            .await
            .expect("autopilot");
        let hash: String = sqlx::query_scalar("SELECT evidence_hash FROM memory_conflicts")
            .fetch_one(&pool)
            .await
            .expect("hash");
        sqlx::query("UPDATE memory_conflicts SET status = 'confirmed'")
            .execute(&pool)
            .await
            .expect("human confirm");

        // Judge tries to dismiss, but the row is no longer `detected`.
        update_conflict_judge_verdict(&pool, &hash, "dismissed", Some("compatible"), 0.95)
            .await
            .expect("update");

        let status: String = sqlx::query_scalar("SELECT status FROM memory_conflicts")
            .fetch_one(&pool)
            .await
            .expect("status");
        assert_eq!(
            status, "confirmed",
            "judge must not overwrite a human verdict"
        );
    }

    #[tokio::test]
    async fn unavailable_judge_leaves_conflict_detected() {
        // When the local judge cannot run (the live call is `cfg!(test)`-skipped
        // inside `refine_pr_review_groups_with_local_ai`, the same outcome as an
        // unavailable / parse-failed model), the persisted conflict must remain
        // at its deterministic `detected` status — never silently confirmed or
        // dismissed.
        let pool = fresh_pool().await;
        seed_conflicting_active_and_candidate(&pool).await;

        run_memory_autopilot(&pool, MemoryAutopilotOptions::default())
            .await
            .expect("autopilot");

        let report = load_memory_conflicts(&pool, MemoryConflictFilter::default())
            .await
            .expect("load conflicts");
        assert_eq!(report.conflicts.len(), 1);
        assert_eq!(
            report.conflicts[0].status, "detected",
            "no judge verdict -> deterministic detected status preserved"
        );
        assert!(
            report.conflicts[0].llm_rationale.is_none(),
            "no rationale recorded when the judge did not run"
        );
        assert!(
            report.conflicts[0].llm_confidence.is_none(),
            "no confidence recorded when the judge did not run"
        );
    }
}
