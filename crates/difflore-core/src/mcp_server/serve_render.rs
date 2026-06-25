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
use crate::domain::rule_fingerprint::memory_citation_token;

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
    /// Compact whyRanked facts (`path-hint; band 9/10; source manual`),
    /// rendered as a `why:` segment on the header line. `None` (e.g.
    /// cross-repo starter rules with no arbitration metadata) renders the
    /// pre-whyRanked header byte-identically. Costs ~5–10 estimated tokens
    /// per rule; the hook's injection budget gate sees it because the segment
    /// is part of the rule block text it measures.
    pub why: Option<&'a str>,
}

/// Render one rule's product-facing block: the title-in-header attribution
/// line (`## Memory N [df:N-fp]: <title> ← learned from <repo> (rank score:
/// … · raw: …)`), an optional cloud `Proof:` line, the rule body, and any
/// captured `### Examples`, terminated by the `\n---\n\n` separator.
pub(crate) fn render_rule_block(args: &RuleBlockArgs<'_>) -> String {
    let &RuleBlockArgs {
        position,
        rel,
        rule,
        trust_evidence,
        examples,
        example_bad_label,
        example_good_label,
        why,
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
    // Use the same "<- learned from <repo>" framing as `review`,
    // `recall`, `init`, and the cloud rule-detail page so the agent reads the
    // same provenance grammar everywhere.
    let source_seg = source
        .map(|s| format!(" \u{2190} learned from {s}"))
        .unwrap_or_default();
    // whyRanked: surface the arbitration facts (path hint / score band /
    // source priority) on the same header line the agent already reads, so
    // citing a memory carries its ranking justification for free.
    let why_seg = why.map(|w| format!(" | why: {w}")).unwrap_or_default();
    let citation_token = memory_citation_token(position, &rule.skill_id);
    let mut text = format!(
        "## Memory {} [{}]: {}{} (rank score: {:.2} | raw: {:.3}{})\n\n",
        position, citation_token, title, source_seg, rel, rule.score, why_seg
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

/// Gate a serve-record error prefix on the debug-telemetry flag: returns
/// `Some(prefix)` only when `DIFFLORE_DEBUG_TELEMETRY` is on, so [`serve_and_record`]
/// logs `record` failures exactly when the hand-rolled tool sites did (each
/// previously wrapped its `eprintln!` in a `debug_telemetry()` guard).
pub(crate) fn serve_record_err_prefix(prefix: &str) -> Option<&str> {
    crate::infra::env::debug_telemetry().then_some(prefix)
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
    let record_result = crate::observability::mcp_rule_serves::record(
        db,
        &crate::observability::mcp_rule_serves::McpRuleServeInput {
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
        query_hash: crate::observability::mcp_rule_serves::query_hash(serve.query),
        rule_ids: serve.rule_ids.to_vec(),
        top_k: serve.top_k,
        was_empty: serve.rule_ids.is_empty(),
        strict_match_count: serve.strict_match_count,
        estimated_tokens: serve.estimated_tokens,
        served_at: chrono::Utc::now(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::estimate_tokens;
    use super::{RuleBlockArgs, render_rule_block};
    use crate::context::retrieval::ScoredRuleChunk;
    use crate::mcp_server::trust_proof::RuleTrustMap;

    fn rule() -> ScoredRuleChunk {
        ScoredRuleChunk {
            skill_id: "why-budget".to_owned(),
            content: "Rule ID: why-budget\nRule Name: Avoid unwrap in handlers\nSource: acme/widgets\n\nNever unwrap request payloads in handlers.".to_owned(),
            score: 0.012,
            confidence: 0.7,
        }
    }

    fn render(why: Option<&str>) -> String {
        let trust = RuleTrustMap::new();
        render_rule_block(&RuleBlockArgs {
            position: 1,
            rel: 0.95,
            rule: &rule(),
            trust_evidence: &trust,
            examples: None,
            example_bad_label: "- Bad:",
            example_good_label: "- Good:",
            why,
        })
    }

    #[test]
    fn why_segment_lands_on_header_line_and_none_is_byte_identical() {
        let with_why = render(Some("path-hint; band 9/10; source manual"));
        let header = with_why.lines().next().expect("header line");
        assert!(
            header.starts_with("## Memory 1 [df:1-"),
            "header must carry stable citation token: {header}"
        );
        assert!(
            header.contains("| why: path-hint; band 9/10; source manual)"),
            "why segment must ride the header line: {header}"
        );

        let without = render(None);
        assert!(
            !without.contains("why:"),
            "None must render the pre-whyRanked block byte-identically"
        );
    }

    #[test]
    fn why_segment_costs_about_five_to_twelve_tokens_per_rule() {
        // Budget accounting (cli-spec ~1500 token hook budget): the why
        // segment must stay a single-digit-ish token overhead per rule using
        // the same chars/4 estimate the budget gate applies. The worst-case
        // grammar ("path-hint; band 10/10; source conversation") is the
        // longest string the arbitration layer can emit.
        let baseline = estimate_tokens(&render(None));
        let with_why = estimate_tokens(&render(Some("path-hint; band 10/10; source conversation")));
        let overhead = with_why.saturating_sub(baseline);
        assert!(
            (1..=13).contains(&overhead),
            "why overhead must be ~5–12 estimated tokens, got {overhead}"
        );
    }
}
