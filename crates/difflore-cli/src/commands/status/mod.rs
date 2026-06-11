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
use crate::support::util::{exit_err, init_db, project_path};

use queries::{
    LocalAcceptedProof, LocalHeroEvidence, LocalMcpRuleServe, LocalRecallProof,
    ProvenRuleDrilldown, ValueLoopEvidence,
};
use transform::{
    CandidatePreview, LaneStatusSummary, LocalValueLoopStatus, NextAction, NextActionInputs,
    RepoScopeStatus,
};

pub(crate) async fn handle_status(json: bool, lane: StatusLane) {
    let db = init_db().await;
    let project = project_path();

    let payload = match compute_status_payload(&db, &project, lane).await {
        Ok(payload) => payload,
        Err(message) => exit_err(&message),
    };

    if json {
        let json_value = payload.to_json_envelope();
        println!(
            "{}",
            crate::support::util::json_compact_or(&json_value, "{}")
        );
        return;
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
    let detected_repo_remotes = difflore_core::infra::git::detect_github_repo_full_names(&project);
    let repo_remotes =
        difflore_core::skills::expand_repo_scopes_with_source_aliases(db, &detected_repo_remotes)
            .await
            .unwrap_or(detected_repo_remotes);

    let local_proof = queries::local_accepted_proof(db, &repo_remotes).await;
    let local_recall_proof = queries::local_recall_proof(db, &repo_remotes).await;
    let local_mcp_serves = queries::local_mcp_rule_serves(db, &repo_remotes).await;

    CompactValueSummary::from_parts(&local_proof, &local_recall_proof, &local_mcp_serves)
}

pub(crate) fn render_compact_value_summary(summary: &CompactValueSummary) -> String {
    let mut parts = vec![
        format!(
            "~{} review-minute{} saved",
            summary.saved_review_minutes,
            transform::plural(summary.saved_review_minutes),
        ),
        format!(
            "{} recall{}",
            summary.recall_events,
            transform::plural(summary.recall_events),
        ),
    ];
    if summary.agent_serves > 0 {
        parts.push(format!(
            "{} ready for agent{}",
            summary.agent_serves,
            transform::plural(summary.agent_serves),
        ));
    }
    if summary.accepted_edits > 0 {
        parts.push(format!(
            "{} accepted edit{}",
            summary.accepted_edits,
            transform::plural(summary.accepted_edits),
        ));
    }

    format!(
        "Value (last {}d): {}",
        summary.window_days,
        parts.join(" | ")
    )
}

/// Bundled output of the status pipeline for both JSON and text rendering.
#[derive(Debug)]
struct StatusPayload {
    active_rules: i64,
    pending_candidates: i64,
    pending_candidates_for_repo: i64,
    scope: RepoScopeStatus,
    value_loop: LocalValueLoopStatus,
    local_proof: LocalAcceptedProof,
    local_recall_proof: LocalRecallProof,
    local_mcp_serves: LocalMcpRuleServe,
    proven_rule: Option<ProvenRuleDrilldown>,
    value_loop_evidence: Option<ValueLoopEvidence>,
    local_hero_evidence: Option<LocalHeroEvidence>,
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
            "activeRules": self.active_rules,
            "pendingCandidates": self.pending_candidates,
            "pendingCandidatesForRepo": self.pending_candidates_for_repo,
            "repoScope": self.scope,
            "valueLoop": self.value_loop,
            "localAcceptedProof": self.local_proof,
            "localRecallProof": self.local_recall_proof,
            "localMcpRuleServes": self.local_mcp_serves,
            "provenRuleDrilldown": self.proven_rule,
            "valueLoopEvidence": self.value_loop_evidence,
            "localHeroEvidence": self.local_hero_evidence,
            "topCandidatesScope": self.candidate_scope,
            "topCandidates": self.top_candidates,
            "next": self.next,
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
        presentation::render_text(
            self.active_rules,
            self.pending_candidates,
            self.pending_candidates_for_repo,
            &self.scope,
            &self.local_proof,
            &self.local_recall_proof,
            &self.local_mcp_serves,
            self.proven_rule.as_ref(),
            self.local_hero_evidence.as_ref(),
            self.candidate_scope,
            &self.top_candidates,
            &self.next,
            &self.proof_path,
            self.selected_lane.as_str(),
            &self.lane_status,
            &self.embedding,
        )
    }

    fn print_text(&self) {
        print!("{}", self.text_view());
    }
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

    let detected_repo_remotes = difflore_core::infra::git::detect_github_repo_full_names(project);
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
    let proven_rule = queries::local_proven_rule_drilldown(db, &repo_remotes).await;
    let value_loop_evidence = queries::local_value_loop_evidence(db, &repo_remotes).await;
    let local_hero_evidence = queries::local_hero_evidence(db, &repo_remotes).await;
    let cloud_logged_in = difflore_core::cloud::client::CloudClient::create()
        .await
        .is_logged_in();
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
        cloud_logged_in,
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
        scope,
        value_loop,
        local_proof,
        local_recall_proof,
        local_mcp_serves,
        proven_rule,
        value_loop_evidence,
        local_hero_evidence,
        candidate_scope,
        top_candidates,
        next,
        proof_path,
        selected_lane,
        lane_status,
        embedding,
    })
}

#[cfg(test)]
#[allow(unsafe_code)] // reason: `env::set_var` is unsafe in 2024 edition; SAFETY comment documents the OnceLock invariant.
mod tests {
    use super::*;

    /// Point DIFFLORE_HOME at a process-unique tempdir so the per-project index
    /// DB is isolated per test process. Without it, parallel nextest processes
    /// contend on a shared on-disk index DB ("database is locked").
    fn ensure_test_home() {
        use std::sync::OnceLock;
        use tempfile::TempDir;
        static HOME: OnceLock<TempDir> = OnceLock::new();
        HOME.get_or_init(|| {
            let dir = TempDir::new().expect("create status test home tempdir");
            // SAFETY: OnceLock runs this once per process; the var is never removed.
            unsafe {
                std::env::set_var("DIFFLORE_HOME", dir.path());
            }
            dir
        });
    }

    /// Locks the JSON envelope against an empty in-memory pool so
    /// query/transform/render changes cannot silently drift.
    #[tokio::test]
    async fn empty_pool_payload_has_zero_shape() {
        use sqlx::sqlite::SqlitePoolOptions;

        ensure_test_home();

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
                "next",
                "pendingCandidates",
                "pendingCandidatesForRepo",
                "proofPath",
                "provenRuleDrilldown",
                "repoScope",
                "selectedLane",
                "topCandidates",
                "topCandidatesScope",
                "valueLoop",
                "valueLoopEvidence",
            ]
        );

        // Counters must be zero on an empty corpus.
        assert_eq!(envelope["activeRules"], 0);
        assert_eq!(envelope["pendingCandidates"], 0);
        assert_eq!(envelope["pendingCandidatesForRepo"], 0);
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
        // With no GitHub origin, the humanized view surfaces a plain
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
}
