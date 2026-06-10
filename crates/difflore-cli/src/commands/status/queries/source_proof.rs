//! Source-proof resolution for a rule: where the rule originally came from (a
//! PR review comment / candidate-promotion event), used to prove a rule was
//! learned from one PR and later served into a different one. Scoring and
//! normalisation helpers sit alongside the SQL so the fuzzy review-comment
//! fallback stays in one place.

use super::super::transform::{normalize_repo, pr_number_for_value_loop};

#[derive(Debug, Clone, sqlx::FromRow)]
struct SkillSourceProofRow {
    name: String,
    description: String,
    source_repo: Option<String>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct ReviewSourceProofRow {
    repo_full_name: Option<String>,
    pr_number: Option<i64>,
    file_path: String,
    comment_url: Option<String>,
    content: String,
}

pub(super) async fn fetch_rule_source_proof(
    db: &difflore_core::SqlitePool,
    rule_id: &str,
    accepted_file_path: Option<&str>,
) -> Option<difflore_core::skills::CandidateSourceProof> {
    let rows = sqlx::query_scalar!(
        r#"SELECT metadata AS "metadata!: String"
         FROM rule_events
         WHERE skill_id = ?1
           AND kind = 'source_proof'
           AND metadata IS NOT NULL
         ORDER BY datetime(created_at) DESC, id DESC
         LIMIT 5"#,
        rule_id,
    )
    .fetch_all(db)
    .await
    .ok()?;

    if let Some(proof) = rows.into_iter().find_map(|raw| {
        let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
        let proof_value = value.get("sourceProof").cloned().unwrap_or(value);
        serde_json::from_value::<difflore_core::skills::CandidateSourceProof>(proof_value)
            .ok()
            .filter(difflore_core::skills::CandidateSourceProof::has_any)
    }) {
        return Some(proof);
    }

    fetch_rule_source_proof_from_skill_and_reviews(db, rule_id, accepted_file_path).await
}

async fn fetch_rule_source_proof_from_skill_and_reviews(
    db: &difflore_core::SqlitePool,
    rule_id: &str,
    accepted_file_path: Option<&str>,
) -> Option<difflore_core::skills::CandidateSourceProof> {
    let row = sqlx::query_as::<_, SkillSourceProofRow>(
        "SELECT name,
                description,
                source_repo
         FROM skills
         WHERE id = ?1
         LIMIT 1",
    )
    .bind(rule_id)
    .fetch_optional(db)
    .await
    .ok()??;

    if let Some(mut proof) = difflore_core::skills::parse_candidate_source_proof(&row.description) {
        if proof.source.is_none() {
            proof.source = source_with_pr_from_comment_or_repo(
                proof.comment_url.as_deref(),
                row.source_repo.as_deref(),
                None,
            );
        }
        if pr_number_for_value_loop(&proof).is_some() {
            return Some(proof);
        }
    }

    let accepted_file = normalized_source_proof_file(Some(accepted_file_path?))?;
    let rows = fetch_review_source_candidates(db, row.source_repo.as_deref()).await;
    let tokens = source_proof_search_tokens(&row.name, &row.description);
    let best = rows
        .into_iter()
        .filter_map(|review| {
            let file_scope_score =
                source_proof_file_scope_score(&accepted_file, &review.file_path).max(
                    source_proof_content_file_scope_score(&accepted_file, &review.content),
                );
            if file_scope_score == 0 {
                return None;
            }
            let score = source_proof_match_score(&tokens, &review.content);
            let min_score = match file_scope_score {
                3 => 3,
                2 => 5,
                1 => 6,
                _ => return None,
            };
            (score >= min_score).then_some((file_scope_score, score, review))
        })
        .max_by(
            |(left_scope, left_score, _), (right_scope, right_score, _)| {
                left_scope
                    .cmp(right_scope)
                    .then(left_score.cmp(right_score))
            },
        )?
        .2;

    let source = source_with_pr_from_comment_or_repo(
        best.comment_url.as_deref(),
        best.repo_full_name
            .as_deref()
            .or(row.source_repo.as_deref()),
        best.pr_number,
    );
    let proof_file = if source_proof_content_file_scope_score(&accepted_file, &best.content) > 0 {
        Some(accepted_file.clone())
    } else {
        looks_like_source_path(&best.file_path).then(|| best.file_path.clone())
    };
    let proof = difflore_core::skills::CandidateSourceProof {
        source,
        comment_url: best.comment_url,
        file: proof_file,
        excerpt: Some(review_excerpt(&best.content)),
    };
    (pr_number_for_value_loop(&proof).is_some()).then_some(proof)
}

async fn fetch_review_source_candidates(
    db: &difflore_core::SqlitePool,
    source_repo: Option<&str>,
) -> Vec<ReviewSourceProofRow> {
    let Some(source_repo) = exact_review_source_repo(source_repo) else {
        return Vec::new();
    };

    sqlx::query_as::<_, ReviewSourceProofRow>(
        "SELECT ri.repo_full_name,
                ri.pr_number,
                ri.file_path,
                rc.comment_url,
                rc.content
         FROM review_comments rc
         INNER JOIN review_items ri ON ri.id = rc.review_item_id
         WHERE LOWER(COALESCE(ri.repo_full_name, '')) = LOWER(?1)
         ORDER BY datetime(rc.created_at) DESC
         LIMIT 500",
    )
    .bind(source_repo)
    .fetch_all(db)
    .await
    .unwrap_or_default()
}

fn exact_review_source_repo(source_repo: Option<&str>) -> Option<String> {
    let repo = source_repo
        .map(normalize_repo)
        .filter(|repo| !repo.is_empty())?;
    let mut parts = repo.split('/');
    let owner = parts.next()?;
    let name = parts.next()?;
    if owner.is_empty() || name.is_empty() || parts.next().is_some() {
        return None;
    }
    Some(repo)
}

fn source_proof_search_tokens(name: &str, description: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let text = format!(
        "{name} {}",
        description.lines().take(8).collect::<Vec<_>>().join(" ")
    );
    for raw in text
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .map(str::to_ascii_lowercase)
    {
        if raw.len() < 4 || SOURCE_PROOF_STOPWORDS.contains(&raw.as_str()) {
            continue;
        }
        if !tokens.contains(&raw) {
            tokens.push(raw);
        }
        if tokens.len() >= 12 {
            break;
        }
    }
    tokens
}

const SOURCE_PROOF_STOPWORDS: &[&str] = &[
    "rule", "rules", "with", "from", "when", "then", "this", "that", "into", "must", "never",
    "avoid", "prefer", "inside", "outside", "using", "used", "code", "file", "files", "good",
    "bad", "example", "examples", "applies",
];

fn source_proof_match_score(tokens: &[String], content: &str) -> usize {
    let haystack = content.to_ascii_lowercase();
    tokens
        .iter()
        .filter(|token| haystack.contains(token.as_str()))
        .count()
}

fn normalized_source_proof_file(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(|text| text.replace('\\', "/").to_ascii_lowercase())
}

fn source_proof_file_scope_score(accepted_file: &str, review_file: &str) -> u8 {
    let Some(review_file) = normalized_source_proof_file(Some(review_file)) else {
        return 0;
    };
    if review_file == accepted_file {
        return 3;
    }
    if accepted_file.starts_with(".github/workflows/")
        && review_file.starts_with(".github/workflows/")
    {
        return 2;
    }
    if !looks_like_source_path(&review_file) {
        return 1;
    }
    0
}

fn source_proof_content_file_scope_score(accepted_file: &str, content: &str) -> u8 {
    let haystack = content.replace('\\', "/").to_ascii_lowercase();
    if haystack.contains(accepted_file) {
        return 2;
    }
    0
}

fn looks_like_source_path(value: &str) -> bool {
    value.contains('/')
        || value.contains('\\')
        || value.rsplit_once('.').is_some_and(|(_, ext)| {
            (1..=8).contains(&ext.len()) && ext.chars().all(|ch| ch.is_ascii_alphanumeric())
        })
}

fn source_with_pr_from_comment_or_repo(
    comment_url: Option<&str>,
    repo: Option<&str>,
    pr_number: Option<i64>,
) -> Option<String> {
    if let Some(url) = comment_url
        && let (Some(repo), Some(pr)) = (
            github_repo_from_url_local(url),
            github_pr_from_url_local(url),
        )
    {
        return Some(format!("{repo}#{pr}"));
    }
    let repo = repo.map(str::trim).filter(|repo| !repo.is_empty())?;
    let pr = pr_number?;
    Some(format!("{repo}#{pr}"))
}

fn github_repo_from_url_local(url: &str) -> Option<&str> {
    let after_host = url.split_once("github.com/")?.1;
    let mut parts = after_host.split('/');
    let owner = parts.next()?.trim();
    let repo = parts.next()?.trim();
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    after_host.get(..owner.len() + 1 + repo.len())
}

fn github_pr_from_url_local(url: &str) -> Option<i64> {
    url.split_once("/pull/")
        .and_then(|(_, rest)| {
            rest.chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .parse()
                .ok()
        })
        .filter(|number| *number > 0)
}

fn review_excerpt(content: &str) -> String {
    const LIMIT: usize = 240;
    let compact = content.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.len() <= LIMIT {
        compact
    } else {
        format!("{}...", compact.chars().take(LIMIT).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{insert_skill_only, value_loop_pool};
    use super::*;

    #[tokio::test]
    async fn review_comment_source_proof_fallback_must_match_accepted_file() {
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
             VALUES ('review-item-1', 'docs/other.md', 'accepted', 'github',
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

        let mismatched = fetch_rule_source_proof_from_skill_and_reviews(
            &pool,
            "comment-rule",
            Some("docs/readme.md"),
        )
        .await;
        assert!(
            mismatched.is_none(),
            "fuzzy source proof fallback must fail closed when the source comment file differs from the accepted edit file"
        );

        let matched = fetch_rule_source_proof_from_skill_and_reviews(
            &pool,
            "comment-rule",
            Some("docs/other.md"),
        )
        .await;
        assert!(matched.is_some());
    }

    #[tokio::test]
    async fn review_comment_source_proof_requires_exact_source_repo() {
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
             VALUES ('review-item-other-owner', 'docs/readme.md', 'accepted', 'github',
                     'github_pr_review', 'other/widgets', 77)",
        )
        .execute(&pool)
        .await
        .expect("insert review item");
        sqlx::query(
            "INSERT INTO review_comments
             (id, review_item_id, external_comment_id, line_number, content, comment_url)
             VALUES ('review-comment-other-owner', 'review-item-other-owner', 'discussion_r1', 12,
                     'Fix blank lines between consecutive blockquotes; markdownlint MD028 flags this.',
                     'https://github.com/other/widgets/pull/77#discussion_r1')",
        )
        .execute(&pool)
        .await
        .expect("insert review comment");

        let proof = fetch_rule_source_proof_from_skill_and_reviews(
            &pool,
            "comment-rule",
            Some("docs/readme.md"),
        )
        .await;
        assert!(
            proof.is_none(),
            "source-proof lookup must not fall back to same repo-name history from another owner"
        );
    }

    #[tokio::test]
    async fn review_comment_source_proof_requires_canonical_source_repo() {
        let pool = value_loop_pool().await;
        insert_skill_only(
            &pool,
            "unscoped-comment-rule",
            "No blank lines in blockquotes",
            "",
        )
        .await;
        sqlx::query(
            "UPDATE skills
             SET description = '# No Blank Lines Inside Consecutive Blockquotes\n\nDo not insert blank lines between consecutive blockquote callouts.'
             WHERE id = 'unscoped-comment-rule'",
        )
        .execute(&pool)
        .await
        .expect("update skill description");
        sqlx::query(
            "INSERT INTO review_items
             (id, file_path, status, source, source_kind, repo_full_name, pr_number)
             VALUES ('review-item-unscoped', 'docs/readme.md', 'accepted', 'github',
                     'github_pr_review', 'acme/widgets', 77)",
        )
        .execute(&pool)
        .await
        .expect("insert review item");
        sqlx::query(
            "INSERT INTO review_comments
             (id, review_item_id, external_comment_id, line_number, content, comment_url)
             VALUES ('review-comment-unscoped', 'review-item-unscoped', 'discussion_r1', 12,
                     'Fix blank lines between consecutive blockquotes; markdownlint MD028 flags this.',
                     'https://github.com/acme/widgets/pull/77#discussion_r1')",
        )
        .execute(&pool)
        .await
        .expect("insert review comment");

        let proof = fetch_rule_source_proof_from_skill_and_reviews(
            &pool,
            "unscoped-comment-rule",
            Some("docs/readme.md"),
        )
        .await;
        assert!(
            proof.is_none(),
            "source-proof lookup must fail closed without canonical source_repo"
        );
    }

    #[test]
    fn exact_review_source_repo_requires_owner_and_repo() {
        assert_eq!(
            exact_review_source_repo(Some(" Acme/Widgets.git ")).as_deref(),
            Some("acme/widgets")
        );
        assert_eq!(exact_review_source_repo(Some("widgets")), None);
        assert_eq!(exact_review_source_repo(Some("acme/widgets/fork")), None);
        assert_eq!(exact_review_source_repo(Some("")), None);
        assert_eq!(exact_review_source_repo(None), None);
    }

    #[tokio::test]
    async fn review_comment_source_proof_allows_workflow_sibling_when_tokens_match() {
        let pool = value_loop_pool().await;
        insert_skill_only(
            &pool,
            "workflow-rule",
            "Pin GitHub Actions to immutable commit SHAs",
            "acme/widgets",
        )
        .await;
        sqlx::query(
            "UPDATE skills
             SET description = 'Do not reference GitHub Actions using mutable refs like action@main. Pin uses entries to immutable commit SHAs.'
             WHERE id = 'workflow-rule'",
        )
        .execute(&pool)
        .await
        .expect("update workflow skill description");
        sqlx::query(
            "INSERT INTO review_items
             (id, file_path, status, source, source_kind, repo_full_name, pr_number)
             VALUES ('workflow-review-item', '.github/workflows/release.yml',
                     'accepted', 'github', 'github_pr_review', 'acme/widgets', 88)",
        )
        .execute(&pool)
        .await
        .expect("insert workflow review item");
        sqlx::query(
            "INSERT INTO review_comments
             (id, review_item_id, external_comment_id, line_number, content, comment_url)
             VALUES ('workflow-review-comment', 'workflow-review-item', 'discussion_r2', 42,
                     'Pin the GitHub Actions uses refs to immutable commit SHAs; mutable action@main refs can silently change release workflow behavior.',
                     'https://github.com/acme/widgets/pull/88#discussion_r2')",
        )
        .execute(&pool)
        .await
        .expect("insert workflow review comment");

        let proof = fetch_rule_source_proof_from_skill_and_reviews(
            &pool,
            "workflow-rule",
            Some(".github/workflows/pr.yml"),
        )
        .await
        .expect("workflow sibling source proof");

        assert_eq!(proof.file.as_deref(), Some(".github/workflows/release.yml"));
        assert_eq!(pr_number_for_value_loop(&proof), Some(88));
    }

    #[tokio::test]
    async fn review_comment_source_proof_allows_strong_pr_overview_match() {
        let pool = value_loop_pool().await;
        insert_skill_only(
            &pool,
            "workflow-rule",
            "Use double quotes for YAML string values in GitHub Actions workflows",
            "acme/widgets",
        )
        .await;
        sqlx::query(
            "UPDATE skills
             SET description = 'Standardize GitHub Actions workflow YAML string values to double quotes rather than single quotes.'
             WHERE id = 'workflow-rule'",
        )
        .execute(&pool)
        .await
        .expect("update workflow skill description");
        sqlx::query(
            "INSERT INTO review_items
             (id, file_path, status, source, source_kind, repo_full_name, pr_number)
             VALUES ('workflow-overview-item', 'ci: standardize workflow quote style',
                     'accepted', 'github', 'github_pr_review', 'acme/widgets', 89)",
        )
        .execute(&pool)
        .await
        .expect("insert workflow overview item");
        sqlx::query(
            "INSERT INTO review_comments
             (id, review_item_id, external_comment_id, line_number, content, comment_url)
             VALUES ('workflow-overview-comment', 'workflow-overview-item', 'review_r3', 1,
                     'This PR standardizes GitHub Actions workflow YAML string values by converting single quotes to double quotes throughout the workflow configuration.',
                     'https://github.com/acme/widgets/pull/89#pullrequestreview-3')",
        )
        .execute(&pool)
        .await
        .expect("insert workflow overview comment");

        let proof = fetch_rule_source_proof_from_skill_and_reviews(
            &pool,
            "workflow-rule",
            Some(".github/workflows/ci.yml"),
        )
        .await
        .expect("strong PR overview source proof");

        assert_eq!(proof.file, None);
        assert_eq!(pr_number_for_value_loop(&proof), Some(89));
    }

    #[tokio::test]
    async fn review_comment_source_proof_uses_file_references_in_comment_body() {
        let pool = value_loop_pool().await;
        insert_skill_only(
            &pool,
            "pin-rule",
            "Pin GitHub Actions to full commit SHAs for supply-chain security",
            "acme/widgets",
        )
        .await;
        sqlx::query(
            "UPDATE skills
             SET description = 'When referencing third-party GitHub Actions in workflows, pin to a full commit SHA because tags can move and weaken supply-chain security.'
             WHERE id = 'pin-rule'",
        )
        .execute(&pool)
        .await
        .expect("update pin skill description");
        sqlx::query(
            "INSERT INTO review_items
             (id, file_path, status, source, source_kind, repo_full_name, pr_number)
             VALUES ('workflow-body-item', 'package.json',
                     'accepted', 'github', 'github_pr_review', 'acme/widgets', 90)",
        )
        .execute(&pool)
        .await
        .expect("insert package review item");
        sqlx::query(
            "INSERT INTO review_comments
             (id, review_item_id, external_comment_id, line_number, content, comment_url)
             VALUES ('workflow-body-comment', 'workflow-body-item', 'review_r4', 1,
                     'In .github/workflows/pr.yml, pin GitHub Actions to full commit SHAs; tags can be moved and this is a supply-chain security risk.',
                     'https://github.com/acme/widgets/pull/90#pullrequestreview-4')",
        )
        .execute(&pool)
        .await
        .expect("insert workflow body comment");

        let proof = fetch_rule_source_proof_from_skill_and_reviews(
            &pool,
            "pin-rule",
            Some(".github/workflows/pr.yml"),
        )
        .await
        .expect("comment body file reference source proof");

        assert_eq!(proof.file.as_deref(), Some(".github/workflows/pr.yml"));
        assert_eq!(pr_number_for_value_loop(&proof), Some(90));
    }
}
