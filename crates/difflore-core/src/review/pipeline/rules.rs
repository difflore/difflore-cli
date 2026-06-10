use crate::context::types::PastVerdict;
use crate::review_trajectory::RecalledVerdict;

/// Convert recalled `PastVerdict`s into the `RecalledVerdict` trajectory shape.
/// `excerpt` is truncated to ~200 characters (with a trailing `…`) to keep the
/// trajectory JSON compact.
pub(super) fn build_recalled_verdicts(past_verdicts: &[PastVerdict]) -> Vec<RecalledVerdict> {
    const EXCERPT_MAX: usize = 200;
    past_verdicts
        .iter()
        .map(|pv| {
            // First non-empty line of the issue text is more readable than the
            // id; fall back to the id when there is none.
            let title = pv
                .issue_text
                .lines()
                .find(|line| !line.trim().is_empty())
                .unwrap_or(&pv.extraction_id)
                .trim()
                .to_owned();
            let snippet = pv.code_snippet.as_str();
            let excerpt = if snippet.chars().count() > EXCERPT_MAX {
                let truncated: String = snippet.chars().take(EXCERPT_MAX).collect();
                format!("{truncated}…")
            } else {
                snippet.to_owned()
            };
            RecalledVerdict {
                id: pv.extraction_id.clone(),
                title,
                similarity: pv.similarity,
                excerpt,
            }
        })
        .collect()
}

pub(super) async fn recall_past_verdicts_for_review(
    settings: &crate::models::AppSettingsRecord,
    diff_content: &str,
    _project_id: Option<&str>,
    repo_full_names: &[String],
) -> Vec<PastVerdict> {
    if !settings.review_engine.past_verdict_recall {
        return Vec::new();
    }
    if diff_content.is_empty() {
        return Vec::new();
    }

    let cloud = crate::cloud::client::CloudClient::create().await;
    if !cloud.is_logged_in() {
        return Vec::new();
    }

    let repo_full_names = {
        let mut primary = Vec::new();
        for repo in repo_full_names {
            let repo = repo.trim();
            if repo.is_empty() {
                continue;
            }
            primary.push(repo.to_owned());
            break;
        }
        primary
    };

    if repo_full_names.is_empty() {
        return Vec::new();
    }

    crate::context::retrieval::retrieve_past_verdicts_by_text(
        &cloud,
        diff_content,
        repo_full_names.first().map(String::as_str),
        crate::context::types::PastVerdictScope::Personal,
        5,
        None,
    )
    .await
}
