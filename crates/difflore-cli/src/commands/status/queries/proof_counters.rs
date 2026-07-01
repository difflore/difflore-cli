//! Repo-scoped proof counters: accepted-edit signatures, recall events, and
//! MCP rule serves. These are the headline "memory is being used" numbers the
//! `status` envelope reports, plus the shared window constants and repo-alias
//! normaliser the other query domains build on.

use sqlx::Row;

use super::super::transform::normalize_repo;

pub(super) const LOCAL_PROOF_WINDOW_DAYS: i64 = 30;
pub(super) const LOCAL_ACCEPTED_RECALL_LOOKBACK_DAYS: i64 = 7;
pub(super) const REVIEW_MINUTES_PER_ACCEPTED_PROOF: i64 = 4;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::commands::status) struct LocalAcceptedProof {
    pub(in crate::commands::status) window_days: i64,
    pub(in crate::commands::status) recall_lookback_days: i64,
    pub(in crate::commands::status) accepted_proof_signatures: i64,
    pub(in crate::commands::status) accepted_hook_outcomes: i64,
    pub(in crate::commands::status) accepted_outcomes_linked_to_prior_recall: i64,
    pub(in crate::commands::status) accepted_outcomes_linked_to_recall_or_edit_proof: i64,
    pub(in crate::commands::status) accepted_outcomes_linked_to_rule_recall: i64,
    pub(in crate::commands::status) accepted_outcomes_linked_to_mcp_rule_serve: i64,
    pub(in crate::commands::status) accepted_outcomes_linked_to_edit_attribution: i64,
    pub(in crate::commands::status) estimated_saved_review_minutes: i64,
    /// Whole-machine accepted outcomes that actually applied (the
    /// repo-scoped fields above remain repo-scoped).
    pub(in crate::commands::status) accepted_and_applied: i64,
    /// Accepted outcomes whose patch never reached disk.
    pub(in crate::commands::status) accepted_but_failed: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::commands::status) struct LocalMcpRuleServe {
    pub(in crate::commands::status) window_days: i64,
    pub(in crate::commands::status) calls: i64,
    pub(in crate::commands::status) empty_calls: i64,
    pub(in crate::commands::status) rules_served: i64,
    pub(in crate::commands::status) strict_matches: i64,
    pub(in crate::commands::status) estimated_tokens: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::commands::status) struct LocalRecallProof {
    pub(in crate::commands::status) window_days: i64,
    pub(in crate::commands::status) recall_events: i64,
    pub(in crate::commands::status) recalled_rules: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::commands::status) struct AcceptedEditProofFunnel {
    pub(in crate::commands::status) window_days: i64,
    pub(in crate::commands::status) stage: String,
    pub(in crate::commands::status) ready_for_cloud_value: bool,
    pub(in crate::commands::status) blockers: Vec<String>,
    pub(in crate::commands::status) next_commands: Vec<String>,
    pub(in crate::commands::status) repo_scope_ready: bool,
    pub(in crate::commands::status) agent_recall_ready: bool,
    pub(in crate::commands::status) accepted_edit_captured: bool,
    pub(in crate::commands::status) accepted_edit_rows_last30: i64,
    pub(in crate::commands::status) accepted_edit_rows_for_current_repo: i64,
    pub(in crate::commands::status) accepted_edit_rows_without_repo: i64,
    pub(in crate::commands::status) accepted_edit_upload_pending: i64,
    pub(in crate::commands::status) accepted_edit_upload_failed: i64,
    pub(in crate::commands::status) accepted_edit_rows_missing_rule_ids: i64,
    pub(in crate::commands::status) accepted_edit_rows_with_cloud_rule_ids: i64,
    pub(in crate::commands::status) accepted_edit_rows_with_local_rule_ids: i64,
    pub(in crate::commands::status) last_upload_error: Option<String>,
}

impl LocalAcceptedProof {
    const fn empty() -> Self {
        Self {
            window_days: LOCAL_PROOF_WINDOW_DAYS,
            recall_lookback_days: LOCAL_ACCEPTED_RECALL_LOOKBACK_DAYS,
            accepted_proof_signatures: 0,
            accepted_hook_outcomes: 0,
            accepted_outcomes_linked_to_prior_recall: 0,
            accepted_outcomes_linked_to_recall_or_edit_proof: 0,
            accepted_outcomes_linked_to_rule_recall: 0,
            accepted_outcomes_linked_to_mcp_rule_serve: 0,
            accepted_outcomes_linked_to_edit_attribution: 0,
            estimated_saved_review_minutes: 0,
            accepted_and_applied: 0,
            accepted_but_failed: 0,
        }
    }
}

impl LocalMcpRuleServe {
    const fn empty() -> Self {
        Self {
            window_days: LOCAL_PROOF_WINDOW_DAYS,
            calls: 0,
            empty_calls: 0,
            rules_served: 0,
            strict_matches: 0,
            estimated_tokens: 0,
        }
    }
}

impl LocalRecallProof {
    const fn empty() -> Self {
        Self {
            window_days: LOCAL_PROOF_WINDOW_DAYS,
            recall_events: 0,
            recalled_rules: 0,
        }
    }
}

impl AcceptedEditProofFunnel {
    fn empty(repo_scope_ready: bool, agent_recall_ready: bool) -> Self {
        Self {
            window_days: LOCAL_PROOF_WINDOW_DAYS,
            stage: "no_accepted_edit_captured".to_owned(),
            ready_for_cloud_value: false,
            blockers: Vec::new(),
            next_commands: Vec::new(),
            repo_scope_ready,
            agent_recall_ready,
            accepted_edit_captured: false,
            accepted_edit_rows_last30: 0,
            accepted_edit_rows_for_current_repo: 0,
            accepted_edit_rows_without_repo: 0,
            accepted_edit_upload_pending: 0,
            accepted_edit_upload_failed: 0,
            accepted_edit_rows_missing_rule_ids: 0,
            accepted_edit_rows_with_cloud_rule_ids: 0,
            accepted_edit_rows_with_local_rule_ids: 0,
            last_upload_error: None,
        }
    }
}

pub(super) fn normalized_repo_aliases(repo_aliases: &[String]) -> Vec<String> {
    repo_aliases
        .iter()
        .map(|repo| normalize_repo(repo))
        .filter(|repo| !repo.is_empty())
        .collect()
}

pub(in crate::commands::status) async fn local_accepted_proof(
    db: &difflore_core::SqlitePool,
    repo_aliases: &[String],
) -> LocalAcceptedProof {
    let normalized_aliases = normalized_repo_aliases(repo_aliases);
    if normalized_aliases.is_empty() {
        return LocalAcceptedProof::empty();
    }

    let accepted_proof_signatures =
        accepted_signature_count_for_repos(db, &normalized_aliases).await;
    // Hook outcomes are intentionally not counted here yet: their local
    // observation payloads do not carry a canonical repo scope. Counting them
    // in `status` would let unrelated accepted edits make a fresh checkout look
    // proof-ready. Signed local fix outcomes below are repo-scoped.
    let (accepted_hook_outcomes, accepted_link_summary) = (
        0,
        difflore_core::cloud::observations::AcceptedRecallLinkSummary::default(),
    );
    let accepted_total = accepted_proof_signatures + accepted_hook_outcomes;
    let accepted_outcomes_linked_to_prior_recall = accepted_link_summary.linked_to_prior_recall;
    // This split is whole-machine on purpose; degrade to zero if the
    // auxiliary query fails so the repo-scoped proof still renders.
    let split =
        difflore_core::observability::fix_outcomes::split_summary(db, LOCAL_PROOF_WINDOW_DAYS)
            .await
            .unwrap_or(
                difflore_core::observability::fix_outcomes::FixOutcomeSplitSummary {
                    accepted_and_applied: 0,
                    accepted_but_failed: 0,
                },
            );
    LocalAcceptedProof {
        window_days: LOCAL_PROOF_WINDOW_DAYS,
        recall_lookback_days: LOCAL_ACCEPTED_RECALL_LOOKBACK_DAYS,
        accepted_proof_signatures,
        accepted_hook_outcomes,
        accepted_outcomes_linked_to_prior_recall,
        accepted_outcomes_linked_to_recall_or_edit_proof: accepted_outcomes_linked_to_prior_recall,
        accepted_outcomes_linked_to_rule_recall: accepted_link_summary.linked_to_rule_recall,
        accepted_outcomes_linked_to_mcp_rule_serve: accepted_link_summary.linked_to_mcp_rule_serve,
        accepted_outcomes_linked_to_edit_attribution: accepted_link_summary
            .linked_to_edit_attribution,
        estimated_saved_review_minutes: accepted_total * REVIEW_MINUTES_PER_ACCEPTED_PROOF,
        accepted_and_applied: split.accepted_and_applied,
        accepted_but_failed: split.accepted_but_failed,
    }
}

async fn accepted_signature_count_for_repos(
    db: &difflore_core::SqlitePool,
    normalized_repos: &[String],
) -> i64 {
    if normalized_repos.is_empty() {
        return 0;
    }
    let repos_json = serde_json::to_string(normalized_repos).unwrap_or_else(|_| "[]".to_owned());
    let window = format!("-{LOCAL_PROOF_WINDOW_DAYS} days");
    sqlx::query_scalar::<_, i64>(
        r"SELECT COUNT(DISTINCT f.diff_signature)
          FROM fix_outcomes f
          LEFT JOIN skills s ON s.id = f.rule_id
          WHERE f.accepted = 1
            AND f.applied_ok = 1
            AND f.diff_signature IS NOT NULL
            AND TRIM(f.diff_signature) <> ''
            AND datetime(f.created_at) >= datetime('now', ?1)
            AND (
              LOWER(COALESCE(f.repo_full_name, '')) IN (SELECT value FROM json_each(?2))
              OR LOWER(COALESCE(s.source_repo, '')) IN (SELECT value FROM json_each(?2))
            )",
    )
    .bind(window)
    .bind(repos_json)
    .fetch_one(db)
    .await
    .unwrap_or(0)
}

pub(in crate::commands::status) async fn local_recall_proof(
    db: &difflore_core::SqlitePool,
    repo_aliases: &[String],
) -> LocalRecallProof {
    let normalized_aliases = normalized_repo_aliases(repo_aliases);
    if normalized_aliases.is_empty() {
        return LocalRecallProof::empty();
    }
    let repos_json = serde_json::to_string(&normalized_aliases).unwrap_or_else(|_| "[]".to_owned());
    let window = format!("-{LOCAL_PROOF_WINDOW_DAYS} days");
    let row = sqlx::query(
        r"SELECT
             COUNT(*) AS recall_events,
             COUNT(DISTINCT rule_id) AS recalled_rules
         FROM rule_outcomes
         WHERE kind = 'recalled'
           AND datetime(created_at) >= datetime('now', ?1)
           AND repo_full_name IS NOT NULL
           AND LOWER(repo_full_name) IN (SELECT value FROM json_each(?2))",
    )
    .bind(window)
    .bind(repos_json)
    .fetch_one(db)
    .await
    .ok();
    LocalRecallProof {
        window_days: LOCAL_PROOF_WINDOW_DAYS,
        recall_events: row
            .as_ref()
            .and_then(|row| row.try_get::<i64, _>("recall_events").ok())
            .unwrap_or(0),
        recalled_rules: row
            .as_ref()
            .and_then(|row| row.try_get::<i64, _>("recalled_rules").ok())
            .unwrap_or(0),
    }
}

pub(in crate::commands::status) async fn local_mcp_rule_serves(
    db: &difflore_core::SqlitePool,
    repo_aliases: &[String],
) -> LocalMcpRuleServe {
    let normalized_aliases = normalized_repo_aliases(repo_aliases);
    if normalized_aliases.is_empty() {
        return LocalMcpRuleServe::empty();
    }
    let summary = difflore_core::observability::mcp_rule_serves::summary_for_repos(
        db,
        &normalized_aliases,
        LOCAL_PROOF_WINDOW_DAYS,
    )
    .await
    .ok();
    LocalMcpRuleServe {
        window_days: LOCAL_PROOF_WINDOW_DAYS,
        calls: summary.as_ref().map_or(0, |row| row.calls),
        empty_calls: summary.as_ref().map_or(0, |row| row.empty_calls),
        rules_served: summary.as_ref().map_or(0, |row| row.rules_served),
        strict_matches: summary.as_ref().map_or(0, |row| row.strict_matches),
        estimated_tokens: summary.as_ref().map_or(0, |row| row.estimated_tokens),
    }
}

pub(in crate::commands::status) async fn accepted_edit_proof_funnel(
    db: &difflore_core::SqlitePool,
    repo_aliases: &[String],
    local_proof: &LocalAcceptedProof,
    local_mcp_serves: &LocalMcpRuleServe,
) -> AcceptedEditProofFunnel {
    let normalized_aliases = normalized_repo_aliases(repo_aliases);
    let repo_scope_ready = !normalized_aliases.is_empty();
    let agent_recall_ready = local_mcp_serves.rules_served > 0;
    let accepted_from_fix =
        local_proof.accepted_proof_signatures + local_proof.accepted_hook_outcomes;
    let rows = load_accepted_edit_outbox_rows(db).await;
    if rows.is_empty() && accepted_from_fix == 0 {
        let mut funnel = AcceptedEditProofFunnel::empty(repo_scope_ready, agent_recall_ready);
        push_base_blockers(&mut funnel);
        return funnel;
    }

    let mut funnel = AcceptedEditProofFunnel {
        window_days: LOCAL_PROOF_WINDOW_DAYS,
        stage: "accepted_edit_captured_locally".to_owned(),
        ready_for_cloud_value: false,
        blockers: Vec::new(),
        next_commands: Vec::new(),
        repo_scope_ready,
        agent_recall_ready,
        accepted_edit_captured: true,
        accepted_edit_rows_last30: i64::try_from(rows.len()).unwrap_or(i64::MAX),
        ..AcceptedEditProofFunnel::empty(repo_scope_ready, agent_recall_ready)
    };

    for row in rows {
        if row.status == "pending" || row.status == "claimed" || row.status == "parked" {
            funnel.accepted_edit_upload_pending += 1;
        }
        if row.status == "failed" || row.status == "abandoned" {
            funnel.accepted_edit_upload_failed += 1;
        }
        if row
            .last_error
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
        {
            funnel.last_upload_error = row.last_error.clone();
        }

        let repo = row.request.repo_full_name.as_deref().map(normalize_repo);
        if repo.as_deref().is_none_or(str::is_empty) {
            funnel.accepted_edit_rows_without_repo += 1;
        } else if normalized_aliases.is_empty()
            || repo
                .as_deref()
                .is_some_and(|repo| normalized_aliases.iter().any(|alias| alias == repo))
        {
            funnel.accepted_edit_rows_for_current_repo += 1;
        }

        if row.request.rule_ids.is_empty() {
            funnel.accepted_edit_rows_missing_rule_ids += 1;
        } else if row
            .request
            .rule_ids
            .iter()
            .all(|rule_id| looks_like_uuid(rule_id))
        {
            funnel.accepted_edit_rows_with_cloud_rule_ids += 1;
        } else {
            funnel.accepted_edit_rows_with_local_rule_ids += 1;
        }
    }

    if funnel.accepted_edit_upload_failed > 0 {
        funnel.stage = "accepted_edit_upload_failed".to_owned();
    } else if funnel.accepted_edit_upload_pending > 0 {
        funnel.stage = "accepted_edit_waiting_for_cloud_sync".to_owned();
    } else if funnel.accepted_edit_rows_with_local_rule_ids > 0 {
        funnel.stage = "accepted_edit_needs_cloud_rule_mapping".to_owned();
    } else if funnel.accepted_edit_rows_with_cloud_rule_ids > 0 || accepted_from_fix > 0 {
        funnel.stage = "accepted_edit_cloud_attribution_ready".to_owned();
        funnel.ready_for_cloud_value = true;
    }
    push_base_blockers(&mut funnel);
    if funnel.accepted_edit_upload_pending > 0 {
        funnel.next_commands.push("difflore cloud sync".to_owned());
    }
    if funnel.accepted_edit_rows_with_local_rule_ids > 0 {
        funnel
            .next_commands
            .push("difflore cloud publish --rule <rule-id>".to_owned());
    }
    funnel
}

fn looks_like_uuid(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    for (idx, byte) in bytes.iter().enumerate() {
        if matches!(idx, 8 | 13 | 18 | 23) {
            if *byte != b'-' {
                return false;
            }
        } else if !byte.is_ascii_hexdigit() {
            return false;
        }
    }
    true
}

fn push_base_blockers(funnel: &mut AcceptedEditProofFunnel) {
    if !funnel.repo_scope_ready {
        funnel.blockers.push("missing_repo_scope".to_owned());
        funnel
            .next_commands
            .push("difflore repo alias set owner/repo".to_owned());
    }
    if !funnel.agent_recall_ready {
        funnel.blockers.push("no_agent_rule_serve_yet".to_owned());
    }
    if !funnel.accepted_edit_captured {
        funnel
            .blockers
            .push("no_accepted_edit_captured_yet".to_owned());
    }
    if funnel.accepted_edit_rows_without_repo > 0 {
        funnel
            .blockers
            .push("accepted_edit_missing_repo".to_owned());
    }
    if funnel.accepted_edit_rows_missing_rule_ids > 0 {
        funnel
            .blockers
            .push("accepted_edit_missing_rule_ids".to_owned());
    }
    if funnel.accepted_edit_rows_with_local_rule_ids > 0 {
        funnel
            .blockers
            .push("accepted_edit_rule_ids_not_cloud_uuid".to_owned());
    }
    if funnel.accepted_edit_upload_failed > 0 {
        funnel
            .blockers
            .push("accepted_edit_upload_failed".to_owned());
    }
}

#[derive(Debug)]
struct AcceptedEditOutboxRow {
    status: String,
    last_error: Option<String>,
    request: difflore_core::contract::RecordAcceptedEditRequest,
}

async fn load_accepted_edit_outbox_rows(
    db: &difflore_core::SqlitePool,
) -> Vec<AcceptedEditOutboxRow> {
    let cutoff_ms = chrono::Utc::now()
        .timestamp_millis()
        .saturating_sub(LOCAL_PROOF_WINDOW_DAYS * 24 * 60 * 60 * 1_000);
    let rows = sqlx::query(
        "SELECT status, payload_json, last_error \
         FROM cloud_outbox \
         WHERE kind = 'accepted_edit' AND created_at >= ?1 \
         ORDER BY created_at DESC",
    )
    .bind(cutoff_ms)
    .fetch_all(db)
    .await
    .unwrap_or_default();

    rows.into_iter()
        .filter_map(|row| {
            let payload: String = row.try_get("payload_json").ok()?;
            let request =
                serde_json::from_str::<difflore_core::contract::RecordAcceptedEditRequest>(
                    &payload,
                )
                .ok()?;
            Some(AcceptedEditOutboxRow {
                status: row.try_get("status").unwrap_or_default(),
                last_error: row.try_get("last_error").ok().flatten(),
                request,
            })
        })
        .collect()
}
