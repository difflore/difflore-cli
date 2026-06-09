use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::process::Command;

use difflore_core::cloud::api_types::{RecordAcceptedEditRequest, RecordAcceptedEditResponse};
use difflore_core::context::retrieval::detect_language_from_path;
use difflore_core::observability::fix_outcomes::FixOutcomeInput;
use difflore_core::review::ReviewIssueRecord;

use crate::style::{self, sym};

use super::{file_loc, fix_debug};

#[derive(Debug, Default)]
pub(super) struct ApplyOutcome {
    pub(super) applied: Vec<OutcomeIssue>,
    pub(super) failed: Vec<(OutcomeIssue, String)>,
    pub(super) accepted_edits: Vec<AcceptedEditProof>,
}

#[derive(Debug, Clone)]
pub(super) struct AcceptedEditProof {
    pub(super) file_path: String,
    pub(super) before_code: String,
    pub(super) after_code: String,
    pub(super) language: Option<String>,
    pub(super) diff_signature: String,
    pub(super) rule_ids: Vec<String>,
}

#[derive(Debug, Clone)]
pub(super) struct OutcomeIssue {
    pub(super) rule_id: Option<String>,
    pub(super) rule_name: String,
    pub(super) file_path: Option<String>,
    pub(super) file_loc: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct AcceptedEditUploadSummary {
    queued: usize,
    uploaded: usize,
    linked_observations: usize,
    missing_rule_ids: usize,
    missing_target_pr: usize,
    missing_team: usize,
    missing_rule_observation: usize,
}

const fn record_accepted_edit_upload_queued(
    summary: &mut AcceptedEditUploadSummary,
    expected_rule_ids: usize,
    has_target_pr: bool,
) {
    summary.queued += 1;
    if expected_rule_ids == 0 {
        summary.missing_rule_ids += 1;
    }
    if !has_target_pr {
        summary.missing_target_pr += 1;
    }
}

const fn record_accepted_edit_upload_response(
    summary: &mut AcceptedEditUploadSummary,
    expected_rule_ids: usize,
    response: &RecordAcceptedEditResponse,
) {
    if !response.acceptance_recorded {
        return;
    }
    summary.uploaded += 1;
    summary.linked_observations += response.observations_inserted as usize;
    if expected_rule_ids == 0 {
        return;
    }
    if response.team_id.is_none() {
        summary.missing_team += 1;
    } else if response.observations_inserted == 0 {
        summary.missing_rule_observation += 1;
    }
}

fn print_accepted_edit_upload_warnings(summary: &AcceptedEditUploadSummary) {
    if summary.uploaded == 0 && summary.queued == 0 {
        return;
    }
    let pending_attribution = summary.queued.saturating_sub(summary.uploaded);
    if pending_attribution > 0 {
        eprintln!(
            "{} {count} accepted edit evidence candidate(s) queued for later cloud upload; cloud-linked accepted-fix evidence status is unverified until cloud sync confirms team workspace and linked rule observations.",
            style::warn(sym::WARN),
            count = pending_attribution
        );
    }
    if summary.missing_rule_ids > 0 {
        eprintln!(
            "{} {count} accepted edit evidence candidate(s) have no recalled rule id; they can be raw accepted-edit telemetry, but cannot count as Impact evidence.",
            style::warn(sym::WARN),
            count = summary.missing_rule_ids
        );
    }
    if summary.missing_target_pr > 0 {
        eprintln!(
            "{} {count} accepted edit evidence candidate(s) have no target PR number; run `difflore fix --pr <PR>` on a real PR before collecting cloud-linked accepted-fix evidence.",
            style::warn(sym::WARN),
            count = summary.missing_target_pr
        );
    }
    if summary.missing_team > 0 {
        eprintln!(
            "{} {count} accepted edit evidence item(s) uploaded, but no cloud team workspace was found; create or join a team before collecting Impact evidence.",
            style::warn(sym::WARN),
            count = summary.missing_team
        );
    }
    if summary.missing_rule_observation > 0 {
        eprintln!(
            "{} {count} accepted edit evidence item(s) uploaded without a linked rule observation; Impact evidence needs accepted fixes with recalled rule ids.",
            style::warn(sym::WARN),
            count = summary.missing_rule_observation
        );
    }
}

impl OutcomeIssue {
    pub(super) fn rule_label(&self) -> String {
        match &self.rule_id {
            Some(id) if !id.trim().is_empty() => format!("{} ({id})", self.rule_name),
            _ => self.rule_name.clone(),
        }
    }
}

impl From<&ReviewIssueRecord> for OutcomeIssue {
    fn from(issue: &ReviewIssueRecord) -> Self {
        Self {
            rule_id: issue.rule_id.clone(),
            rule_name: issue.rule.clone(),
            file_path: issue.file.clone(),
            file_loc: file_loc(issue),
        }
    }
}

pub(super) const fn yes_mode_should_fail(outcome: &ApplyOutcome) -> bool {
    !outcome.failed.is_empty()
}

/// Does this failure reason describe provider/auth misconfiguration rather
/// than a user-accepted bad fix? Drop those rows at the write boundary so
/// `fix_outcomes` carries genuine fix attempts.
///
/// Matches are deliberately substring-based: the upstream copy is
/// generated by `claude` / the chat pipeline and varies by version, so
/// we trade precision for robustness here. Patch-quality failures still
/// get recorded.
fn failure_reason_is_provider_misconfig(reason: &str) -> bool {
    let lower = reason.to_ascii_lowercase();
    lower.contains("not logged in")
        || lower.contains("please run /login")
        || lower.contains("no active ai provider configured")
        || lower.contains("no llm provider configured")
        || lower.contains("no supported agent cli found")
        || lower.contains("failed to spawn `claude` cli")
        || lower.contains("failed to spawn claude cli")
}

pub(super) async fn record_fix_outcomes(
    db: &difflore_core::SqlitePool,
    outcome: &mut ApplyOutcome,
    rejected: &[&ReviewIssueRecord],
    repo_full_name: Option<&str>,
    pr_number: Option<u64>,
    upload_acceptance: bool,
) {
    let target_pr_number = pr_number.and_then(|number| i64::try_from(number).ok());
    let accepted_signatures_by_file: std::collections::BTreeMap<&str, &str> = outcome
        .accepted_edits
        .iter()
        .map(|proof| (proof.file_path.as_str(), proof.diff_signature.as_str()))
        .collect();
    let mut rows: Vec<FixOutcomeInput<'_>> = Vec::new();
    rows.extend(outcome.applied.iter().map(|issue| {
        FixOutcomeInput {
            rule_id: issue.rule_id.as_deref(),
            rule_name: &issue.rule_name,
            file_path: issue.file_path.as_deref(),
            repo_full_name,
            pr_number: target_pr_number,
            diff_signature: issue
                .file_path
                .as_deref()
                .and_then(|path| accepted_signatures_by_file.get(path).copied()),
            accepted: true,
            applied_ok: true,
            failed_reason: None,
        }
    }));
    rows.extend(
        outcome
            .failed
            .iter()
            .filter(|(_, reason)| !failure_reason_is_provider_misconfig(reason))
            .map(|(issue, reason)| FixOutcomeInput {
                rule_id: issue.rule_id.as_deref(),
                rule_name: &issue.rule_name,
                file_path: issue.file_path.as_deref(),
                repo_full_name,
                pr_number: target_pr_number,
                diff_signature: None,
                accepted: true,
                applied_ok: false,
                failed_reason: Some(reason.as_str()),
            }),
    );
    rows.extend(rejected.iter().map(|issue| FixOutcomeInput {
        rule_id: issue.rule_id.as_deref(),
        rule_name: &issue.rule,
        file_path: issue.file.as_deref(),
        repo_full_name,
        pr_number: target_pr_number,
        diff_signature: None,
        accepted: false,
        applied_ok: false,
        failed_reason: None,
    }));

    if let Err(e) = difflore_core::fix_outcomes::record_many(db, &rows).await {
        eprintln!(
            "{} could not record local fix outcomes: {e}",
            style::warn(sym::WARN)
        );
    }

    // Accepted fixes reinforce by +0.05 and rejections by -0.1 through
    // `update_confidence`, which owns clamping and `rule_events` writes.
    // The activity event uses the returned before/after values; unknown
    // rule ids are skipped rather than inventing a strength.
    for issue in &outcome.applied {
        let Some(rule_id) = issue.rule_id.as_deref().filter(|s| !s.trim().is_empty()) else {
            fix_debug!(
                "skipping rule reinforcement for applied issue with empty rule_id (rule={})",
                issue.rule_name
            );
            continue;
        };
        reinforce_rule(db, rule_id, &issue.rule_name, "accept", "fix_accepted").await;
    }
    for issue in rejected {
        let Some(rule_id) = issue.rule_id.as_deref().filter(|s| !s.trim().is_empty()) else {
            fix_debug!(
                "skipping rule reinforcement for rejected issue with empty rule_id (rule={})",
                issue.rule
            );
            continue;
        };
        reinforce_rule(db, rule_id, &issue.rule, "reject", "fix_rejected").await;
    }

    fix_debug!("accepted edit proofs: {}", outcome.accepted_edits.len());
    if !upload_acceptance {
        outcome.accepted_edits.clear();
        return;
    }

    // Drain so the per-acceptance fields can be moved into the cloud
    // request struct rather than cloned. ApplyOutcome.accepted_edits
    // isn't read again after this point (print_apply_summary only
    // touches applied/failed lists).
    let acceptances = std::mem::take(&mut outcome.accepted_edits);
    record_accepted_edit_proofs(db, acceptances, repo_full_name, pr_number).await;
}

/// Apply a reinforcement signal to a local rule and emit its activity event.
///
/// `signal` is exactly "accept" (+0.05) or "reject" (-0.1) — the only two
/// values `difflore_core::skills::update_confidence` accepts. That fn owns
/// the mutation: it reads the before value, clamps the delta into 0.0..=1.0,
/// updates `skills.confidence_score`, and inserts a `rule_events` row. The
/// returned before/after values feed the `RuleReinforced` event.
///
/// On `Err` (the rule_id isn't a local skill — deleted, or a non-skill id
/// with no `skills` row) we skip and `fix_debug!` rather than guess a
/// strength; reinforcement telemetry must never invent state changes.
///
/// `before`/`after` are 0.0..=1.0 REALs, so the f64→f32 narrowing for the
/// activity event is lossless in practice; the lint is allowed at fn scope
/// rather than on a bare tail expression (which stable Rust rejects).
#[allow(clippy::cast_possible_truncation)]
async fn reinforce_rule(
    db: &difflore_core::SqlitePool,
    rule_id: &str,
    rule_title: &str,
    signal: &str,
    reason: &str,
) {
    let input = difflore_core::models::UpdateConfidenceInput {
        skill_id: rule_id.to_owned(),
        signal: signal.to_owned(),
    };
    match difflore_core::skills::update_confidence(db, input).await {
        Ok(change) => {
            difflore_core::activity_stream::record(
                difflore_core::activity_stream::ActivityPayload::RuleReinforced {
                    rule_id: rule_id.to_owned(),
                    rule_title: rule_title.to_owned(),
                    prev_strength: change.before as f32,
                    new_strength: change.after as f32,
                    reason: reason.to_owned(),
                },
            );
        }
        Err(e) => {
            fix_debug!(
                "skipping rule reinforcement for rule_id `{rule_id}` ({reason}); update_confidence failed: {e}"
            );
        }
    }
}

async fn record_accepted_edit_proofs(
    db: &difflore_core::SqlitePool,
    acceptances: Vec<AcceptedEditProof>,
    repo_full_name: Option<&str>,
    pr_number: Option<u64>,
) {
    if acceptances.is_empty() {
        return;
    }

    let queue = difflore_core::cloud::outbox::OutboxQueue::new(db.clone());
    let client = difflore_core::cloud::client::CloudClient::create().await;
    let mut upload_summary = AcceptedEditUploadSummary::default();
    let target_pr_number = pr_number.and_then(|number| i64::try_from(number).ok());
    let mut rule_id_cache: std::collections::BTreeMap<String, Option<String>> =
        std::collections::BTreeMap::new();
    for mut acceptance in acceptances {
        acceptance.rule_ids =
            resolve_acceptance_cloud_rule_ids(db, acceptance.rule_ids, &mut rule_id_cache).await;
        let req = accepted_edit_upload_request(acceptance, repo_full_name, target_pr_number);
        let expected_rule_ids = req.rule_ids.len();
        let payload = match serde_json::to_string(&req) {
            Ok(payload) => payload,
            Err(e) => {
                eprintln!(
                    "{} could not encode accepted edit evidence: {e}",
                    style::warn(sym::WARN)
                );
                continue;
            }
        };
        let row_id = match queue
            .enqueue(difflore_core::cloud::outbox::kind::ACCEPTED_EDIT, &payload)
            .await
        {
            Ok(row_id) => row_id,
            Err(e) => {
                eprintln!(
                    "{} could not queue accepted edit evidence: {e}",
                    style::warn(sym::WARN)
                );
                return;
            }
        };
        record_accepted_edit_upload_queued(
            &mut upload_summary,
            expected_rule_ids,
            target_pr_number.is_some(),
        );

        if client.is_logged_in() {
            match client.record_accepted_edit_response(req).await {
                Ok(response) if response.acceptance_recorded => {
                    record_accepted_edit_upload_response(
                        &mut upload_summary,
                        expected_rule_ids,
                        &response,
                    );
                    if let Err(e) = queue.confirm(row_id).await {
                        eprintln!(
                            "{} accepted edit evidence uploaded but local outbox cleanup failed: {e}",
                            style::warn(sym::WARN)
                        );
                    }
                }
                Ok(response) => {
                    fix_debug!(
                        "accepted edit evidence upload returned ok={} error={:?}",
                        response.ok,
                        response.error
                    );
                }
                Err(e) => {
                    fix_debug!("accepted edit evidence upload failed: {e}");
                }
            }
        }
    }
    print_accepted_edit_upload_warnings(&upload_summary);
}

fn accepted_edit_upload_request(
    acceptance: AcceptedEditProof,
    repo_full_name: Option<&str>,
    target_pr_number: Option<i64>,
) -> RecordAcceptedEditRequest {
    RecordAcceptedEditRequest {
        before_code: acceptance.before_code,
        after_code: acceptance.after_code,
        file_path: Some(acceptance.file_path),
        repo_full_name: repo_full_name.map(str::to_owned),
        target_pr_number,
        language: acceptance.language,
        acceptance_source: Some("difflore_fix".to_owned()),
        client: Some("difflore_cli".to_owned()),
        diff_signature: Some(acceptance.diff_signature),
        rule_ids: acceptance.rule_ids,
    }
}

async fn resolve_acceptance_cloud_rule_ids(
    db: &difflore_core::SqlitePool,
    rule_ids: Vec<String>,
    cache: &mut std::collections::BTreeMap<String, Option<String>>,
) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for rule_id in rule_ids {
        let rule_id = rule_id.trim();
        if rule_id.is_empty() {
            continue;
        }

        let cloud_rule_id = if let Some(cached) = cache.get(rule_id).cloned() {
            cached
        } else {
            let resolved = match difflore_core::team::resolve_known_cloud_rule_id(db, rule_id).await
            {
                Ok(resolved) => resolved,
                Err(e) => {
                    fix_debug!(
                        "accepted edit evidence skipped rule_id `{rule_id}`; cloud id lookup failed: {e}"
                    );
                    None
                }
            };
            cache.insert(rule_id.to_owned(), resolved.clone());
            resolved
        };

        let Some(cloud_rule_id) = cloud_rule_id else {
            fix_debug!("accepted edit evidence omitted unmapped non-cloud rule_id `{rule_id}`");
            continue;
        };
        if seen.insert(cloud_rule_id.clone()) {
            out.push(cloud_rule_id);
        }
    }
    out
}

fn unique_rule_ids(issues: &[&ReviewIssueRecord]) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for issue in issues {
        let Some(rule_id) = issue
            .rule_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
        else {
            continue;
        };
        if seen.insert(rule_id.to_owned()) {
            out.push(rule_id.to_owned());
        }
    }
    out
}

pub(super) fn print_apply_summary(outcome: &ApplyOutcome, skipped: u32, total: usize) {
    println!();
    println!(
        "{} {} applied, {} failed, {} skipped (of {}).",
        style::ok(sym::OK),
        outcome.applied.len(),
        outcome.failed.len(),
        skipped,
        total,
    );
    if !outcome.failed.is_empty() {
        println!();
        println!(
            "  {} patch generation didn't pan out for:",
            style::warn(sym::WARN),
        );
        // Group identical reasons so a systemic provider failure shows once.
        use std::collections::BTreeMap;
        let mut by_reason: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for (issue, reason) in &outcome.failed {
            by_reason
                .entry(reason.as_str())
                .or_default()
                .push(issue.file_loc.as_str());
        }
        for (reason, locs) in &by_reason {
            if locs.len() == 1 {
                println!("      {}  ({reason})", locs[0]);
            } else {
                println!("      {} suggestions · {reason}", locs.len());
                for loc in locs {
                    println!("        · {loc}");
                }
            }
        }
    }
    if !outcome.applied.is_empty() {
        let mut seen = std::collections::BTreeSet::new();
        for issue in &outcome.applied {
            seen.insert(issue.rule_label());
        }
        let rule_word = if seen.len() == 1 { "rule" } else { "rules" };
        println!("  {} applied {rule_word}:", style::pewter(sym::BULLET));
        for label in &seen {
            println!("      {label}");
        }
        println!("  → review what changed: {}", style::cmd("git diff"));
        println!("  → undo: {}", style::cmd("git checkout -p"));
    }
    println!();
    println!("next: {}", style::cmd("difflore status"));
}

// Per-file batching: N issues in one file → ONE LLM round trip → ONE diff.
// Avoids both N× cost and inter-patch line-number races.
pub(super) async fn apply_accepted_patches(
    db: &difflore_core::SqlitePool,
    repo_root: &Path,
    accepted: &[&ReviewIssueRecord],
    sync_staged_index: bool,
    quiet: bool,
) -> ApplyOutcome {
    let mut outcome = ApplyOutcome::default();
    if accepted.is_empty() {
        return outcome;
    }

    use std::collections::BTreeMap;
    let mut by_file: BTreeMap<String, Vec<&ReviewIssueRecord>> = BTreeMap::new();
    let mut orphans: Vec<&ReviewIssueRecord> = Vec::new();
    for issue in accepted {
        match issue.file.as_deref() {
            Some(p) => by_file.entry(p.to_owned()).or_default().push(issue),
            None => orphans.push(issue),
        }
    }

    for issue in &orphans {
        outcome
            .failed
            .push((OutcomeIssue::from(*issue), "issue has no file path".into()));
    }

    let total_files = by_file.len();
    if total_files > 0 && !quiet {
        println!();
    }

    let mut pending_applied: Vec<PendingAppliedPatch> = Vec::new();
    let mut aborted = false;
    for (idx, (file_path, issues)) in by_file.iter().enumerate() {
        let loc = if issues.len() == 1 {
            file_loc(issues[0])
        } else {
            format!("{file_path} ({} suggestions)", issues.len())
        };
        let step = idx + 1;
        if !quiet {
            print_progress_pending(step, total_files, &loc);
        }

        // Security: an issue's `file` is semi-trusted input. Constrain it to the
        // repository before reading + feeding the content to the model, so a
        // crafted `../../etc/...` path can't exfiltrate arbitrary local files.
        let abs_path = match super::path_safety::repo_relative_path(repo_root, file_path) {
            Ok(p) => p,
            Err(reason) => {
                record_failure(
                    &mut outcome,
                    issues,
                    &loc,
                    step,
                    total_files,
                    &reason,
                    quiet,
                );
                rollback_pending_patches(
                    &mut outcome,
                    repo_root,
                    sync_staged_index,
                    &mut pending_applied,
                    &reason,
                );
                aborted = true;
                break;
            }
        };
        let file_content = match std::fs::read_to_string(&abs_path) {
            Ok(s) => s,
            Err(e) => {
                let reason = format!("read failed: {e}");
                record_failure(
                    &mut outcome,
                    issues,
                    &loc,
                    step,
                    total_files,
                    &reason,
                    quiet,
                );
                rollback_pending_patches(
                    &mut outcome,
                    repo_root,
                    sync_staged_index,
                    &mut pending_applied,
                    &reason,
                );
                aborted = true;
                break;
            }
        };

        let prompt = if issues.len() == 1 {
            patch_user_prompt(file_path, &file_content, issues[0])
        } else {
            batched_patch_user_prompt(file_path, &file_content, issues)
        };
        if let Some(path) = difflore_core::env::fix_dump_dir() {
            std::fs::write(format!("{path}/last_patch_prompt.txt"), &prompt).ok();
            std::fs::write(
                format!("{path}/last_patch_system.txt"),
                patch_system_prompt(),
            )
            .ok();
        }

        let raw = match difflore_core::review::complete_with_active_provider(
            db,
            patch_system_prompt(),
            &prompt,
        )
        .await
        {
            Ok(s) => s,
            Err(e) => {
                // Strip CoreError prefix so user-facing copy doesn't leak the Result variant.
                let raw = e.to_string();
                let trimmed = raw
                    .strip_prefix("Internal error: ")
                    .or_else(|| raw.strip_prefix("Validation error: "))
                    .unwrap_or(&raw);
                let reason = trimmed.to_owned();
                record_failure(
                    &mut outcome,
                    issues,
                    &loc,
                    step,
                    total_files,
                    &reason,
                    quiet,
                );
                rollback_pending_patches(
                    &mut outcome,
                    repo_root,
                    sync_staged_index,
                    &mut pending_applied,
                    &reason,
                );
                aborted = true;
                break;
            }
        };
        if let Some(path) = difflore_core::env::fix_dump_dir() {
            std::fs::write(format!("{path}/last_patch_raw.txt"), &raw).ok();
        }

        let Some(diff) = extract_unified_diff(&raw) else {
            let reason = "LLM returned no usable diff";
            record_failure(&mut outcome, issues, &loc, step, total_files, reason, quiet);
            rollback_pending_patches(
                &mut outcome,
                repo_root,
                sync_staged_index,
                &mut pending_applied,
                reason,
            );
            aborted = true;
            break;
        };

        fix_debug!("generated patch:\n{diff}\n[fix-debug] end patch");
        if let Some(path) = difflore_core::env::fix_dump_dir() {
            std::fs::write(format!("{path}/last_patch.diff"), &diff).ok();
        }

        match apply_diff_transactionally(
            repo_root,
            &diff,
            sync_staged_index,
            &abs_path,
            &file_content,
            file_path,
            issues,
        ) {
            Ok(pending) => {
                pending_applied.push(pending);
                if !quiet {
                    print_progress_done(step, total_files, &loc, Ok(()));
                }
            }
            Err(e) => {
                record_failure(&mut outcome, issues, &loc, step, total_files, &e, quiet);
                rollback_pending_patches(
                    &mut outcome,
                    repo_root,
                    sync_staged_index,
                    &mut pending_applied,
                    &e,
                );
                aborted = true;
                break;
            }
        }
    }

    if !aborted {
        for pending in pending_applied {
            outcome.applied.extend(pending.issues);
            if let Some(proof) = pending.accepted_edit {
                outcome.accepted_edits.push(proof);
            }
        }
    }

    outcome
}

struct PendingAppliedPatch {
    diff: String,
    issues: Vec<OutcomeIssue>,
    accepted_edit: Option<AcceptedEditProof>,
}

fn apply_diff_transactionally(
    repo_root: &Path,
    diff: &str,
    sync_staged_index: bool,
    abs_path: &Path,
    before_content: &str,
    file_path: &str,
    issues: &[&ReviewIssueRecord],
) -> Result<PendingAppliedPatch, String> {
    // Security: the model's diff is semi-trusted. Constrain it to the single
    // file this patch was generated for before letting `git apply` touch the
    // worktree — otherwise one issue's patch could mutate any repo file.
    super::path_safety::validate_diff_targets(diff, file_path)?;
    with_diff_tempfile(diff, |diff_path| {
        let diff_path = diff_path.map_err(|e| format!("tempfile: {e}"))?;
        run_git_apply(repo_root, diff_path, true).map_err(|e| format!("validation: {e}"))?;
        if sync_staged_index {
            run_git_apply_cached(repo_root, diff_path, true)
                .map_err(|e| format!("index validation: {e}"))?;
        }
        run_git_apply(repo_root, diff_path, false).map_err(|e| format!("apply: {e}"))?;
        if sync_staged_index && let Err(e) = run_git_apply_cached(repo_root, diff_path, false) {
            let rollback = run_git_apply_reverse(repo_root, diff_path, false).map_or_else(
                |rollback_err| format!("; rollback failed: {rollback_err}"),
                |()| "; worktree rolled back".to_owned(),
            );
            return Err(format!("index apply: {e}{rollback}"));
        }

        let accepted_edit = match std::fs::read_to_string(abs_path) {
            Ok(after_content) => {
                let diff_signature = difflore_core::cloud::api_types::accepted_edit_diff_signature(
                    before_content,
                    &after_content,
                );
                Some(AcceptedEditProof {
                    file_path: file_path.to_owned(),
                    before_code: before_content.to_owned(),
                    after_code: after_content,
                    language: detect_language_from_path(file_path),
                    diff_signature,
                    rule_ids: unique_rule_ids(issues),
                })
            }
            Err(e) => {
                fix_debug!("accepted edit proof skipped for {file_path}: {e}");
                None
            }
        };

        Ok(PendingAppliedPatch {
            diff: diff.to_owned(),
            issues: issues
                .iter()
                .map(|issue| OutcomeIssue::from(*issue))
                .collect(),
            accepted_edit,
        })
    })
}

fn rollback_pending_patches(
    outcome: &mut ApplyOutcome,
    repo_root: &Path,
    sync_staged_index: bool,
    pending: &mut Vec<PendingAppliedPatch>,
    cause: &str,
) {
    if pending.is_empty() {
        return;
    }

    let mut rollback_errors = Vec::new();
    for patch in pending.iter().rev() {
        with_diff_tempfile(&patch.diff, |diff_path| match diff_path {
            Err(e) => rollback_errors.push(format!("tempfile: {e}")),
            Ok(diff_path) => {
                if sync_staged_index
                    && let Err(e) = run_git_apply_reverse(repo_root, diff_path, true)
                {
                    rollback_errors.push(format!("cached reverse apply failed: {e}"));
                }
                if let Err(e) = run_git_apply_reverse(repo_root, diff_path, false) {
                    rollback_errors.push(format!("worktree reverse apply failed: {e}"));
                }
            }
        });
    }

    let reason = if rollback_errors.is_empty() {
        format!("rolled back because apply transaction failed: {cause}")
    } else {
        format!(
            "apply transaction failed: {cause}; rollback reported: {}",
            rollback_errors.join("; ")
        )
    };
    for patch in pending.drain(..) {
        for issue in patch.issues {
            outcome.failed.push((issue, reason.clone()));
        }
    }
}

fn record_failure(
    outcome: &mut ApplyOutcome,
    issues: &[&ReviewIssueRecord],
    loc: &str,
    idx: usize,
    total: usize,
    reason: &str,
    quiet: bool,
) {
    for issue in issues {
        outcome
            .failed
            .push((OutcomeIssue::from(*issue), reason.to_owned()));
    }
    if !quiet {
        print_progress_done(idx, total, loc, Err(reason));
    }
}

fn with_diff_tempfile<R>(diff: &str, f: impl FnOnce(Result<&Path, String>) -> R) -> R {
    use std::io::Write as _;
    let mut tmp = match tempfile::NamedTempFile::new() {
        Ok(t) => t,
        Err(e) => return f(Err(format!("failed to create tempfile: {e}"))),
    };
    if let Err(e) = tmp.write_all(diff.as_bytes()) {
        return f(Err(format!("failed to write tempfile: {e}")));
    }
    f(Ok(tmp.path()))
}

fn print_progress_pending(idx: usize, total: usize, file_loc: &str) {
    let line = format!(
        "  [{idx}/{total}] {} generating patch for {file_loc}...",
        style::pewter(sym::BULLET),
    );
    if io::stdout().is_terminal() {
        write!(io::stdout(), "\r{line}").ok();
        io::stdout().flush().ok();
    } else {
        println!("{line}");
    }
}

fn print_progress_done(idx: usize, total: usize, file_loc: &str, result: Result<(), &str>) {
    let body = match result {
        Ok(()) => format!(
            "  [{idx}/{total}] {} applied {file_loc}",
            style::ok(sym::OK)
        ),
        Err(reason) => format!(
            "  [{idx}/{total}] {} {file_loc}  ({reason})",
            style::warn(sym::WARN)
        ),
    };
    if io::stdout().is_terminal() {
        // \r + clear-to-EOL so the longer pending line doesn't bleed past the done line.
        write!(io::stdout(), "\r\x1b[K{body}\n").ok();
        io::stdout().flush().ok();
    } else {
        println!("{body}");
    }
}

const fn patch_system_prompt() -> &'static str {
    "Task: generate a code patch. Given a source file and a code review \
     suggestion, output ONLY a unified diff that fixes the issue. \
     Strict rules:\n\
     1. Use the standard unified diff format with `--- a/<path>` and \
        `+++ b/<path>` headers and `@@ ... @@` hunk headers.\n\
     2. Match the file content EXACTLY for context lines — preserve \
        whitespace, indentation, and trailing characters.\n\
     3. Make the smallest possible change that satisfies the suggestion.\n\
     4. Do NOT include prose, commentary, or markdown fences in the output.\n\
     5. Before outputting a diff, verify the resulting file would satisfy all \
        explicit constraints in the suggestion; if it would leave any violation, \
        output `NO_PATCH`.\n\
     6. If the suggestion cannot be turned into a precise diff against this \
        file, output the single line `NO_PATCH` and nothing else."
}

fn patch_user_prompt(file_path: &str, content: &str, issue: &ReviewIssueRecord) -> String {
    // Omit issue.line on purpose: when batched patches edit the same file,
    // earlier line numbers go stale; the model anchoring on a stale line
    // produces wrong-region diffs. Suggestion text + content is enough; if
    // not, NO_PATCH is the right answer.
    let suggestion = issue
        .suggestion
        .as_deref()
        .unwrap_or("(no suggestion text)");
    let content_for_prompt = focused_patch_context(content, issue.line);
    let context_label = if content_for_prompt.len() == content.len() {
        "Current file content"
    } else {
        "Focused current file excerpt near the issue"
    };
    format!(
        "File: {file_path}\n\nReview issue: {}\n\nSuggested change:\n{}\n\n\
         {context_label}:\n```\n{}\n```\n\n\
         Output the unified diff (or `NO_PATCH`):",
        issue.message, suggestion, content_for_prompt,
    )
}

fn focused_patch_context(content: &str, line: Option<i32>) -> String {
    const FULL_FILE_LIMIT: usize = 4_000;
    const CONTEXT_RADIUS: usize = 24;
    // HEADER_LINES must stay smaller than CONTEXT_RADIUS so header doesn't overlap focus.
    const HEADER_LINES: usize = 20;

    if content.len() <= FULL_FILE_LIMIT {
        return content.to_owned();
    }

    let Some(line) = line
        .and_then(|n| usize::try_from(n).ok())
        .filter(|n| *n > 0)
    else {
        return content.to_owned();
    };

    let lines: Vec<&str> = content.lines().collect();
    if lines.len() <= CONTEXT_RADIUS * 2 {
        return content.to_owned();
    }

    let target = line.saturating_sub(1).min(lines.len().saturating_sub(1));
    let start = target.saturating_sub(CONTEXT_RADIUS);
    let end = (target + CONTEXT_RADIUS + 1).min(lines.len());
    let focus = lines[start..end].join("\n");

    if start > HEADER_LINES {
        let header = lines[..HEADER_LINES.min(lines.len())].join("\n");
        format!("{header}\n\n...\n\n{focus}")
    } else {
        focus
    }
}

fn batched_patch_user_prompt(
    file_path: &str,
    content: &str,
    issues: &[&ReviewIssueRecord],
) -> String {
    use core::fmt::Write as _;
    let mut s = String::new();
    write!(s, "File: {file_path}\n\n").ok();
    write!(
        s,
        "Apply ALL of these {} review suggestions to the file in a single \
         consolidated unified diff. Each suggestion must be addressed; if any \
         single suggestion cannot be cleanly applied alongside the others, \
         output `NO_PATCH`.\n\n",
        issues.len()
    )
    .ok();
    for (i, issue) in issues.iter().enumerate() {
        let suggestion = issue
            .suggestion
            .as_deref()
            .unwrap_or("(no suggestion text)");
        write!(
            s,
            "── Suggestion {} ──\nReview issue: {}\nSuggested change:\n{}\n\n",
            i + 1,
            issue.message,
            suggestion,
        )
        .ok();
    }
    write!(
        s,
        "Current file content:\n```\n{content}\n```\n\n\
         Output ONE unified diff covering every suggestion (or `NO_PATCH`):"
    )
    .ok();
    s
}

fn extract_unified_diff(raw: &str) -> Option<String> {
    let text = raw.trim();
    if text.lines().any(|l| l.trim() == "NO_PATCH") {
        return None;
    }

    let stripped = if text.starts_with("```") {
        let after_open = text.split_once('\n').map_or("", |x| x.1);
        let body = after_open.trim_end_matches('\n');
        body.strip_suffix("```")
            .map_or_else(|| body.to_owned(), |s| s.trim_end().to_owned())
    } else {
        text.to_owned()
    };

    // First `--- ` line wins; preamble before it is tolerated.
    let start_idx = stripped.find("--- ")?;
    let diff = stripped[start_idx..].trim_end().to_owned();
    if !diff.contains("+++ ") || !diff.contains("@@ ") {
        return None;
    }

    if diff.ends_with('\n') {
        Some(diff)
    } else {
        Some(format!("{diff}\n"))
    }
}

fn run_git_apply(repo_root: &Path, diff_path: &Path, check_only: bool) -> Result<(), String> {
    run_git_apply_impl(repo_root, diff_path, check_only, false)
}

fn run_git_apply_cached(
    repo_root: &Path,
    diff_path: &Path,
    check_only: bool,
) -> Result<(), String> {
    run_git_apply_impl(repo_root, diff_path, check_only, true)
}

fn run_git_apply_reverse(repo_root: &Path, diff_path: &Path, cached: bool) -> Result<(), String> {
    run_git_apply_with_options(repo_root, diff_path, false, cached, true)
}

fn run_git_apply_impl(
    repo_root: &Path,
    diff_path: &Path,
    check_only: bool,
    cached: bool,
) -> Result<(), String> {
    run_git_apply_with_options(repo_root, diff_path, check_only, cached, false)
}

fn run_git_apply_with_options(
    repo_root: &Path,
    diff_path: &Path,
    check_only: bool,
    cached: bool,
    reverse: bool,
) -> Result<(), String> {
    let mut cmd = Command::new("git");
    cmd.arg("apply");
    // Force -p1: `validate_diff_targets` requires `a/`/`b/`-prefixed headers, so
    // strip exactly one path component. git's auto -p detection could otherwise
    // pick a different strip level and apply to a sibling path we never validated.
    cmd.arg("-p1");
    if check_only {
        cmd.arg("--check");
    }
    if cached {
        cmd.arg("--cached");
    }
    if reverse {
        cmd.arg("--reverse");
    }
    // --unidiff-zero tolerates LLMs that elide context;
    // --recount lets git recompute hunk sizes when @@ counts are stale.
    cmd.arg("--unidiff-zero");
    cmd.arg("--recount");
    cmd.arg(diff_path);
    cmd.current_dir(repo_root);

    let out = cmd
        .output()
        .map_err(|e| format!("failed to spawn git: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let trimmed = stderr.trim();
        Err(if trimmed.is_empty() {
            "git apply failed silently".into()
        } else {
            trimmed.to_owned()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_DIFF: &str = "--- a/src/foo.rs\n+++ b/src/foo.rs\n@@ -1,3 +1,3 @@\n fn x() {\n-    println!(\"old\");\n+    println!(\"new\");\n }\n";

    #[test]
    fn extracts_bare_diff() {
        let got = extract_unified_diff(SAMPLE_DIFF).unwrap();
        assert!(got.starts_with("--- a/src/foo.rs"));
        assert!(got.ends_with('\n'));
    }

    #[test]
    fn strips_markdown_fence_no_lang() {
        let raw = format!("```\n{SAMPLE_DIFF}```");
        let got = extract_unified_diff(&raw).unwrap();
        assert!(got.starts_with("--- a/src/foo.rs"));
        assert!(!got.contains("```"));
    }

    #[test]
    fn strips_markdown_fence_with_lang_tag() {
        let raw = format!("```diff\n{SAMPLE_DIFF}```");
        let got = extract_unified_diff(&raw).unwrap();
        assert!(got.starts_with("--- a/src/foo.rs"));
        assert!(!got.contains("```"));
    }

    #[test]
    fn tolerates_preamble_before_diff() {
        let raw = format!("Sure, here's the patch:\n\n{SAMPLE_DIFF}");
        let got = extract_unified_diff(&raw).unwrap();
        assert!(got.starts_with("--- a/src/foo.rs"));
        assert!(!got.contains("here's the patch"));
    }

    #[test]
    fn rejects_no_patch_sentinel() {
        assert!(extract_unified_diff("NO_PATCH").is_none());
        assert!(extract_unified_diff("Sorry,\nNO_PATCH\n").is_none());
    }

    #[test]
    fn rejects_response_without_diff_markers() {
        assert!(extract_unified_diff("Just some prose, no diff.").is_none());
    }

    #[test]
    fn rejects_partial_diff_missing_hunk_header() {
        let bad = "--- a/foo.rs\n+++ b/foo.rs\n(no hunks)\n";
        assert!(extract_unified_diff(bad).is_none());
    }

    #[test]
    fn appends_trailing_newline() {
        let no_newline = SAMPLE_DIFF.trim_end_matches('\n');
        let got = extract_unified_diff(no_newline).unwrap();
        assert!(got.ends_with('\n'));
    }

    fn issue_at(file: Option<&str>, line: Option<i32>, msg: &str) -> ReviewIssueRecord {
        ReviewIssueRecord {
            severity: "warning".into(),
            rule: "R".into(),
            rule_id: None,
            message: msg.into(),
            file: file.map(str::to_owned),
            line,
            suggestion: Some("do the thing".into()),
            source_badge: None,
            perspectives: Vec::new(),
            confidence: 0.9,
        }
    }

    fn issue_with_rule(rule_id: Option<&str>) -> ReviewIssueRecord {
        ReviewIssueRecord {
            severity: "warning".into(),
            rule: "R".into(),
            rule_id: rule_id.map(str::to_owned),
            message: "msg".into(),
            file: Some("src/foo.rs".into()),
            line: Some(1),
            suggestion: Some("do the thing".into()),
            source_badge: None,
            perspectives: Vec::new(),
            confidence: 0.9,
        }
    }

    #[test]
    fn unique_rule_ids_dedupes_and_drops_missing_ids() {
        let a = issue_with_rule(Some("rule-1"));
        let b = issue_with_rule(Some(" rule-2 "));
        let c = issue_with_rule(Some("rule-1"));
        let d = issue_with_rule(None);
        let e = issue_with_rule(Some(""));
        let issues = vec![&a, &b, &c, &d, &e];

        assert_eq!(unique_rule_ids(&issues), vec!["rule-1", "rule-2"]);
    }

    async fn cloud_rule_id_pool() -> sqlx::SqlitePool {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query("CREATE TABLE auth (key TEXT PRIMARY KEY, value TEXT)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("CREATE TABLE skills (id TEXT PRIMARY KEY, cloud_id TEXT)")
            .execute(&pool)
            .await
            .unwrap();
        pool
    }

    #[tokio::test]
    async fn resolve_acceptance_cloud_rule_ids_maps_known_local_ids() {
        let pool = cloud_rule_id_pool().await;
        let auth_cloud_id = "6105b2dd-5b7b-41a4-9af0-5e14c2b245fc";
        let skill_cloud_id = "d09b9631-01a9-4aa5-a4f5-cbed12c4c0de";
        let direct_cloud_id = "771e2e98-c010-4f9f-a387-45eabe55770a";
        sqlx::query("INSERT INTO auth (key, value) VALUES (?1, ?2)")
            .bind("rule_cloud_id:conv-review-aabbccdd")
            .bind(auth_cloud_id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO skills (id, cloud_id) VALUES (?1, ?2)")
            .bind("local-review")
            .bind(skill_cloud_id)
            .execute(&pool)
            .await
            .unwrap();

        let mut cache = std::collections::BTreeMap::new();
        let resolved = resolve_acceptance_cloud_rule_ids(
            &pool,
            vec![
                "conv-review-aabbccdd".to_owned(),
                "local-review".to_owned(),
                direct_cloud_id.to_owned(),
                "unmapped-local".to_owned(),
                "conv-review-aabbccdd".to_owned(),
                " ".to_owned(),
            ],
            &mut cache,
        )
        .await;

        assert_eq!(
            resolved,
            vec![
                auth_cloud_id.to_owned(),
                skill_cloud_id.to_owned(),
                direct_cloud_id.to_owned(),
            ]
        );
        assert!(matches!(cache.get("unmapped-local"), Some(None)));
    }

    #[tokio::test]
    async fn resolve_acceptance_cloud_rule_ids_omits_invalid_mappings() {
        let pool = cloud_rule_id_pool().await;
        sqlx::query("INSERT INTO auth (key, value) VALUES (?1, ?2)")
            .bind("rule_cloud_id:conv-bad-aabbccdd")
            .bind("not-a-cloud-uuid")
            .execute(&pool)
            .await
            .unwrap();

        let mut cache = std::collections::BTreeMap::new();
        let resolved = resolve_acceptance_cloud_rule_ids(
            &pool,
            vec!["conv-bad-aabbccdd".to_owned()],
            &mut cache,
        )
        .await;

        assert!(resolved.is_empty());
    }

    #[test]
    fn accepted_edit_upload_request_keeps_launch_grade_provenance() {
        let req = accepted_edit_upload_request(
            AcceptedEditProof {
                file_path: "src/lib.rs".into(),
                before_code: "old".into(),
                after_code: "new".into(),
                language: Some("rust".into()),
                diff_signature: "sig-1".into(),
                rule_ids: vec!["rule-1".into(), "rule-2".into()],
            },
            Some("difflore/difflore-cli"),
            Some(4543),
        );

        let value = serde_json::to_value(req).unwrap();
        assert_eq!(value["acceptanceSource"], "difflore_fix");
        assert_eq!(value["client"], "difflore_cli");
        assert_eq!(value["repoFullName"], "difflore/difflore-cli");
        assert_eq!(value["targetPrNumber"], 4543);
        assert_eq!(value["filePath"], "src/lib.rs");
        assert_eq!(value["language"], "rust");
        assert_eq!(value["diffSignature"], "sig-1");
        assert_eq!(value["ruleIds"][0], "rule-1");
        assert_eq!(value["ruleIds"][1], "rule-2");
    }

    /// A fully-migrated in-memory pool. `update_confidence` (and so
    /// `reinforce_rule`) is built on the `query!` macro, which is validated
    /// against the real schema, so the reinforcement tests need the actual
    /// `skills` + `rule_events` tables rather than a hand-rolled subset.
    async fn migrated_pool() -> sqlx::SqlitePool {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        difflore_core::db::run_migrations(&pool).await.unwrap();
        pool
    }

    async fn insert_active_skill(pool: &sqlx::SqlitePool, id: &str, confidence: f64) {
        sqlx::query(
            "INSERT INTO skills (id, name, source, directory, version, confidence_score, status)
             VALUES (?1, ?2, 'manual', '/tmp', '1.0.0', ?3, 'active')",
        )
        .bind(id)
        .bind(format!("name-{id}"))
        .bind(confidence)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn reinforce_rule_accept_bumps_confidence_and_writes_event() {
        // Accepting a fix must bump confidence and write a rule_events row.
        let pool = migrated_pool().await;
        insert_active_skill(&pool, "rule-accept", 0.80).await;

        reinforce_rule(
            &pool,
            "rule-accept",
            "Rule Accept",
            "accept",
            "fix_accepted",
        )
        .await;

        let after: f64 = sqlx::query_scalar("SELECT confidence_score FROM skills WHERE id = ?1")
            .bind("rule-accept")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(
            (after - 0.85).abs() < 1e-9,
            "accept must bump confidence by +0.05 (got {after})"
        );

        let events: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM rule_events WHERE skill_id = ?1 AND kind = 'feedback_accept'",
        )
        .bind("rule-accept")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(events, 1, "accept must write exactly one rule_events row");
    }

    #[tokio::test]
    async fn record_fix_outcomes_reinforces_each_applied_rule() {
        // End-to-end through the public entry point: an applied issue with
        // a real local rule_id reinforces that rule (+0.05) and leaves a
        // rule_events trail.
        let pool = migrated_pool().await;
        insert_active_skill(&pool, "rule-applied", 0.50).await;

        let mut outcome = ApplyOutcome::default();
        outcome.applied.push(OutcomeIssue {
            rule_id: Some("rule-applied".to_owned()),
            rule_name: "Rule Applied".to_owned(),
            file_path: Some("src/lib.rs".to_owned()),
            file_loc: "src/lib.rs:1".to_owned(),
        });

        record_fix_outcomes(&pool, &mut outcome, &[], None, None, false).await;

        let after: f64 = sqlx::query_scalar("SELECT confidence_score FROM skills WHERE id = ?1")
            .bind("rule-applied")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(
            (after - 0.55).abs() < 1e-9,
            "an applied fix must reinforce its rule by +0.05 (got {after})"
        );
    }

    #[tokio::test]
    async fn reinforce_rule_skips_unknown_rule_without_error() {
        // A rule_id that isn't a local skill (deleted / non-skill id) must
        // be skipped silently: update_confidence returns NotFound, we
        // fix_debug! it, and nothing is written or mutated. No panic, no
        // invented strength, no stray rule_events row.
        let pool = migrated_pool().await;

        // Must not panic even though "ghost-rule" has no skills row.
        reinforce_rule(&pool, "ghost-rule", "Ghost", "accept", "fix_accepted").await;

        let events: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM rule_events")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(events, 0, "unknown rule must not write a rule_events row");
    }

    #[tokio::test]
    async fn record_fix_outcomes_persists_target_pr_identity() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE fix_outcomes (
                id TEXT PRIMARY KEY NOT NULL,
                rule_id TEXT,
                rule_name TEXT NOT NULL,
                file_path TEXT,
                repo_full_name TEXT,
                pr_number INTEGER,
                diff_signature TEXT,
                accepted INTEGER NOT NULL,
                applied_ok INTEGER NOT NULL DEFAULT 0,
                failed_reason TEXT,
                created_at TEXT DEFAULT (datetime('now')) NOT NULL
            )",
        )
        .execute(&pool)
        .await
        .unwrap();

        let mut outcome = ApplyOutcome::default();
        outcome.applied.push(OutcomeIssue {
            rule_id: Some("rule-1".to_owned()),
            rule_name: "Rule 1".to_owned(),
            file_path: Some("src/lib.rs".to_owned()),
            file_loc: "src/lib.rs:1".to_owned(),
        });
        record_fix_outcomes(
            &pool,
            &mut outcome,
            &[],
            Some("acme/widgets"),
            Some(42),
            false,
        )
        .await;

        let row: (Option<String>, Option<i64>) =
            sqlx::query_as("SELECT repo_full_name, pr_number FROM fix_outcomes WHERE rule_id = ?1")
                .bind("rule-1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(row.0.as_deref(), Some("acme/widgets"));
        assert_eq!(row.1, Some(42));
    }

    fn accepted_edit_response(
        team_id: Option<&str>,
        observations_inserted: u32,
    ) -> RecordAcceptedEditResponse {
        RecordAcceptedEditResponse {
            ok: true,
            acceptance_recorded: true,
            acceptance_id: Some("acc-1".into()),
            diff_signature: Some("sig-1".into()),
            team_id: team_id.map(str::to_owned),
            attributed_rule_ids: Vec::new(),
            observations_inserted,
            memory_reinforcement_recorded: false,
            memory_reinforcement_deduped: false,
            error: None,
        }
    }

    #[test]
    fn accepted_edit_upload_summary_flags_missing_team_for_rule_linked_proof() {
        let mut summary = AcceptedEditUploadSummary::default();
        record_accepted_edit_upload_queued(&mut summary, 2, true);
        record_accepted_edit_upload_response(&mut summary, 2, &accepted_edit_response(None, 0));

        assert_eq!(summary.queued, 1);
        assert_eq!(summary.uploaded, 1);
        assert_eq!(summary.linked_observations, 0);
        assert_eq!(summary.missing_rule_ids, 0);
        assert_eq!(summary.missing_target_pr, 0);
        assert_eq!(summary.missing_team, 1);
        assert_eq!(summary.missing_rule_observation, 0);
    }

    #[test]
    fn accepted_edit_upload_summary_counts_linked_cloud_proof() {
        let mut summary = AcceptedEditUploadSummary::default();
        record_accepted_edit_upload_queued(&mut summary, 2, true);
        record_accepted_edit_upload_response(
            &mut summary,
            2,
            &accepted_edit_response(Some("team-1"), 2),
        );

        assert_eq!(summary.queued, 1);
        assert_eq!(summary.uploaded, 1);
        assert_eq!(summary.linked_observations, 2);
        assert_eq!(summary.missing_rule_ids, 0);
        assert_eq!(summary.missing_target_pr, 0);
        assert_eq!(summary.missing_team, 0);
        assert_eq!(summary.missing_rule_observation, 0);
    }

    #[test]
    fn accepted_edit_upload_summary_flags_unlinked_rule_observation() {
        let mut summary = AcceptedEditUploadSummary::default();
        record_accepted_edit_upload_queued(&mut summary, 2, true);
        record_accepted_edit_upload_response(
            &mut summary,
            2,
            &accepted_edit_response(Some("team-1"), 0),
        );

        assert_eq!(summary.queued, 1);
        assert_eq!(summary.uploaded, 1);
        assert_eq!(summary.linked_observations, 0);
        assert_eq!(summary.missing_rule_ids, 0);
        assert_eq!(summary.missing_target_pr, 0);
        assert_eq!(summary.missing_team, 0);
        assert_eq!(summary.missing_rule_observation, 1);
    }

    #[test]
    fn accepted_edit_upload_summary_flags_missing_rule_ids_before_upload() {
        let mut summary = AcceptedEditUploadSummary::default();
        record_accepted_edit_upload_queued(&mut summary, 0, true);
        record_accepted_edit_upload_response(
            &mut summary,
            0,
            &accepted_edit_response(Some("team-1"), 0),
        );

        assert_eq!(summary.queued, 1);
        assert_eq!(summary.uploaded, 1);
        assert_eq!(summary.linked_observations, 0);
        assert_eq!(summary.missing_rule_ids, 1);
        assert_eq!(summary.missing_target_pr, 0);
        assert_eq!(summary.missing_team, 0);
        assert_eq!(summary.missing_rule_observation, 0);
    }

    #[test]
    fn accepted_edit_upload_summary_flags_missing_target_pr_before_upload() {
        let mut summary = AcceptedEditUploadSummary::default();
        record_accepted_edit_upload_queued(&mut summary, 2, false);
        record_accepted_edit_upload_response(
            &mut summary,
            2,
            &accepted_edit_response(Some("team-1"), 2),
        );

        assert_eq!(summary.queued, 1);
        assert_eq!(summary.uploaded, 1);
        assert_eq!(summary.linked_observations, 2);
        assert_eq!(summary.missing_rule_ids, 0);
        assert_eq!(summary.missing_target_pr, 1);
        assert_eq!(summary.missing_team, 0);
        assert_eq!(summary.missing_rule_observation, 0);
    }

    #[test]
    fn accepted_edit_upload_summary_keeps_queued_rows_pending_attribution() {
        let mut summary = AcceptedEditUploadSummary::default();
        record_accepted_edit_upload_queued(&mut summary, 1, true);

        assert_eq!(summary.queued, 1);
        assert_eq!(summary.uploaded, 0);
        assert_eq!(summary.queued.saturating_sub(summary.uploaded), 1);
    }

    #[test]
    fn failure_reason_is_provider_misconfig_matches_known_auth_and_setup_errors() {
        // Provider/setup failures must be filtered before write.
        for reason in [
            "claude CLI ... Not logged in · Please run /login ...",
            "claude CLI ... Not logged in ...",
            "No active AI provider configured. Run `difflore providers setup`",
            "no LLM provider configured and no supported agent CLI found on PATH",
            "failed to spawn `claude` CLI: The filename or extension is too long",
        ] {
            assert!(
                failure_reason_is_provider_misconfig(reason),
                "expected provider-misconfig classification for: {reason}"
            );
        }
    }

    #[test]
    fn failure_reason_is_provider_misconfig_does_not_swallow_real_patch_failures() {
        // Fix-quality failures must still land in `fix_outcomes`.
        for reason in [
            "LLM returned no usable diff",
            "validation: error: corrupt patch at line 12",
            "issue has no file path",
            "apply: patch does not apply",
        ] {
            assert!(
                !failure_reason_is_provider_misconfig(reason),
                "must NOT silently drop real fix failure: {reason}"
            );
        }
    }

    #[tokio::test]
    async fn record_fix_outcomes_skips_provider_misconfig_failures() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE fix_outcomes (
                id TEXT PRIMARY KEY NOT NULL,
                rule_id TEXT,
                rule_name TEXT NOT NULL,
                file_path TEXT,
                repo_full_name TEXT,
                pr_number INTEGER,
                diff_signature TEXT,
                accepted INTEGER NOT NULL,
                applied_ok INTEGER NOT NULL DEFAULT 0,
                failed_reason TEXT,
                created_at TEXT DEFAULT (datetime('now')) NOT NULL
            )",
        )
        .execute(&pool)
        .await
        .unwrap();

        let mut outcome = ApplyOutcome::default();
        // Provider-misconfig failure — must be filtered out.
        outcome.failed.push((
            OutcomeIssue {
                rule_id: Some("rule-a".to_owned()),
                rule_name: "Rule A".to_owned(),
                file_path: Some("src/a.rs".to_owned()),
                file_loc: "src/a.rs:1".to_owned(),
            },
            "claude CLI ... Not logged in · Please run /login ...".to_owned(),
        ));
        // Real fix-quality failure — must still be recorded.
        outcome.failed.push((
            OutcomeIssue {
                rule_id: Some("rule-b".to_owned()),
                rule_name: "Rule B".to_owned(),
                file_path: Some("src/b.rs".to_owned()),
                file_loc: "src/b.rs:1".to_owned(),
            },
            "LLM returned no usable diff".to_owned(),
        ));

        record_fix_outcomes(&pool, &mut outcome, &[], None, None, false).await;

        let rows: Vec<(String, i64, i64, Option<String>)> = sqlx::query_as(
            "SELECT rule_name, accepted, applied_ok, failed_reason
             FROM fix_outcomes ORDER BY rule_name",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 1, "auth failure must not land in fix_outcomes");
        assert_eq!(rows[0].0, "Rule B");
        assert_eq!(rows[0].1, 1);
        assert_eq!(rows[0].2, 0);
        assert_eq!(rows[0].3.as_deref(), Some("LLM returned no usable diff"));
    }

    #[test]
    fn yes_mode_fails_when_confident_patch_generation_fails() {
        let mut outcome = ApplyOutcome::default();
        outcome.failed.push((
            OutcomeIssue {
                rule_id: None,
                rule_name: "Pin Actions".into(),
                file_path: Some(".github/workflows/pr.yml".into()),
                file_loc: ".github/workflows/pr.yml:12".into(),
            },
            "LLM returned no usable diff".into(),
        ));

        assert!(yes_mode_should_fail(&outcome));
    }

    #[test]
    fn batched_prompt_lists_every_suggestion() {
        let a = issue_at(Some("src/foo.ts"), Some(10), "issue A");
        let b = issue_at(Some("src/foo.ts"), Some(40), "issue B");
        let body = batched_patch_user_prompt("src/foo.ts", "fn x() {}\n", &[&a, &b]);
        assert!(body.contains("issue A"));
        assert!(body.contains("issue B"));
        assert!(body.contains("ONE unified diff"));
        assert!(body.contains("NO_PATCH"));
    }

    #[test]
    fn single_prompt_omits_line_number() {
        // Regression: pre-batching the user prompt embedded `(line N)` even when
        // later batched patches had shifted the file. Guard that it stays gone.
        let a = issue_at(Some("src/foo.ts"), Some(42), "do not write null");
        let body = patch_user_prompt("src/foo.ts", "fn x() {}\n", &a);
        assert!(!body.contains("line 42"));
        assert!(!body.contains("(line "));
        assert!(body.contains("do not write null"));
    }

    #[test]
    fn record_failure_pushes_one_entry_per_issue_with_same_reason() {
        let a = issue_at(Some("src/foo.rs"), Some(10), "issue A");
        let b = issue_at(Some("src/foo.rs"), Some(20), "issue B");
        let issues: Vec<&ReviewIssueRecord> = vec![&a, &b];
        let mut outcome = ApplyOutcome::default();
        record_failure(
            &mut outcome,
            &issues,
            "src/foo.rs",
            1,
            1,
            "validation: nope",
            false,
        );
        assert_eq!(outcome.failed.len(), 2);
        assert!(outcome.applied.is_empty());
        for (_, reason) in &outcome.failed {
            assert_eq!(reason, "validation: nope");
        }
    }

    #[test]
    fn record_failure_preserves_issue_metadata_in_outcome_issue() {
        let a = ReviewIssueRecord {
            severity: "warning".into(),
            rule: "Pin Actions".into(),
            rule_id: Some("pin-actions".into()),
            message: "msg".into(),
            file: Some(".github/workflows/pr.yml".into()),
            line: Some(12),
            suggestion: None,
            source_badge: None,
            perspectives: Vec::new(),
            confidence: 0.9,
        };
        let issues: Vec<&ReviewIssueRecord> = vec![&a];
        let mut outcome = ApplyOutcome::default();
        record_failure(
            &mut outcome,
            &issues,
            ".github/workflows/pr.yml:12",
            1,
            1,
            "boom",
            false,
        );
        let (issue, reason) = &outcome.failed[0];
        assert_eq!(issue.rule_id.as_deref(), Some("pin-actions"));
        assert_eq!(issue.rule_name, "Pin Actions");
        assert_eq!(reason, "boom");
    }

    #[test]
    fn rollback_pending_patches_reverses_worktree_changes() {
        let tmp = tempfile::tempdir().unwrap();
        Command::new("git")
            .arg("init")
            .current_dir(tmp.path())
            .output()
            .unwrap();
        // Pin line-ending handling so this test is deterministic across
        // platforms: GitHub's windows-latest runners default to
        // core.autocrlf=true, which makes `git apply` rewrite LF -> CRLF in the
        // worktree and breaks the byte-exact assertions below.
        for (key, value) in [("core.autocrlf", "false"), ("core.eol", "lf")] {
            Command::new("git")
                .args(["config", key, value])
                .current_dir(tmp.path())
                .output()
                .unwrap();
        }
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        let file = tmp.path().join("src/foo.txt");
        std::fs::write(&file, "old\n").unwrap();
        let diff = "--- a/src/foo.txt\n+++ b/src/foo.txt\n@@ -1 +1 @@\n-old\n+new\n";

        with_diff_tempfile(diff, |diff_path| {
            run_git_apply(tmp.path(), diff_path.unwrap(), false).unwrap();
        });
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "new\n");

        let mut outcome = ApplyOutcome::default();
        let mut pending = vec![PendingAppliedPatch {
            diff: diff.to_owned(),
            issues: vec![OutcomeIssue {
                rule_id: None,
                rule_name: "R".into(),
                file_path: Some("src/foo.txt".into()),
                file_loc: "src/foo.txt:1".into(),
            }],
            accepted_edit: None,
        }];

        rollback_pending_patches(
            &mut outcome,
            tmp.path(),
            false,
            &mut pending,
            "later file failed",
        );

        assert!(pending.is_empty());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "old\n");
        assert_eq!(outcome.failed.len(), 1);
        assert!(outcome.failed[0].1.contains("rolled back"));
    }

    #[test]
    fn focused_patch_context_keeps_nearby_issue_lines_for_large_files() {
        let content = (1..=120)
            .map(|n| format!("line {n} {}", "x".repeat(50)))
            .collect::<Vec<_>>()
            .join("\n");
        let focused = focused_patch_context(&content, Some(60));

        assert!(focused.contains("line 1"));
        assert!(focused.contains("line 60"));
        assert!(focused.contains("line 36"));
        assert!(focused.contains("line 84"));
        assert!(focused.contains("..."));
        assert!(!focused.contains("line 120"));
    }
}
