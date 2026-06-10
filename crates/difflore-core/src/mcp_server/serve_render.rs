//! Shared fragments for full rule recall paths: the `get_rules` detail fetch
//! and the in-process hook path (`hook::fetch_relevant_rules_for_hook`).
//!
//! The two recall surfaces are parallel near-duplicates so each can evolve its
//! own retrieval tuning, score floor / budget cap, trailing summary, and event
//! dispatch. This module factors out only the two highest-drift fragments — the
//! per-rule header/provenance/`Proof:` block and the serve-ledger +
//! `McpRuleServed` payload — which MUST stay byte-identical across both paths
//! or the rule body and the telemetry diverge silently.

use sqlx::SqlitePool;

use crate::cloud::observations::ObservationEvent;
use crate::context::retrieval::ScoredRuleChunk;
use crate::context::rule_source::RuleExample;

use super::trust_proof::{RuleTrustMap, format_trust_evidence};

/// Inputs for [`render_rule_block`]. Only the example bad/good marker labels
/// differ between the MCP tool (`❌ Bad:` / `✅ Good:`) and the hook (`- Bad:` /
/// `- Good:`), so those are parameterised rather than baked in.
pub(crate) struct RuleBlockArgs<'a> {
    /// 1-based memory number shown in the `## Memory {n}:` header, resolved by
    /// the caller (tool: enumerate index; hook: budget-gated `injected + 1`).
    pub position: usize,
    /// Rank-relative score (`rule.score / max_score`, or `0.0`), computed by
    /// the caller over the exact slice it iterates.
    pub rel: f64,
    pub rule: &'a ScoredRuleChunk,
    pub trust_evidence: &'a RuleTrustMap,
    pub examples: Option<&'a Vec<RuleExample>>,
    /// e.g. `"❌ Bad:"` (MCP tool) or `"- Bad:"` (hook).
    pub example_bad_label: &'a str,
    /// e.g. `"✅ Good:"` (MCP tool) or `"- Good:"` (hook).
    pub example_good_label: &'a str,
}

/// Render one rule's product-facing block: the title-in-header attribution
/// line (`## Memory N: <title> ← learned from <repo> (rank score: … · raw:
/// …)`), an optional cloud `Proof:` line, the rule body, and any captured
/// `### Examples`, terminated by the `\n---\n\n` separator.
pub(crate) fn render_rule_block(args: &RuleBlockArgs<'_>) -> String {
    let &RuleBlockArgs {
        position,
        rel,
        rule,
        trust_evidence,
        examples,
        example_bad_label,
        example_good_label,
    } = args;

    // Pull the title out of the indexed body so the header is self-describing:
    // rule numbers are call-local, titles are stable across calls.
    let title = rule
        .content
        .lines()
        .find_map(|l| l.strip_prefix("Rule Name: "))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("(untitled)");
    let source = rule
        .content
        .lines()
        .find_map(|l| l.strip_prefix("Source: "))
        .map(str::trim)
        .filter(|s| !s.is_empty());
    // Use the same "<- learned from <repo>" framing as `fix --preview`,
    // `recall`, the TUI, `init`, and the cloud rule-detail page so the agent
    // reads the same provenance grammar everywhere.
    let source_seg = source
        .map(|s| format!(" \u{2190} learned from {s}"))
        .unwrap_or_default();
    let mut text = format!(
        "## Memory {}: {}{} (rank score: {:.2} | raw: {:.3})\n\n",
        position, title, source_seg, rel, rule.score
    );
    if let Some(proof) = trust_evidence.get(&rule.skill_id)
        && let Some(label) = format_trust_evidence(proof)
    {
        text.push_str(&format!("Proof: {label}\n\n"));
    }
    text.push_str(&rule.content);

    if let Some(examples) = examples
        && !examples.is_empty()
    {
        text.push_str("\n\n### Examples\n");
        for ex in examples {
            text.push_str(&format!(
                "\n{}\n```\n{}\n```\n\n{}\n```\n{}\n```\n",
                example_bad_label, ex.bad_code, example_good_label, ex.good_code
            ));
            if let Some(desc) = &ex.description
                && !desc.is_empty()
            {
                text.push_str(&format!("\n{desc}\n"));
            }
        }
    }
    text.push_str("\n---\n\n");
    text
}

/// Shared scalar inputs for the local serve ledger row and the cloud
/// `McpRuleServed` event. Numeric fields are `i64` so the caller controls its
/// own conversion.
pub(crate) struct RuleServe<'a> {
    pub tool: &'a str,
    /// Ledger `session_id` (nullable). Tool passes `Some("mcp-server")`; hook
    /// passes its incoming `Option<&str>`.
    pub session_id: Option<&'a str>,
    /// Cloud-event `session_id` (non-null), resolved by the caller (tool passes
    /// its `session_id`; hook passes `session_id.unwrap_or("hook")`).
    pub event_session_id: &'a str,
    pub repo_full_name: Option<&'a str>,
    pub target_file: Option<&'a str>,
    pub query: &'a str,
    pub rule_ids: &'a [String],
    pub top_k: i64,
    pub strict_match_count: i64,
    pub estimated_tokens: i64,
}

/// Record the local `mcp_rule_serves` ledger row, then return the constructed
/// `ObservationEvent::McpRuleServed` for the caller to dispatch (the tool spawns
/// a task that flushes to cloud and drains the outbox; the hook uses
/// `enqueue_default`). Dispatch is not centralized here because the two paths
/// differ (spawn vs inline, drain vs no-drain).
///
/// `record_err_prefix`: `Some(p)` logs `record` failures as `"{p}: {e}"`;
/// `None` swallows the error silently.
pub(crate) async fn serve_and_record(
    db: &SqlitePool,
    serve: RuleServe<'_>,
    record_err_prefix: Option<&str>,
) -> ObservationEvent {
    let record_result = crate::mcp_rule_serves::record(
        db,
        &crate::mcp_rule_serves::McpRuleServeInput {
            tool: serve.tool,
            session_id: serve.session_id,
            repo_full_name: serve.repo_full_name,
            file_path: serve.target_file,
            query_text: serve.query,
            rule_ids: serve.rule_ids,
            top_k: serve.top_k,
            strict_match_count: serve.strict_match_count,
            estimated_tokens: serve.estimated_tokens,
        },
    )
    .await;
    if let (Err(e), Some(prefix)) = (record_result, record_err_prefix) {
        eprintln!("{prefix}: {e}");
    }

    ObservationEvent::McpRuleServed {
        tool: serve.tool.to_owned(),
        session_id: serve.event_session_id.to_owned(),
        repo_full_name: serve.repo_full_name.map(ToOwned::to_owned),
        file_path: serve.target_file.map(ToOwned::to_owned),
        query_hash: crate::mcp_rule_serves::query_hash(serve.query),
        rule_ids: serve.rule_ids.to_vec(),
        top_k: serve.top_k,
        was_empty: serve.rule_ids.is_empty(),
        strict_match_count: serve.strict_match_count,
        estimated_tokens: serve.estimated_tokens,
        served_at: chrono::Utc::now(),
    }
}
