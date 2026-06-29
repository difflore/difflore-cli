use crate::context::DEFAULT_TOP_K_RULES;
use crate::context::ann;
use crate::context::embedding::{EmbeddedText, cosine_similarity, embed_text};
use crate::context::index_db::{self, IndexedRuleChunk, QueryFilter};
use crate::domain::glob_match::{GlobErrorPolicy, glob_match, glob_match_changeset};
use crate::error::CoreError;
use crate::observability::trajectory::{TrajectoryBuilder, TrajectoryStep};
use sqlx::SqlitePool;
use std::collections::{HashMap, HashSet};
use std::time::Duration;

use super::query_embed::embed_query_aligned_to_index;
use super::scoring::{directive_intent_aligned, effective_confidence, infer_rule_kind};
use super::{
    ADAPTIVE_INJECT_THRESHOLD, EXPLICIT_RECALL_MIN_RELEVANCE, EXPLICIT_RECALL_RELATIVE_FLOOR,
    INTENT_ALIGNMENT_EXEMPT_SCORE, MIN_RELEVANCE_SCORE, RELATIVE_RELEVANCE_FLOOR, RRF_K,
    ScoredRuleChunk, lexical_terms, rule_quality_weight,
};

const MAX_RULE_RETRIEVAL_TOP_K: usize = 50;
const MAX_ANN_CANDIDATES: usize = 150;
/// Path hints should break close ties, not override semantic relevance.
const PATH_HINT_SCORE_BOOST: f64 = 1.08;
const SEMANTIC_EMBED_RRF_WEIGHT: f64 = 0.35;
const SEMANTIC_FTS_RRF_WEIGHT: f64 = 0.65;
const LOCAL_HASH_EMBED_RRF_WEIGHT: f64 = 0.20;
const LOCAL_HASH_FTS_RRF_WEIGHT: f64 = 0.80;

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
/// parse error (over-recall is correct for retrieval).
pub(super) fn pattern_allows(file_patterns_json: Option<&str>, target_file: &str) -> bool {
    glob_match(file_patterns_json, target_file, GlobErrorPolicy::OverRecall)
        || documentation_code_pattern_allows(file_patterns_json, target_file)
}

fn documentation_code_pattern_allows(file_patterns_json: Option<&str>, target_file: &str) -> bool {
    let normalized_target = target_file
        .trim_start_matches('/')
        .replace('\\', "/")
        .to_ascii_lowercase();
    let is_documentation_markdown = (normalized_target.ends_with(".md")
        || normalized_target.ends_with(".mdx"))
        && (normalized_target.starts_with("docs/") || normalized_target.contains("/docs/"));
    if !is_documentation_markdown {
        return false;
    }

    let Some(raw) = file_patterns_json
        .map(str::trim)
        .filter(|raw| !raw.is_empty())
    else {
        return false;
    };
    let Ok(patterns) = serde_json::from_str::<Vec<String>>(raw) else {
        return false;
    };
    patterns.iter().any(|pattern| {
        let lower = pattern.to_ascii_lowercase();
        lower.contains(".ts")
            || lower.contains(".tsx")
            || lower.contains(".mts")
            || lower.contains(".cts")
            || lower.contains(".js")
            || lower.contains(".jsx")
            || lower.contains(".mjs")
            || lower.contains(".cjs")
    })
}

/// The target files used to score path hints: a single file (hook / MCP
/// per-file paths) or a whole changeset (`recall --diff` / `fix`), where a
/// rule gets a path-hint boost when ANY changed path matches its patterns.
///
/// The single-file hot path (`PostToolUse` hook) keeps using `File`; the
/// changeset variant exists so diff-shaped callers stop collapsing a multi-file
/// diff onto one "primary" file and missing evidence paths on other files.
#[derive(Debug, Clone, Copy)]
pub enum TargetScope<'a> {
    /// One file path, matched exactly like the historical `target_file`.
    File(&'a str),
    /// Every changed path in a diff; ANY-path match brings a rule in scope.
    Changeset(&'a [String]),
}

impl TargetScope<'_> {
    /// Decide whether a chunk's `file_patterns` blob matches this target, with
    /// the retrieval-side over-recall error policy on both variants.
    pub(crate) fn pattern_allows(&self, file_patterns_json: Option<&str>) -> bool {
        match self {
            Self::File(file) => pattern_allows(file_patterns_json, file),
            Self::Changeset(paths) => {
                glob_match_changeset(file_patterns_json, paths, GlobErrorPolicy::OverRecall)
            }
        }
    }

    /// Language hint for the SQL-side pre-filter. A single file uses its
    /// extension (unchanged behaviour); a changeset only yields a language
    /// when every recognised path agrees — a mixed-language diff must not
    /// pre-drop rules tagged for the other language. Unrecognised
    /// extensions are ignored (those rules carry NULL language anyway).
    pub(crate) fn language_hint(&self) -> Option<String> {
        match self {
            Self::File(file) => super::detect_language_from_path(file),
            Self::Changeset(paths) => {
                let mut hint: Option<String> = None;
                for language in paths
                    .iter()
                    .filter_map(|path| super::detect_language_from_path(path))
                {
                    match &hint {
                        None => hint = Some(language),
                        Some(existing) if *existing == language => {}
                        Some(_) => return None,
                    }
                }
                hint
            }
        }
    }
}

fn has_explicit_file_patterns(file_patterns_json: Option<&str>) -> bool {
    let Some(raw) = file_patterns_json
        .map(str::trim)
        .filter(|raw| !raw.is_empty())
    else {
        return false;
    };
    match serde_json::from_str::<Vec<String>>(raw) {
        Ok(patterns) => patterns.iter().any(|pattern| !pattern.trim().is_empty()),
        Err(_) => true,
    }
}

fn path_hint_matches(scope: Option<TargetScope<'_>>, file_patterns_json: Option<&str>) -> bool {
    let Some(scope) = scope else {
        return false;
    };
    has_explicit_file_patterns(file_patterns_json) && scope.pattern_allows(file_patterns_json)
}

/// Retrieval options for confidence weighting, scoping, ANN usage, and
/// trajectory telemetry. The default matches plain retrieval: default top-k,
/// no confidence map, no target-scope cascade, no SQL metadata filter, ANN
/// enabled, no trajectory capture.
pub struct RetrievalOptions<'a> {
    pub top_k: Option<usize>,
    pub confidence_map: Option<&'a HashMap<String, f64>>,
    /// Optional allow-list applied before RRF fusion. Callers that already
    /// know the engine-eligible rule set can pass it here so disabled rules
    /// do not consume top-k score budget.
    pub eligible_skill_ids: Option<&'a HashSet<String>>,
    /// Per-skill age in days, used by the category-keyed half-life decay in
    /// `effective_confidence`. When `None` (or a chunk's `skill_id` is absent
    /// from the map) the scoring site uses `age_days = 0.0` — no decay.
    pub age_days_map: Option<&'a HashMap<String, f32>>,
    /// Target files for path hints: a single target file or a whole changeset.
    /// `None` disables the path-hint score boost.
    pub target_scope: Option<TargetScope<'a>>,
    /// When true, target scope becomes an eligibility filter for explicit
    /// non-matching file_patterns. Universal rules remain eligible.
    pub strict_file_scope: bool,
    pub filter: Option<&'a QueryFilter>,
    pub ann_enabled: bool,
    /// Use the local lexical hash for the query vector without calling the
    /// remote embedder. Latency-critical MCP/hook paths set this so retrieval
    /// never waits on network embedding in the host agent's main flow.
    pub local_query_embedding: bool,
    /// Optional provider-call budget for non-local query embedding. Ignored
    /// when `local_query_embedding` is true.
    pub embedding_timeout: Option<Duration>,
    /// When true, a query embed that fell back to lexical on a base-budget
    /// timeout is retried once with a longer cold-absorbing budget (see
    /// [`COLD_RETRY_EMBEDDING_TIMEOUT`]). Only the human-waiting CLI
    /// `recall`/`search` path sets this.
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
            target_scope: None,
            strict_file_scope: false,
            filter: None,
            ann_enabled: true,
            local_query_embedding: false,
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
/// `target_scope`: when present, applies a small path-hint score boost to
/// chunks whose `file_patterns` match the scope (a single file, or ANY path of
/// a changeset). Path hints are not a hard gate unless `strict_file_scope` is
/// enabled; by default, repo/language filters own eligibility while
/// semantic/lexical relevance owns ranking.
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
        target_scope,
        strict_file_scope,
        filter,
        ann_enabled,
        local_query_embedding,
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
    let embedded_query = if local_query_embedding {
        EmbeddedText {
            vector: embed_text(query),
            semantic: false,
        }
    } else {
        embed_query_aligned_to_index(index_pool, query, embedding_timeout, cold_start_retry).await
    };
    let query_emb = embedded_query.vector;

    // Switch the RRF weighting when the actual query vector is only the
    // local lexical hash, or when a provider failure disabled the vector
    // lane entirely. The hybrid (local hash + FTS5 BM25) outperforms FTS-only
    // because the local hash's bag-of-token overlap fills FTS5's
    // strict-tokenizer gap, even though the hash isn't semantic.
    let is_semantic = embedded_query.semantic;

    // SQL-level metadata pre-filter. When the filter is empty this reduces to
    // `SELECT *`, so it is zero-cost for unscoped callers.
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
    // Embedding deserialization is lazy on the ANN path: the HNSW graph holds
    // the vectors, so `IndexedRuleChunk::embedding` is dead weight there. Only
    // the linear cosine fallback reads it, so we load the full embedding blobs
    // up front exclusively when ANN is disabled; otherwise we re-query the full
    // rows only if the ANN lookup actually misses.
    let chunks = if ann_enabled {
        index_db::query_rule_chunks_no_embeddings(index_pool, filter).await?
    } else {
        index_db::query_rule_chunks(index_pool, filter).await?
    };
    let after_count: u32 = u32::try_from(chunks.len()).unwrap_or(u32::MAX);

    // FTS5 keyword baseline. Pull `k*4` raw hits so we have RRF material even
    // after the pattern cascade trims some out.
    let fts_limit = k.saturating_mul(4).min(200).max(k);
    let fts_hits = index_db::fts_search(index_pool, query, filter, fts_limit)
        .await
        .unwrap_or_default();

    let default_confidence = 0.7;
    let min_confidence = 0.2;

    // Path hints are ranking signals by default. Latency-critical automatic
    // injection paths can opt into strict scope filtering so an explicit
    // `*.py` edit does not receive a rule whose own patterns only cover
    // `*.rs` files. Universal rules (no file_patterns) still pass.
    let active: Vec<&IndexedRuleChunk> = chunks
        .iter()
        .filter(|chunk| {
            !strict_file_scope
                || target_scope
                    .is_none_or(|scope| scope.pattern_allows(chunk.file_patterns.as_deref()))
        })
        .collect();

    // Build a lookup table so FTS hits (identified by chunk id) can be
    // reconciled against the metadata-filtered active set.
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

    // The linear cosine fallback needs embeddings. On the ANN path we loaded
    // `chunks` without them, so when ANN misses we re-query the full embedded
    // rows once. `fallback_chunks` owns that re-loaded set so the borrows in
    // `emb_ranked` stay valid for the rest of the function.
    let fallback_chunks: Option<Vec<IndexedRuleChunk>> = if ann_result.is_none() && ann_enabled {
        Some(index_db::query_rule_chunks(index_pool, filter).await?)
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
        // Score against the embedded set: the re-queried `fallback_chunks` on
        // the ANN-disabled-or-missed path, or the already-embedded `active`
        // set when ANN was disabled from the start. Re-apply the same
        // strict-file-scope filter the `active` set used so the two paths
        // surface the same candidate population.
        let fallback: Vec<(&IndexedRuleChunk, f64)> = match fallback_chunks.as_ref() {
            Some(embedded) => embedded.iter().collect::<Vec<&IndexedRuleChunk>>(),
            None => active.clone(),
        }
        .into_iter()
        .filter(|chunk| {
            !strict_file_scope
                || target_scope
                    .is_none_or(|scope| scope.pattern_allows(chunk.file_patterns.as_deref()))
        })
        .filter_map(|c: &IndexedRuleChunk| {
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
            Some((c, f64::from(sim)))
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
    // FTS remains the precision anchor even when the query vector is semantic:
    // the vector lane should recover paraphrases, not let broad neighbours beat
    // explicit code/API tokens. Local lexical hashes are noisier, so they get a
    // smaller lane share.
    let (w_emb, w_fts) = rrf_lane_weights(is_semantic);

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

    // Final score = fused RRF score × quality signals. Confidence and
    // concreteness are deliberately stronger than the old near-tie-only
    // multiplier so a long generic chunk cannot win by raw text overlap alone.
    let mut scored: Vec<ScoredRuleChunk> = fused
        .into_values()
        .map(|(score, chunk, confidence)| {
            // Confidence tie-breaker plus a content-concreteness boost: rules
            // with no concrete code tokens (generic slogans) otherwise outrank
            // specific rules on lexical match alone. `kind`/`age_days` feed a
            // category-keyed half-life so an old style rule doesn't beat a
            // freshly ratified correction on confidence alone; age_days_map ==
            // None means age 0.0 (no decay).
            let kind = infer_rule_kind(&chunk.content);
            let age_days = age_days_map
                .and_then(|m| m.get(&chunk.skill_id).copied())
                .unwrap_or(0.0);
            let eff_conf = f64::from(effective_confidence(confidence as f32, &kind, age_days));
            let conf_weight = 0.3f64.mul_add(eff_conf.clamp(0.0, 1.0), 0.75);
            let quality_weight = rule_quality_weight(&chunk.content);
            let path_weight = if path_hint_matches(target_scope, chunk.file_patterns.as_deref()) {
                PATH_HINT_SCORE_BOOST
            } else {
                1.0
            };
            ScoredRuleChunk {
                skill_id: chunk.skill_id.clone(),
                content: chunk.content.clone(),
                score: score * conf_weight * quality_weight * path_weight,
                confidence,
            }
        })
        .collect();
    scored.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.skill_id.cmp(&b.skill_id))
    });

    // Adaptive top-K + noise floor. When the top score is itself in the noise
    // band AND there are many results (the "5 weak rules" pathology that
    // induces agent over-engineering), emit ZERO rules so the agent trusts its
    // training. Only safe for unsolicited injection (PreToolUse:Read hook),
    // which opts in via `adaptive_prune`; explicit user/MCP queries must always
    // return what's available even when scores are weak. Small result sets
    // bypass entirely.
    let adaptive_eligible = adaptive_prune && scored.len() >= 5;
    if let Some(top_score) = scored.first().map(|s| s.score) {
        if adaptive_eligible && top_score < ADAPTIVE_INJECT_THRESHOLD {
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

    // Memory-pipeline event: surfaces the ANN/embedding pass to local
    // activity consumers. Best-effort — never blocks recall.
    crate::observability::activity_stream::record(
        crate::observability::activity_stream::ActivityPayload::RetrievalEmbedding {
            hits: u32::try_from(scored.len()).unwrap_or(u32::MAX),
            took_ms: u64::try_from(retrieval_start.elapsed().as_millis()).unwrap_or(u64::MAX),
        },
    );

    Ok(scored)
}

const fn rrf_lane_weights(is_semantic: bool) -> (f64, f64) {
    if is_semantic {
        (SEMANTIC_EMBED_RRF_WEIGHT, SEMANTIC_FTS_RRF_WEIGHT)
    } else {
        (LOCAL_HASH_EMBED_RRF_WEIGHT, LOCAL_HASH_FTS_RRF_WEIGHT)
    }
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

/// Relevance gate for the EXPLICIT recall surfaces (MCP `search_rules`, CLI
/// `recall`). Runs on the FINAL, fully-reranked, sorted list — unlike the hook
/// path's in-retrieval pruning — because these paths add high-value signals
/// after fusion (exact-title-strict matches, the starter set, lexical-intent
/// boosts). Like the hook, a low-relevance query collapses to ZERO results so
/// the caller shows "no relevant memory" rather than filler.
///
/// Two gates, tuned never to suppress genuinely-strong matches:
///   1. Absolute floor — top hit below [`EXPLICIT_RECALL_MIN_RELEVANCE`] means
///      every result is noise: clear. A relevant top hit is boosted well above
///      this; a cascade-only/no-overlap hit stays in the raw RRF band.
///   2. Relative floor — drop tail below [`EXPLICIT_RECALL_RELATIVE_FLOOR`] of
///      the top hit. Looser than the hook's [`RELATIVE_RELEVANCE_FLOOR`]:
///      explicit queries keep more of a real result set.
///
/// Pure / in-place; caller must pass a list already sorted descending by score.
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
/// Topically-adjacent rules can clear the relevance floors while addressing a
/// different action/subject; this checks whether the rule's directive matches
/// the query intent, not just its topic. Biased toward fewer/zero results.
///
///   * All-weak query (no salient terms after stop-word filtering) → clear:
///     no signal to claim any match.
///   * A candidate is KEPT when it is either strongly scored (≥
///     [`INTENT_ALIGNMENT_EXEMPT_SCORE`], already intent-validated upstream) or
///     its directive is intent-aligned per [`directive_intent_aligned`].
///   * The topically-adjacent middle band is dropped.
///
/// The strong-score exemption guarantees genuinely-strong matches (and eval
/// self-recall hits) are never suppressed. Pure / in-place; order preserved.
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
    let project_root = crate::infra::db::current_project_root();
    let project_hash = crate::infra::db::project_hash_from_root(&project_root);

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

    #[test]
    fn semantic_rrf_keeps_fts_as_precision_anchor() {
        let (semantic_emb, semantic_fts) = rrf_lane_weights(true);
        let (local_emb, local_fts) = rrf_lane_weights(false);

        assert!(
            semantic_fts > semantic_emb,
            "semantic vectors may recover paraphrases, but FTS must remain the precision anchor"
        );
        assert!(
            semantic_emb > local_emb,
            "real semantic embeddings should carry more weight than local lexical hashes"
        );
        assert!(
            local_fts > semantic_fts,
            "local lexical hashes are noisier, so FTS should dominate more strongly"
        );
        assert!((semantic_emb + semantic_fts - 1.0).abs() < f64::EPSILON);
        assert!((local_emb + local_fts - 1.0).abs() < f64::EPSILON);
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
        let exempt_score = std::hint::black_box(INTENT_ALIGNMENT_EXEMPT_SCORE);
        let explicit_floor = std::hint::black_box(EXPLICIT_RECALL_MIN_RELEVANCE);
        let exact_title_floor = std::hint::black_box(2.0);
        let min_overlap = std::hint::black_box(MIN_INTENT_DIRECTIVE_OVERLAP);

        assert!(
            exempt_score > explicit_floor,
            "exemption ceiling must be above the explicit relevance floor"
        );
        assert!(
            exempt_score < exact_title_floor,
            "exemption ceiling must be below the exact-title-strict (2.0 + conf) band"
        );
        assert!(
            min_overlap >= 2,
            "a lone topical-anchor overlap must be insufficient"
        );
    }

    // -- TargetScope (single file vs changeset) --

    #[test]
    fn target_scope_changeset_matches_when_any_path_in_scope() {
        let patterns = Some(r#"["db/schema/**", "migrations/**/*.sql"]"#);
        let hit = vec!["src/api.ts".to_owned(), "db/schema/users.sql".to_owned()];
        assert!(TargetScope::Changeset(&hit).pattern_allows(patterns));
        let miss = vec!["src/api.ts".to_owned(), "README.md".to_owned()];
        assert!(!TargetScope::Changeset(&miss).pattern_allows(patterns));
        // File variant keeps the historical single-path behaviour.
        assert!(TargetScope::File("db/schema/users.sql").pattern_allows(patterns));
        assert!(!TargetScope::File("src/api.ts").pattern_allows(patterns));
        // Universal rules match either scope.
        assert!(TargetScope::Changeset(&miss).pattern_allows(None));
        assert!(TargetScope::File("anything.txt").pattern_allows(None));
    }

    #[test]
    fn target_scope_language_hint_requires_unanimous_changeset() {
        // Single file: extension-derived, as before.
        assert_eq!(
            TargetScope::File("src/main.rs").language_hint().as_deref(),
            Some("rust")
        );
        // Unanimous changeset (unrecognised extensions ignored).
        let rust_only = vec![
            "src/main.rs".to_owned(),
            "src/lib.rs".to_owned(),
            "README.md".to_owned(),
        ];
        assert_eq!(
            TargetScope::Changeset(&rust_only)
                .language_hint()
                .as_deref(),
            Some("rust")
        );
        // Mixed-language diff: no filter, so neither language's rules are
        // pre-dropped at SQL time.
        let mixed = vec!["src/main.rs".to_owned(), "web/app.ts".to_owned()];
        assert_eq!(TargetScope::Changeset(&mixed).language_hint(), None);
        // No recognised extension at all: no filter.
        let none = vec!["README.md".to_owned()];
        assert_eq!(TargetScope::Changeset(&none).language_hint(), None);
    }

    #[test]
    fn explicit_recall_floors_are_conservative_relative_to_in_retrieval_gates() {
        let explicit_relative_floor = std::hint::black_box(EXPLICIT_RECALL_RELATIVE_FLOOR);
        let retrieval_relative_floor = std::hint::black_box(RELATIVE_RELEVANCE_FLOOR);
        let explicit_min = std::hint::black_box(EXPLICIT_RECALL_MIN_RELEVANCE);
        let adaptive_threshold = std::hint::black_box(ADAPTIVE_INJECT_THRESHOLD);
        let min_relevance = std::hint::black_box(MIN_RELEVANCE_SCORE);

        assert!(
            explicit_relative_floor < retrieval_relative_floor,
            "explicit relative floor must be looser than the in-retrieval one"
        );
        assert!(
            explicit_min > adaptive_threshold,
            "explicit absolute floor must sit above the hook zero-inject threshold"
        );
        assert!(
            explicit_min > min_relevance,
            "explicit absolute floor must be stricter than the bare RRF noise floor"
        );
    }
}
