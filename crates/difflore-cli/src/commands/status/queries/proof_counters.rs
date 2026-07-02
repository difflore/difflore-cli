//! Repo-scoped proof counters: accepted-edit signatures, recall events, and
//! MCP rule serves. These are the headline "memory is being used" numbers the
//! `status` envelope reports, plus the shared window constants and repo-alias
//! normaliser the other query domains build on.

use sqlx::Row;

use super::super::transform::normalize_repo;
use crate::support::proven_rule::accepted_link_summary_from_default_observation_store;

pub(super) const LOCAL_PROOF_WINDOW_DAYS: i64 = 30;
pub(super) const LOCAL_ACCEPTED_RECALL_LOOKBACK_DAYS: i64 = 7;
pub(super) const REVIEW_MINUTES_PER_ACCEPTED_PROOF: i64 = 4;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::commands::status) struct LocalAcceptedProof {
    pub(in crate::commands::status) window_days: i64,
    pub(in crate::commands::status) recall_lookback_days: i64,
    pub(in crate::commands::status) proof_grade: String,
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
    pub(in crate::commands::status) proof_grade: String,
    pub(in crate::commands::status) ready_for_cloud_value: bool,
    pub(in crate::commands::status) observed_value_ready: bool,
    pub(in crate::commands::status) auditable_accepted_edit_ready: bool,
    pub(in crate::commands::status) launch_grade_paid_value_ready: bool,
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
    fn empty() -> Self {
        Self {
            window_days: LOCAL_PROOF_WINDOW_DAYS,
            recall_lookback_days: LOCAL_ACCEPTED_RECALL_LOOKBACK_DAYS,
            proof_grade: "none".to_owned(),
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
            proof_grade: "none".to_owned(),
            ready_for_cloud_value: false,
            observed_value_ready: false,
            auditable_accepted_edit_ready: false,
            launch_grade_paid_value_ready: false,
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
    let accepted_link_summary = accepted_link_summary_from_default_observation_store(
        db,
        &normalized_aliases,
        LOCAL_PROOF_WINDOW_DAYS,
        LOCAL_ACCEPTED_RECALL_LOOKBACK_DAYS,
    )
    .await;
    let accepted_hook_outcomes = accepted_link_summary.accepted_outcomes;
    let accepted_outcomes_linked_to_prior_recall = accepted_link_summary.linked_to_prior_recall;
    let proof_grade = local_accepted_proof_grade(accepted_proof_signatures, accepted_hook_outcomes);
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
        proof_grade,
        accepted_proof_signatures,
        accepted_hook_outcomes,
        accepted_outcomes_linked_to_prior_recall,
        accepted_outcomes_linked_to_recall_or_edit_proof: accepted_outcomes_linked_to_prior_recall,
        accepted_outcomes_linked_to_rule_recall: accepted_link_summary.linked_to_rule_recall,
        accepted_outcomes_linked_to_mcp_rule_serve: accepted_link_summary.linked_to_mcp_rule_serve,
        accepted_outcomes_linked_to_edit_attribution: accepted_link_summary
            .linked_to_edit_attribution,
        estimated_saved_review_minutes: accepted_proof_signatures
            * REVIEW_MINUTES_PER_ACCEPTED_PROOF,
        accepted_and_applied: split.accepted_and_applied,
        accepted_but_failed: split.accepted_but_failed,
    }
}

fn local_accepted_proof_grade(
    accepted_proof_signatures: i64,
    accepted_hook_outcomes: i64,
) -> String {
    if accepted_proof_signatures > 0 {
        "auditable_accepted_edit".to_owned()
    } else if accepted_hook_outcomes > 0 {
        "observed_value".to_owned()
    } else {
        "none".to_owned()
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
          WHERE f.accepted = 1
            AND f.applied_ok = 1
            AND f.diff_signature IS NOT NULL
            AND TRIM(f.diff_signature) <> ''
            AND datetime(f.created_at) >= datetime('now', ?1)
            AND LOWER(COALESCE(f.repo_full_name, '')) IN (SELECT value FROM json_each(?2))",
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
    let receipt_summary = difflore_core::cloud::accepted_edit_receipts::summary_for_repos(
        db,
        &normalized_aliases,
        LOCAL_PROOF_WINDOW_DAYS,
    )
    .await
    .unwrap_or_default();
    let rows = load_accepted_edit_outbox_rows(db).await;

    let mut funnel = AcceptedEditProofFunnel {
        window_days: LOCAL_PROOF_WINDOW_DAYS,
        stage: "no_accepted_edit_captured".to_owned(),
        proof_grade: "none".to_owned(),
        ready_for_cloud_value: false,
        observed_value_ready: false,
        auditable_accepted_edit_ready: false,
        launch_grade_paid_value_ready: false,
        blockers: Vec::new(),
        next_commands: Vec::new(),
        repo_scope_ready,
        agent_recall_ready,
        accepted_edit_captured: false,
        accepted_edit_rows_last30: receipt_summary
            .rows_last30
            .saturating_add(i64::try_from(rows.len()).unwrap_or(i64::MAX)),
        accepted_edit_rows_for_current_repo: receipt_summary.rows_for_current_repo,
        accepted_edit_rows_without_repo: receipt_summary.rows_without_repo,
        accepted_edit_rows_missing_rule_ids: receipt_summary.rows_missing_rule_ids_for_current_repo,
        accepted_edit_rows_with_cloud_rule_ids: receipt_summary
            .rows_with_cloud_rule_ids_for_current_repo,
        accepted_edit_rows_with_local_rule_ids: receipt_summary
            .rows_with_local_rule_ids_for_current_repo,
        ..AcceptedEditProofFunnel::empty(repo_scope_ready, agent_recall_ready)
    };

    let mut queued_rows_for_current_repo = 0;
    let mut queued_rows_without_repo = 0;
    for row in rows {
        let repo = row.request.repo_full_name.as_deref().map(normalize_repo);
        let repo_matches = repo
            .as_deref()
            .is_some_and(|repo| normalized_aliases.iter().any(|alias| alias == repo));
        let repo_missing = repo.as_deref().is_none_or(str::is_empty);
        let row_relevant_to_current_scope = repo_matches || repo_missing;

        if !row_relevant_to_current_scope {
            continue;
        }

        if matches!(
            row.status.as_str(),
            "pending" | "claimed" | "parked" | "processing"
        ) {
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
            funnel.last_upload_error.clone_from(&row.last_error);
        }

        if repo_missing {
            queued_rows_without_repo += 1;
            funnel.accepted_edit_rows_without_repo += 1;
        } else if repo_matches {
            queued_rows_for_current_repo += 1;
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

    funnel.observed_value_ready = accepted_from_fix > 0
        || receipt_summary.rows_for_current_repo > 0
        || queued_rows_for_current_repo > 0;
    funnel.auditable_accepted_edit_ready =
        local_proof.accepted_proof_signatures > 0 || receipt_summary.rows_for_current_repo > 0;
    funnel.launch_grade_paid_value_ready = receipt_summary.launch_grade_rows_for_current_repo > 0;
    funnel.accepted_edit_captured = funnel.observed_value_ready
        || receipt_summary.rows_without_repo > 0
        || queued_rows_without_repo > 0;

    if funnel.accepted_edit_upload_failed > 0 {
        "accepted_edit_upload_failed".clone_into(&mut funnel.stage);
    } else if funnel.accepted_edit_upload_pending > 0 {
        "accepted_edit_waiting_for_cloud_sync".clone_into(&mut funnel.stage);
    } else if funnel.accepted_edit_rows_with_local_rule_ids > 0 {
        "accepted_edit_needs_cloud_rule_mapping".clone_into(&mut funnel.stage);
    } else if funnel.launch_grade_paid_value_ready {
        "accepted_edit_cloud_attribution_ready".clone_into(&mut funnel.stage);
    } else if funnel.auditable_accepted_edit_ready {
        "auditable_accepted_edit_ready".clone_into(&mut funnel.stage);
    } else if funnel.observed_value_ready {
        "observed_value_ready".clone_into(&mut funnel.stage);
    }
    funnel.proof_grade = funnel_proof_grade(
        funnel.observed_value_ready,
        funnel.auditable_accepted_edit_ready,
        funnel.launch_grade_paid_value_ready,
    );
    if funnel.launch_grade_paid_value_ready {
        funnel.ready_for_cloud_value = true;
    } else {
        funnel.ready_for_cloud_value = false;
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

fn funnel_proof_grade(
    observed_value_ready: bool,
    auditable_accepted_edit_ready: bool,
    launch_grade_paid_value_ready: bool,
) -> String {
    if launch_grade_paid_value_ready {
        "launch_grade_paid_value".to_owned()
    } else if auditable_accepted_edit_ready {
        "auditable_accepted_edit".to_owned()
    } else if observed_value_ready {
        "observed_value".to_owned()
    } else {
        "none".to_owned()
    }
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
    if !funnel.observed_value_ready {
        funnel.blockers.push("no_observed_value_yet".to_owned());
    }
    if !funnel.auditable_accepted_edit_ready {
        funnel
            .blockers
            .push("no_auditable_accepted_edit_yet".to_owned());
    }
    if !funnel.launch_grade_paid_value_ready {
        funnel
            .blockers
            .push("no_launch_grade_paid_attribution_yet".to_owned());
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

#[cfg(test)]
mod tests {
    use super::*;
    use difflore_core::cloud::accepted_edit_receipts::{
        AcceptedEditReceiptInsert, record_confirmed,
    };
    use sqlx::sqlite::SqlitePoolOptions;

    async fn setup() -> difflore_core::SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("open pool");
        difflore_core::infra::db::run_migrations(&pool)
            .await
            .expect("migrate");
        pool
    }

    fn local_mcp_serves() -> LocalMcpRuleServe {
        LocalMcpRuleServe {
            window_days: LOCAL_PROOF_WINDOW_DAYS,
            calls: 1,
            empty_calls: 0,
            rules_served: 1,
            strict_matches: 1,
            estimated_tokens: 10,
        }
    }

    fn receipt(local_key: &str) -> AcceptedEditReceiptInsert {
        receipt_for_repo(local_key, "Acme/App", false)
    }

    fn receipt_for_repo(
        local_key: &str,
        repo_full_name: &str,
        launch_grade: bool,
    ) -> AcceptedEditReceiptInsert {
        AcceptedEditReceiptInsert {
            cloud_acceptance_id: Some(format!("cloud-{local_key}")),
            local_receipt_key: local_key.to_owned(),
            repo_full_name: Some(repo_full_name.to_owned()),
            target_pr_number: Some(12),
            file_path: Some("src/app.ts".to_owned()),
            diff_signature: format!("diff-{local_key}"),
            rule_ids: vec!["550e8400-e29b-41d4-a716-446655440000".to_owned()],
            acceptance_source: "agent_retained_edit".to_owned(),
            client: Some("difflore_hook".to_owned()),
            team_id: Some("team-1".to_owned()),
            observations_inserted: 1,
            launch_grade,
        }
    }

    fn accepted_edit_payload(rule_ids: Vec<String>) -> String {
        serde_json::to_string(&difflore_core::contract::RecordAcceptedEditRequest {
            before_code: "old".to_owned(),
            after_code: "new".to_owned(),
            file_path: Some("src/app.ts".to_owned()),
            repo_full_name: Some("acme/app".to_owned()),
            target_pr_number: Some(12),
            language: Some("typescript".to_owned()),
            acceptance_source: Some("agent_retained_edit".to_owned()),
            client: Some("difflore_hook".to_owned()),
            diff_signature: Some("diff-one".to_owned()),
            rule_ids,
        })
        .expect("serialize request")
    }

    #[tokio::test]
    async fn accepted_edit_funnel_counts_confirmed_receipts_when_outbox_is_empty() {
        let pool = setup().await;
        record_confirmed(&pool, receipt("one"))
            .await
            .expect("record receipt");

        let funnel = accepted_edit_proof_funnel(
            &pool,
            &["acme/app".to_owned()],
            &LocalAcceptedProof::empty(),
            &local_mcp_serves(),
        )
        .await;

        assert!(funnel.accepted_edit_captured);
        assert_eq!(funnel.accepted_edit_rows_last30, 1);
        assert_eq!(funnel.accepted_edit_rows_for_current_repo, 1);
        assert_eq!(funnel.proof_grade, "auditable_accepted_edit");
        assert!(funnel.observed_value_ready);
        assert!(funnel.auditable_accepted_edit_ready);
        assert!(!funnel.launch_grade_paid_value_ready);
        assert!(!funnel.ready_for_cloud_value);
        assert_eq!(funnel.stage, "auditable_accepted_edit_ready");
        assert!(
            !funnel
                .blockers
                .contains(&"no_auditable_accepted_edit_yet".to_owned())
        );
        assert!(
            funnel
                .blockers
                .contains(&"no_launch_grade_paid_attribution_yet".to_owned())
        );
    }

    #[tokio::test]
    async fn accepted_edit_funnel_keeps_queue_failures_as_blockers() {
        let pool = setup().await;
        record_confirmed(&pool, receipt("one"))
            .await
            .expect("record receipt");
        sqlx::query(
            r"INSERT INTO cloud_outbox
              (kind, payload_json, status, retry_count, created_at, last_error)
              VALUES ('accepted_edit', ?1, 'failed', 1, ?2, 'boom')",
        )
        .bind(accepted_edit_payload(vec!["local-rule-1".to_owned()]))
        .bind(chrono::Utc::now().timestamp_millis())
        .execute(&pool)
        .await
        .expect("insert failed accepted edit outbox row");

        let funnel = accepted_edit_proof_funnel(
            &pool,
            &["acme/app".to_owned()],
            &LocalAcceptedProof::empty(),
            &local_mcp_serves(),
        )
        .await;

        assert!(funnel.accepted_edit_captured);
        assert!(funnel.auditable_accepted_edit_ready);
        assert_eq!(funnel.accepted_edit_upload_failed, 1);
        assert_eq!(funnel.stage, "accepted_edit_upload_failed");
        assert!(
            funnel
                .blockers
                .contains(&"accepted_edit_upload_failed".to_owned())
        );
        assert_eq!(funnel.last_upload_error.as_deref(), Some("boom"));
    }

    #[tokio::test]
    async fn accepted_edit_funnel_does_not_inherit_other_repo_launch_grade_receipts() {
        let pool = setup().await;
        record_confirmed(&pool, receipt_for_repo("one", "other/repo", true))
            .await
            .expect("record other repo receipt");

        let funnel = accepted_edit_proof_funnel(
            &pool,
            &["acme/app".to_owned()],
            &LocalAcceptedProof::empty(),
            &local_mcp_serves(),
        )
        .await;

        assert_eq!(funnel.accepted_edit_rows_last30, 1);
        assert_eq!(funnel.accepted_edit_rows_for_current_repo, 0);
        assert!(!funnel.accepted_edit_captured);
        assert_eq!(funnel.proof_grade, "none");
        assert!(!funnel.observed_value_ready);
        assert!(!funnel.auditable_accepted_edit_ready);
        assert!(!funnel.launch_grade_paid_value_ready);
        assert!(!funnel.ready_for_cloud_value);
    }

    #[tokio::test]
    async fn accepted_edit_funnel_treats_processing_rows_as_pending_not_launch_grade() {
        let pool = setup().await;
        sqlx::query(
            r"INSERT INTO cloud_outbox
              (kind, payload_json, status, retry_count, created_at)
              VALUES ('accepted_edit', ?1, 'processing', 0, ?2)",
        )
        .bind(accepted_edit_payload(vec![
            "550e8400-e29b-41d4-a716-446655440000".to_owned(),
        ]))
        .bind(chrono::Utc::now().timestamp_millis())
        .execute(&pool)
        .await
        .expect("insert processing accepted edit outbox row");

        let funnel = accepted_edit_proof_funnel(
            &pool,
            &["acme/app".to_owned()],
            &LocalAcceptedProof::empty(),
            &local_mcp_serves(),
        )
        .await;

        assert!(funnel.accepted_edit_captured);
        assert!(funnel.observed_value_ready);
        assert!(!funnel.auditable_accepted_edit_ready);
        assert!(!funnel.launch_grade_paid_value_ready);
        assert!(!funnel.ready_for_cloud_value);
        assert_eq!(funnel.accepted_edit_upload_pending, 1);
        assert_eq!(funnel.stage, "accepted_edit_waiting_for_cloud_sync");
    }

    #[tokio::test]
    async fn accepted_signature_count_does_not_use_rule_source_repo_as_target_repo() {
        let pool = setup().await;
        sqlx::query(
            r"INSERT INTO skills (id, name, source, directory, version, source_repo)
              VALUES ('rule-1', 'Rule one', 'local', '/tmp/rule', '1', 'acme/app')",
        )
        .execute(&pool)
        .await
        .expect("insert skill");
        sqlx::query(
            r"INSERT INTO fix_outcomes
              (id, rule_id, rule_name, file_path, repo_full_name, diff_signature, accepted, applied_ok, created_at)
              VALUES ('fix-other', 'rule-1', 'Rule one', 'src/app.ts', 'other/repo', 'sig-other', 1, 1, datetime('now'))",
        )
        .execute(&pool)
        .await
        .expect("insert other repo fix");

        assert_eq!(
            accepted_signature_count_for_repos(&pool, &["acme/app".to_owned()]).await,
            0
        );

        sqlx::query(
            r"INSERT INTO fix_outcomes
              (id, rule_id, rule_name, file_path, repo_full_name, diff_signature, accepted, applied_ok, created_at)
              VALUES ('fix-current', 'rule-1', 'Rule one', 'src/app.ts', 'acme/app', 'sig-current', 1, 1, datetime('now'))",
        )
        .execute(&pool)
        .await
        .expect("insert current repo fix");

        assert_eq!(
            accepted_signature_count_for_repos(&pool, &["acme/app".to_owned()]).await,
            1
        );
    }
}
