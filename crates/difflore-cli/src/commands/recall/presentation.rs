//! Recall presentation: `--json` payload construction, human/markdown
//! rendering, and diagnostic formatting.
//!
//! The "how do we show it" half of `difflore recall`; data is gathered by the
//! sibling `retrieval` module, shared types and orchestration live in `mod.rs`.

use difflore_core::context::types::PastVerdictScope;

use crate::style::{self, sym};

use super::{
    CloudRecallResult, DiagnosticStep, LocalRecallResult, LocalRuleHit, RecallDiagnostics,
    recall_subject, source_label, strict_pattern_match_any_file, truncate_one_line,
};

pub(super) fn render_cross_repo_starter_human(hits: &[LocalRuleHit], file: &str) {
    if hits.is_empty() {
        return;
    }
    println!();
    println!("{}", style::ok("Starter rules from your other repos"));
    println!(
        "  {}",
        style::pewter(&format!(
            "transferable, file-matched to {file}; not yet scoped to this repo"
        )),
    );
    for (index, hit) in hits.iter().enumerate() {
        let source = hit
            .source_repo
            .as_deref()
            .map(str::trim)
            .filter(|repo| !repo.is_empty())
            .map_or_else(|| "another repo".to_owned(), ToOwned::to_owned);
        println!(
            "  {} {}  {}",
            style::pewter(&format!("{}.", index + 1)),
            style::title(&hit.title),
            style::emerald(&format!("\u{21aa} from {source}")),
        );
        render_hit_examples(hit, "     ");
    }
    println!();
    println!(
        "  {} Make them this repo's own memory: {}",
        style::pewter(sym::TIP),
        style::cmd("difflore import-reviews"),
    );
}

pub(super) fn cross_repo_starter_json(hits: &[LocalRuleHit]) -> serde_json::Value {
    serde_json::Value::Array(
        hits.iter()
            .map(|hit| {
                serde_json::json!({
                    "id": hit.id,
                    "title": hit.title,
                    "sourceRepo": hit.source_repo,
                    "origin": hit.origin,
                    "sourceRank": hit.source_rank,
                    "sourceProvenance": {
                        "origin": hit.origin,
                        "sourceRepo": hit.source_repo,
                        "sourceRank": hit.source_rank,
                    },
                    "filePatterns": hit.file_patterns,
                    "rawScore": hit.raw_score,
                    "rankScore": hit.rank_score,
                    "bad": hit.bad,
                    "fix": hit.fix,
                })
            })
            .collect(),
    )
}

pub(super) fn local_rules_json(
    local: &LocalRecallResult,
    scope_files: &[String],
) -> serde_json::Value {
    serde_json::json!({
        "rulesIndexed": local.rules_indexed,
        "repoFullName": local.repo_full_name,
        "fileScopeFallback": local.file_scope_fallback,
        "trace": recall_trace_json(local),
        "results": local.matches.iter().map(|hit| local_rule_hit_json(hit, scope_files)).collect::<Vec<_>>(),
    })
}

fn recall_trace_json(local: &LocalRecallResult) -> serde_json::Value {
    let trace = &local.trace;
    let intent_gate_dropped = trace
        .candidates_after_exact_merge
        .saturating_sub(trace.candidates_after_intent_gate);
    let relevance_gate_dropped = trace
        .candidates_after_intent_gate
        .saturating_sub(trace.candidates_after_relevance_gate);
    serde_json::json!({
        "repoScopes": trace.repo_scopes,
        "candidateLimit": trace.candidate_limit,
        "candidatesRetrieved": trace.candidates_retrieved,
        "candidatesAfterExactMerge": trace.candidates_after_exact_merge,
        "candidatesAfterIntentGate": trace.candidates_after_intent_gate,
        "candidatesAfterRelevanceGate": trace.candidates_after_relevance_gate,
        "metadataMissingDropped": trace.metadata_missing_dropped,
        "returned": trace.returned,
        "dropReasons": {
            "intentAlignment": intent_gate_dropped,
            "relevanceFloor": relevance_gate_dropped,
            "staleMetadata": trace.metadata_missing_dropped,
        },
    })
}

/// Serialise one recalled rule for `recall --json`. Beyond the headline
/// (title/preview/scores/bad/fix), this emits the FULL rule body when it was
/// hydrated: the rendered code-spec `body`, the structured `examples`
/// (bad/good/description straight from `rule_examples`), and the
/// `check`/`trigger` fields. Before this, an agent consuming recall could only
/// see headlines with the bodies NULL; now it sees the actual team memory.
///
/// `scope_files` is the path-hint scope (the queried file, or every changed
/// file for `--diff`). `strictFileMatch` is retained for schema compatibility
/// and now means the rule's path hints cover ANY of those files.
pub(super) fn local_rule_hit_json(hit: &LocalRuleHit, scope_files: &[String]) -> serde_json::Value {
    let mut value = serde_json::json!({
        "skillId": hit.id,
        "title": hit.title,
        "rankScore": hit.rank_score,
        "rawScore": hit.raw_score,
        "confidence": hit.confidence,
        "filePatterns": hit.file_patterns,
        "sourceRepo": hit.source_repo,
        "origin": hit.origin,
        "sourceRank": hit.source_rank,
        "sourceProvenance": {
            "origin": hit.origin,
            "sourceRepo": hit.source_repo,
            "sourceRank": hit.source_rank,
        },
        "preview": hit.preview,
        "bad": hit.bad,
        "fix": hit.fix,
        "strictFileMatch": strict_pattern_match_any_file(&hit.file_patterns, scope_files),
    });
    if let Some(rendered) = hit.body.as_ref()
        && let Some(object) = value.as_object_mut()
    {
        // `body` is the same code-spec markdown the MCP `get_rules` detail path
        // returns, so an agent that recalls a rule gets the full contract /
        // cases / self-check / provenance — not just a one-line preview.
        object.insert(
            "body".to_owned(),
            serde_json::Value::String(rendered.body.clone()),
        );
        object.insert(
            "origin".to_owned(),
            serde_json::Value::String(rendered.origin.clone()),
        );
        object.insert(
            "check".to_owned(),
            rendered
                .check
                .clone()
                .map_or(serde_json::Value::Null, serde_json::Value::String),
        );
        object.insert(
            "trigger".to_owned(),
            rendered
                .trigger
                .clone()
                .map_or(serde_json::Value::Null, serde_json::Value::String),
        );
        object.insert(
            "examples".to_owned(),
            serde_json::Value::Array(
                rendered
                    .examples
                    .iter()
                    .map(|ex| {
                        serde_json::json!({
                            "badCode": ex.bad_code,
                            "goodCode": ex.good_code,
                            "description": ex.description,
                        })
                    })
                    .collect(),
            ),
        );
    }
    value
}

pub(super) fn recall_diagnostics_json(diagnostics: &RecallDiagnostics) -> serde_json::Value {
    serde_json::json!({
        "summary": diagnostics.summary,
        "possibleCauses": diagnostics.possible_causes.iter().map(|cause| serde_json::json!({
            "code": cause.code,
            "message": cause.message,
        })).collect::<Vec<_>>(),
        "nextSteps": diagnostics.next_steps.iter().map(|step| serde_json::json!({
            "command": step.command,
            "message": step.message,
        })).collect::<Vec<_>>(),
    })
}

pub(super) fn render_zero_match_compact_human(diagnostics: &RecallDiagnostics) {
    let repo_scope_missing = diagnostics
        .possible_causes
        .iter()
        .any(|cause| cause.code == "repo_scope_missing");
    let local_corpus_empty = diagnostics
        .possible_causes
        .iter()
        .any(|cause| cause.code == "local_corpus_empty");
    let message = if repo_scope_missing {
        "No team rules matched because this checkout has no supported origin/upstream git remote."
    } else if local_corpus_empty {
        "No team rules matched because this repo has no local rules yet."
    } else {
        "No team rules matched this query."
    };
    println!("  {} {message}", style::danger(sym::ERR));

    let next = if repo_scope_missing {
        DiagnosticStep {
            command: Some("git remote -v".to_owned()),
            message: "add or check a supported git remote so DiffLore can scope rules to this repo"
                .to_owned(),
        }
    } else if local_corpus_empty {
        DiagnosticStep {
            command: Some("difflore import-reviews --max-prs 50".to_owned()),
            message: "seed local team rules from recent PR reviews".to_owned(),
        }
    } else {
        diagnostics
            .next_steps
            .iter()
            .find(|step| step.command.is_some())
            .cloned()
            .unwrap_or(DiagnosticStep {
                command: Some("difflore status".to_owned()),
                message: "inspect rule readiness".to_owned(),
            })
    };
    println!(
        "  next: {}  {}",
        style::cmd(next.command.as_deref().unwrap_or_default()),
        style::pewter(&next.message),
    );
}

pub(super) fn render_local_recall_human(
    local: &LocalRecallResult,
    intent: &str,
    file: Option<&str>,
    scope_files: &[String],
    verbose: bool,
) {
    if local.matches.is_empty() {
        let subject = recall_subject(intent);
        println!(
            "  {} No local rules matched for {subject}.",
            style::danger(sym::ERR),
        );
        if let Some(file) = file {
            println!(
                "  {} target file: {}",
                style::pewter(sym::BULLET),
                style::pewter(file)
            );
        }
        if local.repo_full_name.is_none() {
            // No repo scope -> empty by design, not an empty corpus. Steer to the
            // remote rather than import-reviews (which can't help without a scope).
            println!(
                "  {} Local recall needs a supported git remote for repo-scoped rules: {}",
                style::pewter(sym::TIP),
                style::cmd("git remote -v"),
            );
        } else if local.rules_indexed == 0 {
            println!(
                "  {} This repo has no local rules yet. Import reviews locally first: {}",
                style::pewter(sym::TIP),
                style::cmd("difflore import-reviews"),
            );
        } else {
            println!(
                "  {} Local memory has {} rule{} for this repo; try a broader query or inspect status: {}",
                style::pewter(sym::TIP),
                local.rules_indexed,
                if local.rules_indexed == 1 { "" } else { "s" },
                style::cmd("difflore status"),
            );
        }
        return;
    }

    println!(
        "{}",
        style::ok(&format!(
            "Top {} local rules for {} | file={} repo={}",
            local.matches.len(),
            recall_subject(intent),
            file.unwrap_or("(none)"),
            local.repo_full_name.as_deref().unwrap_or("(unscoped)"),
        )),
    );
    println!();
    for (index, hit) in local.matches.iter().enumerate() {
        println!(
            "  {} {}  {}  {}",
            style::pewter(&format!("{}.", index + 1)),
            style::title(&hit.title),
            style::emerald(&format!("rank={:.2}", hit.rank_score)),
            style::pewter(&format!("raw={:.3}", hit.raw_score)),
        );
        if strict_pattern_match_any_file(&hit.file_patterns, scope_files) {
            println!(
                "       {} path hint match via {}",
                style::pewter("why:"),
                hit.file_patterns.join(", "),
            );
        }
        let source = hit
            .source_repo
            .as_deref()
            .filter(|repo| !repo.trim().is_empty())
            .map_or_else(
                || "review history".to_owned(),
                |repo| format!("learned from {repo}"),
            );
        println!("       {} {}", style::pewter("source:"), source);
        if let Some(origin) = hit.origin.as_deref() {
            let source_rank = hit
                .source_rank
                .map_or("-".to_owned(), |rank| rank.to_string());
            println!(
                "       {} origin={} source_rank={}",
                style::pewter("provenance:"),
                origin,
                source_rank,
            );
        }
        // The bad→fix pair is the felt value: it makes real recall as sharp as
        // the `difflore try` demo. Show it unconditionally (when the rule body
        // carries examples) — these snippets ARE the memory, not a verbose
        // extra. The full-text preview stays behind --verbose.
        render_hit_examples(hit, "       ");
        if verbose {
            println!(
                "       {} {}",
                style::pewter("preview:"),
                truncate_one_line(&hit.preview, 180),
            );
        }
    }
    println!();
    println!(
        "  {}",
        style::pewter(
            "local SQLite rules/index only; Cloud PR review rules are appended separately when available"
        ),
    );
}

/// Render a hit's bad→fix example pair in the `difflore try` demo style:
/// a `bad` line in danger red and a `fix` line in emerald, each a single
/// concise line. `indent` is the leading whitespace so callers can align the
/// pair under their own list layout. Omits each line that is absent so a rule
/// without examples (or with only one side) degrades cleanly — no empty
/// `bad:`/`fix:` labels.
pub(super) fn render_hit_examples(hit: &LocalRuleHit, indent: &str) {
    if let Some(bad) = hit.bad.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        println!(
            "{indent}{} {}",
            style::pewter("bad"),
            style::danger(&truncate_one_line(bad, 160)),
        );
    }
    if let Some(fix) = hit.fix.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        println!(
            "{indent}{} {}",
            style::pewter("fix"),
            style::emerald(&truncate_one_line(fix, 160)),
        );
    }
}

pub(super) fn render_cloud_recall_human(
    recall: &CloudRecallResult,
    intent: &str,
    file: Option<&str>,
    verbose: bool,
) {
    if !recall.logged_in {
        println!(
            "  {} Local recall is shown above. Sign in to append cloud PR review memory: {}",
            style::pewter(sym::BULLET),
            style::cmd("difflore cloud login"),
        );
        return;
    }
    let Some(repo) = recall.repo_full_name.as_deref() else {
        println!(
            "  {} Local recall is shown above. Cloud PR review memory needs a supported repo remote.",
            style::pewter(sym::BULLET),
        );
        return;
    };
    if recall.verdicts.is_empty() {
        let subject = recall_subject(intent);
        println!(
            "  {} No cloud PR review rules matched for {subject}.",
            style::danger(sym::ERR),
        );
        println!(
            "  {} repo: {} | scope: {}",
            style::pewter(sym::BULLET),
            style::pewter(repo),
            recall.scope,
        );
        if let Some(file) = file {
            println!(
                "  {} target file: {}",
                style::pewter(sym::BULLET),
                style::pewter(file)
            );
        }
        let seed_hint = if recall.scope == PastVerdictScope::Team.as_str() {
            "Import PR reviews locally first; sync later if you want Cloud recall"
        } else {
            "Import PR reviews locally to seed Free personal recall"
        };
        println!(
            "  {} {}: {}",
            style::pewter(sym::TIP),
            seed_hint,
            style::cmd("difflore import-reviews --max-prs 50"),
        );
        return;
    }

    println!(
        "{}",
        style::ok(&format!(
            "Top {} cloud review memories for {} | file={} repo={} scope={}",
            recall.verdicts.len(),
            recall_subject(intent),
            file.unwrap_or("(none)"),
            repo,
            recall.scope,
        )),
    );
    println!();
    for (index, verdict) in recall.verdicts.iter().enumerate() {
        let source = source_label(verdict, Some(repo)).unwrap_or_else(|| repo.to_owned());
        println!(
            "  {} {}  {}",
            style::pewter(&format!("{}.", index + 1)),
            style::title(&truncate_one_line(&verdict.issue_text, 96)),
            style::emerald(&format!("similarity={:.2}", verdict.similarity)),
        );
        println!("       {} {}", style::pewter("source:"), source);
        if let Some(reason) = verdict.reason.as_deref().map(str::trim)
            && !reason.is_empty()
        {
            println!(
                "       {} {}",
                style::pewter("reason:"),
                truncate_one_line(reason, 160),
            );
        }
        if verbose {
            println!(
                "       {} {}",
                style::pewter("code:"),
                truncate_one_line(&verdict.code_snippet, 180),
            );
        }
    }
    println!();
    println!(
        "  {}",
        style::pewter(
            "cloud ranked these memories; the CLI only supplied intent, file, and repo context",
        ),
    );
}
