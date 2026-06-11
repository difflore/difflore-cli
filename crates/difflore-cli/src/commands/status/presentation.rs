//! Text-mode renderer for `status`. `render_text` builds the full human view
//! as a `String`; `super::StatusPayload::print_text` is the only printer.
//!
//! This view is for a person deciding what to do next. Release-gate vocabulary
//! (lane classification, readiness flags, per-outcome linkage, MCP token
//! accounting) lives in the `--json` envelope, not here.

use std::fmt::Write as _;

use crate::style::{self, sym};

use super::queries::{
    LocalAcceptedProof, LocalHeroEvidence, LocalMcpRuleServe, LocalRecallProof, ProvenRuleDrilldown,
};
use super::transform::{CandidatePreview, LaneStatusSummary, NextAction, RepoScopeStatus, plural};

/// One rule that already earned accepted edits -- the most concrete "it works"
/// signal to show in plain language.
fn format_top_rule(rule: &ProvenRuleDrilldown) -> String {
    let mut line = format!(
        "{}: {} accepted edit{}",
        rule.name,
        rule.accepted_fixes,
        plural(rule.accepted_fixes),
    );
    if let Some(repo) = rule
        .source_repo
        .as_deref()
        .map(str::trim)
        .filter(|repo| !repo.is_empty())
    {
        let _ = write!(line, " from {repo}");
    }
    line
}

fn format_local_hero_evidence(hero: &LocalHeroEvidence) -> Vec<String> {
    let scope_note = if hero.scope == "currentRepo" {
        ""
    } else {
        " (best on this machine)"
    };
    let mut lines = vec![format!("best local memory{scope_note}: {}", hero.title)];

    let mut trail = Vec::new();
    if let Some(source) = hero
        .source_repo
        .as_deref()
        .map(str::trim)
        .filter(|source| !source.is_empty())
    {
        trail.push(format!("learned from {source}"));
    }
    let mut target = hero
        .target_repo_full_name
        .as_deref()
        .map(str::trim)
        .filter(|repo| !repo.is_empty())
        .map(|repo| match hero.target_pr_number {
            Some(pr) if pr > 0 => format!("used on {repo}#{pr}"),
            _ => format!("used on {repo}"),
        });
    if let Some(file) = hero
        .sample_file
        .as_deref()
        .map(str::trim)
        .filter(|file| !file.is_empty())
    {
        if let Some(target) = target.as_mut() {
            let _ = write!(target, " | {file}");
        } else {
            target = Some(format!("used on {file}"));
        }
    }
    if let Some(target) = target {
        trail.push(target);
    }
    if !trail.is_empty() {
        lines.push(trail.join(" -> "));
    }

    let real_agent_serves = hero.agent_serves.max(0);
    let mut metrics = format!(
        "{} accepted edit{} | {} signed diff{} | {} recall{} | {} ready for agent{} | ~{} review-minute{} saved",
        hero.accepted_edits,
        plural(hero.accepted_edits),
        hero.signed_diff_proofs,
        plural(hero.signed_diff_proofs),
        hero.recall_events,
        plural(hero.recall_events),
        real_agent_serves,
        plural(real_agent_serves),
        hero.saved_review_minutes,
        plural(hero.saved_review_minutes),
    );
    if hero.strict_agent_serves > 0 {
        let _ = write!(
            metrics,
            " | {} file-matched deliver{}",
            hero.strict_agent_serves,
            if hero.strict_agent_serves == 1 {
                "y"
            } else {
                "ies"
            }
        );
    }
    if let Some(rank) = hero.best_recall_rank.filter(|rank| *rank > 0) {
        let _ = write!(metrics, " | best matched memory #{rank}");
    }
    lines.push(metrics);

    if hero.scope != "currentRepo" {
        lines.push("from another repo; useful as a demo, not current-repo readiness".to_owned());
    }

    lines
}

/// Repo-scoped recall readiness as a single human phrase.
fn format_scoped_recall(scope: &RepoScopeStatus) -> String {
    if !scope.scoped_recall_ready {
        return format!("not ready ({})", scope.reason);
    }
    let scoped = scope.scoped_active_rules;
    let upstream = scope.review_source_active_rules;
    match (
        scoped,
        upstream,
        scope.review_source_repo_full_name.as_deref(),
    ) {
        (0, n, Some(source)) if n > 0 => format!(
            "ready ({} memor{} from {})",
            n,
            if n == 1 { "y" } else { "ies" },
            source
        ),
        (s, n, Some(source)) if n > 0 => format!("ready ({s} scoped + {n} from {source})"),
        (s, _, _) => format!("ready ({} memor{})", s, if s == 1 { "y" } else { "ies" }),
    }
}

fn format_embedding_status(
    diag: &difflore_core::context::EmbeddingDiagnostics,
) -> Option<Vec<String>> {
    if diag.degraded {
        let detail = if diag.vector_lane_available {
            "semantic vectors degraded; recall still uses vectors plus file/keyword matching"
        } else {
            "semantic vectors paused; recall still works with file-pattern + keyword matching"
        };
        return Some(vec![
            detail.to_owned(),
            format!("check: {}", style::cmd("difflore embeddings status")),
            format!("diagnose: {}", style::cmd("difflore doctor --report")),
        ]);
    }
    // No semantic provider: the active embedder is the local SHA1 lexical hash.
    // This is healthy (`degraded == false`, lane available) but recall is
    // keyword-only, so say so to stay consistent with `difflore embeddings
    // status`. Keyed on the `sha1:` active profile so it fires whether or not
    // the per-repo index has been built yet.
    if diag.active_profile.starts_with("sha1:") {
        return Some(vec![
            "semantic recall: local keyword fallback".to_owned(),
            format!(
                "free semantic recall: {}",
                style::cmd("difflore cloud login")
            ),
            format!("advanced/BYOK: {}", style::cmd("difflore embeddings setup")),
        ]);
    }
    if !diag.vector_lane_available {
        return Some(vec!["semantic recall: local keyword fallback".to_owned()]);
    }
    None
}

/// Plain-English readiness for the selected lane(s). Honors `--lane`
/// (`all` | `local-beta` | `production-ga`); the full classification stays in
/// `--json`.
#[cfg(test)]
fn format_readiness(selected_lane: &str, lane_status: &LaneStatusSummary) -> Vec<String> {
    let show_beta = matches!(selected_lane, "all" | "local-beta");
    let show_ga = matches!(selected_lane, "all" | "production-ga");
    let mut lines = Vec::new();
    if show_beta {
        lines.push(
            if lane_status.local_beta.ready {
                "beta: ready | local review memory is working"
            } else {
                "beta: not yet | no usable local review memory yet"
            }
            .to_owned(),
        );
    }
    if show_ga {
        lines.push(if lane_status.production_ga.ready {
            "GA: ready".to_owned()
        } else {
            "GA: not yet | awaiting production release readiness".to_owned()
        });
    }
    lines
}

#[allow(clippy::too_many_arguments)]
pub(super) fn render_text(
    active_rules: i64,
    pending_candidates: i64,
    pending_candidates_for_repo: i64,
    scope: &RepoScopeStatus,
    local_proof: &LocalAcceptedProof,
    local_recall_proof: &LocalRecallProof,
    local_mcp_serves: &LocalMcpRuleServe,
    proven_rule: Option<&ProvenRuleDrilldown>,
    local_hero_evidence: Option<&LocalHeroEvidence>,
    candidate_scope: &str,
    top_candidates: &[CandidatePreview],
    next: &NextAction,
    proof_path: &[String],
    _selected_lane: &str,
    _lane_status: &LaneStatusSummary,
    embedding: &difflore_core::context::EmbeddingDiagnostics,
) -> String {
    let mut out = String::new();
    let bullet = style::pewter(sym::BULLET);

    if scope.repo_full_name.is_none() {
        let _ = writeln!(out, "{}", style::ok("Repository"));
        let _ = writeln!(
            out,
            "  {} no GitHub origin/upstream remote detected",
            style::warn(sym::WARN)
        );
        let _ = writeln!(out);
    }

    // Memory & recall: what a new user needs to understand first.
    let _ = writeln!(out, "{}", style::ok("Memory"));
    let _ = writeln!(
        out,
        "  {bullet} active on this machine: {active_rules} rule{}",
        plural(active_rules)
    );
    let mut drafts = format!("{pending_candidates} pending");
    if scope.repo_full_name.is_some() && pending_candidates_for_repo > 0 {
        let _ = write!(drafts, " ({pending_candidates_for_repo} for this repo)");
    }
    let _ = writeln!(out, "  {bullet} drafts: {drafts}");
    if let Some(repo) = scope.repo_full_name.as_deref() {
        let _ = writeln!(out, "  {bullet} this repo: {repo}");
    }
    if let Some(source) = scope.review_source_repo_full_name.as_deref() {
        let _ = writeln!(out, "  {bullet} review source: {source}");
    }
    let _ = writeln!(out, "  {bullet} recall: {}", format_scoped_recall(scope));
    if let Some(vectors) = format_embedding_status(embedding) {
        for (index, line) in vectors.iter().enumerate() {
            if index == 0 {
                let _ = writeln!(out, "  {bullet} {line}");
            } else {
                let _ = writeln!(out, "    {line}");
            }
        }
    }

    // Value: concrete, human ROI. Link details stay in --json.
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "{}",
        style::ok(&format!("Value (last {}d)", local_proof.window_days))
    );
    let accepted = local_proof.accepted_proof_signatures + local_proof.accepted_hook_outcomes;
    if accepted > 0 {
        // Distinguish the captured closed loop (edit accepted after a recalled
        // memory) from raw accepted edits, so saved-minutes isn't read as fully
        // recall-attributable. Phrase the caveat as the *captured loop* so it
        // doesn't contradict the `best local memory` line below, which
        // attributes edits to a rule by file + source overlap.
        let traced = local_proof.accepted_outcomes_linked_to_prior_recall;
        let traced_note = if traced > 0 {
            format!(" | {traced} via captured recall-to-edit loop")
        } else {
            " | recall-to-edit loop not captured yet".to_owned()
        };
        let _ = writeln!(
            out,
            "  {bullet} {accepted} edit{} accepted | ~{} review-minute{} saved{traced_note}",
            plural(accepted),
            local_proof.estimated_saved_review_minutes,
            plural(local_proof.estimated_saved_review_minutes),
        );
    } else {
        let _ = writeln!(
            out,
            "  {bullet} no accepted edits yet; accept an agent edit or run {} to start",
            style::cmd("difflore fix")
        );
    }
    // Count only non-empty lookups as "serves": a call that returned no rule
    // delivered no memory, so it must not inflate the value summary.
    let agent_serves = local_mcp_serves
        .calls
        .saturating_sub(local_mcp_serves.empty_calls);
    if local_recall_proof.recall_events > 0 || agent_serves > 0 {
        let _ = writeln!(
            out,
            "  {bullet} {} recall{} | {} ready for agent{}",
            local_recall_proof.recall_events,
            plural(local_recall_proof.recall_events),
            agent_serves,
            plural(agent_serves),
        );
    }
    if let Some(rule) = proven_rule {
        let _ = writeln!(out, "  {bullet} top memory: {}", format_top_rule(rule));
    }
    if let Some(hero) = local_hero_evidence {
        for (index, line) in format_local_hero_evidence(hero).into_iter().enumerate() {
            if index == 0 {
                let _ = writeln!(out, "  {bullet} {line}");
            } else {
                let _ = writeln!(out, "    {}", style::pewter(&line));
            }
        }
    }

    // Pending drafts to review (actionable).
    if !top_candidates.is_empty() {
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "{}",
            style::ok(&top_candidates_heading(
                candidate_scope,
                scope,
                pending_candidates_for_repo,
            ))
        );
        if let Some(note) =
            top_candidates_scope_note(candidate_scope, scope, pending_candidates_for_repo)
        {
            let _ = writeln!(out, "  {bullet} {note}");
        }
        for (index, candidate) in top_candidates.iter().enumerate() {
            let _ = writeln!(
                out,
                "  {} {}  {}",
                style::pewter(&format!("{}.", index + 1)),
                style::title(&candidate.name),
                style::pewter(&candidate.id),
            );
            let mut proof_bits = Vec::new();
            if let Some(source) = candidate.source.as_deref() {
                proof_bits.push(format!("source: {source}"));
            }
            if let Some(file) = candidate.file.as_deref() {
                proof_bits.push(format!("file: {file}"));
            }
            if proof_bits.is_empty() {
                proof_bits.push(format!("origin: {}", candidate.origin));
            }
            let _ = writeln!(out, "     {}", style::pewter(&proof_bits.join("  |  ")));
            if !candidate.preview.is_empty() {
                let _ = writeln!(out, "     {}", candidate.preview);
            }
            let _ = writeln!(out, "     add: {}", style::cmd(&candidate.accept_command));
        }
    }

    // Next step.
    let _ = writeln!(out);
    let missing_repo_scope = scope.repo_full_name.is_none();
    let next_command = if missing_repo_scope {
        "git remote -v"
    } else {
        &next.command
    };
    let next_reason = if missing_repo_scope {
        "add or verify a GitHub origin/upstream remote before repo-scoped recall"
    } else {
        &next.reason
    };
    let _ = writeln!(out, "next: {}", style::cmd(next_command));
    let _ = writeln!(out, "  {}", style::pewter(next_reason));
    // `proof_path` leads with `next_command`, already shown by `next:` above;
    // render only what comes after so the sequence doesn't restate its first
    // step.
    let path_commands = if missing_repo_scope {
        proof_path.iter().map(String::as_str).collect::<Vec<_>>()
    } else {
        proof_path
            .iter()
            .map(String::as_str)
            .skip_while(|cmd| *cmd == next_command)
            .collect::<Vec<_>>()
    };
    if !path_commands.is_empty() {
        let _ = writeln!(out, "then:");
        for command in &path_commands {
            let _ = writeln!(out, "  {}", style::cmd(command));
        }
    }

    style::wrap_human_text(&out)
}

fn top_candidates_heading(
    candidate_scope: &str,
    scope: &RepoScopeStatus,
    pending_candidates_for_repo: i64,
) -> String {
    match candidate_scope {
        "currentRepo" => "Pending memory drafts for current repo".to_owned(),
        "all" if scope.repo_full_name.is_some() && pending_candidates_for_repo == 0 => {
            "Pending memory drafts from other repos".to_owned()
        }
        "all" => "Pending memory drafts across repos".to_owned(),
        _ => "Pending memory drafts".to_owned(),
    }
}

fn top_candidates_scope_note(
    candidate_scope: &str,
    scope: &RepoScopeStatus,
    pending_candidates_for_repo: i64,
) -> Option<String> {
    match (
        candidate_scope,
        scope.repo_full_name.as_deref(),
        pending_candidates_for_repo,
    ) {
        ("all", Some(repo), 0) => Some(format!(
            "current repo {repo} has 0 pending memory drafts; these are not counted as ready for this repo"
        )),
        ("all", None, _) => Some(
            "no GitHub origin/upstream remote detected; add one for repo-scoped memory guidance"
                .to_owned(),
        ),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use difflore_core::context::EmbeddingDiagnostics;

    fn repo_scope(repo: Option<&str>) -> RepoScopeStatus {
        RepoScopeStatus {
            repo_full_name: repo.map(ToOwned::to_owned),
            review_source_repo_full_name: None,
            scoped_recall_ready: false,
            scoped_active_rules: 0,
            review_source_active_rules: 0,
            suggested_import_command: None,
            reason: String::new(),
        }
    }

    fn proven_rule() -> ProvenRuleDrilldown {
        ProvenRuleDrilldown {
            rule_id: "rule-1".to_owned(),
            name: "Return 413 for large request bodies".to_owned(),
            source_repo: Some("gin-gonic/gin".to_owned()),
            accepted_fixes: 2,
            accepted_fix_proofs: 2,
            accepted_hook_outcomes: 0,
            accepted_hook_outcomes_linked_to_prior_recall: 0,
            accepted_hook_outcomes_linked_to_recall_or_edit_proof: 0,
            accepted_hook_outcomes_linked_to_rule_recall: 0,
            accepted_hook_outcomes_linked_to_mcp_rule_serve: 0,
            accepted_hook_outcomes_linked_to_edit_attribution: 0,
            sample_file: Some("binding/binding.go".to_owned()),
            explain_command: "difflore status --json".to_owned(),
        }
    }

    #[test]
    fn format_top_rule_is_plain_language() {
        let out = format_top_rule(&proven_rule());
        assert_eq!(
            out,
            "Return 413 for large request bodies: 2 accepted edits from gin-gonic/gin"
        );
        // No linkage internals leak into the human line.
        assert!(!out.contains("memory-use proof"));
        assert!(!out.contains("hook outcome"));
    }

    #[test]
    fn proven_rule_drilldown_json_exposes_recall_aliases() {
        let value = serde_json::to_value(proven_rule()).expect("serializes");
        assert_eq!(value["acceptedHookOutcomesLinkedToPriorRecall"], 0);
        assert_eq!(value["acceptedHookOutcomesLinkedToRecallOrEditProof"], 0);
        assert_eq!(value["acceptedHookOutcomesLinkedToRuleRecall"], 0);
    }

    #[test]
    fn local_hero_evidence_is_value_focused_and_scope_honest() {
        let hero = LocalHeroEvidence {
            scope: "local".to_owned(),
            rule_id: "rule-1".to_owned(),
            title: "Pin GitHub Actions refs to SHAs".to_owned(),
            source_repo: Some("tanstack/router".to_owned()),
            target_repo_full_name: Some("difflore-fixtures/router".to_owned()),
            target_pr_number: Some(4),
            sample_file: Some(".github/workflows/pr.yml".to_owned()),
            accepted_edits: 5,
            signed_diff_proofs: 5,
            recall_events: 7,
            best_recall_rank: Some(1),
            latest_recall_file: Some(".github/workflows/pr.yml".to_owned()),
            agent_serves: 6,
            strict_agent_serves: 6,
            latest_agent_serve_file: Some(".github/workflows/pr.yml".to_owned()),
            saved_review_minutes: 20,
            latest_accepted_at: Some("2026-05-20 00:00:00".to_owned()),
        };

        let lines = format_local_hero_evidence(&hero);
        let out = lines.join("\n");
        assert!(out.contains("best local memory (best on this machine)"));
        assert!(out.contains("learned from tanstack/router"));
        assert!(out.contains("used on difflore-fixtures/router#4"));
        assert!(out.contains("5 accepted edits"));
        assert!(out.contains("5 signed diffs"));
        assert!(out.contains("6 file-matched deliveries"));
        assert!(out.contains("not current-repo readiness"));
        assert!(!out.contains("accepted edit proof"));
        assert!(!out.contains("memory-use proof"));
    }

    #[test]
    fn local_accepted_proof_json_exposes_recall_edit_alias() {
        let value = serde_json::to_value(LocalAcceptedProof {
            window_days: 30,
            recall_lookback_days: 7,
            accepted_proof_signatures: 0,
            accepted_hook_outcomes: 2,
            accepted_outcomes_linked_to_prior_recall: 2,
            accepted_outcomes_linked_to_recall_or_edit_proof: 2,
            accepted_outcomes_linked_to_rule_recall: 0,
            accepted_outcomes_linked_to_mcp_rule_serve: 1,
            accepted_outcomes_linked_to_edit_attribution: 1,
            estimated_saved_review_minutes: 8,
            accepted_and_applied: 145,
            accepted_but_failed: 40,
        })
        .expect("serializes");

        assert_eq!(value["acceptedOutcomesLinkedToPriorRecall"], 2);
        assert_eq!(value["acceptedOutcomesLinkedToRecallOrEditProof"], 2);
        assert_eq!(value["acceptedOutcomesLinkedToMcpRuleServe"], 1);
        assert_eq!(value["acceptedOutcomesLinkedToEditAttribution"], 1);
        // The headline uses applied accepts, not raw accept attempts.
        assert_eq!(value["acceptedAndApplied"], 145);
        assert_eq!(value["acceptedButFailed"], 40);
    }

    #[test]
    fn top_candidates_copy_marks_other_repo_fallback() {
        let scope = repo_scope(Some("acme/app"));

        assert_eq!(
            top_candidates_heading("all", &scope, 0),
            "Pending memory drafts from other repos"
        );
        assert!(
            top_candidates_scope_note("all", &scope, 0)
                .expect("note")
                .contains("current repo acme/app has 0 pending memory drafts")
        );
        assert_eq!(
            top_candidates_heading("currentRepo", &scope, 2),
            "Pending memory drafts for current repo"
        );
        assert!(top_candidates_scope_note("currentRepo", &scope, 2).is_none());
    }

    #[test]
    fn embedding_status_distinguishes_degraded_from_unavailable() {
        let degraded = EmbeddingDiagnostics {
            active_profile: "cloud:new:1536".to_owned(),
            index_profile: Some("cloud:old:1536".to_owned()),
            profile_match: false,
            degraded: true,
            degraded_reason: Some("profile_mismatch".to_owned()),
            vector_lane_available: true,
        };
        let unavailable = EmbeddingDiagnostics {
            active_profile: "sha1:local:128".to_owned(),
            index_profile: Some("cloud:new:1536".to_owned()),
            profile_match: false,
            degraded: true,
            degraded_reason: Some("provider_fallback".to_owned()),
            vector_lane_available: false,
        };

        assert!(
            format_embedding_status(&degraded)
                .expect("degraded status")
                .join("\n")
                .contains("semantic vectors degraded")
        );
        assert!(
            format_embedding_status(&unavailable)
                .expect("unavailable status")
                .join("\n")
                .contains("semantic vectors paused")
        );
    }

    #[test]
    fn embedding_status_reports_keyword_only_for_healthy_sha1_baseline() {
        // No semantic provider configured + a SHA1 index already built: the lane
        // is "healthy" (SHA1 active matches a SHA1 corpus, so not degraded and the
        // vector lane is available), but recall is keyword-only. `status` must say
        // so — silence here is the inconsistency with `embeddings status` (which
        // reports the same local keyword fallback path) that this branch closes.
        let sha1_healthy = EmbeddingDiagnostics {
            active_profile: "sha1:local:128".to_owned(),
            index_profile: Some("sha1:local:128".to_owned()),
            profile_match: true,
            degraded: false,
            degraded_reason: None,
            vector_lane_available: true,
        };
        let line = format_embedding_status(&sha1_healthy)
            .expect("SHA1 baseline must surface a keyword-only status line")
            .join("\n");
        assert!(
            line.contains("semantic recall: local keyword fallback"),
            "must report local keyword fallback: {line}"
        );
        assert!(
            line.contains("free semantic recall: difflore cloud login")
                && line.contains("advanced/BYOK: difflore embeddings setup"),
            "must name the enablement paths: {line}"
        );

        // A matched semantic lane (real provider, index in sync) stays quiet — the
        // note is only for the keyword-only / degraded cases.
        let cloud_healthy = EmbeddingDiagnostics {
            active_profile: "cloud:text-embedding-3-small:1536".to_owned(),
            index_profile: Some("cloud:text-embedding-3-small:1536".to_owned()),
            profile_match: true,
            degraded: false,
            degraded_reason: None,
            vector_lane_available: true,
        };
        assert!(
            format_embedding_status(&cloud_healthy).is_none(),
            "a healthy semantic lane must not emit a status line"
        );
    }

    fn lane_readiness(name: &str, ready: bool) -> super::super::transform::LaneReadiness {
        super::super::transform::LaneReadiness {
            name: name.to_owned(),
            status: "test".to_owned(),
            ready,
            summary: "test summary".to_owned(),
            next_command: String::new(),
            counts_as_production_evidence: false,
            release_ready_influence: "none".to_owned(),
            production_score_delta: 0,
            required_evidence: Vec::new(),
        }
    }

    #[test]
    fn readiness_is_plain_language_and_lane_aware() {
        let summary = LaneStatusSummary {
            selected_lane: "all".to_owned(),
            local_beta: lane_readiness("local-beta", true),
            production_ga: lane_readiness("production-ga", false),
        };

        let all = format_readiness("all", &summary);
        assert_eq!(all.len(), 2);
        assert!(all[0].starts_with("beta:") && all[0].contains("ready"));
        assert!(all[1].starts_with("GA:"));
        for line in &all {
            assert!(!line.contains("countsAsProductionEvidence"));
            assert!(!line.contains("releaseReadyInfluence"));
        }

        assert_eq!(format_readiness("local-beta", &summary).len(), 1);
        assert!(format_readiness("production-ga", &summary)[0].starts_with("GA:"));
    }

    fn proof_with(accepted_signatures: i64, linked: i64, saved: i64) -> LocalAcceptedProof {
        LocalAcceptedProof {
            window_days: 30,
            recall_lookback_days: 7,
            accepted_proof_signatures: accepted_signatures,
            accepted_hook_outcomes: 0,
            accepted_outcomes_linked_to_prior_recall: linked,
            accepted_outcomes_linked_to_recall_or_edit_proof: linked,
            accepted_outcomes_linked_to_rule_recall: linked,
            accepted_outcomes_linked_to_mcp_rule_serve: 0,
            accepted_outcomes_linked_to_edit_attribution: 0,
            estimated_saved_review_minutes: saved,
            accepted_and_applied: 0,
            accepted_but_failed: 0,
        }
    }

    fn render_with_proof(proof: &LocalAcceptedProof) -> String {
        let empty_recall = LocalRecallProof {
            window_days: 30,
            recall_events: 0,
            recalled_rules: 0,
        };
        let empty_serves = LocalMcpRuleServe {
            window_days: 30,
            calls: 0,
            empty_calls: 0,
            rules_served: 0,
            strict_matches: 0,
            estimated_tokens: 0,
        };
        let lane_status = LaneStatusSummary {
            selected_lane: "all".to_owned(),
            local_beta: lane_readiness("local-beta", false),
            production_ga: lane_readiness("production-ga", false),
        };
        let next = NextAction {
            command: "difflore import-reviews".to_owned(),
            reason: "seed local rules from past PR reviews".to_owned(),
        };
        let embedding = EmbeddingDiagnostics {
            active_profile: "sha1:local:128".to_owned(),
            index_profile: None,
            profile_match: true,
            degraded: false,
            degraded_reason: None,
            vector_lane_available: true,
        };

        render_text(
            0,
            0,
            0,
            &repo_scope(None),
            proof,
            &empty_recall,
            &empty_serves,
            None,
            None,
            "none",
            &[],
            &next,
            &[],
            "all",
            &lane_status,
            &embedding,
        )
    }

    #[test]
    fn render_text_guides_new_user_to_first_value() {
        let out = render_with_proof(&proof_with(0, 0, 0));

        // New user is guided toward the first concrete next step. With no
        // GitHub origin detected (`repo_scope(None)`), the genuine first step
        // is to wire up a remote so repo-scoped recall can work at all.
        assert!(out.contains("no accepted edits yet"), "{out}");
        assert!(out.contains("next: "), "{out}");
        assert!(out.contains("git remote -v"), "{out}");
        // Value-first, human framing via the plain section headers.
        assert!(out.contains("Memory") && out.contains("Value"), "{out}");
        // Internal release-gate vocabulary stays out of the human view.
        assert!(!out.contains("countsAsProductionEvidence"), "{out}");
        assert!(!out.contains("Lane boundary"), "{out}");
        assert!(!out.contains("memory-use proof"), "{out}");
    }

    #[test]
    fn render_text_does_not_repeat_missing_repo_next_in_then_path() {
        let empty_proof = proof_with(0, 0, 0);
        let empty_recall = LocalRecallProof {
            window_days: 30,
            recall_events: 0,
            recalled_rules: 0,
        };
        let empty_serves = LocalMcpRuleServe {
            window_days: 30,
            calls: 0,
            empty_calls: 0,
            rules_served: 0,
            strict_matches: 0,
            estimated_tokens: 0,
        };
        let lane_status = LaneStatusSummary {
            selected_lane: "all".to_owned(),
            local_beta: lane_readiness("local-beta", false),
            production_ga: lane_readiness("production-ga", false),
        };
        let next = NextAction {
            command: "difflore import-reviews".to_owned(),
            reason: "seed local memories from past PR reviews".to_owned(),
        };
        let embedding = EmbeddingDiagnostics {
            active_profile: "sha1:local:128".to_owned(),
            index_profile: None,
            profile_match: true,
            degraded: false,
            degraded_reason: None,
            vector_lane_available: true,
        };
        let proof_path = vec![
            "difflore import-reviews".to_owned(),
            "difflore recall --diff".to_owned(),
        ];
        let out = render_text(
            0,
            0,
            0,
            &repo_scope(None),
            &empty_proof,
            &empty_recall,
            &empty_serves,
            None,
            None,
            "none",
            &[],
            &next,
            &proof_path,
            "all",
            &lane_status,
            &embedding,
        );

        assert_eq!(out.matches("git remote -v").count(), 1, "{out}");
        assert!(out.contains("then:"), "{out}");
        assert!(out.contains("difflore import-reviews"), "{out}");
    }

    #[test]
    fn value_line_qualifies_recall_traced_edits() {
        // Captured closed loop: surface the proven count alongside the
        // saved-minutes heuristic so it is not over-read as value.
        let traced = render_with_proof(&proof_with(8, 3, 32));
        assert!(traced.contains("8 edits accepted"), "{traced}");
        assert!(traced.contains("~32 review-minutes saved"), "{traced}");
        assert!(
            traced.contains("3 via captured recall-to-edit loop"),
            "{traced}"
        );

        // No captured loop: do not imply a closed loop the proof does not show,
        // and do not contradict file/source attribution shown elsewhere.
        let untraced = render_with_proof(&proof_with(5, 0, 20));
        assert!(untraced.contains("5 edits accepted"), "{untraced}");
        assert!(
            untraced.contains("recall-to-edit loop not captured yet"),
            "{untraced}"
        );
    }

    #[test]
    fn value_line_excludes_empty_mcp_lookups_from_serves() {
        let recall = LocalRecallProof {
            window_days: 30,
            recall_events: 0,
            recalled_rules: 0,
        };
        let lane_status = LaneStatusSummary {
            selected_lane: "all".to_owned(),
            local_beta: lane_readiness("local-beta", false),
            production_ga: lane_readiness("production-ga", false),
        };
        let next = NextAction {
            command: "difflore import-reviews".to_owned(),
            reason: "seed local rules from past PR reviews".to_owned(),
        };
        let embedding = EmbeddingDiagnostics {
            active_profile: "sha1:local:128".to_owned(),
            index_profile: None,
            profile_match: true,
            degraded: false,
            degraded_reason: None,
            vector_lane_available: true,
        };
        let proof = proof_with(0, 0, 0);
        let render = |serves: &LocalMcpRuleServe| {
            render_text(
                0,
                0,
                0,
                &repo_scope(None),
                &proof,
                &recall,
                serves,
                None,
                None,
                "none",
                &[],
                &next,
                &[],
                "all",
                &lane_status,
                &embedding,
            )
        };

        // All lookups empty -> nothing was delivered -> no inflated agent line.
        let all_empty = LocalMcpRuleServe {
            window_days: 30,
            calls: 5,
            empty_calls: 5,
            rules_served: 0,
            strict_matches: 0,
            estimated_tokens: 0,
        };
        assert!(
            !render(&all_empty).contains("ready for agent"),
            "empty MCP lookups must not be reported as agent-ready: {}",
            render(&all_empty)
        );

        // Five calls, two empty -> three real agent deliveries.
        let mixed = LocalMcpRuleServe {
            window_days: 30,
            calls: 5,
            empty_calls: 2,
            rules_served: 9,
            strict_matches: 3,
            estimated_tokens: 100,
        };
        assert!(
            render(&mixed).contains("3 ready for agents"),
            "{}",
            render(&mixed)
        );
    }
}
