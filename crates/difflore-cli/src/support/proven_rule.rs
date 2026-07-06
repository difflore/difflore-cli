//! Shared building blocks for the "proven rule" drilldown that both
//! `status` (`queries::proven_rule`) and `doctor` (`memory_snapshot`) compute.
//!
//! The two surfaces differ in scope handling, output shape, and the exact
//! signed-proof SQL, but they share two pieces verbatim: the rule-metadata
//! lookup used to attach a name/source-repo to agent-hook outcomes, and the
//! candidate ranking order. Hoisting those here keeps the two from silently
//! disagreeing about which rule is "the" proven one.

use std::cmp::Ordering;
use std::collections::HashMap;

/// Rule name + source repo loaded by id, used to attach metadata to
/// agent-hook accepted outcomes (which carry only a `rule_id`).
#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct RuleMetadataRow {
    pub(crate) rule_id: String,
    pub(crate) name: String,
    pub(crate) source_repo: Option<String>,
}

/// Load active-rule metadata for `rule_ids`, optionally constrained to
/// `normalized_repos`. Returns a map keyed by rule id; ids that are empty
/// (after trimming) are dropped, and an empty input yields an empty map.
pub(crate) async fn fetch_rule_metadata_for_ids(
    db: &difflore_core::SqlitePool,
    rule_ids: &[String],
    normalized_repos: Option<&[String]>,
) -> HashMap<String, RuleMetadataRow> {
    let ids: Vec<&str> = rule_ids
        .iter()
        .map(String::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .collect();
    if ids.is_empty() {
        return HashMap::new();
    }

    let id_placeholders = std::iter::repeat_n("?", ids.len())
        .collect::<Vec<_>>()
        .join(", ");
    let repo_filter = normalized_repos
        .filter(|repos| !repos.is_empty())
        .map(|repos| {
            let placeholders = std::iter::repeat_n("?", repos.len())
                .collect::<Vec<_>>()
                .join(", ");
            format!("AND LOWER(COALESCE(source_repo, '')) IN ({placeholders})")
        })
        .unwrap_or_default();
    let sql = format!(
        "SELECT id AS rule_id,
                COALESCE(NULLIF(name, ''), id) AS name,
                source_repo AS source_repo
         FROM skills
         WHERE id IN ({id_placeholders})
           AND COALESCE(status, 'active') = 'active'
           {repo_filter}"
    );
    let mut query = sqlx::query_as::<_, RuleMetadataRow>(&sql);
    for id in ids {
        query = query.bind(id);
    }
    if let Some(repos) = normalized_repos {
        for repo in repos {
            query = query.bind(repo);
        }
    }

    query
        .fetch_all(db)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|row| (row.rule_id.clone(), row))
        .collect()
}

/// Aggregate hook accepted-outcome summaries that observation storage has
/// already filtered to the target repo. Rule metadata is used only to discard
/// stale/unknown local ids; it must not decide target-repo scope.
pub(crate) async fn accepted_link_summary_from_target_repo_summaries(
    db: &difflore_core::SqlitePool,
    hook_summaries: &[difflore_core::cloud::observations::AcceptedFixOutcomeRuleSummary],
) -> difflore_core::cloud::observations::AcceptedRecallLinkSummary {
    let rule_ids: Vec<String> = hook_summaries
        .iter()
        .map(|summary| summary.rule_id.clone())
        .collect();
    let metadata = fetch_rule_metadata_for_ids(db, &rule_ids, None).await;
    let mut out = difflore_core::cloud::observations::AcceptedRecallLinkSummary::default();
    for summary in hook_summaries {
        if !metadata.contains_key(&summary.rule_id) {
            continue;
        }
        out.accepted_outcomes += summary.accepted_outcomes;
        out.linked_to_prior_recall += summary.linked_to_prior_recall;
        out.linked_to_rule_recall += summary.linked_to_rule_recall;
        out.linked_to_mcp_rule_serve += summary.linked_to_mcp_rule_serve;
        out.linked_to_edit_attribution += summary.linked_to_edit_attribution;
    }
    out
}

pub(crate) async fn accepted_link_summary_from_default_observation_store(
    db: &difflore_core::SqlitePool,
    normalized_repos: &[String],
    window_days: i64,
    recall_lookback_days: i64,
) -> difflore_core::cloud::observations::AcceptedRecallLinkSummary {
    if normalized_repos.is_empty() {
        return difflore_core::cloud::observations::AcceptedRecallLinkSummary::default();
    }
    let hook_summaries =
        match difflore_core::cloud::observations::ObservationEmitter::open_default().await {
            Ok(emitter) => emitter
                .accepted_fix_outcome_rule_summaries_for_repos(
                    window_days,
                    recall_lookback_days,
                    normalized_repos,
                )
                .await
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        };
    accepted_link_summary_from_target_repo_summaries(db, &hook_summaries).await
}

/// Ranking key for a proven-rule candidate. Both surfaces order candidates by
/// total accepted (signed + hook) desc, then outcomes linked to prior recall
/// desc, then signed fix proofs desc, then name asc as a stable tiebreaker.
pub(crate) struct ProvenRuleRank<'a> {
    pub(crate) total: i64,
    pub(crate) linked_to_prior_recall: i64,
    pub(crate) accepted_fix_proofs: i64,
    pub(crate) name: &'a str,
}

impl ProvenRuleRank<'_> {
    pub(crate) fn cmp(&self, other: &Self) -> Ordering {
        other
            .total
            .cmp(&self.total)
            .then(
                other
                    .linked_to_prior_recall
                    .cmp(&self.linked_to_prior_recall),
            )
            .then(other.accepted_fix_proofs.cmp(&self.accepted_fix_proofs))
            .then(self.name.cmp(other.name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn skills_pool() -> difflore_core::SqlitePool {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("open sqlite");
        sqlx::query(
            "CREATE TABLE skills (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                description TEXT NOT NULL DEFAULT '',
                source_repo TEXT,
                status TEXT
             )",
        )
        .execute(&pool)
        .await
        .expect("create skills");
        pool
    }

    async fn insert_skill(pool: &difflore_core::SqlitePool, id: &str, status: &str) {
        sqlx::query(
            "INSERT INTO skills (id, name, description, source_repo, status)
             VALUES (?1, ?1, '', 'owner/repo', ?2)",
        )
        .bind(id)
        .bind(status)
        .execute(pool)
        .await
        .expect("insert skill");
    }

    #[tokio::test]
    async fn accepted_link_summary_counts_target_repo_scoped_known_rules() {
        let pool = skills_pool().await;
        insert_skill(&pool, "r-known", "active").await;
        insert_skill(&pool, "r-disabled", "disabled").await;
        let summaries = vec![
            difflore_core::cloud::observations::AcceptedFixOutcomeRuleSummary {
                rule_id: "r-known".to_owned(),
                accepted_outcomes: 2,
                linked_to_prior_recall: 1,
                linked_to_rule_recall: 0,
                linked_to_mcp_rule_serve: 1,
                linked_to_edit_attribution: 0,
                sample_file: Some("src/lib.rs".to_owned()),
                latest_occurred_at_ms: 10,
            },
            difflore_core::cloud::observations::AcceptedFixOutcomeRuleSummary {
                rule_id: "r-disabled".to_owned(),
                accepted_outcomes: 9,
                ..Default::default()
            },
            difflore_core::cloud::observations::AcceptedFixOutcomeRuleSummary {
                rule_id: "r-unknown".to_owned(),
                accepted_outcomes: 8,
                ..Default::default()
            },
        ];

        let summary = accepted_link_summary_from_target_repo_summaries(&pool, &summaries).await;

        assert_eq!(summary.accepted_outcomes, 2);
        assert_eq!(summary.linked_to_prior_recall, 1);
        assert_eq!(summary.linked_to_mcp_rule_serve, 1);
    }
}
