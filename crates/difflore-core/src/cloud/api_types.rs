openapi_contract::generate_types!("openapi-spec.json");

// ── Review-memory recall types ──
//
// Hand-written DTOs for fields and derives not provided by the generated
// OpenAPI types.

/// Request body for `POST /reviews/recallPastVerdicts`.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecallPastVerdictsRequest {
    /// Embedding vector of the current chunk / diff. Must be 1024 floats
    /// (Voyage `voyage-code-3`) when present. Optional when
    /// `query_text` is provided — the server will embed server-side,
    /// which avoids any algorithm/dim drift between client and server.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub embedding: Vec<f32>,
    /// Raw query text for server-side embedding. Prefer this when the
    /// client lacks a compatible 1024-dim embedder.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query_text: Option<String>,
    /// Repository identifier so the cloud can scope recall to a single repo.
    /// `None` means "any repo this user can see".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_id: Option<String>,
    /// "personal" | "team".
    pub scope: String,
    /// Team id for team-scope recall. Omitted for personal recall.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub team_id: Option<String>,
    /// Max number of verdicts to return.
    pub k: u32,
    /// Target file path so the cloud can:
    ///
    /// 1. Run the file-pattern cascade (rules whose patterns match
    ///    this path surface first).
    /// 2. Attribute zero-result calls to a concrete file in the
    ///    `mcp.gap` telemetry stream (otherwise gap detection
    ///    drops the call as "unattributable").
    ///
    /// Optional: callers without file context still recall, just
    /// without cascade ordering or gap attribution.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_file: Option<String>,
}

/// A single past-verdict row as returned by the cloud endpoint. Mirrors
/// `context::types::PastVerdict`; the two are intentionally kept separate
/// so the wire type can evolve independently of the in-memory type.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PastVerdictDto {
    pub extraction_id: String,
    pub code_snippet: String,
    pub issue_text: String,
    pub status: String,
    #[serde(default)]
    pub reason: Option<String>,
    pub similarity: f32,
    pub created_at: String,
    /// Canonical fix signature when the cloud carries it. `None` for
    /// older rows that pre-date signature storage.
    #[serde(default)]
    pub signature: Option<String>,
    /// Exact source PR for this recalled verdict when the cloud can trace
    /// it. Optional for backward compatibility with older cloud builds.
    #[serde(default)]
    pub source_pr_number: Option<i64>,
    #[serde(default)]
    pub source_pr_title: Option<String>,
    #[serde(default)]
    pub source_pr_url: Option<String>,
}

// ── Review metrics upload ──
//
// POSTed by the Rust review engine after `run_review_multi` / `run_review`
// completes so the cloud can render the "this review cost you $X, ran
// against M perspectives, recalled K past verdicts" footer without any
// guesswork. The cloud-side endpoint is `POST /reviews/{id}/metrics`,
// handled by `recordReviewMetrics` in `difflore-cloud/src/orpc/reviews.ts`.
//
// All fields except `review_id` are optional: pass `None` for any metric
// the engine doesn't have, and the server will leave the corresponding
// column unchanged. This lets the CLI patch individual fields as data
// arrives (e.g. token counts after the LLM responds, duration at the end).

/// Request body for `POST /reviews/{id}/metrics`. Note that `id` lives in
/// the URL, not the body, so this struct is just the payload fields.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordReviewMetricsRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u32>,
    /// Estimated cost in USD. Computed locally via `cost::estimate_cost_usd`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub perspective_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub past_verdicts_used: Option<u32>,
}

// ── Save trajectory ──
//
// POSTed after `TrajectoryBuilder` finishes collecting steps for a review
// run. The server endpoint `saveTrajectory` on
// `difflore-cloud/src/orpc/reviews.ts` validates the step payload with
// a Zod discriminated union whose field names match this shape exactly.

/// Request body for `POST /reviews/{prReviewId}/trajectory`. `prReviewId`
/// lives in the URL; the body only carries the steps array.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveTrajectoryRequest {
    /// Serialized trajectory steps. The `TrajectoryBuilder::into_json()`
    /// return value is byte-compatible with the server's Zod schema, so
    /// we hand it off as an opaque `serde_json::Value`.
    pub steps: serde_json::Value,
}

/// Response body for `GET /reviews/{prReviewId}/trajectory` — the
/// `getTrajectory` oRPC endpoint in
/// `difflore-cloud/src/orpc/reviews/trajectory.ts`.
///
/// The outer envelope keys are camelCase (oRPC/Drizzle convention), so the
/// struct carries `#[serde(rename_all = "camelCase")]`. The `steps` array
/// reuses the canonical [`crate::review_trajectory::TrajectoryStep`] enum:
/// every step object is tagged `kind` with snake_case fields, exactly the
/// shape the cloud's Zod discriminated union validates and re-emits, so the
/// nested deserialize round-trips without further coercion.
///
/// When a review has no persisted trajectory the cloud returns a
/// zero-UUID placeholder with an empty `steps` array (see the handler's
/// `if (!row)` branch) rather than a 404; the CLI renderer detects the
/// empty array and prints a graceful "no trajectory recorded" message.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetTrajectoryResponse {
    pub id: String,
    pub pr_review_id: String,
    pub team_id: Option<String>,
    pub steps: Vec<crate::review_trajectory::TrajectoryStep>,
    pub created_at: String,
}

// ── Record accepted edit ──
//
// POSTed when the user accepts an edit locally (IDE / CLI). Feeds the
// rule-candidate pipeline the same way a GitHub PR approval does, so
// locally accepted edits contribute to Rule candidate promotion.
// Server: `POST /accepted-edits` → `acceptedEdits.record` in
// `difflore-cloud/src/orpc/accepted-edits.ts`.

pub fn accepted_edit_diff_signature(before: &str, after: &str) -> String {
    use sha2::{Digest as _, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(before.as_bytes());
    hasher.update(b"\n---\n");
    hasher.update(after.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use core::fmt::Write as _;
        write!(&mut out, "{byte:02x}").ok();
    }
    out
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordAcceptedEditRequest {
    pub before_code: String,
    pub after_code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    /// GitHub repository that produced this local acceptance, when the
    /// client can detect it from the current git remote.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_full_name: Option<String>,
    /// Pull request being fixed, when the command was run through
    /// `difflore fix --pr`. Kept separate from imported source PRs so
    /// cloud audits can reject self-source evidence.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_pr_number: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Accepted-edit provenance used by cloud audits.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acceptance_source: Option<String>,
    /// Client that produced the acceptance event, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
    /// Optional canonical diff signature — the server computes its own
    /// sha256 fallback when absent. Kept on the wire for forward-compat
    /// with cloud-side signature clustering.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff_signature: Option<String>,
    /// Exact rules that the local fixer applied for this accepted edit.
    /// Empty when the edit was not tied to a recalled rule.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rule_ids: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordAcceptedEditResponse {
    pub ok: bool,
    pub acceptance_recorded: bool,
    pub acceptance_id: Option<String>,
    pub diff_signature: Option<String>,
    pub team_id: Option<String>,
    pub attributed_rule_ids: Vec<String>,
    pub observations_inserted: u32,
    pub memory_reinforcement_recorded: bool,
    pub memory_reinforcement_deduped: bool,
    pub error: Option<String>,
}

// ── GitHub PR history import upload ──

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadImportedReviewsRequest {
    pub reviews: Vec<ImportedReviewUpload>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportedReviewUpload {
    /// Repository the imported memory should attach to. For fork workflows this
    /// is the user's fork, even when review history was read from upstream.
    pub repo_full_name: String,
    /// Repository the review history was read from. Omitted when it matches
    /// `repo_full_name`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_repo_full_name: Option<String>,
    pub pr_number: i32,
    pub pr_title: Option<String>,
    pub comments: Vec<ImportedCommentUpload>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportedCommentUpload {
    pub file_path: Option<String>,
    pub line_number: i32,
    pub content: String,
    pub author: Option<String>,
    pub comment_url: String,
    pub thread_id: Option<String>,
    pub occurred_at: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::{
        GetTrajectoryResponse, ImpactCoverageDto, ImpactFixScorecardDto, ImpactTopRulesDto,
        RecordAcceptedEditRequest, RecordAcceptedEditResponse, accepted_edit_diff_signature,
    };
    use crate::review_trajectory::TrajectoryStep;

    /// The `getTrajectory` GET envelope is camelCase, but the `steps`
    /// array uses the canonical `kind`-tagged snake_case step shape. Lock
    /// that the Rust DTO decodes a realistic cloud payload into the shared
    /// `TrajectoryStep` enum without coercion — this is exactly the bytes
    /// `difflore trajectory <review-id>` consumes.
    #[test]
    fn get_trajectory_response_decodes_cloud_envelope_and_steps() {
        let payload = r#"{
          "id": "11111111-1111-1111-1111-111111111111",
          "prReviewId": "22222222-2222-2222-2222-222222222222",
          "teamId": "33333333-3333-3333-3333-333333333333",
          "createdAt": "2026-05-29T12:00:00.000Z",
          "steps": [
            { "kind": "chunks_retrieved", "count": 2, "symbols": ["foo"], "similarity_scores": [0.91] },
            { "kind": "rules_applied", "rule_ids": ["r1", "r2"], "source": "team" },
            { "kind": "past_verdicts_recalled", "count": 1, "top_similarities": [0.95],
              "recalled_items": [{ "id": "v1", "title": "no unwrap", "similarity": 0.95, "excerpt": "..." }] },
            { "kind": "self_check", "keep_count": 3, "drop_count": 1, "avg_confidence": 0.87 },
            { "kind": "final_decision", "issue_ids_emitted": ["issue-1"] }
          ]
        }"#;

        let doc: GetTrajectoryResponse = serde_json::from_str(payload).unwrap();
        assert_eq!(doc.pr_review_id, "22222222-2222-2222-2222-222222222222");
        assert_eq!(
            doc.team_id.as_deref(),
            Some("33333333-3333-3333-3333-333333333333")
        );
        assert_eq!(doc.steps.len(), 5);
        assert!(matches!(
            doc.steps[0],
            TrajectoryStep::ChunksRetrieved { count: 2, .. }
        ));
        assert!(matches!(
            &doc.steps[1],
            TrajectoryStep::RulesApplied { rule_ids, .. } if rule_ids.len() == 2
        ));
        assert!(matches!(
            doc.steps[4],
            TrajectoryStep::FinalDecision { ref issue_ids_emitted } if issue_ids_emitted.len() == 1
        ));
    }

    /// The cloud returns a zero-UUID placeholder with an empty `steps`
    /// array (and a nullable `teamId`) when a review has no recorded
    /// trajectory; the DTO must accept that "nothing recorded" shape so
    /// the renderer can show its graceful message.
    #[test]
    fn get_trajectory_response_accepts_empty_placeholder() {
        let payload = r#"{
          "id": "00000000-0000-0000-0000-000000000000",
          "prReviewId": "22222222-2222-2222-2222-222222222222",
          "teamId": null,
          "createdAt": "2026-05-29T12:00:00.000Z",
          "steps": []
        }"#;

        let doc: GetTrajectoryResponse = serde_json::from_str(payload).unwrap();
        assert!(doc.steps.is_empty());
        assert!(doc.team_id.is_none());
    }

    #[test]
    fn accepted_edit_defaults_missing_rule_ids_for_legacy_outbox_rows() {
        let payload = r#"{
          "beforeCode": "old",
          "afterCode": "new",
          "filePath": "src/lib.rs"
        }"#;

        let req: RecordAcceptedEditRequest = serde_json::from_str(payload).unwrap();
        assert!(req.rule_ids.is_empty());
    }

    #[test]
    fn accepted_edit_serializes_rule_ids_when_present() {
        let req = RecordAcceptedEditRequest {
            before_code: "old".into(),
            after_code: "new".into(),
            file_path: Some("src/lib.rs".into()),
            repo_full_name: Some("hibrandonevans/gin".into()),
            target_pr_number: Some(4543),
            language: Some("rust".into()),
            acceptance_source: Some("difflore_fix".into()),
            client: Some("difflore_cli".into()),
            diff_signature: Some(accepted_edit_diff_signature("old", "new")),
            rule_ids: vec!["rule-1".into(), "rule-2".into()],
        };

        let value = serde_json::to_value(req).unwrap();
        assert_eq!(value["acceptanceSource"], "difflore_fix");
        assert_eq!(value["client"], "difflore_cli");
        assert_eq!(value["targetPrNumber"], 4543);
        assert_eq!(value["ruleIds"][0], "rule-1");
        assert_eq!(value["ruleIds"][1], "rule-2");
        assert_eq!(value["diffSignature"].as_str().unwrap().len(), 64);
    }

    #[test]
    fn accepted_edit_diff_signature_is_stable_without_raw_code() {
        let a = accepted_edit_diff_signature("let a = 1;\n", "let a = 2;\n");
        let b = accepted_edit_diff_signature("let a = 1;\n", "let a = 2;\n");
        let c = accepted_edit_diff_signature("let a = 1;\n", "let a = 3;\n");

        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|ch| ch.is_ascii_hexdigit()));
    }

    #[test]
    fn openapi_contract_only_exposes_local_fix_acceptance_proof() {
        let spec = include_str!("../../openapi-spec.json");

        assert!(spec.contains("\"/accepted-edits\""));
        assert!(spec.contains("\"operationId\": \"acceptedEdits.record\""));
        assert!(spec.contains("\"repoFullName\""));
        assert!(spec.contains("\"acceptanceSource\""));
        assert!(spec.contains("\"client\""));
        assert!(spec.contains("\"ruleIds\""));
        assert!(spec.contains("\"acceptanceRecorded\""));
        assert!(spec.contains("\"observationsInserted\""));
        assert!(spec.contains("\"attributedRuleIds\""));

        for forbidden in [
            "\"/fix-runs/acceptances\"",
            "\"/fix-runs\"",
            "\"/fix-runs/{id}\"",
            "\"/fix-runs/{id}/cancel\"",
            "\"/fix-runs/trigger\"",
            "\"fixRunId\"",
            "\"FIX_RUN_NOT_FOUND\"",
            "\"operationId\": \"fixRuns.recordFixAcceptance\"",
            "\"operationId\": \"fixRuns.list\"",
            "\"operationId\": \"fixRuns.get\"",
            "\"operationId\": \"fixRuns.cancel\"",
            "\"operationId\": \"fixRuns.manualTrigger\"",
            "\"FixRunItem\"",
            "\"FixRunDetail\"",
            "\"FixRunList\"",
            "\"FixTriggerResult\"",
            "\"/fix-configs\"",
            "\"/fix-configs/{repoFullName}\"",
            "\"operationId\": \"fixConfigs.list\"",
            "\"operationId\": \"fixConfigs.get\"",
            "\"operationId\": \"fixConfigs.upsert\"",
            "\"FixConfigSummary\"",
            "\"FixConfigDetail\"",
            "\"FixUpsertResult\"",
            "\"monthlyFixQuota\"",
            "\"fixQuota\"",
            "\"fixRunsQuota\"",
            "\"fixRunsUsed\"",
        ] {
            assert!(
                !spec.contains(forbidden),
                "OpenAPI contract reintroduced obsolete managed fix-run surface `{forbidden}`"
            );
        }
    }

    #[test]
    fn accepted_edit_response_deserializes_attribution_details() {
        let payload = r#"{
          "ok": true,
          "acceptanceRecorded": true,
          "acceptanceId": "acc-1",
          "diffSignature": "sig-1",
          "teamId": "team-1",
          "attributedRuleIds": ["rule-1"],
          "observationsInserted": 1,
          "memoryReinforcementRecorded": true,
          "memoryReinforcementDeduped": false,
          "error": null
        }"#;

        let response: RecordAcceptedEditResponse = serde_json::from_str(payload).unwrap();
        assert!(response.ok);
        assert!(response.acceptance_recorded);
        assert_eq!(response.attributed_rule_ids, vec!["rule-1"]);
        assert_eq!(response.observations_inserted, 1);
    }

    #[test]
    fn impact_top_rules_accepts_missing_or_present_proof_source() {
        let legacy_payload = r#"{
          "rules": [{
            "id": "rule-1",
            "name": "Prefer structured parsing",
            "severity": "medium",
            "language": "rust",
            "acceptanceCount": 2,
            "distinctUsers": 1,
            "citedCount": 4,
            "trustRate": 0.5
          }],
          "promotionProgress": []
        }"#;
        let legacy: ImpactTopRulesDto = serde_json::from_str(legacy_payload).unwrap();
        assert_eq!(legacy.rules[0].accepted_proof_source, None);
        assert_eq!(legacy.rules[0].reviewer_proof_ready_count, 0);
        assert_eq!(legacy.rules[0].reviewer_context_serves, 0);
        assert_eq!(legacy.rules[0].reviewer_mentions, 0);
        assert_eq!(legacy.rules[0].source_repo, None);

        let current_payload = r#"{
          "rules": [{
            "id": "rule-1",
            "name": "Prefer structured parsing",
            "acceptanceCount": 2,
            "distinctUsers": 1,
            "acceptedProofSource": "local_fix",
            "reviewerProofReadyCount": 2,
            "reviewerContextServes": 5,
            "reviewerMentions": 2,
            "sourceRepo": "gin-gonic/gin"
          }],
          "promotionProgress": []
        }"#;
        let current: ImpactTopRulesDto = serde_json::from_str(current_payload).unwrap();
        assert_eq!(
            current.rules[0].accepted_proof_source.as_deref(),
            Some("local_fix")
        );
        assert_eq!(current.rules[0].reviewer_proof_ready_count, 2);
        assert_eq!(current.rules[0].reviewer_context_serves, 5);
        assert_eq!(current.rules[0].reviewer_mentions, 2);
        assert_eq!(
            current.rules[0].source_repo.as_deref(),
            Some("gin-gonic/gin")
        );
    }

    #[test]
    fn impact_coverage_defaults_missing_review_comment_count() {
        let payload = r#"{
          "repos": 3,
          "prs": 12,
          "files": 40
        }"#;

        let coverage: ImpactCoverageDto = serde_json::from_str(payload).unwrap();
        assert_eq!(coverage.review_comments_indexed, 0);
    }

    #[test]
    fn impact_fix_scorecard_accepts_roi_when_present() {
        let payload = r#"{
          "last30": { "accepted": 3, "total": 4 },
          "prior30": { "accepted": 1, "total": 2 },
          "trendPct": 50,
          "roi": {
            "acceptedFixesLast30": 3,
            "reviewCommentsAvoided": 3,
            "savedReviewMinutes": 12,
            "repeatFeedbackReduced": 1,
            "sourceEvidenceItems": 4
          }
        }"#;

        let scorecard: ImpactFixScorecardDto = serde_json::from_str(payload).unwrap();
        let roi = scorecard.roi.unwrap();
        assert_eq!(roi.saved_review_minutes, 12);
    }
}

// ── Impact report (CLI `difflore cloud impact`) ──
//
// GET endpoints under /impact/* on the cloud. Hand-written mirrors of
// `src/orpc/schemas/impact.ts`. Kept separate from the generated OpenAPI
// types for the same reason as `PastVerdictDto` — the oRPC routes are not
// yet in the shared spec.

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImpactBannerDto {
    pub past_verdicts_this_week: i64,
    pub week_start_iso: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImpactWeeklyPointDto {
    pub week_start_iso: String,
    pub rules_sedimented: i64,
    pub past_verdicts_recalled: i64,
    pub fixes_accepted: i64,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImpactWeeklyDto {
    pub weeks: Vec<ImpactWeeklyPointDto>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImpactTopRuleDto {
    pub id: String,
    pub name: String,
    pub severity: Option<String>,
    pub language: Option<String>,
    pub acceptance_count: i64,
    pub distinct_users: i64,
    #[serde(default)]
    pub cited_count: i64,
    #[serde(default)]
    pub trust_rate: Option<f64>,
    #[serde(default)]
    pub accepted_proof_source: Option<String>,
    #[serde(default)]
    pub reviewer_proof_ready_count: i64,
    #[serde(default)]
    pub reviewer_context_serves: i64,
    #[serde(default)]
    pub reviewer_mentions: i64,
    #[serde(default)]
    pub source_repo: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImpactPromotionProgressDto {
    pub file_path: Option<String>,
    pub language: Option<String>,
    pub acceptance_count: i64,
    pub required_count: i64,
    pub distinct_users: i64,
    pub required_distinct_users: i64,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImpactTopRulesDto {
    pub rules: Vec<ImpactTopRuleDto>,
    #[serde(default)]
    pub promotion_progress: Vec<ImpactPromotionProgressDto>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImpactCoverageDto {
    pub repos: i64,
    pub prs: i64,
    pub files: i64,
    #[serde(default)]
    pub review_comments_indexed: i64,
    #[serde(default)]
    pub ai_reviewer_comments_indexed: i64,
    #[serde(default)]
    pub human_review_comments_indexed: i64,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImpactFixWindowDto {
    pub accepted: i64,
    pub total: i64,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImpactRoiDto {
    #[serde(default)]
    pub accepted_fixes_last30: i64,
    #[serde(default)]
    pub review_comments_avoided: i64,
    #[serde(default)]
    pub saved_review_minutes: i64,
    #[serde(default)]
    pub repeat_feedback_reduced: i64,
    #[serde(default)]
    pub source_evidence_items: i64,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImpactFixScorecardDto {
    pub last30: ImpactFixWindowDto,
    pub prior30: ImpactFixWindowDto,
    pub trend_pct: Option<f64>,
    #[serde(default)]
    pub roi: Option<ImpactRoiDto>,
}

// ── PostToolUse observations (third supply line for candidate rules) ──
//
// Whenever the Claude Code PostToolUse hook captures a file mutation
// (Edit / MultiEdit / Write), the CLI classifies it into one of six
// observation types (bugfix / feature / refactor / change / discovery /
// decision) and enqueues an `Observation` payload via `OutboxQueue`
// with `kind="observation"`. The cloud-side consumer (task #8b) drains
// `cloud_outbox WHERE kind='observation'`, clusters by `content_hash`,
// and feeds the rule-promoter pipeline alongside `remember_rule` and
// GitHub-App PR-merge signatures.
//
// Wire format: the payload JSON is exactly the `Observation` struct
// below serialised with `serde_json::to_string` (snake_case keys, no
// envelope). Example row in `cloud_outbox.payload_json`:
//
// ```json
// {
//   "session_id": "sess_abc",
//   "ts_ms": 1_714_000_000_000,
//   "obs_type": "bugfix",
//   "tool": "Edit",
//   "file_path": "src/foo.rs",
//   "scope": {
//     "anchor_kind": "file",
//     "anchor_key": "src/foo.rs",
//     "parent_path": "src",
//     "display_name": "foo.rs"
//   },
//   "title": "Edit src/foo.rs: remove FIXME",
//   "narrative": "- // FIXME: crash on None\n+ guard against None",
//   "diff_excerpt": "-// FIXME: crash on None\n+if let Some(x) = v {",
//   "content_hash": "3f1c0a2b4d5e6f70"
// }
// ```
//

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ObservationScope {
    /// Stable scope family for clustering. `file` is the only local
    /// emitter today, but this stays open for future symbol/module
    /// hints from richer local indexing.
    pub anchor_kind: String,
    /// Stable cluster key inside `anchor_kind`. The local classifier
    /// uses the full relative file path for now so cloud-side
    /// clustering can distinguish files that share the same shallow
    /// prefix.
    pub anchor_key: String,
    /// Optional parent directory for display / fallback grouping.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_path: Option<String>,
    /// Human-readable leaf label (typically the filename).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

// All string fields are pre-truncated on the CLI side (title ≤ 120,
// narrative ≤ 500, diff_excerpt ≤ ~1024 bytes) so the cloud side can
// insert without additional validation. `content_hash` is the first
// 16 hex chars of `sha256(session_id|file_path|title|narrative)` —
// the cloud uses it to de-dupe when the same observation gets
// enqueued twice across an outbox retry.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct Observation {
    /// Claude Code session id from the hook stdin payload. Empty
    /// string when the adapter couldn't extract one — cloud-side
    /// clustering treats `""` as "unknown session".
    pub session_id: String,
    /// Unix-ms at the moment the hook fired.
    pub ts_ms: i64,
    /// One of: `"bugfix" | "feature" | "refactor" | "change" |
    /// "discovery" | "decision"`. The CLI classifier only emits the
    /// first four; `discovery` and `decision` are reserved for
    /// future LLM-assisted classification.
    pub obs_type: String,
    /// Source tool: `"Edit" | "MultiEdit" | "Write"`.
    pub tool: String,
    /// Target file path. `None` for edits where the adapter couldn't
    /// identify a file (rare — `classify()` requires a `file_path` so
    /// this is almost always `Some`).
    pub file_path: Option<String>,
    /// Optional structured scope metadata. Newer clients send this so
    /// the cloud can cluster more precisely than a shallow path
    /// prefix; older clients omit it entirely.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<ObservationScope>,
    /// Single-line summary, ≤ 120 chars.
    pub title: String,
    /// Short narrative, ≤ 500 chars. `None` when the classifier had
    /// nothing to add beyond the title.
    pub narrative: Option<String>,
    /// First ~1 KB of the synthesised diff. Large diffs are truncated
    /// with a trailing `…[truncated]` marker so the cloud doesn't
    /// misinterpret them as full diffs.
    pub diff_excerpt: Option<String>,
    /// 16-char hex — `sha256(session_id|file|title|narrative)[:16]`.
    /// Stable across identical observations for cloud-side dedup.
    pub content_hash: String,
}

// ── Knowledge-Agent Corpus (cloud spec §3.16) ────────────────────────
//
// Wraps `POST /knowledge/corpus`, `POST /knowledge/corpus/{id}/prime`,
// `POST /knowledge/corpus/{id}/query`, and `GET /knowledge/corpora`.
// Wire format mirrors the cloud Zod schema 1:1; field names are
// camelCase because the cloud is oRPC + Drizzle which serialises that
// way.

#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildCorpusFilters {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_patterns: Option<Vec<String>>,
    /// ISO-8601 date string (e.g. "2026-01-01"). Cloud parses with `new Date(...)`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildCorpusRequest {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub filters: BuildCorpusFilters,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildCorpusResult {
    pub id: String,
    pub item_count: u32,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrimeCorpusResult {
    pub corpus_id: String,
    pub session_token: String,
    pub primed_at_iso: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct QueryCorpusRequest {
    pub question: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryCitation {
    pub corpus_item_id: String,
    pub item_kind: String,
    pub title: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct QueryCorpusResult {
    pub answer: String,
    pub citations: Vec<QueryCitation>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CorpusSummary {
    pub id: String,
    pub name: String,
    pub item_count: u32,
    pub created_at_iso: String,
    pub primed_at_iso: Option<String>,
    pub last_queried_at_iso: Option<String>,
}
