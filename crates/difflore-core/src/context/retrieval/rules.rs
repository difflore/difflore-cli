use crate::context::DEFAULT_TOP_K_RULES;
use crate::context::ann;
use crate::context::embedding::cosine_similarity;
use crate::context::index_db::{self, IndexedRuleChunk, QueryFilter};
use crate::domain::glob_match::{GlobErrorPolicy, glob_match};
use crate::errors::CoreError;
use crate::review_trajectory::{TrajectoryBuilder, TrajectoryStep};
use sqlx::SqlitePool;
use std::collections::{HashMap, HashSet};
use std::time::Duration;

use super::query_embed::embed_query_aligned_to_index;
use super::scoring::{directive_intent_aligned, effective_confidence, infer_rule_kind};
use super::{
    ADAPTIVE_INJECT_THRESHOLD, EXPLICIT_RECALL_MIN_RELEVANCE, EXPLICIT_RECALL_RELATIVE_FLOOR,
    INTENT_ALIGNMENT_EXEMPT_SCORE, MIN_RELEVANCE_SCORE, RELATIVE_RELEVANCE_FLOOR, RRF_K,
    ScoredRuleChunk, concreteness_score, lexical_terms,
};

const MAX_RULE_RETRIEVAL_TOP_K: usize = 50;
const MAX_ANN_CANDIDATES: usize = 150;

/// Retrieve rules with confidence-weighted ranking.
/// Final score = hybrid rank score with one final confidence tie-breaker.
/// Rules with confidence < 0.2 are excluded (likely rejected).
pub async fn retrieve_rules(
    index_pool: &SqlitePool,
    query: &str,
    top_k: Option<usize>,
) -> Result<Vec<ScoredRuleChunk>, CoreError> {
    retrieve_rules_with_confidence(
        index_pool,
        query,
        RetrievalOptions {
            top_k,
            ..Default::default()
        },
    )
    .await
}

/// Decide whether a chunk's `file_patterns` (JSON-encoded glob list) match
/// the given target file path. Returns `true` if patterns are absent / empty
/// (universal rule), or if any glob matches. Malformed JSON or an unbuildable
/// glob set are treated as a match — never silently drop a rule because of a
/// parse error (over-recall is correct for retrieval). Iter-9 (2026-04-18)
/// port of cloud `patternAllows`; B8 (shared `glob_match`).
pub(super) fn pattern_allows(file_patterns_json: Option<&str>, target_file: &str) -> bool {
    glob_match(file_patterns_json, target_file, GlobErrorPolicy::OverRecall)
}

/// Retrieve rules with confidence weighting plus hybrid FTS / embedding retrieval.
///
/// Retrieval options for confidence weighting, scoping, ANN usage, and
/// trajectory telemetry. The default path matches plain retrieval: default
/// top-k, no confidence map, no target-file cascade, no SQL metadata filter,
/// ANN enabled, and no trajectory capture.
pub struct RetrievalOptions<'a> {
    pub top_k: Option<usize>,
    pub confidence_map: Option<&'a HashMap<String, f64>>,
    /// Optional allow-list applied before RRF fusion. Callers that already
    /// know the engine-eligible rule set can pass it here so disabled rules
    /// do not consume top-k score budget.
    pub eligible_skill_ids: Option<&'a HashSet<String>>,
    /// Iter-13 (2026-05-02). Per-skill age in days, used by the
    /// category-keyed half-life decay in `effective_confidence`. When
    /// `None` (or a chunk's `skill_id` is absent from the map) the
    /// scoring site uses `age_days = 0.0` — identical to the
    /// pre-plumbing behaviour, so no caller breaks if it doesn't pass
    /// a map.
    pub age_days_map: Option<&'a HashMap<String, f32>>,
    pub target_file: Option<&'a str>,
    pub filter: Option<&'a QueryFilter>,
    pub ann_enabled: bool,
    /// Optional provider-call budget for embedding the query. Latency-
    /// sensitive hook paths set this so a slow cloud embedder degrades to
    /// lexical retrieval instead of timing out the host agent's hook.
    pub embedding_timeout: Option<Duration>,
    /// When true, a query embed that falls back to lexical because the base
    /// budget timed out on a healthy cloud lane is retried once with a longer
    /// cold-absorbing budget (see [`COLD_RETRY_EMBEDDING_TIMEOUT`]). Only the
    /// human-waiting CLI `recall`/`search` path sets this; the latency-critical
    /// hook/MCP paths leave it `false` so a cold provider never blocks the agent.
    pub cold_start_retry: bool,
    /// When true, suppresses broad weak matches entirely. This is useful
    /// for unsolicited hook injection where "no extra context" is often
    /// better than five noisy rules. Explicit user/tool queries should
    /// leave this false so a search never looks broken just because the
    /// best match is weak.
    pub adaptive_prune: bool,
    pub trajectory: Option<&'a mut TrajectoryBuilder>,
}

impl Default for RetrievalOptions<'_> {
    fn default() -> Self {
        Self {
            top_k: None,
            confidence_map: None,
            eligible_skill_ids: None,
            age_days_map: None,
            target_file: None,
            filter: None,
            ann_enabled: true,
            embedding_timeout: None,
            cold_start_retry: false,
            adaptive_prune: false,
            trajectory: None,
        }
    }
}

/// Retrieve rules with confidence weighting plus hybrid FTS / embedding retrieval.
///
/// `confidence_map` maps `skill_id` -> `confidence_score`. If None, all rules
/// get default confidence 0.7.
///
/// `target_file`: when present, applies **strict cascade** — chunks whose
/// `file_patterns` don't match the target file are dropped before scoring.
/// When the matched bucket is empty, returns no pattern-scoped rules rather
/// than widening into rules explicitly tagged for other files.
///
/// `filter`: metadata pre-filter applied at SQL time (C2). Empty filter
/// means "no scoping" — retrieval sees every chunk.
///
/// `trajectory`: optional builder that captures RRF / filter statistics
/// for the cloud dashboard. Passing `None` disables telemetry.
pub async fn retrieve_rules_with_confidence(
    index_pool: &SqlitePool,
    query: &str,
    options: RetrievalOptions<'_>,
) -> Result<Vec<ScoredRuleChunk>, CoreError> {
    let RetrievalOptions {
        top_k,
        confidence_map,
        eligible_skill_ids,
        age_days_map,
        target_file,
        filter,
        ann_enabled,
        embedding_timeout,
        cold_start_retry,
        adaptive_prune,
        trajectory,
    } = options;
    let default_filter = QueryFilter::default();
    let filter = filter.unwrap_or(&default_filter);
    let requested_k = top_k.unwrap_or(DEFAULT_TOP_K_RULES);
    if requested_k == 0 {
        return Ok(Vec::new());
    }
    let k = requested_k.min(MAX_RULE_RETRIEVAL_TOP_K);
    let retrieval_start = std::time::Instant::now();
    let embedded_query =
        embed_query_aligned_to_index(index_pool, query, embedding_timeout, cold_start_retry).await;
    let query_emb = embedded_query.vector;

    // Switch the RRF weighting when the actual query vector is only the
    // local lexical hash, or when a provider failure disabled the vector
    // lane entirely.
    //
    // 2026-05-03 A/B verified the hybrid (local hash + FTS5 BM25) lifts
    // self-recall@5 from 45% (FTS-only) to 85%, and @1 from 10% to 45%.
    // The local hash isn't semantic but its bag-of-token overlap fills
    // FTS5's strict-tokenizer gap. Worth keeping until cloud-managed
    // embedding is configured.
    let is_semantic = embedded_query.semantic;

    // ── C2: SQL-level metadata pre-filter ──────────────────────────
    // When the filter is empty this reduces to `SELECT *`, matching the
    // pre-C2 behaviour and so zero-cost for unscoped callers.
    let unfiltered_count: u32 = if filter.is_empty() {
        0
    } else {
        sqlx::query_scalar!(r#"SELECT COUNT(*) as "n!: i64" FROM rule_chunks"#)
            .fetch_one(index_pool)
            .await
            .unwrap_or(0)
            .try_into()
            .unwrap_or(u32::MAX)
    };
    let chunks = index_db::query_rule_chunks(index_pool, filter).await?;
    let after_count: u32 = u32::try_from(chunks.len()).unwrap_or(u32::MAX);

    // ── C4: FTS5 keyword baseline ──────────────────────────────────
    // Pull `k*4` raw hits so we have RRF material even after the
    // pattern cascade trims some out.
    let fts_limit = k.saturating_mul(4).min(200).max(k);
    let fts_hits = index_db::fts_search(index_pool, query, filter, fts_limit)
        .await
        .unwrap_or_default();

    let default_confidence = 0.7;
    let min_confidence = 0.2;

    // Pre-partition by file-pattern match if target_file is set. This is the
    // strict cascade: when ANY chunk matches the target, drop the rest.
    let matched: Vec<&IndexedRuleChunk> = if let Some(tf) = target_file {
        chunks
            .iter()
            .filter(|c| pattern_allows(c.file_patterns.as_deref(), tf))
            .collect()
    } else {
        chunks.iter().collect()
    };
    let active: &[&IndexedRuleChunk] = if target_file.is_some() && matched.is_empty() {
        &[]
    } else {
        &matched
    };

    // Build a lookup table so FTS hits (identified by chunk id) can be
    // reconciled against the cascade-filtered active set.
    let id_to_chunk: HashMap<&str, &IndexedRuleChunk> =
        active.iter().map(|c| (c.id.as_str(), *c)).collect();

    // ── Embedding-ranked candidate list ───────────────────────────
    //
    // Try the HNSW ANN path first. It returns a small candidate set that is
    // intersected with the metadata-filtered `active` set. On any failure, fall
    // back to the linear cosine scan.
    let ann_candidates = k.saturating_mul(3).min(MAX_ANN_CANDIDATES).max(k);
    let ann_result = if ann_enabled {
        try_ann_rank(
            &query_emb,
            ann_candidates,
            &id_to_chunk,
            confidence_map,
            eligible_skill_ids,
            default_confidence,
            min_confidence,
        )
        .await
    } else {
        None
    };

    let (mut emb_ranked, ann_used, ann_index_size, ann_returned): (
        Vec<(&IndexedRuleChunk, f64)>,
        bool,
        u32,
        u32,
    ) = if let Some((ranked, idx_size, returned)) = ann_result {
        (ranked, true, idx_size, returned)
    } else {
        let fallback: Vec<(&IndexedRuleChunk, f64)> = active
            .iter()
            .filter_map(|c: &&IndexedRuleChunk| {
                if !eligible_skill_ids.is_none_or(|ids| ids.contains(&c.skill_id)) {
                    return None;
                }
                let confidence = confidence_map
                    .and_then(|m| m.get(&c.skill_id).copied())
                    .unwrap_or(default_confidence);
                if confidence < min_confidence {
                    return None;
                }
                if query_emb.len() != c.embedding.len() {
                    return None;
                }
                let sim = cosine_similarity(&query_emb, &c.embedding);
                Some((*c, f64::from(sim)))
            })
            .collect();
        (fallback, false, 0, 0)
    };
    emb_ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.id.cmp(&b.0.id)));

    let emb_rank_map: HashMap<&str, usize> = emb_ranked
        .iter()
        .enumerate()
        .map(|(i, (c, _))| (c.id.as_str(), i))
        .collect();

    // ── FTS rank map (only keeps hits that survived the cascade). ──
    let mut fts_rank_map: HashMap<&str, usize> = HashMap::new();
    let mut fts_kept = 0u32;
    for (i, (id, _)) in fts_hits.iter().enumerate() {
        if id_to_chunk.contains_key(id.as_str()) {
            fts_rank_map.insert(id.as_str(), i);
            fts_kept += 1;
        }
    }

    // Overlap metric for telemetry — how many ids were ranked by BOTH
    // paths. High overlap → paths agree; low overlap → they're
    // surfacing complementary results (the whole point of hybrid).
    let overlap: u32 = {
        let fts_ids: HashSet<&str> = fts_rank_map.keys().copied().collect();
        let emb_ids: HashSet<&str> = emb_rank_map.keys().copied().collect();
        u32::try_from(fts_ids.intersection(&emb_ids).count()).unwrap_or(u32::MAX)
    };

    // ── RRF fusion ────────────────────────────────────────────────
    //
    //    score(chunk) = w_emb * 1/(k+rank_emb) + w_fts * 1/(k+rank_fts)
    //
    // When the embedder is not semantic, skew toward the FTS baseline because
    // local SHA1 is noise-dominated.
    let (w_emb, w_fts) = if is_semantic { (0.5, 0.5) } else { (0.2, 0.8) };

    let mut fused: HashMap<&str, (f64, &IndexedRuleChunk, f64 /*confidence*/)> = HashMap::new();
    // `_sim` is unused directly in RRF (ranks already encode it). The
    // raw score is kept in the vector only to sort embedding candidates
    // before assigning reciprocal ranks.
    for (chunk, _sim) in &emb_ranked {
        let rank = emb_rank_map.get(chunk.id.as_str()).copied().unwrap_or(0);
        let contrib = w_emb / (RRF_K + rank as f64 + 1.0);
        let confidence = confidence_map
            .and_then(|m| m.get(&chunk.skill_id).copied())
            .unwrap_or(default_confidence);
        fused
            .entry(chunk.id.as_str())
            .and_modify(|e| e.0 += contrib)
            .or_insert((contrib, *chunk, confidence));
    }
    for (id, rank) in &fts_rank_map {
        if let Some(chunk) = id_to_chunk.get(id) {
            if !eligible_skill_ids.is_none_or(|ids| ids.contains(&chunk.skill_id)) {
                continue;
            }
            let contrib = w_fts / (RRF_K + *rank as f64 + 1.0);
            let confidence = confidence_map
                .and_then(|m| m.get(&chunk.skill_id).copied())
                .unwrap_or(default_confidence);
            if confidence < min_confidence {
                continue;
            }
            fused
                .entry(id)
                .and_modify(|e| e.0 += contrib)
                .or_insert((contrib, *chunk, confidence));
        }
    }

    // ── Emit trajectory telemetry (best-effort, never blocks recall) ──
    if let Some(t) = trajectory {
        if !filter.is_empty() {
            t.push(TrajectoryStep::RetrievalFilter {
                before: unfiltered_count,
                after: after_count,
            });
        }
        t.push(TrajectoryStep::AnnRecall {
            used: ann_used,
            index_size: ann_index_size,
            candidates: ann_returned,
        });
        t.push(TrajectoryStep::HybridFusion {
            fts_hits: fts_kept,
            emb_hits: u32::try_from(emb_ranked.len()).unwrap_or(u32::MAX),
            overlap,
        });
    }

    // Materialise the final scored list. `ScoredRuleChunk.score` is
    // the fused RRF score multiplied by a *small* confidence multiplier
    // — confidence acts as a tie-breaker rather than the primary
    // ranking signal. The earlier `sqrt(confidence)` weight flipped
    // the ordering on real workloads: a freshly captured (conf=0.6)
    // conversation rule with a strong file-pattern + lexical match was
    // demoted below cloud-extracted rules (conf=0.7) whose query
    // overlap was 5-20% lower. Net result: the rule the user just
    // taught DiffLore was the LAST rule injected for the very file it
    // applies to. That breaks the slogan ("AI understands your preferences better and better") at
    // exactly the moment users will check whether the slogan is true.
    //
    // The 0.9 + 0.1 * confidence multiplier keeps spread at 8%
    // (conf=0.2 floor → 0.92; conf=1.0 → 1.0). RRF score gaps between
    // adjacent ranks in our regime are 5-20%, so confidence can break
    // a near-tie but cannot overturn a clear lexical/semantic winner.
    // Strengthening (+0.05 confidence per accept) still earns +0.5%
    // multiplier — enough to win against an equally-relevant peer at
    // a lower confidence, which is what "the rule I've ratified twice
    // outranks the rule captured once" should feel like.
    let mut scored: Vec<ScoredRuleChunk> = fused
        .into_values()
        .map(|(score, chunk, confidence)| {
            // Confidence tie-breaker (8% spread) + content-concreteness
            // boost. Iter-12 (2026-04-25) added the concreteness factor
            // because rule-impact-by-kind audit showed slogan rules
            // ("Trust CI for workflow correctness", "Hold clean PRs for
            // additional review") were misfiring across languages —
            // they have no concrete code tokens to anchor relevance to.
            // The concreteness signal counts backticked tokens + path-
            // like fragments + version literals in the rule's content,
            // saturated at 6 hits to avoid runaway when a rule body is
            // mostly code. Net: a singleton rule citing
            // `useQuery({...})` outranks a generic slogan with the
            // same lexical match, fixing the Python −0.38 over-engineer
            // regime we measured in iter 9.6.
            // Iter-13 (2026-05-02). Borrow jcode's category-keyed half-life
            // so an ancient style rule no longer outranks a freshly ratified
            // correction on conf alone. Kind is inferred from chunk content
            // (no `kind` column on rule_chunks); age_days comes from the
            // optional per-call `age_days_map` (None ⇒ 0.0 ⇒ no decay,
            // matching the original behaviour for callers that haven't
            // wired the map yet).
            let kind = infer_rule_kind(&chunk.content);
            let age_days = age_days_map
                .and_then(|m| m.get(&chunk.skill_id).copied())
                .unwrap_or(0.0);
            let eff_conf = f64::from(effective_confidence(confidence as f32, &kind, age_days));
            let conf_weight = 0.1f64.mul_add(eff_conf.clamp(0.0, 1.0), 0.9);
            let conc = concreteness_score(&chunk.content);
            // Each concreteness "point" adds 5% to score, capped at +30%.
            let conc_weight = 0.05f64.mul_add(conc.min(6) as f64, 1.0);
            ScoredRuleChunk {
                skill_id: chunk.skill_id.clone(),
                content: chunk.content.clone(),
                score: score * conf_weight * conc_weight,
                confidence,
            }
        })
        .collect();
    scored.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.skill_id.cmp(&b.skill_id))
    });

    // Adaptive top-K + noise floor.
    //
    // Iter-12 hardens the "less is more" principle on top of iter-4's
    // floors. The fastapi/Python regression (-0.38 ΔB-A in iter 9.6)
    // traced to the agent receiving 5 weak rules on simple tasks (typo
    // fix, parameter substitution) where claude's training already
    // nailed the answer. Five weak rules induced over-engineering. The
    // fix: when the top result's score itself is in the noise band,
    // emit ZERO rules — let the agent trust its training.
    //
    // Adaptive zero-inject is **only safe for unsolicited
    // injection** (PreToolUse:Read hook). Explicit user queries via
    // Explicit canonical MCP rule-search calls must always
    // return what's available — when a user types `search_rules
    // intent=...`, returning empty would feel broken even if scores
    // are weak. Callers opt in by setting `top_k=Some(5)` AND wanting
    // adaptive behaviour explicitly via the iter-12 hook contract.
    //
    // The rule of thumb: if only the absolute floor would have kept
    // ≥3 results in scope (i.e. there's a real "noise tail" worth
    // pruning), apply adaptive. Tiny corpora with 1-2 candidates
    // bypass adaptive — those results are fine to return as-is.
    // Adaptive zero-inject only when we'd otherwise return many weak
    // matches (the "5 weak rules" pathology). Small corpora and small
    // result sets bypass — those are explicit user queries with
    // limited candidates anyway.
    let adaptive_eligible = adaptive_prune && scored.len() >= 5;
    if let Some(top_score) = scored.first().map(|s| s.score) {
        if adaptive_eligible && top_score < ADAPTIVE_INJECT_THRESHOLD {
            // Top match is itself weak AND we have many results — this
            // is the "5 weak rules" pathology. Return empty.
            scored.clear();
        } else {
            prune_below_floors(&mut scored, top_score);

            // Adaptive K: when many results cluster within 60% of the
            // top, agent can't tell signal from noise — return just
            // the clearly-strong ones. Skip when result set is tiny
            // (already informative).
            if adaptive_eligible {
                let strong_floor = top_score * 0.60;
                let strong_count = scored
                    .iter()
                    .take_while(|s| s.score >= strong_floor)
                    .count();
                if strong_count > 0 && strong_count < scored.len() {
                    scored.truncate(strong_count.min(k));
                }
            }
        }
    }

    scored.truncate(k);

    // Memory-pipeline event: surfaces the ANN/embedding pass to the TUI
    // Activity tab so users can see retrieval running. Best-effort —
    // never blocks recall.
    crate::activity_stream::record(
        crate::activity_stream::ActivityPayload::RetrievalEmbedding {
            hits: u32::try_from(scored.len()).unwrap_or(u32::MAX),
            took_ms: u64::try_from(retrieval_start.elapsed().as_millis()).unwrap_or(u64::MAX),
        },
    );

    Ok(scored)
}

/// Drop the RRF noise tail from an already-sorted (descending) scored
/// list using the two floors that have always guarded retrieval: the
/// absolute [`MIN_RELEVANCE_SCORE`] (RRF rounding noise / cascade-only
/// admits) AND the relative [`RELATIVE_RELEVANCE_FLOOR`] fraction of the
/// top hit (the "everything scored 0.02" flat-distribution pathology).
///
/// Factored out of `retrieve_rules_with_confidence` so the same retain
/// is shared with the explicit-recall gate below and is unit-testable in
/// isolation. Pure: mutates `scored` in place, never re-sorts (the caller
/// has already sorted), so `top_score` must be the current leader's score.
fn prune_below_floors(scored: &mut Vec<ScoredRuleChunk>, top_score: f64) {
    let relative_floor = top_score * RELATIVE_RELEVANCE_FLOOR;
    scored.retain(|s| s.score > MIN_RELEVANCE_SCORE && s.score >= relative_floor);
}

/// Adaptive relevance gate for the EXPLICIT recall surfaces — the MCP
/// `search_rules` tool and the CLI `recall` command. Mirrors the hook
/// path's adaptive pruning so an agent never has to weigh five weak rules
/// against an empty answer: irrelevant memory is worse than none.
///
/// The hook path (`adaptive_prune == true` inside
/// `retrieve_rules_with_confidence`) zero-injects on a weak top hit and
/// drops the noise tail *before* any downstream reranking. The explicit
/// paths can't do that in-retrieval because they still add high-value
/// signals after fusion — exact-title-strict matches (score `2.0 + conf`),
/// the cross-repo starter set, and the lexical-intent re-rank boost — so
/// this gate runs on the FINAL, fully-reranked, sorted list instead. The
/// net contract is the same as the hook's: a low-relevance query
/// (wrong-file, no intent overlap — e.g. a Codecov rule surfacing in a
/// wrong-file top-3) collapses to ZERO results so the caller emits its
/// existing "no relevant memory" message rather than confident filler.
///
/// Two conservative gates, tuned so genuinely-strong matches are NEVER
/// suppressed:
///   1. Absolute floor — if even the top hit is below
///      [`EXPLICIT_RECALL_MIN_RELEVANCE`], every result is noise: clear.
///      After the lexical-intent re-rank a genuinely relevant top hit
///      sits far above this floor (boosted into the 0.1+ range), while a
///      cascade-only / no-overlap top hit stays in the raw RRF band
///      (~0.001–0.005) and is correctly dropped.
///   2. Relative floor — drop tail results below
///      [`EXPLICIT_RECALL_RELATIVE_FLOOR`] of the (surviving) top hit, so
///      a strong leader doesn't drag along far-weaker filler. Deliberately
///      looser than the hook's [`RELATIVE_RELEVANCE_FLOOR`]: explicit
///      queries should keep more of a real result set, only shedding the
///      clearly-irrelevant tail.
///
/// Pure and in-place. The caller must pass a list already sorted
/// descending by `score` (both explicit call sites do, via their final
/// re-rank). Strong matches (including exact-title-strict and starter
/// hits) clear both floors by a wide margin, so this never regresses a
/// real recall.
pub fn apply_explicit_recall_threshold(scored: &mut Vec<ScoredRuleChunk>) {
    let Some(top_score) = scored.first().map(|s| s.score) else {
        return;
    };
    // Absolute floor: the best match itself is noise → return nothing.
    if top_score < EXPLICIT_RECALL_MIN_RELEVANCE {
        scored.clear();
        return;
    }
    // Relative floor: shed the tail far below the leader.
    let relative_floor = top_score * EXPLICIT_RECALL_RELATIVE_FLOOR;
    scored.retain(|s| s.score >= relative_floor);
}

/// Intent-alignment gate for the EXPLICIT recall surfaces — applied BEFORE
/// [`apply_explicit_recall_threshold`] on the final, fully-reranked list.
///
/// WHY: topically adjacent rules can clear relevance floors while addressing a
/// different action or subject than the directive. This gate adds the missing
/// axis: does the rule's directive match the query intent, not just its topic?
///
/// Behaviour, biased hard toward FEWER / zero (DiffLore's "stay silent
/// unless it clearly applies" positioning):
///   * An all-weak query (no salient terms after stop-word filtering) cannot
///     establish intent for ANY rule → clear. Returning nothing is correct
///     here: we have no signal to claim a match.
///   * A candidate is KEPT when it is either strongly scored (≥
///     [`INTENT_ALIGNMENT_EXEMPT_SCORE`] — exact-title-strict / starter /
///     strongly lexically-boosted hits, already intent-validated upstream)
///     or its directive is intent-aligned per [`directive_intent_aligned`].
///   * Every other candidate — the topically-adjacent middle band — is
///     dropped.
///
/// Conservative by construction: the strong-score exemption guarantees no
/// genuinely-strong match (and therefore no eval self-recall hit, where the
/// query is the rule's own intent text and overlap is near-total) is ever
/// suppressed. Pure / in-place; order is preserved (the caller has already
/// sorted, and this only `retain`s).
pub fn apply_intent_alignment_gate(scored: &mut Vec<ScoredRuleChunk>, intent: &str) {
    if scored.is_empty() {
        return;
    }
    let query_terms = lexical_terms(intent);
    if query_terms.is_empty() {
        // No salient intent to align against — per the "fewer / zero"
        // bias, an unscorable intent yields no confident matches.
        scored.clear();
        return;
    }
    scored.retain(|chunk| {
        chunk.score >= INTENT_ALIGNMENT_EXEMPT_SCORE
            || directive_intent_aligned(&chunk.content, &query_terms)
    });
}

/// Attempt the HNSW ANN ranking path for the current project.
///
/// Returns `Some((ranked, index_size, returned))` on a successful ANN
/// lookup that produced at least one candidate inside the
/// metadata-filtered `active` set. Returns `None` on any of:
/// - empty / missing on-disk index
/// - dim mismatch between query and stored vectors
/// - ANN search yielded zero usable candidates (e.g. all hits were
///   tombstoned or outside the active filter)
/// - any internal error talking to the ANN cache
///
/// The caller MUST treat `None` as "use the linear cosine scan". This
/// is the safety net that guarantees retrieval keeps working when the
/// HNSW index is absent or stale.
async fn try_ann_rank<'a>(
    query_emb: &[f32],
    candidates: usize,
    id_to_chunk: &HashMap<&'a str, &'a IndexedRuleChunk>,
    confidence_map: Option<&HashMap<String, f64>>,
    eligible_skill_ids: Option<&HashSet<String>>,
    default_confidence: f64,
    min_confidence: f64,
) -> Option<(Vec<(&'a IndexedRuleChunk, f64)>, u32, u32)> {
    if query_emb.is_empty() || candidates == 0 {
        return None;
    }
    // Resolve the project hash from the current working directory. The
    // ANN cache is keyed on this hash so MCP calls running in the same
    // project share one graph across calls. Retrieval call sites that
    // run outside a project root (unit tests in a tempdir) will still
    // get a valid hash — they just won't have a persisted graph to
    // reload, which is fine: `load_or_empty` returns an empty index and
    // we fall through to the linear scan.
    let project_root = crate::db::current_project_root();
    let project_hash = crate::db::project_hash_from_root(&project_root);

    let ann_arc = ann::get_ann_for_project(&project_hash, query_emb.len())
        .await
        .ok()?;
    let ann_guard = ann_arc.lock().await;
    let index_size = ann_guard.live_size();
    if index_size == 0 {
        return None;
    }
    let hits = ann_guard.search(query_emb, candidates);
    if hits.is_empty() {
        return None;
    }
    let returned = u32::try_from(hits.len()).unwrap_or(u32::MAX);

    // Translate the ANN hit set back into `&IndexedRuleChunk` + RRF
    // score. The score we carry is raw cosine similarity so confidence
    // is applied at exactly one ranking site (the final tie-breaker).
    // DistCosine returns `1 - cos`, so cosine similarity is `1 - distance`.
    let mut ranked: Vec<(&IndexedRuleChunk, f64)> = Vec::with_capacity(hits.len());
    for (chunk_id, distance) in hits {
        let Some(chunk) = id_to_chunk.get(chunk_id.as_str()) else {
            // Hit lives in the graph but didn't survive the metadata
            // pre-filter — drop it.
            continue;
        };
        if !eligible_skill_ids.is_none_or(|ids| ids.contains(&chunk.skill_id)) {
            continue;
        }
        let confidence = confidence_map
            .and_then(|m| m.get(&chunk.skill_id).copied())
            .unwrap_or(default_confidence);
        if confidence < min_confidence {
            continue;
        }
        let sim = (1.0 - f64::from(distance)).max(0.0);
        ranked.push((*chunk, sim));
    }
    if ranked.is_empty() {
        // ANN surfaced hits but none survived the filter — treat as a
        // miss so the linear scan can try to find something.
        return None;
    }
    Some((ranked, index_size, returned))
}

#[cfg(test)]
mod tests {
    use super::super::MIN_INTENT_DIRECTIVE_OVERLAP;
    use super::*;

    fn chunk(id: &str, score: f64) -> ScoredRuleChunk {
        ScoredRuleChunk {
            skill_id: id.to_owned(),
            content: format!("Rule ID: {id}\nRule Name: {id}\n\nbody"),
            score,
            confidence: 0.7,
        }
    }

    #[test]
    fn explicit_recall_threshold_strong_top_hit_survives() {
        // A genuinely strong match (lexically boosted into the 0.1+ band)
        // must always survive — the gate is conservative and never
        // suppresses real recall.
        let mut scored = vec![chunk("strong", 0.30), chunk("supporting", 0.12)];
        apply_explicit_recall_threshold(&mut scored);
        assert_eq!(scored.len(), 2, "strong matches must not be pruned");
        assert_eq!(scored[0].skill_id, "strong");
    }

    #[test]
    fn explicit_recall_threshold_all_weak_returns_empty() {
        // Wrong-file / low-relevance query: even the top hit is in the raw
        // fused RRF noise band, so the whole set is filler and should return
        // zero results.
        let mut scored = vec![
            chunk("noise-1", 0.004),
            chunk("noise-2", 0.003),
            chunk("noise-3", 0.002),
            chunk("noise-4", 0.0015),
            chunk("noise-5", 0.001),
        ];
        apply_explicit_recall_threshold(&mut scored);
        assert!(
            scored.is_empty(),
            "a query whose only matches are weak must return zero results"
        );
    }

    #[test]
    fn explicit_recall_threshold_borderline_keeps_only_strong() {
        // Borderline set: one clear leader well above the absolute floor,
        // plus tail rules far below it. The leader (and anything within the
        // relative band) survives; the far-below-leader tail is dropped.
        let mut scored = vec![
            chunk("leader", 0.40),
            chunk("near", 0.10), // 25% of leader — within the 0.20 relative floor
            chunk("tail-1", 0.05), // 12.5% of leader — dropped
            chunk("tail-2", 0.02),
            chunk("tail-3", 0.011),
        ];
        apply_explicit_recall_threshold(&mut scored);
        let ids: Vec<&str> = scored.iter().map(|s| s.skill_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["leader", "near"],
            "only the leader and rules within the relative band survive"
        );
    }

    #[test]
    fn explicit_recall_threshold_top_hit_at_absolute_floor_is_kept() {
        // A top hit exactly at the absolute floor is NOT below it, so it
        // survives — proving the gate suppresses only genuine sub-floor
        // noise, never a borderline-but-present match.
        let mut scored = vec![chunk("at-floor", EXPLICIT_RECALL_MIN_RELEVANCE)];
        apply_explicit_recall_threshold(&mut scored);
        assert_eq!(scored.len(), 1, "top hit at the floor must be kept");
    }

    #[test]
    fn explicit_recall_threshold_empty_input_is_noop() {
        let mut scored: Vec<ScoredRuleChunk> = Vec::new();
        apply_explicit_recall_threshold(&mut scored);
        assert!(scored.is_empty());
    }

    // -- Intent-alignment gate tests (precision fix) --

    /// Build a candidate whose distilled directive is its `Rule Name:` title.
    /// `score` is left in the moderate (gated) band by default so the gate's
    /// alignment check — not the strong-score exemption — decides its fate.
    fn directive_chunk(id: &str, title: &str, score: f64) -> ScoredRuleChunk {
        ScoredRuleChunk {
            skill_id: id.to_owned(),
            content: format!(
                "Rule ID: {id}\nRule Name: {title}\nType: convention\nTags: \n\n{title}."
            ),
            score,
            confidence: 0.7,
        }
    }

    #[test]
    fn intent_gate_drops_topically_adjacent_different_subject_rule() {
        // The diagnosed failure: a "return false vs panic" directive recalls a
        // panic-MESSAGE-wording rule and a test-timing rule. Both share the
        // file area / topical anchor ("panic"/"test") but address a DIFFERENT
        // action+subject than the query, so the agent gets distracted. Each is
        // dropped because its directive shares <2 of the query's salient terms
        // (and <half of them).
        let mut scored = vec![
            directive_chunk(
                "panic-message-wording",
                "Panic messages should describe the violated invariant",
                0.12,
            ),
            directive_chunk(
                "test-timing",
                "Avoid sleep-based waits in tests; poll for the condition",
                0.10,
            ),
        ];
        apply_intent_alignment_gate(
            &mut scored,
            "return false instead of panic on invalid input",
        );
        assert!(
            scored.is_empty(),
            "topically-adjacent, wrong-subject rules must be dropped, kept: {:?}",
            scored.iter().map(|s| &s.skill_id).collect::<Vec<_>>()
        );
    }

    #[test]
    fn intent_gate_keeps_directly_on_subject_rule() {
        // The on-subject rule shares the action verb AND its object
        // (return + false + panic + input), clearing the absolute-overlap
        // bar, so it survives even at a moderate (non-exempt) score.
        let mut scored = vec![directive_chunk(
            "return-false-not-panic",
            "Return false rather than panic on invalid input",
            0.12,
        )];
        apply_intent_alignment_gate(
            &mut scored,
            "return false instead of panic on invalid input",
        );
        assert_eq!(
            scored
                .iter()
                .map(|s| s.skill_id.as_str())
                .collect::<Vec<_>>(),
            vec!["return-false-not-panic"],
            "a directly-on-subject directive must survive the intent gate"
        );
    }

    #[test]
    fn intent_gate_keeps_on_subject_drops_adjacent_in_same_set() {
        // The realistic mixed set the A/B saw: the on-subject rule plus the two
        // topically-adjacent distractors, all admitted by hybrid retrieval.
        // The gate keeps only the aligned one.
        let mut scored = vec![
            directive_chunk(
                "return-false-not-panic",
                "Return false rather than panic on invalid input",
                0.12,
            ),
            directive_chunk(
                "panic-message-wording",
                "Panic messages should describe the violated invariant",
                0.11,
            ),
            directive_chunk(
                "test-timing",
                "Avoid sleep-based waits in tests; poll for the condition",
                0.10,
            ),
        ];
        apply_intent_alignment_gate(
            &mut scored,
            "return false instead of panic on invalid input",
        );
        assert_eq!(
            scored
                .iter()
                .map(|s| s.skill_id.as_str())
                .collect::<Vec<_>>(),
            vec!["return-false-not-panic"],
            "only the intent-aligned rule should survive the mixed set"
        );
    }

    #[test]
    fn intent_gate_all_weak_query_returns_zero() {
        // A query with no salient (non-stop-word, ≥3-char) terms gives the gate
        // nothing to align against. Per DiffLore's "stay silent unless it
        // clearly applies" bias, that yields zero — no confident match.
        let mut scored = vec![
            directive_chunk("a", "Return false rather than panic on invalid input", 0.12),
            directive_chunk("b", "Use structured errors in request handlers", 0.10),
        ];
        // "the and to of" → all stop words; nothing ≥3 chars survives lexical_terms.
        apply_intent_alignment_gate(&mut scored, "the and to of");
        assert!(
            scored.is_empty(),
            "an all-weak query must return zero, kept: {:?}",
            scored.iter().map(|s| &s.skill_id).collect::<Vec<_>>()
        );
    }

    #[test]
    fn intent_gate_exempts_strongly_scored_hits() {
        // Exact-title-strict / starter / lexically-boosted hits land at or
        // above the exemption ceiling and are kept regardless of directive
        // overlap — the strong-match / self-recall non-regression guarantee.
        let mut scored = vec![ScoredRuleChunk {
            skill_id: "exact-title-strict".to_owned(),
            content: "Rule ID: x\nRule Name: Completely unrelated heading\n\nbody".to_owned(),
            // 2.0 + conf band: an exact-title-strict match.
            score: 2.7,
            confidence: 0.7,
        }];
        apply_intent_alignment_gate(
            &mut scored,
            "return false instead of panic on invalid input",
        );
        assert_eq!(
            scored.len(),
            1,
            "a strongly-scored hit must be exempt from the alignment gate"
        );
    }

    #[test]
    fn intent_gate_ratio_path_keeps_short_sharp_query_match() {
        // A short 2-salient-term intent ("panic safety") whose directive shares
        // ONE term is below the absolute bar (2) but covers half the query's
        // salient terms, so the ratio path keeps it — short queries don't
        // over-prune.
        let mut scored = vec![directive_chunk(
            "panic-safety",
            "Document panic safety for unsafe blocks",
            0.12,
        )];
        apply_intent_alignment_gate(&mut scored, "panic safety");
        assert_eq!(
            scored.len(),
            1,
            "a half-coverage match on a short query must survive via the ratio path"
        );
    }

    #[test]
    fn intent_gate_empty_input_is_noop() {
        let mut scored: Vec<ScoredRuleChunk> = Vec::new();
        apply_intent_alignment_gate(&mut scored, "anything");
        assert!(scored.is_empty());
    }

    // -- Iter-2 stricter concern-match tests --

    #[test]
    fn intent_gate_drops_two_generic_anchor_overlap_without_distinctive_term() {
        // The precision tightening over iter-1. The OLD gate kept any rule whose
        // directive shared >=2 query terms. Here a "panic on invalid input"
        // intent and a runtime-error rule share TWO terms — but both are GENERIC
        // anchors (`panic`, `error`, `input`) with no specific subject/action
        // token in common. That is exactly the topical-adjacency the A/B blamed
        // for the extra false positives, so the hardened gate drops it.
        let mut scored = vec![directive_chunk(
            "runtime-error-logging",
            "Log every panic and error with the request input id",
            0.12,
        )];
        apply_intent_alignment_gate(&mut scored, "panic on invalid input handling");
        assert!(
            scored.is_empty(),
            "an all-generic-anchor overlap must not establish a concern match, kept: {:?}",
            scored.iter().map(|s| &s.skill_id).collect::<Vec<_>>()
        );
    }

    #[test]
    fn intent_gate_drops_off_subject_rule_that_namedrops_one_distinctive_token() {
        // A rule about a DIFFERENT subject that merely name-drops one of the
        // query's distinctive tokens. The query "validate the auth token before
        // issuing a session" shares `token` with a CSV-parsing rule, but the
        // rule's own directive is overwhelmingly about something else, so its
        // rule-side coverage is far below the floor → dropped. This is the
        // bidirectional half of the gate: a single shared word inside a rule
        // about another concern is not a match.
        let mut scored = vec![directive_chunk(
            "csv-token-splitting",
            "Split each CSV row into fields on the comma token boundary carefully",
            0.12,
        )];
        apply_intent_alignment_gate(
            &mut scored,
            "validate the auth token before issuing session",
        );
        assert!(
            scored.is_empty(),
            "a one-token name-drop in an off-subject rule must be dropped, kept: {:?}",
            scored.iter().map(|s| &s.skill_id).collect::<Vec<_>>()
        );
    }

    #[test]
    fn intent_gate_keeps_on_subject_rule_with_verbose_body() {
        // No over-pruning regression: a genuinely on-subject rule whose title
        // states the concern but whose BODY is long must still be kept. The
        // rule-side coverage is measured against the TITLE (the core directive),
        // so the verbose body does not dilute it below the floor.
        let verbose_body = "When a handler receives malformed input it should return a typed \
            error to the caller rather than calling panic!, because a panic unwinds the worker \
            thread and takes down unrelated in-flight requests; prefer Result and propagate. \
            See the request lifecycle docs and the error-taxonomy appendix for the full list.";
        let mut scored = vec![ScoredRuleChunk {
            skill_id: "validate-return-error".to_owned(),
            content: format!(
                "Rule ID: r\nRule Name: Validate input and return a typed error not panic\nType: correction\nTags: \n\n{verbose_body}"
            ),
            score: 0.12,
            confidence: 0.7,
        }];
        apply_intent_alignment_gate(
            &mut scored,
            "validate input and return error instead of panic",
        );
        assert_eq!(
            scored
                .iter()
                .map(|s| s.skill_id.as_str())
                .collect::<Vec<_>>(),
            vec!["validate-return-error"],
            "an on-subject rule with a long body must survive (title-scoped coverage)"
        );
    }

    #[test]
    fn intent_gate_strictly_subsumes_old_overlap_count_on_anchor_only_match() {
        // Anchor-only overlap is rejected, while the distinctive-token sibling
        // is kept. Both share the same raw term count; only distinctiveness and
        // rule-side coverage separate them.
        let intent = "panic on invalid input";
        // overlap = {panic(g), input(g)} = 2, distinctive = 0 → DROP under new gate.
        let mut anchor_only = vec![directive_chunk(
            "anchor-only",
            "Buffer every panic and input event into the queue",
            0.12,
        )];
        apply_intent_alignment_gate(&mut anchor_only, intent);
        assert!(
            anchor_only.is_empty(),
            "anchor-only overlap (old gate would keep) must now drop"
        );
        // overlap = {panic(g), invalid(d)} ⊇ the subject; distinctive = 1 → KEEP.
        let mut on_subject = vec![directive_chunk(
            "on-subject",
            "Reject invalid input instead of letting it panic",
            0.12,
        )];
        apply_intent_alignment_gate(&mut on_subject, intent);
        assert_eq!(
            on_subject
                .iter()
                .map(|s| s.skill_id.as_str())
                .collect::<Vec<_>>(),
            vec!["on-subject"],
            "the distinctive-token sibling must be kept"
        );
    }

    #[test]
    fn intent_alignment_exempt_score_sits_above_strong_band_below_exact_title() {
        // The exemption ceiling must sit ABOVE the boosted strong-match RRF
        // band (lexical re-rank tops out at +0.45 over a ~0.1 fused score) and
        // BELOW the exact-title-strict `2.0 + conf` floor, so it exempts the
        // unambiguous winners without exempting the topically-adjacent middle
        // band the gate exists to scrutinise.
        assert!(
            INTENT_ALIGNMENT_EXEMPT_SCORE > EXPLICIT_RECALL_MIN_RELEVANCE,
            "exemption ceiling must be above the explicit relevance floor"
        );
        assert!(
            INTENT_ALIGNMENT_EXEMPT_SCORE < 2.0,
            "exemption ceiling must be below the exact-title-strict (2.0 + conf) band"
        );
        assert!(
            MIN_INTENT_DIRECTIVE_OVERLAP >= 2,
            "a lone topical-anchor overlap must be insufficient"
        );
    }

    #[test]
    fn explicit_recall_floors_are_conservative_relative_to_in_retrieval_gates() {
        // The explicit gate must be looser than the in-retrieval hook gate
        // (it should keep MORE of a real result set), and its absolute
        // floor must sit above the hook's zero-inject threshold so it can
        // actually drop the cascade-only noise the hook also rejects.
        assert!(
            EXPLICIT_RECALL_RELATIVE_FLOOR < RELATIVE_RELEVANCE_FLOOR,
            "explicit relative floor must be looser than the in-retrieval one"
        );
        assert!(
            EXPLICIT_RECALL_MIN_RELEVANCE > ADAPTIVE_INJECT_THRESHOLD,
            "explicit absolute floor must sit above the hook zero-inject threshold"
        );
        assert!(
            EXPLICIT_RECALL_MIN_RELEVANCE > MIN_RELEVANCE_SCORE,
            "explicit absolute floor must be stricter than the bare RRF noise floor"
        );
    }
}
