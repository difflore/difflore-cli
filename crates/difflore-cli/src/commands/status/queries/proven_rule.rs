//! "Proven rule" drilldown: the single rule (repo-scoped, else best-on-machine)
//! with the most accepted-edit proofs, merging signed local fix outcomes with
//! agent-hook accepted outcomes. Feeds the `provenRuleDrilldown` envelope key.

use std::collections::HashMap;

use super::super::transform::ProvenRuleCandidate;
use super::proof_counters::{
    LOCAL_ACCEPTED_RECALL_LOOKBACK_DAYS, LOCAL_PROOF_WINDOW_DAYS, normalized_repo_aliases,
};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::commands::status) struct ProvenRuleDrilldown {
    pub(in crate::commands::status) rule_id: String,
    pub(in crate::commands::status) name: String,
    pub(in crate::commands::status) source_repo: Option<String>,
    pub(in crate::commands::status) accepted_fixes: i64,
    pub(in crate::commands::status) accepted_fix_proofs: i64,
    pub(in crate::commands::status) accepted_hook_outcomes: i64,
    pub(in crate::commands::status) accepted_hook_outcomes_linked_to_prior_recall: i64,
    pub(in crate::commands::status) accepted_hook_outcomes_linked_to_recall_or_edit_proof: i64,
    pub(in crate::commands::status) accepted_hook_outcomes_linked_to_rule_recall: i64,
    pub(in crate::commands::status) accepted_hook_outcomes_linked_to_mcp_rule_serve: i64,
    pub(in crate::commands::status) accepted_hook_outcomes_linked_to_edit_attribution: i64,
    pub(in crate::commands::status) sample_file: Option<String>,
    pub(in crate::commands::status) explain_command: String,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub(super) struct ProvenRuleDrilldownRow {
    pub(super) rule_id: String,
    pub(super) name: String,
    pub(super) source_repo: Option<String>,
    pub(super) accepted_fix_proofs: i64,
    pub(super) sample_file: Option<String>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub(super) struct RuleMetadataRow {
    pub(super) rule_id: String,
    pub(super) name: String,
    pub(super) source_repo: Option<String>,
}

pub(in crate::commands::status) async fn local_proven_rule_drilldown(
    db: &difflore_core::SqlitePool,
    repo_aliases: &[String],
) -> Option<ProvenRuleDrilldown> {
    let normalized_aliases = normalized_repo_aliases(repo_aliases);
    if normalized_aliases.is_empty() {
        return None;
    }

    let hook_summaries =
        match difflore_core::cloud::observations::ObservationEmitter::open_default().await {
            Ok(emitter) => emitter
                .accepted_fix_outcome_rule_summaries(
                    LOCAL_PROOF_WINDOW_DAYS,
                    LOCAL_ACCEPTED_RECALL_LOOKBACK_DAYS,
                )
                .await
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        };

    if let Some(rule) =
        fetch_proven_rule_drilldown(db, Some(&normalized_aliases), &hook_summaries).await
    {
        return Some(rule);
    }

    None
}

pub(in crate::commands::status) async fn fetch_proven_rule_drilldown(
    db: &difflore_core::SqlitePool,
    normalized_repos: Option<&[String]>,
    hook_summaries: &[difflore_core::cloud::observations::AcceptedFixOutcomeRuleSummary],
) -> Option<ProvenRuleDrilldown> {
    let mut candidates: std::collections::BTreeMap<String, ProvenRuleCandidate> =
        std::collections::BTreeMap::new();

    for row in fetch_signed_proven_rule_rows(db, normalized_repos).await {
        candidates.insert(
            row.rule_id.clone(),
            ProvenRuleCandidate {
                rule_id: row.rule_id,
                name: row.name,
                source_repo: row.source_repo,
                accepted_fix_proofs: row.accepted_fix_proofs,
                accepted_hook_outcomes: 0,
                accepted_hook_outcomes_linked_to_prior_recall: 0,
                accepted_hook_outcomes_linked_to_recall_or_edit_proof: 0,
                accepted_hook_outcomes_linked_to_rule_recall: 0,
                accepted_hook_outcomes_linked_to_mcp_rule_serve: 0,
                accepted_hook_outcomes_linked_to_edit_attribution: 0,
                sample_file: row.sample_file,
            },
        );
    }

    let hook_ids: Vec<String> = hook_summaries
        .iter()
        .map(|summary| summary.rule_id.clone())
        .collect();
    let metadata = fetch_rule_metadata_for_ids(db, &hook_ids, normalized_repos).await;
    for summary in hook_summaries {
        let Some(rule) = metadata.get(&summary.rule_id) else {
            continue;
        };
        let candidate = candidates
            .entry(summary.rule_id.clone())
            .or_insert_with(|| ProvenRuleCandidate {
                rule_id: summary.rule_id.clone(),
                name: rule.name.clone(),
                source_repo: rule.source_repo.clone(),
                accepted_fix_proofs: 0,
                accepted_hook_outcomes: 0,
                accepted_hook_outcomes_linked_to_prior_recall: 0,
                accepted_hook_outcomes_linked_to_recall_or_edit_proof: 0,
                accepted_hook_outcomes_linked_to_rule_recall: 0,
                accepted_hook_outcomes_linked_to_mcp_rule_serve: 0,
                accepted_hook_outcomes_linked_to_edit_attribution: 0,
                sample_file: None,
            });
        candidate.accepted_hook_outcomes += summary.accepted_outcomes;
        candidate.accepted_hook_outcomes_linked_to_prior_recall += summary.linked_to_prior_recall;
        candidate.accepted_hook_outcomes_linked_to_recall_or_edit_proof +=
            summary.linked_to_prior_recall;
        candidate.accepted_hook_outcomes_linked_to_rule_recall += summary.linked_to_rule_recall;
        candidate.accepted_hook_outcomes_linked_to_mcp_rule_serve +=
            summary.linked_to_mcp_rule_serve;
        candidate.accepted_hook_outcomes_linked_to_edit_attribution +=
            summary.linked_to_edit_attribution;
        if candidate
            .sample_file
            .as_deref()
            .unwrap_or("")
            .trim()
            .is_empty()
            && let Some(file) = summary
                .sample_file
                .as_deref()
                .map(str::trim)
                .filter(|file| !file.is_empty())
        {
            candidate.sample_file = Some(file.to_owned());
        }
    }

    let mut candidates: Vec<_> = candidates
        .into_values()
        .filter(|candidate| candidate.accepted_fix_proofs + candidate.accepted_hook_outcomes > 0)
        .collect();
    candidates.sort_by(|a, b| {
        let a_total = a.accepted_fix_proofs + a.accepted_hook_outcomes;
        let b_total = b.accepted_fix_proofs + b.accepted_hook_outcomes;
        b_total
            .cmp(&a_total)
            .then(
                b.accepted_hook_outcomes_linked_to_prior_recall
                    .cmp(&a.accepted_hook_outcomes_linked_to_prior_recall),
            )
            .then(b.accepted_fix_proofs.cmp(&a.accepted_fix_proofs))
            .then(a.name.cmp(&b.name))
    });

    candidates
        .into_iter()
        .next()
        .map(|candidate| ProvenRuleDrilldown {
            explain_command: "difflore status --json".to_owned(),
            accepted_fixes: candidate.accepted_fix_proofs + candidate.accepted_hook_outcomes,
            accepted_fix_proofs: candidate.accepted_fix_proofs,
            accepted_hook_outcomes: candidate.accepted_hook_outcomes,
            accepted_hook_outcomes_linked_to_prior_recall: candidate
                .accepted_hook_outcomes_linked_to_prior_recall,
            accepted_hook_outcomes_linked_to_recall_or_edit_proof: candidate
                .accepted_hook_outcomes_linked_to_recall_or_edit_proof,
            accepted_hook_outcomes_linked_to_rule_recall: candidate
                .accepted_hook_outcomes_linked_to_rule_recall,
            accepted_hook_outcomes_linked_to_mcp_rule_serve: candidate
                .accepted_hook_outcomes_linked_to_mcp_rule_serve,
            accepted_hook_outcomes_linked_to_edit_attribution: candidate
                .accepted_hook_outcomes_linked_to_edit_attribution,
            rule_id: candidate.rule_id,
            name: candidate.name,
            source_repo: candidate.source_repo,
            sample_file: candidate.sample_file,
        })
}

async fn fetch_signed_proven_rule_rows(
    db: &difflore_core::SqlitePool,
    normalized_repos: Option<&[String]>,
) -> Vec<ProvenRuleDrilldownRow> {
    let repo_filter = normalized_repos
        .filter(|repos| !repos.is_empty())
        .map(|repos| {
            let placeholders = std::iter::repeat_n("?", repos.len())
                .collect::<Vec<_>>()
                .join(", ");
            format!("AND LOWER(COALESCE(s.source_repo, '')) IN ({placeholders})")
        })
        .unwrap_or_default();
    let sql = format!(
        "SELECT f.rule_id AS rule_id,
                COALESCE(NULLIF(s.name, ''), f.rule_name) AS name,
                s.source_repo AS source_repo,
                COUNT(*) AS accepted_fix_proofs,
                MAX(NULLIF(f.file_path, '')) AS sample_file
         FROM fix_outcomes f
         INNER JOIN skills s ON s.id = f.rule_id
         WHERE f.accepted = 1
           AND f.applied_ok = 1
           AND f.rule_id IS NOT NULL
           AND COALESCE(s.status, 'active') = 'active'
           {repo_filter}
         GROUP BY f.rule_id, COALESCE(NULLIF(s.name, ''), f.rule_name), s.source_repo
         ORDER BY COUNT(*) DESC, MAX(f.created_at) DESC
         LIMIT 20"
    );
    let mut query = sqlx::query_as::<_, ProvenRuleDrilldownRow>(&sql);
    if let Some(repos) = normalized_repos {
        for repo in repos {
            query = query.bind(repo);
        }
    }

    query.fetch_all(db).await.unwrap_or_default()
}

async fn fetch_rule_metadata_for_ids(
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

#[cfg(test)]
mod tests {
    use super::super::test_support::{
        ProvenRuleSeed, insert_proven_rule, insert_skill_only, proven_rule_pool,
    };
    use super::*;

    #[tokio::test]
    async fn local_proven_rule_drilldown_prefers_current_repo_alias() {
        let pool = proven_rule_pool().await;
        insert_proven_rule(
            &pool,
            ProvenRuleSeed {
                id: "router",
                name: "Pin Actions to commit SHAs",
                repo: "tanstack/router",
                file: ".github/workflows/ci.yml",
                accepted: 3,
            },
        )
        .await;
        insert_proven_rule(
            &pool,
            ProvenRuleSeed {
                id: "gin",
                name: "Return 413 for large request bodies",
                repo: "gin-gonic/gin",
                file: "binding/binding.go",
                accepted: 1,
            },
        )
        .await;

        let scoped = fetch_proven_rule_drilldown(
            &pool,
            Some(&["hibrandonevans/gin".to_owned(), "gin-gonic/gin".to_owned()]),
            &[],
        )
        .await
        .expect("scoped proven rule");
        assert_eq!(scoped.rule_id, "gin");
        assert_eq!(scoped.source_repo.as_deref(), Some("gin-gonic/gin"));
        assert_eq!(scoped.accepted_fixes, 1);
        assert_eq!(scoped.accepted_fix_proofs, 1);
        assert_eq!(scoped.accepted_hook_outcomes, 0);
        assert_eq!(scoped.explain_command, "difflore status --json");

        let scoped_none =
            fetch_proven_rule_drilldown(&pool, Some(&["unknown/repo".to_owned()]), &[])
                .await
                .is_none();
        assert!(scoped_none, "scoped lookup should not leak other repos");
    }

    #[tokio::test]
    async fn local_proven_rule_drilldown_does_not_global_fallback_when_repo_is_known() {
        let pool = proven_rule_pool().await;
        insert_proven_rule(
            &pool,
            ProvenRuleSeed {
                id: "router",
                name: "Pin Actions to commit SHAs",
                repo: "tanstack/router",
                file: ".github/workflows/ci.yml",
                accepted: 3,
            },
        )
        .await;

        let scoped = local_proven_rule_drilldown(&pool, &["hibrandonevans/store".to_owned()]).await;
        assert!(
            scoped.is_none(),
            "status must not advertise a global proven rule that current-repo recall/MCP cannot serve"
        );

        let no_scope = local_proven_rule_drilldown(&pool, &[]).await;
        assert!(
            no_scope.is_none(),
            "status must not advertise unrelated proven rules when no repo scope is known"
        );
    }

    #[tokio::test]
    async fn proven_rule_drilldown_includes_agent_hook_accepted_outcomes() {
        let pool = proven_rule_pool().await;
        insert_skill_only(
            &pool,
            "agent-rule",
            "Prefer structured API parsing",
            "acme/widgets",
        )
        .await;
        let hook_summaries = vec![
            difflore_core::cloud::observations::AcceptedFixOutcomeRuleSummary {
                rule_id: "agent-rule".to_owned(),
                accepted_outcomes: 2,
                linked_to_prior_recall: 1,
                linked_to_rule_recall: 0,
                linked_to_mcp_rule_serve: 1,
                linked_to_edit_attribution: 0,
                sample_file: Some("src/parser.rs".to_owned()),
                latest_occurred_at_ms: 123,
            },
        ];

        let drilldown =
            fetch_proven_rule_drilldown(&pool, Some(&["acme/widgets".to_owned()]), &hook_summaries)
                .await
                .expect("hook proof drilldown");

        assert_eq!(drilldown.rule_id, "agent-rule");
        assert_eq!(drilldown.accepted_fixes, 2);
        assert_eq!(drilldown.accepted_fix_proofs, 0);
        assert_eq!(drilldown.accepted_hook_outcomes, 2);
        assert_eq!(drilldown.accepted_hook_outcomes_linked_to_prior_recall, 1);
        assert_eq!(drilldown.accepted_hook_outcomes_linked_to_mcp_rule_serve, 1);
        assert_eq!(drilldown.sample_file.as_deref(), Some("src/parser.rs"));
    }

    #[tokio::test]
    async fn proven_rule_drilldown_global_fallback_uses_best_available_proof() {
        let pool = proven_rule_pool().await;
        insert_proven_rule(
            &pool,
            ProvenRuleSeed {
                id: "router",
                name: "Pin Actions to commit SHAs",
                repo: "tanstack/router",
                file: ".github/workflows/ci.yml",
                accepted: 3,
            },
        )
        .await;

        let fallback = fetch_proven_rule_drilldown(&pool, None, &[])
            .await
            .expect("global proven rule");
        assert_eq!(fallback.rule_id, "router");
        assert_eq!(fallback.accepted_fixes, 3);
    }
}
