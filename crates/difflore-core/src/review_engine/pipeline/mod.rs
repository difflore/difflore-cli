use super::parse::{parse_issues, severity_rank};
use super::prompts::{build_segmented_prompt, build_user_prompt};
use super::{
    HttpReviewLlm, ReviewCheckInput, ReviewCheckResult, ReviewIssueRecord, ReviewLlm,
    ReviewPerspective, ReviewStats,
};
use crate::observability::trajectory::{RuleSource, TrajectoryBuilder, TrajectoryStep};
use gate4agent::CliTool;

mod chat;
mod judge;
mod resolver;
mod rules;
mod validate;

pub(super) use chat::resolve_review_engine;
#[cfg(test)]
pub(super) use validate::{run_review_summary, verify_pass};

use chat::{
    PerspectiveRun, call_review_engine, get_active_provider, make_review_llm, run_one_perspective,
};
use rules::{build_recalled_verdicts, recall_past_verdicts_for_review};
use validate::{
    run_review_summary as run_review_summary_internal, verify_pass as verify_pass_internal,
};

pub(super) fn repo_scopes_for_input(input: &ReviewCheckInput) -> Vec<String> {
    let mut scopes = Vec::new();
    if let Some(repo) = input.repo_full_name.as_deref() {
        let repo = repo.trim();
        if !repo.is_empty() {
            scopes.push(repo.to_owned());
        }
    }
    for repo in &input.repo_full_name_aliases {
        let repo = repo.trim();
        if repo.is_empty() {
            continue;
        }
        if !scopes
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(repo))
        {
            scopes.push(repo.to_owned());
        }
    }
    scopes
}

/// Candidate rule pool size requested at review time when the applicability
/// judge is enabled. Deeper than [`crate::context::DEFAULT_TOP_K_RULES`] since
/// review is latency-tolerant; the judge then filters it down. The assembler's
/// `rule_token_budget` still bounds what reaches the prompt.
const JUDGE_CANDIDATE_POOL_TOP_K: usize = 18;

/// Prepared matched-rule context: rendered rules text plus the parallel
/// id/title/count bookkeeping the rest of the pipeline consumes.
struct PreparedReviewRules {
    rules_text: Option<String>,
    count: i32,
    ids: Vec<String>,
    titles: Vec<String>,
}

/// Join rule items into the `rules_text` blob the review prompt expects (one
/// rule's `content` per section, blank-line separated) — the same shape
/// `intent_filter::maybe_rerank_for_review` produces.
fn rules_text_from_items(
    items: &[crate::context::types::ContextSourceItemRecord],
) -> Option<String> {
    if items.is_empty() {
        return None;
    }
    Some(
        items
            .iter()
            .map(|item| item.content.clone())
            .collect::<Vec<_>>()
            .join("\n\n"),
    )
}

/// Shared matched-rule preparation for both the single-pass and
/// multi-perspective review paths.
///
/// Retrieves the candidate rule pool (deepened to
/// [`JUDGE_CANDIDATE_POOL_TOP_K`] when the applicability judge is enabled),
/// applies the intent rerank, and — when the judge is enabled — asks the
/// review LLM which recalled rules apply to this diff, dropping the rest and
/// rebuilding `rules_text`/ids/titles from the survivors.
///
/// With the judge OFF, retrieval depth, rerank, and `rules_text` are unchanged
/// so the prompt stays byte-identical.
async fn prepare_review_rules(
    db: &sqlx::SqlitePool,
    input: &ReviewCheckInput,
    retrieval_query: &str,
    repo_scopes: &[String],
    judge_llm: &dyn ReviewLlm,
    review_engine: &crate::domain::models::ReviewEngineRecord,
    log_tag: &str,
) -> PreparedReviewRules {
    if input.project_id.is_empty() {
        return PreparedReviewRules {
            rules_text: None,
            count: 0,
            ids: Vec::new(),
            titles: Vec::new(),
        };
    }

    let judge_enabled = review_engine.rule_applicability_judge;
    // Deepen the candidate pool only when the judge will filter it back down.
    let top_k_override = judge_enabled.then_some(JUDGE_CANDIDATE_POOL_TOP_K);

    // Send the WHOLE changeset as path hints so rules tagged for any changed
    // file can get a boost (a multi-file diff no longer collapses onto the
    // primary file). Callers that know the authoritative file list pass
    // `diff_files`; otherwise derive it from the diff text.
    // With no files at all, fall back to the single-file hint.
    let changeset: Vec<String> = if input.diff_files.is_empty() {
        collect_diff_files(&input.diff_content)
    } else {
        input.diff_files.clone()
    };
    let target_scope = if changeset.is_empty() {
        input
            .file_path
            .as_deref()
            .map(crate::context::retrieval::TargetScope::File)
    } else {
        Some(crate::context::retrieval::TargetScope::Changeset(
            &changeset,
        ))
    };

    let pack = match crate::context::orchestrator::prepare_with_scope_and_repo_scopes_with_top_k(
        db,
        &input.project_id,
        input.engine.as_deref().unwrap_or("claude"),
        retrieval_query,
        Some("review"),
        target_scope,
        repo_scopes,
        top_k_override,
    )
    .await
    {
        Ok(pack) => pack,
        Err(e) => {
            if crate::infra::env::debug_providers() {
                eprintln!("[{log_tag}] context prepare failed: {e:?}, proceeding without rules");
            }
            return PreparedReviewRules {
                rules_text: None,
                count: 0,
                ids: Vec::new(),
                titles: Vec::new(),
            };
        }
    };

    let reranked =
        crate::context::intent_filter::maybe_rerank_for_review(&pack.rule_context, retrieval_query);

    // Judge-OFF path: count from the reranked len, else the assembler's
    // `metadata.rule_count`; ids/titles from whichever item set.
    if !judge_enabled {
        let (rules_text, count, ids, titles) = if let Some((reranked, rules_text)) = reranked {
            let count = i32::try_from(reranked.len()).unwrap_or(i32::MAX);
            let (ids, titles) = matched_rule_ids_and_titles(&reranked);
            (rules_text, count, ids, titles)
        } else {
            let count = i32::try_from(pack.metadata.rule_count).unwrap_or(i32::MAX);
            let (ids, titles) = matched_rule_ids_and_titles(&pack.rule_context);
            (pack.sections.rules, count, ids, titles)
        };
        return PreparedReviewRules {
            rules_text,
            count,
            ids,
            titles,
        };
    }

    // Judge-ON path: take the reranked items (else the full retrieved
    // context), let the judge drop non-applicable rules, then derive
    // text/ids/titles from the final pool so all three stay consistent.
    let pool: Vec<_> = match reranked {
        Some((reranked, _reranked_text)) => reranked,
        None => pack.rule_context.clone(),
    };

    let pool = judge::run_applicability_judge(judge_llm, true, &input.diff_content, pool).await;

    let rules_text = rules_text_from_items(&pool);
    let count = i32::try_from(pool.len()).unwrap_or(i32::MAX);
    let (ids, titles) = matched_rule_ids_and_titles(&pool);
    PreparedReviewRules {
        rules_text,
        count,
        ids,
        titles,
    }
}

pub(in super::super) fn count_blocking(issues: &[ReviewIssueRecord]) -> (u32, u32) {
    let mut blocking = 0u32;
    let mut non_blocking = 0u32;
    for i in issues {
        match i.severity.as_str() {
            "error" | "critical" => blocking += 1,
            _ => non_blocking += 1,
        }
    }
    (blocking, non_blocking)
}

pub(in super::super) fn collect_diff_files(diff: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("+++ ") {
            let file = rest.strip_prefix("b/").unwrap_or(rest).trim().to_owned();
            if file.is_empty() || file == "/dev/null" {
                continue;
            }
            if !out.iter().any(|f| f == &file) {
                out.push(file);
            }
        }
    }
    out
}

/// How `run_review` talks to the LLM: a remote HTTP provider when configured,
/// else a local agent CLI driven through `gate4agent`. Resolved once per
/// review by `resolve_review_engine`.
#[derive(Debug, Clone)]
pub enum ReviewEngine {
    HttpProvider {
        provider_name: String,
        base_url: String,
        api_key: String,
        model: String,
    },
    AgentCli {
        tool: CliTool,
        /// Empty string lets the CLI default kick in; populated when the user
        /// configured a model in `providers setup`.
        model: String,
    },
}

/// Merge issues from multiple perspective passes.
///
/// Dedupe key: `(file, line, rule_id_or_rule)`. When duplicates exist, the
/// issue with the highest severity wins, and the `perspectives` vector
/// lists every perspective (in fixed canonical order) whose pass flagged it.
pub fn merge_perspective_issues(
    per_perspective: Vec<(ReviewPerspective, Vec<ReviewIssueRecord>)>,
) -> Vec<ReviewIssueRecord> {
    use std::collections::BTreeMap;

    // Preserve first-seen order while still deduping by key.
    let mut order: Vec<String> = Vec::new();
    let mut merged: BTreeMap<String, ReviewIssueRecord> = BTreeMap::new();

    for (persp, issues) in per_perspective {
        let persp_name = persp.name();
        for mut issue in issues {
            let key = format!(
                "{}|{}|{}",
                issue.file.as_deref().unwrap_or_default(),
                issue.line.map(|n| n.to_string()).unwrap_or_default(),
                issue.rule_id.as_deref().unwrap_or(issue.rule.as_str()),
            );

            if let Some(existing) = merged.get_mut(&key) {
                if severity_rank(&issue.severity) > severity_rank(&existing.severity) {
                    let mut perspectives = existing.perspectives.clone();
                    if !perspectives.iter().any(|p| p == persp_name) {
                        perspectives.push(persp_name.to_owned());
                    }
                    issue.perspectives = perspectives;
                    *existing = issue;
                } else if !existing.perspectives.iter().any(|p| p == persp_name) {
                    existing.perspectives.push(persp_name.to_owned());
                }
            } else {
                if !issue.perspectives.iter().any(|p| p == persp_name) {
                    issue.perspectives.push(persp_name.to_owned());
                }
                order.push(key.clone());
                merged.insert(key, issue);
            }
        }
    }

    // Reorder perspectives on each issue to a stable canonical order.
    let canonical = [
        ReviewPerspective::Safety.name(),
        ReviewPerspective::Performance.name(),
        ReviewPerspective::Style.name(),
        ReviewPerspective::Docs.name(),
        ReviewPerspective::ApiDesign.name(),
    ];

    order
        .into_iter()
        .filter_map(|k| merged.remove(&k))
        .map(|mut issue| {
            let mut sorted: Vec<String> = canonical
                .iter()
                .filter(|c| issue.perspectives.iter().any(|p| p == *c))
                .map(ToString::to_string)
                .collect();
            for p in &issue.perspectives {
                if !sorted.iter().any(|s| s == p) {
                    sorted.push(p.clone());
                }
            }
            issue.perspectives = sorted;
            issue
        })
        .collect()
}

fn matched_rule_ids_and_titles(
    rule_context: &[crate::context::types::ContextSourceItemRecord],
) -> (Vec<String>, Vec<String>) {
    let ids = rule_context
        .iter()
        .map(|item| item.source_id.clone())
        .collect();
    let titles = rule_context
        .iter()
        .map(|item| {
            item.title
                .clone()
                .filter(|title| !title.trim().is_empty())
                .unwrap_or_else(|| item.source_id.clone())
        })
        .collect();
    (ids, titles)
}

fn issue_text_for_attribution(issue: &ReviewIssueRecord) -> String {
    format!(
        "{} {} {} {}",
        issue.rule,
        issue.message,
        issue.suggestion.as_deref().unwrap_or_default(),
        issue.file.as_deref().unwrap_or_default(),
    )
    .to_ascii_lowercase()
}

fn contains_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| text.contains(needle))
}

fn is_workflow_pin_issue(issue: &ReviewIssueRecord) -> bool {
    let text = issue_text_for_attribution(issue);
    let workflow_context = issue
        .file
        .as_deref()
        .is_some_and(|file| file.contains(".github/workflows/"))
        || contains_any(
            &text,
            &[
                "github action",
                "actions/",
                "uses:",
                "workflow",
                "checkout@",
            ],
        );
    let pin_context = contains_any(
        &text,
        &[
            "pin",
            "sha",
            "immutable",
            "mutable",
            "floating",
            "@main",
            "@master",
        ],
    );
    workflow_context && pin_context
}

fn is_workflow_pin_rule_title(title: &str) -> bool {
    let text = title.to_ascii_lowercase();
    contains_any(&text, &["github action", "actions", "workflow"])
        && contains_any(&text, &["pin", "sha", "immutable"])
}

fn attribution_tokens(text: &str) -> std::collections::BTreeSet<String> {
    const STOPWORDS: &[&str] = &[
        "the", "and", "for", "from", "into", "with", "this", "that", "must", "should", "would",
        "could", "rule", "rules", "file", "line", "review", "code", "when", "where", "than",
        "then", "they", "them", "your", "their",
    ];
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .filter_map(|raw| {
            let token = raw.trim().to_ascii_lowercase();
            if token.is_empty() || token.len() < 3 {
                return None;
            }
            let token = match token.as_str() {
                "shas" => "sha".to_owned(),
                "references" => "reference".to_owned(),
                other => other.to_owned(),
            };
            (!STOPWORDS.contains(&token.as_str())).then_some(token)
        })
        .collect()
}

fn infer_rule_id_for_issue(
    issue: &ReviewIssueRecord,
    matched_rule_ids: &[String],
    matched_rule_titles: &[String],
) -> Option<String> {
    if matched_rule_ids.is_empty() {
        return None;
    }

    if is_workflow_pin_issue(issue)
        && let Some((idx, _)) = matched_rule_titles
            .iter()
            .enumerate()
            .find(|(_, title)| is_workflow_pin_rule_title(title))
    {
        return matched_rule_ids.get(idx).cloned();
    }

    let issue_tokens = attribution_tokens(&issue_text_for_attribution(issue));
    if issue_tokens.is_empty() {
        return None;
    }

    let mut best: Option<(usize, f32, usize)> = None;
    let mut second_best = 0.0_f32;
    for (idx, title) in matched_rule_titles.iter().enumerate() {
        let title_tokens = attribution_tokens(title);
        if title_tokens.is_empty() {
            continue;
        }
        let overlap = title_tokens
            .iter()
            .filter(|token| issue_tokens.contains(*token))
            .count();
        if overlap < 2 {
            continue;
        }
        let score = overlap as f32 / title_tokens.len() as f32;
        match best {
            Some((_, best_score, _)) if score > best_score => {
                second_best = best_score;
                best = Some((idx, score, overlap));
            }
            Some(_) => {
                second_best = second_best.max(score);
            }
            None => best = Some((idx, score, overlap)),
        }
    }

    let (idx, score, overlap) = best?;
    if overlap >= 2 && score >= 0.60 && score >= second_best + 0.15 {
        matched_rule_ids.get(idx).cloned()
    } else {
        None
    }
}

fn apply_missing_rule_attributions(
    issues: &mut [ReviewIssueRecord],
    matched_rule_ids: &[String],
    matched_rule_titles: &[String],
) {
    for issue in issues {
        if issue
            .rule_id
            .as_deref()
            .is_some_and(|rule_id| !rule_id.trim().is_empty())
        {
            continue;
        }
        if let Some(rule_id) = infer_rule_id_for_issue(issue, matched_rule_ids, matched_rule_titles)
        {
            issue.rule_id = Some(rule_id);
        }
    }
}

/// Snap each issue's `issue.line` to the exact new-file line via
/// [`resolver::resolve_issue_lines`], using the per-issue `snippets` (parallel
/// to `issues`) and the claimed line. Issues whose file isn't in the diff, or
/// that don't confidently match, are left untouched — this only ever sharpens
/// a line number, never regresses it. A shorter/empty `snippets` slice is
/// tolerated.
fn apply_hunk_line_resolution(
    issues: &mut [ReviewIssueRecord],
    snippets: &[Option<String>],
    diff: &str,
) {
    use std::collections::HashMap;

    let sections = split_diff_by_file(diff);
    let mut cache: HashMap<String, Vec<resolver::DiffHunk>> = HashMap::new();

    for (idx, issue) in issues.iter_mut().enumerate() {
        let Some(file) = issue.file.as_deref() else {
            continue;
        };
        let hunks = cache.entry(file.to_owned()).or_insert_with(|| {
            sections
                .get(file)
                .map(|section| resolver::parse_hunks(section))
                .unwrap_or_default()
        });
        if hunks.is_empty() {
            continue;
        }
        let target = resolver::ResolveTarget {
            snippet: snippets.get(idx).and_then(Clone::clone),
            claimed_line: issue.line,
        };
        if let Some((start, _end)) = resolver::resolve_issue_lines(&target, hunks) {
            issue.line = Some(start);
        }
    }
}

/// Split a multi-file unified diff into `path -> section` so each file's
/// hunks can be parsed independently. The section is the slice starting at
/// the file's `@@` hunks (file headers are tolerated by the hunk parser).
/// Keyed by the new-side (`+++ b/…`) path so it matches `issue.file`.
fn split_diff_by_file(diff: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    let mut current_path: Option<String> = None;
    let mut current_body = String::new();

    let flush = |path: &mut Option<String>,
                 body: &mut String,
                 out: &mut std::collections::HashMap<String, String>| {
        if let Some(p) = path.take() {
            if body.trim().is_empty() {
                body.clear();
            } else {
                out.insert(p, std::mem::take(body));
            }
        }
    };

    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            flush(&mut current_path, &mut current_body, &mut out);
            current_path = None;
            current_body.clear();
        } else if let Some(rest) = line.strip_prefix("+++ ") {
            let path = rest.strip_prefix("b/").unwrap_or(rest).trim();
            if !path.is_empty() && path != "/dev/null" {
                current_path = Some(path.to_owned());
            }
        }
        if current_path.is_some() {
            current_body.push_str(line);
            current_body.push('\n');
        }
    }
    flush(&mut current_path, &mut current_body, &mut out);
    out
}

/// Shared pre-LLM state built once per review, before either the single-pass or
/// multi-perspective path runs its own LLM fan-out.
///
/// Owns the matched-rule bookkeeping, recalled past verdicts, rendered user
/// prompt, and the settings snapshot the post-LLM finalization reuses. Building
/// it also pushes the retrieval / rules / past-verdict trajectory steps.
struct PreparedReviewContext {
    trace_id: String,
    settings: crate::domain::models::AppSettingsRecord,
    matched_rules: i32,
    matched_rule_ids: Vec<String>,
    matched_rule_titles: Vec<String>,
    past_verdicts: Vec<crate::context::types::PastVerdict>,
    user_prompt: String,
    prompt_tokens_estimate: i32,
}

/// Run the shared pre-LLM pipeline both review paths begin with: build the
/// retrieval intent/query and repo scopes, snapshot settings, prepare matched
/// rules through `judge_llm`, recall past verdicts, render the user prompt, and
/// push the `ChunksRetrieved` / `RulesApplied` / `PastVerdictsRecalled`
/// trajectory steps.
///
/// `log_tag` distinguishes the single-pass (`review_check`) and multi-
/// perspective (`review_check_multi`) call sites in diagnostics. `judge_llm`
/// is the caller's already-resolved review LLM, reused by the applicability
/// judge.
async fn prepare_review_context(
    db: &sqlx::SqlitePool,
    input: &ReviewCheckInput,
    judge_llm: &dyn ReviewLlm,
    log_tag: &str,
    mut trajectory: Option<&mut TrajectoryBuilder>,
) -> PreparedReviewContext {
    let trace_id = uuid::Uuid::new_v4().to_string();

    let retrieval_intent = crate::context::intent_filter::build_review_intent_text(
        input.file_path.as_deref(),
        &input.diff_content,
    );
    let retrieval_query = if retrieval_intent.trim().is_empty() {
        input.diff_content.as_str()
    } else {
        retrieval_intent.as_str()
    };
    let repo_scopes = repo_scopes_for_input(input);

    // Settings gate the applicability judge below and the past-verdict recall
    // / self-check / summary steps further down.
    let settings = crate::infra::settings::get().await.unwrap_or_default();

    let PreparedReviewRules {
        rules_text,
        count: matched_rules,
        ids: matched_rule_ids,
        titles: matched_rule_titles,
    } = prepare_review_rules(
        db,
        input,
        retrieval_query,
        &repo_scopes,
        judge_llm,
        &settings.review_engine,
        log_tag,
    )
    .await;

    if let Some(tb) = trajectory.as_deref_mut() {
        tb.push(TrajectoryStep::ChunksRetrieved {
            count: matched_rules.try_into().unwrap_or(usize::MAX),
            symbols: matched_rule_titles.clone(),
            similarity_scores: Vec::new(),
        });
        tb.push(TrajectoryStep::RulesApplied {
            rule_ids: matched_rule_ids.clone(),
            source: RuleSource::Team,
        });
    }

    // Past-verdict recall. Preview callers skip it for a bounded first answer;
    // they can inspect memory separately with `difflore recall --diff`.
    let past_verdicts = if input.fast_preview {
        Vec::new()
    } else {
        recall_past_verdicts_for_review(
            &settings,
            &input.diff_content,
            if input.project_id.is_empty() {
                None
            } else {
                Some(&input.project_id)
            },
            &repo_scopes,
        )
        .await
    };

    if let Some(tb) = trajectory {
        let recalled_items = build_recalled_verdicts(&past_verdicts);
        let top_similarities: Vec<f32> =
            recalled_items.iter().map(|item| item.similarity).collect();
        tb.push(TrajectoryStep::PastVerdictsRecalled {
            count: past_verdicts.len(),
            top_similarities,
            recalled_items,
        });
    }

    let user_prompt = build_user_prompt(
        &input.diff_content,
        rules_text.as_deref(),
        input.file_path.as_deref(),
    );
    let prompt_tokens_estimate = (i32::try_from(user_prompt.len())
        .unwrap_or(i32::MAX)
        .saturating_add(3))
        / 4;

    PreparedReviewContext {
        trace_id,
        settings,
        matched_rules,
        matched_rule_ids,
        matched_rule_titles,
        past_verdicts,
        user_prompt,
        prompt_tokens_estimate,
    }
}

/// Run the shared post-LLM finalization both review paths end with, starting
/// from the already-verified, attribution-pending issues.
///
/// Pushes the `SelfCheck` trajectory step, applies missing rule attributions,
/// sorts by descending confidence, generates the review summary, pushes the
/// `FinalDecision` trajectory step, and assembles the `ReviewCheckResult`.
///
/// `pre_verify_count` is the issue count before the self-check pass (for the
/// drop tally); `perspective_count` is 1 for single-pass and 5 for multi.
#[allow(clippy::too_many_arguments)]
async fn finalize_review(
    llm: &dyn ReviewLlm,
    prepared: PreparedReviewContext,
    fast_preview: bool,
    diff_content: &str,
    mut issues: Vec<ReviewIssueRecord>,
    pre_verify_count: usize,
    perspective_count: u32,
    mut trajectory: Option<&mut TrajectoryBuilder>,
) -> ReviewCheckResult {
    let PreparedReviewContext {
        trace_id,
        settings,
        matched_rules,
        matched_rule_ids,
        matched_rule_titles,
        past_verdicts,
        user_prompt: _,
        prompt_tokens_estimate,
    } = prepared;

    if let Some(tb) = trajectory.as_deref_mut() {
        let keep_count = u32::try_from(issues.len()).unwrap_or(u32::MAX);
        let drop_count =
            u32::try_from(pre_verify_count.saturating_sub(issues.len())).unwrap_or(u32::MAX);
        let avg_confidence = if issues.is_empty() {
            0.0
        } else {
            issues.iter().map(|i| i.confidence).sum::<f32>() / (issues.len() as f32)
        };
        tb.push(TrajectoryStep::SelfCheck {
            keep_count,
            drop_count,
            avg_confidence,
        });
    }

    apply_missing_rule_attributions(&mut issues, &matched_rule_ids, &matched_rule_titles);
    issues.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let summary = run_review_summary_internal(
        llm,
        settings.review_engine.review_summary_enabled && !fast_preview,
        diff_content,
        &issues,
    )
    .await;

    if let Some(tb) = trajectory.as_deref_mut() {
        let ids = issues
            .iter()
            .map(|i| i.rule_id.clone().unwrap_or_else(|| i.rule.clone()))
            .collect();
        tb.push(TrajectoryStep::FinalDecision {
            issue_ids_emitted: ids,
        });
    }

    let stats = ReviewStats {
        input_tokens: u32::try_from(prompt_tokens_estimate.max(0)).unwrap_or(u32::MAX),
        duration_ms: None,
        perspective_count,
        past_verdicts_used: u32::try_from(past_verdicts.len()).unwrap_or(u32::MAX),
        trajectory_step_count: trajectory
            .as_deref()
            .map(|tb| u32::try_from(tb.len()).unwrap_or(u32::MAX)),
    };

    ReviewCheckResult {
        issues,
        matched_rules,
        matched_rule_ids,
        matched_rule_titles,
        prompt_tokens_estimate,
        trace_id,
        summary,
        stats: Some(stats),
    }
}

/// Multi-perspective review.
pub async fn run_review_multi(
    db: &sqlx::SqlitePool,
    input: ReviewCheckInput,
) -> crate::Result<ReviewCheckResult> {
    run_review_multi_with_trajectory(db, input, None).await
}

/// Trajectory-aware variant of `run_review_multi`.
pub async fn run_review_multi_with_trajectory(
    db: &sqlx::SqlitePool,
    input: ReviewCheckInput,
    mut trajectory: Option<&mut TrajectoryBuilder>,
) -> crate::Result<ReviewCheckResult> {
    // Active provider, shared by all perspectives. The applicability judge, when
    // enabled, reuses it through its own `HttpReviewLlm`.
    let (provider_name, base_url, api_key, model) = get_active_provider(db).await?;
    let judge_llm = HttpReviewLlm {
        provider_name: provider_name.clone(),
        base_url: base_url.clone(),
        api_key: api_key.clone(),
        model: model.clone(),
    };

    let prepared = prepare_review_context(
        db,
        &input,
        &judge_llm,
        "review_check_multi",
        trajectory.as_deref_mut(),
    )
    .await;

    let (safety_issues, perf_issues, style_issues, docs_issues, api_design_issues) = tokio::join!(
        run_one_perspective(PerspectiveRun {
            provider_name: &provider_name,
            base_url: &base_url,
            api_key: &api_key,
            model: &model,
            user_prompt: &prepared.user_prompt,
            perspective: ReviewPerspective::Safety,
            diff_content: &input.diff_content,
            past_verdicts: &prepared.past_verdicts,
        }),
        run_one_perspective(PerspectiveRun {
            provider_name: &provider_name,
            base_url: &base_url,
            api_key: &api_key,
            model: &model,
            user_prompt: &prepared.user_prompt,
            perspective: ReviewPerspective::Performance,
            diff_content: &input.diff_content,
            past_verdicts: &prepared.past_verdicts,
        }),
        run_one_perspective(PerspectiveRun {
            provider_name: &provider_name,
            base_url: &base_url,
            api_key: &api_key,
            model: &model,
            user_prompt: &prepared.user_prompt,
            perspective: ReviewPerspective::Style,
            diff_content: &input.diff_content,
            past_verdicts: &prepared.past_verdicts,
        }),
        run_one_perspective(PerspectiveRun {
            provider_name: &provider_name,
            base_url: &base_url,
            api_key: &api_key,
            model: &model,
            user_prompt: &prepared.user_prompt,
            perspective: ReviewPerspective::Docs,
            diff_content: &input.diff_content,
            past_verdicts: &prepared.past_verdicts,
        }),
        run_one_perspective(PerspectiveRun {
            provider_name: &provider_name,
            base_url: &base_url,
            api_key: &api_key,
            model: &model,
            user_prompt: &prepared.user_prompt,
            perspective: ReviewPerspective::ApiDesign,
            diff_content: &input.diff_content,
            past_verdicts: &prepared.past_verdicts,
        }),
    );

    if let Some(tb) = trajectory.as_deref_mut() {
        let per_call_input = u32::try_from(prepared.prompt_tokens_estimate).unwrap_or(u32::MAX);
        for perspective in ReviewPerspective::all() {
            tb.push(TrajectoryStep::LlmCall {
                perspective: perspective.name().to_owned(),
                input_tokens: per_call_input,
                output_tokens: 0,
                raw_output: None,
            });
        }
    }

    let issues = merge_perspective_issues(vec![
        (ReviewPerspective::Safety, safety_issues),
        (ReviewPerspective::Performance, perf_issues),
        (ReviewPerspective::Style, style_issues),
        (ReviewPerspective::Docs, docs_issues),
        (ReviewPerspective::ApiDesign, api_design_issues),
    ]);

    let llm: Box<dyn ReviewLlm> = Box::new(HttpReviewLlm {
        provider_name,
        base_url,
        api_key,
        model,
    });
    let pre_verify_count = issues.len();
    let mut issues = verify_pass_internal(
        llm.as_ref(),
        prepared.settings.review_engine.self_check_enabled && !input.fast_preview,
        &input.diff_content,
        issues,
    )
    .await;

    // Hunk-aware line snap (gated; default off). The multi-pass merge drops
    // snippets, so this path snaps using the claimed line only.
    if prepared.settings.review_engine.hunk_line_resolution {
        apply_hunk_line_resolution(&mut issues, &[], &input.diff_content);
    }

    Ok(finalize_review(
        llm.as_ref(),
        prepared,
        input.fast_preview,
        &input.diff_content,
        issues,
        pre_verify_count,
        5,
        trajectory,
    )
    .await)
}

/// Stable label for the code path selected by `run_review_smart`.
pub const fn select_review_mode(multi_perspective: bool) -> &'static str {
    if multi_perspective { "multi" } else { "single" }
}

#[allow(clippy::items_after_test_module)]
#[cfg(test)]
mod tests {
    use super::*;

    fn review_input(repo: Option<&str>, aliases: Vec<&str>) -> ReviewCheckInput {
        ReviewCheckInput {
            project_id: "project-1".to_owned(),
            diff_content: String::new(),
            file_path: None,
            diff_files: Vec::new(),
            engine: None,
            review_id: None,
            repo_full_name: repo.map(str::to_owned),
            repo_full_name_aliases: aliases.into_iter().map(str::to_owned).collect(),
            fast_preview: false,
        }
    }

    #[test]
    fn repo_scopes_include_origin_and_upstream_aliases() {
        let input = review_input(
            Some("difflore-fixtures/router"),
            vec!["difflore-fixtures/router", "tanstack/router"],
        );

        assert_eq!(
            repo_scopes_for_input(&input),
            vec![
                "difflore-fixtures/router".to_owned(),
                "tanstack/router".to_owned(),
            ],
        );
    }

    #[test]
    fn repo_scopes_dedupe_aliases_case_insensitively() {
        let input = review_input(
            Some("TanStack/router"),
            vec!["tanstack/router", "  ", "difflore-fixtures/router"],
        );

        assert_eq!(
            repo_scopes_for_input(&input),
            vec![
                "TanStack/router".to_owned(),
                "difflore-fixtures/router".to_owned(),
            ],
        );
    }

    #[test]
    fn fast_preview_input_marks_secondary_review_passes_skippable() {
        let mut input = review_input(Some("owner/repo"), vec![]);
        assert!(!input.fast_preview);

        input.fast_preview = true;

        assert!(input.fast_preview);
    }

    #[test]
    fn workflow_pin_issue_gets_recalled_rule_id_when_model_omits_it() {
        let issue = ReviewIssueRecord {
            severity: "warning".to_owned(),
            rule: "Pin GitHub Actions to immutable references".to_owned(),
            rule_id: None,
            message: "actions/checkout@main is a floating ref".to_owned(),
            file: Some(".github/workflows/pr.yml".to_owned()),
            line: Some(26),
            suggestion: Some("Use a commit SHA instead of main.".to_owned()),
            source_badge: None,
            perspectives: Vec::new(),
            confidence: 0.98,
        };

        let rule_id = infer_rule_id_for_issue(
            &issue,
            &[
                "pin-actions-rule".to_owned(),
                "version-update-rule".to_owned(),
            ],
            &[
                "Pin Actions to commit SHAs".to_owned(),
                "Update GitHub Actions versions atomically".to_owned(),
            ],
        );

        assert_eq!(rule_id.as_deref(), Some("pin-actions-rule"));
    }

    #[test]
    fn missing_rule_attribution_stays_empty_for_ambiguous_text() {
        let mut issues = vec![ReviewIssueRecord {
            severity: "warning".to_owned(),
            rule: "Improve code".to_owned(),
            rule_id: None,
            message: "This should be cleaner.".to_owned(),
            file: Some("src/lib.rs".to_owned()),
            line: Some(1),
            suggestion: Some("Refactor it.".to_owned()),
            source_badge: None,
            perspectives: Vec::new(),
            confidence: 0.8,
        }];

        apply_missing_rule_attributions(
            &mut issues,
            &["pin-actions-rule".to_owned()],
            &["Pin Actions to commit SHAs".to_owned()],
        );

        assert!(issues[0].rule_id.is_none());
    }

    const MULTI_FILE_DIFF: &str = "\
diff --git a/src/a.rs b/src/a.rs
index 1111111..2222222 100644
--- a/src/a.rs
+++ b/src/a.rs
@@ -5,4 +5,5 @@ fn a() {
     let x = 1;
     let y = 2;
+    let z = dangerous(x, y);
     done();
 }
diff --git a/src/b.rs b/src/b.rs
index 3333333..4444444 100644
--- a/src/b.rs
+++ b/src/b.rs
@@ -20,3 +20,4 @@ fn b() {
     setup();
+    let secret = read_env();
     teardown();
";

    fn issue_at(file: &str, line: i32) -> ReviewIssueRecord {
        ReviewIssueRecord {
            severity: "warning".to_owned(),
            rule: "r".to_owned(),
            rule_id: None,
            message: "m".to_owned(),
            file: Some(file.to_owned()),
            line: Some(line),
            suggestion: None,
            source_badge: None,
            perspectives: Vec::new(),
            confidence: 0.9,
        }
    }

    #[test]
    fn split_diff_by_file_keys_on_new_side_path() {
        let map = split_diff_by_file(MULTI_FILE_DIFF);
        assert_eq!(map.len(), 2);
        assert!(map.contains_key("src/a.rs"));
        assert!(map.contains_key("src/b.rs"));
        assert!(map["src/a.rs"].contains("dangerous(x, y)"));
        assert!(map["src/b.rs"].contains("read_env()"));
    }

    #[test]
    fn hunk_resolution_snaps_issue_to_exact_line_via_snippet() {
        let mut issues = vec![issue_at("src/a.rs", 999), issue_at("src/b.rs", 1)];
        let snippets = vec![
            Some("let z = dangerous(x, y);".to_owned()),
            Some("let secret = read_env();".to_owned()),
        ];
        apply_hunk_line_resolution(&mut issues, &snippets, MULTI_FILE_DIFF);
        // a.rs new-side: 5,6 context, 7 added `z`, ... → line 7.
        assert_eq!(issues[0].line, Some(7));
        // b.rs new-side: 20 context, 21 added `secret` → line 21.
        assert_eq!(issues[1].line, Some(21));
    }

    #[test]
    fn hunk_resolution_leaves_line_when_file_not_in_diff() {
        let mut issues = vec![issue_at("src/unknown.rs", 42)];
        let snippets = vec![Some("whatever".to_owned())];
        apply_hunk_line_resolution(&mut issues, &snippets, MULTI_FILE_DIFF);
        assert_eq!(issues[0].line, Some(42), "untouched when no diff section");
    }

    #[test]
    fn hunk_resolution_snaps_via_claimed_line_without_snippet() {
        // No snippets at all (mirrors the multi-perspective path). The model
        // claimed line 6 in a.rs, which is a real new-side context line.
        let mut issues = vec![issue_at("src/a.rs", 6)];
        apply_hunk_line_resolution(&mut issues, &[], MULTI_FILE_DIFF);
        assert_eq!(issues[0].line, Some(6));
    }

    #[test]
    fn hunk_resolution_tolerates_shorter_snippet_slice() {
        // snippets slice shorter than issues — extra issues fall back to
        // claimed-line snap, no panic.
        let mut issues = vec![issue_at("src/a.rs", 7), issue_at("src/b.rs", 21)];
        let snippets = vec![Some("let z = dangerous(x, y);".to_owned())];
        apply_hunk_line_resolution(&mut issues, &snippets, MULTI_FILE_DIFF);
        assert_eq!(issues[0].line, Some(7));
        assert_eq!(issues[1].line, Some(21));
    }

    #[test]
    fn hunk_resolution_falls_back_when_nothing_matches() {
        // Backward-safety guarantee: the file IS in the diff, but neither the
        // snippet nor the claimed line corresponds to any hunk line. Hunk
        // attribution must decline (resolver → None) and leave the model's
        // claimed line exactly as-is, so we degrade to the prior token-overlap
        // behaviour instead of inventing a wrong line.
        let mut issues = vec![issue_at("src/a.rs", 900)];
        let snippets = vec![Some("text that appears nowhere in the diff".to_owned())];
        apply_hunk_line_resolution(&mut issues, &snippets, MULTI_FILE_DIFF);
        assert_eq!(
            issues[0].line,
            Some(900),
            "no confident hunk match → claimed line preserved (no regression)"
        );
    }

    #[test]
    fn hunk_resolution_maps_multiline_finding_to_range_start() {
        // A finding whose snippet spans two consecutive new-side lines must be
        // attributed to the START of that exact range. In a.rs the added
        // `dangerous` line is 7 and the following context `done();` is 8, so a
        // two-line snippet resolves to the range 7..=8; `issue.line` carries
        // the range start (7). This is the "exact changed line RANGE" mapping
        // the hunk resolver provides over token overlap.
        let mut issues = vec![issue_at("src/a.rs", 1)];
        let snippets = vec![Some("let z = dangerous(x, y);\ndone();".to_owned())];
        apply_hunk_line_resolution(&mut issues, &snippets, MULTI_FILE_DIFF);
        assert_eq!(
            issues[0].line,
            Some(7),
            "multi-line finding anchors on the first line of the changed range"
        );
    }

    // hunk_line_resolution end-to-end coverage, using a real diff + LLM
    // response captured from a live `difflore review` run against
    // difflore-test-e2e/hono/src/compose.ts. Ground-truth new-file lines: 42,
    // 43, 52. Drives the production gate path: split_diff_by_file →
    // parse_hunks → resolve_issue_lines, via apply_hunk_line_resolution.
    // OFF = claimed line untouched; ON = apply_hunk_line_resolution.

    const HONO_DIFF: &str = "\
--- a/src/compose.ts
+++ b/src/compose.ts
@@ -39,6 +39,9 @@ export const compose = <E extends Env = Env>(
       let isError = false
       let handler

+      const apiKey = \"sk-live-1234567890abcdef\"
+      console.log(\"dispatching middleware at index \" + i + \" key=\" + apiKey)
+
       if (middleware[i]) {
         handler = middleware[i][0][0]
         context.req.routeIndex = i
@@ -46,6 +49,10 @@ export const compose = <E extends Env = Env>(
         handler = (i === middleware.length && next) || undefined
       }

+      if (handler == null) {
+        handler = middleware[i][0][0]
+      }
+
       if (handler) {
         try {
           res = await handler(context, () => dispatch(i + 1))
";

    // (real_snippet, ground_truth_new_file_line) for each issue the model
    // actually returned.
    fn hono_cases() -> Vec<(String, i32)> {
        vec![
            (
                "      const apiKey = \"sk-live-1234567890abcdef\"".to_owned(),
                42,
            ),
            (
                "      console.log(\"dispatching middleware at index \" + i + \" key=\" + apiKey)"
                    .to_owned(),
                43,
            ),
            (
                "      if (handler == null) {\n        handler = middleware[i][0][0]\n      }"
                    .to_owned(),
                52,
            ),
        ]
    }

    /// Build the issue set + snippet set for a scenario.
    /// `claimed[i]` is the line the model "claimed"; `with_snippet` chooses
    /// whether the real snippet is supplied (snippet path) or not (claimed-
    /// line-only path).
    fn build(claimed: &[i32], with_snippet: bool) -> (Vec<ReviewIssueRecord>, Vec<Option<String>>) {
        let cases = hono_cases();
        let issues = claimed
            .iter()
            .map(|&l| issue_at("src/compose.ts", l))
            .collect();
        let snippets = cases
            .iter()
            .map(|(s, _)| if with_snippet { Some(s.clone()) } else { None })
            .collect();
        (issues, snippets)
    }

    fn ground_truth() -> Vec<i32> {
        hono_cases().into_iter().map(|(_, gt)| gt).collect()
    }

    /// Count how many issues land exactly on ground truth.
    fn precise_count(issues: &[ReviewIssueRecord], gt: &[i32]) -> usize {
        issues
            .iter()
            .zip(gt.iter())
            .filter(|(iss, g)| iss.line == Some(**g))
            .count()
    }

    #[test]
    fn measure_real_response_off_equals_on_no_change() {
        // Scenario A: the REAL claimed lines the model returned (already
        // correct). OFF should equal ON — feature is a no-op here.
        let gt = ground_truth();
        let claimed_real = gt.clone(); // model claimed 42,43,52 == GT
        let (off_issues, snippets) = build(&claimed_real, true);
        let off_precise = precise_count(&off_issues, &gt);

        let (mut on_issues, _) = build(&claimed_real, true);
        apply_hunk_line_resolution(&mut on_issues, &snippets, HONO_DIFF);
        let on_precise = precise_count(&on_issues, &gt);

        let on_lines: Vec<_> = on_issues.iter().map(|i| i.line).collect();
        eprintln!(
            "[MEASURE A real-response] OFF precise={off_precise}/3 ON precise={on_precise}/3 ON_lines={on_lines:?}"
        );
        assert_eq!(off_precise, 3, "model already correct on this diff");
        assert_eq!(on_precise, 3, "ON keeps all correct (no regression)");
    }

    #[test]
    fn measure_corrupted_lines_with_real_snippets() {
        // Scenario B: simulate the documented model failure modes the
        // resolver exists to fix (resolver.rs docstring: "diff-relative or
        // off-by-N numbers, or count from the hunk header"), with the REAL
        // snippets preserved.
        //
        //   issue0 (GT 42): diff-relative  -> 4  (counts within hunk body)
        //   issue1 (GT 43): off-by-2 high  -> 45
        //   issue2 (GT 52): hunk-header rel-> 49 (the @@ new_start, off by 3)
        let gt = ground_truth();
        let corrupted = vec![4, 45, 49];

        let (off_issues, _) = build(&corrupted, true);
        let off_precise = precise_count(&off_issues, &gt);

        let (mut on_issues, snippets) = build(&corrupted, true);
        apply_hunk_line_resolution(&mut on_issues, &snippets, HONO_DIFF);
        let on_precise = precise_count(&on_issues, &gt);

        let off_lines: Vec<_> = off_issues.iter().map(|i| i.line).collect();
        let on_lines: Vec<_> = on_issues.iter().map(|i| i.line).collect();
        eprintln!(
            "[MEASURE B corrupted+snippet] GT={gt:?} corrupted={corrupted:?} \
             OFF_lines={off_lines:?} (precise {off_precise}/3) \
             ON_lines={on_lines:?} (precise {on_precise}/3)"
        );
        // Honest assertions: ON must recover all 3 via snippet; OFF gets 0.
        assert_eq!(off_precise, 0, "all corrupted lines are wrong");
        assert_eq!(on_precise, 3, "snippet match recovers exact line for all");
    }

    #[test]
    fn measure_corrupted_lines_without_snippets_claimed_only() {
        // Scenario C: same corruption but NO snippets (mirrors the multi-
        // perspective merge path, which drops per-issue snippets). Tests the
        // weaker claimed-line snap + checks for any REGRESSION (snapping a
        // line further from GT than where it started).
        let gt = ground_truth();
        let corrupted = vec![4, 45, 49];

        let (off_issues, _) = build(&corrupted, false);
        let off_precise = precise_count(&off_issues, &gt);

        let (mut on_issues, _) = build(&corrupted, false);
        apply_hunk_line_resolution(&mut on_issues, &[], HONO_DIFF);
        let on_precise = precise_count(&on_issues, &gt);

        // Regression detector: for each issue, did ON move it strictly
        // farther from GT than OFF was?
        let mut regressions = 0;
        for ((off, on), &g) in off_issues.iter().zip(on_issues.iter()).zip(gt.iter()) {
            let off_d = (off.line.unwrap_or(g) - g).abs();
            let on_d = (on.line.unwrap_or(g) - g).abs();
            if on_d > off_d {
                regressions += 1;
            }
        }

        let off_lines: Vec<_> = off_issues.iter().map(|i| i.line).collect();
        let on_lines: Vec<_> = on_issues.iter().map(|i| i.line).collect();
        eprintln!(
            "[MEASURE C corrupted no-snippet] GT={gt:?} corrupted={corrupted:?} \
             OFF_lines={off_lines:?} (precise {off_precise}/3) \
             ON_lines={on_lines:?} (precise {on_precise}/3) regressions={regressions}"
        );
        // No assertion on precise count here (claimed-only is weaker); the
        // eprintln carries the numbers. Guard only against regressions.
        assert_eq!(
            regressions, 0,
            "claimed-line snap must not move AWAY from GT"
        );
    }

    #[test]
    fn measure_claimed_only_boundary_offbyone() {
        // Scenario C': the one case the claimed-line snap CAN help —
        // claimed lands just OUTSIDE the hunk (within the +/-2 tolerance) on
        // a non-existent new-side position. GT 43; claim 48 = one past
        // hunk1's last new line (47) -> snaps back to 47 (closer to GT, not
        // exact). GT 52; claim 59 = one past hunk2 last line (58) -> 58.
        let gt = vec![43, 52];
        let corrupted = vec![48, 59];
        let issues_off: Vec<_> = corrupted
            .iter()
            .map(|&l| issue_at("src/compose.ts", l))
            .collect();
        let mut issues_on = issues_off.clone();
        apply_hunk_line_resolution(&mut issues_on, &[], HONO_DIFF);
        let on_lines: Vec<_> = issues_on.iter().map(|i| i.line).collect();
        // Did ON move each line CLOSER to GT than OFF?
        let mut improved = 0;
        let mut regressed = 0;
        for ((off, on), &g) in issues_off.iter().zip(issues_on.iter()).zip(gt.iter()) {
            let off_d = (off.line.unwrap_or(g) - g).abs();
            let on_d = (on.line.unwrap_or(g) - g).abs();
            if on_d < off_d {
                improved += 1;
            }
            if on_d > off_d {
                regressed += 1;
            }
        }
        eprintln!(
            "[MEASURE C' claimed-only boundary] GT={gt:?} corrupted={corrupted:?} \
             ON_lines={on_lines:?} improved(closer)={improved} regressed={regressed}"
        );
        // Snap clamps an out-of-range claim back to the nearest real line:
        // strictly closer, never farther. (Closer, not exact — snippetless.)
        assert_eq!(regressed, 0);
    }

    #[test]
    fn ambiguous_duplicate_snippet_prefers_claimed_occurrence() {
        // The snippet `handler = middleware[i][0][0]` occurs on the new side at
        // BOTH line 46 (pre-existing context) and line 53 (the newly-added
        // dup). For an issue whose claimed line is 53, the claimed-line
        // tie-break must keep the nearer occurrence (53) rather than snapping
        // to the first match (46) — the end-to-end check of the resolver guard
        // (without it, ON would move a correctly-claimed line onto line 46).
        let snippet = "      handler = middleware[i][0][0]".to_owned();
        let mut issues = vec![issue_at("src/compose.ts", 53)];
        let snippets = vec![Some(snippet)];
        apply_hunk_line_resolution(&mut issues, &snippets, HONO_DIFF);
        assert_eq!(
            issues[0].line,
            Some(53),
            "must keep the claimed duplicate (53), not snap to the far one (46)"
        );
    }
}

pub async fn run_review_smart(
    db: &sqlx::SqlitePool,
    input: ReviewCheckInput,
) -> crate::Result<ReviewCheckResult> {
    let settings = crate::infra::settings::get().await.unwrap_or_default();
    let review_id = input.review_id.clone();
    let multi_perspective = settings.review_engine.multi_perspective;

    if review_id.is_none() {
        let started = std::time::Instant::now();
        let mut result = match select_review_mode(multi_perspective) {
            "multi" => run_review_multi(db, input).await?,
            _ => run_review(db, input).await?,
        };
        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        if let Some(stats) = result.stats.as_mut() {
            stats.duration_ms = Some(duration_ms);
        }
        return Ok(result);
    }

    let started = std::time::Instant::now();
    let mut trajectory = TrajectoryBuilder::new();
    let mut result = match select_review_mode(multi_perspective) {
        "multi" => run_review_multi_with_trajectory(db, input, Some(&mut trajectory)).await?,
        _ => run_review_with_trajectory(db, input, Some(&mut trajectory)).await?,
    };
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    if let Some(stats) = result.stats.as_mut() {
        stats.duration_ms = Some(duration_ms);
    }

    if let Some(id) = review_id {
        upload_review_telemetry(id, duration_ms, multi_perspective, &result, trajectory).await;
    }

    Ok(result)
}

/// Fire-and-forget telemetry writeback.
async fn upload_review_telemetry(
    review_id: String,
    duration_ms: u64,
    multi_perspective: bool,
    result: &ReviewCheckResult,
    trajectory: TrajectoryBuilder,
) {
    let cloud = crate::cloud::client::CloudClient::create().await;
    if !cloud.is_logged_in() {
        return;
    }

    let past_verdicts_used = trajectory.steps().iter().find_map(|step| match step {
        TrajectoryStep::PastVerdictsRecalled { count, .. } => {
            Some(u32::try_from(*count).unwrap_or(u32::MAX))
        }
        _ => None,
    });

    let metrics_req = crate::contract::RecordReviewMetricsRequest {
        input_tokens: Some(u32::try_from(result.prompt_tokens_estimate.max(0)).unwrap_or(u32::MAX)),
        output_tokens: None,
        estimated_cost_usd: None,
        duration_ms: Some(duration_ms),
        perspective_count: Some(if multi_perspective { 5 } else { 1 }),
        past_verdicts_used,
    };
    let trajectory_steps = if trajectory.is_empty() {
        None
    } else {
        match trajectory.into_json() {
            Ok(value) => Some(value),
            Err(err) => {
                eprintln!("difflore: failed to serialize review trajectory: {err}");
                None
            }
        }
    };

    let pool = crate::infra::db::init_db().await.ok();
    if let Some(pool) = pool {
        let q = crate::cloud::outbox::OutboxQueue::new(pool);
        let metrics_payload = serde_json::json!({
            "review_id": review_id,
            "req": metrics_req,
        });
        if let Ok(s) = serde_json::to_string(&metrics_payload) {
            let _ = q
                .enqueue(crate::cloud::outbox::kind::REVIEW_METRICS, &s)
                .await;
        }
        if let Some(steps) = trajectory_steps {
            let trajectory_payload = serde_json::json!({
                "pr_review_id": review_id,
                "steps": steps,
            });
            if let Ok(s) = serde_json::to_string(&trajectory_payload) {
                let _ = q.enqueue(crate::cloud::outbox::kind::TRAJECTORY, &s).await;
            }
        }
        let _ = crate::cloud::outbox::drain_outbox(&q, &cloud, 8).await;
    } else {
        let _ = cloud.record_review_metrics(&review_id, metrics_req).await;
        if let Some(steps) = trajectory_steps {
            let _ = cloud.save_trajectory(&review_id, steps).await;
        }
    }
}

pub async fn run_review(
    db: &sqlx::SqlitePool,
    input: ReviewCheckInput,
) -> crate::Result<ReviewCheckResult> {
    run_review_with_trajectory(db, input, None).await
}

/// Trajectory-aware variant of `run_review`.
pub async fn run_review_with_trajectory(
    db: &sqlx::SqlitePool,
    input: ReviewCheckInput,
    mut trajectory: Option<&mut TrajectoryBuilder>,
) -> crate::Result<ReviewCheckResult> {
    let engine = resolve_review_engine(db).await?;

    // The judge reuses the resolved engine via its own `ReviewLlm`; clone here
    // since the main review call consumes `engine` later.
    let judge_llm = make_review_llm(engine.clone());
    let prepared = prepare_review_context(
        db,
        &input,
        judge_llm.as_ref(),
        "review_check",
        trajectory.as_deref_mut(),
    )
    .await;

    let seg = build_segmented_prompt(
        None,
        &[],
        &input.diff_content,
        "",
        None,
        if prepared.past_verdicts.is_empty() {
            None
        } else {
            Some(&prepared.past_verdicts)
        },
    );

    if let Some(path) = crate::infra::env::fix_dump_dir() {
        let _ = std::fs::create_dir_all(&path);
        let _ = std::fs::write(format!("{path}/last_user.txt"), &prepared.user_prompt);
        let _ = std::fs::write(
            format!("{path}/last_system.txt"),
            format!("{}{}", seg.stable_prefix, seg.dynamic_suffix),
        );
    }

    let ai_response = call_review_engine(&engine, &seg, &prepared.user_prompt).await?;
    if let Some(path) = crate::infra::env::fix_dump_dir() {
        let _ = std::fs::write(format!("{path}/last_response.txt"), &ai_response);
    }

    if let Some(tb) = trajectory.as_deref_mut() {
        tb.push(TrajectoryStep::LlmCall {
            perspective: "single".to_owned(),
            input_tokens: u32::try_from(prepared.prompt_tokens_estimate.max(0)).unwrap_or(u32::MAX),
            output_tokens: 0,
            raw_output: None,
        });
    }

    let mut issues = parse_issues(&ai_response);
    // Snap each issue to its exact diff line before verification so the verify
    // pass and `difflore fix` see the precise location. Default off -> no-op.
    if prepared.settings.review_engine.hunk_line_resolution {
        let snippets = super::parse::extract_issue_snippets(&ai_response);
        apply_hunk_line_resolution(&mut issues, &snippets, &input.diff_content);
    }
    let issues = issues;
    if crate::infra::env::fix_debug() {
        eprintln!(
            "[fix-debug] single-pass raw_response_len={} parsed_issues={}",
            ai_response.len(),
            issues.len(),
        );
        if issues.is_empty() && ai_response.len() < 4000 {
            eprintln!("[fix-debug] response body: {ai_response}");
        }
    }

    let llm: Box<dyn ReviewLlm> = make_review_llm(engine);
    let pre_verify_count = issues.len();
    let issues = verify_pass_internal(
        llm.as_ref(),
        prepared.settings.review_engine.self_check_enabled && !input.fast_preview,
        &input.diff_content,
        issues,
    )
    .await;
    if crate::infra::env::fix_debug() {
        eprintln!(
            "[fix-debug] verify: pre={} post={} self_check_enabled={}",
            pre_verify_count,
            issues.len(),
            prepared.settings.review_engine.self_check_enabled && !input.fast_preview,
        );
    }

    Ok(finalize_review(
        llm.as_ref(),
        prepared,
        input.fast_preview,
        &input.diff_content,
        issues,
        pre_verify_count,
        1,
        trajectory,
    )
    .await)
}
