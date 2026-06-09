//! Shared, behavior-preserving fragments for full rule recall paths,
//! including `get_rules` detail fetches and the in-process hook path
//! (`hook::fetch_relevant_rules_for_hook`).
//!
//! These two functions are *deliberately* parallel near-duplicates so the
//! JSON-RPC tool surface and the hook surface can evolve independently
//! (custom `_meta`, deterministic empty-retry, token-budget cap, different
//! trailing summary wording). That parallelism is also the drift hazard:
//! the per-rule **header / `learned from` provenance / `Proof:` line** and
//! the **serve-ledger + `McpRuleServed` payload** must stay byte-identical
//! across both paths or the product-facing rule body and the telemetry
//! diverge silently.
//!
//! This module factors out exactly those two highest-drift fragments and
//! nothing else. Each helper reproduces the pre-refactor output
//! byte-for-byte; the two surfaces still own their own retrieval tuning,
//! score floor / budget cap, trailing summary, and event dispatch.
//!
//! Intentionally NOT unified (would risk observable drift — see the
//! function docs in `hook.rs`):
//!   * retrieval args (`lexical_query`, `embedding_timeout`,
//!     `adaptive_prune`, `candidate_limit`/`top_k`),
//!   * the tool's deterministic empty-recall retry vs the hook's raw-score
//!     floor,
//!   * the hook's per-rule token-budget cap,
//!   * the trailing one-line summary wording,
//!   * the example bad/good markers (`❌`/`✅` tool vs `-`/`-` hook),
//!   * event dispatch (tool: `tokio::spawn` + flush + outbox drain; hook:
//!     `enqueue_default`).

use sqlx::SqlitePool;

use crate::cloud::observations::ObservationEvent;
use crate::context::retrieval::ScoredRuleChunk;
use crate::context::rule_source::RuleExample;

use super::trust_proof::{RuleTrustMap, format_trust_evidence};

/// Inputs for [`render_rule_block`]. The header, `← learned from <repo>`
/// provenance segment, optional `Proof:` line, and rule body are emitted
/// byte-identically for both surfaces; only the example bad/good marker
/// labels differ between the MCP tool (`❌ Bad:` / `✅ Good:`) and the hook
/// (`- Bad:` / `- Good:`), so those are parameterised rather than baked in.
pub(crate) struct RuleBlockArgs<'a> {
    /// 1-based memory number shown in the `## Memory {n}:` header. The
    /// tool uses the final enumerate index; the hook uses its
    /// budget-gated `injected + 1`, so the caller passes the resolved
    /// number rather than letting this helper guess it.
    pub position: usize,
    /// Rank-relative score (`rule.score / max_score`, or `0.0`). Computed
    /// by the caller over the exact slice it iterates so the displayed
    /// `rank score` matches each surface's pre-refactor value.
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
///
/// Pure / synchronous. The output is byte-identical to the inline loop
/// bodies used by full rule detail fetches and
/// `fetch_relevant_rules_for_hook` (given the matching example labels).
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

    // Pull title out of the indexed body so the markdown header is
    // self-describing — agents that cite "applying Rule 2" otherwise
    // give the user no idea which rule they followed (rule numbers
    // are call-local, titles are stable across calls).
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
    // `recall`, the TUI, `init`, and the cloud rule-detail page so the
    // agent reads the same provenance grammar everywhere. When the
    // agent cites this in chat ("applying Rule 2 (learned from
    // gin-gonic/gin)"), the user sees the consistent product wording
    // surface in their main work tool — strongest free brand
    // propagation we have.
    let source_seg = source
        .map(|s| format!(" \u{2190} learned from {s}"))
        .unwrap_or_default();
    let mut text = format!(
        "## Memory {}: {}{} (rank score: {:.2} · raw: {:.3})\n\n",
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
/// `McpRuleServed` event. Every numeric field is already typed as `i64`
/// so the caller keeps full control of its own conversion (the tool does
/// `i64::try_from(top_k).unwrap_or(i64::MAX)`; the hook passes the literal
/// `5`), guaranteeing the recorded values are exactly what each surface
/// recorded before this refactor.
pub(crate) struct RuleServe<'a> {
    pub tool: &'a str,
    /// Ledger `session_id` (nullable). Tool passes `Some("mcp-server")`;
    /// hook passes its incoming `Option<&str>`.
    pub session_id: Option<&'a str>,
    /// Cloud-event `session_id` (non-null). The caller resolves the
    /// default so the helper does not bake one in: the tool passes its
    /// `session_id` &str directly, the hook passes
    /// `session_id.unwrap_or("hook")` — matching prior behavior exactly.
    pub event_session_id: &'a str,
    pub repo_full_name: Option<&'a str>,
    pub target_file: Option<&'a str>,
    pub query: &'a str,
    pub rule_ids: &'a [String],
    pub top_k: i64,
    pub strict_match_count: i64,
    pub estimated_tokens: i64,
}

/// Record the local `mcp_rule_serves` ledger row, then return the
/// constructed `ObservationEvent::McpRuleServed` for the caller to
/// dispatch via whatever path semantics it requires (the tool spawns a
/// task that flushes to cloud and drains the MCP query outbox; the hook
/// uses `enqueue_default`). Dispatch is intentionally NOT centralized
/// here because the two paths differ (spawn vs inline, drain vs no-drain).
///
/// `was_empty` is derived as `rule_ids.is_empty()`, which equals the
/// literal/expression each call site used before (`true` with `&[]` in
/// the tool's empty branch, `serve_rule_ids.is_empty()` in its success
/// branch, `rule_ids.is_empty()` in the hook).
///
/// `record_err_prefix`: `Some(p)` → log `record` failures as
/// `"{p}: {e}"` using the caller's tool-specific prefix;
/// `None` → swallow the error silently, matching the hook's prior
/// `let _ = …record(…).await;`.
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
