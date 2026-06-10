//! "Value loop" evidence: proof that one rule was learned from a PR review,
//! recalled and served (causally, before the edit), and then produced an
//! accepted edit in a *different* PR. Strict causal/file-match gating lives
//! here; the source-proof half is delegated to `super::source_proof`.

use super::super::transform::{
    ValueLoopAcceptedCandidate, normalize_repo, pr_number_for_value_loop,
    source_repo_for_value_loop, value_loop_files_match, value_loop_times_causal,
};
use super::proof_counters::REVIEW_MINUTES_PER_ACCEPTED_PROOF;
use super::source_proof::fetch_rule_source_proof;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::commands::status) struct ValueLoopEvidence {
    pub(in crate::commands::status) imported_review: ImportedReviewEvidence,
    pub(in crate::commands::status) accepted_rule: AcceptedRuleEvidence,
    pub(in crate::commands::status) recall: RecallEvidence,
    pub(in crate::commands::status) mcp_serve: McpServeEvidence,
    pub(in crate::commands::status) accepted_edit_proof: AcceptedEditProofEvidence,
    pub(in crate::commands::status) saved_review_minutes: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::commands::status) struct ImportedReviewEvidence {
    pub(in crate::commands::status) source_repo: String,
    pub(in crate::commands::status) pr_number: i64,
    pub(in crate::commands::status) comment_url: Option<String>,
    pub(in crate::commands::status) file: Option<String>,
    pub(in crate::commands::status) excerpt: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::commands::status) struct AcceptedRuleEvidence {
    pub(in crate::commands::status) rule_id: String,
    pub(in crate::commands::status) title: String,
    pub(in crate::commands::status) source_proof: difflore_core::skills::CandidateSourceProof,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::commands::status) struct RecallEvidence {
    pub(in crate::commands::status) rule_id: String,
    pub(in crate::commands::status) rank: i64,
    pub(in crate::commands::status) top_k: i64,
    pub(in crate::commands::status) command: String,
    pub(in crate::commands::status) repo_full_name: Option<String>,
    pub(in crate::commands::status) file: Option<String>,
    pub(in crate::commands::status) strict_file_match: bool,
    pub(in crate::commands::status) recalled_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::commands::status) struct McpServeEvidence {
    pub(in crate::commands::status) tool: String,
    pub(in crate::commands::status) rule_ids: Vec<String>,
    pub(in crate::commands::status) repo_full_name: Option<String>,
    pub(in crate::commands::status) file: Option<String>,
    pub(in crate::commands::status) strict_scoped: bool,
    pub(in crate::commands::status) estimated_tokens: i64,
    pub(in crate::commands::status) served_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::commands::status) struct AcceptedEditProofEvidence {
    pub(in crate::commands::status) rule_id: String,
    pub(in crate::commands::status) file: Option<String>,
    pub(in crate::commands::status) diff_signature: Option<String>,
    pub(in crate::commands::status) accepted_at: String,
    pub(in crate::commands::status) source: String,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub(in crate::commands::status) struct ValueLoopAcceptedRow {
    pub(in crate::commands::status) rule_id: String,
    pub(in crate::commands::status) name: String,
    pub(in crate::commands::status) source_repo: Option<String>,
    pub(in crate::commands::status) target_repo_full_name: Option<String>,
    pub(in crate::commands::status) target_pr_number: Option<i64>,
    pub(in crate::commands::status) accepted_file_path: Option<String>,
    pub(in crate::commands::status) accepted_at: String,
    pub(in crate::commands::status) diff_signature: Option<String>,
}

pub(in crate::commands::status) async fn local_value_loop_evidence(
    db: &difflore_core::SqlitePool,
    repo_aliases: &[String],
) -> Option<ValueLoopEvidence> {
    let normalized_aliases = super::proof_counters::normalized_repo_aliases(repo_aliases);
    if normalized_aliases.is_empty() {
        return None;
    }

    fetch_value_loop_evidence(db, Some(&normalized_aliases)).await
}

pub(in crate::commands::status) async fn fetch_value_loop_evidence(
    db: &difflore_core::SqlitePool,
    normalized_repos: Option<&[String]>,
) -> Option<ValueLoopEvidence> {
    // Hook-only outcomes currently lack target PR identity, so they cannot
    // satisfy cloud-linked value-loop evidence. Skip them here instead of
    // doing DB work that build_value_loop_evidence_for_candidate must discard.
    for candidate in fetch_value_loop_accepted_rows(db, normalized_repos)
        .await
        .into_iter()
        .map(ValueLoopAcceptedCandidate::from)
    {
        if let Some(evidence) = build_value_loop_evidence_for_candidate(db, candidate).await {
            return Some(evidence);
        }
    }
    None
}

pub(in crate::commands::status) async fn build_value_loop_evidence_for_candidate(
    db: &difflore_core::SqlitePool,
    candidate: ValueLoopAcceptedCandidate,
) -> Option<ValueLoopEvidence> {
    if candidate.source == "agent_hook_outcome" && candidate.target_pr_number.is_none() {
        return None;
    }
    let source_proof = fetch_rule_source_proof(
        db,
        &candidate.rule_id,
        candidate.accepted_file_path.as_deref(),
    )
    .await?;
    let source_repo = source_repo_for_value_loop(&source_proof, candidate.source_repo.as_deref())?;
    let pr_number = pr_number_for_value_loop(&source_proof)?;
    if value_loop_source_matches_target_pr(
        &source_repo,
        pr_number,
        candidate.target_repo_full_name.as_deref(),
        candidate.target_pr_number,
    ) {
        return None;
    }
    let mcp_summary = difflore_core::observability::mcp_rule_serves::summary_for_rule(db, &candidate.rule_id, 30)
        .await
        .ok()?;
    let mcp_latest = mcp_summary.latest?;
    let recall = causal_top3_recall_for(
        db,
        &candidate.rule_id,
        &mcp_latest.served_at,
        &candidate.accepted_at,
    )
    .await?;
    if !recall.strict_file_match || !mcp_latest.strict_scoped {
        return None;
    }
    if !value_loop_files_match(
        candidate.accepted_file_path.as_deref(),
        recall.file_path.as_deref(),
        mcp_latest.file_path.as_deref(),
    ) {
        return None;
    }
    if !value_loop_times_causal(
        &recall.recalled_at,
        &mcp_latest.served_at,
        &candidate.accepted_at,
    ) {
        return None;
    }

    let command = if let Some(file) = recall.file_path.as_deref() {
        format!(
            "difflore recall --file {file} --top-k {}",
            recall.top_k.max(3)
        )
    } else {
        format!("difflore recall --top-k {}", recall.top_k.max(3))
    };

    Some(ValueLoopEvidence {
        imported_review: ImportedReviewEvidence {
            source_repo,
            pr_number,
            comment_url: source_proof.comment_url.clone(),
            file: source_proof.file.clone(),
            excerpt: source_proof.excerpt.clone(),
        },
        accepted_rule: AcceptedRuleEvidence {
            rule_id: candidate.rule_id.clone(),
            title: candidate.name,
            source_proof,
        },
        recall: RecallEvidence {
            rule_id: candidate.rule_id.clone(),
            rank: recall.rank,
            top_k: recall.top_k,
            command,
            repo_full_name: recall.repo_full_name,
            file: recall.file_path,
            strict_file_match: recall.strict_file_match,
            recalled_at: recall.recalled_at,
        },
        mcp_serve: McpServeEvidence {
            tool: mcp_latest.tool,
            rule_ids: vec![candidate.rule_id.clone()],
            repo_full_name: mcp_latest.repo_full_name,
            file: mcp_latest.file_path,
            strict_scoped: mcp_latest.strict_scoped,
            estimated_tokens: mcp_latest.estimated_tokens,
            served_at: mcp_latest.served_at,
        },
        accepted_edit_proof: AcceptedEditProofEvidence {
            rule_id: candidate.rule_id,
            file: candidate.accepted_file_path,
            diff_signature: candidate.diff_signature,
            accepted_at: candidate.accepted_at,
            source: candidate.source,
        },
        saved_review_minutes: REVIEW_MINUTES_PER_ACCEPTED_PROOF,
    })
}

fn value_loop_source_matches_target_pr(
    source_repo: &str,
    source_pr_number: i64,
    target_repo: Option<&str>,
    target_pr_number: Option<i64>,
) -> bool {
    let Some(target_pr_number) = target_pr_number else {
        return false;
    };
    if source_pr_number != target_pr_number {
        return false;
    }
    let Some(target_repo) = target_repo else {
        return false;
    };
    normalize_repo(source_repo) == normalize_repo(target_repo)
}

async fn causal_top3_recall_for(
    db: &difflore_core::SqlitePool,
    rule_id: &str,
    served_at: &str,
    accepted_at: &str,
) -> Option<difflore_core::observability::rule_outcomes::TopRecallEvidence> {
    // Push the causal predicate into SQL so `LIMIT` cannot hide the one
    // valid pre-serve recall behind later non-causal rows.
    let recall = sqlx::query_as::<_, difflore_core::observability::rule_outcomes::TopRecallEvidence>(
        r"SELECT rule_id,
                  repo_full_name,
                  file_path,
                  COALESCE(rank, 999) AS rank,
                  COALESCE(top_k, 0) AS top_k,
                  strict_file_match != 0 AS strict_file_match,
                  created_at AS recalled_at
           FROM rule_outcomes
           WHERE kind = 'recalled'
             AND rule_id = ?1
             AND rank BETWEEN 1 AND 3
             AND datetime(created_at) >= datetime('now', '-30 days')
             AND datetime(created_at) <= datetime(?2)
           ORDER BY datetime(created_at) DESC, id DESC
           LIMIT 1",
    )
    .bind(rule_id)
    .bind(served_at)
    .fetch_optional(db)
    .await
    .ok()??;
    // Defence in depth: SQL already filters `recall <= serve`; this
    // catches any `served_at > accepted_at` data-quality drift in the
    // upstream invariant.
    if value_loop_times_causal(&recall.recalled_at, served_at, accepted_at) {
        Some(recall)
    } else {
        None
    }
}

async fn fetch_value_loop_accepted_rows(
    db: &difflore_core::SqlitePool,
    normalized_repos: Option<&[String]>,
) -> Vec<ValueLoopAcceptedRow> {
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
                NULLIF(f.repo_full_name, '') AS target_repo_full_name,
                f.pr_number AS target_pr_number,
                NULLIF(f.file_path, '') AS accepted_file_path,
                f.created_at AS accepted_at,
                NULLIF(f.diff_signature, '') AS diff_signature
         FROM fix_outcomes f
         INNER JOIN skills s ON s.id = f.rule_id
         WHERE f.accepted = 1
           AND f.applied_ok = 1
           AND f.rule_id IS NOT NULL
           AND COALESCE(s.status, 'active') = 'active'
           {repo_filter}
         ORDER BY datetime(f.created_at) DESC, f.id DESC
         LIMIT 50"
    );
    let mut query = sqlx::query_as::<_, ValueLoopAcceptedRow>(&sql);
    if let Some(repos) = normalized_repos {
        for repo in repos {
            query = query.bind(repo);
        }
    }
    query.fetch_all(db).await.unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{insert_skill_only, insert_source_proof, value_loop_pool};
    use super::*;

    #[tokio::test]
    async fn value_loop_evidence_requires_same_rule_through_recall_mcp_and_acceptance() {
        let pool = value_loop_pool().await;
        insert_skill_only(
            &pool,
            "rule-1",
            "Prefer structured API parsing",
            "acme/widgets",
        )
        .await;
        insert_source_proof(
            &pool,
            "rule-1",
            serde_json::json!({
                "sourceProof": {
                    "source": "acme/widgets#42",
                    "commentUrl": "https://github.com/acme/widgets/pull/42#discussion_r1",
                    "file": "src/parser.rs",
                    "excerpt": "Prefer structured parsing here."
                }
            }),
        )
        .await;
        difflore_core::observability::rule_outcomes::record_recalled_with_context(
            &pool,
            &[difflore_core::observability::rule_outcomes::RuleRecallInput {
                rule_id: "rule-1",
                session_id: Some("session-1"),
                repo_full_name: Some("acme/widgets"),
                file_path: Some("src/parser.rs"),
                query_text: "src/parser.rs structured parser",
                rank: 1,
                top_k: 3,
                strict_file_match: true,
            }],
        )
        .await
        .expect("record recall");
        let recall_at = recent_offset(&pool, "-3 minutes").await;
        sqlx::query("UPDATE rule_outcomes SET created_at = ?1")
            .bind(&recall_at)
            .execute(&pool)
            .await
            .expect("pin recall time");
        difflore_core::observability::mcp_rule_serves::record(
            &pool,
            &difflore_core::observability::mcp_rule_serves::McpRuleServeInput {
                tool: "search_rules",
                session_id: Some("session-1"),
                repo_full_name: Some("acme/widgets"),
                file_path: Some("src/parser.rs"),
                query_text: "src/parser.rs structured parser",
                rule_ids: &["rule-1".to_owned()],
                top_k: 3,
                strict_match_count: 1,
                estimated_tokens: 120,
            },
        )
        .await
        .expect("record mcp serve");
        let serve_at = recent_offset(&pool, "-2 minutes").await;
        sqlx::query("UPDATE mcp_rule_serves SET served_at = ?1")
            .bind(&serve_at)
            .execute(&pool)
            .await
            .expect("pin mcp serve time");
        let accepted_at = recent_offset(&pool, "-1 minutes").await;
        sqlx::query(
            "INSERT INTO fix_outcomes
             (id, rule_id, rule_name, file_path, repo_full_name, pr_number, diff_signature,
              accepted, applied_ok, created_at)
             VALUES ('fix-1', 'rule-1', 'Prefer structured API parsing', 'src/parser.rs',
                     'acme/widgets', 43, 'sha256:abc', 1, 1, ?1)",
        )
        .bind(&accepted_at)
        .execute(&pool)
        .await
        .expect("insert accepted fix");

        let evidence = fetch_value_loop_evidence(&pool, Some(&["acme/widgets".to_owned()]))
            .await
            .expect("value loop evidence");

        assert_eq!(evidence.imported_review.pr_number, 42);
        assert_eq!(evidence.accepted_rule.rule_id, "rule-1");
        assert_eq!(evidence.recall.rank, 1);
        assert_eq!(evidence.mcp_serve.rule_ids, vec!["rule-1".to_owned()]);
        assert_eq!(
            evidence.accepted_edit_proof.diff_signature.as_deref(),
            Some("sha256:abc")
        );
        assert_eq!(evidence.saved_review_minutes, 4);

        sqlx::query("UPDATE fix_outcomes SET pr_number = 42 WHERE id = 'fix-1'")
            .execute(&pool)
            .await
            .expect("make accepted fix self-sourced");
        let self_sourced =
            fetch_value_loop_evidence(&pool, Some(&["acme/widgets".to_owned()])).await;
        assert!(
            self_sourced.is_none(),
            "value-loop evidence must not count a rule sourced from the same PR it fixed"
        );

        let unrelated = local_value_loop_evidence(&pool, &["other/repo".to_owned()]).await;
        assert!(
            unrelated.is_none(),
            "repo-scoped status must not fall back to stale evidence from another repo"
        );
    }

    #[tokio::test]
    async fn value_loop_evidence_falls_back_to_review_comment_source_proof() {
        let pool = value_loop_pool().await;
        insert_skill_only(
            &pool,
            "comment-rule",
            "No blank lines in blockquotes",
            "acme/widgets",
        )
        .await;
        sqlx::query(
            "UPDATE skills
             SET description = '# No Blank Lines Inside Consecutive Blockquotes\n\nDo not insert blank lines between consecutive blockquote callouts.'
             WHERE id = 'comment-rule'",
        )
        .execute(&pool)
        .await
        .expect("update skill description");
        sqlx::query(
            "INSERT INTO review_items
             (id, file_path, status, source, source_kind, repo_full_name, pr_number)
             VALUES ('review-item-1', 'docs/readme.md', 'accepted', 'github',
                     'github_pr_review', 'acme/widgets', 77)",
        )
        .execute(&pool)
        .await
        .expect("insert review item");
        sqlx::query(
            "INSERT INTO review_comments
             (id, review_item_id, external_comment_id, line_number, content, comment_url)
             VALUES ('review-comment-1', 'review-item-1', 'discussion_r1', 12,
                     'Fix blank lines between consecutive blockquotes; markdownlint MD028 flags this.',
                     'https://github.com/acme/widgets/pull/77#discussion_r1')",
        )
        .execute(&pool)
        .await
        .expect("insert review comment");
        difflore_core::observability::rule_outcomes::record_recalled_with_context(
            &pool,
            &[difflore_core::observability::rule_outcomes::RuleRecallInput {
                rule_id: "comment-rule",
                session_id: Some("session-comment"),
                repo_full_name: Some("acme/widgets"),
                file_path: Some("docs/readme.md"),
                query_text: "docs/readme.md blank lines blockquotes",
                rank: 1,
                top_k: 3,
                strict_file_match: true,
            }],
        )
        .await
        .expect("record recall");
        let recall_at = recent_offset(&pool, "-3 minutes").await;
        sqlx::query("UPDATE rule_outcomes SET created_at = ?1")
            .bind(&recall_at)
            .execute(&pool)
            .await
            .expect("pin recall time");
        difflore_core::observability::mcp_rule_serves::record(
            &pool,
            &difflore_core::observability::mcp_rule_serves::McpRuleServeInput {
                tool: "search_rules",
                session_id: Some("session-comment"),
                repo_full_name: Some("acme/widgets"),
                file_path: Some("docs/readme.md"),
                query_text: "docs/readme.md blank lines blockquotes",
                rule_ids: &["comment-rule".to_owned()],
                top_k: 3,
                strict_match_count: 1,
                estimated_tokens: 100,
            },
        )
        .await
        .expect("record mcp serve");
        let serve_at = recent_offset(&pool, "-2 minutes").await;
        sqlx::query("UPDATE mcp_rule_serves SET served_at = ?1")
            .bind(&serve_at)
            .execute(&pool)
            .await
            .expect("pin mcp serve time");
        let accepted_at = recent_offset(&pool, "-1 minutes").await;
        sqlx::query(
            "INSERT INTO fix_outcomes
             (id, rule_id, rule_name, file_path, diff_signature, accepted, applied_ok, created_at)
             VALUES ('fix-comment', 'comment-rule', 'No blank lines in blockquotes',
                     'docs/readme.md', 'sha256:def', 1, 1, ?1)",
        )
        .bind(&accepted_at)
        .execute(&pool)
        .await
        .expect("insert accepted fix");

        let evidence = fetch_value_loop_evidence(&pool, Some(&["acme/widgets".to_owned()]))
            .await
            .expect("value loop evidence");

        assert_eq!(evidence.imported_review.source_repo, "acme/widgets");
        assert_eq!(evidence.imported_review.pr_number, 77);
        assert_eq!(
            evidence.imported_review.comment_url.as_deref(),
            Some("https://github.com/acme/widgets/pull/77#discussion_r1")
        );
    }

    #[tokio::test]
    async fn value_loop_evidence_rejects_agent_hook_without_target_pr() {
        let pool = value_loop_pool().await;
        insert_skill_only(&pool, "agent-rule", "Use stable waits", "acme/widgets").await;
        insert_source_proof(
            &pool,
            "agent-rule",
            serde_json::json!({
                "sourceProof": {
                    "source": "acme/widgets#77",
                    "commentUrl": "https://github.com/acme/widgets/pull/77#discussion_r1",
                    "file": "src/wait.ts",
                    "excerpt": "Use stable waits instead of scheduler races."
                }
            }),
        )
        .await;
        difflore_core::observability::rule_outcomes::record_recalled_with_context(
            &pool,
            &[difflore_core::observability::rule_outcomes::RuleRecallInput {
                rule_id: "agent-rule",
                session_id: Some("session-2"),
                repo_full_name: Some("acme/widgets"),
                file_path: Some("src/wait.ts"),
                query_text: "src/wait.ts stable wait",
                rank: 2,
                top_k: 3,
                strict_file_match: true,
            }],
        )
        .await
        .expect("record recall");
        let recall_at = recent_offset(&pool, "-3 minutes").await;
        sqlx::query("UPDATE rule_outcomes SET created_at = ?1")
            .bind(&recall_at)
            .execute(&pool)
            .await
            .expect("pin recall time");
        difflore_core::observability::mcp_rule_serves::record(
            &pool,
            &difflore_core::observability::mcp_rule_serves::McpRuleServeInput {
                tool: "search_rules",
                session_id: Some("session-2"),
                repo_full_name: Some("acme/widgets"),
                file_path: Some("src/wait.ts"),
                query_text: "src/wait.ts stable wait",
                rule_ids: &["agent-rule".to_owned()],
                top_k: 3,
                strict_match_count: 1,
                estimated_tokens: 96,
            },
        )
        .await
        .expect("record mcp serve");
        let serve_at = recent_offset(&pool, "-2 minutes").await;
        sqlx::query("UPDATE mcp_rule_serves SET served_at = ?1")
            .bind(&serve_at)
            .execute(&pool)
            .await
            .expect("pin mcp serve time");
        let accepted_at = recent_offset(&pool, "-1 minutes").await;

        let evidence = build_value_loop_evidence_for_candidate(
            &pool,
            ValueLoopAcceptedCandidate {
                rule_id: "agent-rule".to_owned(),
                name: "Use stable waits".to_owned(),
                source_repo: Some("acme/widgets".to_owned()),
                target_repo_full_name: None,
                target_pr_number: None,
                accepted_file_path: Some("src/wait.ts".to_owned()),
                accepted_at,
                diff_signature: None,
                source: "agent_hook_outcome".to_owned(),
            },
        )
        .await;

        assert!(
            evidence.is_none(),
            "agent hook outcomes do not carry target PR identity, so they must not upgrade to auditable value-loop evidence"
        );
    }

    #[tokio::test]
    async fn value_loop_evidence_requires_strict_recall_and_mcp_file_proof() {
        let pool = value_loop_pool().await;
        insert_skill_only(&pool, "loose-rule", "Avoid loose proof", "acme/widgets").await;
        insert_source_proof(
            &pool,
            "loose-rule",
            serde_json::json!({
                "sourceProof": {
                    "source": "acme/widgets#78",
                    "commentUrl": "https://github.com/acme/widgets/pull/78#discussion_r1",
                    "file": "src/proof.ts",
                    "excerpt": "Only count value evidence when file proof is strict."
                }
            }),
        )
        .await;
        difflore_core::observability::rule_outcomes::record_recalled_with_context(
            &pool,
            &[difflore_core::observability::rule_outcomes::RuleRecallInput {
                rule_id: "loose-rule",
                session_id: Some("session-loose"),
                repo_full_name: Some("acme/widgets"),
                file_path: Some("src/other.ts"),
                query_text: "src/other.ts loose proof",
                rank: 1,
                top_k: 3,
                strict_file_match: false,
            }],
        )
        .await
        .expect("record loose recall");
        let recall_at = recent_offset(&pool, "-3 minutes").await;
        sqlx::query("UPDATE rule_outcomes SET created_at = ?1")
            .bind(&recall_at)
            .execute(&pool)
            .await
            .expect("pin recall time");
        difflore_core::observability::mcp_rule_serves::record(
            &pool,
            &difflore_core::observability::mcp_rule_serves::McpRuleServeInput {
                tool: "search_rules",
                session_id: Some("session-loose"),
                repo_full_name: Some("acme/widgets"),
                file_path: Some("src/other.ts"),
                query_text: "src/other.ts loose proof",
                rule_ids: &["loose-rule".to_owned()],
                top_k: 3,
                strict_match_count: 0,
                estimated_tokens: 96,
            },
        )
        .await
        .expect("record loose mcp serve");
        let serve_at = recent_offset(&pool, "-2 minutes").await;
        sqlx::query("UPDATE mcp_rule_serves SET served_at = ?1")
            .bind(&serve_at)
            .execute(&pool)
            .await
            .expect("pin mcp serve time");
        let accepted_at = recent_offset(&pool, "-1 minutes").await;

        let evidence = build_value_loop_evidence_for_candidate(
            &pool,
            ValueLoopAcceptedCandidate {
                rule_id: "loose-rule".to_owned(),
                name: "Avoid loose proof".to_owned(),
                source_repo: Some("acme/widgets".to_owned()),
                target_repo_full_name: Some("acme/widgets".to_owned()),
                target_pr_number: Some(79),
                accepted_file_path: Some("src/other.ts".to_owned()),
                accepted_at,
                diff_signature: None,
                source: "local_fix_outcome".to_owned(),
            },
        )
        .await;

        assert!(
            evidence.is_none(),
            "buyer-grade value evidence must require strict recall and MCP file proof"
        );
    }

    #[tokio::test]
    async fn value_loop_evidence_rejects_non_causal_event_order() {
        let pool = value_loop_pool().await;
        insert_skill_only(&pool, "late-serve-rule", "Use causal proof", "acme/widgets").await;
        insert_source_proof(
            &pool,
            "late-serve-rule",
            serde_json::json!({
                "sourceProof": {
                    "source": "acme/widgets#88",
                    "commentUrl": "https://github.com/acme/widgets/pull/88#discussion_r1",
                    "file": "src/causal.ts",
                    "excerpt": "Only count proof when the rule was served before the accepted edit."
                }
            }),
        )
        .await;
        difflore_core::observability::rule_outcomes::record_recalled_with_context(
            &pool,
            &[difflore_core::observability::rule_outcomes::RuleRecallInput {
                rule_id: "late-serve-rule",
                session_id: Some("session-3"),
                repo_full_name: Some("acme/widgets"),
                file_path: Some("src/causal.ts"),
                query_text: "src/causal.ts causal proof",
                rank: 1,
                top_k: 3,
                strict_file_match: true,
            }],
        )
        .await
        .expect("record recall");
        let recall_at = recent_offset(&pool, "-3 minutes").await;
        sqlx::query("UPDATE rule_outcomes SET created_at = ?1")
            .bind(&recall_at)
            .execute(&pool)
            .await
            .expect("pin recall time");
        difflore_core::observability::mcp_rule_serves::record(
            &pool,
            &difflore_core::observability::mcp_rule_serves::McpRuleServeInput {
                tool: "hook_post_edit",
                session_id: Some("session-3"),
                repo_full_name: Some("acme/widgets"),
                file_path: Some("src/causal.ts"),
                query_text: "src/causal.ts causal proof",
                rule_ids: &["late-serve-rule".to_owned()],
                top_k: 3,
                strict_match_count: 1,
                estimated_tokens: 80,
            },
        )
        .await
        .expect("record mcp serve");
        // Serve happens AFTER the accepted edit -> non-causal. Pin it one
        // minute past the accepted edit, relative to "now" so the recall
        // stays inside the 30-day window regardless of wall-clock date.
        let serve_at = recent_offset(&pool, "+1 minutes").await;
        sqlx::query("UPDATE mcp_rule_serves SET served_at = ?1")
            .bind(&serve_at)
            .execute(&pool)
            .await
            .expect("pin mcp serve time");
        let accepted_at = recent_offset(&pool, "-1 minutes").await;

        let evidence = build_value_loop_evidence_for_candidate(
            &pool,
            ValueLoopAcceptedCandidate {
                rule_id: "late-serve-rule".to_owned(),
                name: "Use causal proof".to_owned(),
                source_repo: Some("acme/widgets".to_owned()),
                target_repo_full_name: Some("acme/widgets".to_owned()),
                target_pr_number: Some(89),
                accepted_file_path: Some("src/causal.ts".to_owned()),
                accepted_at,
                diff_signature: None,
                source: "local_fix_outcome".to_owned(),
            },
        )
        .await;

        assert!(
            evidence.is_none(),
            "buyer-grade evidence must not count a serve that happened after the accepted edit"
        );
    }

    #[tokio::test]
    async fn causal_top3_recall_for_returns_valid_recall_past_old_limit_50_truncation() {
        // Valid pre-serve recalls must remain reachable even when many later
        // non-causal recalls sort ahead of them.
        let pool = value_loop_pool().await;
        // Anchor every timestamp to "now" so the valid pre-serve recall is
        // always inside the production query's rolling `-30 days` window,
        // independent of the wall-clock date the suite runs on.
        let pre_serve_recall_at = recent_offset(&pool, "-5 minutes").await;
        let served_at = recent_offset(&pool, "-4 minutes").await;
        let accepted_at = recent_offset(&pool, "-3 minutes").await;
        let rule_id = "long-tail-rule";

        // Seed enough later non-causal recalls to prove SQL applies the
        // causal filter before the limit.
        for i in 0..60u32 {
            let ts = recent_offset(&pool, &format!("+{} seconds", i + 1)).await;
            sqlx::query(
                "INSERT INTO rule_outcomes
                 (rule_id, kind, session_id, repo_full_name, file_path,
                  query_hash, rank, top_k, strict_file_match, created_at)
                 VALUES (?1, 'recalled', 'session-tail', 'acme/widgets',
                         'src/tail.ts', 'q', 1, 3, 1, ?2)",
            )
            .bind(rule_id)
            .bind(&ts)
            .execute(&pool)
            .await
            .expect("insert post-serve recall");
        }

        // Insert the one valid pre-serve recall directly.
        sqlx::query(
            "INSERT INTO rule_outcomes
             (rule_id, kind, session_id, repo_full_name, file_path,
              query_hash, rank, top_k, strict_file_match, created_at)
             VALUES (?1, 'recalled', 'session-pre-serve', 'acme/widgets',
                     'src/tail.ts', 'q', 2, 3, 1, ?2)",
        )
        .bind(rule_id)
        .bind(&pre_serve_recall_at)
        .execute(&pool)
        .await
        .expect("insert pre-serve recall");

        let result = causal_top3_recall_for(&pool, rule_id, &served_at, &accepted_at).await;
        let recall = result.expect(
            "pre-serve recall must be reachable even with 60 later post-serve rows ahead of it",
        );
        assert_eq!(recall.rank, 2);
        assert_eq!(recall.recalled_at, pre_serve_recall_at);
    }

    #[tokio::test]
    async fn causal_top3_recall_for_accepts_rfc3339_recall_timestamps() {
        let pool = value_loop_pool().await;
        let rule_id = "rfc3339-recall-rule";
        let recalled_at: String =
            sqlx::query_scalar("SELECT strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '-2 minutes')")
                .fetch_one(&pool)
                .await
                .expect("format recalled_at");
        let served_at: String =
            sqlx::query_scalar("SELECT strftime('%Y-%m-%d %H:%M:%S', 'now', '-1 minutes')")
                .fetch_one(&pool)
                .await
                .expect("format served_at");
        let accepted_at: String =
            sqlx::query_scalar("SELECT strftime('%Y-%m-%dT%H:%M:%SZ', 'now')")
                .fetch_one(&pool)
                .await
                .expect("format accepted_at");

        sqlx::query(
            "INSERT INTO rule_outcomes
             (rule_id, kind, session_id, repo_full_name, file_path,
              query_hash, rank, top_k, strict_file_match, created_at)
             VALUES (?1, 'recalled', 'session-rfc3339', 'acme/widgets',
                     'src/time.ts', 'q', 1, 3, 1, ?2)",
        )
        .bind(rule_id)
        .bind(&recalled_at)
        .execute(&pool)
        .await
        .expect("insert rfc3339 recall");

        let result = causal_top3_recall_for(&pool, rule_id, &served_at, &accepted_at).await;

        let recall = result.expect("SQLite datetime parsing must handle UTC RFC3339 recall rows");
        assert_eq!(recall.rank, 1);
        assert_eq!(recall.recalled_at, recalled_at);
    }

    /// Format a `YYYY-MM-DD HH:MM:SS` timestamp at a SQLite modifier offset
    /// from the DB's own `now`, so fixtures stay inside the production
    /// queries' rolling `-30 days` window no matter what date the suite runs.
    async fn recent_offset(pool: &difflore_core::SqlitePool, modifier: &str) -> String {
        sqlx::query_scalar("SELECT strftime('%Y-%m-%d %H:%M:%S', 'now', ?1)")
            .bind(modifier)
            .fetch_one(pool)
            .await
            .expect("format offset timestamp")
    }
}
