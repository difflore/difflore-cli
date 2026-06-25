//! Deterministic serve-layer rule arbitration (VOC roadmap item ③b).
//!
//! Sits AFTER retrieval/fusion/rerank at the two serve exits (the post-edit
//! hook and the `search_rules` MCP tool) and never reaches into RRF fusion
//! internals. It imposes a composite, fully deterministic ordering:
//!
//!   1. relative-score quantisation band, `floor((score / top_score) * 10)`
//!      (desc) — scores are only comparable within a 10% band, so source
//!      priority may flip *near-ties* but never a clear relevance winner;
//!   2. path-hint match (desc) — explicit file evidence breaks close ties
//!      without letting stale globs override relevance;
//!   3. source rank (asc) — founder ruling 2026-06: `manual(0)` >
//!      `cloud`/`team`(1) > `pr_review`(2) > `extracted`(3) >
//!      `conversation`(4); unknown origins last;
//!   4. confidence (desc);
//!   5. `skill_id` (asc) — total order, so equal candidates can never
//!      shuffle between runs.
//!
//! Rollback: setting `DIFFLORE_DISABLE_SOURCE_PRIORITY` (env, truthy) makes
//! the callers pass `source_priority_disabled = true`, which leaves the
//! incoming (pre-change) order untouched. The `why` facts are still computed
//! in that mode — they describe the candidate, not the ordering.

use std::collections::{HashMap, HashSet};

use super::ScoredRuleChunk;

/// Number of relative-score quantisation bands (band width = 10%).
pub const ARBITRATION_BANDS: u8 = 10;

/// Source rank dropped to unknown / unmapped origins. Strictly below every
/// ratified origin so an unexpected value can never outrank known provenance.
pub const UNKNOWN_SOURCE_RANK: u8 = 5;

/// Map a skill `origin` to its arbitration priority (lower wins).
///
/// Founder ruling (voc-roadmap-2026-06 §仲裁): personal overrides win over
/// governance — `manual` beats `cloud`/`team`, which beat the extracted
/// lanes. `cloud` and `team` share one rank: both are the team-ratified lane,
/// split only by sync mechanics.
#[must_use]
pub fn source_rank(origin: &str) -> u8 {
    match origin {
        "manual" => 0,
        "cloud" | "team" => 1,
        "pr_review" => 2,
        "extracted" => 3,
        "conversation" => 4,
        _ => UNKNOWN_SOURCE_RANK,
    }
}

/// Quantise `score` relative to `top_score` into 10% bands:
/// `floor((score / top_score) * 10)`, clamped to `[0, 10]`.
///
/// Within one band two candidates are a "near tie" and the source priority is
/// allowed to flip them; across bands the higher band always wins. Note the
/// exact-top candidate lands in band 10 alone unless another candidate has the
/// identical score — source priority never demotes a strict relevance leader.
///
/// Degenerate inputs (`top_score <= 0`, non-finite ratio) collapse to band 0
/// so arbitration falls through to the later key components instead of
/// panicking or producing NaN ordering.
#[must_use]
pub fn relative_score_band(score: f64, top_score: f64) -> u8 {
    if top_score <= 0.0 || !top_score.is_finite() || !score.is_finite() {
        return 0;
    }
    let ratio = (score / top_score).clamp(0.0, 1.0);
    // ratio ∈ [0, 1] → band ∈ [0, 10]; f64→u8 cast is safe after the clamp.
    (ratio * f64::from(ARBITRATION_BANDS)).floor() as u8
}

/// The facts the arbitration key was computed from, kept per rule so the
/// `whyRanked` surfaces (MCP evidence `why` field, hook header `why:` segment)
/// render exactly what the sort saw — no second derivation that could drift.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleRankingWhy {
    /// The rule's path hints matched the target file.
    pub strict_hit: bool,
    /// Relative-score band, `0..=10` (see [`relative_score_band`]).
    pub band: u8,
    /// Skill `origin` as stored (`manual` / `team` / …). `None` when the
    /// caller had no metadata row for the rule (e.g. cross-repo starter).
    pub source: Option<String>,
}

impl RuleRankingWhy {
    /// Compact, agent-facing explanation, ~5–10 estimated tokens per rule:
    /// `path-hint; band 9/10; source manual` (the `path-hint;` prefix and
    /// the `source` segment are omitted when not applicable).
    #[must_use]
    pub fn compact(&self) -> String {
        let mut out = String::with_capacity(40);
        if self.strict_hit {
            out.push_str("path-hint; ");
        }
        out.push_str(&format!("band {}/{}", self.band, ARBITRATION_BANDS));
        if let Some(source) = &self.source {
            out.push_str("; source ");
            out.push_str(source);
        }
        out
    }
}

/// Per-candidate composite key. Field order mirrors the documented sort
/// precedence; `Ord` is implemented manually because half the components sort
/// descending.
struct ArbitrationKey {
    strict_hit: bool,
    band: u8,
    source_rank: u8,
    confidence: f64,
}

fn compare_keys(
    a: &ArbitrationKey,
    a_skill_id: &str,
    b: &ArbitrationKey,
    b_skill_id: &str,
) -> std::cmp::Ordering {
    // ① band desc ② path hint desc ③ source rank asc ④ confidence desc
    // ⑤ skill_id asc — a total order over distinct skill_ids.
    b.band
        .cmp(&a.band)
        .then_with(|| b.strict_hit.cmp(&a.strict_hit))
        .then_with(|| a.source_rank.cmp(&b.source_rank))
        .then_with(|| b.confidence.total_cmp(&a.confidence))
        .then_with(|| a_skill_id.cmp(b_skill_id))
}

/// Arbitrate the final serve order of an already-retrieved, already-reranked
/// candidate list. Pure: no I/O, no env reads — the caller resolves the
/// rollback flag (`DIFFLORE_DISABLE_SOURCE_PRIORITY`) into
/// `source_priority_disabled`.
///
/// * `strict_skill_ids` — rules whose path hints matched the target scope.
/// * `origin_by_skill_id` — skill `origin` per id, from metadata the caller
///   already fetched (zero additional queries by contract). Missing ids get
///   [`UNKNOWN_SOURCE_RANK`] and `source: None` in the why facts.
/// * `source_priority_disabled` — rollback switch: when `true` the incoming
///   order is left untouched (exact pre-arbitration behaviour) and only the
///   why facts are returned.
///
/// Returns the per-rule [`RuleRankingWhy`] facts keyed by `skill_id`, computed
/// from the same inputs the sort used. Bands are relative to the incoming
/// list's max score, which is order-independent.
#[allow(clippy::implicit_hasher)] // reason: stable public API; `HashMap/HashSet` (default hasher) is what every caller passes.
pub fn arbitrate_rule_order(
    scored: &mut [ScoredRuleChunk],
    strict_skill_ids: &HashSet<String>,
    origin_by_skill_id: &HashMap<String, String>,
    source_priority_disabled: bool,
) -> HashMap<String, RuleRankingWhy> {
    let top_score = scored
        .iter()
        .map(|chunk| chunk.score)
        .fold(f64::NEG_INFINITY, f64::max);

    let mut whys = HashMap::with_capacity(scored.len());
    let mut keys: HashMap<String, ArbitrationKey> = HashMap::with_capacity(scored.len());
    for chunk in scored.iter() {
        let strict_hit = strict_skill_ids.contains(&chunk.skill_id);
        let band = relative_score_band(chunk.score, top_score);
        let source = origin_by_skill_id.get(&chunk.skill_id);
        keys.insert(
            chunk.skill_id.clone(),
            ArbitrationKey {
                strict_hit,
                band,
                source_rank: source.map_or(UNKNOWN_SOURCE_RANK, |origin| source_rank(origin)),
                confidence: chunk.confidence,
            },
        );
        whys.insert(
            chunk.skill_id.clone(),
            RuleRankingWhy {
                strict_hit,
                band,
                source: source.cloned(),
            },
        );
    }

    if !source_priority_disabled {
        scored.sort_by(|a, b| {
            // Both lookups are guaranteed present (inserted above); fall back
            // to skill_id ordering defensively rather than panicking.
            match (keys.get(&a.skill_id), keys.get(&b.skill_id)) {
                (Some(ka), Some(kb)) => compare_keys(ka, &a.skill_id, kb, &b.skill_id),
                _ => a.skill_id.cmp(&b.skill_id),
            }
        });
    }

    whys
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(id: &str, score: f64, confidence: f64) -> ScoredRuleChunk {
        ScoredRuleChunk {
            skill_id: id.to_owned(),
            content: format!("Rule ID: {id}\nRule Name: {id}\n\nbody"),
            score,
            confidence,
        }
    }

    fn origins(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(id, origin)| ((*id).to_owned(), (*origin).to_owned()))
            .collect()
    }

    fn ids(scored: &[ScoredRuleChunk]) -> Vec<&str> {
        scored.iter().map(|c| c.skill_id.as_str()).collect()
    }

    #[test]
    fn source_rank_follows_founder_ruling() {
        assert_eq!(source_rank("manual"), 0);
        assert_eq!(source_rank("cloud"), 1);
        assert_eq!(source_rank("team"), 1);
        assert_eq!(source_rank("pr_review"), 2);
        assert_eq!(source_rank("extracted"), 3);
        assert_eq!(source_rank("conversation"), 4);
        // Unknown origins must never outrank ratified ones.
        assert_eq!(source_rank("unknown_source"), UNKNOWN_SOURCE_RANK);
        assert_eq!(source_rank(""), UNKNOWN_SOURCE_RANK);
    }

    #[test]
    fn relative_score_band_boundaries() {
        // Exact top → band 10, alone unless scores are identical.
        assert_eq!(relative_score_band(1.0, 1.0), 10);
        // Just inside the 90% band.
        assert_eq!(relative_score_band(0.999, 1.0), 9);
        assert_eq!(relative_score_band(0.90, 1.0), 9);
        // Just below the 90% boundary falls to band 8.
        assert_eq!(relative_score_band(0.8999, 1.0), 8);
        assert_eq!(relative_score_band(0.10, 1.0), 1);
        assert_eq!(relative_score_band(0.0999, 1.0), 0);
        assert_eq!(relative_score_band(0.0, 1.0), 0);
        // Scale invariance: bands depend on the ratio, not absolute scores.
        assert_eq!(relative_score_band(0.0095, 0.01), 9);
    }

    #[test]
    fn relative_score_band_degenerate_inputs_collapse_to_zero() {
        assert_eq!(relative_score_band(0.5, 0.0), 0);
        assert_eq!(relative_score_band(0.5, -1.0), 0);
        assert_eq!(relative_score_band(f64::NAN, 1.0), 0);
        assert_eq!(relative_score_band(0.5, f64::NAN), 0);
        assert_eq!(relative_score_band(0.5, f64::INFINITY), 0);
        // Negative scores clamp into band 0 rather than underflowing.
        assert_eq!(relative_score_band(-0.5, 1.0), 0);
    }

    #[test]
    fn source_priority_flips_only_near_ties_inside_one_band() {
        // `team-rule` and `manual-rule` are within 10% of each other (same
        // band 9..=10? — both quantise to the same band), so the manual rule
        // wins despite the slightly lower score. `conversation-leader` is the
        // top score: band 10, untouchable.
        let mut scored = vec![
            chunk("conversation-leader", 1.00, 0.7),
            chunk("team-rule", 0.92, 0.7),
            chunk("manual-rule", 0.91, 0.7),
        ];
        let origin_map = origins(&[
            ("conversation-leader", "conversation"),
            ("team-rule", "team"),
            ("manual-rule", "manual"),
        ]);
        let whys = arbitrate_rule_order(&mut scored, &HashSet::new(), &origin_map, false);

        assert_eq!(
            ids(&scored),
            vec!["conversation-leader", "manual-rule", "team-rule"],
            "manual flips team inside the shared band; the band-10 leader is untouched"
        );
        assert_eq!(whys["manual-rule"].band, 9);
        assert_eq!(whys["team-rule"].band, 9);
        assert_eq!(whys["conversation-leader"].band, 10);
    }

    #[test]
    fn source_priority_cannot_flip_across_bands() {
        // The manual rule is far below the conversation rule (band 5 vs 10):
        // source priority must NOT promote it.
        let mut scored = vec![
            chunk("conversation-strong", 1.0, 0.7),
            chunk("manual-weak", 0.55, 0.9),
        ];
        let origin_map = origins(&[
            ("conversation-strong", "conversation"),
            ("manual-weak", "manual"),
        ]);
        arbitrate_rule_order(&mut scored, &HashSet::new(), &origin_map, false);
        assert_eq!(ids(&scored), vec!["conversation-strong", "manual-weak"]);
    }

    #[test]
    fn path_hint_breaks_ties_inside_one_band_only() {
        let mut scored = vec![
            chunk("manual-universal", 1.0, 0.9),
            chunk("conversation-path-hint", 0.92, 0.3),
        ];
        let origin_map = origins(&[
            ("manual-universal", "manual"),
            ("conversation-path-hint", "conversation"),
        ]);
        let strict: HashSet<String> = ["conversation-path-hint".to_owned()].into();
        let whys = arbitrate_rule_order(&mut scored, &strict, &origin_map, false);
        assert_eq!(
            ids(&scored),
            vec!["manual-universal", "conversation-path-hint"],
            "the band-10 relevance leader must stay ahead of a lower-band path hint"
        );
        assert!(whys["conversation-path-hint"].strict_hit);
        assert!(!whys["manual-universal"].strict_hit);

        let mut same_band = vec![
            chunk("conversation-leader", 1.0, 0.7),
            chunk("manual-universal", 0.92, 0.9),
            chunk("conversation-path-hint", 0.91, 0.3),
        ];
        let origin_map = origins(&[
            ("conversation-leader", "conversation"),
            ("manual-universal", "manual"),
            ("conversation-path-hint", "conversation"),
        ]);
        let whys = arbitrate_rule_order(&mut same_band, &strict, &origin_map, false);
        assert_eq!(
            ids(&same_band),
            vec![
                "conversation-leader",
                "conversation-path-hint",
                "manual-universal"
            ],
            "path hints may break ties inside the same relevance band"
        );
        assert_eq!(
            whys["conversation-path-hint"].band,
            whys["manual-universal"].band
        );
    }

    #[test]
    fn equal_band_and_source_tie_breaks_by_confidence_then_skill_id() {
        let mut scored = vec![
            chunk("b-low-conf", 1.0, 0.5),
            chunk("c-high-conf", 1.0, 0.9),
            chunk("a-high-conf", 1.0, 0.9),
        ];
        let origin_map = origins(&[
            ("b-low-conf", "team"),
            ("c-high-conf", "team"),
            ("a-high-conf", "team"),
        ]);
        arbitrate_rule_order(&mut scored, &HashSet::new(), &origin_map, false);
        assert_eq!(
            ids(&scored),
            vec!["a-high-conf", "c-high-conf", "b-low-conf"],
            "confidence desc, then skill_id asc — a total order"
        );
    }

    #[test]
    fn unknown_origin_ranks_below_every_known_source() {
        let mut scored = vec![
            chunk("unknown-rule", 1.0, 0.9),
            chunk("conversation-rule", 1.0, 0.5),
        ];
        let origin_map = origins(&[
            ("unknown-rule", "unknown_source"),
            ("conversation-rule", "conversation"),
        ]);
        arbitrate_rule_order(&mut scored, &HashSet::new(), &origin_map, false);
        assert_eq!(ids(&scored), vec!["conversation-rule", "unknown-rule"]);
    }

    #[test]
    fn missing_metadata_gets_unknown_rank_and_no_source_fact() {
        let mut scored = vec![chunk("no-meta", 1.0, 0.7), chunk("manual-rule", 1.0, 0.7)];
        let origin_map = origins(&[("manual-rule", "manual")]);
        let whys = arbitrate_rule_order(&mut scored, &HashSet::new(), &origin_map, false);
        assert_eq!(ids(&scored), vec!["manual-rule", "no-meta"]);
        assert_eq!(whys["no-meta"].source, None);
        assert_eq!(whys["manual-rule"].source.as_deref(), Some("manual"));
    }

    #[test]
    fn rollback_switch_preserves_incoming_order_but_still_reports_why() {
        // A near-tie the active arbitration WOULD flip (same band, manual
        // beats team): with the rollback flag the incoming (legacy) order
        // must be byte-identical.
        let mut scored = vec![
            chunk("conversation-leader", 1.00, 0.7),
            chunk("team-rule", 0.92, 0.7),
            chunk("manual-rule", 0.91, 0.7),
        ];
        let origin_map = origins(&[
            ("conversation-leader", "conversation"),
            ("team-rule", "team"),
            ("manual-rule", "manual"),
        ]);
        let whys = arbitrate_rule_order(&mut scored, &HashSet::new(), &origin_map, true);
        assert_eq!(
            ids(&scored),
            vec!["conversation-leader", "team-rule", "manual-rule"],
            "rollback must leave the pre-arbitration order untouched"
        );
        // Why facts stay available: they describe the candidate, not the sort.
        assert_eq!(whys["manual-rule"].source.as_deref(), Some("manual"));
        assert_eq!(whys["team-rule"].band, 9);
        assert_eq!(whys["manual-rule"].band, 9);
        assert_eq!(whys["conversation-leader"].band, 10);
    }

    #[test]
    fn arbitration_is_deterministic_across_input_permutations() {
        let build = |order: &[&str]| -> Vec<ScoredRuleChunk> {
            order
                .iter()
                .map(|id| match *id {
                    "m" => chunk("m", 0.95, 0.8),
                    "t" => chunk("t", 0.97, 0.8),
                    "c" => chunk("c", 1.0, 0.8),
                    other => chunk(other, 0.5, 0.5),
                })
                .collect()
        };
        let origin_map = origins(&[("m", "manual"), ("t", "team"), ("c", "conversation")]);
        let strict = HashSet::new();
        let mut a = build(&["m", "t", "c"]);
        let mut b = build(&["c", "m", "t"]);
        let mut c = build(&["t", "c", "m"]);
        arbitrate_rule_order(&mut a, &strict, &origin_map, false);
        arbitrate_rule_order(&mut b, &strict, &origin_map, false);
        arbitrate_rule_order(&mut c, &strict, &origin_map, false);
        assert_eq!(ids(&a), ids(&b));
        assert_eq!(ids(&b), ids(&c));
    }

    #[test]
    fn why_compact_renders_the_agreed_grammar() {
        let strict_manual = RuleRankingWhy {
            strict_hit: true,
            band: 9,
            source: Some("manual".to_owned()),
        };
        assert_eq!(
            strict_manual.compact(),
            "path-hint; band 9/10; source manual"
        );

        let plain = RuleRankingWhy {
            strict_hit: false,
            band: 10,
            source: Some("pr_review".to_owned()),
        };
        assert_eq!(plain.compact(), "band 10/10; source pr_review");

        let no_meta = RuleRankingWhy {
            strict_hit: false,
            band: 3,
            source: None,
        };
        assert_eq!(no_meta.compact(), "band 3/10");
    }

    #[test]
    fn why_compact_stays_within_the_serve_token_budget() {
        // whyRanked costs ~5–10 estimated tokens per rule (chars / 4, the
        // shared MCP estimate). Keep the worst-case grammar bounded so the
        // hook's 1500-token budget accounting stays predictable.
        let worst = RuleRankingWhy {
            strict_hit: true,
            band: 10,
            source: Some("conversation".to_owned()),
        };
        let rendered = worst.compact();
        assert!(
            rendered.len() / 4 <= 12,
            "why segment must stay within ~5–12 estimated tokens, got {} chars: {rendered}",
            rendered.len()
        );
    }
}
