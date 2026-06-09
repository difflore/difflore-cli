//! Render a mined/remembered rule as a concrete **code-spec** instead of a
//! free-prose blob (roadmap item ⑥).
//!
//! The output is a single slot-based Markdown template with progressive
//! disclosure: a section is emitted only when its source data exists, and a
//! missing slot is silently dropped rather than rendered as "N/A" (an empty
//! placeholder reads as fabricated emptiness). Every rendered slot traces to a
//! field we already store or a stored `rule_example` — nothing here invents a
//! contract, a validation condition, a trigger, or a metric. The mapping is
//! pure, deterministic string reshaping with no LLM call and no new persisted
//! data.
//!
//! This module is intentionally **public and DB-free** so item ① (rule packs)
//! can import the same `render_*` helpers and serialize a published pack in the
//! exact format a locally-recalled rule renders in — see the roadmap's
//! "missing-field decision table" which both surfaces honor.
//!
//! The MCP `get_rules` detail path drives this via
//! [`crate::mcp_server::tools::util::render_full_rule_with_examples`], which
//! builds a [`RuleRenderInput`] from its private `SkillDetailRow` and calls
//! [`render_code_spec`]. The PostToolUse hook keeps its own compact render
//! (`serve_render::render_rule_block`) by design — it renders from the indexed
//! body string, not a row, and enforces a tight per-injection token budget.

use crate::context::rule_source::{RuleExample, repo_scope_from_source_repo};
use crate::skills::{parse_candidate_drafted_rule, parse_candidate_source_proof};

/// Cap the rendered reviewer excerpt so a verbose mined rule can't balloon a
/// `get_rules` batch of 20. The stored excerpt is already capped (≤500 chars in
/// `candidates::reviewer_excerpt`); we tighten it further at render time.
const REVIEWER_EXCERPT_RENDER_LIMIT: usize = 300;

/// Cap the rationale (the prose that follows a conversation rule's directive
/// first line) at roughly the first two sentences so the Contract stays a
/// checkable obligation, not a paragraph.
const RATIONALE_SENTENCE_LIMIT: usize = 2;

/// Borrowed, DB-free view of the fields a renderer needs. Built from the MCP
/// layer's private `SkillDetailRow` (which can't leak out of `mcp_server`) and
/// — for item ① — from a pack row, so both surfaces render identically.
pub struct RuleRenderInput<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub r#type: &'a str,
    pub confidence: f64,
    pub origin: &'a str,
    /// `owner/repo` attribution column, if any.
    pub source_repo: Option<&'a str>,
    /// Already-parsed `file_patterns` (use [`crate::mcp_server::tools::util::parse_file_patterns`]
    /// or the candidate parser at the call site).
    pub file_patterns: &'a [String],
    /// The rule body prose. For mined rules this is the structured
    /// `Rule:` / `Source evidence:` / `Reviewer said:` shape that the candidate
    /// parsers understand.
    pub description: &'a str,
    /// `skills.trigger`, surfaced when present (free specificity).
    pub trigger: Option<&'a str>,
    /// `skills.check_prompt`, surfaced when present.
    pub check_prompt: Option<&'a str>,
    /// Structured example rows, already loaded by the caller.
    pub examples: Option<&'a [RuleExample]>,
}

/// Whether a directive states a positive obligation (`MUST`) or a prohibition
/// (`AVOID`). Cheap keyword classify; defaults to `Must` when ambiguous.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Polarity {
    Must,
    Avoid,
}

impl Polarity {
    const fn label(self) -> &'static str {
        match self {
            Self::Must => "MUST",
            Self::Avoid => "AVOID",
        }
    }
}

/// Classify a directive statement as a positive obligation or a prohibition by
/// scanning for negative-polarity keywords. Pure and unit-testable; ambiguous
/// statements default to [`Polarity::Must`].
#[must_use]
pub fn directive_polarity(statement: &str) -> Polarity {
    let lower = statement.to_ascii_lowercase();
    const AVOID_MARKERS: &[&str] = &[
        "avoid",
        "don't",
        "do not",
        "dont",
        "never",
        "no longer",
        "must not",
        "mustn't",
        "should not",
        "shouldn't",
        "stop ",
    ];
    if AVOID_MARKERS.iter().any(|m| lower.contains(m)) {
        Polarity::Avoid
    } else {
        Polarity::Must
    }
}

/// Human-readable origin label for the code-spec header. Mirrors the
/// `origin_to_kind` mapping used by the timeline/telemetry so the same origin
/// string reads consistently across surfaces.
fn origin_label(origin: &str) -> &str {
    match origin {
        "pr_review" => "PR review",
        "conversation" => "remembered in conversation",
        "extracted" => "extracted",
        "manual" => "manual",
        "cloud" => "cloud-synced",
        "team" => "team-synced",
        other => other,
    }
}

/// Split a paragraph into up to `limit` sentences, re-joining them. Used to cap
/// a rationale; deliberately simple (splits on `. ` / `? ` / `! `) so it stays
/// pure and predictable. Never drops body text silently — if there are fewer
/// than `limit` sentences the whole input round-trips.
fn first_sentences(text: &str, limit: usize) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() || limit == 0 {
        return trimmed.to_owned();
    }
    let mut out = String::new();
    let mut count = 0usize;
    let mut chars = trimmed.chars().peekable();
    while let Some(c) = chars.next() {
        out.push(c);
        if matches!(c, '.' | '!' | '?') && chars.peek().is_none_or(|n| n.is_whitespace()) {
            count += 1;
            if count >= limit {
                break;
            }
        }
    }
    out.trim().to_owned()
}

/// Truncate to at most `limit` chars without splitting a grapheme, appending an
/// ellipsis only when something was actually dropped.
fn truncate_with_ellipsis(s: &str, limit: usize) -> String {
    let mut chars = s.chars();
    let head: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        format!("{head}...")
    } else {
        head
    }
}

/// `### Contract` block. Always present (the verbatim-description fallback
/// guarantees body text is never dropped). For mined rules the `Rule:`
/// statement becomes a single `MUST:`/`AVOID:` obligation; for verb-led
/// conversation rules the first sentence becomes the obligation and the rest
/// becomes a `Rationale:` sub-line (the WHY is the value, so it is kept).
#[must_use]
pub fn render_contract_block(origin: &str, description: &str) -> String {
    let mut out = String::from("### Contract\n");

    // Mined rule: reuse the battle-tested candidate parser, don't re-implement.
    if origin == "pr_review"
        && let Some(stmt) = parse_candidate_drafted_rule(description)
        && !stmt.trim().is_empty()
    {
        let stmt = stmt.trim();
        let polarity = directive_polarity(stmt);
        out.push_str(&format!("- {}: {}\n", polarity.label(), stmt));
        return out;
    }

    // Conversation / other origin with a verb-led first line: the first
    // sentence becomes the obligation, remaining prose becomes the rationale.
    let trimmed = description.trim();
    if let Some((first, rest)) = split_directive_and_rationale(trimmed) {
        let polarity = directive_polarity(&first);
        out.push_str(&format!("- {}: {}\n", polarity.label(), first.trim()));
        if let Some(rest) = rest {
            let rationale = first_sentences(&rest, RATIONALE_SENTENCE_LIMIT);
            if !rationale.is_empty() {
                out.push_str(&format!("\nRationale: {rationale}\n"));
            }
        }
        return out;
    }

    // Fallback (reached only for an empty/edge body, since a non-empty body
    // always yields a directive first line above): emit the raw description
    // verbatim and never drop body text. A contract we can't structure is
    // still strictly more useful rendered than dropped.
    if trimmed.is_empty() {
        // Defensive: keep the section non-empty so the template stays valid.
        out.push_str("- (no rule body)\n");
    } else {
        out.push_str(&format!("{trimmed}\n"));
    }
    out
}

/// Split a body into (first sentence, remaining prose) when the first line
/// reads like a directive. Returns `None` when the body has no usable first
/// line so the caller falls back to verbatim rendering.
fn split_directive_and_rationale(body: &str) -> Option<(String, Option<String>)> {
    let body = body.trim();
    if body.is_empty() {
        return None;
    }
    // Use the first sentence as the obligation; everything after it is rationale.
    let first = first_sentences(body, 1);
    if first.is_empty() {
        return None;
    }
    let rest = body
        .get(first.len()..)
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .map(ToOwned::to_owned);
    Some((first, rest))
}

/// `### Validation / Error matrix` block, or `None` when no row is derivable.
///
/// Derivation-only, never generation: a row comes from a stored `rule_example`
/// (its own bad/good/description) or from a mined directive that already has a
/// "When X, do Y" shape (the distiller emits "When touching `path`, …"). If
/// neither source exists the whole section is omitted — we never render an
/// empty table or a hallucinated edge case.
#[must_use]
pub fn render_validation_matrix(
    origin: &str,
    description: &str,
    examples: Option<&[RuleExample]>,
) -> Option<String> {
    let mut rows: Vec<(String, String, String)> = Vec::new();

    // Row from the first example: the bad pattern → the good form, flagged.
    if let Some(ex) = examples.and_then(|e| e.first()) {
        let condition = matrix_cell(&ex.bad_code);
        let expected = matrix_cell(&ex.good_code);
        let on_violation = ex
            .description
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .map_or_else(|| "reviewer flagged this".to_owned(), matrix_cell);
        rows.push((condition, expected, on_violation));
    }

    // Row from a mined "When X, Y" directive: split on the first comma.
    if origin == "pr_review"
        && let Some(stmt) = parse_candidate_drafted_rule(description)
        && let Some(row) = when_directive_row(&stmt)
    {
        rows.push(row);
    }

    if rows.is_empty() {
        return None;
    }

    let mut out = String::from("### Validation / Error matrix\n");
    out.push_str("| Condition | Expected | On violation |\n");
    out.push_str("|---|---|---|\n");
    for (cond, expected, on_violation) in rows {
        out.push_str(&format!("| {cond} | {expected} | {on_violation} |\n"));
    }
    Some(out)
}

/// Turn a "When X, Y" directive into a single matrix row. The distiller emits
/// "When touching `path`, <directive>." so we split on the first comma into
/// Condition / Expected. Returns `None` when there is no comma (no "when…,"
/// shape) so the caller doesn't fabricate a condition.
fn when_directive_row(statement: &str) -> Option<(String, String, String)> {
    let stmt = statement.trim();
    let lower = stmt.to_ascii_lowercase();
    if !lower.starts_with("when ") {
        return None;
    }
    let (cond, expected) = stmt.split_once(',')?;
    let cond = cond.trim();
    let expected = expected.trim().trim_end_matches('.').trim();
    if cond.is_empty() || expected.is_empty() {
        return None;
    }
    Some((
        matrix_cell(cond),
        matrix_cell(expected),
        "directive applies".to_owned(),
    ))
}

/// Flatten a snippet into a single Markdown-table cell: collapse newlines,
/// escape the pipe that would break the column, and cap length so a multi-line
/// code example can't blow out the table.
fn matrix_cell(value: &str) -> String {
    let flat: String = value
        .trim()
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    let escaped = flat.replace('|', "\\|");
    truncate_with_ellipsis(escaped.trim(), 80)
}

/// `### Cases` block, or `None` when there are no examples. Framed as a
/// conformance test (`❌ Counter-example` / `✅ Conforming`) rather than
/// decoration so the agent reads the pair as the acceptance criterion.
#[must_use]
pub fn render_cases_block(examples: Option<&[RuleExample]>) -> Option<String> {
    let examples = examples.filter(|e| !e.is_empty())?;
    let mut out = String::from("### Cases\n");
    for ex in examples {
        out.push_str(&format!(
            "❌ Counter-example:\n```\n{}\n```\n\n✅ Conforming:\n```\n{}\n```\n",
            ex.bad_code, ex.good_code
        ));
        if let Some(desc) = ex.description.as_deref().map(str::trim)
            && !desc.is_empty()
        {
            out.push_str(&format!("\n{desc}\n"));
        }
    }
    Some(out)
}

/// `### Provenance` block. Reuses the candidate source-proof parser to pull the
/// PR `Source:`, the `Comment:` URL and a short reviewer excerpt out of a mined
/// description. Returns `None` only when there is neither parseable source proof
/// nor a `source_repo` — the header's `← learned from` segment already carries
/// the top-level attribution, so a conversation rule with no proof omits this
/// section rather than repeating the header.
#[must_use]
pub fn render_provenance_block(description: &str, source_repo: Option<&str>) -> Option<String> {
    let proof = parse_candidate_source_proof(description);
    let has_repo = source_repo.map(str::trim).is_some_and(|r| !r.is_empty());
    if proof.is_none() && !has_repo {
        return None;
    }

    let mut out = String::from("### Provenance\n");
    if let Some(proof) = proof.as_ref() {
        let source = proof
            .source
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let comment = proof
            .comment_url
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        match (source, comment) {
            (Some(s), Some(c)) => out.push_str(&format!("Source: {s} · {c}\n")),
            (Some(s), None) => out.push_str(&format!("Source: {s}\n")),
            (None, Some(c)) => out.push_str(&format!("Source: {c}\n")),
            (None, None) => {}
        }
        if let Some(excerpt) = proof
            .excerpt
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let excerpt = truncate_with_ellipsis(excerpt, REVIEWER_EXCERPT_RENDER_LIMIT);
            out.push_str(&format!("Reviewer: {excerpt}\n"));
        }
    }
    // If proof carried nothing renderable but we still have a repo, keep the
    // section honest: the header already shows "← learned from", so only emit
    // the section when it adds something. Drop an otherwise-empty header.
    if out == "### Provenance\n" {
        return None;
    }
    Some(out)
}

/// Render a full rule as the §3 code-spec template. Pure and DB-free: the
/// caller supplies a [`RuleRenderInput`]; every section is derived
/// deterministically from those fields with progressive disclosure.
#[must_use]
pub fn render_code_spec(input: &RuleRenderInput<'_>) -> String {
    let mut out = String::new();

    // Header. Keep the id in a stable, greppable position.
    out.push_str(&format!("## Rule {} — {}\n", input.id, input.name));

    // Scope line.
    let scope = if input.file_patterns.is_empty() {
        "repo-wide (no file scope)".to_owned()
    } else {
        input.file_patterns.join(", ")
    };
    out.push_str(&format!("Scope: {scope}\n"));

    // Type · Confidence · Origin (+ learned-from attribution).
    let learned_from = repo_scope_from_source_repo(input.source_repo)
        .map(|s| format!(" \u{2190} learned from {s}"))
        .unwrap_or_default();
    out.push_str(&format!(
        "Type: {} · Confidence: {:.2} · Origin: {}{}\n",
        input.r#type,
        input.confidence,
        origin_label(input.origin),
        learned_from,
    ));

    // Contract — always present.
    out.push('\n');
    out.push_str(&render_contract_block(input.origin, input.description));

    // Validation / Error matrix — only when derivable.
    if let Some(matrix) = render_validation_matrix(input.origin, input.description, input.examples)
    {
        out.push('\n');
        out.push_str(&matrix);
    }

    // Trigger — only when the column is populated.
    if let Some(trigger) = input.trigger.map(str::trim).filter(|t| !t.is_empty()) {
        out.push_str(&format!("\n### Trigger\n{trigger}\n"));
    }

    // Self-check — only when the column is populated.
    if let Some(check) = input.check_prompt.map(str::trim).filter(|c| !c.is_empty()) {
        out.push_str(&format!("\n### Self-check\n{check}\n"));
    }

    // Cases — only when ≥1 example.
    if let Some(cases) = render_cases_block(input.examples) {
        out.push('\n');
        out.push_str(&cases);
    }

    // Provenance — source proof and/or learned-from repo.
    if let Some(prov) = render_provenance_block(input.description, input.source_repo) {
        out.push('\n');
        out.push_str(&prov);
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::rule_source::RuleExample;

    fn example(bad: &str, good: &str, desc: Option<&str>) -> RuleExample {
        RuleExample {
            id: "ex1".to_owned(),
            skill_id: "rule1".to_owned(),
            bad_code: bad.to_owned(),
            good_code: good.to_owned(),
            description: desc.map(ToOwned::to_owned),
            source: "test".to_owned(),
        }
    }

    #[test]
    fn directive_polarity_classifies_negatives_as_avoid() {
        assert_eq!(
            directive_polarity("never unwrap in handlers"),
            Polarity::Avoid
        );
        assert_eq!(directive_polarity("Avoid magic numbers"), Polarity::Avoid);
        assert_eq!(directive_polarity("don't swallow errors"), Polarity::Avoid);
        assert_eq!(
            directive_polarity("Should not panic in hot paths"),
            Polarity::Avoid
        );
    }

    #[test]
    fn directive_polarity_defaults_to_must() {
        assert_eq!(
            directive_polarity("prefer structured parsing in resolve"),
            Polarity::Must
        );
        assert_eq!(
            directive_polarity("return a structured error instead"),
            Polarity::Must
        );
    }

    #[test]
    fn first_sentences_caps_at_limit_and_roundtrips_shorter() {
        assert_eq!(first_sentences("One. Two. Three.", 2), "One. Two.");
        assert_eq!(first_sentences("Only one sentence", 2), "Only one sentence");
        assert_eq!(first_sentences("", 2), "");
    }

    #[test]
    fn contract_from_mined_rule_renders_must_bullet() {
        let desc = "Rule:\nWhen touching `src/**/*.rs`, prefer structured parsing.\n\nSource evidence:\nSource: acme/widgets#7\n\nReviewer said:\nPlease prefer structured parsing.";
        let block = render_contract_block("pr_review", desc);
        assert!(block.starts_with("### Contract\n"));
        assert!(
            block.contains("- MUST: When touching `src/**/*.rs`, prefer structured parsing."),
            "got: {block}"
        );
    }

    #[test]
    fn contract_from_mined_avoid_rule_renders_avoid_bullet() {
        let desc = "Rule:\nWhen touching `src/http`, never unwrap request payloads.\n\nSource evidence:\nSource: acme/widgets#7";
        let block = render_contract_block("pr_review", desc);
        assert!(block.contains("- AVOID:"), "got: {block}");
    }

    #[test]
    fn contract_from_conversation_rule_splits_directive_and_rationale() {
        let desc = "Prefer dependency injection for clients. It makes the handler testable and avoids hidden globals.";
        let block = render_contract_block("conversation", desc);
        assert!(block.contains("- MUST: Prefer dependency injection for clients."));
        assert!(block.contains("Rationale: It makes the handler testable"));
    }

    #[test]
    fn contract_never_drops_body_when_unparseable() {
        // Mined origin but no `Rule:` section -> falls back to verbatim.
        let desc = "Some freeform mined note without the structured shape";
        let block = render_contract_block("pr_review", desc);
        assert!(block.contains("Some freeform mined note"), "got: {block}");
    }

    #[test]
    fn validation_matrix_row_from_example() {
        let ex = [example(
            "foo.unwrap()",
            "foo?",
            Some("reviewer asked for ?"),
        )];
        let matrix =
            render_validation_matrix("conversation", "irrelevant", Some(&ex)).expect("matrix");
        assert!(matrix.contains("| Condition | Expected | On violation |"));
        assert!(matrix.contains("foo.unwrap()"));
        assert!(matrix.contains("foo?"));
        assert!(matrix.contains("reviewer asked for ?"));
    }

    #[test]
    fn validation_matrix_row_from_when_directive() {
        let desc = "Rule:\nWhen touching `src/http`, return a structured error.\n\nSource evidence:\nSource: acme/widgets#7";
        let matrix = render_validation_matrix("pr_review", desc, None).expect("matrix");
        assert!(matrix.contains("When touching `src/http`"));
        assert!(matrix.contains("return a structured error"));
    }

    #[test]
    fn validation_matrix_omitted_when_no_source() {
        assert!(render_validation_matrix("conversation", "freeform prose", None).is_none());
    }

    #[test]
    fn matrix_cell_escapes_pipes_and_collapses_newlines() {
        assert_eq!(matrix_cell("a | b\nc"), "a \\| b c");
    }

    #[test]
    fn cases_block_uses_conformance_framing_not_bad_good() {
        let ex = [example("bad()", "good()", None)];
        let block = render_cases_block(Some(&ex)).expect("cases");
        assert!(block.contains("❌ Counter-example:"));
        assert!(block.contains("✅ Conforming:"));
        // Must NOT reuse the old index-leak markers.
        assert!(!block.contains("### Examples"));
    }

    #[test]
    fn cases_block_omitted_when_empty() {
        assert!(render_cases_block(None).is_none());
        let empty: [RuleExample; 0] = [];
        assert!(render_cases_block(Some(&empty)).is_none());
    }

    #[test]
    fn provenance_from_mined_rule_includes_source_and_reviewer() {
        let desc = "Rule:\nPrefer X.\n\nSource evidence:\nSource: acme/widgets#7\nComment: https://example.com/c/1\n\nReviewer said:\nPlease prefer X over Y.";
        let prov = render_provenance_block(desc, Some("acme/widgets")).expect("provenance");
        assert!(prov.contains("Source: acme/widgets#7"));
        assert!(prov.contains("https://example.com/c/1"));
        assert!(prov.contains("Reviewer: Please prefer X over Y."));
    }

    #[test]
    fn provenance_omitted_for_conversation_rule_without_proof() {
        // No source proof, no repo -> the header already says nothing to learn
        // from, so the section is dropped.
        assert!(render_provenance_block("freeform note", None).is_none());
    }

    #[test]
    fn golden_mined_rule_with_example() {
        let ex = [example(
            "let v = resolve(x).unwrap();",
            "let v = resolve(x)?;",
            Some("reviewer flagged unwrap"),
        )];
        let input = RuleRenderInput {
            id: "conv-foo-ab12",
            name: "Prefer structured parsing in resolve",
            r#type: "review_standard",
            confidence: 0.82,
            origin: "pr_review",
            source_repo: Some("vitejs/vite"),
            file_patterns: &["packages/vite/src/**/*.ts".to_owned()],
            description: "Rule:\nWhen touching `packages/vite/src`, never unwrap resolve results.\n\nSource evidence:\nSource: vitejs/vite#42\nComment: https://example.com/pr/42#c\nFile: resolve.ts\n\nReviewer said:\nPlease return a structured error.",
            trigger: None,
            check_prompt: None,
            examples: Some(&ex),
        };
        let body = render_code_spec(&input);
        assert!(body.starts_with("## Rule conv-foo-ab12 — Prefer structured parsing in resolve\n"));
        assert!(body.contains("Scope: packages/vite/src/**/*.ts"));
        assert!(body.contains("Confidence: 0.82"));
        assert!(body.contains("\u{2190} learned from vitejs/vite"));
        assert!(body.contains("### Contract"));
        assert!(
            body.contains(
                "- AVOID: When touching `packages/vite/src`, never unwrap resolve results."
            )
        );
        assert!(body.contains("### Validation / Error matrix"));
        assert!(body.contains("### Cases"));
        assert!(body.contains("❌ Counter-example:"));
        assert!(body.contains("### Provenance"));
        assert!(body.contains("Source: vitejs/vite#42"));
        // No trigger/self-check columns populated -> sections omitted.
        assert!(!body.contains("### Trigger"));
        assert!(!body.contains("### Self-check"));
    }

    #[test]
    fn golden_conversation_rule_with_neither_trigger_nor_example() {
        let input = RuleRenderInput {
            id: "conv-bare-1",
            name: "Keep handlers thin",
            r#type: "review_standard",
            confidence: 0.5,
            origin: "conversation",
            source_repo: None,
            file_patterns: &[],
            description: "Keep request handlers thin and push logic into services.",
            trigger: None,
            check_prompt: None,
            examples: None,
        };
        let body = render_code_spec(&input);
        assert!(body.contains("## Rule conv-bare-1 — Keep handlers thin"));
        assert!(body.contains("Scope: repo-wide (no file scope)"));
        assert!(body.contains("### Contract"));
        assert!(body.contains("- MUST: Keep request handlers thin"));
        // Slot omission: no matrix, trigger, self-check, cases, or provenance.
        assert!(!body.contains("### Validation / Error matrix"));
        assert!(!body.contains("### Trigger"));
        assert!(!body.contains("### Self-check"));
        assert!(!body.contains("### Cases"));
        assert!(!body.contains("### Provenance"));
    }

    #[test]
    fn golden_rule_with_check_prompt_only() {
        let input = RuleRenderInput {
            id: "team-rule-9",
            name: "Validate webhook signatures",
            r#type: "review_standard",
            confidence: 0.9,
            origin: "team",
            source_repo: None,
            file_patterns: &["src/webhooks/**/*.ts".to_owned()],
            description: "Always verify the HMAC signature before processing a webhook.",
            trigger: None,
            check_prompt: Some("Did you verify the signature before reading the body?"),
            examples: None,
        };
        let body = render_code_spec(&input);
        assert!(body.contains("### Self-check"));
        assert!(body.contains("Did you verify the signature before reading the body?"));
        assert!(!body.contains("### Trigger"));
        assert!(!body.contains("### Cases"));
    }
}
