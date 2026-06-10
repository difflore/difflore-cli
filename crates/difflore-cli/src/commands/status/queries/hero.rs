//! "Local hero" evidence: the current-repo rule with the strongest combined
//! signal (accepted edits + signed diff proofs + recall + MCP serves).
//! Strictly current-repo scoped. Feeds the `localHeroEvidence` envelope key.

use super::proof_counters::{
    LOCAL_PROOF_WINDOW_DAYS, REVIEW_MINUTES_PER_ACCEPTED_PROOF, normalized_repo_aliases,
};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::commands::status) struct LocalHeroEvidence {
    pub(in crate::commands::status) scope: String,
    pub(in crate::commands::status) rule_id: String,
    pub(in crate::commands::status) title: String,
    pub(in crate::commands::status) source_repo: Option<String>,
    pub(in crate::commands::status) target_repo_full_name: Option<String>,
    pub(in crate::commands::status) target_pr_number: Option<i64>,
    pub(in crate::commands::status) sample_file: Option<String>,
    pub(in crate::commands::status) accepted_edits: i64,
    pub(in crate::commands::status) signed_diff_proofs: i64,
    pub(in crate::commands::status) recall_events: i64,
    pub(in crate::commands::status) best_recall_rank: Option<i64>,
    pub(in crate::commands::status) latest_recall_file: Option<String>,
    pub(in crate::commands::status) agent_serves: i64,
    pub(in crate::commands::status) strict_agent_serves: i64,
    pub(in crate::commands::status) latest_agent_serve_file: Option<String>,
    pub(in crate::commands::status) saved_review_minutes: i64,
    pub(in crate::commands::status) latest_accepted_at: Option<String>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct HeroCandidateRow {
    rule_id: String,
    title: String,
    source_repo: Option<String>,
    accepted_edits: i64,
    signed_diff_proofs: i64,
    latest_accepted_at: Option<String>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct HeroAcceptedFixRow {
    target_repo_full_name: Option<String>,
    target_pr_number: Option<i64>,
    sample_file: Option<String>,
}

pub(in crate::commands::status) async fn local_hero_evidence(
    db: &difflore_core::SqlitePool,
    repo_aliases: &[String],
) -> Option<LocalHeroEvidence> {
    let normalized_aliases = normalized_repo_aliases(repo_aliases);
    if normalized_aliases.is_empty() {
        return None;
    }

    fetch_local_hero_evidence(db, Some(&normalized_aliases), "currentRepo").await
}

async fn fetch_local_hero_evidence(
    db: &difflore_core::SqlitePool,
    normalized_repos: Option<&[String]>,
    scope: &str,
) -> Option<LocalHeroEvidence> {
    for candidate in fetch_hero_candidate_rows(db, normalized_repos).await {
        let latest_fix = fetch_hero_accepted_fix(db, &candidate.rule_id).await;
        let recall_events = difflore_core::rule_outcomes::recall_count_for(
            db,
            &candidate.rule_id,
            LOCAL_PROOF_WINDOW_DAYS,
        )
        .await
        .unwrap_or(0);
        let top_recall = difflore_core::rule_outcomes::latest_top3_recall_for(
            db,
            &candidate.rule_id,
            LOCAL_PROOF_WINDOW_DAYS,
        )
        .await
        .ok()
        .flatten();
        let mcp_summary = difflore_core::mcp_rule_serves::summary_for_rule(
            db,
            &candidate.rule_id,
            LOCAL_PROOF_WINDOW_DAYS,
        )
        .await
        .unwrap_or_default();

        if candidate.accepted_edits <= 0 {
            continue;
        }

        return Some(LocalHeroEvidence {
            scope: scope.to_owned(),
            rule_id: candidate.rule_id,
            title: candidate.title,
            source_repo: candidate.source_repo,
            target_repo_full_name: latest_fix
                .as_ref()
                .and_then(|fix| fix.target_repo_full_name.clone()),
            target_pr_number: latest_fix.as_ref().and_then(|fix| fix.target_pr_number),
            sample_file: latest_fix.as_ref().and_then(|fix| fix.sample_file.clone()),
            accepted_edits: candidate.accepted_edits,
            signed_diff_proofs: candidate.signed_diff_proofs,
            recall_events,
            best_recall_rank: top_recall.as_ref().map(|recall| recall.rank),
            latest_recall_file: top_recall.and_then(|recall| recall.file_path),
            agent_serves: mcp_summary.calls,
            strict_agent_serves: mcp_summary.strict_match_calls,
            latest_agent_serve_file: mcp_summary.latest.and_then(|serve| serve.file_path),
            saved_review_minutes: candidate.accepted_edits * REVIEW_MINUTES_PER_ACCEPTED_PROOF,
            latest_accepted_at: candidate.latest_accepted_at,
        });
    }
    None
}

async fn fetch_hero_candidate_rows(
    db: &difflore_core::SqlitePool,
    normalized_repos: Option<&[String]>,
) -> Vec<HeroCandidateRow> {
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
                COALESCE(NULLIF(s.name, ''), f.rule_name) AS title,
                s.source_repo AS source_repo,
                COUNT(*) AS accepted_edits,
                COUNT(DISTINCT NULLIF(f.diff_signature, '')) AS signed_diff_proofs,
                MAX(f.created_at) AS latest_accepted_at
         FROM fix_outcomes f
         INNER JOIN skills s ON s.id = f.rule_id
         WHERE f.accepted = 1
           AND f.applied_ok = 1
           AND f.rule_id IS NOT NULL
           AND COALESCE(s.status, 'active') = 'active'
           AND datetime(f.created_at) >= datetime('now', ?)
           {repo_filter}
         GROUP BY f.rule_id, COALESCE(NULLIF(s.name, ''), f.rule_name), s.source_repo
         ORDER BY COUNT(DISTINCT NULLIF(f.diff_signature, '')) DESC,
                  COUNT(*) DESC,
                  datetime(MAX(f.created_at)) DESC,
                  title ASC
         LIMIT 20"
    );
    let mut query = sqlx::query_as::<_, HeroCandidateRow>(&sql)
        .bind(format!("-{LOCAL_PROOF_WINDOW_DAYS} days"));
    if let Some(repos) = normalized_repos {
        for repo in repos {
            query = query.bind(repo);
        }
    }
    query.fetch_all(db).await.unwrap_or_default()
}

async fn fetch_hero_accepted_fix(
    db: &difflore_core::SqlitePool,
    rule_id: &str,
) -> Option<HeroAcceptedFixRow> {
    sqlx::query_as::<_, HeroAcceptedFixRow>(
        "SELECT NULLIF(repo_full_name, '') AS target_repo_full_name,
                pr_number AS target_pr_number,
                NULLIF(file_path, '') AS sample_file
         FROM fix_outcomes
         WHERE rule_id = ?1
           AND accepted = 1
           AND applied_ok = 1
           AND datetime(created_at) >= datetime('now', ?2)
         ORDER BY datetime(created_at) DESC, id DESC
         LIMIT 1",
    )
    .bind(rule_id)
    .bind(format!("-{LOCAL_PROOF_WINDOW_DAYS} days"))
    .fetch_optional(db)
    .await
    .ok()
    .flatten()
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{insert_skill_only, value_loop_pool};
    use super::*;

    #[tokio::test]
    async fn local_hero_evidence_is_current_repo_only() {
        let pool = value_loop_pool().await;
        insert_skill_only(
            &pool,
            "repo-rule",
            "Prefer structured API parsing",
            "acme/widgets",
        )
        .await;
        insert_skill_only(
            &pool,
            "global-rule",
            "Pin GitHub Actions refs to SHAs",
            "tanstack/router",
        )
        .await;

        sqlx::query(
            "INSERT INTO fix_outcomes
             (id, rule_id, rule_name, file_path, repo_full_name, pr_number,
              diff_signature, accepted, applied_ok, created_at)
             VALUES
             ('repo-fix-1', 'repo-rule', 'Prefer structured API parsing', 'src/parser.rs',
              'acme/widgets', 12, 'sha256:repo', 1, 1, datetime('now')),
             ('global-fix-1', 'global-rule', 'Pin GitHub Actions refs to SHAs',
              '.github/workflows/pr.yml', 'difflore-fixtures/router', 4, 'sha256:one',
              1, 1, datetime('now')),
             ('global-fix-2', 'global-rule', 'Pin GitHub Actions refs to SHAs',
              '.github/workflows/release.yml', 'difflore-fixtures/router', 4, 'sha256:two',
              1, 1, datetime('now'))",
        )
        .execute(&pool)
        .await
        .expect("insert accepted fixes");

        difflore_core::rule_outcomes::record_recalled_with_context(
            &pool,
            &[difflore_core::rule_outcomes::RuleRecallInput {
                rule_id: "repo-rule",
                session_id: Some("session-repo"),
                repo_full_name: Some("acme/widgets"),
                file_path: Some("src/parser.rs"),
                query_text: "structured parser",
                rank: 1,
                top_k: 3,
                strict_file_match: true,
            }],
        )
        .await
        .expect("record repo recall");
        difflore_core::mcp_rule_serves::record(
            &pool,
            &difflore_core::mcp_rule_serves::McpRuleServeInput {
                tool: "search_rules",
                session_id: Some("session-repo"),
                repo_full_name: Some("acme/widgets"),
                file_path: Some("src/parser.rs"),
                query_text: "structured parser",
                rule_ids: &["repo-rule".to_owned()],
                top_k: 3,
                strict_match_count: 1,
                estimated_tokens: 100,
            },
        )
        .await
        .expect("record repo mcp serve");

        let scoped = local_hero_evidence(&pool, &["acme/widgets".to_owned()])
            .await
            .expect("current repo hero");
        assert_eq!(scoped.scope, "currentRepo");
        assert_eq!(scoped.rule_id, "repo-rule");
        assert_eq!(scoped.accepted_edits, 1);
        assert_eq!(scoped.recall_events, 1);
        assert_eq!(scoped.strict_agent_serves, 1);

        let scoped_none = local_hero_evidence(&pool, &["missing/repo".to_owned()]).await;
        assert!(
            scoped_none.is_none(),
            "known repo scopes must not fall back to unrelated local hero evidence"
        );

        let no_scope = local_hero_evidence(&pool, &[]).await;
        assert!(
            no_scope.is_none(),
            "status must not show a best-on-machine hero when no repo scope is known"
        );
    }
}
