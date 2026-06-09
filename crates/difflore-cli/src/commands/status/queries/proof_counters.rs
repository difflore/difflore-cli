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
    /// Whole-machine accepted outcomes that actually applied. The
    /// repo-scoped fields above stay stable for existing dashboards.
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
    let split = difflore_core::fix_outcomes::split_summary(db, LOCAL_PROOF_WINDOW_DAYS)
        .await
        .unwrap_or(difflore_core::fix_outcomes::FixOutcomeSplitSummary {
            accepted_and_applied: 0,
            accepted_but_failed: 0,
        });
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
    if normalized_repo_aliases(repo_aliases).is_empty() {
        return LocalMcpRuleServe::empty();
    }
    let summary = difflore_core::mcp_rule_serves::summary_for_repos(
        db,
        repo_aliases,
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
