//! `difflore trajectory <review-id>` — local replay of a recorded review
//! trajectory.
//!
//! Fetches one review's trajectory via [`CloudClient::get_trajectory`] (the
//! cloud's `getTrajectory` oRPC GET, mirroring the Rust enum in
//! `difflore_core::review_trajectory`) and renders it as a readable step
//! ladder so every emitted issue traces back to its memory evidence: chunks
//! retrieved, rules applied (with `← learned from <repo>` provenance from the
//! local `skills` table), past verdicts recalled, each `llm_call`, the
//! self-check, and the final issue ids.
//!
//! The renderer ([`render_trajectory`]) is a pure function with no I/O and no
//! color so it is unit-tested against fixtures. `--json` bypasses it and emits
//! the raw trajectory document.

use std::collections::HashMap;

use difflore_core::review_trajectory::{RuleSource, TrajectoryStep};

use crate::runtime::CommandContext;
use crate::style;

/// Arguments for `difflore trajectory`.
pub(crate) struct TrajectoryArgs {
    /// The PR review id (UUID) whose trajectory to replay.
    pub review_id: String,
    /// Emit the raw trajectory document as JSON instead of the ladder.
    pub json: bool,
}

/// Top-level handler: fetch the trajectory for `review_id` from the cloud,
/// resolve local rule provenance, and render the step ladder (or JSON).
///
/// Fetch failures are surfaced with an actionable message rather than being
/// swallowed — unlike best-effort recall, a `trajectory` invocation is an
/// explicit user request to *see* the trail, so "not logged in" /
/// "plan-gated" / "not found" each get their own remediation line.
pub(crate) async fn handle_trajectory(ctx: &CommandContext, args: TrajectoryArgs) {
    let review_id = args.review_id.trim();
    if review_id.is_empty() {
        if args.json {
            println!(
                "{}",
                crate::commands::util::json_compact_or(
                    &serde_json::json!({ "error": "missing_review_id" }),
                    "{}"
                )
            );
        } else {
            eprintln!(
                "{} A review id is required: {}",
                style::err(style::sym::ERR),
                style::cmd("difflore trajectory <review-id>")
            );
        }
        return;
    }

    let client = ctx.cloud().await;
    let fetched = client.get_trajectory(review_id).await;

    let doc = match fetched {
        Ok(doc) => doc,
        Err(e) => {
            render_fetch_error(review_id, &e, args.json);
            return;
        }
    };

    // `--json` is the machine path: emit the raw document untouched so
    // downstream tooling sees the canonical step shapes (snake_case,
    // tagged by `kind`) exactly as the cloud stored them.
    if args.json {
        let payload = serde_json::json!({
            "reviewId": doc.pr_review_id,
            "trajectoryId": doc.id,
            "teamId": doc.team_id,
            "createdAt": doc.created_at,
            "stepCount": doc.steps.len(),
            "steps": doc.steps,
        });
        println!("{}", crate::commands::util::json_or(&payload, "{}"));
        return;
    }

    // Resolve `rule_id → source_repo` once from the local skills table so
    // each `rules_applied` line can carry its `← learned from <repo>`
    // provenance. A failed lookup (fresh DB / no skills) degrades to an
    // empty map — the ladder still renders, just without the suffix.
    let provenance = difflore_core::skills::list_source_repos(&ctx.db)
        .await
        .unwrap_or_default();

    for line in render_trajectory(review_id, &doc, &provenance) {
        println!("{line}");
    }
}

/// Print the fetch-failure message. Splits the three failure shapes the
/// cloud client surfaces — `not_logged_in`, plan-gated (403
/// `plan_limit_exceeded`), and review-not-found (404) — into distinct,
/// actionable lines so the user knows what to do next.
fn render_fetch_error(review_id: &str, err: &str, json: bool) {
    if json {
        println!(
            "{}",
            crate::commands::util::json_compact_or(
                &serde_json::json!({
                    "reviewId": review_id,
                    "error": classify_fetch_error(err).as_str(),
                    "detail": err,
                }),
                "{}"
            )
        );
        return;
    }

    match classify_fetch_error(err) {
        FetchError::NotLoggedIn => {
            eprintln!(
                "{} Not logged in to DiffLore Cloud — trajectories live in the cloud.",
                style::pewter(style::sym::BULLET)
            );
            eprintln!("  next: {}", style::cmd("difflore cloud login"));
        }
        FetchError::PlanGated => {
            eprintln!(
                "{} Trajectory replay needs a plan with review-trajectory audit.",
                style::warn(style::sym::WARN)
            );
            eprintln!("  This review's trail exists; your plan can't read it yet.");
        }
        FetchError::NotFound => {
            eprintln!(
                "{} No review found for id {}.",
                style::warn(style::sym::WARN),
                style::ident(review_id)
            );
            eprintln!("  Double-check the id from your last review, or run a fresh review first.");
        }
        FetchError::Other => {
            eprintln!(
                "{} Could not fetch trajectory for {}.",
                style::err(style::sym::ERR),
                style::ident(review_id)
            );
            eprintln!("  {}", style::pewter(err));
        }
    }
}

/// Coarse classification of a [`CloudClient::get_trajectory`] error string
/// into the buckets the renderer reacts to. The client surfaces these as
/// plain strings (`"not_logged_in"`, `"[get_trajectory] returned 4xx: …"`),
/// so we pattern-match the well-known fragments. Anything unrecognised
/// falls through to [`FetchError::Other`] and the verbatim detail is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FetchError {
    NotLoggedIn,
    PlanGated,
    NotFound,
    Other,
}

impl FetchError {
    const fn as_str(self) -> &'static str {
        match self {
            Self::NotLoggedIn => "not_logged_in",
            Self::PlanGated => "plan_gated",
            Self::NotFound => "not_found",
            Self::Other => "fetch_failed",
        }
    }
}

fn classify_fetch_error(err: &str) -> FetchError {
    let lower = err.to_ascii_lowercase();
    if lower.contains("not_logged_in") {
        FetchError::NotLoggedIn
    } else if lower.contains("plan_limit_exceeded")
        || lower.contains("plangated")
        || lower.contains(" 403")
        || lower.contains("forbidden")
    {
        FetchError::PlanGated
    } else if lower.contains("reviewnotfound")
        || lower.contains("review_not_found")
        || lower.contains(" 404")
        || lower.contains("not found")
    {
        FetchError::NotFound
    } else {
        FetchError::Other
    }
}

/// Human label for a [`RuleSource`] used in the `rules applied` line.
const fn rule_source_label(source: RuleSource) -> &'static str {
    match source {
        RuleSource::Local => "local",
        RuleSource::Team => "team",
        RuleSource::Global => "global",
    }
}

/// Render at most `n` items of `items` joined by `", "`, appending
/// ` (+K more)` when the list was truncated. Used for symbol lists,
/// issue-id lists, etc. so a wide step never blows past one line.
fn join_capped(items: &[String], n: usize) -> String {
    if items.is_empty() {
        return String::new();
    }
    let shown: Vec<&str> = items.iter().take(n).map(String::as_str).collect();
    let mut out = shown.join(", ");
    if items.len() > n {
        out.push_str(&format!(" (+{} more)", items.len() - n));
    }
    out
}

/// Format a slice of similarity scores to two decimals, capped at `n`.
fn join_scores_capped(scores: &[f32], n: usize) -> String {
    let shown: Vec<String> = scores.iter().take(n).map(|s| format!("{s:.2}")).collect();
    let mut out = shown.join(", ");
    if scores.len() > n {
        out.push_str(&format!(" (+{} more)", scores.len() - n));
    }
    out
}

/// Pure renderer: turn a fetched trajectory document into an ordered list
/// of plain (uncolored) lines forming a readable step ladder.
///
/// `provenance` maps `rule_id → Option<source_repo>` (from the local
/// `skills` table) so `rules_applied` rows can show `← learned from
/// <repo>`; ids absent from the map (or mapped to `None`) simply omit the
/// suffix. The function performs no I/O and no coloring, which is what
/// makes it unit-testable against fixtures.
///
/// An empty `steps` array — the cloud's "review exists but nothing was
/// recorded" sentinel — yields a short, graceful "no trajectory recorded"
/// block instead of a bare header.
pub(crate) fn render_trajectory(
    review_id: &str,
    doc: &difflore_core::cloud::api_types::GetTrajectoryResponse,
    provenance: &HashMap<String, Option<String>>,
) -> Vec<String> {
    let mut out = Vec::new();
    out.push(format!("Review trajectory  {review_id}"));

    if doc.steps.is_empty() {
        out.push(format!("recorded {}  ·  0 steps", doc.created_at));
        out.push(String::new());
        out.push("No trajectory was recorded for this review.".to_owned());
        out.push(
            "Trajectories are captured when a review runs through the engine; \
re-run the review to populate one."
                .to_owned(),
        );
        return out;
    }

    out.push(format!(
        "recorded {}  ·  {} step{}",
        doc.created_at,
        doc.steps.len(),
        if doc.steps.len() == 1 { "" } else { "s" }
    ));
    out.push(String::new());

    for (idx, step) in doc.steps.iter().enumerate() {
        render_step(&mut out, idx + 1, step, provenance);
    }

    out.push(String::new());
    out.push("Every emitted issue traces to the rules + verdicts above.".to_owned());
    out
}

/// Render one step into `out`. Each step is one indexed headline line plus
/// any number of indented detail lines (rule ids with provenance, recalled
/// verdict titles, etc.).
fn render_step(
    out: &mut Vec<String>,
    n: usize,
    step: &TrajectoryStep,
    provenance: &HashMap<String, Option<String>>,
) {
    // Fixed-width step number so the ladder columns line up to ~99 steps.
    let head = |label: &str, detail: &str| -> String {
        if detail.is_empty() {
            format!("{n:>2}. {label}")
        } else {
            format!("{n:>2}. {label:<18} {detail}")
        }
    };

    match step {
        TrajectoryStep::ChunksRetrieved {
            count,
            symbols,
            similarity_scores,
        } => {
            let mut detail = format!("{count} chunk{}", if *count == 1 { "" } else { "s" });
            if !symbols.is_empty() {
                detail.push_str(&format!("  ·  symbols: {}", join_capped(symbols, 6)));
            }
            if !similarity_scores.is_empty() {
                detail.push_str(&format!(
                    "  ·  top sim {}",
                    join_scores_capped(similarity_scores, 3)
                ));
            }
            out.push(head("chunks retrieved", &detail));
        }
        TrajectoryStep::RulesApplied { rule_ids, source } => {
            let detail = format!(
                "{} rule{} ({})",
                rule_ids.len(),
                if rule_ids.len() == 1 { "" } else { "s" },
                rule_source_label(*source)
            );
            out.push(head("rules applied", &detail));
            for id in rule_ids {
                // Provenance suffix: `← learned from <repo>` when the local
                // skills table knows where the rule came from. The same
                // framing the agent sees in MCP serves + the TUI.
                let suffix = match provenance.get(id) {
                    Some(Some(repo)) if !repo.trim().is_empty() => {
                        format!("  \u{2190} learned from {repo}")
                    }
                    _ => String::new(),
                };
                out.push(format!("      - {id}{suffix}"));
            }
        }
        TrajectoryStep::PastVerdictsRecalled {
            count,
            top_similarities,
            recalled_items,
        } => {
            let mut detail = format!("{count} recalled");
            if !top_similarities.is_empty() {
                detail.push_str(&format!(
                    "  ·  top sim {}",
                    join_scores_capped(top_similarities, 3)
                ));
            }
            out.push(head("past verdicts", &detail));
            for item in recalled_items.iter().take(5) {
                out.push(format!("      - {}  ({:.2})", item.title, item.similarity));
            }
        }
        TrajectoryStep::LlmCall {
            perspective,
            input_tokens,
            output_tokens,
            ..
        } => {
            let detail =
                format!("{perspective}  ·  in {input_tokens} tok / out {output_tokens} tok");
            out.push(head("llm call", &detail));
        }
        TrajectoryStep::SelfCheck {
            keep_count,
            drop_count,
            avg_confidence,
        } => {
            let detail = format!(
                "kept {keep_count}, dropped {drop_count}  ·  avg confidence {avg_confidence:.2}"
            );
            out.push(head("self-check", &detail));
        }
        TrajectoryStep::SignatureConfidenceAdjust {
            accepted_bumps,
            rejected_bumps,
        } => {
            let detail = format!("+{accepted_bumps} accepted, -{rejected_bumps} rejected");
            out.push(head("confidence adjust", &detail));
        }
        TrajectoryStep::FinalDecision { issue_ids_emitted } => {
            let detail = if issue_ids_emitted.is_empty() {
                "no issues emitted".to_owned()
            } else {
                format!(
                    "{} issue{} emitted: {}",
                    issue_ids_emitted.len(),
                    if issue_ids_emitted.len() == 1 {
                        ""
                    } else {
                        "s"
                    },
                    join_capped(issue_ids_emitted, 8)
                )
            };
            out.push(head("final decision", &detail));
        }
        TrajectoryStep::McpResponseSize {
            tool,
            total_tokens,
            rules_injected,
        } => {
            let detail = format!("{tool}  ·  {total_tokens} tok, {rules_injected} rules injected");
            out.push(head("mcp response", &detail));
        }
        TrajectoryStep::RuleHitByOrigin {
            manual,
            conversation,
            pr_review,
            extracted,
            cloud,
        } => {
            let detail = format!(
                "manual {manual} · conv {conversation} · pr {pr_review} · extracted {extracted} · cloud {cloud}"
            );
            out.push(head("rule origins", &detail));
        }
        TrajectoryStep::RetrievalFilter { before, after } => {
            let detail = format!("{before} → {after} chunks after metadata filter");
            out.push(head("retrieval filter", &detail));
        }
        TrajectoryStep::HybridFusion {
            fts_hits,
            emb_hits,
            overlap,
        } => {
            let detail = format!("fts {fts_hits} · embed {emb_hits} · overlap {overlap}");
            out.push(head("hybrid fusion", &detail));
        }
        TrajectoryStep::AnnRecall {
            used,
            index_size,
            candidates,
        } => {
            let detail = format!(
                "{}  ·  index {index_size}, {candidates} candidates",
                if *used { "ANN used" } else { "linear fallback" }
            );
            out.push(head("ann recall", &detail));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use difflore_core::cloud::api_types::GetTrajectoryResponse;
    use difflore_core::review_trajectory::RecalledVerdict;

    fn doc(steps: Vec<TrajectoryStep>) -> GetTrajectoryResponse {
        GetTrajectoryResponse {
            id: "00000000-0000-0000-0000-0000000000aa".to_owned(),
            pr_review_id: "review-1".to_owned(),
            team_id: Some("team-1".to_owned()),
            steps,
            created_at: "2026-05-29T12:00:00.000Z".to_owned(),
        }
    }

    /// Full canonical trajectory → exact expected ladder. Locks the headline
    /// format, the per-rule `← learned from <repo>` provenance suffix, the
    /// recalled-verdict detail rows, and the trailing traceability line.
    #[test]
    fn renders_full_trajectory_ladder_with_provenance() {
        let steps = vec![
            TrajectoryStep::ChunksRetrieved {
                count: 4,
                symbols: vec!["handler".to_owned(), "parse".to_owned()],
                similarity_scores: vec![0.91, 0.84],
            },
            TrajectoryStep::RulesApplied {
                rule_ids: vec!["r1".to_owned(), "r2".to_owned()],
                source: RuleSource::Team,
            },
            TrajectoryStep::PastVerdictsRecalled {
                count: 1,
                top_similarities: vec![0.95],
                recalled_items: vec![RecalledVerdict {
                    id: "v1".to_owned(),
                    title: "avoid unwrap in request handlers".to_owned(),
                    similarity: 0.95,
                    excerpt: "fn handler() { x.unwrap() }".to_owned(),
                }],
            },
            TrajectoryStep::LlmCall {
                perspective: "safety".to_owned(),
                input_tokens: 200,
                output_tokens: 45,
                raw_output: None,
            },
            TrajectoryStep::SelfCheck {
                keep_count: 3,
                drop_count: 1,
                avg_confidence: 0.87,
            },
            TrajectoryStep::FinalDecision {
                issue_ids_emitted: vec!["issue-1".to_owned(), "issue-2".to_owned()],
            },
        ];

        let mut provenance = HashMap::new();
        provenance.insert("r1".to_owned(), Some("gin-gonic/gin".to_owned()));
        provenance.insert("r2".to_owned(), Some("tokio-rs/tokio".to_owned()));

        let lines = render_trajectory("review-1", &doc(steps), &provenance);

        let expected = vec![
            "Review trajectory  review-1".to_owned(),
            "recorded 2026-05-29T12:00:00.000Z  ·  6 steps".to_owned(),
            String::new(),
            " 1. chunks retrieved   4 chunks  ·  symbols: handler, parse  ·  top sim 0.91, 0.84"
                .to_owned(),
            " 2. rules applied      2 rules (team)".to_owned(),
            "      - r1  \u{2190} learned from gin-gonic/gin".to_owned(),
            "      - r2  \u{2190} learned from tokio-rs/tokio".to_owned(),
            " 3. past verdicts      1 recalled  ·  top sim 0.95".to_owned(),
            "      - avoid unwrap in request handlers  (0.95)".to_owned(),
            " 4. llm call           safety  ·  in 200 tok / out 45 tok".to_owned(),
            " 5. self-check         kept 3, dropped 1  ·  avg confidence 0.87".to_owned(),
            " 6. final decision     2 issues emitted: issue-1, issue-2".to_owned(),
            String::new(),
            "Every emitted issue traces to the rules + verdicts above.".to_owned(),
        ];

        assert_eq!(lines, expected);
    }

    /// A rule id missing from the provenance map (or mapped to `None`)
    /// renders without the `← learned from` suffix — the ladder must not
    /// fabricate provenance.
    #[test]
    fn omits_provenance_suffix_when_unknown() {
        let steps = vec![TrajectoryStep::RulesApplied {
            rule_ids: vec![
                "known".to_owned(),
                "unknown".to_owned(),
                "null-repo".to_owned(),
            ],
            source: RuleSource::Global,
        }];
        let mut provenance = HashMap::new();
        provenance.insert("known".to_owned(), Some("acme/widgets".to_owned()));
        provenance.insert("null-repo".to_owned(), None);

        let lines = render_trajectory("rid", &doc(steps), &provenance);

        assert!(
            lines
                .iter()
                .any(|l| l == " 1. rules applied      3 rules (global)")
        );
        assert!(
            lines
                .iter()
                .any(|l| l == "      - known  \u{2190} learned from acme/widgets")
        );
        // Unknown id: bare line, no suffix.
        assert!(lines.iter().any(|l| l == "      - unknown"));
        // Known id but null repo: also bare, no empty "learned from".
        assert!(lines.iter().any(|l| l == "      - null-repo"));
        assert!(!lines.iter().any(|l| l.contains("learned from \n")));
    }

    /// Empty trajectory (cloud's "review exists, nothing recorded" sentinel)
    /// → graceful message, never a bare header.
    #[test]
    fn empty_trajectory_renders_graceful_message() {
        let lines = render_trajectory("review-empty", &doc(vec![]), &HashMap::new());
        assert_eq!(lines[0], "Review trajectory  review-empty");
        assert!(lines.iter().any(|l| l.contains("0 steps")));
        assert!(
            lines
                .iter()
                .any(|l| l == "No trajectory was recorded for this review.")
        );
        // No step rows, no traceability footer.
        assert!(!lines.iter().any(|l| l.starts_with(" 1.")));
        assert!(!lines.iter().any(|l| l.contains("traces to the rules")));
    }

    /// The telemetry-flavoured variants (mcp/origins/fusion/ann/filter/
    /// confidence-adjust) all render a headline without panicking, so a
    /// trajectory that carries them replays cleanly rather than being a
    /// rendering hole.
    #[test]
    fn renders_telemetry_variants() {
        let steps = vec![
            TrajectoryStep::McpResponseSize {
                tool: "search_rules".to_owned(),
                total_tokens: 1234,
                rules_injected: 3,
            },
            TrajectoryStep::RuleHitByOrigin {
                manual: 1,
                conversation: 2,
                pr_review: 0,
                extracted: 1,
                cloud: 0,
            },
            TrajectoryStep::RetrievalFilter {
                before: 200,
                after: 40,
            },
            TrajectoryStep::HybridFusion {
                fts_hits: 10,
                emb_hits: 8,
                overlap: 5,
            },
            TrajectoryStep::AnnRecall {
                used: true,
                index_size: 500,
                candidates: 20,
            },
            TrajectoryStep::SignatureConfidenceAdjust {
                accepted_bumps: 2,
                rejected_bumps: 1,
            },
        ];

        let lines = render_trajectory("rid", &doc(steps), &HashMap::new());
        assert!(lines.iter().any(|l| l.contains("mcp response")));
        assert!(lines.iter().any(|l| l.contains("search_rules")));
        assert!(lines.iter().any(|l| l.contains("rule origins")));
        assert!(lines.iter().any(|l| l.contains("retrieval filter")));
        assert!(lines.iter().any(|l| l.contains("200 → 40 chunks")));
        assert!(lines.iter().any(|l| l.contains("hybrid fusion")));
        assert!(lines.iter().any(|l| l.contains("ann recall")));
        assert!(lines.iter().any(|l| l.contains("ANN used")));
        assert!(lines.iter().any(|l| l.contains("confidence adjust")));
    }

    /// Long symbol / issue lists are capped with a `(+K more)` marker so a
    /// wide step never sprawls across the terminal.
    #[test]
    fn caps_long_lists_with_more_marker() {
        let symbols: Vec<String> = (0..10).map(|i| format!("sym{i}")).collect();
        let issues: Vec<String> = (0..12).map(|i| format!("issue-{i}")).collect();
        let steps = vec![
            TrajectoryStep::ChunksRetrieved {
                count: 10,
                symbols,
                similarity_scores: vec![],
            },
            TrajectoryStep::FinalDecision {
                issue_ids_emitted: issues,
            },
        ];
        let lines = render_trajectory("rid", &doc(steps), &HashMap::new());
        assert!(
            lines.iter().any(|l| l.contains("(+4 more)")),
            "symbols cap: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("(+4 more)")),
            "issues cap: {lines:?}"
        );
    }

    #[test]
    fn classify_fetch_error_buckets() {
        assert_eq!(
            classify_fetch_error("not_logged_in"),
            FetchError::NotLoggedIn
        );
        assert_eq!(
            classify_fetch_error("[get_trajectory] returned 403 Forbidden: plan_limit_exceeded"),
            FetchError::PlanGated
        );
        assert_eq!(
            classify_fetch_error("[get_trajectory] returned 404 Not Found: ReviewNotFound"),
            FetchError::NotFound
        );
        assert_eq!(
            classify_fetch_error("[get_trajectory] network error: connection refused"),
            FetchError::Other
        );
        assert_eq!(
            classify_fetch_error("not_logged_in").as_str(),
            "not_logged_in"
        );
        assert_eq!(FetchError::Other.as_str(), "fetch_failed");
    }
}
