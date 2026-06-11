//! Pure transforms and in-memory views used by `status`.
//!
//! These functions are side-effect free: no I/O, no DB. They depend on the
//! DTOs in `super::queries` for inputs but never touch SQL.

use std::collections::HashMap;

use super::queries::{
    LocalAcceptedProof, LocalMcpRuleServe, LocalRecallProof, ValueLoopAcceptedRow,
    ValueLoopEvidence,
};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RepoScopeStatus {
    pub(super) repo_full_name: Option<String>,
    pub(super) review_source_repo_full_name: Option<String>,
    pub(super) scoped_recall_ready: bool,
    pub(super) scoped_active_rules: i64,
    pub(super) review_source_active_rules: i64,
    pub(super) suggested_import_command: Option<String>,
    pub(super) reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct NextAction {
    pub(super) command: String,
    pub(super) reason: String,
}

#[derive(Debug, Clone)]
pub(super) struct ProvenRuleCandidate {
    pub(super) rule_id: String,
    pub(super) name: String,
    pub(super) source_repo: Option<String>,
    pub(super) accepted_fix_proofs: i64,
    pub(super) accepted_hook_outcomes: i64,
    pub(super) accepted_hook_outcomes_linked_to_prior_recall: i64,
    pub(super) accepted_hook_outcomes_linked_to_recall_or_edit_proof: i64,
    pub(super) accepted_hook_outcomes_linked_to_rule_recall: i64,
    pub(super) accepted_hook_outcomes_linked_to_mcp_rule_serve: i64,
    pub(super) accepted_hook_outcomes_linked_to_edit_attribution: i64,
    pub(super) sample_file: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct LocalValueLoopStatus {
    pub(super) stage: String,
    pub(super) repo_scoped_rules_ready: bool,
    pub(super) repo_candidates_ready: bool,
    pub(super) local_recall_proof_ready: bool,
    pub(super) mcp_agent_recall_proof_ready: bool,
    pub(super) accepted_edit_proof_ready: bool,
    pub(super) auditable_value_loop_ready: bool,
    pub(super) saved_review_minutes: i64,
    pub(super) buyer_evidence: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct LaneReadiness {
    pub(super) name: String,
    pub(super) status: String,
    pub(super) ready: bool,
    pub(super) summary: String,
    pub(super) next_command: String,
    pub(super) counts_as_production_evidence: bool,
    pub(super) release_ready_influence: String,
    pub(super) production_score_delta: i64,
    pub(super) required_evidence: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct LaneStatusSummary {
    pub(super) selected_lane: String,
    pub(super) local_beta: LaneReadiness,
    pub(super) production_ga: LaneReadiness,
}

#[derive(Debug, Clone)]
pub(super) struct ValueLoopAcceptedCandidate {
    pub(super) rule_id: String,
    pub(super) name: String,
    pub(super) source_repo: Option<String>,
    pub(super) target_repo_full_name: Option<String>,
    pub(super) target_pr_number: Option<i64>,
    pub(super) accepted_file_path: Option<String>,
    pub(super) accepted_at: String,
    pub(super) diff_signature: Option<String>,
    pub(super) source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CandidatePreview {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) origin: String,
    pub(super) captured_at: String,
    pub(super) source: Option<String>,
    pub(super) file: Option<String>,
    pub(super) comment_url: Option<String>,
    pub(super) preview: String,
    pub(super) accept_command: String,
    pub(super) explain_command: String,
}

pub(super) struct NextActionInputs<'a> {
    pub(super) active_rules: i64,
    pub(super) pending_candidates: i64,
    pub(super) pending_candidates_for_repo: i64,
    pub(super) cloud_logged_in: bool,
    pub(super) scope: &'a RepoScopeStatus,
    pub(super) local_proof: &'a LocalAcceptedProof,
    pub(super) local_recall_proof: &'a LocalRecallProof,
    pub(super) local_mcp_serves: &'a LocalMcpRuleServe,
}

impl From<ValueLoopAcceptedRow> for ValueLoopAcceptedCandidate {
    fn from(row: ValueLoopAcceptedRow) -> Self {
        Self {
            rule_id: row.rule_id,
            name: row.name,
            source_repo: row.source_repo,
            target_repo_full_name: row.target_repo_full_name,
            target_pr_number: row.target_pr_number,
            accepted_file_path: row.accepted_file_path,
            accepted_at: row.accepted_at,
            diff_signature: row.diff_signature,
            source: "local_fix_outcome".to_owned(),
        }
    }
}

pub(super) const fn plural(count: i64) -> &'static str {
    if count == 1 { "" } else { "s" }
}

pub(super) fn source_repo_for_value_loop(
    proof: &difflore_core::skills::CandidateSourceProof,
    fallback: Option<&str>,
) -> Option<String> {
    proof
        .source
        .as_deref()
        .and_then(|source| source.split_once('#').map(|(repo, _)| repo))
        .or_else(|| proof.comment_url.as_deref().and_then(github_repo_from_url))
        .or(fallback)
        .map(str::trim)
        .filter(|repo| !repo.is_empty())
        .map(ToOwned::to_owned)
}

pub(super) fn pr_number_for_value_loop(
    proof: &difflore_core::skills::CandidateSourceProof,
) -> Option<i64> {
    proof
        .source
        .as_deref()
        .and_then(|source| source.rsplit_once('#').map(|(_, pr)| pr))
        .and_then(parse_positive_i64_prefix)
        .or_else(|| {
            proof
                .comment_url
                .as_deref()
                .and_then(github_pr_number_from_url)
        })
}

fn github_repo_from_url(url: &str) -> Option<&str> {
    let marker = "github.com/";
    let after_host = url.split_once(marker)?.1;
    let mut parts = after_host.split('/');
    let owner = parts.next()?.trim();
    let repo = parts.next()?.trim();
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    let end = owner.len() + 1 + repo.len();
    after_host.get(..end)
}

fn github_pr_number_from_url(url: &str) -> Option<i64> {
    let after_pull = url.split_once("/pull/")?.1;
    parse_positive_i64_prefix(after_pull)
}

fn parse_positive_i64_prefix(value: &str) -> Option<i64> {
    let digits: String = value.chars().take_while(char::is_ascii_digit).collect();
    let number = digits.parse::<i64>().ok()?;
    (number > 0).then_some(number)
}

pub(super) fn value_loop_files_match(
    accepted_file: Option<&str>,
    recall_file: Option<&str>,
    mcp_file: Option<&str>,
) -> bool {
    fn eq_path(a: &str, b: &str) -> bool {
        a.replace('\\', "/")
            .eq_ignore_ascii_case(&b.replace('\\', "/"))
    }

    if let (Some(accepted), Some(recall)) = (accepted_file, recall_file)
        && !eq_path(accepted, recall)
    {
        return false;
    }
    if let (Some(accepted), Some(mcp)) = (accepted_file, mcp_file)
        && !eq_path(accepted, mcp)
    {
        return false;
    }
    true
}

pub(super) fn value_loop_times_causal(
    recalled_at: &str,
    served_at: &str,
    accepted_at: &str,
) -> bool {
    let Some(recall) = parse_value_loop_time(recalled_at) else {
        return false;
    };
    let Some(serve) = parse_value_loop_time(served_at) else {
        return false;
    };
    let Some(accepted) = parse_value_loop_time(accepted_at) else {
        return false;
    };
    recall <= serve && serve <= accepted
}

fn parse_value_loop_time(value: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .ok()
        .or_else(|| {
            chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S")
                .ok()
                .map(|dt| dt.and_utc())
        })
}

pub(super) fn local_value_loop_status(
    scope: &RepoScopeStatus,
    pending_candidates_for_repo: i64,
    recall: &LocalRecallProof,
    mcp_serves: &LocalMcpRuleServe,
    accepted: &LocalAcceptedProof,
    value_loop_evidence: Option<&ValueLoopEvidence>,
) -> LocalValueLoopStatus {
    let accepted_total = accepted.accepted_proof_signatures + accepted.accepted_hook_outcomes;
    let auditable_value_loop_ready = value_loop_evidence.is_some();
    let repo_scoped_rules_ready = scope.scoped_recall_ready;
    let accepted_after_memory_use = accepted.accepted_outcomes_linked_to_prior_recall > 0;
    let accepted_edit_proof_ready =
        repo_scoped_rules_ready && accepted_total > 0 && accepted_after_memory_use;
    let local_recall_proof_ready = repo_scoped_rules_ready && recall.recall_events > 0;
    let mcp_agent_recall_proof_ready = repo_scoped_rules_ready && mcp_serves.strict_matches > 0;
    let repo_candidates_ready = pending_candidates_for_repo > 0;

    let (stage, buyer_evidence) = if let Some(evidence) = value_loop_evidence {
        (
            "auditable_value_loop_ready",
            format!(
                "{} from {}#{} matched memory #{} and became an accepted edit (~{} review minute{} saved locally)",
                evidence.accepted_rule.title,
                evidence.imported_review.source_repo,
                evidence.imported_review.pr_number,
                evidence.recall.rank,
                evidence.saved_review_minutes,
                plural(evidence.saved_review_minutes),
            ),
        )
    } else if accepted_edit_proof_ready {
        let recall_breakdown = crate::support::util::format_recall_edit_proof_breakdown(
            accepted.accepted_outcomes_linked_to_rule_recall,
            accepted.accepted_outcomes_linked_to_mcp_rule_serve,
            accepted.accepted_outcomes_linked_to_edit_attribution,
        );
        (
            "accepted_edit_proof_ready",
            format!(
                "{} accepted edit{} ({} after prior memory use{} within {}d, ~{} review minute{} saved locally)",
                accepted_total,
                plural(accepted_total),
                accepted.accepted_outcomes_linked_to_prior_recall,
                recall_breakdown,
                accepted.recall_lookback_days,
                accepted.estimated_saved_review_minutes,
                plural(accepted.estimated_saved_review_minutes),
            ),
        )
    } else if mcp_agent_recall_proof_ready {
        (
            "agent_recall_seen",
            mcp_agent_recall_buyer_evidence(mcp_serves),
        )
    } else if repo_scoped_rules_ready && local_recall_proof_ready {
        (
            "local_recall_seen",
            format!(
                "{} local recall event{} recorded; preview fixes, then accept a matching agent edit",
                recall.recall_events,
                plural(recall.recall_events),
            ),
        )
    } else if accepted_total > 0 {
        (
            "accepted_edit_seen",
            format!(
                "{} accepted edit{} recorded for this repo, but 0 followed memory use within {}d; preview recalled memories before accepting edits",
                accepted_total,
                plural(accepted_total),
                accepted.recall_lookback_days,
            ),
        )
    } else if repo_scoped_rules_ready {
        (
            "repo_rules_ready",
            "repo-scoped memories exist; preview them against this diff".to_owned(),
        )
    } else if repo_candidates_ready {
        (
            "repo_candidates_pending",
            format!(
                "{pending_candidates_for_repo} rule candidate{} drafted for this repo; accept one to enable recall",
                plural(pending_candidates_for_repo),
            ),
        )
    } else if scope.review_source_repo_full_name.is_some() {
        (
            "fork_memory_missing",
            "attach upstream review rules to this fork, then test recall".to_owned(),
        )
    } else {
        (
            "repo_memory_missing",
            "import this repo's review rules before testing recall".to_owned(),
        )
    };

    LocalValueLoopStatus {
        stage: stage.to_owned(),
        repo_scoped_rules_ready,
        repo_candidates_ready,
        local_recall_proof_ready,
        mcp_agent_recall_proof_ready,
        accepted_edit_proof_ready,
        auditable_value_loop_ready,
        saved_review_minutes: accepted.estimated_saved_review_minutes,
        buyer_evidence,
    }
}

pub(super) fn mcp_agent_recall_buyer_evidence(serves: &LocalMcpRuleServe) -> String {
    if serves.strict_matches > 0 {
        return format!(
            "{} file-scoped memory match{} ready for agents; preview fixes, then accept a matching agent edit",
            serves.strict_matches,
            if serves.strict_matches == 1 { "" } else { "es" },
        );
    }
    format!(
        "{} memor{} ready for agents; preview fixes, then accept a matching agent edit",
        serves.rules_served,
        if serves.rules_served == 1 { "y" } else { "ies" },
    )
}

pub(super) fn lane_status_summary(
    selected_lane: &str,
    value_loop: &LocalValueLoopStatus,
    next: &NextAction,
) -> LaneStatusSummary {
    LaneStatusSummary {
        selected_lane: selected_lane.to_owned(),
        local_beta: local_beta_lane_readiness(value_loop, next),
        production_ga: production_ga_lane_readiness(),
    }
}

fn local_beta_lane_readiness(
    value_loop: &LocalValueLoopStatus,
    next: &NextAction,
) -> LaneReadiness {
    let ready = value_loop.auditable_value_loop_ready
        || value_loop.accepted_edit_proof_ready
        || value_loop.mcp_agent_recall_proof_ready
        || value_loop.local_recall_proof_ready;
    let status = if value_loop.auditable_value_loop_ready {
        "auditable_value_loop_ready"
    } else if value_loop.accepted_edit_proof_ready {
        "accepted_edit_proof_ready"
    } else if value_loop.mcp_agent_recall_proof_ready {
        "agent_recall_seen"
    } else if value_loop.local_recall_proof_ready {
        "local_recall_seen"
    } else if value_loop.repo_scoped_rules_ready {
        "repo_rules_ready"
    } else if value_loop.repo_candidates_ready {
        "repo_candidates_pending"
    } else {
        "needs_review_memory"
    };
    let summary = if ready {
        format!(
            "Local/design-partner beta lane has usable rule-recall signal: {}. This does not count toward production GA.",
            value_loop.buyer_evidence
        )
    } else {
        format!(
            "Local/design-partner beta lane is not ready yet: {}.",
            value_loop.buyer_evidence
        )
    };
    LaneReadiness {
        name: "local-beta".to_owned(),
        status: status.to_owned(),
        ready,
        summary,
        next_command: next.command.clone(),
        counts_as_production_evidence: false,
        release_ready_influence: "none".to_owned(),
        production_score_delta: 0,
        required_evidence: vec![
            "repo-scoped review rules imported from PR reviews".to_owned(),
            "local recall or agent delivery that matches the current repo/file".to_owned(),
            "accepted edit after prior rule use for stronger beta readiness".to_owned(),
        ],
    }
}

fn production_ga_lane_readiness() -> LaneReadiness {
    LaneReadiness {
        name: "production-ga".to_owned(),
        status: "blocked_external_release_gates_required".to_owned(),
        ready: false,
        summary: "Local `difflore status` cannot approve production GA; GA requires production-ready release artifacts from DiffLore Cloud.".to_owned(),
        next_command: "difflore doctor --report".to_owned(),
        counts_as_production_evidence: false,
        release_ready_influence: "none".to_owned(),
        production_score_delta: 0,
        required_evidence: vec![
            "30 counted production accepted difflore fix edits".to_owned(),
            "3 real production tenants".to_owned(),
            "10 counted production accepted edits per tenant".to_owned(),
            "6 current third-party confirmations".to_owned(),
            "release gates pass in the same readiness window".to_owned(),
            "Claude second-layer audit returns APPROVE_10=true on that same bundle".to_owned(),
        ],
    }
}

pub(super) fn count_rules_for_repo(
    rules: &[difflore_core::domain::models::SkillRecord],
    source_repos: &HashMap<String, Option<String>>,
    repo: Option<&str>,
) -> i64 {
    let Some(repo) = repo.map(normalize_repo).filter(|repo| !repo.is_empty()) else {
        return 0;
    };

    rules
        .iter()
        .filter(|rule| {
            rule_repo_scope(rule, source_repos)
                .as_deref()
                .map(normalize_repo)
                .as_deref()
                == Some(repo.as_str())
        })
        .count() as i64
}

fn rule_repo_scope(
    rule: &difflore_core::domain::models::SkillRecord,
    source_repos: &HashMap<String, Option<String>>,
) -> Option<String> {
    source_repos
        .get(&rule.id)
        .and_then(|repo| repo.as_deref())
        .map(str::trim)
        .filter(|repo| !repo.is_empty())
        .map(ToOwned::to_owned)
}

pub(super) fn normalize_repo(repo: &str) -> String {
    repo.trim().trim_end_matches(".git").to_ascii_lowercase()
}

pub(super) fn repo_scope_status(
    repo_full_name: Option<String>,
    review_source_repo_full_name: Option<String>,
    scoped_active_rules: i64,
    review_source_active_rules: i64,
) -> RepoScopeStatus {
    let review_source_ready =
        review_source_repo_full_name.is_some() && review_source_active_rules > 0;
    let scoped_recall_ready =
        repo_full_name.is_some() && (scoped_active_rules > 0 || review_source_ready);
    let suggested_import_command = if scoped_recall_ready {
        None
    } else {
        repo_full_name.as_deref().map(|repo| {
            if let Some(source_repo) = review_source_repo_full_name.as_deref() {
                format!("difflore import-reviews --repo {repo} --from-upstream {source_repo}")
            } else {
                format!("difflore import-reviews --repo {repo}")
            }
        })
    };
    let reason = match (
        repo_full_name.as_deref(),
        review_source_repo_full_name.as_deref(),
        scoped_active_rules,
        review_source_active_rules,
    ) {
        (None, _, _, _) => "no GitHub origin/upstream remote was detected".to_owned(),
        (Some(_repo), Some(source_repo), 0, source_count) if source_count > 0 => format!(
            "{source_count} upstream active memor{} from {source_repo} are available to this fork",
            if source_count == 1 { "y" } else { "ies" }
        ),
        (Some(_), _, 0, _) => "no active memory is scoped to this repo yet".to_owned(),
        (Some(repo), _, count, _) => {
            format!(
                "{count} active memor{} scoped to {repo}",
                if count == 1 { "y" } else { "ies" }
            )
        }
    };
    RepoScopeStatus {
        repo_full_name,
        review_source_repo_full_name,
        scoped_recall_ready,
        scoped_active_rules,
        review_source_active_rules,
        suggested_import_command,
        reason,
    }
}

pub(super) fn next_action(inputs: &NextActionInputs<'_>) -> NextAction {
    let &NextActionInputs {
        active_rules,
        pending_candidates,
        pending_candidates_for_repo,
        cloud_logged_in,
        scope,
        local_proof,
        local_recall_proof,
        local_mcp_serves,
    } = inputs;
    if local_proof.accepted_outcomes_linked_to_prior_recall > 0 {
        if !cloud_logged_in {
            return NextAction {
                command: "difflore cloud login".to_owned(),
                reason: "log in before uploading local accepted edits".to_owned(),
            };
        }
        return NextAction {
            command: "difflore cloud team --json".to_owned(),
            reason: "confirm your team workspace before uploading accepted edits".to_owned(),
        };
    }

    if active_rules == 0 && pending_candidates_for_repo > 0 {
        return NextAction {
            command: draft_review_command(scope),
            reason: "review pending drafts into active local rules".to_owned(),
        };
    }

    if scope.scoped_recall_ready {
        if local_recall_proof.recall_events > 0 || local_mcp_serves.strict_matches > 0 {
            return NextAction {
                command: "difflore fix --preview".to_owned(),
                reason: "turn recalled memories into accepted edits".to_owned(),
            };
        }
        return NextAction {
            command: "difflore recall --diff".to_owned(),
            reason: "preview the rules agents would see on this change".to_owned(),
        };
    }

    if active_rules > 0 && scope.repo_full_name.is_none() {
        return NextAction {
            command: "difflore recall \"review this change\"".to_owned(),
            reason: "preview local rules; add a GitHub origin remote for repo-scoped recall"
                .to_owned(),
        };
    }

    if pending_candidates_for_repo > 0 {
        return NextAction {
            command: draft_review_command(scope),
            reason: "review pending drafts into active local rules before testing recall"
                .to_owned(),
        };
    }

    if scope.repo_full_name.is_some() {
        return NextAction {
            command: scope
                .suggested_import_command
                .clone()
                .unwrap_or_else(|| "difflore import-reviews".to_owned()),
            reason: if scope.review_source_repo_full_name.is_some() {
                "create local rules from upstream PR reviews and attach them to this fork"
                    .to_owned()
            } else {
                "create local review rules without Cloud".to_owned()
            },
        };
    }

    if pending_candidates > 0 && scope.repo_full_name.is_none() {
        return NextAction {
            command: "difflore drafts review".to_owned(),
            reason: "review pending drafts; add a GitHub origin remote for repo-scoped guidance"
                .to_owned(),
        };
    }

    NextAction {
        command: "difflore import-reviews".to_owned(),
        reason: "seed local rules from past PR reviews".to_owned(),
    }
}

fn draft_review_command(scope: &RepoScopeStatus) -> String {
    scope.repo_full_name.as_ref().map_or_else(
        || "difflore drafts review".to_owned(),
        |repo| format!("difflore drafts review --repo {repo}"),
    )
}

pub(super) fn proof_path_commands(next: &NextAction, cloud_logged_in: bool) -> Vec<String> {
    const CLOUD_LOGIN_COMMAND: &str = "difflore cloud login";
    const CLOUD_PROOF_READINESS_COMMAND: &str = "difflore cloud team --json";
    const CLOUD_IMPACT_COMMAND: &str = "difflore cloud impact";

    fn cloud_proof_path(cloud_logged_in: bool, include_impact: bool) -> Vec<String> {
        if cloud_logged_in {
            let mut path = vec![CLOUD_PROOF_READINESS_COMMAND.to_owned()];
            if include_impact {
                path.push(CLOUD_IMPACT_COMMAND.to_owned());
            }
            path
        } else {
            vec![
                CLOUD_LOGIN_COMMAND.to_owned(),
                CLOUD_PROOF_READINESS_COMMAND.to_owned(),
            ]
        }
    }

    let command = next.command.as_str();
    let mut path = if matches!(
        command,
        CLOUD_IMPACT_COMMAND | CLOUD_PROOF_READINESS_COMMAND
    ) {
        cloud_proof_path(cloud_logged_in, true)
    } else if command == CLOUD_LOGIN_COMMAND {
        cloud_proof_path(cloud_logged_in, false)
    } else {
        vec![command.to_owned()]
    };

    if command == CLOUD_IMPACT_COMMAND
        || command == CLOUD_PROOF_READINESS_COMMAND
        || command == CLOUD_LOGIN_COMMAND
    {
        return path;
    }

    if command.starts_with("difflore import-reviews") || command.starts_with("difflore drafts ") {
        path.push("difflore recall --diff".to_owned());
        path.push("difflore fix --preview".to_owned());
    }

    path.extend(cloud_proof_path(cloud_logged_in, cloud_logged_in));
    path
}

pub(super) fn candidate_previews(
    candidates: &[difflore_core::skills::CandidateRule],
    limit: usize,
) -> Vec<CandidatePreview> {
    candidates
        .iter()
        .take(limit)
        .map(candidate_preview)
        .collect()
}

fn candidate_preview(candidate: &difflore_core::skills::CandidateRule) -> CandidatePreview {
    let source_proof = difflore_core::skills::parse_candidate_source_proof(&candidate.description);
    CandidatePreview {
        id: candidate.id.clone(),
        name: candidate.name.clone(),
        origin: candidate.origin.clone(),
        captured_at: candidate.installed_at.clone(),
        source: source_proof.as_ref().and_then(|proof| proof.source.clone()),
        file: source_proof.as_ref().and_then(|proof| proof.file.clone()),
        comment_url: source_proof
            .as_ref()
            .and_then(|proof| proof.comment_url.clone()),
        preview: candidate_body_preview(&candidate.description),
        accept_command: format!("difflore drafts approve {}", candidate.id),
        explain_command: format!("difflore drafts show {}", candidate.id),
    }
}

fn candidate_body_preview(description: &str) -> String {
    let body = description
        .split_once("Reviewer said:")
        .map_or(description, |(_, tail)| tail);
    let mut text = body
        .lines()
        .map(str::trim)
        .filter(|line| {
            !line.is_empty()
                && !line.starts_with("Imported from")
                && !line.starts_with("Keep as")
                && !line.starts_with("Source:")
                && !line.starts_with("Comment:")
                && !line.starts_with("File:")
        })
        .collect::<Vec<_>>()
        .join(" ");
    if text.is_empty() {
        description
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .clone_into(&mut text);
    }
    truncate_chars(&text, 140)
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let head: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{head}...")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn next_action_for_test(
        active_rules: i64,
        pending_candidates: i64,
        pending_candidates_for_repo: i64,
        scope: &RepoScopeStatus,
        local_proof: &LocalAcceptedProof,
    ) -> NextAction {
        next_action(&NextActionInputs {
            active_rules,
            pending_candidates,
            pending_candidates_for_repo,
            cloud_logged_in: false,
            scope,
            local_proof,
            local_recall_proof: &empty_recall_proof(),
            local_mcp_serves: &empty_mcp_serves(),
        })
    }

    fn scope(repo: Option<&str>, scoped_active_rules: i64) -> RepoScopeStatus {
        repo_scope_status(repo.map(ToOwned::to_owned), None, scoped_active_rules, 0)
    }

    fn fork_scope(
        repo: &str,
        upstream: &str,
        scoped_active_rules: i64,
        upstream_active_rules: i64,
    ) -> RepoScopeStatus {
        repo_scope_status(
            Some(repo.to_owned()),
            Some(upstream.to_owned()),
            scoped_active_rules,
            upstream_active_rules,
        )
    }

    fn rule(
        id: &str,
        owner: Option<&str>,
        repo: Option<&str>,
    ) -> difflore_core::domain::models::SkillRecord {
        difflore_core::domain::models::SkillRecord {
            id: id.to_owned(),
            name: "Rule".to_owned(),
            source: "local".to_owned(),
            directory: "rule".to_owned(),
            version: "1.0.0".to_owned(),
            description: "body".to_owned(),
            r#type: "review_standard".to_owned(),
            engines: vec![],
            tags: vec![],
            trigger: None,
            check_prompt: None,
            repo_owner: owner.map(ToOwned::to_owned),
            repo_name: repo.map(ToOwned::to_owned),
            repo_branch: None,
            readme_url: None,
            enabled_for_codex: true,
            enabled_for_claude: true,
            enabled_for_gemini: true,
            enabled_for_cursor: true,
            installed_at: "2026-05-06 00:00:00".to_owned(),
            updated_at: "2026-05-06 00:00:00".to_owned(),
            enforcement: None,
            origin: "manual".to_owned(),
        }
    }

    fn empty_local_proof() -> LocalAcceptedProof {
        LocalAcceptedProof {
            window_days: 30,
            recall_lookback_days: 7,
            accepted_proof_signatures: 0,
            accepted_hook_outcomes: 0,
            accepted_outcomes_linked_to_prior_recall: 0,
            accepted_outcomes_linked_to_recall_or_edit_proof: 0,
            accepted_outcomes_linked_to_rule_recall: 0,
            accepted_outcomes_linked_to_mcp_rule_serve: 0,
            accepted_outcomes_linked_to_edit_attribution: 0,
            estimated_saved_review_minutes: 0,
            accepted_and_applied: 0,
            accepted_but_failed: 0,
        }
    }

    fn empty_recall_proof() -> LocalRecallProof {
        LocalRecallProof {
            window_days: 30,
            recall_events: 0,
            recalled_rules: 0,
        }
    }

    fn empty_mcp_serves() -> LocalMcpRuleServe {
        LocalMcpRuleServe {
            window_days: 30,
            calls: 0,
            empty_calls: 0,
            rules_served: 0,
            strict_matches: 0,
            estimated_tokens: 0,
        }
    }

    #[test]
    fn count_rules_for_repo_uses_canonical_source_repo_only() {
        let rules = vec![
            rule("r1", Some("wrong"), Some("repo")),
            rule("r2", Some("acme"), Some("app")),
        ];
        let mut source_repos = HashMap::new();
        source_repos.insert("r1".to_owned(), Some("Acme/App".to_owned()));

        assert_eq!(
            count_rules_for_repo(&rules, &source_repos, Some("acme/app")),
            1
        );
    }

    #[test]
    fn repo_scope_status_requires_repo_and_scoped_rules() {
        let no_repo = scope(None, 3);
        assert!(!no_repo.scoped_recall_ready);
        assert!(no_repo.reason.contains("no GitHub origin"));

        let empty_repo = scope(Some("acme/app"), 0);
        assert!(!empty_repo.scoped_recall_ready);
        assert!(empty_repo.reason.contains("no active memory"));

        let ready = scope(Some("acme/app"), 2);
        assert!(ready.scoped_recall_ready);
    }

    #[test]
    fn repo_scope_status_surfaces_upstream_import_command_for_forks() {
        let status = fork_scope("me/app", "upstream/app", 0, 0);

        assert!(!status.scoped_recall_ready);
        assert_eq!(
            status.suggested_import_command.as_deref(),
            Some("difflore import-reviews --repo me/app --from-upstream upstream/app")
        );
    }

    #[test]
    fn repo_scope_status_treats_upstream_rules_as_fork_ready() {
        let status = fork_scope("me/app", "upstream/app", 0, 12);

        assert!(status.scoped_recall_ready);
        assert_eq!(status.suggested_import_command, None);
        assert!(status.reason.contains("12 upstream active memories"));
        assert!(status.reason.contains("upstream/app"));
    }

    #[test]
    fn next_action_prioritizes_candidate_promotion_for_empty_corpus() {
        let scope = scope(Some("acme/app"), 0);
        let proof = empty_local_proof();

        assert_eq!(
            next_action_for_test(0, 4, 2, &scope, &proof).command,
            "difflore drafts review --repo acme/app"
        );
    }

    #[test]
    fn next_action_falls_back_to_repo_candidate_list_without_candidate_id() {
        let scope = scope(Some("acme/app"), 0);
        let proof = empty_local_proof();

        assert_eq!(
            next_action_for_test(0, 4, 2, &scope, &proof).command,
            "difflore drafts review --repo acme/app"
        );
    }

    #[test]
    fn next_action_uses_recall_when_repo_scoped_rules_exist() {
        let scope = scope(Some("acme/app"), 2);
        let proof = empty_local_proof();

        assert_eq!(
            next_action_for_test(5, 0, 0, &scope, &proof).command,
            "difflore recall --diff"
        );
    }

    #[test]
    fn next_action_uses_fix_preview_after_local_recall_proof_when_cloud_logged_in() {
        let scope = scope(Some("acme/app"), 2);
        let proof = empty_local_proof();
        let recall_proof = LocalRecallProof {
            window_days: 30,
            recall_events: 3,
            recalled_rules: 2,
        };
        let next = next_action(&NextActionInputs {
            active_rules: 5,
            pending_candidates: 0,
            pending_candidates_for_repo: 0,
            cloud_logged_in: true,
            scope: &scope,
            local_proof: &proof,
            local_recall_proof: &recall_proof,
            local_mcp_serves: &empty_mcp_serves(),
        });

        assert_eq!(next.command, "difflore fix --preview");
        assert!(next.reason.contains("accepted edits"));
    }

    #[test]
    fn next_action_uses_fix_preview_after_local_recall_proof_when_cloud_logged_out() {
        let scope = scope(Some("acme/app"), 2);
        let proof = empty_local_proof();
        let recall_proof = LocalRecallProof {
            window_days: 30,
            recall_events: 3,
            recalled_rules: 2,
        };
        let next = next_action(&NextActionInputs {
            active_rules: 5,
            pending_candidates: 0,
            pending_candidates_for_repo: 0,
            cloud_logged_in: false,
            scope: &scope,
            local_proof: &proof,
            local_recall_proof: &recall_proof,
            local_mcp_serves: &empty_mcp_serves(),
        });

        assert_eq!(next.command, "difflore fix --preview");
        assert!(next.reason.contains("accepted edits"));
    }

    #[test]
    fn next_action_uses_fix_preview_after_mcp_rule_serves_when_cloud_logged_in() {
        let scope = scope(Some("acme/app"), 2);
        let proof = empty_local_proof();
        let mcp_serves = LocalMcpRuleServe {
            window_days: 30,
            calls: 1,
            empty_calls: 0,
            rules_served: 2,
            strict_matches: 2,
            estimated_tokens: 200,
        };
        let next = next_action(&NextActionInputs {
            active_rules: 5,
            pending_candidates: 0,
            pending_candidates_for_repo: 0,
            cloud_logged_in: true,
            scope: &scope,
            local_proof: &proof,
            local_recall_proof: &empty_recall_proof(),
            local_mcp_serves: &mcp_serves,
        });

        assert_eq!(next.command, "difflore fix --preview");
        assert!(next.reason.contains("accepted edits"));
    }

    #[test]
    fn next_action_stays_cloud_free_for_unseeded_repo() {
        let scoped_repo = scope(Some("acme/app"), 0);
        let proof = empty_local_proof();
        assert_eq!(
            next_action_for_test(0, 0, 0, &scoped_repo, &proof).command,
            "difflore import-reviews --repo acme/app"
        );

        let no_repo = scope(None, 0);
        assert_eq!(
            next_action_for_test(0, 0, 0, &no_repo, &proof).command,
            "difflore import-reviews"
        );
    }

    #[test]
    fn next_action_imports_upstream_reviews_for_unseeded_fork() {
        let fork = fork_scope("me/app", "upstream/app", 0, 0);
        let proof = empty_local_proof();
        let next = next_action_for_test(10, 0, 0, &fork, &proof);

        assert_eq!(
            next.command,
            "difflore import-reviews --repo me/app --from-upstream upstream/app"
        );
        assert!(next.reason.contains("upstream PR reviews"));
    }

    #[test]
    fn next_action_uses_recall_when_upstream_rules_are_ready_for_fork() {
        let fork = fork_scope("me/app", "upstream/app", 0, 12);
        let proof = empty_local_proof();
        let next = next_action_for_test(10, 0, 0, &fork, &proof);

        assert_eq!(next.command, "difflore recall --diff");
        assert!(next.reason.contains("preview the rules"));
    }

    #[test]
    fn proof_path_includes_cloud_pre_capture_readiness() {
        let import = NextAction {
            command: "difflore import-reviews --repo acme/app".to_owned(),
            reason: String::new(),
        };
        assert_eq!(
            proof_path_commands(&import, true),
            vec![
                "difflore import-reviews --repo acme/app",
                "difflore recall --diff",
                "difflore fix --preview",
                "difflore cloud team --json",
                "difflore cloud impact",
            ]
        );
        assert_eq!(
            proof_path_commands(&import, false),
            vec![
                "difflore import-reviews --repo acme/app",
                "difflore recall --diff",
                "difflore fix --preview",
                "difflore cloud login",
                "difflore cloud team --json",
            ]
        );

        let impact = NextAction {
            command: "difflore cloud impact".to_owned(),
            reason: String::new(),
        };
        assert_eq!(
            proof_path_commands(&impact, true),
            vec!["difflore cloud team --json", "difflore cloud impact"]
        );
        assert_eq!(
            proof_path_commands(&impact, false),
            vec!["difflore cloud login", "difflore cloud team --json"]
        );

        let team = NextAction {
            command: "difflore cloud team --json".to_owned(),
            reason: String::new(),
        };
        assert_eq!(
            proof_path_commands(&team, true),
            vec!["difflore cloud team --json", "difflore cloud impact"]
        );
        assert_eq!(
            proof_path_commands(&team, false),
            vec!["difflore cloud login", "difflore cloud team --json"]
        );

        let login = NextAction {
            command: "difflore cloud login".to_owned(),
            reason: String::new(),
        };
        assert_eq!(
            proof_path_commands(&login, false),
            vec!["difflore cloud login", "difflore cloud team --json"]
        );
    }

    #[test]
    fn next_action_does_not_promote_other_repo_candidates_for_scoped_repo() {
        let scope = scope(Some("acme/app"), 0);
        let proof = empty_local_proof();

        assert_eq!(
            next_action_for_test(10, 3, 0, &scope, &proof).command,
            "difflore import-reviews --repo acme/app"
        );
    }

    #[test]
    fn next_action_reviews_global_drafts_without_repo_scope() {
        let scope = scope(None, 0);
        let proof = empty_local_proof();

        assert_eq!(
            next_action_for_test(0, 3, 0, &scope, &proof).command,
            "difflore drafts review"
        );
    }

    #[test]
    fn next_action_surfaces_local_accepted_proof_first_when_cloud_logged_in() {
        let scope = scope(Some("acme/app"), 2);
        let proof = LocalAcceptedProof {
            window_days: 30,
            recall_lookback_days: 7,
            accepted_proof_signatures: 3,
            accepted_hook_outcomes: 0,
            accepted_outcomes_linked_to_prior_recall: 2,
            accepted_outcomes_linked_to_recall_or_edit_proof: 2,
            accepted_outcomes_linked_to_rule_recall: 1,
            accepted_outcomes_linked_to_mcp_rule_serve: 1,
            accepted_outcomes_linked_to_edit_attribution: 0,
            estimated_saved_review_minutes: 12,
            accepted_and_applied: 0,
            accepted_but_failed: 0,
        };

        let next = next_action(&NextActionInputs {
            active_rules: 5,
            pending_candidates: 0,
            pending_candidates_for_repo: 0,
            cloud_logged_in: true,
            scope: &scope,
            local_proof: &proof,
            local_recall_proof: &empty_recall_proof(),
            local_mcp_serves: &empty_mcp_serves(),
        });

        assert_eq!(next.command, "difflore cloud team --json");
        assert!(next.reason.contains("accepted edits"));
    }

    #[test]
    fn next_action_surfaces_login_before_cloud_impact_when_accepted_proof_exists_logged_out() {
        let scope = scope(Some("acme/app"), 2);
        let proof = LocalAcceptedProof {
            window_days: 30,
            recall_lookback_days: 7,
            accepted_proof_signatures: 3,
            accepted_hook_outcomes: 0,
            accepted_outcomes_linked_to_prior_recall: 2,
            accepted_outcomes_linked_to_recall_or_edit_proof: 2,
            accepted_outcomes_linked_to_rule_recall: 1,
            accepted_outcomes_linked_to_mcp_rule_serve: 1,
            accepted_outcomes_linked_to_edit_attribution: 0,
            estimated_saved_review_minutes: 12,
            accepted_and_applied: 0,
            accepted_but_failed: 0,
        };

        let next = next_action(&NextActionInputs {
            active_rules: 5,
            pending_candidates: 0,
            pending_candidates_for_repo: 0,
            cloud_logged_in: false,
            scope: &scope,
            local_proof: &proof,
            local_recall_proof: &empty_recall_proof(),
            local_mcp_serves: &empty_mcp_serves(),
        });

        assert_eq!(next.command, "difflore cloud login");
        assert!(
            next.reason
                .contains("before uploading local accepted edits")
        );
    }

    #[test]
    fn value_loop_status_surfaces_repo_memory_gap() {
        let status = local_value_loop_status(
            &scope(Some("acme/app"), 0),
            0,
            &empty_recall_proof(),
            &empty_mcp_serves(),
            &empty_local_proof(),
            None,
        );

        assert_eq!(status.stage, "repo_memory_missing");
        assert!(!status.repo_scoped_rules_ready);
        assert!(status.buyer_evidence.contains("import this repo"));
    }

    #[test]
    fn value_loop_status_surfaces_pending_repo_candidates() {
        let status = local_value_loop_status(
            &scope(Some("acme/app"), 0),
            5,
            &empty_recall_proof(),
            &empty_mcp_serves(),
            &empty_local_proof(),
            None,
        );

        assert_eq!(status.stage, "repo_candidates_pending");
        assert!(status.repo_candidates_ready);
        assert!(status.buyer_evidence.contains("5 rule candidates"));
    }

    #[test]
    fn value_loop_status_surfaces_agent_recall_before_accepted_proof() {
        let mcp_serves = LocalMcpRuleServe {
            window_days: 30,
            calls: 1,
            empty_calls: 0,
            rules_served: 3,
            strict_matches: 3,
            estimated_tokens: 240,
        };
        let status = local_value_loop_status(
            &scope(Some("acme/app"), 2),
            0,
            &empty_recall_proof(),
            &mcp_serves,
            &empty_local_proof(),
            None,
        );

        assert_eq!(status.stage, "agent_recall_seen");
        assert!(status.mcp_agent_recall_proof_ready);
        assert!(status.buyer_evidence.contains("file-scoped memory matches"));
        assert!(status.buyer_evidence.contains("preview fixes"));
        assert!(!status.buyer_evidence.contains("run impact"));
    }

    #[test]
    fn value_loop_status_does_not_count_unscoped_mcp_serves_as_agent_recall() {
        let mcp_serves = LocalMcpRuleServe {
            window_days: 30,
            calls: 1,
            empty_calls: 0,
            rules_served: 3,
            strict_matches: 0,
            estimated_tokens: 240,
        };
        let status = local_value_loop_status(
            &scope(Some("acme/app"), 2),
            0,
            &empty_recall_proof(),
            &mcp_serves,
            &empty_local_proof(),
            None,
        );

        assert_eq!(status.stage, "repo_rules_ready");
        assert!(!status.mcp_agent_recall_proof_ready);
        assert!(status.buyer_evidence.contains("preview them"));
    }

    #[test]
    fn value_loop_status_prioritizes_accepted_edit_proof() {
        let accepted = LocalAcceptedProof {
            window_days: 30,
            recall_lookback_days: 7,
            accepted_proof_signatures: 2,
            accepted_hook_outcomes: 0,
            accepted_outcomes_linked_to_prior_recall: 2,
            accepted_outcomes_linked_to_recall_or_edit_proof: 2,
            accepted_outcomes_linked_to_rule_recall: 2,
            accepted_outcomes_linked_to_mcp_rule_serve: 0,
            accepted_outcomes_linked_to_edit_attribution: 0,
            estimated_saved_review_minutes: 8,
            accepted_and_applied: 0,
            accepted_but_failed: 0,
        };
        let status = local_value_loop_status(
            &scope(Some("acme/app"), 2),
            0,
            &LocalRecallProof {
                window_days: 30,
                recall_events: 4,
                recalled_rules: 2,
            },
            &empty_mcp_serves(),
            &accepted,
            None,
        );

        assert_eq!(status.stage, "accepted_edit_proof_ready");
        assert!(status.accepted_edit_proof_ready);
        assert!(
            status
                .buyer_evidence
                .contains("2 after prior memory use (2 rule recalls) within 7d")
        );
        assert_eq!(status.saved_review_minutes, 8);
    }

    #[test]
    fn lane_status_keeps_local_beta_separate_from_production_ga() {
        let value_loop = LocalValueLoopStatus {
            stage: "local_recall_seen".to_owned(),
            repo_scoped_rules_ready: true,
            repo_candidates_ready: false,
            local_recall_proof_ready: true,
            mcp_agent_recall_proof_ready: false,
            accepted_edit_proof_ready: false,
            auditable_value_loop_ready: false,
            saved_review_minutes: 0,
            buyer_evidence: "2 local recall events recorded".to_owned(),
        };
        let next = NextAction {
            command: "difflore cloud team --json".to_owned(),
            reason: "verify cloud session".to_owned(),
        };

        let lanes = lane_status_summary("local-beta", &value_loop, &next);

        assert_eq!(lanes.selected_lane, "local-beta");
        assert!(lanes.local_beta.ready);
        assert_eq!(lanes.local_beta.status, "local_recall_seen");
        assert!(!lanes.local_beta.counts_as_production_evidence);
        assert_eq!(lanes.local_beta.release_ready_influence, "none");
        assert_eq!(lanes.local_beta.production_score_delta, 0);
        assert!(!lanes.production_ga.ready);
        assert_eq!(
            lanes.production_ga.status,
            "blocked_external_release_gates_required"
        );
        assert!(!lanes.production_ga.counts_as_production_evidence);
        assert!(
            lanes
                .production_ga
                .required_evidence
                .iter()
                .any(|item| item.contains("30 counted production"))
        );
    }

    #[test]
    fn value_loop_status_does_not_overclaim_unlinked_accepted_edits() {
        let accepted = LocalAcceptedProof {
            window_days: 30,
            recall_lookback_days: 7,
            accepted_proof_signatures: 2,
            accepted_hook_outcomes: 0,
            accepted_outcomes_linked_to_prior_recall: 0,
            accepted_outcomes_linked_to_recall_or_edit_proof: 0,
            accepted_outcomes_linked_to_rule_recall: 0,
            accepted_outcomes_linked_to_mcp_rule_serve: 0,
            accepted_outcomes_linked_to_edit_attribution: 0,
            estimated_saved_review_minutes: 8,
            accepted_and_applied: 0,
            accepted_but_failed: 0,
        };
        let status = local_value_loop_status(
            &scope(Some("acme/app"), 2),
            0,
            &LocalRecallProof {
                window_days: 30,
                recall_events: 4,
                recalled_rules: 2,
            },
            &empty_mcp_serves(),
            &accepted,
            None,
        );

        assert_eq!(status.stage, "local_recall_seen");
        assert!(status.local_recall_proof_ready);
        assert!(!status.accepted_edit_proof_ready);
        assert!(
            status
                .buyer_evidence
                .contains("4 local recall events recorded")
        );
    }

    #[test]
    fn value_loop_source_parsing_reads_repo_and_pr() {
        let proof = difflore_core::skills::CandidateSourceProof {
            source: Some("acme/widgets#42".to_owned()),
            comment_url: Some("https://github.com/acme/widgets/pull/42#discussion_r1".to_owned()),
            file: Some("src/parser.rs".to_owned()),
            excerpt: None,
        };

        assert_eq!(
            source_repo_for_value_loop(&proof, None).as_deref(),
            Some("acme/widgets")
        );
        assert_eq!(pr_number_for_value_loop(&proof), Some(42));
    }

    #[test]
    fn candidate_preview_extracts_review_source_and_accept_command() {
        let candidate = difflore_core::skills::CandidateRule {
            id: "cand-123".to_owned(),
            name: "Use stable waits in router tests".to_owned(),
            description: "\
Imported from a GitHub PR review comment. Keep as a pending memory draft until a human confirms this is a repeatable review rule.

Source: tanstack/router#42
Comment: https://github.com/tanstack/router/pull/42#discussion_r1
File: packages/router/src/test.ts

Reviewer said:
Prefer stable waits here instead of relying on a race with the scheduler."
                .to_owned(),
            origin: "pr_review".to_owned(),
            installed_at: "2026-05-06 00:00:00".to_owned(),
            source_repo: Some("tanstack/router".to_owned()),
            file_patterns: vec!["packages/router/**/*.ts".to_owned()],
            drafted_rule: Some(
                "When touching `packages/router/**/*.ts`, prefer stable waits.".to_owned(),
            ),
            source_proof: difflore_core::skills::parse_candidate_source_proof(
                "\
Source: tanstack/router#42
Comment: https://github.com/tanstack/router/pull/42#discussion_r1
File: packages/router/src/test.ts

Reviewer said:
Prefer stable waits here instead of relying on a race with the scheduler.",
            ),
        };

        let preview = candidate_preview(&candidate);

        assert_eq!(preview.source.as_deref(), Some("tanstack/router#42"));
        assert_eq!(preview.file.as_deref(), Some("packages/router/src/test.ts"));
        assert_eq!(
            preview.comment_url.as_deref(),
            Some("https://github.com/tanstack/router/pull/42#discussion_r1")
        );
        assert_eq!(
            preview.preview,
            "Prefer stable waits here instead of relying on a race with the scheduler."
        );
        assert_eq!(preview.accept_command, "difflore drafts approve cand-123");
        assert_eq!(preview.explain_command, "difflore drafts show cand-123");
    }
}
