//! `difflore status` -- local-only proof that the CLI has usable memory.
//!
//! Three-layer split:
//! - `queries`: SQL + DTOs the queries materialise.
//! - `transform`: pure helpers (scope, next-action, candidate previews, …).
//! - `presentation`: text-mode renderers; JSON envelope stays here.

mod presentation;
mod queries;
mod transform;

use crate::cli::StatusLane;
use crate::commands::ai_contract::{CLI_SCHEMA_VERSION, NextActionContract};
use crate::support::util::{init_db, project_path};
use sqlx::Row;
use std::collections::BTreeMap;

use queries::{
    LocalAcceptedProof, LocalHeroEvidence, LocalMcpRuleServe, LocalRecallProof, MemoryInboxSummary,
    ProvenRuleDrilldown, ValueLoopEvidence,
};
use transform::{
    CandidatePreview, LaneStatusSummary, LocalValueLoopStatus, NextAction, NextActionInputs,
    RepoScopeStatus,
};

pub(crate) async fn handle_status(json: bool, lane: StatusLane) -> anyhow::Result<()> {
    let db = init_db().await;
    let project = project_path();

    let payload = compute_status_payload(&db, &project, lane)
        .await
        .map_err(anyhow::Error::msg)?;

    if json {
        let json_value = payload.to_json_envelope();
        println!(
            "{}",
            crate::support::util::json_compact_or(&json_value, "{}")
        );
        return Ok(());
    }

    payload.print_text();
    if let Some(nudge) = crate::installer::agent_update_nudge() {
        println!();
        println!(
            "  {} {}",
            crate::style::emerald(crate::style::sym::TIP),
            crate::style::pewter(&nudge),
        );
    }

    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompactValueSummary {
    pub(crate) window_days: i64,
    pub(crate) accepted_edits: i64,
    pub(crate) saved_review_minutes: i64,
    pub(crate) recall_events: i64,
    pub(crate) agent_serves: i64,
}

impl CompactValueSummary {
    const fn from_parts(
        accepted: &LocalAcceptedProof,
        recall: &LocalRecallProof,
        mcp_serves: &LocalMcpRuleServe,
    ) -> Self {
        Self {
            window_days: accepted.window_days,
            accepted_edits: accepted.accepted_proof_signatures + accepted.accepted_hook_outcomes,
            saved_review_minutes: accepted.estimated_saved_review_minutes,
            recall_events: recall.recall_events,
            agent_serves: mcp_serves.calls.saturating_sub(mcp_serves.empty_calls),
        }
    }
}

pub(crate) async fn compact_value_summary_for_current_project(
    db: &difflore_core::SqlitePool,
) -> CompactValueSummary {
    let project = project_path();
    let configured_gitlab_hosts = difflore_core::ingest::gitlab::auth::configured_hosts().await;
    let detected_repo_remotes = difflore_core::infra::git::detect_repo_full_names_with_gitlab_hosts(
        &project,
        &configured_gitlab_hosts,
    );
    let repo_remotes =
        difflore_core::skills::expand_repo_scopes_with_source_aliases(db, &detected_repo_remotes)
            .await
            .unwrap_or(detected_repo_remotes);

    let local_proof = queries::local_accepted_proof(db, &repo_remotes).await;
    let local_recall_proof = queries::local_recall_proof(db, &repo_remotes).await;
    let local_mcp_serves = queries::local_mcp_rule_serves(db, &repo_remotes).await;

    CompactValueSummary::from_parts(&local_proof, &local_recall_proof, &local_mcp_serves)
}

pub(crate) fn render_compact_value_summary(summary: &CompactValueSummary) -> Option<String> {
    if summary.accepted_edits > 0 {
        return Some(format!(
            "Value (last {}d): {} accepted edit{}",
            summary.window_days,
            summary.accepted_edits,
            transform::plural(summary.accepted_edits),
        ));
    }

    let mut parts = Vec::new();
    if summary.recall_events > 0 {
        parts.push(format!(
            "{} recall{}",
            summary.recall_events,
            transform::plural(summary.recall_events),
        ));
    }
    if summary.agent_serves > 0 {
        parts.push(format!(
            "{} ready for agent{}",
            summary.agent_serves,
            transform::plural(summary.agent_serves),
        ));
    }
    if parts.is_empty() {
        return None;
    }

    Some(format!(
        "Readiness (last {}d): {}",
        summary.window_days,
        parts.join(" | ")
    ))
}

/// Bundled output of the status pipeline for both JSON and text rendering.
#[derive(Debug)]
struct StatusPayload {
    active_rules: i64,
    pending_candidates: i64,
    pending_candidates_for_repo: i64,
    memory_inbox: MemoryInboxSummary,
    scope: RepoScopeStatus,
    value_loop: LocalValueLoopStatus,
    local_proof: LocalAcceptedProof,
    local_recall_proof: LocalRecallProof,
    local_mcp_serves: LocalMcpRuleServe,
    recall_trace: RecallTraceSummary,
    proven_rule: Option<ProvenRuleDrilldown>,
    value_loop_evidence: Option<ValueLoopEvidence>,
    local_hero_evidence: Option<LocalHeroEvidence>,
    autopilot: difflore_core::memory_autopilot_schedule::MemoryAutopilotScheduleStatus,
    memory_pulse: MemoryPulseStatus,
    candidate_scope: &'static str,
    top_candidates: Vec<CandidatePreview>,
    next: NextAction,
    proof_path: Vec<String>,
    selected_lane: StatusLane,
    lane_status: LaneStatusSummary,
    /// Index-vs-active embedding profile match. A mismatch silently disables
    /// the vector lane and forces FTS-only retrieval; surface it so `--json`
    /// consumers can detect a stale index without running `doctor`.
    embedding: difflore_core::context::EmbeddingDiagnostics,
}

impl StatusPayload {
    fn to_json_envelope(&self) -> serde_json::Value {
        serde_json::json!({
            "schemaVersion": CLI_SCHEMA_VERSION,
            "activeRules": self.active_rules,
            "pendingCandidates": self.pending_candidates,
            "pendingCandidatesForRepo": self.pending_candidates_for_repo,
            "memoryInbox": self.memory_inbox,
            "repoScope": self.scope,
            "valueLoop": self.value_loop,
            "localAcceptedProof": self.local_proof,
            "localRecallProof": self.local_recall_proof,
            "localMcpRuleServes": self.local_mcp_serves,
            "recallTrace": self.recall_trace,
            "provenRuleDrilldown": self.proven_rule,
            "valueLoopEvidence": self.value_loop_evidence,
            "localHeroEvidence": self.local_hero_evidence,
            "autopilot": self.autopilot,
            "memoryPulse": self.memory_pulse,
            "topCandidatesScope": self.candidate_scope,
            "topCandidates": self.top_candidates,
            "next": NextActionContract::with_blocked_by(
                self.next.command.clone(),
                self.next.reason.clone(),
                self.next.blocked_by.clone()
            ),
            "proofPath": self.proof_path,
            "selectedLane": self.selected_lane.as_str(),
            "laneStatus": self.lane_status,
            "embeddingActiveProfile": self.embedding.active_profile,
            "embeddingIndexProfile": self.embedding.index_profile,
            "embeddingProfileMatch": self.embedding.profile_match,
            "embeddingDegraded": self.embedding.degraded,
            "embeddingDegradedReason": self.embedding.degraded_reason,
        })
    }

    fn text_view(&self) -> String {
        let mut out = presentation::render_text(&presentation::StatusTextView {
            active_rules: self.active_rules,
            pending_candidates: self.pending_candidates,
            pending_candidates_for_repo: self.pending_candidates_for_repo,
            memory_inbox: &self.memory_inbox,
            scope: &self.scope,
            local_proof: &self.local_proof,
            local_recall_proof: &self.local_recall_proof,
            local_mcp_serves: &self.local_mcp_serves,
            recall_trace: &self.recall_trace,
            proven_rule: self.proven_rule.as_ref(),
            local_hero_evidence: self.local_hero_evidence.as_ref(),
            candidate_scope: self.candidate_scope,
            top_candidates: &self.top_candidates,
            next: &self.next,
            proof_path: &self.proof_path,
            embedding: &self.embedding,
        });
        append_memory_pulse_text(&mut out, &self.memory_pulse);
        out
    }

    fn print_text(&self) {
        print!("{}", self.text_view());
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct MemoryPulseStatus {
    newly_active: Vec<String>,
    to_confirm: Vec<String>,
}

fn append_memory_pulse_text(out: &mut String, pulse: &MemoryPulseStatus) {
    if pulse.newly_active.is_empty() && pulse.to_confirm.is_empty() {
        return;
    }
    use std::fmt::Write as _;
    let bullet = crate::style::pewter(crate::style::sym::BULLET);
    let _ = writeln!(out);
    let _ = writeln!(out, "{}", crate::style::ok("Memory pulse"));
    if !pulse.newly_active.is_empty() {
        let _ = writeln!(
            out,
            "  {bullet} newly active: {}",
            pulse.newly_active.join("; ")
        );
    }
    if !pulse.to_confirm.is_empty() {
        let _ = writeln!(
            out,
            "  {bullet} to confirm: {}",
            pulse.to_confirm.join("; ")
        );
    }
}

async fn memory_pulse_status(
    db: &difflore_core::SqlitePool,
    repo_aliases: &[String],
) -> MemoryPulseStatus {
    if repo_aliases.is_empty() {
        return MemoryPulseStatus::default();
    }
    let aliases = repo_aliases
        .iter()
        .map(|alias| alias.to_ascii_lowercase())
        .collect::<Vec<_>>();
    let Ok(repos_json) = serde_json::to_string(&aliases) else {
        return MemoryPulseStatus::default();
    };
    if sqlx::query(
        "CREATE TABLE IF NOT EXISTS memory_autopilot_events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            event_type TEXT NOT NULL,
            rule_id TEXT,
            item_ids_json TEXT NOT NULL DEFAULT '[]',
            group_id TEXT,
            title TEXT NOT NULL DEFAULT '',
            reason TEXT NOT NULL DEFAULT '',
            payload_json TEXT NOT NULL DEFAULT '{}',
            created_at TEXT DEFAULT (datetime('now')) NOT NULL
        )",
    )
    .execute(db)
    .await
    .is_err()
    {
        return MemoryPulseStatus::default();
    }
    let Ok(rows) = sqlx::query(
        r"SELECT event_type, title
          FROM memory_autopilot_events
          WHERE datetime(created_at) > datetime('now', '-7 days')
            AND event_type IN (
                'auto_enabled',
                'agent_file_review_rule_pending',
                'candidate_confirm_pending'
            )
            AND group_id IS NOT NULL
            AND EXISTS (
                SELECT 1
                FROM json_each(?1)
                WHERE LOWER(group_id) LIKE value || ':%'
            )
          ORDER BY id DESC
          LIMIT 8",
    )
    .bind(repos_json)
    .fetch_all(db)
    .await
    else {
        return MemoryPulseStatus::default();
    };

    let mut pulse = MemoryPulseStatus::default();
    for row in rows {
        let event_type: String = row.try_get("event_type").unwrap_or_default();
        let title: String = row.try_get("title").unwrap_or_default();
        let title = title.trim();
        if title.is_empty() {
            continue;
        }
        match event_type.as_str() {
            "auto_enabled" if pulse.newly_active.len() < 3 => {
                pulse.newly_active.push(title.to_owned());
            }
            "agent_file_review_rule_pending" | "candidate_confirm_pending"
                if pulse.to_confirm.len() < 3 =>
            {
                pulse.to_confirm.push(title.to_owned());
            }
            _ => {}
        }
    }
    pulse
}

async fn compute_status_payload(
    db: &difflore_core::SqlitePool,
    project: &str,
    selected_lane: StatusLane,
) -> Result<StatusPayload, String> {
    let stats = difflore_core::skills::stats(db)
        .await
        .map_err(|e| format!("failed to load local rule stats: {e}"))?;
    let all_candidates = difflore_core::skills::list_candidates(db, None, None)
        .await
        .map_err(|e| format!("failed to load local candidates: {e}"))?;
    let pending_candidates = all_candidates.len() as i64;
    let active_rules = difflore_core::skills::list(db)
        .await
        .map_err(|e| format!("failed to load local rules: {e}"))?;
    let source_repos = difflore_core::skills::list_source_repos(db)
        .await
        .unwrap_or_default();

    let configured_gitlab_hosts = difflore_core::ingest::gitlab::auth::configured_hosts().await;
    let detected_repo_remotes = difflore_core::infra::git::detect_repo_full_names_with_gitlab_hosts(
        project,
        &configured_gitlab_hosts,
    );
    let repo_remotes =
        difflore_core::skills::expand_repo_scopes_with_source_aliases(db, &detected_repo_remotes)
            .await
            .unwrap_or(detected_repo_remotes);
    let repo_full_name = repo_remotes.first().cloned();
    let review_source_repo_full_name = repo_remotes.get(1).cloned();
    let repo_candidates = if let Some(repo) = repo_full_name.as_deref() {
        difflore_core::skills::list_candidates(db, Some(repo), None)
            .await
            .map_err(|e| format!("failed to load repo-scoped candidates: {e}"))?
    } else {
        Vec::new()
    };
    let pending_candidates_for_repo = repo_candidates.len() as i64;
    let (candidate_scope, candidate_source) = if repo_candidates.is_empty() {
        ("none", &[][..])
    } else {
        ("currentRepo", repo_candidates.as_slice())
    };
    let top_candidates = transform::candidate_previews(candidate_source, 3);
    let scoped_active_rules =
        transform::count_rules_for_repo(&active_rules, &source_repos, repo_full_name.as_deref());
    let review_source_active_rules = transform::count_rules_for_repo(
        &active_rules,
        &source_repos,
        review_source_repo_full_name.as_deref(),
    );
    let scope = transform::repo_scope_status(
        repo_full_name,
        review_source_repo_full_name,
        scoped_active_rules,
        review_source_active_rules,
    );
    let local_proof = queries::local_accepted_proof(db, &repo_remotes).await;
    let local_recall_proof = queries::local_recall_proof(db, &repo_remotes).await;
    let local_mcp_serves = queries::local_mcp_rule_serves(db, &repo_remotes).await;
    let recall_trace = RecallTraceSummary::from_injection_log(
        difflore_core::observability::injection_log::summary_24h(),
    );
    let proven_rule = queries::local_proven_rule_drilldown(db, &repo_remotes).await;
    let value_loop_evidence = queries::local_value_loop_evidence(db, &repo_remotes).await;
    let local_hero_evidence = queries::local_hero_evidence(db, &repo_remotes).await;
    let cloud_logged_in = difflore_core::cloud::client::CloudClient::create()
        .await
        .is_logged_in();
    let memory_inbox =
        queries::memory_inbox_summary(db, stats.total, pending_candidates, cloud_logged_in).await;
    let autopilot = difflore_core::memory_autopilot_schedule::load_autopilot_schedule_status(db)
        .await
        .map_err(|e| format!("failed to load memory autopilot status: {e}"))?;
    let memory_pulse = memory_pulse_status(db, &repo_remotes).await;
    let value_loop = transform::local_value_loop_status(
        &scope,
        pending_candidates_for_repo,
        &local_recall_proof,
        &local_mcp_serves,
        &local_proof,
        value_loop_evidence.as_ref(),
    );
    // `db` is the main rule DB; embedding diagnostics need the per-project
    // index pool (where the embed profile is persisted), acquired the same
    // way doctor + self-recall do via `get_pool_for_cwd`.
    let index_pool = difflore_core::context::index_db::get_pool_for_cwd()
        .await
        .map_err(|e| format!("failed to open context index pool: {e}"))?;
    let embedding =
        difflore_core::context::gather_embedding_diagnostics_with_activity(&index_pool).await;
    let next = transform::next_action(&NextActionInputs {
        active_rules: stats.total,
        pending_candidates,
        pending_candidates_for_repo,
        session_mined_candidates: memory_inbox.local_discoveries.session_mined_candidates,
        cloud_logged_in,
        team_ready: memory_inbox.cloud.team_ready,
        scope: &scope,
        local_proof: &local_proof,
        local_recall_proof: &local_recall_proof,
        local_mcp_serves: &local_mcp_serves,
    });
    let proof_path = transform::proof_path_commands(&next, cloud_logged_in);
    let lane_status = transform::lane_status_summary(selected_lane.as_str(), &value_loop, &next);

    Ok(StatusPayload {
        active_rules: stats.total,
        pending_candidates,
        pending_candidates_for_repo,
        memory_inbox,
        scope,
        value_loop,
        local_proof,
        local_recall_proof,
        local_mcp_serves,
        recall_trace,
        proven_rule,
        value_loop_evidence,
        local_hero_evidence,
        autopilot,
        memory_pulse,
        candidate_scope,
        top_candidates,
        next,
        proof_path,
        selected_lane,
        lane_status,
        embedding,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RecallTraceSummary {
    pub(super) window_hours: i64,
    pub(super) scope: &'static str,
    pub(super) events: usize,
    pub(super) total_rules_injected: usize,
    pub(super) by_path: BTreeMap<String, usize>,
    pub(super) injected_by_path: BTreeMap<String, usize>,
    pub(super) dropped_by_reason: BTreeMap<String, usize>,
    pub(super) log_path: Option<String>,
    pub(super) detail: Option<String>,
}

impl RecallTraceSummary {
    fn from_injection_log(
        summary: difflore_core::observability::injection_log::InjectionPathSummary,
    ) -> Self {
        Self {
            window_hours: 24,
            scope: "machine",
            events: summary.count_24h,
            total_rules_injected: summary.total_rules_injected,
            by_path: summary.by_path,
            injected_by_path: summary.injected_by_path,
            dropped_by_reason: summary.dropped_by_reason,
            log_path: summary.path.map(|path| path.display().to_string()),
            detail: summary.detail,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::support::test_home::pin_test_home;

    /// Locks the JSON envelope against an empty in-memory pool so
    /// query/transform/render changes cannot silently drift.
    #[tokio::test]
    async fn empty_pool_payload_has_zero_shape() {
        use sqlx::sqlite::SqlitePoolOptions;

        pin_test_home();

        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("open pool");
        difflore_core::infra::db::run_migrations(&pool)
            .await
            .expect("apply migrations");

        let payload = compute_status_payload(&pool, "/tmp/status-test", StatusLane::All)
            .await
            .expect("compute payload");
        let envelope = payload.to_json_envelope();

        // Top-level keys (order-independent — JSON object semantics).
        let object = envelope.as_object().expect("object envelope");
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "activeRules",
                "autopilot",
                "embeddingActiveProfile",
                "embeddingDegraded",
                "embeddingDegradedReason",
                "embeddingIndexProfile",
                "embeddingProfileMatch",
                "laneStatus",
                "localAcceptedProof",
                "localHeroEvidence",
                "localMcpRuleServes",
                "localRecallProof",
                "memoryInbox",
                "memoryPulse",
                "next",
                "pendingCandidates",
                "pendingCandidatesForRepo",
                "proofPath",
                "provenRuleDrilldown",
                "recallTrace",
                "repoScope",
                "schemaVersion",
                "selectedLane",
                "topCandidates",
                "topCandidatesScope",
                "valueLoop",
                "valueLoopEvidence",
            ]
        );

        // Counters must be zero on an empty corpus.
        assert_eq!(envelope["schemaVersion"], CLI_SCHEMA_VERSION);
        assert_eq!(envelope["activeRules"], 0);
        assert_eq!(envelope["pendingCandidates"], 0);
        assert_eq!(envelope["pendingCandidatesForRepo"], 0);
        assert_eq!(envelope["autopilot"]["enabled"], true);
        assert_eq!(envelope["autopilot"]["dirty"], false);
        assert_eq!(envelope["autopilot"]["triggerCount"], 0);
        assert_eq!(envelope["autopilot"]["runCount"], 0);
        assert_eq!(envelope["autopilot"]["productiveRunCount"], 0);
        assert_eq!(envelope["memoryInbox"]["activeRules"], 0);
        assert_eq!(envelope["memoryInbox"]["localDrafts"], 0);
        assert_eq!(
            envelope["memoryInbox"]["localDiscoveries"]["sessionMinedCandidates"],
            0
        );
        assert_eq!(
            envelope["memoryInbox"]["localDiscoveries"]["latest"],
            serde_json::json!([])
        );
        assert_eq!(
            envelope["memoryInbox"]["queues"]["cloudOutbox"],
            serde_json::json!([])
        );
        assert_eq!(envelope["memoryInbox"]["queues"]["sessionMinedPending"], 0);
        assert!(
            !envelope["memoryInbox"]["cloud"]["loggedIn"]
                .as_bool()
                .expect("cloud logged-in flag")
        );
        assert!(envelope["memoryInbox"]["cloud"]["teamReady"].is_null());
        assert_eq!(envelope["localAcceptedProof"]["acceptedProofSignatures"], 0);
        let accepted_proof_signatures = envelope["localAcceptedProof"]["acceptedProofSignatures"]
            .as_i64()
            .expect("accepted proof signatures count");
        let accepted_hook_outcomes = envelope["localAcceptedProof"]["acceptedHookOutcomes"]
            .as_i64()
            .expect("accepted hook outcomes count");
        assert_eq!(
            envelope["localAcceptedProof"]["estimatedSavedReviewMinutes"],
            (accepted_proof_signatures + accepted_hook_outcomes) * 4
        );
        assert_eq!(envelope["localRecallProof"]["recallEvents"], 0);
        assert_eq!(envelope["localMcpRuleServes"]["calls"], 0);
        assert_eq!(envelope["recallTrace"]["windowHours"], 24);
        assert!(envelope["recallTrace"]["events"].is_number());
        assert!(envelope["recallTrace"]["droppedByReason"].is_object());
        assert!(envelope["provenRuleDrilldown"].is_null());
        assert!(envelope["valueLoopEvidence"].is_null());
        assert!(envelope["localHeroEvidence"].is_null());
        assert_eq!(envelope["topCandidates"], serde_json::json!([]));
        assert_eq!(envelope["topCandidatesScope"], "none");

        // Repo scope: no remote detected => unscoped, with reason.
        assert_eq!(envelope["repoScope"]["scopedRecallReady"], false);
        assert!(envelope["repoScope"]["repoFullName"].is_null());

        // Next action exists with a command + reason and proof path may be empty.
        assert!(envelope["next"]["command"].is_string());
        assert!(envelope["next"]["reason"].is_string());
        assert!(envelope["next"]["safetyTier"].is_number());
        assert!(envelope["next"]["sideEffects"].is_array());
        assert!(envelope["next"]["requiresUserIntent"].is_boolean());
        assert!(
            envelope["next"]["jsonCommand"].is_string()
                || envelope["next"]["jsonCommand"].is_null()
        );
        assert!(envelope["proofPath"].is_array());
        assert_eq!(envelope["selectedLane"], "all");
        assert!(envelope["laneStatus"]["localBeta"]["ready"].is_boolean());
        assert!(envelope["laneStatus"]["localBeta"]["status"].is_string());
        assert_eq!(
            envelope["laneStatus"]["localBeta"]["countsAsProductionEvidence"],
            false
        );
        assert_eq!(envelope["laneStatus"]["productionGa"]["ready"], false);
        assert_eq!(
            envelope["laneStatus"]["productionGa"]["status"],
            "blocked_external_release_gates_required"
        );
        assert_eq!(
            envelope["laneStatus"]["productionGa"]["releaseReadyInfluence"],
            "none"
        );

        // Embedding diagnostics: stable shape regardless of the resolved
        // index profile. Active profile is always a non-empty string; the
        // match + degraded flags are booleans; index/reason are string-or-null.
        assert!(
            envelope["embeddingActiveProfile"]
                .as_str()
                .is_some_and(|p| !p.is_empty())
        );
        assert!(envelope["embeddingProfileMatch"].is_boolean());
        assert!(envelope["embeddingDegraded"].is_boolean());
        assert!(
            envelope["embeddingIndexProfile"].is_string()
                || envelope["embeddingIndexProfile"].is_null()
        );
        assert!(
            envelope["embeddingDegradedReason"].is_string()
                || envelope["embeddingDegradedReason"].is_null()
        );
    }

    /// The human text view leads with value and must never leak the internal
    /// release-gate / evidence vocabulary that lives in the `--json` envelope.
    #[tokio::test]
    async fn empty_pool_text_view_is_value_first_without_gate_jargon() {
        use sqlx::sqlite::SqlitePoolOptions;

        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("open pool");
        difflore_core::infra::db::run_migrations(&pool)
            .await
            .expect("apply migrations");

        let payload = compute_status_payload(&pool, "/tmp/status-test", StatusLane::All)
            .await
            .expect("compute payload");
        let text = payload.text_view();

        // Value-first, human framing via the plain section headers.
        assert!(text.contains("Memory"), "missing Memory section: {text}");
        assert!(text.contains("Value"), "missing Value section: {text}");
        assert!(text.contains("next:"), "missing next action: {text}");
        // With no supported origin, the humanized view surfaces a plain
        // Repository section; release-gate details stay in JSON.
        assert!(
            text.contains("Repository"),
            "missing Repository section: {text}"
        );

        // Internal release-gate / evidence vocabulary must not leak here; it
        // belongs in the --json envelope, not the human view.
        for jargon in [
            "Lane boundary",
            "countsAsProductionEvidence",
            "releaseReadyInfluence",
            "memory-use proof",
            "context tokens",
            "accepted edit proof",
        ] {
            assert!(
                !text.contains(jargon),
                "human status text leaked gate jargon `{jargon}`: {text}"
            );
        }
    }

    #[test]
    fn compact_summary_uses_value_only_after_accepted_edits() {
        let value = render_compact_value_summary(&CompactValueSummary {
            window_days: 30,
            accepted_edits: 2,
            saved_review_minutes: 8,
            recall_events: 5,
            agent_serves: 64,
        })
        .expect("accepted edits should produce a value line");

        assert_eq!(value, "Value (last 30d): 2 accepted edits");
        assert!(!value.contains("top memory"));
        assert!(!value.contains("ready for agents"));
    }

    #[test]
    fn compact_summary_without_accepted_edits_is_readiness_not_negative_value() {
        let readiness = render_compact_value_summary(&CompactValueSummary {
            window_days: 30,
            accepted_edits: 0,
            saved_review_minutes: 0,
            recall_events: 5,
            agent_serves: 64,
        })
        .expect("activity should produce a readiness line");

        assert_eq!(
            readiness,
            "Readiness (last 30d): 5 recalls | 64 ready for agents"
        );
        assert!(!readiness.contains("no accepted edits yet"));
    }

    #[test]
    fn compact_summary_stays_silent_when_there_is_no_signal() {
        assert!(
            render_compact_value_summary(&CompactValueSummary {
                window_days: 30,
                accepted_edits: 0,
                saved_review_minutes: 0,
                recall_events: 0,
                agent_serves: 0,
            })
            .is_none()
        );
    }
}
