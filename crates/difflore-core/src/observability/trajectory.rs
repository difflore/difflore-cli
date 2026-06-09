//! Review trajectory builder.
//!
//! A trajectory is the ordered decision trail for a single review run:
//! what chunks were retrieved, what rules were applied, which LLM calls
//! fired, what past verdicts were recalled, what the self-check kept vs
//! dropped, and which issues were finally emitted.
//!
//! The builder is deliberately additive and optional: the review pipeline
//! threads an `Option<&mut TrajectoryBuilder>` through its hot path, so callers
//! that do not need trajectory data pass `None`.
//!
//! The JSON shape produced by `into_json()` is byte-compatible with the
//! TypeScript discriminated union in
//! `difflore-cloud/src/types/trajectory.ts`. When that shape changes,
//! BOTH sides must be updated in lockstep — the `saveTrajectory` oRPC
//! endpoint validates the payload with the matching Zod schema on
//! ingress, so any drift fails the round-trip test.

use serde::{Deserialize, Serialize};

/// Where a `rules_applied` step's rules came from. Matches the TS
/// `TrajectoryRuleSource` literal set exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleSource {
    Local,
    Team,
    Global,
}

/// One past verdict recalled from the review-memory store, surfaced on
/// the cloud detail page so reviewers can see **which** prior decisions
/// influenced the current run. Shape: `{ id, title, similarity, excerpt }`.
/// The `excerpt` field is
/// truncated by callers to ~200 characters (with a trailing `…`) so the
/// trajectory payload stays compact.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RecalledVerdict {
    pub id: String,
    pub title: String,
    pub similarity: f32,
    pub excerpt: String,
}

/// Ordered discriminated step. Serialized with `tag = "kind"` so the JSON
/// shape matches the TS union; every new variant must add a matching
/// zod arm in `difflore-cloud/src/types/trajectory.ts`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TrajectoryStep {
    /// Retrieval pass produced N chunks for the prompt context.
    ChunksRetrieved {
        count: usize,
        symbols: Vec<String>,
        similarity_scores: Vec<f32>,
    },
    /// Rule resolution picked the given rule IDs from `source`.
    RulesApplied {
        rule_ids: Vec<String>,
        source: RuleSource,
    },
    /// One LLM invocation — perspective + token usage. `raw_output` is
    /// optional so callers can choose to omit it for cost/privacy.
    LlmCall {
        perspective: String,
        input_tokens: u32,
        output_tokens: u32,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        raw_output: Option<String>,
    },
    /// Review Memory recall fetched `count` past verdicts with the given
    /// top-k similarity scores. `recalled_items` carries the per-verdict
    /// payload the cloud detail page renders (id/title/similarity/excerpt);
    /// it is `#[serde(default)]` so older trajectories that only carry
    /// `count` + `top_similarities` still round-trip cleanly.
    PastVerdictsRecalled {
        count: usize,
        top_similarities: Vec<f32>,
        #[serde(default)]
        recalled_items: Vec<RecalledVerdict>,
    },
    /// Self-check (`verify_pass`) kept N issues, dropped M, and produced
    /// an average confidence score across the kept set.
    SelfCheck {
        keep_count: u32,
        drop_count: u32,
        avg_confidence: f32,
    },
    /// Signature-based confidence adjustment applied after self-check.
    /// Records per-issue adjustments so the cloud detail page can show
    /// which past verdicts influenced confidence scoring.
    SignatureConfidenceAdjust {
        /// Number of issues that received a positive bump (accepted match).
        accepted_bumps: u32,
        /// Number of issues that received a negative bump (rejected match).
        rejected_bumps: u32,
    },
    /// Final decision: the issue IDs emitted to the user.
    FinalDecision { issue_ids_emitted: Vec<String> },
    /// MCP tool responded with `total_tokens` worth of payload, of which
    /// `rules_injected` rules were included. Lets the cloud dashboard chart
    /// MCP response sizes over time so we can spot token bloat early.
    /// Token count is a coarse estimate (`byte_len` / 4).
    McpResponseSize {
        tool: String,
        total_tokens: usize,
        rules_injected: usize,
    },
    /// Breakdown of which origins the hit rules came from. Aggregated
    /// across one MCP response so downstream analytics can answer
    /// "how much value are conversation captures vs extracted rules
    /// actually driving in recall".
    RuleHitByOrigin {
        manual: u32,
        conversation: u32,
        pr_review: u32,
        extracted: u32,
        cloud: u32,
    },
    /// How many candidate chunks the metadata pre-filter kept. `before` is the
    /// count pre-filter; `after` is the count the embedding / FTS path scored.
    RetrievalFilter { before: u32, after: u32 },
    /// RRF fusion of the FTS and embedding candidate sets. `fts_hits` /
    /// `emb_hits` record raw pre-fusion sizes; `overlap` records how many chunk
    /// ids appeared in both.
    HybridFusion {
        fts_hits: u32,
        emb_hits: u32,
        overlap: u32,
    },
    /// HNSW ANN recall stats for a single retrieval call.
    /// `used = true` means the ANN path produced candidates that fed
    /// the RRF fusion; `used = false` means we fell back to the linear
    /// cosine scan (empty index, dim mismatch, or any internal error).
    /// `index_size` is the live (non-tombstoned) chunk count known to
    /// the ANN graph at call time; `candidates` is how many top-k
    /// results came back from `ann.search` before RRF de-duped and
    /// re-ranked them.
    AnnRecall {
        used: bool,
        index_size: u32,
        candidates: u32,
    },
}

/// Ordered collector for `TrajectoryStep`s. Threaded through the review
/// pipeline as `Option<&mut TrajectoryBuilder>` so absence is a no-op.
///
/// Construction is `Default::default()`; callers push steps in the order
/// they happen and finish with `into_json()` to hand the serialized
/// payload off to the cloud `saveTrajectory` endpoint.
#[derive(Debug, Clone, Default)]
pub struct TrajectoryBuilder {
    steps: Vec<TrajectoryStep>,
}

impl TrajectoryBuilder {
    /// Start a fresh builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a single step. Preserves insertion order; callers control
    /// the ordering so the resulting trajectory reads as a timeline.
    pub fn push(&mut self, step: TrajectoryStep) {
        self.steps.push(step);
    }

    /// Number of steps collected so far. Useful for tests + the final
    /// decision step which wants to know "did anything at all happen".
    pub const fn len(&self) -> usize {
        self.steps.len()
    }

    /// Convenience: true when no steps have been pushed.
    pub const fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// Borrow the steps collected so far — used by tests so they can
    /// introspect without consuming the builder.
    pub fn steps(&self) -> &[TrajectoryStep] {
        &self.steps
    }

    /// Consume the builder and serialize to `serde_json::Value`. The
    /// returned value is an array (`Value::Array`) of step objects, one
    /// per `push`. Matches the TS side's `TrajectoryStep[]` exactly.
    pub fn into_json(self) -> serde_json::Value {
        serde_json::to_value(self.steps).unwrap_or(serde_json::Value::Array(vec![]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialize_matches_ts_shape_exactly() {
        // The TS union uses `kind: "chunks_retrieved"` with snake_case
        // field names. Lock the exact bytes so field drift is caught here.
        let mut b = TrajectoryBuilder::new();
        b.push(TrajectoryStep::ChunksRetrieved {
            count: 2,
            symbols: vec!["foo".into()],
            similarity_scores: vec![0.91],
        });
        b.push(TrajectoryStep::SelfCheck {
            keep_count: 3,
            drop_count: 1,
            avg_confidence: 0.82,
        });
        let value = b.into_json();
        let arr = value.as_array().expect("top-level must be array");
        assert_eq!(arr.len(), 2);

        let first = arr[0].as_object().unwrap();
        assert_eq!(
            first.get("kind").and_then(|v| v.as_str()),
            Some("chunks_retrieved")
        );
        assert_eq!(
            first.get("count").and_then(serde_json::Value::as_u64),
            Some(2)
        );
        assert!(first.contains_key("symbols"));
        assert!(first.contains_key("similarity_scores"));

        let second = arr[1].as_object().unwrap();
        assert_eq!(
            second.get("kind").and_then(|v| v.as_str()),
            Some("self_check")
        );
        assert_eq!(
            second.get("keep_count").and_then(serde_json::Value::as_u64),
            Some(3)
        );
        assert_eq!(
            second.get("drop_count").and_then(serde_json::Value::as_u64),
            Some(1)
        );
        assert!(
            (second
                .get("avg_confidence")
                .and_then(serde_json::Value::as_f64)
                .unwrap()
                - 0.82)
                .abs()
                < 1e-6
        );
    }

    #[test]
    fn llm_call_omits_raw_output_when_absent() {
        // `raw_output` is `#[serde(skip_serializing_if = "Option::is_none")]`
        // so absent output should not emit the key at all. This keeps the
        // on-the-wire shape minimal for cost-sensitive deployments.
        let mut b = TrajectoryBuilder::new();
        b.push(TrajectoryStep::LlmCall {
            perspective: "safety".into(),
            input_tokens: 123,
            output_tokens: 45,
            raw_output: None,
        });
        let value = b.into_json();
        let obj = value.as_array().unwrap()[0].as_object().unwrap();
        assert_eq!(
            obj.get("perspective").and_then(|v| v.as_str()),
            Some("safety")
        );
        assert!(!obj.contains_key("raw_output"));
    }

    #[test]
    fn full_pipeline_shape_matches_plan_capture_points() {
        // Simulates the sequence emitted on a successful run: chunks, rules,
        // past verdicts, one LLM call per perspective, self_check, then
        // final_decision. Ordering is part of the wire contract.
        let mut b = TrajectoryBuilder::new();
        b.push(TrajectoryStep::ChunksRetrieved {
            count: 4,
            symbols: vec!["foo".into()],
            similarity_scores: vec![],
        });
        b.push(TrajectoryStep::RulesApplied {
            rule_ids: vec!["r1".into(), "r2".into()],
            source: RuleSource::Team,
        });
        b.push(TrajectoryStep::PastVerdictsRecalled {
            count: 2,
            top_similarities: vec![],
            recalled_items: vec![],
        });
        for p in ["safety", "performance", "style", "docs", "api_design"] {
            b.push(TrajectoryStep::LlmCall {
                perspective: p.to_owned(),
                input_tokens: 200,
                output_tokens: 0,
                raw_output: None,
            });
        }
        b.push(TrajectoryStep::SelfCheck {
            keep_count: 3,
            drop_count: 1,
            avg_confidence: 0.87,
        });
        b.push(TrajectoryStep::FinalDecision {
            issue_ids_emitted: vec!["issue-1".into(), "issue-2".into(), "issue-3".into()],
        });

        assert_eq!(b.len(), 1 + 1 + 1 + 5 + 1 + 1);

        // Kind order must be exactly this. Any reorder is a breaking
        // change to the wire format and should fail here first.
        let kinds: Vec<&str> = b
            .steps()
            .iter()
            .map(|s| match s {
                TrajectoryStep::ChunksRetrieved { .. } => "chunks_retrieved",
                TrajectoryStep::RulesApplied { .. } => "rules_applied",
                TrajectoryStep::PastVerdictsRecalled { .. } => "past_verdicts_recalled",
                TrajectoryStep::LlmCall { .. } => "llm_call",
                TrajectoryStep::SelfCheck { .. } => "self_check",
                TrajectoryStep::SignatureConfidenceAdjust { .. } => "signature_confidence_adjust",
                TrajectoryStep::FinalDecision { .. } => "final_decision",
                TrajectoryStep::McpResponseSize { .. } => "mcp_response_size",
                TrajectoryStep::RuleHitByOrigin { .. } => "rule_hit_by_origin",
                TrajectoryStep::RetrievalFilter { .. } => "retrieval_filter",
                TrajectoryStep::HybridFusion { .. } => "hybrid_fusion",
                TrajectoryStep::AnnRecall { .. } => "ann_recall",
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                "chunks_retrieved",
                "rules_applied",
                "past_verdicts_recalled",
                "llm_call",
                "llm_call",
                "llm_call",
                "llm_call",
                "llm_call",
                "self_check",
                "final_decision",
            ]
        );
    }

    #[test]
    fn mcp_response_size_and_rule_hit_by_origin_serialize() {
        // Lock the on-the-wire shape so telemetry consumers can rely on stable
        // field names.
        let mut b = TrajectoryBuilder::new();
        b.push(TrajectoryStep::McpResponseSize {
            tool: "search_rules".into(),
            total_tokens: 1234,
            rules_injected: 3,
        });
        b.push(TrajectoryStep::RuleHitByOrigin {
            manual: 1,
            conversation: 2,
            pr_review: 0,
            extracted: 1,
            cloud: 0,
        });

        let value = b.clone().into_json();
        let arr = value.as_array().expect("top-level array");
        assert_eq!(arr[0]["kind"], "mcp_response_size");
        assert_eq!(arr[0]["tool"], "search_rules");
        assert_eq!(arr[0]["total_tokens"], 1234);
        assert_eq!(arr[0]["rules_injected"], 3);
        assert_eq!(arr[1]["kind"], "rule_hit_by_origin");
        assert_eq!(arr[1]["manual"], 1);
        assert_eq!(arr[1]["conversation"], 2);
        assert_eq!(arr[1]["pr_review"], 0);
        assert_eq!(arr[1]["extracted"], 1);
        assert_eq!(arr[1]["cloud"], 0);

        // Round-trip back to the enum so field drift fails here.
        let text = serde_json::to_string(&value).unwrap();
        let parsed: Vec<TrajectoryStep> = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, b.steps().to_vec());
    }

    #[test]
    fn round_trip_deserialize_via_serde_json() {
        // Ensures the on-the-wire bytes deserialize back into the same
        // enum variants — locks the `tag = "kind"` contract.
        let mut b = TrajectoryBuilder::new();
        b.push(TrajectoryStep::PastVerdictsRecalled {
            count: 4,
            top_similarities: vec![0.95, 0.88, 0.80, 0.72],
            recalled_items: vec![RecalledVerdict {
                id: "verdict-1".into(),
                title: "avoid unwrap in request handlers".into(),
                similarity: 0.95,
                excerpt: "fn handler() { ... .unwrap() ... }".into(),
            }],
        });
        b.push(TrajectoryStep::RulesApplied {
            rule_ids: vec!["r1".into(), "r2".into()],
            source: RuleSource::Global,
        });
        b.push(TrajectoryStep::FinalDecision {
            issue_ids_emitted: vec!["issue-1".into()],
        });

        let value = b.clone().into_json();
        let text = serde_json::to_string(&value).unwrap();
        let parsed: Vec<TrajectoryStep> = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, b.steps().to_vec());
    }
}
