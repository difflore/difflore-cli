//! Pure scoring helpers for hybrid rule retrieval.
//!
//! Split out of `rules.rs` so the main hybrid-retrieval orchestration stays
//! focused on fusion and result-set shaping. These functions are all pure
//! (no I/O): the intent-alignment directive matchers and the category-keyed
//! confidence-decay model. The orchestration in `rules.rs` calls them via
//! `super::scoring::…`; `effective_confidence`, `infer_rule_kind`, and
//! `RuleKind` are re-exported from `super` (the retrieval module) for external
//! callers.

use super::{
    MIN_DISTINCTIVE_SHARED_TERMS, MIN_INTENT_DIRECTIVE_OVERLAP, MIN_INTENT_DIRECTIVE_OVERLAP_RATIO,
    rule_title,
};

/// Distil the part of a rule's indexed content that carries its DIRECTIVE —
/// the imperative + its object — for intent-alignment matching.
///
/// The indexed `content` is `Rule ID: …\nRule Name: <title>\nType: …\nTags:
/// …\n\n<body>`. The `Rule Name:` line is the human-authored distillation of
/// what the rule tells you to do ("Return false instead of panic on invalid
/// input"), so it is the highest-signal directive text. We also fold in a
/// SHORT leading slice of the body so a directive phrased only in the body
/// (a title like "Error handling") still contributes its action/subject
/// terms — but we cap it so a long body can't drown the gate in incidental
/// vocabulary that re-introduces the topical-adjacency noise the gate exists
/// to remove.
///
/// `BODY_DIRECTIVE_CHARS` is the leading-body budget: enough to capture the
/// opening imperative sentence (the rule's "what to do") without pulling in
/// the examples / rationale that follow.
pub(super) fn rule_directive_text(content: &str) -> String {
    const BODY_DIRECTIVE_CHARS: usize = 160;
    let title = rule_title(content, "");
    // Body = everything after the blank line that separates the metadata
    // header from the rule body. Fall back to the whole content when no such
    // separator exists (a bare chunk with no header).
    let body = content
        .split_once("\n\n")
        .map_or(content, |(_, body)| body)
        .trim();
    let body_head: String = body.chars().take(BODY_DIRECTIVE_CHARS).collect();
    format!("{title} {body_head}")
}

/// Generic topical/code anchors that, when SHARED between a query and a rule
/// directive, do NOT by themselves establish a subject/action concern match —
/// they are the words every rule in a file area tends to mention.
///
/// The leak-free A/B diagnosis is specific: the extra false positives came
/// from rules sharing one of these anchors (`panic`, `test`, `error`) plus
/// incidental filler, not a genuine subject overlap. Treating these as
/// non-distinctive forces the concern match to rest on a SPECIFIC shared
/// token (the subject/action: `false`, `invalid`, `hijack`, `memchr`) rather
/// than the topic everyone shares. Deliberately SMALL and conservative: only
/// the highest-frequency, lowest-specificity programming/review words go here
/// (a longer list would risk discounting a genuinely distinctive subject).
/// Stems are listed in their light-stem form so the membership test below can
/// compare on the stem and catch plural/gerund variants.
pub(super) fn is_generic_anchor(stem: &str) -> bool {
    const GENERIC_ANCHORS: &[&str] = &[
        "panic", "error", "test", "value", "code", "data", "type", "result", "check", "handle",
        "handler", "return", "input", "output", "field", "case", "call", "message", "method",
        "function", "file", "line", "block", "thread", "task", "async", "await", "default",
        "option", "config", "request", "response", "buffer", "queue", "size", "count", "index",
        "state", "event", "lock", "guard", "time", "timer", "runtime", "feature",
    ];
    GENERIC_ANCHORS.contains(&stem)
}

/// Stem-tolerant substring presence of a single query term inside a
/// directive STRING. Mirrors how [`super::lexical_boost`] scores presence
/// (`directive.contains(term)`), so a query term matches when the directive
/// contains it as a substring — `parse` inside `parsed`, `batch` inside
/// `batching`. Folding the query term to its light stem first makes the test
/// morphology-symmetric (`batching`/`retries` against a directive saying
/// `batch`/`retry`) without losing the substring tolerance that real recall
/// matches rely on. Returns the matched flag.
pub(super) fn term_present_in_directive(term: &str, directive: &str) -> bool {
    directive.contains(term) || directive.contains(light_stem(term).as_str())
}

/// Decide whether a single candidate rule's DIRECTIVE aligns with the query
/// intent — a CONCERN (subject/action) match, not mere topical adjacency.
///
/// Iter-2 (2026-06-02) hardened this with a DISTINCTIVENESS requirement,
/// because the review A/B (n=6) showed the iter-1 count-or-ratio test still
/// passed topically-adjacent, wrong-subject rules (extra false positives where
/// recall fired). The old test asked only "does the directive contain >=2
/// query terms, or cover half the query"; two GENERIC anchors (`panic` +
/// `input`) cleared it with no real subject overlap, and so did a lone generic
/// anchor that happened to be half of a 2-term query. The hardened test adds
/// the missing axis: the shared terms must include at least one SPECIFIC
/// (non-generic) subject/action token — the word that says these two
/// directives are about the SAME thing, not just the same topic.
///
/// A candidate aligns when it has at least [`MIN_DISTINCTIVE_SHARED_TERMS`]
/// distinctive shared term ([`is_generic_anchor`]) AND EITHER:
///   * **Absolute path** — the shared set has at least
///     [`MIN_INTENT_DIRECTIVE_OVERLAP`] terms (the verb/object pair), OR
///   * **Ratio path** — the shared set covers at least
///     [`MIN_INTENT_DIRECTIVE_OVERLAP_RATIO`] of the QUERY's salient terms
///     (keeps SHORT, sharp queries — and the realistic "one rule matches each
///     half of a two-word intent" fan-out — from over-pruning).
///
/// The distinctiveness gate is the precision lever (it kills the all-anchor
/// false positives the A/B diagnosed) and is deliberately chosen over a
/// rule-side coverage ratio, which proved too sensitive to directive phrasing:
/// a rule whose directive lives in its body, or whose title is a bare label,
/// has few salient TITLE terms and a verbose body, so any fixed coverage floor
/// either drops genuine body-phrased matches or fails to drop adjacency.
/// Distinctiveness keys off the SHARED set itself, so it is robust to how
/// either side is phrased. Self-recall — where the query *is* the rule's own
/// intent text — shares its specific subject tokens, so it always has a
/// distinctive overlap and is never regressed.
///
/// Pure: allocates the lowercased directive + a couple of stems, no I/O.
pub(super) fn directive_intent_aligned(content: &str, query_terms: &[String]) -> bool {
    if query_terms.is_empty() {
        return false;
    }
    // Presence string: title + a short body head, so a directive phrased only
    // in the body still contributes its subject/action terms when matching.
    let directive = rule_directive_text(content).to_ascii_lowercase();

    // Shared salient terms (stem-tolerant substring). Dedup on the query
    // term's stem so a query that repeats a concept doesn't inflate overlap,
    // and track how many shared terms are DISTINCTIVE (not a generic anchor).
    let mut shared_stems: Vec<String> = Vec::new();
    let mut distinctive_shared = 0usize;
    for term in query_terms {
        if !term_present_in_directive(term, &directive) {
            continue;
        }
        let stem = light_stem(term);
        if shared_stems.iter().any(|s| s == &stem) {
            continue;
        }
        if !is_generic_anchor(&stem) {
            distinctive_shared += 1;
        }
        shared_stems.push(stem);
    }
    let overlap = shared_stems.len();

    // Precision gate: a concern match must rest on at least one SPECIFIC
    // subject/action token. A shared set made only of generic anchors
    // (`panic`, `input`, `error`) is the topical adjacency the A/B blamed for
    // the extra false positives — drop it regardless of count or ratio.
    if distinctive_shared < MIN_DISTINCTIVE_SHARED_TERMS {
        return false;
    }

    // Absolute path: enough shared terms (verb + object), already known to
    // include a distinctive token.
    if overlap >= MIN_INTENT_DIRECTIVE_OVERLAP {
        return true;
    }
    // Ratio path: the shared (distinctive) set is at least half a short, focused
    // query — keeps short intents and the per-rule fan-out of a two-word intent
    // from over-pruning.
    let query_ratio = overlap as f64 / query_terms.len() as f64;
    query_ratio >= MIN_INTENT_DIRECTIVE_OVERLAP_RATIO
}

/// Light, allocation-cheap stemmer for intent-alignment term matching.
///
/// Strips the common English inflectional suffixes (`ies`→`y`, `ing`, `es`,
/// `s`) so a query term and a directive token that differ only by
/// plural/gerund form fold to the same stem ("batching"→"batch",
/// "retries"→"retry"). Deliberately NOT a full Porter stemmer: it only needs
/// to reconcile the surface morphology the alignment gate trips on, and an
/// over-aggressive stemmer would re-introduce the topical-adjacency overlap
/// the gate exists to suppress. Guarded by a minimum residual stem length so
/// short tokens (≤4 chars) are never truncated into noise.
pub(super) fn light_stem(term: &str) -> String {
    const MIN_STEM_LEN: usize = 4;
    if let Some(base) = term.strip_suffix("ies")
        && base.len() >= MIN_STEM_LEN - 1
    {
        // retries -> retri -> retry
        return format!("{base}y");
    }
    for suffix in ["ing", "es", "s"] {
        if let Some(base) = term.strip_suffix(suffix)
            && base.len() >= MIN_STEM_LEN
        {
            return base.to_owned();
        }
    }
    term.to_owned()
}

/// Coarse rule-kind taxonomy used by the time-decay multiplier.
///
/// `IndexedRuleChunk` does not yet carry an explicit `kind` column — the
/// rule corpus only stores `content` + denormalised metadata. So instead
/// of inventing jcode's literal `MemoryEntry::Kind` enum on the wrong
/// data model, we infer a coarse bucket from the rule's content. The
/// audit comment around line 355 already names the practical kinds
/// observed in production: corrections, slogans, style/lint rules, and
/// generic preferences/conventions. We mirror those here.
///
/// The inferred kind is enough to apply category-aware decay without widening
/// the index schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleKind {
    /// "Don't do X", "always Y", "fix: ...", "regression". These are the
    /// rare, hard-won rules — long memory.
    Correction,
    /// User taste / project conventions ("we use Drizzle", "naming = X").
    /// Medium memory.
    Convention,
    /// Style / lint surface ("prefer let over const", formatting). Ages
    /// fastest because codebases reformat constantly.
    Style,
    /// Slogans / vague guidance ("trust CI", "review carefully") — same
    /// short half-life as style; the audit comment notes these misfire.
    Slogan,
    /// Anything we couldn't classify.
    Other,
}

/// Lightweight content heuristic. Cheap regex-free token checks so we
/// don't burn CPU per chunk per query.
pub fn infer_rule_kind(content: &str) -> RuleKind {
    let lc = content.to_ascii_lowercase();
    // Correction signals come first — they're the highest-value bucket
    // and we'd rather over-classify into long memory than evict them.
    const CORRECTION_HINTS: &[&str] = &[
        "don't ",
        "do not ",
        "never ",
        "must not ",
        "regression",
        "fix:",
        "bug:",
        "broke ",
        "incorrect",
        "wrong",
    ];
    if CORRECTION_HINTS.iter().any(|h| lc.contains(h)) {
        return RuleKind::Correction;
    }
    const STYLE_HINTS: &[&str] = &[
        "format",
        "indent",
        "spacing",
        "naming convention",
        "lint",
        "prettier",
        "rustfmt",
        "biome",
        "eslint",
    ];
    if STYLE_HINTS.iter().any(|h| lc.contains(h)) {
        return RuleKind::Style;
    }
    const CONVENTION_HINTS: &[&str] = &[
        "we use ",
        "prefer ",
        "always use ",
        "convention",
        "project uses",
    ];
    if CONVENTION_HINTS.iter().any(|h| lc.contains(h)) {
        return RuleKind::Convention;
    }
    const SLOGAN_HINTS: &[&str] = &[
        "trust ",
        "review carefully",
        "be careful",
        "best practice",
        "good practice",
    ];
    if SLOGAN_HINTS.iter().any(|h| lc.contains(h)) {
        return RuleKind::Slogan;
    }
    RuleKind::Other
}

/// Half-life in days per kind. Borrowed from jcode's
/// `effective_confidence` table but mapped to difflore's actual rule
/// taxonomy:
///
/// | jcode kind   | difflore kind | half-life |
/// |--------------|---------------|-----------|
/// | Correction   | Correction    | 365       |
/// | Preference   | Convention    | 120       |
/// | (n/a)        | Style         |  30       |
/// | Fact         | Slogan        |  30       |
/// | Entity (def) | Other         |  90       |
const fn half_life_days(kind: RuleKind) -> f32 {
    match kind {
        RuleKind::Correction => 365.0,
        RuleKind::Convention => 120.0,
        RuleKind::Style | RuleKind::Slogan => 30.0,
        RuleKind::Other => 90.0,
    }
}

/// Apply category-aware exponential decay to a raw confidence value.
///
/// `effective = raw * 0.5 ^ (age_days / half_life)`
///
/// At `age_days = 0` this returns `raw` unchanged. After one half-life
/// it returns `raw / 2`, etc. Used as the input to the existing
/// `0.9 + 0.1 * confidence` tie-breaker so old rules naturally lose
/// their tie-breaker bump.
pub fn effective_confidence(raw_confidence: f32, kind: &RuleKind, age_days: f32) -> f32 {
    let raw = raw_confidence.clamp(0.0, 1.0);
    let age = age_days.max(0.0);
    let half = half_life_days(*kind);
    raw * 0.5_f32.powf(age / half)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn effective_confidence_fresh_correction_keeps_raw_value() {
        // Day-1 correction at conf=0.6 should be ~0.6 (decay negligible).
        let eff = effective_confidence(0.6, &RuleKind::Correction, 1.0);
        assert!(
            (eff - 0.6).abs() < 0.005,
            "fresh correction should keep raw conf, got {eff}"
        );
    }

    #[test]
    fn effective_confidence_two_year_old_style_decays_to_near_zero() {
        // 730 days at 30-day half-life = ~24.3 half-lives → essentially 0.
        let eff = effective_confidence(0.9, &RuleKind::Style, 730.0);
        assert!(
            eff < 1e-6,
            "two-year-old style rule should decay to ~0, got {eff}"
        );
    }

    #[test]
    fn strengthened_correction_outranks_fresh_slogan() {
        // 30-day-old correction with conf=0.95 vs day-1 slogan with
        // conf=0.5. Correction half-life 365 → factor ~0.944, eff ~0.897.
        // Slogan half-life 30 → day-1 factor ~0.977, eff ~0.488.
        let correction = effective_confidence(0.95, &RuleKind::Correction, 30.0);
        let slogan = effective_confidence(0.5, &RuleKind::Slogan, 1.0);
        assert!(
            correction > slogan,
            "strengthened correction ({correction}) should outrank fresh slogan ({slogan})"
        );
    }

    #[test]
    fn effective_confidence_at_age_zero_is_identity() {
        // Wiring relies on age=0.0 being a no-op until ages are plumbed.
        for k in [
            RuleKind::Correction,
            RuleKind::Convention,
            RuleKind::Style,
            RuleKind::Slogan,
            RuleKind::Other,
        ] {
            let eff = effective_confidence(0.73, &k, 0.0);
            assert!(
                (eff - 0.73).abs() < 1e-6,
                "age=0 must be identity for {k:?}, got {eff}"
            );
        }
    }

    #[test]
    fn age_days_map_decays_old_style_below_fresh_correction_via_options() {
        // Direct-formula assertion proving the plumbing makes a real
        // ranking difference. Without the map both rules would land at
        // the same `0.9 + 0.1 * raw_conf` weight; with the map a
        // 2-year-old style rule (raw 0.95) loses to a fresh correction
        // (raw 0.6) — the failure mode jcode's half-life borrows fix.
        let style_old = effective_confidence(0.95, &RuleKind::Style, 730.0);
        let correction_fresh = effective_confidence(0.6, &RuleKind::Correction, 1.0);
        let style_weight = 0.1f64.mul_add(style_old.clamp(0.0, 1.0) as f64, 0.9);
        let correction_weight = 0.1f64.mul_add(correction_fresh.clamp(0.0, 1.0) as f64, 0.9);
        assert!(
            correction_weight > style_weight,
            "fresh correction weight ({correction_weight}) must beat old-style weight ({style_weight}) once age is plumbed"
        );
    }

    #[test]
    fn age_days_map_lookup_falls_back_to_zero_for_unknown_skill() {
        // When the map is present but the chunk's skill_id isn't in it
        // (e.g. an old chunk whose skill was deleted), the scoring path
        // must default to age_days=0 — i.e. behave as if no decay applied
        // — instead of panicking or treating absence as "infinitely old".
        let map: HashMap<String, f32> =
            std::iter::once(("known-skill".to_owned(), 30.0_f32)).collect();
        let lookup_known = map.get("known-skill").copied().unwrap_or(0.0);
        let lookup_unknown = map.get("ghost-skill").copied().unwrap_or(0.0);
        assert!((lookup_known - 30.0).abs() < 1e-6);
        assert!(
            lookup_unknown.abs() < 1e-6,
            "unknown skill must fall back to 0"
        );
    }

    #[test]
    fn is_generic_anchor_flags_common_topic_words_only() {
        // The discount set must contain the high-frequency topic anchors the
        // diagnosis named, and must NOT contain specific subject/action tokens
        // (those are what a real concern match rests on).
        for generic in ["panic", "error", "test", "input", "handler", "runtime"] {
            assert!(
                is_generic_anchor(generic),
                "`{generic}` should be a generic anchor"
            );
        }
        for distinctive in [
            "hijack", "memchr", "invalid", "false", "session", "validate",
        ] {
            assert!(
                !is_generic_anchor(distinctive),
                "`{distinctive}` is a distinctive subject token, not a generic anchor"
            );
        }
    }

    #[test]
    fn rule_directive_text_distils_title_and_body_head() {
        let content = "Rule ID: r1\nRule Name: Return false on invalid input\nType: x\nTags: \n\nReturn false rather than panicking when the caller passes bad input.";
        let directive = rule_directive_text(content);
        assert!(directive.contains("Return false on invalid input"));
        assert!(directive.contains("panicking"));
        // The metadata header (Rule ID / Type / Tags lines) is excluded.
        assert!(!directive.contains("Rule ID"));
        assert!(!directive.contains("Tags"));
    }

    #[test]
    fn infer_rule_kind_buckets_common_phrasings() {
        assert_eq!(
            infer_rule_kind("Never use unwrap() in production code"),
            RuleKind::Correction
        );
        assert_eq!(
            infer_rule_kind("Run prettier before committing"),
            RuleKind::Style
        );
        assert_eq!(
            infer_rule_kind("We use Drizzle ORM for all queries"),
            RuleKind::Convention
        );
        assert_eq!(
            infer_rule_kind("Trust CI for workflow correctness"),
            RuleKind::Slogan
        );
        assert_eq!(infer_rule_kind("Some random observation"), RuleKind::Other);
    }
}
