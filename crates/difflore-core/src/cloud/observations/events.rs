use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "event_type", rename_all = "snake_case")]
pub enum ObservationEvent {
    RuleFired {
        rule_ids: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        file_path: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        intent: Option<String>,
        session_id: String,
        fired_at: DateTime<Utc>,
    },
    McpRuleServed {
        tool: String,
        session_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        repo_full_name: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        file_path: Option<String>,
        query_hash: String,
        rule_ids: Vec<String>,
        top_k: i64,
        was_empty: bool,
        strict_match_count: i64,
        estimated_tokens: i64,
        served_at: DateTime<Utc>,
    },
    RuleCitedInEdit {
        rule_id: String,
        session_id: String,
        file_path: String,
        diff_excerpt: String,
        cited_at: DateTime<Utc>,
    },
    RuleActuallyCited {
        rule_id: String,
        session_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        file_path: Option<String>,
        citation_excerpt: String,
        cited_at: DateTime<Utc>,
    },
    FixOutcome {
        rule_id: String,
        session_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        file_path: Option<String>,
        accepted: bool,
        occurred_at: DateTime<Utc>,
        // Outbox row ids of mcp_rule_served events that surfaced this rule
        // shortly before the accepted edit for the same scope. Lets the
        // `acceptedOutcomesLinkedToMcpRuleServe` cross-link fire even when
        // session_id/file_path heuristics miss. Older rows deserialize as
        // `Vec::new()`.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        mcp_serve_event_ids: Vec<i64>,
    },
}

impl ObservationEvent {
    pub const fn event_type(&self) -> &'static str {
        match self {
            Self::RuleFired { .. } => "rule_fired",
            Self::McpRuleServed { .. } => "mcp_rule_served",
            Self::RuleCitedInEdit { .. } => "rule_cited_in_edit",
            Self::RuleActuallyCited { .. } => "rule_actually_cited",
            Self::FixOutcome { .. } => "fix_outcome",
        }
    }

    pub fn session_id(&self) -> &str {
        match self {
            Self::RuleFired { session_id, .. }
            | Self::McpRuleServed { session_id, .. }
            | Self::RuleCitedInEdit { session_id, .. }
            | Self::RuleActuallyCited { session_id, .. }
            | Self::FixOutcome { session_id, .. } => session_id,
        }
    }

    pub(super) fn file_path(&self) -> Option<&str> {
        match self {
            Self::RuleFired { file_path, .. }
            | Self::McpRuleServed { file_path, .. }
            | Self::RuleActuallyCited { file_path, .. }
            | Self::FixOutcome { file_path, .. } => file_path.as_deref(),
            Self::RuleCitedInEdit { file_path, .. } => Some(file_path),
        }
    }

    pub(super) fn rule_id(&self) -> Option<&str> {
        match self {
            Self::RuleCitedInEdit { rule_id, .. }
            | Self::RuleActuallyCited { rule_id, .. }
            | Self::FixOutcome { rule_id, .. } => Some(rule_id),
            Self::RuleFired { .. } | Self::McpRuleServed { .. } => None,
        }
    }

    pub(super) fn rule_ids(&self) -> Vec<String> {
        match self {
            Self::RuleFired { rule_ids, .. } | Self::McpRuleServed { rule_ids, .. } => {
                rule_ids.clone()
            }
            Self::RuleCitedInEdit { rule_id, .. }
            | Self::RuleActuallyCited { rule_id, .. }
            | Self::FixOutcome { rule_id, .. } => vec![rule_id.clone()],
        }
    }

    pub(super) const fn occurred_at_ms(&self) -> i64 {
        match self {
            Self::RuleFired { fired_at, .. } => fired_at.timestamp_millis(),
            Self::McpRuleServed { served_at, .. } => served_at.timestamp_millis(),
            Self::RuleCitedInEdit { cited_at, .. } | Self::RuleActuallyCited { cited_at, .. } => {
                cited_at.timestamp_millis()
            }
            Self::FixOutcome { occurred_at, .. } => occurred_at.timestamp_millis(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CitedEdit {
    pub rule_id: String,
    pub file_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleFireSnapshot {
    pub rule_ids: Vec<String>,
    pub file_path: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActualCitationSummary {
    pub actual_citations: i64,
    pub rule_fires: i64,
    pub pending_uploads: i64,
    pub pending_upload_issue: Option<ObservationUploadIssue>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AcceptedRecallLinkSummary {
    pub accepted_outcomes: i64,
    pub linked_to_prior_recall: i64,
    pub linked_to_rule_recall: i64,
    pub linked_to_mcp_rule_serve: i64,
    pub linked_to_edit_attribution: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AcceptedFixOutcomeRuleSummary {
    pub rule_id: String,
    pub accepted_outcomes: i64,
    pub linked_to_prior_recall: i64,
    pub linked_to_rule_recall: i64,
    pub linked_to_mcp_rule_serve: i64,
    pub linked_to_edit_attribution: i64,
    pub sample_file: Option<String>,
    pub latest_occurred_at_ms: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct PriorRuleUseLinks {
    pub rule_recall: bool,
    pub mcp_rule_serve: bool,
    pub edit_attribution: bool,
}

impl PriorRuleUseLinks {
    pub(super) const fn any(&self) -> bool {
        self.rule_recall || self.mcp_rule_serve || self.edit_attribution
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationUploadIssue {
    MissingCloudScope,
    RateLimited,
    InvalidBatch,
    ServerRejected,
    Unknown,
}

impl ObservationUploadIssue {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MissingCloudScope => "missing_cloud_scope",
            Self::RateLimited => "rate_limited",
            Self::InvalidBatch => "invalid_batch",
            Self::ServerRejected => "server_rejected",
            Self::Unknown => "unknown",
        }
    }
}
