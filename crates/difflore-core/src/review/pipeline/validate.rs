use super::super::parse::{parse_summary_response, parse_verify_response};
use super::super::prompts::{
    SUMMARY_SYSTEM_PROMPT, VERIFY_SYSTEM_PROMPT, build_summary_user_prompt,
    build_verify_user_prompt,
};
use super::super::{ReviewIssueRecord, ReviewLlm};
use super::{collect_diff_files, count_blocking};
use crate::models::ReviewSummary;

/// Self-check: re-run a cheap LLM pass over merged issues to score confidence
/// and drop obvious false positives.
///
/// Graceful degradation is the #1 invariant here: on any failure
/// (disabled, empty, LLM error, parser error) the caller's candidate
/// issues come back untouched. We NEVER drop issues because the verify
/// pass broke.
pub(in super::super) async fn verify_pass(
    llm: &dyn ReviewLlm,
    self_check_enabled: bool,
    diff: &str,
    issues: Vec<ReviewIssueRecord>,
) -> Vec<ReviewIssueRecord> {
    if !self_check_enabled {
        return issues;
    }
    if issues.is_empty() {
        return issues;
    }
    let original_issues = issues.clone();

    let user_prompt = build_verify_user_prompt(diff, &issues);
    let response = match llm.chat(VERIFY_SYSTEM_PROMPT, &user_prompt).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[verify_pass] cheap-model call failed: {e:?}");
            return issues;
        }
    };

    let map = match parse_verify_response(&response) {
        Some(m) if !m.is_empty() => m,
        _ => {
            eprintln!("[verify_pass] could not parse verify response; keeping issues unchanged");
            return issues;
        }
    };

    let mut out: Vec<ReviewIssueRecord> = Vec::with_capacity(issues.len());
    for (idx, mut issue) in issues.into_iter().enumerate() {
        match map.get(&idx) {
            Some((confidence, keep)) => {
                if !*keep {
                    continue;
                }
                issue.confidence = *confidence;
                out.push(issue);
            }
            None => {
                // Model didn't score this one — keep it at default
                // confidence rather than silently dropping it.
                out.push(issue);
            }
        }
    }

    if out.is_empty() && !original_issues.is_empty() {
        eprintln!(
            "[verify_pass] verifier dropped every candidate issue; keeping original issues to avoid a false-negative review"
        );
        return original_issues;
    }

    out
}

/// Review summary: emit a one-line PR description plus per-file walkthrough and
/// blocking / non-blocking counts. Graceful: returns `None` on any error.
pub(in super::super) async fn run_review_summary(
    llm: &dyn ReviewLlm,
    review_summary_enabled: bool,
    diff: &str,
    issues: &[ReviewIssueRecord],
) -> Option<ReviewSummary> {
    if !review_summary_enabled {
        return None;
    }
    if diff.is_empty() {
        return None;
    }

    let files = collect_diff_files(diff);
    let user_prompt = build_summary_user_prompt(diff, &files);
    let response = match llm.chat(SUMMARY_SYSTEM_PROMPT, &user_prompt).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[run_review_summary] cheap-model call failed: {e:?}");
            return None;
        }
    };

    let (one_line, walkthrough) = parse_summary_response(&response)?;
    let (blocking_count, non_blocking_count) = count_blocking(issues);

    Some(ReviewSummary {
        one_line_summary: one_line,
        walkthrough_by_file: walkthrough,
        blocking_count,
        non_blocking_count,
    })
}
