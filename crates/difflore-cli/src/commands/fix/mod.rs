// reason: CLI handlers exit on hard errors / after final output flush; refactoring shared
// code paths in the report builder would obscure intent.
#![allow(clippy::exit, clippy::branches_sharing_code)]

use std::io::{self, BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use difflore_core::models::DiffContentRecord;
use difflore_core::review::{
    DiffContextFile, DiffContextMode, DiffContextOptions, PackedDiffContext, ReviewCheckResult,
    ReviewIssueRecord, pack_diff_context,
};

use crate::commands::util::{diff_records_to_string, exit_err};
use crate::mcp_install;
use crate::runtime::CommandContext;
use crate::style::{self, sym};

mod apply;
mod attribution;
mod ci;
mod context;
mod errors;
mod modes;
mod path_safety;
mod pr;
mod preflight;
mod render;
mod scope;
mod scope_guardrail;

use apply::{
    AcceptedEditProof, ApplyOutcome, OutcomeIssue, apply_accepted_patches, record_fix_outcomes,
    yes_mode_should_fail,
};
use attribution::fetch_rule_source_repos;
use ci::{exit_after_output, finish_ci_mode};
use context::{FixContext, prepare_fix_context, primary_file_for_retrieval};
use errors::format_fix_err;
use modes::FixOutputMode;
use pr::print_pr_review_instructions;
use preflight::{
    REVIEW_TIMEOUT_SECS, preflight_provider_backend, review_id_for_provider_run,
    review_timeout_for_args,
};
use render::{
    emit_fix_json, render_agent_handoff_markdown, render_fix_report_markdown, write_fix_report,
};
use scope_guardrail::scope_guardrail_for_handoff;

// Confidence under which a patch is treated as "low confidence" — shown in
// dry-run, but in the interactive walkthrough the default key flips Y→N and
// `--yes` skips it. Matches the redesign contract (2026-04-27): "user can
// press Enter all the way through and end up with the safe outcome".
pub(crate) const CONFIDENCE_THRESHOLD: f32 = 0.80;
const PREVIEW_RECALL_DIAGNOSTIC_TIMEOUT: Duration = Duration::from_secs(5);
const FIX_EXIT_OUTBOX_DRAIN_MAX: usize = 16;
const FIX_RECALL_EMBEDDING_TIMEOUT: Duration = Duration::from_millis(2500);
const FIX_PR_DIFF_CONTEXT_CHAR_BUDGET: usize = 60_000;
const FIX_PR_DIFF_CONTEXT_ENV: &str = "DIFFLORE_FIX_PR_DIFF_CONTEXT_CHARS";

#[derive(Default)]
struct HandoffRuleRecall {
    ids: Vec<String>,
    titles: Vec<String>,
    note: Option<String>,
}

struct PreviewDiagnostic {
    kind: &'static str,
    message: String,
    budget_ms: Option<u64>,
    elapsed_ms: u64,
}

struct ReviewDiffContext {
    text: String,
    packed: Option<PackedDiffContext>,
}

// Machine-readable review status, surfaced in --json so a caller (CI, agent) can
// tell a *clean review* (provider ran, found nothing) apart from *no review at
// all* (no provider / provider error / timeout). Only `Reviewed` is a passing
// state. Everything else must not read as a clean pass.
const REVIEW_STATUS_REVIEWED: &str = "reviewed";
const REVIEW_STATUS_NOT_REVIEWED: &str = "not_reviewed";

impl PreviewDiagnostic {
    // Every preview diagnostic represents a path where DiffLore could not actually
    // review the diff (missing provider, provider error, or timeout), so the review
    // status is always "not_reviewed" and the exit code is non-success.
    const fn review_status() -> &'static str {
        REVIEW_STATUS_NOT_REVIEWED
    }
}

// Maps a fix `outcome` string to the machine-readable review status. The failure
// outcomes — no provider configured, provider error, or review timeout — mean the
// diff was never actually reviewed, so they must NOT read as a clean pass. Every
// other outcome (observed, no_changes, no_patches, applied, ...) means a review
// actually ran (or there was nothing to review), which is a passing state.
pub(crate) fn review_status_for_outcome(outcome: &str) -> &'static str {
    match outcome {
        "no_provider" | "provider_error" | "review_timeout" => REVIEW_STATUS_NOT_REVIEWED,
        _ => REVIEW_STATUS_REVIEWED,
    }
}

// Exit code used when fix --preview could not complete a real review. Non-zero so
// CI / scripts never mistake "could not review" for "reviewed clean".
const PREVIEW_NOT_REVIEWED_EXIT_CODE: i32 = 2;

macro_rules! fix_debug {
    ($($arg:tt)*) => {{
        if difflore_core::env::fix_debug() {
            eprintln!("[fix-debug] {}", format!($($arg)*));
        }
    }};
}
pub(crate) use fix_debug;

pub(crate) struct FixArgs {
    pub yes: bool,
    pub preview: bool,
    pub ci: bool,
    pub strict: bool,
    pub diff_scope: Option<String>,
    pub pr: Option<String>,
    pub repo: Option<String>,
    pub base: Option<String>,
    pub work_branch: Option<String>,
    pub no_checkout: bool,
    pub allow_dirty: bool,
    pub no_upload_acceptance: bool,
    pub explain_rules: bool,
    pub report: Option<String>,
    pub json: bool,
    pub path: Option<PathBuf>,
    pub agent: FixAgentMode,
}

impl From<crate::cli::FixCliArgs> for FixArgs {
    fn from(args: crate::cli::FixCliArgs) -> Self {
        Self {
            yes: args.yes,
            preview: args.preview,
            ci: args.ci,
            strict: args.strict,
            diff_scope: args.diff,
            explain_rules: args.explain_rules,
            report: args.report,
            json: args.json,
            pr: args.pr,
            repo: None,
            base: None,
            work_branch: args.work_branch,
            no_checkout: args.no_checkout,
            allow_dirty: args.allow_dirty,
            no_upload_acceptance: args.no_upload_acceptance,
            agent: FixAgentMode::Provider,
            path: args.path,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FixAgentMode {
    Provider,
    Handoff,
}

pub(crate) async fn handle_fix(cmd_ctx: &CommandContext, args: FixArgs) {
    let structured_output =
        args.report.is_some() || args.json || args.agent == FixAgentMode::Handoff;
    let mode = FixOutputMode::pick(&args, structured_output);

    let ctx = match prepare_fix_context(
        cmd_ctx,
        args.diff_scope.as_deref(),
        args.pr.as_deref(),
        args.repo.as_deref(),
        args.base.as_deref(),
        args.work_branch.as_deref(),
        args.no_checkout,
        args.allow_dirty,
        args.yes,
        args.preview,
        args.path.as_ref(),
    )
    .await
    {
        Ok(ctx) => ctx,
        Err(e) => exit_err(&format_fix_err("Fix failed", &format!("{e:#}"))),
    };
    let scope_label = ctx.diff_scope.label();

    if ctx.diff_records.is_empty() {
        return handle_empty_diff(&ctx, &args, scope_label, mode).await;
    }

    let review_diff = review_diff_context_for_fix(&ctx);
    let diff_text = review_diff.text;
    if let Some(packed) = review_diff.packed.as_ref() {
        fix_debug!(
            "pr_diff_context packed_chars={} original_chars={} included_files={} summaries={} budget={}",
            packed.packed_chars,
            packed.original_chars,
            packed.included_files.len(),
            packed.summaries.len(),
            packed
                .char_budget
                .map_or_else(|| "none".to_owned(), |budget| budget.to_string())
        );
    }
    fix_debug!("repo_scopes={:?}", ctx.repo_full_name_aliases);

    let primary_file = primary_file_for_retrieval(&ctx.diff_records);

    if args.agent == FixAgentMode::Provider
        && let Err(e) = preflight_provider_backend(&ctx.db, args.preview).await
    {
        let message = format_fix_err("Fix failed", &e);
        if args.preview {
            emit_preview_diagnostic(
                &ctx,
                &args,
                scope_label,
                &diff_text,
                primary_file.as_deref(),
                PreviewDiagnostic {
                    kind: "no_provider",
                    message,
                    budget_ms: None,
                    elapsed_ms: 0,
                },
            )
            .await;
            return;
        }
        exit_err(&message);
    }

    if mode == FixOutputMode::Handoff {
        let recalled = recall_rules_for_handoff(&ctx, &diff_text, primary_file.as_deref()).await;
        let attributions = fetch_rule_source_repos(&ctx.db, &recalled.ids).await;
        let suggestions: Vec<&ReviewIssueRecord> = Vec::new();
        let scope_guardrail = scope_guardrail_for_handoff(&ctx).await;
        let md = render_agent_handoff_markdown(
            scope_label,
            ctx.pr_fix.as_ref(),
            &ctx.path,
            &ctx.diff_records,
            scope_guardrail.as_deref(),
            recalled.note.as_deref(),
            &recalled.ids,
            &recalled.titles,
            &suggestions,
            &attributions,
        );
        let target = args.report.as_deref().unwrap_or("-");
        write_fix_report(target, &md, false);
        return;
    }

    let review_input = difflore_core::review::ReviewCheckInput {
        project_id: ctx.project_id.clone(),
        diff_content: diff_text.clone(),
        file_path: primary_file.clone(),
        engine: None,
        review_id: review_id_for_provider_run(ctx.review_id.as_deref(), args.preview),
        repo_full_name: ctx.repo_full_name.clone(),
        repo_full_name_aliases: ctx.repo_full_name_aliases.clone(),
        fast_preview: args.preview,
    };

    let review_timeout = review_timeout_for_args(&args);
    let review_started = Instant::now();
    let mut result = match tokio::time::timeout(
        review_timeout,
        difflore_core::review::run_review_smart(&ctx.db, review_input),
    )
    .await
    {
        Ok(Ok(r)) => r,
        Ok(Err(e)) if args.preview => {
            let elapsed_ms = elapsed_ms(review_started);
            emit_preview_diagnostic(
                &ctx,
                &args,
                scope_label,
                &diff_text,
                primary_file.as_deref(),
                PreviewDiagnostic {
                    kind: "provider_error",
                    message: format_fix_err("Fix pipeline failed", &e.to_string()),
                    budget_ms: Some(duration_ms(review_timeout)),
                    elapsed_ms,
                },
            )
            .await;
            return;
        }
        Ok(Err(e)) => exit_err(&format_fix_err("Fix pipeline failed", &e.to_string())),
        Err(_) if args.preview => {
            let review_timeout_secs = review_timeout.as_secs();
            emit_preview_diagnostic(
                &ctx,
                &args,
                scope_label,
                &diff_text,
                primary_file.as_deref(),
                PreviewDiagnostic {
                    kind: "review_timeout",
                    message: format!(
                        "fix preview stopped after {review_timeout_secs}s while waiting for the review provider. \
                         Set DIFFLORE_FIX_PREVIEW_REVIEW_TIMEOUT_SECS to a higher value for slow local providers."
                    ),
                    budget_ms: Some(duration_ms(review_timeout)),
                    elapsed_ms: duration_ms(review_timeout),
                },
            )
            .await;
            return;
        }
        Err(_) => exit_err(&format!(
            "fix pipeline timed out after {REVIEW_TIMEOUT_SECS}s while waiting for the review provider. \
             Run `difflore doctor` to check the active provider, then retry."
        )),
    };
    fix_debug!(
        "review_provider elapsed={}ms budget={}ms preview={}",
        elapsed_ms(review_started),
        duration_ms(review_timeout),
        args.preview,
    );

    fix_debug!(
        "matched_rules={} issues_total={} ids={:?} titles={:?}",
        result.matched_rules,
        result.issues.len(),
        result.matched_rule_ids,
        result.matched_rule_titles,
    );
    for (i, issue) in result.issues.iter().enumerate() {
        fix_debug!(
            "issue[{i}] file={:?} line={:?} conf={} has_suggestion={} rule={:?}",
            issue.file,
            issue.line,
            issue.confidence,
            issue
                .suggestion
                .as_deref()
                .is_some_and(|s| !s.trim().is_empty()),
            issue.rule,
        );
    }

    if fix_result_needs_rule_recall_supplement(&result) {
        let recalled = if skip_recall_supplement_for_args(&args) {
            HandoffRuleRecall::default()
        } else if args.preview {
            recall_rules_for_preview_diagnostic(&ctx, &diff_text, primary_file.as_deref()).await
        } else {
            recall_rules_for_handoff(&ctx, &diff_text, primary_file.as_deref()).await
        };
        supplement_fix_result_with_recalled_rules(&mut result, &recalled);
    }

    let suggestions: Vec<&ReviewIssueRecord> = result
        .issues
        .iter()
        .filter(|issue| {
            issue
                .suggestion
                .as_deref()
                .is_some_and(|s| !s.trim().is_empty())
        })
        .collect();

    let attributions = fetch_rule_source_repos(&ctx.db, &result.matched_rule_ids).await;

    match mode {
        FixOutputMode::Handoff => {
            let scope_guardrail = scope_guardrail_for_handoff(&ctx).await;
            let md = render_agent_handoff_markdown(
                scope_label,
                ctx.pr_fix.as_ref(),
                &ctx.path,
                &ctx.diff_records,
                scope_guardrail.as_deref(),
                None,
                &result.matched_rule_ids,
                &result.matched_rule_titles,
                &suggestions,
                &attributions,
            );
            let target = args.report.as_deref().unwrap_or("-");
            write_fix_report(target, &md, false);
        }
        FixOutputMode::Structured => {
            if args.json {
                emit_fix_json(
                    scope_label,
                    &result.matched_rule_ids,
                    &result.matched_rule_titles,
                    &suggestions,
                    &attributions,
                    "observed",
                );
            }
            if let Some(report_target) = args.report.as_deref() {
                let md = render_fix_report_markdown(
                    scope_label,
                    &result.matched_rule_ids,
                    &result.matched_rule_titles,
                    &suggestions,
                    &attributions,
                    "observed",
                );
                write_fix_report(report_target, &md, args.json);
            }
            if args.ci {
                flush_fix_outbox_before_exit(&ctx.db).await;
                finish_ci_mode(&suggestions, args.strict, scope_label);
            }
        }
        FixOutputMode::Preview => {
            run_preview_mode(
                &suggestions,
                scope_label,
                result.matched_rules,
                &result.matched_rule_ids,
                &result.matched_rule_titles,
                &attributions,
                args.explain_rules,
            );
        }
        FixOutputMode::Ci => {
            flush_fix_outbox_before_exit(&ctx.db).await;
            finish_ci_mode(&suggestions, args.strict, scope_label);
        }
        FixOutputMode::Yes => {
            if suggestions.is_empty() {
                if args.json {
                    emit_fix_json(
                        scope_label,
                        &result.matched_rule_ids,
                        &result.matched_rule_titles,
                        &suggestions,
                        &attributions,
                        "no_patches",
                    );
                } else {
                    println!(
                        "{} no patches suggested in {scope_label} ({} rule{} considered).",
                        style::ok(sym::OK),
                        result.matched_rules,
                        if result.matched_rules == 1 { "" } else { "s" },
                    );
                    println!();
                    println!("next: {}", style::cmd("difflore status"));
                }
                return;
            }
            let yes_outcome = run_yes_mode(
                &ctx.db,
                &ctx.path,
                &suggestions,
                ctx.repo_full_name.as_deref(),
                ctx.pr_fix.as_ref().map(|pr| pr.pr_number),
                YesModeFlags {
                    sync_staged_index: ctx.diff_scope.should_sync_index_after_apply(),
                    upload_acceptance: !args.no_upload_acceptance,
                    emit_human: !args.json,
                },
            )
            .await;
            if args.json {
                emit_fix_yes_json(
                    scope_label,
                    &result.matched_rule_ids,
                    &result.matched_rule_titles,
                    &suggestions,
                    &attributions,
                    &yes_outcome,
                );
            }
            if !args.json
                && let Some(pr) = ctx.pr_fix.as_ref()
            {
                print_pr_review_instructions(pr);
            }
            if yes_mode_should_fail(&yes_outcome.outcome) {
                flush_fix_outbox_before_exit(&ctx.db).await;
                exit_after_output(1);
            }
        }
        FixOutputMode::Pipe => {
            // Piped / scripted without `--yes`: dump suggestions as text. Style helpers
            // no-op outside a TTY so this is ANSI-clean for jq / less / CI logs.
            print_pipe_format(&suggestions);
        }
        FixOutputMode::Interactive => {
            run_interactive(
                &ctx.db,
                &ctx.path,
                &suggestions,
                ctx.repo_full_name.as_deref(),
                ctx.pr_fix.as_ref().map(|pr| pr.pr_number),
                ctx.diff_scope.should_sync_index_after_apply(),
                !args.no_upload_acceptance,
            )
            .await;
            if let Some(pr) = ctx.pr_fix.as_ref() {
                print_pr_review_instructions(pr);
            }
        }
    }
}

fn review_diff_context_for_fix(ctx: &FixContext) -> ReviewDiffContext {
    let full_text = diff_records_to_string(&ctx.diff_records);
    if !matches!(ctx.diff_scope, scope::DiffScope::PullRequest { .. }) {
        return ReviewDiffContext {
            text: full_text,
            packed: None,
        };
    }

    let patches = diff_record_patches(&ctx.diff_records);
    let files: Vec<DiffContextFile<'_>> = ctx
        .diff_records
        .iter()
        .zip(patches.iter())
        .map(|(record, patch)| DiffContextFile::new(record.file_path.as_str(), patch.as_str()))
        .collect();
    let packed = pack_diff_context(
        &files,
        DiffContextOptions {
            char_budget: Some(fix_pr_diff_context_char_budget()),
            mode: DiffContextMode::FixPr,
        },
    );

    let packed_text = render_packed_review_diff_context(&packed);
    if packed_text.trim().is_empty() {
        return ReviewDiffContext {
            text: full_text,
            packed: None,
        };
    }

    ReviewDiffContext {
        text: packed_text,
        packed: Some(packed),
    }
}

fn diff_record_patches(records: &[DiffContentRecord]) -> Vec<String> {
    records.iter().map(diff_record_patch).collect()
}

fn diff_record_patch(record: &DiffContentRecord) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "--- a/{}\n+++ b/{}\n",
        record.file_path, record.file_path
    ));
    for hunk in &record.hunks {
        out.push_str(&hunk.header);
        out.push('\n');
        out.push_str(&hunk.body);
    }
    out
}

fn render_packed_review_diff_context(packed: &PackedDiffContext) -> String {
    let mut out = String::new();
    out.push_str("## Packed PR Diff Context\n\n");
    out.push_str("DiffLore selected the most review-relevant PR diff content within the configured context budget. ");
    out.push_str(
        "Files listed in the summary were deleted, empty, truncated, or deferred for budget.\n\n",
    );
    out.push_str(&packed.text);

    if !packed.summaries.is_empty() {
        out.push_str("\n\n## Diff Context Summary\n\n");
        for summary in &packed.summaries {
            out.push_str("- ");
            out.push_str(&summary.summary);
            out.push('\n');
        }
    }

    out
}

fn fix_pr_diff_context_char_budget() -> usize {
    fix_pr_diff_context_char_budget_from_env(|key| std::env::var(key).ok())
}

fn fix_pr_diff_context_char_budget_from_env(env_var: impl Fn(&str) -> Option<String>) -> usize {
    env_var(FIX_PR_DIFF_CONTEXT_ENV)
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .filter(|budget| *budget >= 4_000)
        .unwrap_or(FIX_PR_DIFF_CONTEXT_CHAR_BUDGET)
}

async fn recall_rules_for_handoff(
    ctx: &FixContext,
    diff_text: &str,
    primary_file: Option<&str>,
) -> HandoffRuleRecall {
    use difflore_core::context::{index_db, retrieval, rule_source};

    let index_pool = match index_db::get_pool_for_cwd().await {
        Ok(pool) => pool,
        Err(e) => return handoff_rule_recall_failed("open rule index", e),
    };
    if let Err(e) =
        difflore_core::context::orchestrator::ensure_rules_indexed_with_embedding_timeout(
            &ctx.db,
            &index_pool,
            Some(FIX_RECALL_EMBEDDING_TIMEOUT),
        )
        .await
    {
        return handoff_rule_recall_failed("refresh rule index", e);
    }

    let retrieval_intent =
        difflore_core::context::intent_filter::build_review_intent_text(primary_file, diff_text);
    let query = if retrieval_intent.trim().is_empty() {
        match primary_file {
            Some(file) => format!("{file}\n{diff_text}"),
            None => diff_text.to_owned(),
        }
    } else {
        retrieval_intent
    };
    let ranking_inputs = rule_source::load_rule_ranking_inputs(&ctx.db).await;
    let mut repo_scopes = ctx.repo_full_name_aliases.clone();
    if repo_scopes.is_empty()
        && let Some(repo) = ctx.repo_full_name.clone()
    {
        repo_scopes.push(repo);
    }
    let scored = match retrieval::retrieve_rules_for_search(
        &index_pool,
        retrieval::RuleSearchRetrievalOptions {
            query: &query,
            lexical_query: &query,
            top_k: 5,
            confidence_map: ranking_inputs.confidence_map.as_ref(),
            age_days_map: ranking_inputs.age_days_map.as_ref(),
            target_file: primary_file,
            repo_scopes: repo_scopes.as_slice(),
            ann_enabled: true,
            embedding_timeout: Some(FIX_RECALL_EMBEDDING_TIMEOUT),
            // Scoped to interactive `recall` for now; `fix` keeps its existing
            // fast-degrade behaviour.
            cold_start_retry: false,
            adaptive_prune: false,
        },
    )
    .await
    {
        Ok(scored) => scored,
        Err(e) => return handoff_rule_recall_failed("retrieve relevant review rules", e),
    };

    let ids: Vec<String> = scored.iter().map(|rule| rule.skill_id.clone()).collect();
    let titles = scored
        .iter()
        .map(|rule| rule_title_from_content(&rule.content, &rule.skill_id))
        .collect();
    HandoffRuleRecall {
        ids,
        titles,
        note: None,
    }
}

fn handoff_rule_recall_failed(stage: &str, error: impl std::fmt::Display) -> HandoffRuleRecall {
    HandoffRuleRecall {
        ids: Vec::new(),
        titles: Vec::new(),
        note: Some(format!(
            "Rule memory retrieval could not complete while trying to {stage}: {error}. Treat this as unavailable recall evidence, not as proof that no rule matched."
        )),
    }
}

fn rule_title_from_content(content: &str, fallback: &str) -> String {
    content
        .lines()
        .find_map(|line| line.strip_prefix("Rule Name:").map(str::trim))
        .filter(|title| !title.is_empty())
        .unwrap_or(fallback)
        .to_owned()
}

fn fix_result_needs_rule_recall_supplement(result: &ReviewCheckResult) -> bool {
    result.matched_rule_ids.is_empty()
        || result.issues.iter().any(|issue| {
            issue
                .rule_id
                .as_deref()
                .is_none_or(|rule_id| rule_id.trim().is_empty())
        })
}

const fn skip_recall_supplement_for_args(args: &FixArgs) -> bool {
    args.preview && args.json
}

fn supplement_fix_result_with_recalled_rules(
    result: &mut ReviewCheckResult,
    recalled: &HandoffRuleRecall,
) {
    if recalled.ids.is_empty() {
        return;
    }

    for (idx, id) in recalled.ids.iter().enumerate() {
        if result
            .matched_rule_ids
            .iter()
            .any(|existing| existing == id)
        {
            continue;
        }
        result.matched_rule_ids.push(id.clone());
        result.matched_rule_titles.push(
            recalled
                .titles
                .get(idx)
                .cloned()
                .unwrap_or_else(|| id.clone()),
        );
    }
    result.matched_rules = i32::try_from(result.matched_rule_ids.len()).unwrap_or(i32::MAX);
    backfill_missing_issue_rule_ids(
        &mut result.issues,
        &result.matched_rule_ids,
        &result.matched_rule_titles,
    );
}

fn backfill_missing_issue_rule_ids(
    issues: &mut [ReviewIssueRecord],
    matched_rule_ids: &[String],
    matched_rule_titles: &[String],
) {
    if matched_rule_ids.is_empty() {
        return;
    }
    // A single recalled rule is already the disambiguation signal; multi-rule
    // recalls still require title/message overlap below.
    let single_recalled_rule_fallback =
        matched_rule_ids.len() == 1 && issues.iter().all(issue_rule_id_is_missing);
    for issue in issues {
        if !issue_rule_id_is_missing(issue) {
            continue;
        }
        let idx = if single_recalled_rule_fallback {
            Some(0)
        } else {
            best_recalled_rule_idx_for_issue(issue, matched_rule_titles)
        };
        let Some(idx) = idx else {
            continue;
        };
        if let Some(rule_id) = matched_rule_ids
            .get(idx)
            .filter(|rule_id| !rule_id.trim().is_empty())
        {
            issue.rule_id = Some(rule_id.clone());
        }
    }
}

fn issue_rule_id_is_missing(issue: &ReviewIssueRecord) -> bool {
    issue
        .rule_id
        .as_deref()
        .is_none_or(|rule_id| rule_id.trim().is_empty())
}

fn best_recalled_rule_idx_for_issue(
    issue: &ReviewIssueRecord,
    matched_rule_titles: &[String],
) -> Option<usize> {
    if matched_rule_titles.is_empty() {
        return None;
    }
    if matched_rule_titles.len() == 1 {
        return recalled_rule_title_has_overlap(issue, &matched_rule_titles[0]).then_some(0);
    }

    let issue_tokens = attribution_tokens_for_fix(&format!(
        "{} {} {}",
        issue.rule,
        issue.message,
        issue.suggestion.as_deref().unwrap_or_default(),
    ));
    let mut best: Option<(usize, usize)> = None;
    for (idx, title) in matched_rule_titles.iter().enumerate() {
        let title_tokens = attribution_tokens_for_fix(title);
        let overlap = title_tokens
            .iter()
            .filter(|token| issue_tokens.contains(*token))
            .count();
        if overlap < 2 {
            continue;
        }
        match best {
            Some((_, best_overlap)) if overlap <= best_overlap => {}
            _ => best = Some((idx, overlap)),
        }
    }
    best.map(|(idx, _)| idx)
}

fn recalled_rule_title_has_overlap(issue: &ReviewIssueRecord, title: &str) -> bool {
    let issue_tokens = attribution_tokens_for_fix(&format!(
        "{} {} {}",
        issue.rule,
        issue.message,
        issue.suggestion.as_deref().unwrap_or_default(),
    ));
    attribution_tokens_for_fix(title)
        .iter()
        .any(|token| issue_tokens.contains(token))
}

fn attribution_tokens_for_fix(text: &str) -> std::collections::BTreeSet<String> {
    const STOPWORDS: &[&str] = &[
        "the", "and", "for", "from", "into", "with", "this", "that", "must", "should", "would",
        "could", "rule", "rules", "file", "line", "review", "code", "when", "where", "than",
        "then", "they", "them", "your", "their", "use", "uses", "using",
    ];
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .filter_map(|raw| {
            let token = raw.trim().to_ascii_lowercase();
            if token.is_empty() || token.len() < 3 || STOPWORDS.contains(&token.as_str()) {
                None
            } else {
                Some(token)
            }
        })
        .collect()
}

fn print_preview_failure(message: &str) {
    let message = message
        .trim()
        .strip_prefix("Fix pipeline failed: ")
        .unwrap_or(message.trim());
    if message.starts_with("fix needs ") {
        println!("{} {message}", style::warn(sym::WARN));
    } else {
        println!(
            "{} preview could not complete: {message}",
            style::warn(sym::WARN)
        );
    }
}

fn duration_ms(duration: Duration) -> u64 {
    duration.as_secs().saturating_mul(1000) + u64::from(duration.subsec_millis())
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

async fn recall_rules_for_preview_diagnostic(
    ctx: &FixContext,
    diff_text: &str,
    primary_file: Option<&str>,
) -> HandoffRuleRecall {
    match tokio::time::timeout(
        PREVIEW_RECALL_DIAGNOSTIC_TIMEOUT,
        recall_rules_for_handoff(ctx, diff_text, primary_file),
    )
    .await
    {
        Ok(recalled) => recalled,
        Err(_) => HandoffRuleRecall {
            ids: Vec::new(),
            titles: Vec::new(),
            note: Some(format!(
                "Rule memory retrieval did not finish within {}ms; this is a preview diagnostic, not proof that no memory matched.",
                duration_ms(PREVIEW_RECALL_DIAGNOSTIC_TIMEOUT)
            )),
        },
    }
}

async fn emit_preview_diagnostic(
    ctx: &FixContext,
    args: &FixArgs,
    scope_label: &str,
    diff_text: &str,
    primary_file: Option<&str>,
    diagnostic: PreviewDiagnostic,
) {
    let recalled = recall_rules_for_preview_diagnostic(ctx, diff_text, primary_file).await;
    let attributions = fetch_rule_source_repos(&ctx.db, &recalled.ids).await;
    if args.json {
        emit_preview_diagnostic_json(scope_label, &recalled, &attributions, &diagnostic);
    } else {
        print_preview_diagnostic(scope_label, &recalled, &attributions, &diagnostic);
    }
    if let Some(report_target) = args.report.as_deref() {
        let suggestions: Vec<&ReviewIssueRecord> = Vec::new();
        let md = render_fix_report_markdown(
            scope_label,
            &recalled.ids,
            &recalled.titles,
            &suggestions,
            &attributions,
            diagnostic.kind,
        );
        write_fix_report(report_target, &md, args.json);
    }
    // The review never produced a verdict (no provider / provider error / timeout),
    // so exit non-success: a stranger (or CI) must not read this as a clean pass.
    flush_fix_outbox_before_exit(&ctx.db).await;
    exit_after_output(PREVIEW_NOT_REVIEWED_EXIT_CODE);
}

fn preview_diagnostic_json_value(
    scope_label: &str,
    recalled: &HandoffRuleRecall,
    attributions: &std::collections::HashMap<String, String>,
    diagnostic: &PreviewDiagnostic,
) -> serde_json::Value {
    let recalled_provenance: Vec<serde_json::Value> = recalled
        .ids
        .iter()
        .zip(recalled.titles.iter())
        .map(|(id, title)| {
            serde_json::json!({
                "id": id,
                "title": title,
                "sourceRepo": attributions.get(id),
            })
        })
        .collect();
    serde_json::json!({
        "mode": "preview",
        "scope": scope_label,
        "recalledRuleIds": recalled.ids,
        "recalledRuleTitles": recalled.titles,
        "recalled": recalled_provenance,
        "findings": [],
        "outcome": diagnostic.kind,
        // Machine-readable: "not_reviewed" means DiffLore could not review the diff,
        // so this is NOT a clean pass even though there are zero findings.
        "status": PreviewDiagnostic::review_status(),
        "diagnostic": {
            "kind": diagnostic.kind,
            "message": diagnostic.message,
            "budgetMs": diagnostic.budget_ms,
            "elapsedMs": diagnostic.elapsed_ms,
            "recallNote": recalled.note,
        },
    })
}

fn emit_preview_diagnostic_json(
    scope_label: &str,
    recalled: &HandoffRuleRecall,
    attributions: &std::collections::HashMap<String, String>,
    diagnostic: &PreviewDiagnostic,
) {
    let payload = preview_diagnostic_json_value(scope_label, recalled, attributions, diagnostic);
    println!("{}", crate::commands::util::json_compact_or(&payload, "{}"));
}

fn print_preview_diagnostic(
    scope_label: &str,
    recalled: &HandoffRuleRecall,
    attributions: &std::collections::HashMap<String, String>,
    diagnostic: &PreviewDiagnostic,
) {
    print_preview_failure(&diagnostic.message);
    println!(
        "{} Scope: {}",
        style::pewter(sym::BULLET),
        style::ident(scope_label)
    );
    if !recalled.titles.is_empty() {
        println!();
        println!(
            "  {}",
            style::pewter("Recalled memories available before patching:")
        );
        for (i, title) in recalled.titles.iter().take(3).enumerate() {
            let attribution_suffix = recalled
                .ids
                .get(i)
                .and_then(|id| attributions.get(id))
                .map(|repo| format!("  {}", style::pewter(&format!("← learned from {repo}"))))
                .unwrap_or_default();
            println!(
                "    {} {title}{attribution_suffix}",
                style::pewter(sym::BULLET),
            );
        }
    } else if let Some(note) = recalled.note.as_deref() {
        println!("  {} {note}", style::pewter(sym::BULLET));
    }
    println!();
    println!(
        "next: {}  {}",
        style::cmd("difflore recall --diff"),
        style::pewter("# inspect memory without calling the fix provider"),
    );
}

async fn handle_empty_diff(
    ctx: &FixContext,
    args: &FixArgs,
    scope_label: &str,
    mode: FixOutputMode,
) {
    let _ = &ctx.target_file;
    let empty_suggestions: Vec<&ReviewIssueRecord> = Vec::new();
    let empty_attributions: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    if mode == FixOutputMode::Handoff {
        let md = render_agent_handoff_markdown(
            scope_label,
            ctx.pr_fix.as_ref(),
            &ctx.path,
            &ctx.diff_records,
            None,
            None,
            &[],
            &[],
            &empty_suggestions,
            &empty_attributions,
        );
        let target = args.report.as_deref().unwrap_or("-");
        write_fix_report(target, &md, false);
        return;
    }
    if mode == FixOutputMode::Structured {
        if args.json {
            emit_fix_json(
                scope_label,
                &[],
                &[],
                &empty_suggestions,
                &empty_attributions,
                "no_changes",
            );
        }
        if let Some(report_target) = args.report.as_deref() {
            let md = render_fix_report_markdown(
                scope_label,
                &[],
                &[],
                &empty_suggestions,
                &empty_attributions,
                "no_changes",
            );
            write_fix_report(report_target, &md, args.json);
        }
        if args.ci {
            flush_fix_outbox_before_exit(&ctx.db).await;
            finish_ci_mode(&empty_suggestions, args.strict, scope_label);
        }
        return;
    }
    if args.ci {
        eprintln!(
            "{} no changed files to check in {scope_label}.",
            style::ok(sym::OK),
        );
        return;
    }
    if args.preview {
        println!(
            "{} no changed files to preview in {scope_label}.",
            style::ok(sym::OK),
        );
        println!();
        // `recall --diff` would error the same way (also reads git diff);
        // suggest a non-diff path instead.
        println!(
            "next: {}",
            style::cmd("difflore recall \"<intent phrase>\""),
        );
        return;
    }
    if io::stdout().is_terminal() {
        println!("{} Nothing to fix in {scope_label}.", style::ok(sym::OK));
        print_empty_state_hint(&ctx.db).await;
        mcp_install::maybe_print_mcp_hint().await;
    }
}

// Decisions are batched: nothing applies until the walkthrough completes (or
// quit), so partial-tree state can't leak mid-walk.
async fn run_interactive(
    db: &difflore_core::SqlitePool,
    repo_root: &std::path::Path,
    suggestions: &[&ReviewIssueRecord],
    repo_full_name: Option<&str>,
    pr_number: Option<u64>,
    sync_staged_index: bool,
    upload_acceptance: bool,
) {
    println!();
    println!(
        "{} Found {} suggestion{} in your changes.",
        style::ok(sym::OK),
        suggestions.len(),
        if suggestions.len() == 1 { "" } else { "s" },
    );
    println!();

    let mut accepted: Vec<&ReviewIssueRecord> = Vec::new();
    let mut skipped: Vec<&ReviewIssueRecord> = Vec::new();
    let mut auto_rest = false;
    let total = suggestions.len();

    let stdin = io::stdin();
    let mut input = String::new();

    'walk: for (i, issue) in suggestions.iter().enumerate() {
        let confident = issue.confidence >= CONFIDENCE_THRESHOLD;
        print_patch_card(i + 1, total, issue);

        // Auto-apply tail (after `a`): only auto-accept confident; low-confidence still asks.
        if auto_rest && confident {
            println!("  {} auto-accepted (rest)", style::ok(sym::OK));
            accepted.push(issue);
            continue;
        }

        let default_label = if confident {
            "[Y/n/a/q/?]"
        } else {
            "[y/N/a/q/?]"
        };
        loop {
            print!("  Apply? {default_label} > ");
            io::stdout().flush().ok();
            input.clear();
            if stdin.lock().read_line(&mut input).is_err() {
                break 'walk;
            }
            let key = input.trim().to_lowercase();
            let decision = if key.is_empty() {
                if confident { "y" } else { "n" }
            } else {
                key.as_str()
            };
            match decision {
                "y" => {
                    accepted.push(issue);
                    break;
                }
                "n" => {
                    skipped.push(issue);
                    break;
                }
                "a" => {
                    auto_rest = true;
                    if confident {
                        accepted.push(issue);
                    } else {
                        skipped.push(issue);
                    }
                    break;
                }
                "q" => break 'walk,
                "?" => {
                    print_explain(issue);
                    print_patch_card(i + 1, total, issue);
                }
                _ => {
                    println!(
                        "  {} unknown key '{decision}'. Enter for default, or one of y/n/a/q/?.",
                        style::warn(sym::WARN)
                    );
                }
            }
        }
        println!();
    }

    let mut outcome =
        apply_accepted_patches(db, repo_root, &accepted, sync_staged_index, false).await;
    record_fix_outcomes(
        db,
        &mut outcome,
        &skipped,
        repo_full_name,
        pr_number,
        upload_acceptance,
    )
    .await;
    apply::print_apply_summary(
        &outcome,
        u32::try_from(skipped.len()).unwrap_or(u32::MAX),
        total,
    );
}

struct YesModeOutcome {
    outcome: ApplyOutcome,
    held_back: Vec<OutcomeIssue>,
    accepted_edit_proofs: Vec<AcceptedEditProof>,
}

struct YesModeFlags {
    sync_staged_index: bool,
    upload_acceptance: bool,
    emit_human: bool,
}

async fn run_yes_mode(
    db: &difflore_core::SqlitePool,
    repo_root: &std::path::Path,
    suggestions: &[&ReviewIssueRecord],
    repo_full_name: Option<&str>,
    pr_number: Option<u64>,
    flags: YesModeFlags,
) -> YesModeOutcome {
    let YesModeFlags {
        sync_staged_index,
        upload_acceptance,
        emit_human,
    } = flags;
    let confident: Vec<&ReviewIssueRecord> = suggestions
        .iter()
        .copied()
        .filter(|s| s.confidence >= CONFIDENCE_THRESHOLD)
        .collect();
    let held_back: Vec<&ReviewIssueRecord> = suggestions
        .iter()
        .copied()
        .filter(|s| s.confidence < CONFIDENCE_THRESHOLD)
        .collect();

    if emit_human {
        println!();
        println!(
            "{} applying {} confident patch(es){}",
            style::ok(sym::OK),
            confident.len(),
            if held_back.is_empty() {
                ".".to_owned()
            } else {
                format!(", {} held back as low-confidence.", held_back.len())
            },
        );
    }

    let mut outcome =
        apply_accepted_patches(db, repo_root, &confident, sync_staged_index, !emit_human).await;
    let accepted_edit_proofs = outcome.accepted_edits.clone();
    record_fix_outcomes(
        db,
        &mut outcome,
        &held_back,
        repo_full_name,
        pr_number,
        upload_acceptance,
    )
    .await;
    let held_back = held_back
        .iter()
        .copied()
        .map(OutcomeIssue::from)
        .collect::<Vec<_>>();

    if emit_human && !held_back.is_empty() {
        println!();
        println!(
            "  {} held back (--yes won't auto-apply low-confidence):",
            style::pewter(sym::BULLET),
        );
        for issue in &held_back {
            println!("      {}  ⌕ {}", issue.file_loc, issue.rule_label());
        }
        println!(
            "      → review interactively: {}",
            style::cmd("difflore fix")
        );
    }

    if emit_human {
        apply::print_apply_summary(
            &outcome,
            u32::try_from(held_back.len()).unwrap_or(u32::MAX),
            suggestions.len(),
        );
    }

    YesModeOutcome {
        outcome,
        held_back,
        accepted_edit_proofs,
    }
}

fn outcome_issue_json(issue: &OutcomeIssue) -> serde_json::Value {
    serde_json::json!({
        "id": issue.rule_id,
        "rule": issue.rule_name,
        "file": issue.file_path,
        "location": issue.file_loc,
    })
}

const fn yes_outcome_label(outcome: &ApplyOutcome, held_back: &[OutcomeIssue]) -> &'static str {
    if !outcome.failed.is_empty() {
        "failed"
    } else if !outcome.applied.is_empty() {
        "applied"
    } else if !held_back.is_empty() {
        "held_back"
    } else {
        "no_patches"
    }
}

fn emit_fix_yes_json(
    scope_label: &str,
    matched_rule_ids: &[String],
    matched_rule_titles: &[String],
    suggestions: &[&ReviewIssueRecord],
    attributions: &std::collections::HashMap<String, String>,
    yes_outcome: &YesModeOutcome,
) {
    let findings: Vec<serde_json::Value> = suggestions
        .iter()
        .map(|issue| {
            let source_repo = issue
                .rule_id
                .as_deref()
                .and_then(|id| attributions.get(id))
                .cloned();
            serde_json::json!({
                "id": issue.rule_id,
                "rule": issue.rule,
                "file": issue.file,
                "line": issue.line,
                "confidence": issue.confidence,
                "summary": issue.message,
                "diff": issue.suggestion,
                "sourceRepo": source_repo,
            })
        })
        .collect();
    let recalled_provenance: Vec<serde_json::Value> = matched_rule_ids
        .iter()
        .zip(matched_rule_titles.iter())
        .map(|(id, title)| {
            serde_json::json!({
                "id": id,
                "title": title,
                "sourceRepo": attributions.get(id),
            })
        })
        .collect();
    let failed: Vec<serde_json::Value> = yes_outcome
        .outcome
        .failed
        .iter()
        .map(|(issue, reason)| {
            let mut value = outcome_issue_json(issue);
            if let serde_json::Value::Object(map) = &mut value {
                map.insert(
                    "reason".to_owned(),
                    serde_json::Value::String(reason.clone()),
                );
            }
            value
        })
        .collect();
    let accepted_edit_proofs: Vec<serde_json::Value> = yes_outcome
        .accepted_edit_proofs
        .iter()
        .map(|proof| {
            serde_json::json!({
                "file": proof.file_path,
                "language": proof.language,
                "diffSignature": proof.diff_signature,
                "ruleIds": proof.rule_ids,
            })
        })
        .collect();
    let outcome = yes_outcome_label(&yes_outcome.outcome, &yes_outcome.held_back);
    let payload = serde_json::json!({
        "mode": "yes",
        "scope": scope_label,
        "recalledRuleIds": matched_rule_ids,
        "recalledRuleTitles": matched_rule_titles,
        "recalled": recalled_provenance,
        "findings": findings,
        "outcome": outcome,
        // A provider review actually ran in --yes mode; the outcome describes the
        // apply result, not whether a review happened, so status is always reviewed.
        "status": review_status_for_outcome(outcome),
        "apply": {
            "appliedCount": yes_outcome.outcome.applied.len(),
            "failedCount": yes_outcome.outcome.failed.len(),
            "skippedCount": yes_outcome.held_back.len(),
            "acceptedEditProofCount": yes_outcome.accepted_edit_proofs.len(),
            "applied": yes_outcome
                .outcome
                .applied
                .iter()
                .map(outcome_issue_json)
                .collect::<Vec<_>>(),
            "failed": failed,
            "skipped": yes_outcome
                .held_back
                .iter()
                .map(outcome_issue_json)
                .collect::<Vec<_>>(),
            "acceptedEditProofs": accepted_edit_proofs,
        },
    });
    println!("{}", crate::commands::util::json_compact_or(&payload, "{}"));
}

async fn flush_fix_outbox_before_exit(db: &difflore_core::SqlitePool) {
    let client = difflore_core::cloud::client::CloudClient::create().await;
    if !client.is_logged_in() {
        return;
    }
    let queue = difflore_core::cloud::outbox::OutboxQueue::new(db.clone());
    if let Err(e) =
        difflore_core::cloud::outbox::drain_outbox(&queue, &client, FIX_EXIT_OUTBOX_DRAIN_MAX).await
    {
        eprintln!(
            "{} local telemetry remains queued: {e}",
            style::warn(sym::WARN)
        );
    }
}

async fn print_empty_state_hint(db: &difflore_core::SqlitePool) {
    match difflore_core::skills::stats(db).await {
        Ok(stats) if stats.total == 0 => {
            println!(
                "  {} No team review memory in your local corpus yet.",
                style::pewter(sym::BULLET)
            );
            println!(
                "  → create local memories from PR history: {}",
                style::cmd("difflore import-reviews --max-prs 50")
            );
            println!(
                "  → preview recalled memory: {}",
                style::cmd("difflore recall --diff")
            );
            // Cloud-aware secondary hint: `--upload` and `sync` both need
            // an active session. Keep the always-available CLI path first so
            // OSS-only users still see a green path before the paid upgrade.
            let cloud_client = difflore_core::cloud::client::CloudClient::create().await;
            if cloud_client.is_logged_in() {
                println!(
                    "  → or use cloud extraction/governance: {}",
                    style::cmd("difflore import-reviews --max-prs 50 --upload")
                );
                println!(
                    "  → then pull extracted memory: {}",
                    style::cmd("difflore cloud sync")
                );
            } else {
                println!(
                    "  → optional cloud extraction/governance: {}",
                    style::cmd("difflore cloud login")
                );
            }
        }
        Ok(stats) => {
            println!(
                "  {} {} remembered rule{} ready for agents. Make a change, then run {} again.",
                style::pewter(sym::BULLET),
                stats.total,
                if stats.total == 1 { "" } else { "s" },
                style::cmd("difflore fix"),
            );
        }
        Err(_) => {
            println!(
                "  → teach agents from PR history: {}",
                style::cmd("difflore import-reviews --max-prs 50")
            );
        }
    }
}

// `--preview` order: Scope → Recalled memories (top 3) →
// Findings/patches → Next. `explain_rules` swaps the rule label on each finding
// for the rule_id form so users can re-derive the source memory.
fn run_preview_mode(
    suggestions: &[&ReviewIssueRecord],
    scope_label: &str,
    matched_rules: i32,
    matched_rule_ids: &[String],
    matched_rule_titles: &[String],
    attributions: &std::collections::HashMap<String, String>,
    explain_rules: bool,
) {
    println!(
        "{} Scope: {}",
        style::ok(sym::OK),
        style::ident(scope_label),
    );
    if !matched_rule_titles.is_empty() {
        println!();
        println!("  {}", style::pewter("Recalled memories (top 3):"));
        for (i, title) in matched_rule_titles.iter().take(3).enumerate() {
            let attribution_suffix = matched_rule_ids
                .get(i)
                .and_then(|id| attributions.get(id))
                .map(|repo| format!("  {}", style::pewter(&format!("← learned from {repo}"))))
                .unwrap_or_default();
            println!(
                "    {} {title}{attribution_suffix}",
                style::pewter(sym::BULLET),
            );
        }
    }
    println!();
    if suggestions.is_empty() {
        // 0 patches with N>0 recalled rules is the *good* outcome — frame it that
        // way so users don't assume the system is broken vs. a previous run.
        if matched_rules > 0 {
            println!(
                "{} {scope_label} looks clean against {} recalled memor{}. No patches suggested.",
                style::ok(sym::OK),
                matched_rules,
                if matched_rules == 1 { "y" } else { "ies" },
            );
            println!();
            // Forward-pointing bridge: a clean scope is the *good* outcome,
            // so route the user to evidence of accumulated value instead of
            // looping them back into another preview of the same diff.
            println!(
                "next: {}  {}",
                style::cmd("difflore status"),
                style::pewter("# see the local proof loop and next command"),
            );
        } else {
            // 0 rules + 0 patches: corpus/scope didn't match. Recall is the right
            // diagnostic, not another fix preview.
            println!(
                "{} no patches suggested in {scope_label} (0 rules matched the changed files).",
                style::ok(sym::OK),
            );
            println!();
            println!(
                "next: {}  {}",
                style::cmd("difflore recall --diff"),
                style::pewter("# see what memory agents would receive"),
            );
        }
        return;
    }
    let confident = suggestions
        .iter()
        .filter(|s| s.confidence >= CONFIDENCE_THRESHOLD)
        .count();
    let low = suggestions.len() - confident;
    println!(
        "{} {} suggestion{} in {scope_label} ({confident} confident, {low} low-confidence). Preview only; no files changed.",
        style::ok(sym::OK),
        suggestions.len(),
        if suggestions.len() == 1 { "" } else { "s" },
    );
    for issue in suggestions {
        let badge = if issue.confidence >= CONFIDENCE_THRESHOLD {
            style::ok(&format!("{}% \u{2713}", percent(issue.confidence)))
        } else {
            style::warn(&format!("{}% low", percent(issue.confidence)))
        };
        println!(
            "  {} {}  ·  {}  ·  {badge}",
            style::pewter(sym::BULLET),
            file_loc(issue),
            issue_rule_label(issue),
        );
        if explain_rules {
            let snippet: String = issue.message.chars().take(120).collect();
            let suffix = if issue.message.chars().count() > 120 {
                "…"
            } else {
                ""
            };
            if let Some(id) = issue.rule_id.as_deref() {
                println!(
                    "      {} {}  {} {}{}",
                    style::pewter("rule:"),
                    id,
                    style::pewter("why:"),
                    snippet,
                    suffix,
                );
            } else {
                println!("      {} {}{}", style::pewter("why:"), snippet, suffix,);
            }
        }
    }
    println!();
    println!(
        "next: {}  {}",
        style::cmd("difflore fix"),
        style::pewter("apply confident patches after preview"),
    );
}

fn print_pipe_format(suggestions: &[&ReviewIssueRecord]) {
    for (i, issue) in suggestions.iter().enumerate() {
        if i > 0 {
            println!();
        }
        let file_loc = issue.file.as_deref().map_or_else(
            || "<unknown>".into(),
            |f: &str| match issue.line {
                Some(l) => format!("{f}:{l}"),
                None => f.to_owned(),
            },
        );
        println!("--- {file_loc} ---");
        println!(
            "rule: {}  ({}% accept)",
            issue.rule,
            percent(issue.confidence)
        );
        println!();
        println!("{}", issue.message);
        if let Some(s) = &issue.suggestion {
            println!();
            println!("{}", s.trim());
        }
    }
}

fn print_patch_card(idx: usize, total: usize, issue: &ReviewIssueRecord) {
    let file_loc = issue.file.as_deref().map_or_else(
        || "<unknown>".into(),
        |f| match issue.line {
            Some(l) => format!("{f}:{l}"),
            None => f.to_owned(),
        },
    );
    let pct = percent(issue.confidence);
    let badge = if issue.confidence >= CONFIDENCE_THRESHOLD {
        style::ok(&format!("{pct}% \u{2713}"))
    } else {
        style::warn(&format!("{pct}% \u{26a0} low confidence"))
    };

    println!(
        "{}  [{idx}/{total}]  {file_loc}",
        style::pewter("\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}"),
    );
    println!(
        "       {} {}  ·  {badge}",
        style::pewter("\u{2315}"),
        style::title(&issue.rule),
    );
    println!();
    if !issue.message.trim().is_empty() {
        println!("  {}", issue.message.trim());
    }
    if let Some(s) = &issue.suggestion
        && !s.trim().is_empty()
    {
        // Cap to 12 lines so a noisy suggestion doesn't push the prompt off-screen.
        println!();
        for line in s.trim().lines().take(12) {
            println!("  {line}");
        }
        if s.lines().count() > 12 {
            println!(
                "  {} (truncated; press {} for full text)",
                style::pewter("\u{2026}"),
                style::cmd("?"),
            );
        }
    }
    println!();
}

fn print_explain(issue: &ReviewIssueRecord) {
    println!();
    println!("  {} {}", style::pewter("rule"), style::title(&issue.rule),);
    if let Some(id) = &issue.rule_id {
        println!(
            "  {} {}     {}",
            style::pewter("id  "),
            style::pewter(id),
            style::pewter("(inspect with: difflore status --json)"),
        );
    }
    if let Some(s) = &issue.suggestion {
        println!();
        for line in s.trim().lines() {
            println!("  {line}");
        }
    }
    println!();
}

pub(crate) fn issue_rule_label(issue: &ReviewIssueRecord) -> String {
    match &issue.rule_id {
        Some(id) if !id.trim().is_empty() => format!("{} ({id})", issue.rule),
        _ => issue.rule.clone(),
    }
}

pub(crate) fn file_loc(issue: &ReviewIssueRecord) -> String {
    issue.file.as_deref().map_or_else(
        || "<unknown>".into(),
        |f: &str| match issue.line {
            Some(l) => format!("{f}:{l}"),
            None => f.to_owned(),
        },
    )
}

pub(crate) fn percent(c: f32) -> u8 {
    (c.clamp(0.0, 1.0) * 100.0).round() as u8
}

#[cfg(test)]
mod tests {
    use super::preflight::{
        PREVIEW_REVIEW_TIMEOUT_SECS, no_provider_configured_message, preflight_decision,
        review_timeout_for_args_with_env,
    };
    use super::*;

    fn fix_args(preview: bool, json: bool) -> FixArgs {
        FixArgs {
            yes: false,
            preview,
            ci: false,
            strict: false,
            diff_scope: None,
            pr: None,
            repo: None,
            base: None,
            work_branch: None,
            no_checkout: false,
            allow_dirty: false,
            no_upload_acceptance: false,
            explain_rules: false,
            report: None,
            json,
            path: None,
            agent: FixAgentMode::Provider,
        }
    }

    fn review_issue(rule: &str, message: &str, suggestion: Option<&str>) -> ReviewIssueRecord {
        ReviewIssueRecord {
            severity: "warning".to_owned(),
            rule: rule.to_owned(),
            rule_id: None,
            message: message.to_owned(),
            file: Some("src/example.rs".to_owned()),
            line: Some(2),
            suggestion: suggestion.map(str::to_owned),
            source_badge: None,
            perspectives: Vec::new(),
            confidence: 0.98,
        }
    }

    fn diff_record(file_path: &str, body: &str) -> DiffContentRecord {
        DiffContentRecord {
            file_path: file_path.to_owned(),
            hunks: vec![difflore_core::models::DiffHunkRecord {
                header: "@@ -1,2 +1,2 @@".to_owned(),
                body: body.to_owned(),
            }],
        }
    }

    fn fix_context_for_diff(pr: bool, diff_records: Vec<DiffContentRecord>) -> FixContext {
        FixContext {
            db: sqlx::SqlitePool::connect_lazy("sqlite::memory:").unwrap(),
            path: PathBuf::from("."),
            project_id: "project".to_owned(),
            diff_records,
            diff_scope: if pr {
                scope::DiffScope::PullRequest {
                    label: "PR #42 (main...HEAD)".to_owned(),
                }
            } else {
                scope::DiffScope::Worktree
            },
            repo_full_name: pr.then(|| "acme/api".to_owned()),
            repo_full_name_aliases: Vec::new(),
            target_file: None,
            review_id: pr.then(|| "github-pr:acme/api#42".to_owned()),
            pr_fix: None,
        }
    }

    #[test]
    fn preview_json_keeps_structured_output_but_uses_preview_budget() {
        let args = fix_args(true, true);

        assert_eq!(FixOutputMode::pick(&args, true), FixOutputMode::Structured);
        assert_eq!(
            review_timeout_for_args_with_env(&args, |_| None),
            Duration::from_secs(PREVIEW_REVIEW_TIMEOUT_SECS)
        );
    }

    #[tokio::test]
    async fn non_pr_fix_uses_full_diff_text_without_packing() {
        let ctx = fix_context_for_diff(
            false,
            vec![diff_record("src/lib.rs", "-old\n+new\n context\n")],
        );

        let review_diff = review_diff_context_for_fix(&ctx);

        assert!(review_diff.packed.is_none());
        assert_eq!(review_diff.text, diff_records_to_string(&ctx.diff_records));
        assert!(!review_diff.text.contains("Packed PR Diff Context"));
    }

    #[tokio::test]
    async fn pr_fix_uses_packed_context_and_summarizes_omitted_files() {
        let large = format!(
            "{}\n+important_change\n-old_value\n",
            " context line\n".repeat(8_000)
        );
        let ctx = fix_context_for_diff(
            true,
            vec![
                diff_record("src/large.rs", &large),
                diff_record("src/small.rs", "-a\n+b\n"),
            ],
        );

        let review_diff = review_diff_context_for_fix(&ctx);

        let packed = review_diff.packed.as_ref().expect("packed PR context");
        assert!(review_diff.text.contains("## Packed PR Diff Context"));
        assert!(review_diff.text.contains("## Diff Context Summary"));
        assert!(review_diff.text.contains("src/large.rs"));
        assert!(packed.packed_chars <= fix_pr_diff_context_char_budget());
        assert!(packed.original_chars >= packed.packed_chars);
        assert!(!packed.summaries.is_empty());
    }

    #[test]
    fn invalid_pr_diff_context_budget_env_falls_back_to_default() {
        assert_eq!(
            fix_pr_diff_context_char_budget_from_env(|key| {
                (key == FIX_PR_DIFF_CONTEXT_ENV).then(|| "12".to_owned())
            }),
            FIX_PR_DIFF_CONTEXT_CHAR_BUDGET
        );
        assert_eq!(
            fix_pr_diff_context_char_budget_from_env(|key| {
                (key == FIX_PR_DIFF_CONTEXT_ENV).then(|| "9000".to_owned())
            }),
            9000
        );
    }

    #[test]
    fn preview_json_skips_second_recall_supplement() {
        assert!(skip_recall_supplement_for_args(&fix_args(true, true)));
        assert!(!skip_recall_supplement_for_args(&fix_args(true, false)));
        assert!(!skip_recall_supplement_for_args(&fix_args(false, true)));
    }

    #[test]
    fn preview_diagnostic_json_includes_budget_and_recalled_memory() {
        let recalled = HandoffRuleRecall {
            ids: vec!["rule-1".to_owned()],
            titles: vec!["Avoid slow preview providers".to_owned()],
            note: Some("recall stayed local".to_owned()),
        };
        let mut attributions = std::collections::HashMap::new();
        attributions.insert("rule-1".to_owned(), "acme/api".to_owned());
        let diagnostic = PreviewDiagnostic {
            kind: "review_timeout",
            message: "preview timed out".to_owned(),
            budget_ms: Some(15_000),
            elapsed_ms: 15_000,
        };

        let payload = preview_diagnostic_json_value(
            "PR #12 (main...feature)",
            &recalled,
            &attributions,
            &diagnostic,
        );

        assert_eq!(payload["mode"], "preview");
        assert_eq!(payload["outcome"], "review_timeout");
        // A timed-out review must NOT read as a clean pass even with zero findings.
        assert_eq!(payload["status"], "not_reviewed");
        assert_eq!(payload["diagnostic"]["budgetMs"], 15_000);
        assert_eq!(payload["recalled"][0]["sourceRepo"], "acme/api");
        assert_eq!(payload["findings"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn no_provider_preview_diagnostic_is_not_reviewed_with_actionable_message() {
        let recalled = HandoffRuleRecall::default();
        let attributions = std::collections::HashMap::new();
        let diagnostic = PreviewDiagnostic {
            kind: "no_provider",
            message: format_fix_err("Fix failed", &no_provider_configured_message()),
            budget_ms: None,
            elapsed_ms: 0,
        };

        // Status/outcome distinguish "could not review" from a clean pass.
        assert_eq!(PreviewDiagnostic::review_status(), "not_reviewed");

        let payload =
            preview_diagnostic_json_value("working tree", &recalled, &attributions, &diagnostic);
        assert_eq!(payload["outcome"], "no_provider");
        assert_eq!(payload["status"], "not_reviewed");
        assert_eq!(payload["findings"].as_array().unwrap().len(), 0);
        // The surfaced message must tell the reader how to fix it.
        let message = payload["diagnostic"]["message"].as_str().unwrap();
        assert!(
            message.contains("difflore providers setup"),
            "no-provider diagnostic should point at `difflore providers setup`, got: {message}"
        );
    }

    #[test]
    fn preview_no_provider_preflight_error_is_actionable_and_disclaims_cli_fallback() {
        // The preview rejection must read as not_reviewed once surfaced, point at
        // `difflore providers setup`, and make clear it will not silently use a
        // PATH agent CLI for the preview verdict.
        let raw = preflight_decision(false, Some("claude"), true)
            .expect_err("no-provider preview must be a preflight error");
        let surfaced = format_fix_err("Fix failed", &raw);
        assert!(
            surfaced.contains("difflore providers setup"),
            "preview no-provider error must point at setup, got: {surfaced}"
        );
        assert!(
            surfaced
                .to_ascii_lowercase()
                .contains("no ai provider configured"),
            "preview no-provider error must state no provider is configured, got: {surfaced}"
        );
        assert!(
            surfaced.contains("will not silently fall back"),
            "preview no-provider error must disclaim the CLI fallback, got: {surfaced}"
        );

        // End-to-end through the same JSON the live `--preview` path emits.
        let diagnostic = PreviewDiagnostic {
            kind: "no_provider",
            message: surfaced,
            budget_ms: None,
            elapsed_ms: 0,
        };
        assert_eq!(PreviewDiagnostic::review_status(), "not_reviewed");
        let payload = preview_diagnostic_json_value(
            "working tree",
            &HandoffRuleRecall::default(),
            &std::collections::HashMap::new(),
            &diagnostic,
        );
        assert_eq!(payload["outcome"], "no_provider");
        assert_eq!(payload["status"], "not_reviewed");
        assert_eq!(payload["findings"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn provider_failure_outcomes_are_not_reviewed_but_clean_review_is_reviewed() {
        // Launch-blocker outcomes: a review never actually produced a verdict.
        assert_eq!(review_status_for_outcome("no_provider"), "not_reviewed");
        assert_eq!(review_status_for_outcome("provider_error"), "not_reviewed");
        assert_eq!(review_status_for_outcome("review_timeout"), "not_reviewed");
        // A genuine clean review (provider ran, found nothing) stays a passing state,
        // as do the empty-diff and yes-mode apply outcomes.
        assert_eq!(review_status_for_outcome("observed"), "reviewed");
        assert_eq!(review_status_for_outcome("no_patches"), "reviewed");
        assert_eq!(review_status_for_outcome("no_changes"), "reviewed");
        assert_eq!(review_status_for_outcome("applied"), "reviewed");
    }

    #[test]
    fn not_reviewed_preview_uses_non_success_exit_code() {
        // The "could not review" exit code must be non-zero so CI never reads it
        // as a clean pass, and distinct from the blocking-findings code (1).
        assert_ne!(PREVIEW_NOT_REVIEWED_EXIT_CODE, 0);
        assert_ne!(PREVIEW_NOT_REVIEWED_EXIT_CODE, 1);
    }

    #[test]
    fn supplement_recall_backfills_object_is_issue_rule_id() {
        let mut result = ReviewCheckResult {
            issues: vec![review_issue(
                "Use Object.is for change detection in signals",
                "NaN and signed zero need Object.is semantics.",
                Some("return !Object.is(previous, next)"),
            )],
            matched_rules: 0,
            matched_rule_ids: Vec::new(),
            matched_rule_titles: Vec::new(),
            prompt_tokens_estimate: 0,
            trace_id: "trace".to_owned(),
            summary: None,
            stats: None,
        };

        supplement_fix_result_with_recalled_rules(
            &mut result,
            &HandoffRuleRecall {
                ids: vec!["6105b2dd-5b7b-41a4-9af0-5e14c2b245fc".to_owned()],
                titles: vec!["Use Object.is for reactive value comparisons".to_owned()],
                note: None,
            },
        );

        assert_eq!(
            result.issues[0].rule_id.as_deref(),
            Some("6105b2dd-5b7b-41a4-9af0-5e14c2b245fc")
        );
        assert_eq!(result.matched_rules, 1);
    }

    #[test]
    fn supplement_recall_single_rule_backfills_unrelated_issue_title() {
        let rule_id = "771e2e98-c010-4f9f-a387-45eabe55770a";
        let mut result = ReviewCheckResult {
            issues: vec![
                review_issue(
                    "Correct index for headChar",
                    "The provider finding title no longer shares words with the recalled memory.",
                    Some("Use the already validated byte index."),
                ),
                review_issue(
                    "Prefer safeAt for fallback reads",
                    "The concrete patch is still derived from the same single recalled rule.",
                    None,
                ),
            ],
            matched_rules: 0,
            matched_rule_ids: Vec::new(),
            matched_rule_titles: Vec::new(),
            prompt_tokens_estimate: 0,
            trace_id: "trace".to_owned(),
            summary: None,
            stats: None,
        };

        supplement_fix_result_with_recalled_rules(
            &mut result,
            &HandoffRuleRecall {
                ids: vec![rule_id.to_owned()],
                titles: vec!["Check c.Bind() error return value".to_owned()],
                note: None,
            },
        );

        assert!(
            result
                .issues
                .iter()
                .all(|issue| issue.rule_id.as_deref() == Some(rule_id))
        );
        assert_eq!(result.matched_rules, 1);
    }

    #[test]
    fn supplement_recall_multi_rule_still_requires_overlap() {
        let mut result = ReviewCheckResult {
            issues: vec![
                review_issue(
                    "Correct index for headChar",
                    "This unrelated finding must not be attributed when recall has multiple candidates.",
                    Some("Use the already validated byte index."),
                ),
                review_issue(
                    "Return body size limit error",
                    "Large request bodies need a stable 413 response.",
                    Some("Return 413 for body size limit errors."),
                ),
            ],
            matched_rules: 0,
            matched_rule_ids: Vec::new(),
            matched_rule_titles: Vec::new(),
            prompt_tokens_estimate: 0,
            trace_id: "trace".to_owned(),
            summary: None,
            stats: None,
        };

        supplement_fix_result_with_recalled_rules(
            &mut result,
            &HandoffRuleRecall {
                ids: vec![
                    "771e2e98-c010-4f9f-a387-45eabe55770a".to_owned(),
                    "d09b9631-01a9-4aa5-a4f5-cbed12c4c0de".to_owned(),
                ],
                titles: vec![
                    "Check c.Bind() error return value".to_owned(),
                    "Return 413 for body size limit errors".to_owned(),
                ],
                note: None,
            },
        );

        assert_eq!(result.issues[0].rule_id, None);
        assert_eq!(
            result.issues[1].rule_id.as_deref(),
            Some("d09b9631-01a9-4aa5-a4f5-cbed12c4c0de")
        );
        assert_eq!(result.matched_rules, 2);
    }

    #[test]
    fn format_fix_err_classifies_missing_provider_and_git() {
        let provider = format_fix_err(
            "Fix failed",
            "no LLM provider configured and no supported agent CLI found on PATH",
        );
        assert!(provider.contains("difflore providers setup"));
        assert!(provider.contains("Claude Code / Codex / Gemini / OpenCode"));

        let git = format_fix_err(
            "Fix failed",
            "failed to spawn git: No such file or directory",
        );
        assert!(git.contains("Git is required"));
        assert!(git.contains("Install Git"));
    }
}
