use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{Row, SqlitePool};

use crate::cloud::outbox::kind;
use crate::cloud::session_mined::{
    SessionMinedCandidate, SessionMinedLocalEvidence, SessionMinedLocalTriage,
    SessionMinedLocalTriageStatus,
};
use crate::domain::models::{RememberRuleInput, SkillRecord};
use crate::{CoreError, Result};

const DEFAULT_LATEST_LIMIT: usize = 5;
const MAX_LATEST_LIMIT: usize = 1_000;
const SESSION_MINED_APPROVED_CONFIDENCE: f32 = 0.65;
const LOCAL_REVIEW_KEY: &str = "localReview";
const LOCAL_REVIEW_STATUS_KEY: &str = "status";
const LOCAL_REVIEW_RULE_ID_KEY: &str = "ruleId";
const LOCAL_REVIEW_REVIEWED_AT_KEY: &str = "reviewedAtMs";
const LOCAL_REVIEW_STATUS_APPROVED: &str = "approved";
const LOCAL_REVIEW_STATUS_PATH: &str = "$.localReview.status";
const LOCAL_TRIAGE_STATUS_PATH: &str = "$.localTriage.status";
const LOCAL_TRIAGE_SUPERSEDED_BY: &str = "superseded_by";
const LOCAL_TRIAGE_CLUSTERED_INTO: &str = "clustered_into";
const LOCAL_TRIAGE_DROPPED_LOW_SIGNAL: &str = "dropped_low_signal";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MemoryInbox {
    pub active_rules: MemoryRuleSection,
    pub local_drafts: MemoryRuleSection,
    pub local_discoveries: SessionMinedSection,
    pub queues: MemoryQueueSection,
    pub usage: MemoryUsage,
    pub warnings: Vec<MemoryInboxWarning>,
}

impl MemoryInbox {
    pub const fn active_rule_count(&self) -> i64 {
        self.active_rules.count
    }

    pub const fn local_draft_count(&self) -> i64 {
        self.local_drafts.count
    }

    pub const fn session_mined_count(&self) -> i64 {
        self.local_discoveries.count
    }

    pub fn memory_candidates_pending(&self) -> i64 {
        self.queues
            .cloud_outbox
            .iter()
            .filter(|count| count.kind == kind::SESSION_MINED_CANDIDATE)
            .map(|count| count.count)
            .sum()
    }

    pub fn cloud_observations_pending(&self) -> i64 {
        self.queues
            .cloud_outbox
            .iter()
            .filter(|count| count.kind == kind::OBSERVATION)
            .map(|count| count.count)
            .sum()
    }

    pub fn observation_events_pending(&self) -> i64 {
        self.queues
            .observations_outbox
            .iter()
            .filter(|count| count.status == "pending")
            .map(|count| count.count)
            .sum()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemoryListFilter {
    pub state: Option<String>,
    pub kind: Option<String>,
    pub repo_full_name: Option<String>,
    pub query: Option<String>,
    pub limit: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MemoryList {
    pub counts: MemoryStateCounts,
    pub items: Vec<MemoryListItem>,
    pub warnings: Vec<MemoryInboxWarning>,
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MemoryStateCounts {
    pub active: i64,
    pub drafts: i64,
    pub candidates: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MemoryListItem {
    pub item_id: String,
    pub kind: String,
    pub state: String,
    pub active: bool,
    pub served_to_agents: bool,
    pub approval_required: bool,
    pub title: String,
    pub summary: Option<String>,
    pub origin: Option<String>,
    pub source_repo: Option<String>,
    pub file_patterns: Vec<String>,
    pub updated_at: Option<String>,
    pub review_hint: Option<String>,
    pub commands: MemoryItemCommands,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MemoryItemCommands {
    pub show: String,
    pub approve: Option<String>,
    pub reject: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MemoryItemDetail {
    pub item: MemoryListItem,
    pub body: String,
    pub provenance: Option<Value>,
    pub activity: Option<MemoryActivitySummary>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemoryActivityFilter {
    pub rule_id: Option<String>,
    pub repo_full_name: Option<String>,
    pub days: i64,
    pub limit: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MemoryActivity {
    pub days: i64,
    pub summary: MemoryActivitySummary,
    pub recent: Vec<MemoryActivityEvent>,
    pub note: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MemoryActivitySummary {
    pub calls: i64,
    pub empty_calls: i64,
    pub rules_served: i64,
    pub strict_matches: i64,
    pub estimated_tokens: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MemoryActivityEvent {
    pub phase: String,
    pub tool: String,
    pub session_id: Option<String>,
    pub repo_full_name: Option<String>,
    pub file_path: Option<String>,
    pub rule_ids: Vec<String>,
    pub rule_count: i64,
    pub strict_match_count: i64,
    pub estimated_tokens: i64,
    pub at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MemoryRuleSection {
    pub count: i64,
    pub latest: Vec<MemoryRuleItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MemoryRuleItem {
    pub id: String,
    pub name: String,
    pub origin: String,
    pub source_repo: Option<String>,
    pub file_patterns: Vec<String>,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionMinedSection {
    pub count: i64,
    pub latest: Vec<SessionMinedDiscovery>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionMinedDiscovery {
    pub item_id: String,
    pub content_hash: String,
    pub outbox_id: i64,
    pub status: String,
    pub retry_count: i64,
    pub created_at_ms: i64,
    pub last_error: Option<String>,
    pub source_repo: String,
    pub title: String,
    pub body: String,
    pub file_patterns: Vec<String>,
    pub gate_verdict: String,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distinct_evidence_count: Option<usize>,
}

impl SessionMinedDiscovery {
    fn from_candidate(row: SessionMinedRow, candidate: SessionMinedCandidate) -> Self {
        Self {
            item_id: format!("session:{}", candidate.content_hash),
            content_hash: candidate.content_hash,
            outbox_id: row.id,
            status: row.status,
            retry_count: row.retry_count,
            created_at_ms: row.created_at,
            last_error: row.last_error,
            source_repo: candidate.source_repo,
            title: candidate.title,
            body: candidate.body,
            file_patterns: candidate.file_patterns,
            gate_verdict: candidate.gate_verdict,
            session_id: candidate.session_id,
            distinct_evidence_count: candidate
                .local_evidence
                .as_ref()
                .map(|evidence| evidence.distinct_evidence_count),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MemoryQueueSection {
    pub cloud_outbox: Vec<OutboxQueueCount>,
    pub observations_outbox: Vec<ObservationQueueCount>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OutboxQueueCount {
    pub kind: String,
    pub status: String,
    pub count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ObservationQueueCount {
    pub event_type: String,
    pub status: String,
    pub count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MemoryUsage {
    pub local_agent_serves: i64,
    pub proof_surface: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MemoryInboxWarning {
    pub code: String,
    pub message: String,
    pub outbox_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovedSessionMinedCandidate {
    pub item_id: String,
    pub content_hash: String,
    pub outbox_id: i64,
    pub rule: SkillRecord,
    pub deduped: bool,
    pub confidence_after: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RejectedSessionMinedCandidate {
    pub item_id: String,
    pub content_hash: String,
    pub outbox_id: i64,
}

#[derive(Debug, Clone)]
struct SessionMinedRow {
    id: i64,
    payload_json: String,
    status: String,
    retry_count: i64,
    created_at: i64,
    last_error: Option<String>,
}

pub async fn load_memory_inbox(pool: &SqlitePool, latest_limit: usize) -> Result<MemoryInbox> {
    let latest_limit = normalize_limit(latest_limit);
    let active_rules = load_rule_section(pool, "active", latest_limit).await?;
    let local_drafts = load_rule_section(pool, "pending", latest_limit).await?;
    let (local_discoveries, mut warnings) = load_session_mined_section(pool, latest_limit).await?;
    let queues = MemoryQueueSection {
        cloud_outbox: load_cloud_outbox_counts(pool).await?,
        observations_outbox: load_observation_counts(pool).await?,
    };
    let usage = MemoryUsage {
        local_agent_serves: load_local_agent_serves(pool).await?,
        proof_surface: "difflore status --json".to_owned(),
    };

    if local_discoveries.count > 0 && local_discoveries.latest.is_empty() {
        warnings.push(MemoryInboxWarning {
            code: "session_mined_unreadable".to_owned(),
            message: "session-mined rows exist, but none could be parsed for display".to_owned(),
            outbox_id: None,
        });
    }

    Ok(MemoryInbox {
        active_rules,
        local_drafts,
        local_discoveries,
        queues,
        usage,
        warnings,
    })
}

pub async fn load_memory_inbox_default(pool: &SqlitePool) -> Result<MemoryInbox> {
    load_memory_inbox(pool, DEFAULT_LATEST_LIMIT).await
}

pub async fn load_memory_items(pool: &SqlitePool, filter: MemoryListFilter) -> Result<MemoryList> {
    let limit = normalize_limit(filter.limit);
    let inbox = load_memory_inbox(pool, limit).await?;
    let mut items = Vec::new();

    for rule in &inbox.active_rules.latest {
        items.push(active_rule_item(rule));
    }
    for draft in &inbox.local_drafts.latest {
        items.push(draft_rule_item(draft));
    }
    for discovery in &inbox.local_discoveries.latest {
        items.push(session_discovery_item(discovery));
    }

    let state = filter.state.as_deref().map(normalize_memory_filter);
    let kind = filter.kind.as_deref().map(normalize_memory_filter);
    let repo = filter
        .repo_full_name
        .as_deref()
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty());
    let query = filter
        .query
        .as_deref()
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty());

    items.retain(|item| {
        state_matches(state.as_deref(), item)
            && kind_matches(kind.as_deref(), item)
            && repo_matches(repo.as_deref(), item)
            && query_matches(query.as_deref(), item)
    });
    items.truncate(limit);

    Ok(MemoryList {
        counts: MemoryStateCounts {
            active: inbox.active_rule_count(),
            drafts: inbox.local_draft_count(),
            candidates: inbox.session_mined_count(),
        },
        items,
        warnings: inbox.warnings,
        note: "MCP memory tools can read and propose. Use the DiffLore CLI to approve, reject, sync, archive, or otherwise govern memory.".to_owned(),
    })
}

pub async fn get_memory_item(pool: &SqlitePool, item_id: &str) -> Result<Option<MemoryItemDetail>> {
    let trimmed = item_id.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    if let Some(draft_id) = trimmed.strip_prefix("draft:") {
        return draft_detail(pool, draft_id).await;
    }
    if let Some(rule_id) = trimmed.strip_prefix("rule:") {
        return active_rule_detail(pool, rule_id).await;
    }
    if let Ok(content_hash) = parse_session_item_id(trimmed) {
        return session_detail(pool, &content_hash).await;
    }

    active_rule_detail(pool, trimmed).await
}

pub async fn load_memory_activity(
    pool: &SqlitePool,
    filter: MemoryActivityFilter,
) -> Result<MemoryActivity> {
    let days = filter.days.max(1);
    let limit = normalize_limit(filter.limit.max(DEFAULT_LATEST_LIMIT));
    if !table_exists(pool, "mcp_rule_serves").await? {
        return Ok(MemoryActivity {
            days,
            summary: MemoryActivitySummary::default(),
            recent: Vec::new(),
            note: memory_activity_note(),
        });
    }

    let rows = sqlx::query(
        "SELECT tool, session_id, repo_full_name, file_path, rule_ids_json, \
                rule_count, was_empty, strict_match_count, estimated_tokens, served_at \
         FROM mcp_rule_serves \
         WHERE datetime(served_at) >= datetime('now', ?1) \
         ORDER BY datetime(served_at) DESC, id DESC \
         LIMIT ?2",
    )
    .bind(format!("-{days} days"))
    .bind(limit_i64(limit))
    .fetch_all(pool)
    .await?;

    let repo_filter = filter
        .repo_full_name
        .as_deref()
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty());
    let rule_filter = filter
        .rule_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());

    let mut recent = Vec::new();
    let mut summary = MemoryActivitySummary::default();
    for row in rows {
        let repo_full_name: Option<String> = row.try_get("repo_full_name").ok();
        if let Some(repo) = repo_filter.as_deref()
            && repo_full_name
                .as_deref()
                .map(str::to_ascii_lowercase)
                .as_deref()
                != Some(repo)
        {
            continue;
        }

        let rule_ids_json: String = row.try_get("rule_ids_json").unwrap_or_default();
        let rule_ids = parse_string_list(Some(&rule_ids_json));
        if let Some(rule_id) = rule_filter
            && !rule_ids.iter().any(|id| id == rule_id)
        {
            continue;
        }

        let was_empty: i64 = row.try_get("was_empty").unwrap_or_default();
        let rule_count: i64 = row.try_get("rule_count").unwrap_or_default();
        let strict_match_count: i64 = row.try_get("strict_match_count").unwrap_or_default();
        let estimated_tokens: i64 = row.try_get("estimated_tokens").unwrap_or_default();

        summary.calls += 1;
        summary.empty_calls += i64::from(was_empty != 0 || rule_count == 0);
        summary.rules_served += rule_count.max(0);
        summary.strict_matches += strict_match_count.max(0);
        summary.estimated_tokens += estimated_tokens.max(0);

        recent.push(MemoryActivityEvent {
            phase: if rule_count > 0 {
                "surfaced_to_agent".to_owned()
            } else {
                "retrieval_empty".to_owned()
            },
            tool: row.try_get("tool").unwrap_or_default(),
            session_id: row.try_get("session_id").ok(),
            repo_full_name,
            file_path: row.try_get("file_path").ok(),
            rule_ids,
            rule_count,
            strict_match_count,
            estimated_tokens,
            at: row.try_get("served_at").unwrap_or_default(),
        });
    }

    Ok(MemoryActivity {
        days,
        summary,
        recent,
        note: memory_activity_note(),
    })
}

pub async fn find_session_mined_by_content_hash(
    pool: &SqlitePool,
    content_hash: &str,
) -> Result<Option<SessionMinedDiscovery>> {
    let target = content_hash.trim();
    if target.is_empty() {
        return Ok(None);
    }

    let rows = load_session_mined_rows(pool, None).await?;
    for row in rows {
        if session_mined_locally_approved_payload(&row.payload_json) {
            continue;
        }
        let Ok(candidate) = serde_json::from_str::<SessionMinedCandidate>(&row.payload_json) else {
            continue;
        };
        if candidate.content_hash == target {
            return Ok(Some(SessionMinedDiscovery::from_candidate(row, candidate)));
        }
    }
    Ok(None)
}

pub async fn approve_session_mined_candidate(
    pool: &SqlitePool,
    content_hash: &str,
) -> Result<ApprovedSessionMinedCandidate> {
    let (row, candidate) = find_session_mined_row_by_content_hash(pool, content_hash)
        .await?
        .ok_or_else(|| session_candidate_not_found(content_hash))?;
    validate_session_candidate(&candidate, row.id)?;

    let outcome = crate::skills::remember_as_candidate_with_confidence(
        pool,
        session_candidate_input(&candidate),
        SESSION_MINED_APPROVED_CONFIDENCE,
    )
    .await?;

    attach_session_candidate_repo_scope(pool, &outcome.skill.id, &candidate.source_repo).await?;
    let activated = match crate::skills::promote_candidate(pool, &outcome.skill.id).await {
        Ok(rule) => rule,
        Err(CoreError::Validation(message)) if message.contains("already active") => {
            load_skill_record(pool, &outcome.skill.id).await?
        }
        Err(err) => return Err(err),
    };

    mark_session_mined_locally_approved(pool, row.id, &activated.id).await?;

    Ok(ApprovedSessionMinedCandidate {
        item_id: format!("session:{}", candidate.content_hash),
        content_hash: candidate.content_hash,
        outbox_id: row.id,
        rule: activated,
        deduped: outcome.deduped,
        confidence_after: outcome.confidence_after,
    })
}

pub async fn mark_session_mined_candidate_approved_for_rule(
    pool: &SqlitePool,
    content_hash: &str,
    rule_id: &str,
) -> Result<Option<i64>> {
    let Some((row, candidate)) = find_session_mined_row_by_content_hash(pool, content_hash).await?
    else {
        return Ok(None);
    };
    validate_session_candidate(&candidate, row.id)?;
    mark_session_mined_locally_approved(pool, row.id, rule_id).await?;
    Ok(Some(row.id))
}

pub async fn set_candidate_triage(
    pool: &SqlitePool,
    content_hash: &str,
    status: SessionMinedLocalTriageStatus,
    reason: &str,
    reference: Option<&str>,
) -> Result<Option<i64>> {
    let target = content_hash.trim();
    if target.is_empty() {
        return Ok(None);
    }
    if matches!(
        status,
        SessionMinedLocalTriageStatus::SupersededBy | SessionMinedLocalTriageStatus::ClusteredInto
    ) && reference.is_some_and(|value| value.trim() == target)
    {
        return Err(CoreError::Validation(
            "session-mined local triage reference must not point at the candidate itself"
                .to_owned(),
        ));
    }
    let triage_json = session_mined_local_triage_json(status, reason, reference)?;

    let rows = load_all_session_mined_rows(pool).await?;
    let mut updated_row_id = None;
    for row in rows {
        let Ok(candidate) = serde_json::from_str::<SessionMinedCandidate>(&row.payload_json) else {
            continue;
        };
        if candidate.content_hash != target {
            continue;
        }
        if candidate
            .local_triage
            .as_ref()
            .is_some_and(|triage| !matches!(triage.status, SessionMinedLocalTriageStatus::Unknown))
        {
            continue;
        }
        sqlx::query(
            "UPDATE cloud_outbox \
             SET payload_json = json_set(payload_json, '$.localTriage', json(?1)) \
             WHERE id = ?2",
        )
        .bind(&triage_json)
        .bind(row.id)
        .execute(pool)
        .await?;
        if updated_row_id.is_none() {
            updated_row_id = Some(row.id);
        }
    }

    Ok(updated_row_id)
}

pub async fn set_candidate_distinct_evidence_count(
    pool: &SqlitePool,
    content_hash: &str,
    distinct_evidence_count: usize,
) -> Result<Option<i64>> {
    let target = content_hash.trim();
    if target.is_empty() || distinct_evidence_count == 0 {
        return Ok(None);
    }

    let rows = load_all_session_mined_rows(pool).await?;
    let mut updated_row_id = None;
    for row in rows {
        let Ok(candidate) = serde_json::from_str::<SessionMinedCandidate>(&row.payload_json) else {
            continue;
        };
        if candidate.content_hash != target {
            continue;
        }
        if candidate
            .local_evidence
            .as_ref()
            .is_some_and(|evidence| evidence.distinct_evidence_count == distinct_evidence_count)
        {
            continue;
        }
        let evidence_json = session_mined_local_evidence_json(distinct_evidence_count)?;
        sqlx::query(
            "UPDATE cloud_outbox \
             SET payload_json = json_set(payload_json, '$.localEvidence', json(?1)) \
             WHERE id = ?2",
        )
        .bind(evidence_json)
        .bind(row.id)
        .execute(pool)
        .await?;
        if updated_row_id.is_none() {
            updated_row_id = Some(row.id);
        }
    }

    Ok(updated_row_id)
}

pub async fn delete_session_mined_candidates_by_content_hash(
    pool: &SqlitePool,
    content_hash: &str,
) -> Result<Vec<RejectedSessionMinedCandidate>> {
    let target = content_hash.trim();
    if target.is_empty() {
        return Ok(Vec::new());
    }

    let rows = load_all_session_mined_rows(pool).await?;
    let mut deleted = Vec::new();
    for row in rows {
        if session_mined_locally_approved_payload(&row.payload_json) {
            continue;
        }
        let Ok(candidate) = serde_json::from_str::<SessionMinedCandidate>(&row.payload_json) else {
            continue;
        };
        if candidate.content_hash != target {
            continue;
        }
        delete_session_mined_outbox_row(pool, row.id).await?;
        deleted.push(RejectedSessionMinedCandidate {
            item_id: format!("session:{}", candidate.content_hash),
            content_hash: candidate.content_hash,
            outbox_id: row.id,
        });
    }
    Ok(deleted)
}

pub async fn delete_dropped_low_signal_session_mined_candidates(
    pool: &SqlitePool,
) -> Result<Vec<RejectedSessionMinedCandidate>> {
    let rows = load_all_session_mined_rows(pool).await?;
    let mut deleted = Vec::new();
    for row in rows {
        if session_mined_locally_approved_payload(&row.payload_json) {
            continue;
        }
        let Ok(candidate) = serde_json::from_str::<SessionMinedCandidate>(&row.payload_json) else {
            continue;
        };
        if !candidate.local_triage.as_ref().is_some_and(|triage| {
            matches!(
                triage.status,
                SessionMinedLocalTriageStatus::DroppedLowSignal
            )
        }) {
            continue;
        }
        delete_session_mined_outbox_row(pool, row.id).await?;
        deleted.push(RejectedSessionMinedCandidate {
            item_id: format!("session:{}", candidate.content_hash),
            content_hash: candidate.content_hash,
            outbox_id: row.id,
        });
    }
    Ok(deleted)
}

pub async fn reject_session_mined_candidate(
    pool: &SqlitePool,
    content_hash: &str,
) -> Result<RejectedSessionMinedCandidate> {
    let (row, candidate) = find_session_mined_row_by_content_hash(pool, content_hash)
        .await?
        .ok_or_else(|| session_candidate_not_found(content_hash))?;
    delete_session_mined_outbox_row(pool, row.id).await?;

    Ok(RejectedSessionMinedCandidate {
        item_id: format!("session:{}", candidate.content_hash),
        content_hash: candidate.content_hash,
        outbox_id: row.id,
    })
}

async fn find_session_mined_row_by_content_hash(
    pool: &SqlitePool,
    content_hash: &str,
) -> Result<Option<(SessionMinedRow, SessionMinedCandidate)>> {
    let target = content_hash.trim();
    if target.is_empty() {
        return Ok(None);
    }

    let rows = load_session_mined_rows(pool, None).await?;
    for row in rows {
        if session_mined_locally_approved_payload(&row.payload_json) {
            continue;
        }
        let Ok(candidate) = serde_json::from_str::<SessionMinedCandidate>(&row.payload_json) else {
            continue;
        };
        if candidate.content_hash == target {
            return Ok(Some((row, candidate)));
        }
    }
    Ok(None)
}

fn session_candidate_not_found(content_hash: &str) -> CoreError {
    CoreError::NotFound(format!(
        "candidate memory `session:{}` not found",
        content_hash.trim()
    ))
}

fn validate_session_candidate(candidate: &SessionMinedCandidate, outbox_id: i64) -> Result<()> {
    candidate.validate().map_err(|err| {
        CoreError::Validation(format!(
            "candidate memory row {outbox_id} is invalid and cannot be approved locally: {err}"
        ))
    })
}

fn session_candidate_input(candidate: &SessionMinedCandidate) -> RememberRuleInput {
    RememberRuleInput {
        title: candidate.title.clone(),
        body: candidate.body.clone(),
        file_patterns: Some(candidate.file_patterns.clone()),
        bad_code: None,
        good_code: None,
        severity: None,
        kind: None,
        category: None,
        origin: Some(crate::cloud::session_mined::ORIGIN.to_owned()),
        captured_by_client: Some("session-mined".to_owned()),
    }
}

async fn attach_session_candidate_repo_scope(
    pool: &SqlitePool,
    skill_id: &str,
    source_repo: &str,
) -> Result<()> {
    sqlx::query(
        "UPDATE skills SET source_repo = ?1 \
         WHERE id = ?2 AND (source_repo IS NULL OR trim(source_repo) = '' OR source_repo = ?1)",
    )
    .bind(source_repo)
    .bind(skill_id)
    .execute(pool)
    .await?;
    Ok(())
}

async fn load_skill_record(pool: &SqlitePool, id: &str) -> Result<SkillRecord> {
    let row = sqlx::query_as!(
        crate::skills::SkillRow,
        "SELECT id, name, source, directory, version, description, type, \
         engines, tags, trigger, check_prompt, repo_owner, repo_name, repo_branch, readme_url, \
         enabled_for_codex, enabled_for_claude, enabled_for_gemini, enabled_for_cursor, \
         installed_at, updated_at, origin FROM skills WHERE id = ?1",
        id
    )
    .fetch_one(pool)
    .await?;
    Ok(SkillRecord::from(row))
}

async fn load_rule_section(
    pool: &SqlitePool,
    status: &str,
    latest_limit: usize,
) -> Result<MemoryRuleSection> {
    let count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM skills WHERE status = ?1")
        .bind(status)
        .fetch_one(pool)
        .await?;
    let rows = sqlx::query(
        "SELECT id, name, origin, source_repo, file_patterns, \
                COALESCE(updated_at, installed_at) AS updated_at \
         FROM skills \
         WHERE status = ?1 \
         ORDER BY datetime(COALESCE(updated_at, installed_at)) DESC, id ASC \
         LIMIT ?2",
    )
    .bind(status)
    .bind(limit_i64(latest_limit))
    .fetch_all(pool)
    .await?;

    let latest = rows
        .into_iter()
        .map(|row| {
            let file_patterns: Option<String> = row.try_get("file_patterns").ok();
            MemoryRuleItem {
                id: row.try_get("id").unwrap_or_default(),
                name: row.try_get("name").unwrap_or_default(),
                origin: row.try_get("origin").unwrap_or_default(),
                source_repo: row.try_get("source_repo").ok(),
                file_patterns: parse_string_list(file_patterns.as_deref()),
                updated_at: row.try_get("updated_at").unwrap_or_default(),
            }
        })
        .collect();

    Ok(MemoryRuleSection { count, latest })
}

async fn load_session_mined_section(
    pool: &SqlitePool,
    latest_limit: usize,
) -> Result<(SessionMinedSection, Vec<MemoryInboxWarning>)> {
    let count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM cloud_outbox \
         WHERE kind = ?1 \
	           AND NOT (
	                json_valid(payload_json)
	                AND LOWER(COALESCE(json_extract(payload_json, ?2), '')) = ?3
	           ) \
	           AND NOT (
	                json_valid(payload_json)
	                AND LOWER(COALESCE(json_extract(payload_json, ?4), '')) IN (?5, ?6, ?7)
	           )",
    )
    .bind(kind::SESSION_MINED_CANDIDATE)
    .bind(LOCAL_REVIEW_STATUS_PATH)
    .bind(LOCAL_REVIEW_STATUS_APPROVED)
    .bind(LOCAL_TRIAGE_STATUS_PATH)
    .bind(LOCAL_TRIAGE_SUPERSEDED_BY)
    .bind(LOCAL_TRIAGE_CLUSTERED_INTO)
    .bind(LOCAL_TRIAGE_DROPPED_LOW_SIGNAL)
    .fetch_one(pool)
    .await?;
    let rows = load_session_mined_rows(pool, Some(latest_limit)).await?;
    let mut latest = Vec::new();
    let mut warnings = Vec::new();
    for row in rows {
        match serde_json::from_str::<SessionMinedCandidate>(&row.payload_json) {
            Ok(candidate) => {
                if let Err(err) = candidate.validate() {
                    warnings.push(MemoryInboxWarning {
                        code: "session_mined_invalid".to_owned(),
                        message: format!("session-mined row {} failed validation: {err}", row.id),
                        outbox_id: Some(row.id),
                    });
                }
                latest.push(SessionMinedDiscovery::from_candidate(row, candidate));
            }
            Err(err) => warnings.push(MemoryInboxWarning {
                code: "session_mined_parse_failed".to_owned(),
                message: format!("session-mined row {} could not be parsed: {err}", row.id),
                outbox_id: Some(row.id),
            }),
        }
    }

    Ok((SessionMinedSection { count, latest }, warnings))
}

async fn mark_session_mined_locally_approved(
    pool: &SqlitePool,
    outbox_id: i64,
    rule_id: &str,
) -> Result<()> {
    let payload_json: String =
        sqlx::query_scalar("SELECT payload_json FROM cloud_outbox WHERE id = ?1")
            .bind(outbox_id)
            .fetch_one(pool)
            .await?;
    let marked = session_mined_payload_with_local_approval(&payload_json, rule_id)?;
    sqlx::query(
        "UPDATE cloud_outbox \
         SET payload_json = ?1, status = 'pending', retry_count = 0, claimed_at = NULL, last_error = NULL \
         WHERE id = ?2",
    )
        .bind(marked)
        .bind(outbox_id)
        .execute(pool)
        .await?;
    Ok(())
}

async fn delete_session_mined_outbox_row(pool: &SqlitePool, outbox_id: i64) -> Result<()> {
    sqlx::query("DELETE FROM cloud_outbox WHERE id = ?1")
        .bind(outbox_id)
        .execute(pool)
        .await?;
    Ok(())
}

fn session_mined_payload_with_local_approval(payload_json: &str, rule_id: &str) -> Result<String> {
    let mut value: Value = serde_json::from_str(payload_json)
        .map_err(|err| CoreError::Validation(format!("session-mined payload invalid: {err}")))?;
    let Some(object) = value.as_object_mut() else {
        return Err(CoreError::Validation(
            "session-mined payload must be a JSON object".to_owned(),
        ));
    };
    object.insert(
        LOCAL_REVIEW_KEY.to_owned(),
        serde_json::json!({
            LOCAL_REVIEW_STATUS_KEY: LOCAL_REVIEW_STATUS_APPROVED,
            LOCAL_REVIEW_RULE_ID_KEY: rule_id,
            LOCAL_REVIEW_REVIEWED_AT_KEY: crate::cloud::outbox_core::now_unix_ms(),
        }),
    );
    serde_json::to_string(&value)
        .map_err(|err| CoreError::Validation(format!("session-mined payload invalid: {err}")))
}

fn session_mined_local_triage_json(
    status: SessionMinedLocalTriageStatus,
    reason: &str,
    reference: Option<&str>,
) -> Result<String> {
    let reason = reason.trim();
    if reason.is_empty() {
        return Err(CoreError::Validation(
            "session-mined triage reason must not be empty".to_owned(),
        ));
    }
    let reference = reference
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    if matches!(
        status,
        SessionMinedLocalTriageStatus::SupersededBy | SessionMinedLocalTriageStatus::ClusteredInto
    ) && reference.is_none()
    {
        return Err(CoreError::Validation(
            "session-mined triage reference is required for superseded/clustered candidates"
                .to_owned(),
        ));
    }
    let triage = SessionMinedLocalTriage {
        status,
        reason: reason.to_owned(),
        reference,
        at: crate::cloud::outbox_core::now_unix_ms(),
    };
    serde_json::to_string(&triage)
        .map_err(|err| CoreError::Validation(format!("session-mined triage invalid: {err}")))
}

fn session_mined_local_evidence_json(distinct_evidence_count: usize) -> Result<String> {
    if distinct_evidence_count == 0 {
        return Err(CoreError::Validation(
            "session-mined evidence count must be positive".to_owned(),
        ));
    }
    let evidence = SessionMinedLocalEvidence {
        distinct_evidence_count,
        updated_at: crate::cloud::outbox_core::now_unix_ms(),
    };
    serde_json::to_string(&evidence)
        .map_err(|err| CoreError::Validation(format!("session-mined evidence invalid: {err}")))
}

fn session_mined_locally_approved_payload(payload_json: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(payload_json) else {
        return false;
    };
    value
        .get(LOCAL_REVIEW_KEY)
        .and_then(|review| review.get(LOCAL_REVIEW_STATUS_KEY))
        .and_then(Value::as_str)
        .is_some_and(|status| status.eq_ignore_ascii_case(LOCAL_REVIEW_STATUS_APPROVED))
}

async fn load_all_session_mined_rows(pool: &SqlitePool) -> Result<Vec<SessionMinedRow>> {
    let rows = sqlx::query(
        "SELECT id, payload_json, status, retry_count, created_at, last_error \
         FROM cloud_outbox \
         WHERE kind = ?1 \
         ORDER BY created_at DESC, id DESC",
    )
    .bind(kind::SESSION_MINED_CANDIDATE)
    .fetch_all(pool)
    .await?;

    rows.iter().map(session_mined_row_from_sql).collect()
}

async fn load_session_mined_rows(
    pool: &SqlitePool,
    latest_limit: Option<usize>,
) -> Result<Vec<SessionMinedRow>> {
    let rows = if let Some(limit) = latest_limit {
        sqlx::query(
            "SELECT id, payload_json, status, retry_count, created_at, last_error \
             FROM cloud_outbox \
             WHERE kind = ?1 \
	               AND NOT (
	                    json_valid(payload_json)
	                    AND LOWER(COALESCE(json_extract(payload_json, ?2), '')) = ?3
	               ) \
	               AND NOT (
	                    json_valid(payload_json)
	                    AND LOWER(COALESCE(json_extract(payload_json, ?4), '')) IN (?5, ?6, ?7)
	               ) \
	             ORDER BY created_at DESC, id DESC \
	             LIMIT ?8",
        )
        .bind(kind::SESSION_MINED_CANDIDATE)
        .bind(LOCAL_REVIEW_STATUS_PATH)
        .bind(LOCAL_REVIEW_STATUS_APPROVED)
        .bind(LOCAL_TRIAGE_STATUS_PATH)
        .bind(LOCAL_TRIAGE_SUPERSEDED_BY)
        .bind(LOCAL_TRIAGE_CLUSTERED_INTO)
        .bind(LOCAL_TRIAGE_DROPPED_LOW_SIGNAL)
        .bind(limit_i64(limit))
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query(
            "SELECT id, payload_json, status, retry_count, created_at, last_error \
             FROM cloud_outbox \
             WHERE kind = ?1 \
	               AND NOT (
	                    json_valid(payload_json)
	                    AND LOWER(COALESCE(json_extract(payload_json, ?2), '')) = ?3
	               ) \
	               AND NOT (
	                    json_valid(payload_json)
	                    AND LOWER(COALESCE(json_extract(payload_json, ?4), '')) IN (?5, ?6, ?7)
	               ) \
	             ORDER BY created_at DESC, id DESC",
        )
        .bind(kind::SESSION_MINED_CANDIDATE)
        .bind(LOCAL_REVIEW_STATUS_PATH)
        .bind(LOCAL_REVIEW_STATUS_APPROVED)
        .bind(LOCAL_TRIAGE_STATUS_PATH)
        .bind(LOCAL_TRIAGE_SUPERSEDED_BY)
        .bind(LOCAL_TRIAGE_CLUSTERED_INTO)
        .bind(LOCAL_TRIAGE_DROPPED_LOW_SIGNAL)
        .fetch_all(pool)
        .await?
    };

    rows.iter().map(session_mined_row_from_sql).collect()
}

fn session_mined_row_from_sql(row: &sqlx::sqlite::SqliteRow) -> Result<SessionMinedRow> {
    Ok(SessionMinedRow {
        id: row.try_get("id")?,
        payload_json: row.try_get("payload_json")?,
        status: row.try_get("status")?,
        retry_count: row.try_get("retry_count")?,
        created_at: row.try_get("created_at")?,
        last_error: row.try_get("last_error").ok(),
    })
}

async fn load_cloud_outbox_counts(pool: &SqlitePool) -> Result<Vec<OutboxQueueCount>> {
    let rows = sqlx::query(
        "SELECT kind, status, COUNT(*) AS count \
         FROM cloud_outbox \
         WHERE kind != ?1 \
            OR (
                json_valid(payload_json)
	                AND LOWER(COALESCE(json_extract(payload_json, ?2), '')) = ?3
	                AND NOT (
	                    json_valid(payload_json)
	                    AND LOWER(COALESCE(json_extract(payload_json, ?4), '')) IN (?5, ?6, ?7)
	                )
	            ) \
	         GROUP BY kind, status \
	         ORDER BY kind ASC, status ASC",
    )
    .bind(kind::SESSION_MINED_CANDIDATE)
    .bind(LOCAL_REVIEW_STATUS_PATH)
    .bind(LOCAL_REVIEW_STATUS_APPROVED)
    .bind(LOCAL_TRIAGE_STATUS_PATH)
    .bind(LOCAL_TRIAGE_SUPERSEDED_BY)
    .bind(LOCAL_TRIAGE_CLUSTERED_INTO)
    .bind(LOCAL_TRIAGE_DROPPED_LOW_SIGNAL)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| OutboxQueueCount {
            kind: row.try_get("kind").unwrap_or_default(),
            status: row.try_get("status").unwrap_or_default(),
            count: row.try_get("count").unwrap_or_default(),
        })
        .collect())
}

async fn load_observation_counts(pool: &SqlitePool) -> Result<Vec<ObservationQueueCount>> {
    if !table_exists(pool, "observation_events").await? {
        return Ok(Vec::new());
    }

    let rows = sqlx::query(
        "SELECT event_type, status, COUNT(*) AS count \
         FROM observation_events \
         GROUP BY event_type, status \
         ORDER BY event_type ASC, status ASC",
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| ObservationQueueCount {
            event_type: row.try_get("event_type").unwrap_or_default(),
            status: row.try_get("status").unwrap_or_default(),
            count: row.try_get("count").unwrap_or_default(),
        })
        .collect())
}

async fn load_local_agent_serves(pool: &SqlitePool) -> Result<i64> {
    let mut count = 0;
    if table_exists(pool, "mcp_rule_serves").await? {
        count += sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM mcp_rule_serves")
            .fetch_one(pool)
            .await?;
    }
    if table_exists(pool, "observation_events").await? {
        count += sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM observation_events WHERE event_type = 'mcp_rule_served'",
        )
        .fetch_one(pool)
        .await?;
    }
    Ok(count)
}

async fn table_exists(pool: &SqlitePool, name: &str) -> Result<bool> {
    let count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
    )
    .bind(name)
    .fetch_one(pool)
    .await?;
    Ok(count > 0)
}

fn parse_string_list(raw: Option<&str>) -> Vec<String> {
    raw.map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(|value| serde_json::from_str::<Vec<String>>(value).ok())
        .unwrap_or_default()
}

fn active_rule_item(rule: &MemoryRuleItem) -> MemoryListItem {
    MemoryListItem {
        item_id: format!("rule:{}", rule.id),
        kind: "rule".to_owned(),
        state: "active".to_owned(),
        active: true,
        served_to_agents: true,
        approval_required: false,
        title: rule.name.clone(),
        summary: None,
        origin: Some(rule.origin.clone()),
        source_repo: rule.source_repo.clone(),
        file_patterns: rule.file_patterns.clone(),
        updated_at: Some(rule.updated_at.clone()),
        review_hint: Some("served to agents when recall matches".to_owned()),
        commands: MemoryItemCommands {
            show: format!("difflore memory show rule:{}", rule.id),
            approve: None,
            reject: None,
        },
    }
}

fn draft_rule_item(rule: &MemoryRuleItem) -> MemoryListItem {
    MemoryListItem {
        item_id: format!("draft:{}", rule.id),
        kind: "draft".to_owned(),
        state: "draft".to_owned(),
        active: false,
        served_to_agents: false,
        approval_required: true,
        title: rule.name.clone(),
        summary: None,
        origin: Some(rule.origin.clone()),
        source_repo: rule.source_repo.clone(),
        file_patterns: rule.file_patterns.clone(),
        updated_at: Some(rule.updated_at.clone()),
        review_hint: Some("approve locally before agents can use it".to_owned()),
        commands: MemoryItemCommands {
            show: format!("difflore memory show draft:{}", rule.id),
            approve: Some(format!("difflore memory approve draft:{}", rule.id)),
            reject: Some(format!("difflore memory reject draft:{}", rule.id)),
        },
    }
}

fn session_discovery_item(discovery: &SessionMinedDiscovery) -> MemoryListItem {
    MemoryListItem {
        item_id: discovery.item_id.clone(),
        kind: "candidate".to_owned(),
        state: "candidate".to_owned(),
        active: false,
        served_to_agents: false,
        approval_required: true,
        title: discovery.title.clone(),
        summary: Some(truncate_summary(&discovery.body, 240)),
        origin: Some("session_mined".to_owned()),
        source_repo: Some(discovery.source_repo.clone()),
        file_patterns: discovery.file_patterns.clone(),
        updated_at: Some(discovery.created_at_ms.to_string()),
        review_hint: Some(session_review_hint(&discovery.gate_verdict)),
        commands: MemoryItemCommands {
            show: format!("difflore memory show {}", discovery.item_id),
            approve: Some(format!("difflore memory approve {}", discovery.item_id)),
            reject: Some(format!("difflore memory reject {}", discovery.item_id)),
        },
    }
}

async fn draft_detail(pool: &SqlitePool, draft_id: &str) -> Result<Option<MemoryItemDetail>> {
    let draft = crate::skills::list_candidates(pool, None, None)
        .await?
        .into_iter()
        .find(|candidate| candidate.id == draft_id);
    let Some(draft) = draft else {
        return Ok(None);
    };
    let item = MemoryListItem {
        item_id: format!("draft:{}", draft.id),
        kind: "draft".to_owned(),
        state: "draft".to_owned(),
        active: false,
        served_to_agents: false,
        approval_required: true,
        title: draft.name.clone(),
        summary: Some(truncate_summary(&draft.description, 240)),
        origin: Some(draft.origin.clone()),
        source_repo: draft.source_repo.clone(),
        file_patterns: draft.file_patterns.clone(),
        updated_at: Some(draft.installed_at.clone()),
        review_hint: Some("approve locally before agents can use it".to_owned()),
        commands: MemoryItemCommands {
            show: format!("difflore memory show draft:{}", draft.id),
            approve: Some(format!("difflore memory approve draft:{}", draft.id)),
            reject: Some(format!("difflore memory reject draft:{}", draft.id)),
        },
    };
    let provenance = draft
        .source_proof
        .as_ref()
        .and_then(|proof| serde_json::to_value(proof).ok());
    Ok(Some(MemoryItemDetail {
        item,
        body: draft
            .drafted_rule
            .clone()
            .unwrap_or_else(|| draft.description.clone()),
        provenance,
        activity: None,
    }))
}

async fn session_detail(pool: &SqlitePool, content_hash: &str) -> Result<Option<MemoryItemDetail>> {
    let Some(discovery) = find_session_mined_by_content_hash(pool, content_hash).await? else {
        return Ok(None);
    };
    let item = session_discovery_item(&discovery);
    Ok(Some(MemoryItemDetail {
        item,
        body: discovery.body,
        provenance: Some(serde_json::json!({
            "sessionId": discovery.session_id,
            "gateVerdict": discovery.gate_verdict,
            "outboxId": discovery.outbox_id,
            "status": discovery.status,
            "retryCount": discovery.retry_count,
            "lastError": discovery.last_error,
        })),
        activity: None,
    }))
}

async fn active_rule_detail(pool: &SqlitePool, rule_id: &str) -> Result<Option<MemoryItemDetail>> {
    let row = sqlx::query(
        "SELECT id, name, description, origin, source_repo, file_patterns, \
                COALESCE(updated_at, installed_at) AS updated_at \
         FROM skills \
         WHERE id = ?1 AND status = 'active'",
    )
    .bind(rule_id.trim())
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let id: String = row.try_get("id").unwrap_or_default();
    let description: String = row.try_get("description").unwrap_or_default();
    let file_patterns_raw: Option<String> = row.try_get("file_patterns").ok().flatten();
    let item = MemoryListItem {
        item_id: format!("rule:{id}"),
        kind: "rule".to_owned(),
        state: "active".to_owned(),
        active: true,
        served_to_agents: true,
        approval_required: false,
        title: row.try_get("name").unwrap_or_default(),
        summary: Some(truncate_summary(&description, 240)),
        origin: row.try_get("origin").ok(),
        source_repo: row.try_get("source_repo").ok(),
        file_patterns: parse_string_list(file_patterns_raw.as_deref()),
        updated_at: row.try_get("updated_at").ok(),
        review_hint: Some("served to agents when recall matches".to_owned()),
        commands: MemoryItemCommands {
            show: format!("difflore memory show rule:{id}"),
            approve: None,
            reject: None,
        },
    };
    let activity = load_memory_activity(
        pool,
        MemoryActivityFilter {
            rule_id: Some(id),
            repo_full_name: None,
            days: 30,
            limit: 100,
        },
    )
    .await
    .ok()
    .map(|activity| activity.summary);
    Ok(Some(MemoryItemDetail {
        item,
        body: description,
        provenance: None,
        activity,
    }))
}

fn normalize_memory_filter(value: &str) -> String {
    value.trim().to_ascii_lowercase().replace('_', "-")
}

fn state_matches(state: Option<&str>, item: &MemoryListItem) -> bool {
    match state {
        None | Some("" | "all") => true,
        Some("pending") => matches!(item.state.as_str(), "draft" | "candidate"),
        Some("candidate" | "candidates") => item.state == "candidate",
        Some("draft" | "drafts") => item.state == "draft",
        Some("active" | "rule" | "rules") => item.state == "active",
        Some(other) => item.state == other,
    }
}

fn kind_matches(kind: Option<&str>, item: &MemoryListItem) -> bool {
    match kind {
        None | Some("" | "all") => true,
        Some("pending") => matches!(item.kind.as_str(), "draft" | "candidate"),
        Some("rules") => item.kind == "rule",
        Some("drafts") => item.kind == "draft",
        Some("candidates" | "session" | "session-mined") => item.kind == "candidate",
        Some(other) => item.kind == other,
    }
}

fn repo_matches(repo: Option<&str>, item: &MemoryListItem) -> bool {
    match repo {
        None => true,
        Some(repo) => {
            item.source_repo
                .as_deref()
                .map(str::to_ascii_lowercase)
                .as_deref()
                == Some(repo)
        }
    }
}

fn query_matches(query: Option<&str>, item: &MemoryListItem) -> bool {
    let Some(query) = query else {
        return true;
    };
    item.item_id.to_ascii_lowercase().contains(query)
        || item.title.to_ascii_lowercase().contains(query)
        || item
            .summary
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase()
            .contains(query)
        || item
            .file_patterns
            .iter()
            .any(|pattern| pattern.to_ascii_lowercase().contains(query))
}

fn session_review_hint(verdict: &str) -> String {
    let trimmed = verdict.trim();
    if trimmed.eq_ignore_ascii_case("KEEP") {
        "review as a new memory".to_owned()
    } else if trimmed.eq_ignore_ascii_case("DROP") {
        "probably reject".to_owned()
    } else if let Some(target) = trimmed.strip_prefix("MERGE:") {
        format!("merge with existing memory `{}`", target.trim())
    } else if trimmed.is_empty() {
        "needs review".to_owned()
    } else {
        format!("needs review ({trimmed})")
    }
}

fn truncate_summary(value: &str, limit: usize) -> String {
    let mut chars = value.chars();
    let truncated: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
}

fn memory_activity_note() -> String {
    "Activity means a rule was retrieved or surfaced to an agent. It is not proof by itself that the rule changed the final code.".to_owned()
}

fn normalize_limit(limit: usize) -> usize {
    if limit == 0 {
        DEFAULT_LATEST_LIMIT
    } else {
        limit.min(MAX_LATEST_LIMIT)
    }
}

fn limit_i64(limit: usize) -> i64 {
    i64::try_from(normalize_limit(limit)).unwrap_or(MAX_LATEST_LIMIT as i64)
}

pub fn parse_session_item_id(item_id: &str) -> Result<String> {
    let Some((prefix, value)) = item_id.split_once(':') else {
        return Err(CoreError::Validation(
            "expected item id like session:<content_hash>".to_owned(),
        ));
    };
    let value = value.trim();
    if value.is_empty() {
        return Err(CoreError::Validation(
            "memory inbox item id is missing its value".to_owned(),
        ));
    }
    match prefix {
        "session" => Ok(value.to_owned()),
        _ => Err(CoreError::Validation(format!(
            "unsupported memory inbox item prefix `{prefix}`"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud::session_mined::{SessionMinedCandidate, SessionMinedCandidateArgs};
    use crate::infra::git::RepoScope;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;

    async fn fresh_pool() -> SqlitePool {
        let opts = SqliteConnectOptions::new()
            .filename(":memory:")
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .expect("connect in-memory sqlite");
        create_schema(&pool).await;
        pool
    }

    async fn migrated_pool() -> SqlitePool {
        let _home = crate::infra::db::shared_test_home();
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")
            .expect("sqlite opts")
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .expect("connect migrated sqlite");
        crate::infra::db::run_migrations(&pool)
            .await
            .expect("migrate");
        pool
    }

    async fn create_schema(pool: &SqlitePool) {
        sqlx::query(
            "CREATE TABLE skills (
                id TEXT PRIMARY KEY NOT NULL,
                name TEXT NOT NULL,
                origin TEXT NOT NULL DEFAULT 'manual',
                source_repo TEXT,
                file_patterns TEXT,
                status TEXT NOT NULL,
                installed_at TEXT DEFAULT (datetime('now')) NOT NULL,
                updated_at TEXT DEFAULT (datetime('now')) NOT NULL
            )",
        )
        .execute(pool)
        .await
        .expect("create skills");
        sqlx::query(
            "CREATE TABLE cloud_outbox (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                kind TEXT NOT NULL,
                payload_json TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending',
                retry_count INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL,
                claimed_at INTEGER,
                last_error TEXT,
                enriched_at INTEGER
            )",
        )
        .execute(pool)
        .await
        .expect("create cloud_outbox");
        sqlx::query(
            "CREATE TABLE observation_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                event_type TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending'
            )",
        )
        .execute(pool)
        .await
        .expect("create observation_events");
        sqlx::query(
            "CREATE TABLE mcp_rule_serves (
                id INTEGER PRIMARY KEY AUTOINCREMENT
            )",
        )
        .execute(pool)
        .await
        .expect("create mcp_rule_serves");
    }

    fn candidate(title: &str) -> SessionMinedCandidate {
        SessionMinedCandidate::try_new(SessionMinedCandidateArgs {
            session_id: "sess-1".to_owned(),
            ts_ms: 1_714_000_000_000,
            source_repo: RepoScope::canonical("owner/repo").expect("scope"),
            title: title.to_owned(),
            body: "Prefer typed parsing for queue payloads.".to_owned(),
            file_patterns: vec!["src/**/*.rs".to_owned()],
            gate_model: "claude:haiku".to_owned(),
            gate_verdict: "KEEP".to_owned(),
        })
        .expect("candidate")
    }

    #[tokio::test]
    async fn counts_active_rules_drafts_queues_and_usage() {
        let pool = fresh_pool().await;
        sqlx::query(
            "INSERT INTO skills (id, name, origin, source_repo, file_patterns, status) \
             VALUES ('rule-1', 'Use typed parsing', 'manual', 'owner/repo', '[\"src/**/*.rs\"]', 'active'), \
                    ('draft-1', 'Draft tests', 'pr_review', 'owner/repo', '[\"tests/**/*.rs\"]', 'pending')",
        )
        .execute(&pool)
        .await
        .expect("insert skills");
        sqlx::query(
            "INSERT INTO cloud_outbox (kind, payload_json, status, created_at) \
             VALUES ('observation', '{}', 'pending', 1)",
        )
        .execute(&pool)
        .await
        .expect("insert outbox");
        sqlx::query(
            "INSERT INTO observation_events (event_type, status) \
             VALUES ('mcp_rule_served', 'pending')",
        )
        .execute(&pool)
        .await
        .expect("insert observation");
        sqlx::query("INSERT INTO mcp_rule_serves DEFAULT VALUES")
            .execute(&pool)
            .await
            .expect("insert serve");

        let inbox = load_memory_inbox(&pool, 5).await.expect("inbox");

        assert_eq!(inbox.active_rule_count(), 1);
        assert_eq!(inbox.local_draft_count(), 1);
        assert_eq!(inbox.cloud_observations_pending(), 1);
        assert_eq!(inbox.observation_events_pending(), 1);
        assert_eq!(inbox.usage.local_agent_serves, 2);
        assert_eq!(inbox.active_rules.latest[0].file_patterns, ["src/**/*.rs"]);
    }

    #[tokio::test]
    async fn memory_items_respects_explicit_limit_below_default_preview() {
        let pool = fresh_pool().await;
        sqlx::query(
            "INSERT INTO skills (id, name, origin, status) \
             VALUES ('rule-1', 'Use typed parsing', 'manual', 'active'), \
                    ('rule-2', 'Prefer narrow scopes', 'manual', 'active'), \
                    ('rule-3', 'Keep proof links', 'manual', 'active')",
        )
        .execute(&pool)
        .await
        .expect("insert skills");

        let list = load_memory_items(
            &pool,
            MemoryListFilter {
                state: Some("active".to_owned()),
                kind: Some("rule".to_owned()),
                repo_full_name: None,
                query: None,
                limit: 1,
            },
        )
        .await
        .expect("memory list");

        assert_eq!(list.counts.active, 3);
        assert_eq!(list.items.len(), 1);
    }

    #[tokio::test]
    async fn parses_session_mined_candidates_and_uses_content_hash_ids() {
        let pool = fresh_pool().await;
        let candidate = candidate("Prefer typed deserialization");
        let hash = candidate.content_hash.clone();
        let payload = serde_json::to_string(&candidate).expect("payload");
        sqlx::query(
            "INSERT INTO cloud_outbox (kind, payload_json, status, retry_count, created_at, last_error) \
             VALUES (?1, ?2, 'abandoned', 8, 42, 'previous sync outage')",
        )
        .bind(kind::SESSION_MINED_CANDIDATE)
        .bind(payload)
        .execute(&pool)
        .await
        .expect("insert session candidate");

        let inbox = load_memory_inbox(&pool, 5).await.expect("inbox");

        assert_eq!(inbox.session_mined_count(), 1);
        assert_eq!(inbox.memory_candidates_pending(), 0);
        assert_eq!(
            inbox.local_discoveries.latest[0].item_id,
            format!("session:{hash}")
        );
        assert_eq!(
            find_session_mined_by_content_hash(&pool, &hash)
                .await
                .expect("lookup")
                .expect("candidate")
                .title,
            "Prefer typed deserialization"
        );
    }

    #[tokio::test]
    async fn corrupt_session_mined_json_is_a_warning_not_a_failure() {
        let pool = fresh_pool().await;
        sqlx::query(
            "INSERT INTO cloud_outbox (kind, payload_json, status, retry_count, created_at) \
             VALUES (?1, '{not-json', 'pending', 0, 42)",
        )
        .bind(kind::SESSION_MINED_CANDIDATE)
        .execute(&pool)
        .await
        .expect("insert corrupt row");

        let inbox = load_memory_inbox(&pool, 5).await.expect("inbox");

        assert_eq!(inbox.session_mined_count(), 1);
        assert!(inbox.local_discoveries.latest.is_empty());
        assert!(inbox.warnings.iter().any(|warning| {
            warning.code == "session_mined_parse_failed" && warning.outbox_id == Some(1)
        }));
    }

    #[tokio::test]
    async fn local_triage_statuses_hide_session_mined_candidates() {
        let pool = fresh_pool().await;
        for (idx, status) in [
            SessionMinedLocalTriageStatus::SupersededBy,
            SessionMinedLocalTriageStatus::ClusteredInto,
            SessionMinedLocalTriageStatus::DroppedLowSignal,
        ]
        .into_iter()
        .enumerate()
        {
            let candidate = candidate(&format!("Prefer typed deserialization {idx}"));
            let hash = candidate.content_hash.clone();
            let payload = serde_json::to_string(&candidate).expect("payload");
            sqlx::query(
                "INSERT INTO cloud_outbox (kind, payload_json, status, retry_count, created_at) \
                 VALUES (?1, ?2, 'pending', 0, ?3)",
            )
            .bind(kind::SESSION_MINED_CANDIDATE)
            .bind(payload)
            .bind(i64::try_from(idx + 1).expect("created_at"))
            .execute(&pool)
            .await
            .expect("insert session candidate");
            assert_eq!(
                set_candidate_triage(&pool, &hash, status, "covered by canonical", Some("canon"))
                    .await
                    .expect("set triage"),
                Some(i64::try_from(idx + 1).expect("row id"))
            );
        }
        let visible = candidate("Visible candidate");
        let visible_hash = visible.content_hash.clone();
        let payload = serde_json::to_string(&visible).expect("payload");
        sqlx::query(
            "INSERT INTO cloud_outbox (kind, payload_json, status, retry_count, created_at) \
             VALUES (?1, ?2, 'pending', 0, 10)",
        )
        .bind(kind::SESSION_MINED_CANDIDATE)
        .bind(payload)
        .execute(&pool)
        .await
        .expect("insert visible candidate");

        let inbox = load_memory_inbox(&pool, 10).await.expect("inbox");

        assert_eq!(inbox.session_mined_count(), 1);
        assert_eq!(inbox.local_discoveries.latest.len(), 1);
        assert_eq!(
            inbox.local_discoveries.latest[0].item_id,
            format!("session:{visible_hash}")
        );
    }

    #[tokio::test]
    async fn unknown_local_triage_status_remains_visible() {
        let pool = fresh_pool().await;
        let candidate = candidate("Visible candidate with future triage");
        let hash = candidate.content_hash.clone();
        let mut payload = serde_json::to_value(&candidate).expect("payload");
        payload["localTriage"] = serde_json::json!({
            "status": "future_status",
            "reason": "future client wrote a status this build does not know",
            "at": 1_714_000_000_001i64
        });
        sqlx::query(
            "INSERT INTO cloud_outbox (kind, payload_json, status, retry_count, created_at) \
             VALUES (?1, ?2, 'pending', 0, 42)",
        )
        .bind(kind::SESSION_MINED_CANDIDATE)
        .bind(serde_json::to_string(&payload).expect("payload json"))
        .execute(&pool)
        .await
        .expect("insert session candidate");

        let inbox = load_memory_inbox(&pool, 5).await.expect("inbox");

        assert_eq!(inbox.session_mined_count(), 1);
        assert_eq!(
            inbox.local_discoveries.latest[0].item_id,
            format!("session:{hash}")
        );
        assert!(inbox.warnings.is_empty());
    }

    #[tokio::test]
    async fn set_candidate_triage_leaves_outbox_status_untouched() {
        let pool = fresh_pool().await;
        let candidate = candidate("Prefer typed deserialization");
        let hash = candidate.content_hash.clone();
        let payload = serde_json::to_string(&candidate).expect("payload");
        sqlx::query(
            "INSERT INTO cloud_outbox (kind, payload_json, status, retry_count, created_at) \
             VALUES (?1, ?2, 'abandoned', 8, 42)",
        )
        .bind(kind::SESSION_MINED_CANDIDATE)
        .bind(payload)
        .execute(&pool)
        .await
        .expect("insert session candidate");

        let row_id = set_candidate_triage(
            &pool,
            &hash,
            SessionMinedLocalTriageStatus::DroppedLowSignal,
            "single-session implementation detail",
            None,
        )
        .await
        .expect("set triage")
        .expect("row");

        let outbox_status: String =
            sqlx::query_scalar("SELECT status FROM cloud_outbox WHERE id = ?1")
                .bind(row_id)
                .fetch_one(&pool)
                .await
                .expect("outbox status");
        assert_eq!(outbox_status, "abandoned");
        let payload_json: String =
            sqlx::query_scalar("SELECT payload_json FROM cloud_outbox WHERE id = ?1")
                .bind(row_id)
                .fetch_one(&pool)
                .await
                .expect("payload");
        let payload: Value = serde_json::from_str(&payload_json).expect("payload json");
        assert_eq!(
            payload["localTriage"]["status"],
            serde_json::json!("dropped_low_signal")
        );

        let inbox = load_memory_inbox(&pool, 5).await.expect("inbox");
        assert_eq!(inbox.session_mined_count(), 0);
        assert!(inbox.local_discoveries.latest.is_empty());
    }

    #[tokio::test]
    async fn set_candidate_triage_skips_existing_triage_and_marks_visible_duplicate_hash_rows() {
        let pool = fresh_pool().await;
        let candidate = candidate("Prefer typed deserialization");
        let hash = candidate.content_hash.clone();
        let mut triaged_payload = serde_json::to_value(&candidate).expect("payload");
        triaged_payload["localTriage"] = serde_json::json!({
            "status": "superseded_by",
            "reason": "covered by canonical",
            "ref": "canonical-hash",
            "at": 1_714_000_000_001i64
        });

        sqlx::query(
            "INSERT INTO cloud_outbox (kind, payload_json, status, retry_count, created_at) \
             VALUES (?1, ?2, 'pending', 0, 2)",
        )
        .bind(kind::SESSION_MINED_CANDIDATE)
        .bind(serde_json::to_string(&triaged_payload).expect("triaged payload json"))
        .execute(&pool)
        .await
        .expect("insert triaged session candidate");
        let visible_id = sqlx::query(
            "INSERT INTO cloud_outbox (kind, payload_json, status, retry_count, created_at) \
             VALUES (?1, ?2, 'pending', 0, 1)",
        )
        .bind(kind::SESSION_MINED_CANDIDATE)
        .bind(serde_json::to_string(&candidate).expect("payload json"))
        .execute(&pool)
        .await
        .expect("insert visible duplicate")
        .last_insert_rowid();

        let row_id = set_candidate_triage(
            &pool,
            &hash,
            SessionMinedLocalTriageStatus::DroppedLowSignal,
            "single-session candidate aged out without repeated evidence",
            None,
        )
        .await
        .expect("set triage")
        .expect("row");

        assert_eq!(row_id, visible_id);
        let visible_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM cloud_outbox \
             WHERE kind = ?1 AND json_extract(payload_json, '$.localTriage.status') IS NULL",
        )
        .bind(kind::SESSION_MINED_CANDIDATE)
        .fetch_one(&pool)
        .await
        .expect("visible count");
        assert_eq!(visible_count, 0);
    }

    #[tokio::test]
    async fn approved_session_candidate_with_local_triage_is_not_counted_pending() {
        let pool = fresh_pool().await;
        let candidate = candidate("Prefer typed deserialization");
        let mut payload = serde_json::to_value(&candidate).expect("payload");
        payload["localReview"] = serde_json::json!({
            "status": "approved",
            "ruleId": "rule-1",
            "reviewedAtMs": 1_714_000_000_001i64
        });
        payload["localTriage"] = serde_json::json!({
            "status": "dropped_low_signal",
            "reason": "noise",
            "at": 1_714_000_000_002i64
        });
        sqlx::query(
            "INSERT INTO cloud_outbox (kind, payload_json, status, retry_count, created_at) \
             VALUES (?1, ?2, 'pending', 0, 42)",
        )
        .bind(kind::SESSION_MINED_CANDIDATE)
        .bind(serde_json::to_string(&payload).expect("payload json"))
        .execute(&pool)
        .await
        .expect("insert session candidate");

        let inbox = load_memory_inbox(&pool, 5).await.expect("inbox");

        assert_eq!(inbox.session_mined_count(), 0);
        assert_eq!(inbox.memory_candidates_pending(), 0);
    }

    #[tokio::test]
    async fn delete_session_mined_by_hash_uses_session_item_ids_and_skips_approved_rows() {
        let pool = fresh_pool().await;
        let candidate = candidate("Prefer typed deserialization");
        let hash = candidate.content_hash.clone();
        let mut approved_payload = serde_json::to_value(&candidate).expect("approved payload");
        approved_payload["localReview"] = serde_json::json!({
            "status": "approved",
            "ruleId": "rule-1",
            "reviewedAtMs": 1_714_000_000_001i64
        });
        sqlx::query(
            "INSERT INTO cloud_outbox (kind, payload_json, status, retry_count, created_at) \
             VALUES (?1, ?2, 'pending', 0, 1), (?1, ?3, 'pending', 0, 2)",
        )
        .bind(kind::SESSION_MINED_CANDIDATE)
        .bind(serde_json::to_string(&candidate).expect("candidate payload"))
        .bind(serde_json::to_string(&approved_payload).expect("approved payload json"))
        .execute(&pool)
        .await
        .expect("insert session candidates");

        let deleted = delete_session_mined_candidates_by_content_hash(&pool, &hash)
            .await
            .expect("delete by hash");

        assert_eq!(deleted.len(), 1);
        assert_eq!(deleted[0].item_id, format!("session:{hash}"));
        let remaining_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM cloud_outbox WHERE kind = ?1")
                .bind(kind::SESSION_MINED_CANDIDATE)
                .fetch_one(&pool)
                .await
                .expect("remaining count");
        assert_eq!(remaining_count, 1);
        let remaining_payload: String =
            sqlx::query_scalar("SELECT payload_json FROM cloud_outbox WHERE kind = ?1")
                .bind(kind::SESSION_MINED_CANDIDATE)
                .fetch_one(&pool)
                .await
                .expect("remaining payload");
        assert!(session_mined_locally_approved_payload(&remaining_payload));
    }

    #[tokio::test]
    async fn set_candidate_triage_validates_reason_and_required_reference() {
        let pool = fresh_pool().await;
        let candidate = candidate("Prefer typed deserialization");
        let hash = candidate.content_hash.clone();
        let payload = serde_json::to_string(&candidate).expect("payload");
        sqlx::query(
            "INSERT INTO cloud_outbox (kind, payload_json, status, retry_count, created_at) \
             VALUES (?1, ?2, 'pending', 0, 42)",
        )
        .bind(kind::SESSION_MINED_CANDIDATE)
        .bind(payload)
        .execute(&pool)
        .await
        .expect("insert session candidate");

        assert!(
            set_candidate_triage(
                &pool,
                &hash,
                SessionMinedLocalTriageStatus::DroppedLowSignal,
                " ",
                None,
            )
            .await
            .is_err()
        );
        assert!(
            set_candidate_triage(
                &pool,
                &hash,
                SessionMinedLocalTriageStatus::SupersededBy,
                "covered by canonical",
                None,
            )
            .await
            .is_err()
        );
        assert!(
            set_candidate_triage(
                &pool,
                &hash,
                SessionMinedLocalTriageStatus::ClusteredInto,
                "covered by canonical",
                Some(&hash),
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn approve_session_mined_candidate_promotes_local_rule_and_keeps_sync_row() {
        let pool = migrated_pool().await;
        let candidate = candidate("Prefer typed deserialization");
        let hash = candidate.content_hash.clone();
        let payload = serde_json::to_string(&candidate).expect("payload");
        sqlx::query(
            "INSERT INTO cloud_outbox (kind, payload_json, status, retry_count, created_at) \
             VALUES (?1, ?2, 'pending', 0, 42)",
        )
        .bind(kind::SESSION_MINED_CANDIDATE)
        .bind(payload)
        .execute(&pool)
        .await
        .expect("insert session candidate");

        let approved = approve_session_mined_candidate(&pool, &hash)
            .await
            .expect("approve locally");

        assert_eq!(approved.content_hash, hash);
        assert_eq!(approved.rule.name, "Prefer typed deserialization");
        let status: String = sqlx::query_scalar("SELECT status FROM skills WHERE id = ?1")
            .bind(&approved.rule.id)
            .fetch_one(&pool)
            .await
            .expect("status");
        assert_eq!(status, "active");
        let source_repo: String =
            sqlx::query_scalar("SELECT source_repo FROM skills WHERE id = ?1")
                .bind(&approved.rule.id)
                .fetch_one(&pool)
                .await
                .expect("source repo");
        assert_eq!(source_repo, "owner/repo");
        let outbox_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cloud_outbox")
            .fetch_one(&pool)
            .await
            .expect("outbox count");
        assert_eq!(outbox_count, 1);

        let outbox_status: String =
            sqlx::query_scalar("SELECT status FROM cloud_outbox WHERE id = ?1")
                .bind(approved.outbox_id)
                .fetch_one(&pool)
                .await
                .expect("outbox status");
        assert_eq!(outbox_status, "pending");
        let retry_count: i64 =
            sqlx::query_scalar("SELECT retry_count FROM cloud_outbox WHERE id = ?1")
                .bind(approved.outbox_id)
                .fetch_one(&pool)
                .await
                .expect("retry count");
        assert_eq!(retry_count, 0);
        let last_error: Option<String> =
            sqlx::query_scalar("SELECT last_error FROM cloud_outbox WHERE id = ?1")
                .bind(approved.outbox_id)
                .fetch_one(&pool)
                .await
                .expect("last error");
        assert_eq!(last_error, None);

        let marked_payload: String =
            sqlx::query_scalar("SELECT payload_json FROM cloud_outbox WHERE id = ?1")
                .bind(approved.outbox_id)
                .fetch_one(&pool)
                .await
                .expect("marked payload");
        assert!(session_mined_locally_approved_payload(&marked_payload));
        let wire_candidate: SessionMinedCandidate =
            serde_json::from_str(&marked_payload).expect("wire candidate still decodes");
        assert_eq!(wire_candidate.content_hash, hash);

        let inbox = load_memory_inbox(&pool, 5)
            .await
            .expect("approved local item is hidden from local review");
        assert_eq!(inbox.session_mined_count(), 0);
        assert!(inbox.local_discoveries.latest.is_empty());
        assert_eq!(inbox.memory_candidates_pending(), 1);
        assert!(
            find_session_mined_by_content_hash(&pool, &hash)
                .await
                .expect("lookup")
                .is_none()
        );
    }

    #[tokio::test]
    async fn reject_session_mined_candidate_deletes_outbox_row() {
        let pool = migrated_pool().await;
        let candidate = candidate("Prefer typed deserialization");
        let hash = candidate.content_hash.clone();
        let payload = serde_json::to_string(&candidate).expect("payload");
        sqlx::query(
            "INSERT INTO cloud_outbox (kind, payload_json, status, retry_count, created_at) \
             VALUES (?1, ?2, 'pending', 0, 42)",
        )
        .bind(kind::SESSION_MINED_CANDIDATE)
        .bind(payload)
        .execute(&pool)
        .await
        .expect("insert session candidate");

        let rejected = reject_session_mined_candidate(&pool, &hash)
            .await
            .expect("reject locally");

        assert_eq!(rejected.content_hash, hash);
        let outbox_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cloud_outbox")
            .fetch_one(&pool)
            .await
            .expect("outbox count");
        assert_eq!(outbox_count, 0);
    }

    #[tokio::test]
    async fn reject_session_mined_candidate_can_clear_invalid_candidate_rows() {
        let pool = migrated_pool().await;
        let candidate = candidate("Prefer typed deserialization");
        let hash = candidate.content_hash.clone();
        let mut payload = serde_json::to_value(&candidate).expect("payload");
        payload["file_patterns"] = serde_json::json!([]);
        sqlx::query(
            "INSERT INTO cloud_outbox (kind, payload_json, status, retry_count, created_at) \
             VALUES (?1, ?2, 'pending', 0, 42)",
        )
        .bind(kind::SESSION_MINED_CANDIDATE)
        .bind(serde_json::to_string(&payload).expect("payload json"))
        .execute(&pool)
        .await
        .expect("insert invalid session candidate");

        let rejected = reject_session_mined_candidate(&pool, &hash)
            .await
            .expect("reject invalid candidate locally");

        assert_eq!(rejected.content_hash, hash);
        let outbox_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cloud_outbox")
            .fetch_one(&pool)
            .await
            .expect("outbox count");
        assert_eq!(outbox_count, 0);
    }

    #[test]
    fn parses_session_item_ids() {
        assert_eq!(
            parse_session_item_id("session:abc123").expect("session id"),
            "abc123".to_owned()
        );
        assert!(parse_session_item_id("draft:abc").is_err());
        assert!(parse_session_item_id("candidate:abc").is_err());
    }
}
