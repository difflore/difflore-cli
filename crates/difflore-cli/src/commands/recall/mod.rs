//! `difflore recall` — primary surface for "show which memories agents would see".
//!
//! Local rules are the product's open-source value floor: `recall` must show
//! what the CLI can retrieve from the on-disk rule corpus even when Cloud is
//! absent. Cloud review memory is an append-only enhancement when the user is
//! logged in and the current git repo can be scoped safely.
//!
//! This module is split by concern: data gathering (local + cloud retrieval,
//! embedder probing, example extraction, zero-match diagnostics) lives in
//! [`retrieval`], and `--json`/human/markdown rendering lives in
//! [`presentation`]. The shared result types, argument validation, and the
//! `handle_recall` orchestration stay here.

use std::process::Command;

use anyhow::{Context, bail};
use difflore_core::context::embedding::ActiveEmbedderKind;
use difflore_core::context::retrieval::RenderedRuleBody;
use difflore_core::context::types::PastVerdict;
use globset::Glob;

use crate::support::util::{exit_code, project_path};
use crate::installer;
use crate::runtime::CommandContext;
use crate::style::{self, sym};

mod presentation;
mod retrieval;
mod search;

use presentation::{
    cross_repo_starter_json, local_rules_json, recall_diagnostics_json, render_cloud_recall_human,
    render_cross_repo_starter_human, render_local_recall_human, render_zero_match_compact_human,
};
use retrieval::{
    build_zero_match_diagnostics, cross_repo_starter_hits, recall_cloud_review_memory,
    recall_local_rules, record_local_recall,
};

/// Whether the active embedder produces semantic vectors, plus a stable mode
/// tag for `--json`. Derived from the same `probe_active_embedder` chain the
/// `embeddings status` / `status` surfaces read, so all three agree on whether
/// recall ranking is semantic or keyword-only.
struct RecallSemanticState {
    /// True when a real embedding provider (cloud-managed or BYOK) is active.
    /// False for the local SHA1 lexical hash (no provider configured).
    semantic: bool,
    /// Stable snake_case tag for `--json`: `cloud` | `byok` | `keyword`.
    mode: &'static str,
}

impl RecallSemanticState {
    const fn from_kind(kind: &ActiveEmbedderKind) -> Self {
        match kind {
            ActiveEmbedderKind::Cloud { .. } => Self {
                semantic: true,
                mode: "cloud",
            },
            ActiveEmbedderKind::Byok { .. } => Self {
                semantic: true,
                mode: "byok",
            },
            ActiveEmbedderKind::Sha1 => Self {
                semantic: false,
                mode: "keyword",
            },
        }
    }

    /// One-line note for human surfaces when ranking is keyword-only; `None`
    /// when a semantic provider is active. Mirrors the `embeddings status` /
    /// `status` wording so the surfaces read consistently.
    const fn keyword_only_note(&self) -> Option<&'static str> {
        if self.semantic {
            None
        } else {
            Some(
                "semantic matching off; these matched by keyword (enable with `difflore embeddings setup` or `difflore cloud login`)",
            )
        }
    }
}

pub(crate) struct RecallArgs {
    pub(crate) intent: Option<String>,
    pub(crate) file: Option<String>,
    pub(crate) diff: bool,
    pub(crate) top_k: usize,
    pub(crate) json: bool,
    /// Effective verbose flag: callers fold `--why` into this so we only
    /// have to thread one knob downstream.
    pub(crate) verbose: bool,
    /// Print recalled rules as a paste-ready Markdown block.
    pub(crate) copy: bool,
}

impl From<crate::cli::RecallCliArgs> for RecallArgs {
    fn from(args: crate::cli::RecallCliArgs) -> Self {
        Self {
            intent: args.intent,
            file: args.file,
            diff: args.diff,
            top_k: args.top_k,
            json: args.json,
            verbose: args.verbose,
            copy: args.copy,
        }
    }
}

pub(crate) async fn handle_recall(ctx: &CommandContext, args: RecallArgs) {
    let RecallArgs {
        intent,
        file,
        diff,
        top_k,
        json,
        verbose,
        copy,
    } = args;

    let (resolved_intent, resolved_file, diff_files) =
        match resolve_intent_and_file(intent, file, diff) {
            Ok(triple) => triple,
            Err(e) => {
                eprintln!("{} {:#}", style::err(sym::ERR), e);
                exit_code(2);
            }
        };

    // `--copy` is a paste-friendly short-circuit: emit a self-contained
    // Markdown block without styled footers.
    if copy {
        handle_recall_copy(ctx, resolved_intent, resolved_file, &diff_files, top_k).await;
        return;
    }

    if !json {
        let header = if diff {
            "Top memories for current diff".to_owned()
        } else {
            format!("Top memories for: {resolved_intent}")
        };
        println!("{}", style::ok(&header));
        println!();
    }

    let (local, cloud) = recall_local_and_cloud(
        ctx,
        &resolved_intent,
        resolved_file.as_deref(),
        &diff_files,
        top_k,
        "cli-recall",
    )
    .await;

    let zero_match_diagnostics = if local.matches.is_empty() && cloud.verdicts.is_empty() {
        Some(build_zero_match_diagnostics(
            &local,
            &cloud,
            &resolved_intent,
            resolved_file.as_deref(),
        ))
    } else {
        None
    };

    // Whether the ranking the user just saw was semantic or keyword-only.
    // Probed from the same chain `embeddings status` reads (cheap, no network on
    // the SHA1/BYOK paths), so recall's honesty note agrees with the dedicated
    // status surface instead of silently presenting a lexical recall as if it
    // were semantic.
    let semantic_state = RecallSemanticState::from_kind(
        &difflore_core::context::embedding::probe_active_embedder().await,
    );

    if json {
        // Surface the strict-file-match information that record_local_recall
        // already computes, plus a recall timestamp — buyer-grade proof
        // pipelines need both at the top level and per-result.
        let recalled_at = chrono::Utc::now().to_rfc3339();
        let queried_file = resolved_file.as_deref();
        let strict_match_count = local
            .matches
            .iter()
            .filter(|hit| strict_file_pattern_match(&hit.file_patterns, queried_file))
            .count();
        let any_strict = strict_match_count > 0;
        let mut payload = serde_json::json!({
            "intent": resolved_intent,
            "file": queried_file,
            "recalledAt": recalled_at,
            "fileScopeFallback": local.file_scope_fallback,
            "strictFileMatch": any_strict,
            "strictMatchCount": strict_match_count,
            // Whether the ranking above is semantic or keyword-only, so a `--json`
            // consumer never mistakes a lexical (no-provider) recall for a semantic
            // one. `note` is non-null only when ranking degraded to keyword.
            "semanticRanking": {
                "semantic": semantic_state.semantic,
                "mode": semantic_state.mode,
                "note": semantic_state.keyword_only_note(),
            },
            "localRules": local_rules_json(&local, queried_file),
            "cloudReviewMemory": {
                "loggedIn": cloud.logged_in,
                "repoFullName": cloud.repo_full_name,
                "scope": cloud.scope,
                "teamId": cloud.team_id,
                "verdicts": cloud.verdicts,
            },
        });
        if let Some(diagnostics) = zero_match_diagnostics.as_ref()
            && let Some(object) = payload.as_object_mut()
        {
            object.insert(
                "diagnostics".to_owned(),
                recall_diagnostics_json(diagnostics),
            );
        }
        // Include cross-repo starter suggestions for the cold-start
        // (no scoped memory) case so proof pipelines can see the fallback fired.
        if local.matches.is_empty()
            && local.rules_indexed == 0
            && let Some(file) = queried_file
        {
            let starter = cross_repo_starter_hits(ctx, &resolved_intent, file, top_k).await;
            if !starter.is_empty()
                && let Some(object) = payload.as_object_mut()
            {
                object.insert(
                    "crossRepoStarter".to_owned(),
                    cross_repo_starter_json(&starter),
                );
            }
        }
        println!("{}", crate::support::util::json_or(&payload, "{}"));
        return;
    }

    if let Some(diagnostics) = zero_match_diagnostics.as_ref() {
        render_zero_match_compact_human(diagnostics);
    } else {
        render_local_recall_human(&local, &resolved_intent, resolved_file.as_deref(), verbose);
        // Honest degradation note: when these matches were ranked by the local
        // keyword hash (no embedding provider configured) say so, so the user does
        // not read a lexical recall as a semantic one. Only on the with-matches
        // path — the zero-match diagnostics branch already explains the empty case,
        // and a "ranking was keyword-only" note there would be noise.
        if !local.matches.is_empty()
            && let Some(note) = semantic_state.keyword_only_note()
        {
            println!("  {} {}", style::amber(sym::WARN), style::pewter(note));
        }
        // Cold-start fallback: no scoped memory here → offer transferable,
        // file-pattern-matched rules from other repos, clearly labeled (a separate
        // "from other repos" section, never presented as this repo's own memory).
        if local.matches.is_empty()
            && local.rules_indexed == 0
            && let Some(file) = resolved_file.as_deref()
        {
            let starter = cross_repo_starter_hits(ctx, &resolved_intent, file, top_k).await;
            render_cross_repo_starter_human(&starter, file);
        }
        println!();
        render_cloud_recall_human(&cloud, &resolved_intent, resolved_file.as_deref(), verbose);
    }

    println!();
    // Affirmation bridge confirming wired agents will receive these rules.
    // Only fires when there is at least one match; the empty branch already
    // routes to import-reviews.
    if !local.matches.is_empty() {
        let snapshot = installer::collect_status_snapshot();
        let installed: Vec<&'static str> = snapshot
            .clients
            .iter()
            .filter(|c| matches!(c.state, installer::InstallState::Installed))
            .map(|c| c.name)
            .collect();
        if installed.is_empty() {
            println!(
                "  {} No agents are wired yet; these local rules are ready once you run {} so Claude/Codex/Cursor can recall them.",
                style::pewter(sym::BULLET),
                style::cmd("difflore agents install"),
            );
        } else {
            let names = installed.join(", ");
            let n = local.matches.len();
            println!(
                "  {} {} will see {} local rule{} like these next time {} touch{} a matching file in this repo.",
                style::emerald(sym::OK),
                names,
                n,
                if n == 1 { "" } else { "s" },
                if installed.len() == 1 { "it" } else { "they" },
                if installed.len() == 1 { "es" } else { "" },
            );
        }
    }

    println!();
    // Bridge to the next useful action. If 0 rules came back, "fix --preview"
    // is a misleading bounce (it would just rerun the same empty retrieval);
    // route the user toward the local candidate path first. Cloud extraction
    // is an upgrade path, not the first gate for CLI-only value.
    if zero_match_diagnostics.is_none() {
        println!(
            "next: {}  {}",
            style::cmd("difflore status"),
            style::pewter("see matched memories, agent readiness, and accepted edits"),
        );
    }
}

/// Resolve the (intent, file, diff_files) triple from CLI flags. `--diff`
/// infers all three from the current git diff: file = first changed source
/// path, intent = a compact review-intent string built from the actual
/// diff, diff_files = the full ordered list of changed paths used by the
/// candidate-file fallback in `recall_local_and_cloud`. So
/// `recall --diff` previews the same structural signals `fix` will use.
fn resolve_intent_and_file(
    intent: Option<String>,
    file: Option<String>,
    diff: bool,
) -> anyhow::Result<(String, Option<String>, Vec<String>)> {
    if !diff {
        let intent = intent.context("missing intent. Provide a phrase, or pass `--diff`.")?;
        return Ok((intent, file, Vec::new()));
    }

    let files = git_diff_files()?;
    if files.is_empty() {
        bail!("`--diff` found no changed files. Stage or modify some files first.");
    }

    let target_file = file.or_else(|| primary_recall_file(&files));

    let synthetic_intent = match intent {
        Some(text) if !text.trim().is_empty() => text,
        _ => {
            let diff_text = git_diff_text().unwrap_or_default();
            let review_intent = difflore_core::context::intent_filter::build_review_intent_text(
                target_file.as_deref(),
                &diff_text,
            );
            if review_intent.trim().is_empty() {
                format!("changes in {}", files.join(", "))
            } else {
                review_intent
            }
        }
    };

    Ok((synthetic_intent, target_file, files))
}

/// `git diff --name-only` then `git diff --name-only --cached`. Union
/// (preserving order) covers staged + unstaged changes — same scope as
/// `difflore fix` defaults to.
fn git_diff_files() -> anyhow::Result<Vec<String>> {
    let cwd = project_path();
    let mut seen = std::collections::BTreeSet::new();
    let mut out: Vec<String> = Vec::new();

    for args in [
        &["diff", "--name-only"][..],
        &["diff", "--name-only", "--cached"][..],
    ] {
        let output = Command::new("git")
            .args(args)
            .current_dir(&cwd)
            .output()
            // ENOENT on `git` itself is a different failure mode than
            // "running git in a non-repo dir". Tell the user to install
            // git rather than leaking the OS-error string.
            .with_context(|| "`git` not found on PATH (install it, then retry)")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // The "not a git repository" case is the most common
            // user-facing error here — the user ran `recall --diff` from
            // a non-repo dir. Rewrite to actionable framing.
            if stderr.to_ascii_lowercase().contains("not a git repository") {
                bail!(
                    "`--diff` requires a git repo. cd into one, or pass an intent phrase \
                     (e.g. `difflore recall \"input validation\"`)."
                );
            }
            bail!("git {} failed: {}", args.join(" "), stderr.trim());
        }
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let trimmed = line.trim();
            if !trimmed.is_empty() && seen.insert(trimmed.to_owned()) {
                out.push(trimmed.to_owned());
            }
        }
    }

    Ok(out)
}

fn git_diff_text() -> anyhow::Result<String> {
    let cwd = project_path();
    let mut out = String::new();

    for args in [
        &["diff", "--no-ext-diff", "--unified=8"][..],
        &["diff", "--cached", "--no-ext-diff", "--unified=8"][..],
    ] {
        let output = Command::new("git")
            .args(args)
            .current_dir(&cwd)
            .output()
            .with_context(|| "`git` not found on PATH (install it, then retry)")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("git {} failed: {}", args.join(" "), stderr.trim());
        }
        out.push_str(&String::from_utf8_lossy(&output.stdout));
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }

    Ok(out)
}

async fn handle_recall_copy(
    ctx: &CommandContext,
    intent: String,
    file: Option<String>,
    diff_files: &[String],
    top_k: usize,
) {
    let (local, cloud) = recall_local_and_cloud(
        ctx,
        &intent,
        file.as_deref(),
        diff_files,
        top_k,
        "cli-recall-copy",
    )
    .await;

    if local.matches.is_empty() && cloud.verdicts.is_empty() {
        let diagnostics = build_zero_match_diagnostics(&local, &cloud, &intent, file.as_deref());
        if local.repo_full_name.is_none() {
            // No repo scope -> empty by design, not an empty corpus (mirror the
            // precedence used by the styled renderer + diagnostics).
            println!(
                "_difflore recalled 0 local memories for \"{intent}\"; this checkout has no GitHub remote, and local recall is repo-scoped (add one with `git remote -v`)._"
            );
        } else if local.rules_indexed == 0 {
            println!(
                "_difflore recalled 0 local memories for \"{intent}\" because this repo has no local memories yet._"
            );
        } else {
            println!("_difflore recalled 0 local memories for \"{intent}\"._");
        }
        if !cloud.logged_in {
            println!("_Cloud review memory was skipped because you are not logged in._");
        } else if cloud.repo_full_name.is_none() {
            println!(
                "_Cloud review memory was skipped because no GitHub repo remote was detected._"
            );
        }
        println!();
        println!("_Likely causes:_");
        for cause in diagnostics.possible_causes {
            println!("- _{}_", cause.message);
        }
        println!();
        println!("_Next steps:_");
        for step in diagnostics.next_steps {
            match step.command {
                Some(command) => println!("- `{command}`: _{}_", step.message),
                None => println!("- _{}_", step.message),
            }
        }
        return;
    }

    println!(
        "**difflore recalled {} local rule{} for \"{}\":**",
        local.matches.len(),
        if local.matches.len() == 1 { "" } else { "s" },
        intent,
    );
    println!();
    if local.matches.is_empty() {
        println!("- _No local rule matched._");
    }
    for hit in &local.matches {
        let source = hit
            .source_repo
            .as_deref()
            .filter(|repo| !repo.trim().is_empty())
            .unwrap_or("review memory");
        println!(
            "- **{}** <- learned from `{}`",
            truncate_one_line(&hit.title, 110),
            source,
        );
    }

    if !cloud.verdicts.is_empty() {
        println!();
        println!(
            "**Cloud review memory appended ({}):**",
            cloud.verdicts.len(),
        );
        for verdict in &cloud.verdicts {
            let source = source_label(verdict, cloud.repo_full_name.as_deref())
                .unwrap_or_else(|| "review memory".to_owned());
            println!(
                "- **{}** <- learned from `{}`",
                truncate_one_line(&verdict.issue_text, 110),
                source,
            );
        }
    } else if !cloud.logged_in {
        println!();
        println!("_Cloud review memory skipped: not logged in._");
    } else if cloud.repo_full_name.is_none() {
        println!();
        println!("_Cloud review memory skipped: no GitHub repo remote detected._");
    }
    // Same honesty note as the styled surface: if this paste-ready block was
    // ranked by the local keyword hash, say so, so a user pasting it into an
    // agent does not present a lexical recall as semantic.
    let semantic_state = RecallSemanticState::from_kind(
        &difflore_core::context::embedding::probe_active_embedder().await,
    );
    if let Some(note) = semantic_state.keyword_only_note() {
        println!();
        println!("_{note}_");
    }
    println!();
    println!("_Generated by `difflore recall`_");
}

async fn recall_local_and_cloud(
    ctx: &CommandContext,
    intent: &str,
    file: Option<&str>,
    diff_files: &[String],
    top_k: usize,
    session_id: &str,
) -> (LocalRecallResult, CloudRecallResult) {
    let local_branch = async {
        let mut local = recall_local_rules(ctx, intent, file, top_k).await;
        if let Some(primary_file) = file
            && local.rules_indexed > 0
            && strict_match_count_for_file(&local, Some(primary_file)) == 0
        {
            // Drive the fallback off the real `git diff --name-only` list,
            // since `build_review_intent_text` never emits a parseable
            // "changes in ..." prefix.
            for candidate_file in candidate_fallback_files(diff_files, primary_file) {
                let mut candidate =
                    recall_local_rules(ctx, intent, Some(candidate_file), top_k).await;
                if strict_match_count_for_file(&candidate, Some(candidate_file)) > 0 {
                    candidate.file_scope_fallback = true;
                    local = candidate;
                    break;
                }
            }
        }
        record_local_recall(ctx, &local, intent, file, top_k, session_id).await;
        local
    };
    let cloud_branch = recall_cloud_review_memory(ctx, intent, file, top_k);
    tokio::join!(local_branch, cloud_branch)
}

fn strict_match_count_for_file(local: &LocalRecallResult, file: Option<&str>) -> usize {
    local
        .matches
        .iter()
        .filter(|hit| strict_file_pattern_match(&hit.file_patterns, file))
        .count()
}

/// Synthetic parser kept only as a unit-test fixture documenting the
/// `"changes in ..."` intent shape. Production uses `git diff --name-only`
/// via the `diff_files` argument instead.
#[cfg(test)]
fn synthetic_diff_files(intent: &str) -> Vec<String> {
    let Some(rest) = intent.strip_prefix("changes in ") else {
        return Vec::new();
    };
    rest.split(", ")
        .filter_map(|part| {
            let trimmed = part.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_owned())
            }
        })
        .collect()
}

/// Pure helper: pick the candidate-file fallback list for
/// `recall_local_and_cloud`. The primary file is filtered out so the
/// fallback never re-queries the same scope. Order is preserved from
/// `diff_files` (which is `git diff --name-only` order).
fn candidate_fallback_files<'a>(diff_files: &'a [String], primary_file: &str) -> Vec<&'a str> {
    diff_files
        .iter()
        .map(String::as_str)
        .filter(|f| *f != primary_file)
        .collect()
}

pub(super) struct LocalRuleHit {
    pub(super) id: String,
    pub(super) title: String,
    pub(super) preview: String,
    /// First meaningful line of the rule's "bad"/"wrong" example, if the
    /// rule body carries a recognizable examples section. Rendered like the
    /// `difflore try` demo so real recall feels as sharp as the taste.
    pub(super) bad: Option<String>,
    /// First meaningful line of the rule's "good"/"fix" example, paired with
    /// `bad`. Either side may be present without the other.
    pub(super) fix: Option<String>,
    pub(super) rank_score: f64,
    pub(super) raw_score: f64,
    pub(super) confidence: f64,
    pub(super) file_patterns: Vec<String>,
    pub(super) source_repo: Option<String>,
    /// Full rule body and structured examples, hydrated from the DB after
    /// retrieval (see `hydrate_full_rule_bodies`). `None` until hydration runs;
    /// the cross-repo-starter path leaves it unset, keeping the chunk-only
    /// `--json` shape. When present, `recall --json` surfaces the actual
    /// fix/bad/good code rather than a headline with NULL bodies.
    pub(super) body: Option<RenderedRuleBody>,
}

pub(super) struct LocalRecallResult {
    pub(super) rules_indexed: usize,
    pub(super) repo_full_name: Option<String>,
    pub(super) matches: Vec<LocalRuleHit>,
    pub(super) file_scope_fallback: bool,
}

pub(super) struct RecallDiagnostics {
    pub(super) summary: String,
    pub(super) possible_causes: Vec<DiagnosticItem>,
    pub(super) next_steps: Vec<DiagnosticStep>,
}

pub(super) struct DiagnosticItem {
    pub(super) code: &'static str,
    pub(super) message: String,
}

#[derive(Clone)]
pub(super) struct DiagnosticStep {
    pub(super) command: Option<String>,
    pub(super) message: String,
}

/// Size of the candidate pool to retrieve before the strict-pattern
/// re-rank truncates back to `top_k`. We over-fetch (4x) so file-pattern
/// matches the content-only retriever ranks low still have room to
/// surface, but we cap the pool to bound work for small `top_k`. The
/// cap must never fall below `top_k` itself: `--top-k` is clamped to the
/// documented 1..=50 range upstream, so a fixed cap of 40 would make
/// `usize::clamp(top_k, 40)` panic (`min > max`) for `top_k` in 41..=50.
pub(super) fn candidate_pool_size(top_k: usize) -> usize {
    top_k.saturating_mul(4).clamp(top_k, top_k.max(40))
}

pub(super) fn query_looks_broad(intent: &str) -> bool {
    let meaningful_words = intent
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| {
            let lower = word.to_ascii_lowercase();
            lower.len() > 2
                && !matches!(
                    lower.as_str(),
                    "ask"
                        | "bug"
                        | "change"
                        | "changes"
                        | "code"
                        | "current"
                        | "diff"
                        | "file"
                        | "files"
                        | "fix"
                        | "issue"
                        | "review"
                        | "thing"
                        | "update"
                )
        })
        .count();
    meaningful_words <= 2 || intent.trim().chars().count() <= 18
}

pub(super) fn more_specific_query_example(intent: &str, file: Option<&str>) -> String {
    if intent_looks_like_diff(intent) {
        if let Some(file) = file.and_then(file_extension_hint) {
            return format!("{file} review convention for the current diff");
        }
        return "review convention for the current diff".to_owned();
    }
    if let Some(file) = file.and_then(file_extension_hint) {
        return format!("{file} review convention for {intent}");
    }
    format!("{intent} around validation, error handling, or team conventions")
}

fn file_extension_hint(file: &str) -> Option<&'static str> {
    match std::path::Path::new(file)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("rs") => Some("Rust"),
        Some("ts" | "tsx" | "js" | "jsx") => Some("TypeScript"),
        Some("go") => Some("Go"),
        Some("py") => Some("Python"),
        Some("rb") => Some("Ruby"),
        Some("java") => Some("Java"),
        _ => None,
    }
}

pub(super) fn recall_command(intent: &str, file: Option<&str>) -> String {
    let mut command = format!("difflore recall {}", quote_cli_arg(intent));
    if let Some(file) = file.map(str::trim).filter(|file| !file.is_empty()) {
        command.push_str(" --file ");
        command.push_str(&quote_cli_arg(file));
    }
    command
}

pub(super) fn recall_command_for_zero_match(intent: &str, file: Option<&str>) -> String {
    if intent_looks_like_diff(intent) {
        return "difflore recall --diff".to_owned();
    }
    recall_command(intent, file)
}

fn intent_looks_like_diff(intent: &str) -> bool {
    let trimmed = intent.trim_start();
    trimmed.starts_with("diff --git")
        || trimmed.contains("\n@@")
        || trimmed.contains("\n-")
        || trimmed.contains("\n+")
        || intent.lines().count() > 1
        || intent.chars().count() > 160
}

pub(super) fn recall_subject(intent: &str) -> String {
    if intent_looks_like_diff(intent) {
        "this diff".to_owned()
    } else {
        format!("\"{}\"", truncate_one_line(intent, 72))
    }
}

fn quote_cli_arg(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

pub(super) fn strict_file_pattern_match(patterns: &[String], file: Option<&str>) -> bool {
    let Some(file) = file.map(str::trim).filter(|file| !file.is_empty()) else {
        return false;
    };
    let normalised = file.trim_start_matches('/').replace('\\', "/");
    patterns.iter().any(|pattern| {
        Glob::new(pattern).is_ok_and(|glob| glob.compile_matcher().is_match(&normalised))
    })
}

pub(super) fn local_rule_title(content: &str, fallback: &str) -> String {
    search::rule_title(content, fallback)
}

pub(super) struct CloudRecallResult {
    pub(super) logged_in: bool,
    pub(super) repo_full_name: Option<String>,
    pub(super) scope: &'static str,
    pub(super) team_id: Option<String>,
    pub(super) verdicts: Vec<PastVerdict>,
}

pub(super) fn source_label(verdict: &PastVerdict, repo: Option<&str>) -> Option<String> {
    match (repo, verdict.source_pr_number) {
        (Some(repo), Some(number)) => Some(format!("{repo}#{number}")),
        (Some(repo), None) => Some(repo.to_owned()),
        (None, Some(number)) => Some(format!("source PR #{number}")),
        (None, None) => None,
    }
}

pub(super) fn truncate_one_line(value: &str, max_chars: usize) -> String {
    let cleaned = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if cleaned.chars().count() <= max_chars {
        return cleaned;
    }
    let mut out: String = cleaned.chars().take(max_chars.saturating_sub(3)).collect();
    out.push_str("...");
    out
}

fn is_source_or_test_file(file: &str) -> bool {
    let normalized = file.replace('\\', "/").to_ascii_lowercase();
    let Some(ext) = std::path::Path::new(&normalized)
        .extension()
        .and_then(|ext| ext.to_str())
    else {
        return false;
    };
    matches!(
        ext,
        "c" | "cc"
            | "cpp"
            | "cxx"
            | "h"
            | "hpp"
            | "cs"
            | "go"
            | "java"
            | "js"
            | "jsx"
            | "mjs"
            | "cjs"
            | "ts"
            | "tsx"
            | "mts"
            | "cts"
            | "py"
            | "rb"
            | "rs"
            | "swift"
            | "kt"
            | "kts"
            | "php"
            | "vue"
            | "svelte"
    )
}

fn primary_recall_file(files: &[String]) -> Option<String> {
    files
        .iter()
        .find(|p| is_source_or_test_file(p))
        .or_else(|| files.iter().find(|p| is_manifest_or_lockfile(p)))
        .or_else(|| files.iter().find(|p| is_reviewable_config_file(p)))
        .or_else(|| files.first())
        .cloned()
}

fn is_manifest_or_lockfile(file: &str) -> bool {
    let normalized = file.replace('\\', "/").to_ascii_lowercase();
    let basename = normalized.rsplit('/').next().unwrap_or(normalized.as_str());
    matches!(
        basename,
        "package.json"
            | "pnpm-lock.yaml"
            | "package-lock.json"
            | "yarn.lock"
            | "bun.lockb"
            | "cargo.toml"
            | "cargo.lock"
            | "go.mod"
            | "go.sum"
            | "gemfile"
            | "gemfile.lock"
            | "pyproject.toml"
            | "poetry.lock"
            | "requirements.txt"
            | "pom.xml"
            | "build.gradle"
            | "build.gradle.kts"
    ) || basename.ends_with(".gemspec")
}

fn is_reviewable_config_file(file: &str) -> bool {
    let normalized = file.replace('\\', "/").to_ascii_lowercase();
    if normalized.starts_with(".github/workflows/") {
        return true;
    }
    let basename = normalized.rsplit('/').next().unwrap_or(normalized.as_str());
    matches!(
        basename,
        "dockerfile"
            | "docker-compose.yml"
            | "docker-compose.yaml"
            | "tsconfig.json"
            | "vite.config.ts"
            | "vite.config.js"
            | "webpack.config.js"
            | "eslint.config.js"
            | "eslint.config.mjs"
            | ".eslintrc"
            | ".eslintrc.json"
            | ".prettierrc"
            | "rubocop.yml"
            | ".rubocop.yml"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    // Test-only helpers pulled from the `retrieval` submodule explicitly so the
    // parent module's production imports stay minimal.
    use super::retrieval::{
        ExampleSide, build_local_hits, classify_example_heading, content_only_file_scope_fallback,
        divergent_example_lines, extract_rule_examples, filter_starter_by_relevance,
        first_example_code_line, is_markdown_section_break,
    };
    use difflore_core::context::retrieval::{RenderedRuleExample, ScoredRuleChunk};
    use difflore_core::skills::SearchSkillMeta;

    // ── semantic-state honesty note ──────────────────────────────────────
    //
    // Recall must not present a keyword-only (no-provider SHA1) ranking as a
    // semantic one. These pin the mapping from the active embedder to the
    // honesty note + the stable `--json` mode tag, consistent with what
    // `embeddings status` reports.

    #[test]
    fn semantic_state_sha1_is_keyword_only_with_honest_note() {
        let state = RecallSemanticState::from_kind(&ActiveEmbedderKind::Sha1);
        assert!(!state.semantic, "SHA1 hash is not semantic");
        assert_eq!(state.mode, "keyword");
        let note = state.keyword_only_note().expect("keyword path has a note");
        assert!(
            note.contains("semantic matching off"),
            "note must say semantic is off: {note}"
        );
        // The note must name the enablement paths so it is actionable and reads
        // consistently with `embeddings status` / `status`.
        assert!(note.contains("difflore embeddings setup"), "note: {note}");
        assert!(note.contains("difflore cloud login"), "note: {note}");
    }

    #[test]
    fn semantic_state_providers_are_semantic_with_no_note() {
        let cloud = RecallSemanticState::from_kind(&ActiveEmbedderKind::Cloud {
            model: "text-embedding-3-small".to_owned(),
            dim: 1536,
        });
        assert!(cloud.semantic);
        assert_eq!(cloud.mode, "cloud");
        assert!(
            cloud.keyword_only_note().is_none(),
            "a semantic provider must not emit a keyword-only note"
        );

        let byok = RecallSemanticState::from_kind(&ActiveEmbedderKind::Byok {
            provider_host: "api.openai.com".to_owned(),
            model: "text-embedding-3-small".to_owned(),
            dim: 1536,
        });
        assert!(byok.semantic);
        assert_eq!(byok.mode, "byok");
        assert!(byok.keyword_only_note().is_none());
    }

    // ── extract_rule_examples: format coverage ───────────────────────────
    //
    // The parser is the felt-value core: it must pull a concrete bad→fix pair
    // out of every shape a real rule body uses, and degrade silently when none
    // is present. Each test pins one format the corpus actually contains.

    #[test]
    fn examples_plain_bad_good_blocks() {
        // The `difflore try` / bundled-corpus shape: `Bad:` then a line, blank,
        // `Good:` then a line. No fences.
        let content = "Rule Name: Cap bodies\nType: review\n\n\
             Some prose about limits.\n\n\
             Bad:\ndata, _ := io.ReadAll(r.Body)\n\n\
             Good:\nr.Body = http.MaxBytesReader(w, r.Body, max)";
        let (bad, fix) = extract_rule_examples(content);
        assert_eq!(bad.as_deref(), Some("data, _ := io.ReadAll(r.Body)"));
        assert_eq!(
            fix.as_deref(),
            Some("r.Body = http.MaxBytesReader(w, r.Body, max)")
        );
    }

    #[test]
    fn examples_generated_examples_section_with_fences() {
        // The dominant generated/imported format: `### Examples` then
        // `❌ Bad:` ```code``` `✅ Good:` ```code```. The `### Examples` line
        // itself must NOT be treated as a side heading, and the fenced first
        // line is what we return.
        let content = "# Avoid unbounded reads\n\nProse.\n\n\
             ### Examples\n\n\
             ❌ Bad:\n```go\nbody, _ := io.ReadAll(r.Body)\nuse(body)\n```\n\n\
             ✅ Good:\n```go\nr.Body = http.MaxBytesReader(w, r.Body, 10<<20)\nbody, err := io.ReadAll(r.Body)\n```\n";
        let (bad, fix) = extract_rule_examples(content);
        assert_eq!(bad.as_deref(), Some("body, _ := io.ReadAll(r.Body)"));
        assert_eq!(
            fix.as_deref(),
            Some("r.Body = http.MaxBytesReader(w, r.Body, 10<<20)")
        );
    }

    #[test]
    fn examples_markdown_wrong_correct_headings_with_fences() {
        // `### ❌ Wrong` / `### ✅ Correct`, the format the task calls dominant
        // in the real corpus. Glyph + `###` + keyword must all be tolerated.
        // This is a before/after of the SAME function: the first code line of
        // each fence is the identical `switch v {`. The divergence walk must
        // skip the shared `switch v {` / `case A:` prefix and surface the first
        // line that actually changes — NOT show the same signature on both sides.
        let content = "# Switch defaults\n\n\
             ### ❌ Wrong\n```go\nswitch v {\ncase A:\n}\n```\n\n\
             ### ✅ Correct\n```go\nswitch v {\ncase A:\ndefault:\n\treturn fmt.Errorf(\"unhandled %v\", v)\n}\n```\n";
        let (bad, fix) = extract_rule_examples(content);
        assert_eq!(bad.as_deref(), Some("}"));
        assert_eq!(fix.as_deref(), Some("default:"));
        // The rendered bad/fix must differ — that is the whole point of the
        // divergence skip; identical lines looked broken.
        assert_ne!(bad, fix);
    }

    #[test]
    fn examples_before_after_same_signature_surfaces_the_real_change() {
        // The canonical bug: bad and good are a before/after of the SAME
        // function, so each fence's first line is the identical signature
        // `func applyVisibility(opts *RequestOptions, visibility string) {`.
        // The fix must skip that shared signature and surface the first line
        // that actually DIFFERS, not echo the signature on both sides.
        let content = "# Apply visibility\n\n\
             ### ❌ Bad\n```go\n\
             func applyVisibility(opts *RequestOptions, visibility string) {\n\
             \topts.Visibility = visibility\n\
             }\n```\n\n\
             ### ✅ Fix\n```go\n\
             func applyVisibility(opts *RequestOptions, visibility string) {\n\
             \tif visibility == \"\" {\n\
             \t\treturn\n\
             \t}\n\
             \topts.Visibility = visibility\n\
             }\n```\n";
        let (bad, fix) = extract_rule_examples(content);
        // First differing line, NOT the shared `func applyVisibility(...)`.
        assert_eq!(bad.as_deref(), Some("opts.Visibility = visibility"));
        assert_eq!(fix.as_deref(), Some("if visibility == \"\" {"));
        assert_ne!(bad, fix);
        assert!(
            !bad.as_deref().unwrap().contains("func applyVisibility"),
            "bad must not be the shared function signature",
        );
    }

    #[test]
    fn examples_real_enum_switch_rule_diverges_past_shared_signature() {
        // Regression pin using the EXACT body of the real corpus rule
        // "Enum switch must have default return" (cli/cli). Its `## Bad` and
        // `## Good` blocks are a before/after of the same `applyVisibility`
        // function, so the OLD first-line logic returned the identical
        // signature on both sides (the reported bug). The divergence walk must
        // skip the shared 11-line prefix and surface the first changed lines:
        // the `// no default` comment on the bad side and `default:` on the fix
        // side.
        let content = "# Enum Switch Must Have a Default Early-Return Clause\n\n\
             ## Bad\n\n\
             ```go\n\
             func applyVisibility(opts *RequestOptions, visibility string) {\n\
             \tswitch visibility {\n\
             \tcase \"public\":\n\
             \t\topts.Private = boolPtr(false)\n\
             \t\topts.Internal = boolPtr(false)\n\
             \tcase \"private\":\n\
             \t\topts.Private = boolPtr(true)\n\
             \t\topts.Internal = boolPtr(false)\n\
             \tcase \"internal\":\n\
             \t\topts.Private = boolPtr(false)\n\
             \t\topts.Internal = boolPtr(true)\n\
             \t// no default — unrecognized value silently sets nothing,\n\
             \t// but pointer fields may already be non-nil from earlier code\n\
             \t}\n\
             }\n\
             ```\n\n\
             ## Good\n\n\
             ```go\n\
             func applyVisibility(opts *RequestOptions, visibility string) {\n\
             \tswitch visibility {\n\
             \tcase \"public\":\n\
             \t\topts.Private = boolPtr(false)\n\
             \t\topts.Internal = boolPtr(false)\n\
             \tcase \"private\":\n\
             \t\topts.Private = boolPtr(true)\n\
             \t\topts.Internal = boolPtr(false)\n\
             \tcase \"internal\":\n\
             \t\topts.Private = boolPtr(false)\n\
             \t\topts.Internal = boolPtr(true)\n\
             \tdefault:\n\
             \t\treturn // unrecognized value: leave struct untouched\n\
             \t}\n\
             }\n\
             ```\n";
        let (bad, fix) = extract_rule_examples(content);
        assert_eq!(
            bad.as_deref(),
            Some("// no default — unrecognized value silently sets nothing,"),
        );
        assert_eq!(fix.as_deref(), Some("default:"));
        assert_ne!(bad, fix);
        // Neither side may be the shared `func applyVisibility(...)` signature.
        assert!(!bad.as_deref().unwrap().contains("func applyVisibility"));
        assert!(!fix.as_deref().unwrap().contains("func applyVisibility"));
    }

    #[test]
    fn divergent_lines_keep_first_lines_when_first_lines_differ() {
        // The common demo-style case: first lines already differ, so they are
        // returned unchanged (no divergence walk).
        let bad = "data, _ := io.ReadAll(r.Body)\nuse(data)";
        let fix = "r.Body = http.MaxBytesReader(w, r.Body, max)\nbody, _ := io.ReadAll(r.Body)";
        assert_eq!(
            divergent_example_lines(Some(bad), Some(fix)),
            (
                Some("data, _ := io.ReadAll(r.Body)".to_owned()),
                Some("r.Body = http.MaxBytesReader(w, r.Body, max)".to_owned()),
            ),
        );
    }

    #[test]
    fn divergent_lines_single_line_blocks_kept_as_is() {
        // Single-line blocks with the SAME content: nothing to advance to, so
        // keep the first lines rather than inventing a divergence.
        let line = "switch v {";
        assert_eq!(
            divergent_example_lines(Some(line), Some(line)),
            (Some("switch v {".to_owned()), Some("switch v {".to_owned())),
        );
    }

    #[test]
    fn divergent_lines_identical_blocks_keep_first_lines() {
        // Fully identical multi-line blocks (degenerate corpus): both walks
        // exhaust together with no divergence → keep first lines.
        let block = "func f() {\n\tdoThing()\n}";
        assert_eq!(
            divergent_example_lines(Some(block), Some(block)),
            (Some("func f() {".to_owned()), Some("func f() {".to_owned())),
        );
    }

    #[test]
    fn divergent_lines_good_adds_trailing_lines_surfaces_added_line() {
        // Same signature, good adds lines, bad is a STRICT PREFIX of good.
        // The fix side surfaces the first added line; the bad side falls back to
        // its (shared) first line since it has nothing past the prefix.
        let bad = "func f() {\n\tdoThing()";
        let fix = "func f() {\n\tdoThing()\n\tdoExtra()\n}";
        assert_eq!(
            divergent_example_lines(Some(bad), Some(fix)),
            (Some("func f() {".to_owned()), Some("doExtra()".to_owned())),
        );
    }

    #[test]
    fn divergent_lines_bad_has_extra_trailing_keeps_first_lines() {
        // Same signature, but only the BAD side has extra trailing lines (fix is
        // a strict prefix). There is no new good line to show, so keep first
        // lines rather than surfacing a divergence the fix example doesn't have.
        let bad = "func f() {\n\tdoThing()\n\tleak()\n}";
        let fix = "func f() {\n\tdoThing()";
        assert_eq!(
            divergent_example_lines(Some(bad), Some(fix)),
            (Some("func f() {".to_owned()), Some("func f() {".to_owned())),
        );
    }

    #[test]
    fn divergent_lines_one_side_missing_returns_present_first_line() {
        // One side absent → unchanged: return the present side's first line.
        assert_eq!(
            divergent_example_lines(Some("panic(err)\nmore()"), None),
            (Some("panic(err)".to_owned()), None),
        );
        assert_eq!(
            divergent_example_lines(None, Some("return nil\nmore()")),
            (None, Some("return nil".to_owned())),
        );
        assert_eq!(divergent_example_lines(None, None), (None, None));
    }

    #[test]
    fn examples_wrong_right_without_hashes() {
        // `❌ Wrong` / `✅ Right` with no `###` prefix.
        let content = "Use errors.Is for sentinel comparison.\n\n\
             ❌ Wrong\n```go\nif err == io.EOF {}\n```\n\n\
             ✅ Right\n```go\nif errors.Is(err, io.EOF) {}\n```\n";
        let (bad, fix) = extract_rule_examples(content);
        assert_eq!(bad.as_deref(), Some("if err == io.EOF {}"));
        assert_eq!(fix.as_deref(), Some("if errors.Is(err, io.EOF) {}"));
    }

    #[test]
    fn examples_bad_example_good_example_labels() {
        // `Bad example:` / `Good example:` with `### Examples` umbrella.
        let content = "## Examples\n\n\
             Bad example:\n```ts\nconst x = await fetch(url)\n```\n\n\
             Good example:\n```ts\nconst x = await fetchWithTimeout(url, 5000)\n```\n";
        let (bad, fix) = extract_rule_examples(content);
        assert_eq!(bad.as_deref(), Some("const x = await fetch(url)"));
        assert_eq!(
            fix.as_deref(),
            Some("const x = await fetchWithTimeout(url, 5000)")
        );
    }

    #[test]
    fn examples_wrong_correct_colon_labels_no_fence() {
        // `Wrong:` / `Correct:` inline label form, no code fences.
        let content = "Prefer guard clauses.\n\n\
             Wrong:\nif (ok) { doStuff(); }\n\n\
             Correct:\nif (!ok) return;\ndoStuff();";
        let (bad, fix) = extract_rule_examples(content);
        assert_eq!(bad.as_deref(), Some("if (ok) { doStuff(); }"));
        assert_eq!(fix.as_deref(), Some("if (!ok) return;"));
    }

    #[test]
    fn examples_bad_fix_labels() {
        // `Bad:` / `Fix:` pairing (Fix is a synonym for the good side).
        let content = "Bad:\nx == nil\n\nFix:\nx.IsZero()";
        let (bad, fix) = extract_rule_examples(content);
        assert_eq!(bad.as_deref(), Some("x == nil"));
        assert_eq!(fix.as_deref(), Some("x.IsZero()"));
    }

    #[test]
    fn examples_anti_pattern_better_headings() {
        // The real corpus also uses `### ❌ Anti-pattern:` / `### ✅ Better:`.
        let content = "### ❌ Anti-pattern: Separate state\n```rust\nlet a = 1;\n```\n\n\
             ### ✅ Better: Capture together\n```rust\nlet b = 2;\n```\n";
        let (bad, fix) = extract_rule_examples(content);
        assert_eq!(bad.as_deref(), Some("let a = 1;"));
        assert_eq!(fix.as_deref(), Some("let b = 2;"));
    }

    #[test]
    fn examples_none_when_no_recognizable_section() {
        // Plain prose with no example headings -> (None, None) so the renderer
        // degrades to today's preview-only output.
        let content = "Rule Name: Be careful\nType: review\n\n\
             Always validate user input before using it in a query. Discuss \
             with the team if unsure. No code samples here.";
        assert_eq!(extract_rule_examples(content), (None, None));
    }

    #[test]
    fn examples_one_side_only_bad() {
        // A rule with only the bad side present must return (Some, None), and
        // the renderer omits the missing `fix` line.
        let content = "### ❌ Wrong\n```go\npanic(err)\n```\n\nSome trailing prose, no good block.";
        let (bad, fix) = extract_rule_examples(content);
        assert_eq!(bad.as_deref(), Some("panic(err)"));
        assert_eq!(fix, None);
    }

    #[test]
    fn examples_one_side_only_fix() {
        // Symmetric: only a good/fix side present.
        let content = "✅ Good:\n```go\nreturn fmt.Errorf(\"wrap: %w\", err)\n```\n";
        let (bad, fix) = extract_rule_examples(content);
        assert_eq!(bad, None);
        assert_eq!(fix.as_deref(), Some("return fmt.Errorf(\"wrap: %w\", err)"));
    }

    #[test]
    fn examples_skip_blank_lines_inside_fence() {
        // The first NON-blank fenced line is returned, not the opening blank.
        let content = "### ❌ Wrong\n```go\n\n\n   actualCode()\n```\n";
        let (bad, _fix) = extract_rule_examples(content);
        assert_eq!(bad.as_deref(), Some("actualCode()"));
    }

    #[test]
    fn examples_inline_bad_prose_is_not_a_heading() {
        // `Bad: this leaks a fd` is inline prose, not a section heading — it has
        // prose after the keyword, so it must not be misread as a `bad` marker.
        // With no real example section, the result is (None, None).
        let content =
            "Closing matters.\n\nBad: this silently leaks a file descriptor on the error path.";
        assert_eq!(extract_rule_examples(content), (None, None));
    }

    #[test]
    fn examples_inline_comment_markers_in_one_shared_fence() {
        // The DOMINANT real-corpus format (from imported PR reviews): a single
        // ```go fence whose `// ❌ Bad — …` and `// ✅ Good — …` lines are inline
        // comments, not separate `###` headings. The `fix` block is the LAST
        // marker and runs through the closing fence into a later `## How to
        // Apply` section — the snippet must be the good CODE line, never the
        // trailing heading. This is the exact shape that exposed the
        // over-extension bug.
        let content = "# Gate prefetch and prompt\n\n## Example\n\n\
             ```go\n\
             // \u{274c} Bad \u{2014} two different conditions for the same decision\n\
             if reviewerSearchFunc != nil {\n\
             \tgo prefetchReviewers()\n\
             }\n\
             if useReviewerSearch {\n\
             \tshowReviewerPrompt(prefetchedReviewers)\n\
             }\n\n\
             // \u{2705} Good \u{2014} single source of truth\n\
             useReviewerSearch := reviewerSearchFunc != nil\n\
             if useReviewerSearch {\n\
             \tgo prefetchReviewers()\n\
             }\n\
             ```\n\n\
             ## How to Apply\n\n\
             When reviewing code that prefetches...\n";
        let (bad, fix) = extract_rule_examples(content);
        assert_eq!(bad.as_deref(), Some("if reviewerSearchFunc != nil {"));
        assert_eq!(
            fix.as_deref(),
            Some("useReviewerSearch := reviewerSearchFunc != nil"),
            "fix must be the good code line, never the trailing `## How to Apply` heading",
        );
    }

    #[test]
    fn first_example_code_line_stops_at_section_break() {
        // A `✅ Good` block that runs to end-of-body must stop at the next
        // markdown heading / horizontal rule, not return it.
        assert_eq!(
            first_example_code_line("```\n```\n\n## How to Apply\nprose"),
            None,
            "a block with only a closing fence then a heading has no code line",
        );
        assert_eq!(
            first_example_code_line("real_code()\n## Next"),
            Some("real_code()".to_owned()),
        );
        assert_eq!(
            first_example_code_line("## Heading first\ncode()"),
            None,
            "stop immediately at a leading heading",
        );
    }

    #[test]
    fn is_markdown_section_break_recognizes_headings_and_rules() {
        assert!(is_markdown_section_break("# Title"));
        assert!(is_markdown_section_break("### Examples"));
        assert!(is_markdown_section_break("---"));
        assert!(is_markdown_section_break("***"));
        // Not breaks: code, prose, a bare `#`, or `#` glued to text.
        assert!(!is_markdown_section_break("let x = 1; // # not a heading"));
        assert!(!is_markdown_section_break("#nohash"));
        assert!(!is_markdown_section_break("#"));
        assert!(!is_markdown_section_break("plain prose"));
    }

    #[test]
    fn examples_section_heading_examples_is_not_a_side() {
        // `### Examples` must never classify as bad or fix on its own.
        assert_eq!(classify_example_heading("### Examples"), None);
        assert_eq!(classify_example_heading("## Examples"), None);
        assert_eq!(classify_example_heading("Examples"), None);
    }

    #[test]
    fn classify_heading_is_case_and_decoration_insensitive() {
        assert_eq!(classify_example_heading("BAD:"), Some(ExampleSide::Bad));
        assert_eq!(
            classify_example_heading("**Good example:**"),
            Some(ExampleSide::Fix)
        );
        assert_eq!(
            classify_example_heading("### ❌ wrong"),
            Some(ExampleSide::Bad)
        );
        assert_eq!(
            classify_example_heading("- Correct:"),
            Some(ExampleSide::Fix)
        );
        assert_eq!(classify_example_heading("random line"), None);
        assert_eq!(classify_example_heading(""), None);
    }

    #[test]
    fn build_local_hits_populates_bad_fix_from_content() {
        // End-to-end through the hit builder: a chunk whose body carries an
        // examples section must surface bad/fix on the hit so the renderer and
        // JSON can show them.
        let scored = vec![ScoredRuleChunk {
            skill_id: "r1".to_owned(),
            content: "Rule Name: Cap bodies\n\n### ❌ Wrong\n```go\nio.ReadAll(r.Body)\n```\n\n### ✅ Correct\n```go\nhttp.MaxBytesReader(w, r.Body, max)\n```\n".to_owned(),
            score: 0.9,
            confidence: 0.8,
        }];
        let mut metas = std::collections::HashMap::new();
        metas.insert(
            "r1".to_owned(),
            SearchSkillMeta {
                file_patterns: vec!["**/*.go".to_owned()],
                source_repo: Some("acme/widgets".to_owned()),
            },
        );
        let hits = build_local_hits(&scored, &metas);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].bad.as_deref(), Some("io.ReadAll(r.Body)"));
        assert_eq!(
            hits[0].fix.as_deref(),
            Some("http.MaxBytesReader(w, r.Body, max)")
        );
    }

    #[test]
    fn local_rules_json_includes_bad_fix() {
        let local = LocalRecallResult {
            rules_indexed: 1,
            repo_full_name: Some("acme/widgets".to_owned()),
            file_scope_fallback: false,
            matches: vec![LocalRuleHit {
                id: "rule-1".to_owned(),
                title: "Cap bodies".to_owned(),
                preview: "p".to_owned(),
                bad: Some("io.ReadAll(r.Body)".to_owned()),
                fix: Some("http.MaxBytesReader(...)".to_owned()),
                rank_score: 1.0,
                raw_score: 0.4,
                confidence: 0.8,
                file_patterns: vec!["**/*.go".to_owned()],
                source_repo: Some("acme/widgets".to_owned()),
                body: None,
            }],
        };
        let json = local_rules_json(&local, Some("internal/x.go"));
        assert_eq!(json["results"][0]["bad"], "io.ReadAll(r.Body)");
        assert_eq!(json["results"][0]["fix"], "http.MaxBytesReader(...)");

        // A hit with no examples must serialise bad/fix as null, never "".
        let bare = LocalRecallResult {
            rules_indexed: 1,
            repo_full_name: Some("acme/widgets".to_owned()),
            file_scope_fallback: false,
            matches: vec![LocalRuleHit {
                id: "rule-2".to_owned(),
                title: "No examples".to_owned(),
                preview: "p".to_owned(),
                bad: None,
                fix: None,
                rank_score: 1.0,
                raw_score: 0.4,
                confidence: 0.8,
                file_patterns: Vec::new(),
                source_repo: None,
                body: None,
            }],
        };
        let bare_json = local_rules_json(&bare, None);
        assert!(bare_json["results"][0]["bad"].is_null());
        assert!(bare_json["results"][0]["fix"].is_null());
    }

    #[test]
    fn local_rules_json_surfaces_full_body_for_recalled_rule_with_example() {
        // The reported bug: a recalled rule that HAS a bad/good example came
        // back with the fix/bad/good bodies NULL in `--json`, so an agent saw
        // only the headline. After hydration, the hit carries the full rendered
        // body + structured examples + the authoritative bad/fix code, and
        // `--json` must surface all of them as non-null.
        let local = LocalRecallResult {
            rules_indexed: 1,
            repo_full_name: Some("acme/widgets".to_owned()),
            file_scope_fallback: false,
            matches: vec![LocalRuleHit {
                id: "rule-cap".to_owned(),
                title: "Cap request bodies".to_owned(),
                preview: "Rule Name: Cap request bodies".to_owned(),
                // Pre-hydration these were NULL because the indexed chunk body
                // carried no example section; hydration fills them from the DB.
                bad: Some("data, _ := io.ReadAll(r.Body)".to_owned()),
                fix: Some("r.Body = http.MaxBytesReader(w, r.Body, max)".to_owned()),
                rank_score: 1.0,
                raw_score: 0.42,
                confidence: 0.82,
                file_patterns: vec!["**/*.go".to_owned()],
                source_repo: Some("acme/widgets".to_owned()),
                body: Some(RenderedRuleBody {
                    body: "## Rule rule-cap — Cap request bodies\n### Cases\n❌ Counter-example:\n```\ndata, _ := io.ReadAll(r.Body)\n```\n✅ Conforming:\n```\nr.Body = http.MaxBytesReader(w, r.Body, max)\n```\n".to_owned(),
                    origin: "pr_review".to_owned(),
                    confidence: 0.82,
                    trigger: Some("Touching an HTTP body read".to_owned()),
                    check: Some("Is the body capped before reading?".to_owned()),
                    examples: vec![RenderedRuleExample {
                        bad_code: "data, _ := io.ReadAll(r.Body)".to_owned(),
                        good_code: "r.Body = http.MaxBytesReader(w, r.Body, max)".to_owned(),
                        description: Some("reviewer flagged unbounded read".to_owned()),
                    }],
                }),
            }],
        };

        let json = local_rules_json(&local, Some("internal/server.go"));
        let result = &json["results"][0];

        // Headline still present.
        assert_eq!(result["skillId"], "rule-cap");
        assert_eq!(result["bad"], "data, _ := io.ReadAll(r.Body)");
        assert_eq!(
            result["fix"],
            "r.Body = http.MaxBytesReader(w, r.Body, max)"
        );
        // Full body is non-null and carries the rendered Cases block.
        assert!(!result["body"].is_null(), "body must be non-null");
        assert!(
            result["body"]
                .as_str()
                .expect("body string")
                .contains("### Cases")
        );
        // Structured examples expose the authoritative bad/good code.
        assert_eq!(
            result["examples"][0]["badCode"],
            "data, _ := io.ReadAll(r.Body)"
        );
        assert_eq!(
            result["examples"][0]["goodCode"],
            "r.Body = http.MaxBytesReader(w, r.Body, max)"
        );
        // Supplementary fix/check fields are surfaced.
        assert_eq!(result["origin"], "pr_review");
        assert_eq!(result["check"], "Is the body capped before reading?");
        assert_eq!(result["trigger"], "Touching an HTTP body read");
    }

    #[test]
    fn local_rules_json_omits_body_fields_when_not_hydrated() {
        // A hit whose body was never hydrated (e.g. the cross-repo-starter path,
        // or a stale index entry) keeps the chunk-only shape: no `body` /
        // `examples` keys, so the output stays backward-compatible.
        let local = LocalRecallResult {
            rules_indexed: 1,
            repo_full_name: Some("acme/widgets".to_owned()),
            file_scope_fallback: false,
            matches: vec![LocalRuleHit {
                id: "rule-unhydrated".to_owned(),
                title: "Unhydrated".to_owned(),
                preview: "p".to_owned(),
                bad: None,
                fix: None,
                rank_score: 1.0,
                raw_score: 0.2,
                confidence: 0.7,
                file_patterns: Vec::new(),
                source_repo: None,
                body: None,
            }],
        };
        let json = local_rules_json(&local, None);
        let result = &json["results"][0];
        assert!(result.get("body").is_none(), "no body key when unhydrated");
        assert!(result.get("examples").is_none());
        // Original chunk-only fields remain.
        assert_eq!(result["skillId"], "rule-unhydrated");
        assert!(result["bad"].is_null());
    }

    #[test]
    fn candidate_pool_size_never_panics_in_documented_range() {
        // `--top-k` is clamped to 1..=50 upstream. The pool must be at
        // least `top_k` (so truncation has enough candidates) and must
        // never panic, including the 41..=50 region that previously hit
        // `usize::clamp(top_k, 40)` with `min > max`.
        for top_k in 1..=50usize {
            let pool = candidate_pool_size(top_k);
            assert!(pool >= top_k, "pool {pool} must be >= top_k {top_k}");
        }
        // Common range keeps the 4x-capped-at-40 behavior.
        assert_eq!(candidate_pool_size(5), 20);
        assert_eq!(candidate_pool_size(10), 40);
        assert_eq!(candidate_pool_size(20), 40);
        // Edge range no longer panics; pool collapses to top_k.
        assert_eq!(candidate_pool_size(45), 45);
        assert_eq!(candidate_pool_size(50), 50);
    }

    #[test]
    fn cross_repo_starter_json_carries_attribution() {
        let hits = vec![LocalRuleHit {
            id: "r1".to_owned(),
            title: "Return 413 for oversized bodies".to_owned(),
            preview: "body...".to_owned(),
            bad: Some("data, _ := io.ReadAll(r.Body)".to_owned()),
            fix: Some("r.Body = http.MaxBytesReader(w, r.Body, max)".to_owned()),
            rank_score: 0.9,
            raw_score: 0.4,
            confidence: 0.8,
            file_patterns: vec!["**/*.go".to_owned()],
            source_repo: Some("gin-gonic/gin".to_owned()),
            body: None,
        }];
        let json = cross_repo_starter_json(&hits);
        let arr = json.as_array().expect("array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["sourceRepo"], "gin-gonic/gin");
        assert_eq!(arr[0]["title"], "Return 413 for oversized bodies");
        assert_eq!(arr[0]["filePatterns"][0], "**/*.go");
        // Empty input must serialise as an empty array, never null.
        assert!(
            cross_repo_starter_json(&[])
                .as_array()
                .expect("array")
                .is_empty()
        );
    }

    fn starter_hit(id: &str, raw_score: f64) -> LocalRuleHit {
        LocalRuleHit {
            id: id.to_owned(),
            title: id.to_owned(),
            preview: String::new(),
            bad: None,
            fix: None,
            rank_score: 1.0,
            raw_score,
            confidence: 0.8,
            file_patterns: vec!["**/*.go".to_owned()],
            source_repo: Some("x/y".to_owned()),
            body: None,
        }
    }

    #[test]
    fn starter_relevance_floor_drops_low_intent_hits() {
        // A file-extension match with near-zero intent relevance is noise, not
        // memory; only hits at/above the floor survive (boundary is inclusive).
        let hits = vec![
            starter_hit("strong", 0.40),
            starter_hit("weak", 0.05),
            starter_hit("border", 0.12),
        ];
        let kept = filter_starter_by_relevance(hits, 0.12);
        let ids: Vec<&str> = kept.iter().map(|h| h.id.as_str()).collect();
        assert_eq!(ids, vec!["strong", "border"]);
    }

    #[test]
    fn starter_relevance_floor_empty_when_all_irrelevant() {
        // When nothing clears the floor, cold-start shows nothing rather than
        // confident-but-irrelevant filler.
        let hits = vec![starter_hit("a", 0.01), starter_hit("b", 0.08)];
        assert!(filter_starter_by_relevance(hits, 0.12).is_empty());
    }

    #[test]
    fn cross_repo_starter_json_carries_scores() {
        let json = cross_repo_starter_json(&[starter_hit("r1", 0.37)]);
        assert!((json[0]["rawScore"].as_f64().expect("rawScore") - 0.37).abs() < 1e-9);
        assert!(json[0]["rankScore"].as_f64().is_some());
    }

    fn scored_chunk(id: &str, score: f64) -> ScoredRuleChunk {
        ScoredRuleChunk {
            skill_id: id.to_owned(),
            content: format!("Rule ID: {id}\nRule Name: {id}\n\nbody"),
            score,
            confidence: 0.7,
        }
    }

    #[test]
    fn explicit_recall_gate_drops_all_weak_local_candidates() {
        // The CLI recall path applies the shared explicit gate on the
        // score-sorted candidate set. A wrong-file / low-relevance query
        // whose candidates are all in the raw fused-RRF noise band must
        // collapse to zero so `recall` renders its zero-match diagnostics
        // instead of ~5 weak filler rules.
        let mut scored = vec![
            scored_chunk("noise-1", 0.004),
            scored_chunk("noise-2", 0.003),
            scored_chunk("noise-3", 0.0015),
        ];
        difflore_core::context::retrieval::apply_explicit_recall_threshold(&mut scored);
        assert!(
            scored.is_empty(),
            "all-weak local candidates must produce a zero-match recall"
        );
    }

    #[test]
    fn explicit_recall_gate_keeps_strong_local_match() {
        // A genuinely strong match (lexically boosted) plus a far-weaker
        // tail: the strong leader survives, the tail is shed — recall is
        // never regressed for a real match.
        let mut scored = vec![scored_chunk("strong", 0.32), scored_chunk("tail", 0.02)];
        difflore_core::context::retrieval::apply_explicit_recall_threshold(&mut scored);
        assert_eq!(
            scored
                .iter()
                .map(|s| s.skill_id.as_str())
                .collect::<Vec<_>>(),
            vec!["strong"],
            "the strong match survives; the far-weaker tail is dropped"
        );
    }

    #[test]
    fn build_local_hits_skips_chunks_with_missing_meta() {
        // A chunk whose skill metadata is absent (stale index entry) must be
        // dropped, matching the MCP `search_rules` soft-skip semantics, so the
        // two recall paths agree instead of surfacing a ghost rule.
        let scored = vec![
            ScoredRuleChunk {
                skill_id: "live-rule".to_owned(),
                content: "Rule Name: Live rule\nAlways assert status".to_owned(),
                score: 0.9,
                confidence: 0.8,
            },
            ScoredRuleChunk {
                skill_id: "ghost-rule".to_owned(),
                content: "Rule Name: Ghost rule\nDeleted skill row".to_owned(),
                score: 0.5,
                confidence: 0.6,
            },
        ];
        let mut metas = std::collections::HashMap::new();
        metas.insert(
            "live-rule".to_owned(),
            SearchSkillMeta {
                file_patterns: vec!["**/*.go".to_owned()],
                source_repo: Some("acme/widgets".to_owned()),
            },
        );

        let hits = build_local_hits(&scored, &metas);

        assert_eq!(hits.len(), 1, "ghost rule with no meta must be skipped");
        assert_eq!(hits[0].id, "live-rule");
        assert!(
            !hits.iter().any(|hit| hit.id == "ghost-rule"),
            "stale chunk must not appear in hits",
        );
    }

    #[test]
    fn local_rules_json_uses_cli_only_shape() {
        let local = LocalRecallResult {
            rules_indexed: 3,
            repo_full_name: Some("acme/widgets".to_owned()),
            file_scope_fallback: true,
            matches: vec![LocalRuleHit {
                id: "rule-1".to_owned(),
                title: "Prefer explicit status assertions".to_owned(),
                preview: "Rule Name: Prefer explicit status assertions".to_owned(),
                bad: Some("assert!(resp.ok())".to_owned()),
                fix: Some("assert_eq!(resp.status(), 200)".to_owned()),
                rank_score: 1.0,
                raw_score: 0.42,
                confidence: 0.7,
                file_patterns: vec!["**/*.go".to_owned()],
                source_repo: Some("acme/widgets".to_owned()),
                body: None,
            }],
        };

        let json = local_rules_json(&local, Some("internal/server.go"));

        assert_eq!(json["rulesIndexed"], 3);
        assert_eq!(json["repoFullName"], "acme/widgets");
        assert_eq!(json["fileScopeFallback"], true);
        assert_eq!(json["results"][0]["skillId"], "rule-1");
        assert_eq!(json["results"][0]["sourceRepo"], "acme/widgets");
        assert_eq!(json["results"][0]["strictFileMatch"], true);

        let no_match = local_rules_json(&local, Some("README.md"));
        assert_eq!(no_match["results"][0]["strictFileMatch"], false);
    }

    #[test]
    fn file_scoped_recall_marks_content_only_matches_as_fallback() {
        let content_only = vec![LocalRuleHit {
            id: "rule-global".to_owned(),
            title: "Review: generic praise".to_owned(),
            preview: "Rule Name: generic praise".to_owned(),
            bad: None,
            fix: None,
            rank_score: 1.0,
            raw_score: 0.2,
            confidence: 0.6,
            file_patterns: Vec::new(),
            source_repo: Some("acme/widgets".to_owned()),
            body: None,
        }];
        let strict = vec![LocalRuleHit {
            id: "rule-scoped".to_owned(),
            title: "Review: scoped".to_owned(),
            preview: "Rule Name: scoped".to_owned(),
            bad: None,
            fix: None,
            rank_score: 1.0,
            raw_score: 0.2,
            confidence: 0.6,
            file_patterns: vec!["src/**/*.rs".to_owned()],
            source_repo: Some("acme/widgets".to_owned()),
            body: None,
        }];

        assert!(content_only_file_scope_fallback(
            &content_only,
            Some("src/lib.rs")
        ));
        assert!(!content_only_file_scope_fallback(
            &strict,
            Some("src/lib.rs")
        ));
        assert!(!content_only_file_scope_fallback(&content_only, None));
    }

    #[test]
    fn zero_match_diagnostics_empty_corpus_points_to_import_and_accept() {
        let local = LocalRecallResult {
            rules_indexed: 0,
            repo_full_name: Some("acme/widgets".to_owned()),
            matches: Vec::new(),
            file_scope_fallback: false,
        };
        let cloud = CloudRecallResult {
            logged_in: false,
            repo_full_name: Some("acme/widgets".to_owned()),
            scope: "personal",
            team_id: None,
            verdicts: Vec::new(),
        };

        let diagnostics =
            build_zero_match_diagnostics(&local, &cloud, "unwrap", Some("src/lib.rs"));
        let json = recall_diagnostics_json(&diagnostics);

        assert!(
            diagnostics
                .possible_causes
                .iter()
                .any(|cause| cause.code == "local_corpus_empty")
        );
        assert_eq!(
            diagnostics.next_steps[0].command.as_deref(),
            Some("difflore import-reviews --max-prs 50")
        );
        assert!(
            diagnostics
                .next_steps
                .iter()
                .any(|step| step.command.as_deref()
                    == Some("difflore import-reviews --max-prs 50"))
        );
        assert!(
            diagnostics
                .next_steps
                .iter()
                .all(|step| step.command.as_deref() != Some("difflore candidates accept --top 3"))
        );
        assert_eq!(json["possibleCauses"][0]["code"], "local_corpus_empty");
    }

    #[test]
    fn zero_match_diagnostics_no_remote_points_to_git_remote_not_import() {
        // A no-GitHub-remote checkout also reports rules_indexed == 0, but the
        // root cause is the missing repo scope, not an empty corpus. It must be
        // diagnosed as such (and steered to the remote), or a local-only user
        // sees a misleading "corpus empty, import reviews" message.
        let local = LocalRecallResult {
            rules_indexed: 0,
            repo_full_name: None,
            matches: Vec::new(),
            file_scope_fallback: false,
        };
        let cloud = CloudRecallResult {
            logged_in: false,
            repo_full_name: None,
            scope: "personal",
            team_id: None,
            verdicts: Vec::new(),
        };

        let diagnostics =
            build_zero_match_diagnostics(&local, &cloud, "handle the 413 error path", None);
        let json = recall_diagnostics_json(&diagnostics);

        assert_eq!(
            diagnostics.possible_causes[0].code, "repo_scope_missing",
            "missing repo scope must be the primary cause for a no-remote checkout"
        );
        assert!(
            diagnostics
                .possible_causes
                .iter()
                .all(|cause| cause.code != "local_corpus_empty"),
            "a no-remote checkout must not be mislabeled as an empty corpus"
        );
        assert!(
            diagnostics
                .next_steps
                .iter()
                .any(|step| step.command.as_deref() == Some("git remote -v")),
            "the actionable step is adding a GitHub remote"
        );
        assert!(
            diagnostics
                .next_steps
                .iter()
                .all(|step| step.command.as_deref() != Some("difflore import-reviews --max-prs 50")),
            "import-reviews is not offered when there is no repo scope to attach rules to"
        );
        assert_eq!(json["possibleCauses"][0]["code"], "repo_scope_missing");
    }

    #[test]
    fn zero_match_diagnostics_explain_file_scope_and_broad_query() {
        let local = LocalRecallResult {
            rules_indexed: 12,
            repo_full_name: Some("acme/widgets".to_owned()),
            matches: Vec::new(),
            file_scope_fallback: false,
        };
        let cloud = CloudRecallResult {
            logged_in: true,
            repo_full_name: Some("acme/widgets".to_owned()),
            scope: "personal",
            team_id: None,
            verdicts: Vec::new(),
        };

        let diagnostics =
            build_zero_match_diagnostics(&local, &cloud, "fix bug", Some("web/app.tsx"));

        assert!(
            diagnostics
                .possible_causes
                .iter()
                .any(|cause| cause.code == "file_pattern_scope")
        );
        assert!(
            diagnostics
                .possible_causes
                .iter()
                .any(|cause| cause.code == "query_too_broad")
        );
        assert!(
            diagnostics
                .next_steps
                .iter()
                .any(|step| { step.command.as_deref() == Some("difflore recall \"fix bug\"") })
        );
        assert!(diagnostics.next_steps.iter().any(|step| {
            step.command
                .as_deref()
                .is_some_and(|command| command.contains("TypeScript review convention"))
        }));
    }

    #[test]
    fn strict_file_pattern_match_requires_real_glob_match() {
        let patterns = vec!["src/**/*.rs".to_owned()];

        assert!(strict_file_pattern_match(&patterns, Some("src/lib.rs")));
        assert!(!strict_file_pattern_match(&patterns, Some("README.md")));
        assert!(!strict_file_pattern_match(&[], Some("src/lib.rs")));
        assert!(!strict_file_pattern_match(&patterns, None));
    }

    #[test]
    fn synthetic_diff_files_extracts_ordered_files_from_diff_intent() {
        let files = synthetic_diff_files(
            "changes in clients/src/main/java/A.java, core/src/test/scala/B.scala",
        );
        assert_eq!(
            files,
            vec![
                "clients/src/main/java/A.java".to_owned(),
                "core/src/test/scala/B.scala".to_owned(),
            ]
        );
        assert!(synthetic_diff_files("fix group coordinator").is_empty());
    }

    #[test]
    fn candidate_fallback_files_skips_primary_and_keeps_diff_order() {
        // Simulates the real `--diff` shape: `build_review_intent_text`
        // emits a structured intent starting with the file path and
        // diff hunks — NOT a `"changes in ..."` string. The fallback
        // must drive off the actual `git diff --name-only` list, so
        // when the primary file produced no strict matches, we still
        // try the other changed files (here `src/bar.rs`).
        let diff_files = vec!["src/foo.rs".to_owned(), "src/bar.rs".to_owned()];
        let structured_intent = "src/foo.rs\n@@ -10,0 +10,2 @@\n+ let x = 1;\n+ let y = 2;\n";

        // The retired `synthetic_diff_files` parser would have returned
        // an empty list for the structured intent, killing the fallback.
        assert!(
            synthetic_diff_files(structured_intent).is_empty(),
            "structured intent never starts with 'changes in '",
        );

        let candidates = candidate_fallback_files(&diff_files, "src/foo.rs");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0], "src/bar.rs");

        // Primary-only diff (one changed file) should produce no
        // candidates — there is no other file to fall back to.
        let only_primary = vec!["src/foo.rs".to_owned()];
        assert!(candidate_fallback_files(&only_primary, "src/foo.rs").is_empty());

        // Multi-file diff preserves `git diff --name-only` order.
        let many = vec![
            "src/a.rs".to_owned(),
            "src/b.rs".to_owned(),
            "src/c.rs".to_owned(),
        ];
        let kept: Vec<&str> = candidate_fallback_files(&many, "src/b.rs");
        assert_eq!(kept, vec!["src/a.rs", "src/c.rs"]);
    }

    #[test]
    fn primary_recall_file_prefers_source_then_manifest_then_config() {
        let source = vec![
            ".changeset/release.md".to_owned(),
            "packages/app/package.json".to_owned(),
            "src/lib.ts".to_owned(),
        ];
        assert_eq!(primary_recall_file(&source).as_deref(), Some("src/lib.ts"));

        let manifest = vec![
            ".changeset/release.md".to_owned(),
            "examples/react/package.json".to_owned(),
            "pnpm-lock.yaml".to_owned(),
        ];
        assert_eq!(
            primary_recall_file(&manifest).as_deref(),
            Some("examples/react/package.json")
        );

        let config = vec![
            "docs/usage.md".to_owned(),
            ".github/workflows/release.yml".to_owned(),
        ];
        assert_eq!(
            primary_recall_file(&config).as_deref(),
            Some(".github/workflows/release.yml")
        );
    }

    #[test]
    fn source_label_prefers_repo_and_pr_number() {
        let verdict = PastVerdict {
            extraction_id: "extraction-1".to_owned(),
            issue_text: "Use errors.Is".to_owned(),
            code_snippet: String::new(),
            status: "accepted".to_owned(),
            reason: None,
            similarity: 0.9,
            created_at: "2026-05-06T00:00:00Z".to_owned(),
            signature: None,
            source_pr_number: Some(42),
            source_pr_title: None,
            source_pr_url: None,
        };

        assert_eq!(
            source_label(&verdict, Some("acme/widgets")).as_deref(),
            Some("acme/widgets#42"),
        );
    }
}
