//! Context engine record types.

use crate::domain::models::SkillRecord;

/// A past review verdict recalled from the cloud review-memory store.
///
/// Produced by `cloud::client::recall_past_verdicts` / surfaced by
/// `context::retrieval::retrieve_past_verdicts_by_text` and injected into the
/// review prompt by `review::build_segmented_prompt`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PastVerdict {
    pub extraction_id: String,
    pub code_snippet: String,
    pub issue_text: String,
    /// "approved" | "rejected" (mirrors the cloud extraction status).
    pub status: String,
    pub reason: Option<String>,
    pub similarity: f32,
    pub created_at: String,
    /// Canonical fix signature (SHA1 hex) computed from the code snippet.
    /// `None` for verdicts recalled from cloud endpoints that pre-date the
    /// signature field — callers must tolerate absence gracefully.
    #[serde(default)]
    pub signature: Option<String>,
    /// Source pull request that produced this recalled verdict, when the
    /// cloud can trace it. Lets agent-facing MCP output cite the exact
    /// review event, not just the source repository.
    #[serde(default)]
    pub source_pr_number: Option<i64>,
    #[serde(default)]
    pub source_pr_title: Option<String>,
    #[serde(default)]
    pub source_pr_url: Option<String>,
}

/// Shared evidence classification used by both rule match explanations and
/// rule timeline rows.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum EvidenceKind {
    FilePatternMatch,
    RetrievalMatch,
    SemanticSimilarity,
    PastVerdictRecall,
    RuleCreated,
    RuleUpdated,
    RuleExample,
    TriggerMatch,
}

/// Compact evidence record shared across explanation surfaces.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvidenceRecord {
    pub kind: EvidenceKind,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_value: Option<String>,
}

impl EvidenceRecord {
    pub fn new(kind: EvidenceKind, reason: impl Into<String>) -> Self {
        Self {
            kind,
            reason: reason.into(),
            source: None,
            ts: None,
            score: None,
            target: None,
            matched_value: None,
        }
    }

    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = Some(source.into());
        self
    }

    pub fn with_ts(mut self, ts: impl Into<String>) -> Self {
        self.ts = Some(ts.into());
        self
    }

    pub const fn with_score(mut self, score: f64) -> Self {
        self.score = Some(score);
        self
    }

    pub fn with_target(mut self, target: impl Into<String>) -> Self {
        self.target = Some(target.into());
        self
    }

    pub fn with_matched_value(mut self, matched_value: impl Into<String>) -> Self {
        self.matched_value = Some(matched_value.into());
        self
    }
}

/// Explanation surface for a rule match surfaced via MCP.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleMatchEvidenceRecord {
    pub id: String,
    pub title: String,
    pub origin: String,
    pub confidence: f64,
    pub similarity: f64,
    #[serde(rename = "file_patterns")]
    pub file_patterns: Vec<String>,
    pub preview: String,
    /// Source repo this rule was learned from (e.g. "gin-gonic/gin").
    /// `None` for manual / globally-scoped rules. Lets the agent surface
    /// the same "<- learned from <repo>" provenance the user sees in
    /// `review`, `recall`, the cloud rule-detail page, etc.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_repo: Option<String>,
    /// Number of times the cloud value loop observed this rule cited by a
    /// generated edit. Best-effort remote enrichment; omitted when unavailable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cited_count: Option<i64>,
    /// accepted_count / cited_count for the rule, as computed by the cloud
    /// impact endpoint. Best-effort remote enrichment; omitted when unavailable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trust_rate: Option<f64>,
    /// Compact whyRanked explanation of this result's serve position
    /// (`path-hint; band 9/10; source manual`): path-hint match,
    /// 10%-band relative score, and source priority — the same facts the
    /// deterministic serve arbitration sorted on. Omitted when arbitration
    /// metadata was unavailable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<EvidenceRecord>,
}

/// Explanation surface for a rule timeline row surfaced via MCP.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleTimelineEventRecord {
    pub id: String,
    pub ts: String,
    pub kind: String,
    pub source: String,
    pub preview: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<EvidenceRecord>,
}

/// Scope requested from the cloud when recalling past verdicts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PastVerdictScope {
    Personal,
    Team,
}

impl PastVerdictScope {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Personal => "personal",
            Self::Team => "team",
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextSourceItemRecord {
    pub source_type: String,
    pub source_id: String,
    pub relative_path: Option<String>,
    pub start_line: Option<i64>,
    pub end_line: Option<i64>,
    pub title: Option<String>,
    pub content: String,
    pub score: f64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextPackSectionsRecord {
    pub introduction: String,
    pub rules: Option<String>,
    pub review: Option<String>,
    pub closing: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextPackMetadataRecord {
    pub rule_count: i64,
    pub review_count: i64,
    pub review_reason: Option<String>,
    pub review_source_summary: Option<String>,
    pub selected_review_count: Option<i64>,
    pub recent_run_hint: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextPackRecord {
    pub task_intent: String,
    pub project_id: String,
    pub engine: String,
    pub query: String,
    pub rule_context: Vec<ContextSourceItemRecord>,
    pub review_context: Vec<ContextSourceItemRecord>,
    pub sections: ContextPackSectionsRecord,
    pub token_budget: i64,
    pub estimated_tokens: i64,
    pub trace_id: String,
    pub prompt_text: Option<String>,
    pub metadata: ContextPackMetadataRecord,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextDebugMetadataRecord {
    pub rule_count: i64,
    pub review_count: i64,
    pub reason: Option<String>,
    pub review_reason: Option<String>,
    pub review_source_summary: Option<String>,
    pub selected_review_count: Option<i64>,
    pub recent_run_hint: Option<String>,
    pub retrieval_mode: Option<String>,
    pub rerank_strategy: Option<String>,
    pub user_action_type: Option<String>,
    pub selected_rule_count: Option<i64>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextDebugResult {
    pub project_id: String,
    pub query: String,
    pub engine: String,
    pub status: String,
    pub rule_candidates: Vec<ContextSourceItemRecord>,
    pub review_candidates: Vec<ContextSourceItemRecord>,
    pub trace_id: String,
    pub estimated_tokens: i64,
    pub metadata: ContextDebugMetadataRecord,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextIndexStatusRecord {
    pub project_id: String,
    pub status: String,
    pub rule_chunk_count: i64,
    pub last_indexed_at: Option<String>,
    pub active_job_id: Option<String>,
    pub error: Option<String>,
    pub reason: Option<String>,
}

/// Build a compact provenance trail for a rule record as surfaced in local
/// and cloud memory detail views.
pub fn rule_provenance_evidence(rule: &SkillRecord) -> Vec<EvidenceRecord> {
    let mut evidence = vec![
        EvidenceRecord::new(
            EvidenceKind::RuleCreated,
            format!("Captured from {} on {}", rule.origin, rule.installed_at),
        )
        .with_source(rule.source.clone())
        .with_ts(rule.installed_at.clone())
        .with_matched_value(rule.directory.clone()),
    ];

    if rule.updated_at != rule.installed_at {
        evidence.push(
            EvidenceRecord::new(
                EvidenceKind::RuleUpdated,
                format!("Last updated on {}", rule.updated_at),
            )
            .with_source(rule.source.clone())
            .with_ts(rule.updated_at.clone())
            .with_matched_value(rule.name.clone()),
        );
    }

    if let Some(trigger) = &rule.trigger
        && !trigger.trim().is_empty()
    {
        evidence.push(
            EvidenceRecord::new(
                EvidenceKind::TriggerMatch,
                format!("Trigger text: {}", trigger.trim()),
            )
            .with_source(rule.source.clone())
            .with_matched_value(trigger.trim().to_owned()),
        );
    }

    evidence
}
