mod past_verdicts;
mod query_embed;
mod rule_bodies;
mod rules;
mod scoring;

pub use past_verdicts::{
    PastVerdictRecaller, merge_past_verdicts, retrieve_past_verdicts,
    retrieve_past_verdicts_by_text, retrieve_past_verdicts_by_text_with_team,
    retrieve_past_verdicts_with_team,
};
pub use rule_bodies::{RenderedRuleBody, RenderedRuleExample, render_full_rule_bodies};
pub use rules::{
    RetrievalOptions, TargetScope, apply_explicit_recall_threshold, apply_intent_alignment_gate,
    retrieve_rules, retrieve_rules_with_confidence,
};
pub use scoring::{RuleKind, effective_confidence, infer_rule_kind};

#[derive(Debug, Clone)]
pub struct ScoredRuleChunk {
    pub skill_id: String,
    pub content: String,
    pub score: f64,
    /// Confidence from the skill record (0.0-1.0). Used for display and ranking.
    pub confidence: f64,
}

fn compare_scored_rule_chunks(a: &ScoredRuleChunk, b: &ScoredRuleChunk) -> std::cmp::Ordering {
    b.score
        .total_cmp(&a.score)
        .then_with(|| a.skill_id.cmp(&b.skill_id))
}

/// Merge multiple groups of scored rule chunks into one ranked list.
///
/// De-dupes by `skill_id` (higher-scoring copy wins), sorts descending by
/// `score`, and truncates to `limit`. Callers should pass a single
/// repo/project scope, not a cross-project set.
pub fn merge_scored_rule_chunks(
    groups: impl IntoIterator<Item = Vec<ScoredRuleChunk>>,
    limit: usize,
) -> Vec<ScoredRuleChunk> {
    let mut by_skill_id: std::collections::HashMap<String, ScoredRuleChunk> =
        std::collections::HashMap::new();
    for group in groups {
        for chunk in group {
            match by_skill_id.get(&chunk.skill_id) {
                Some(existing) if existing.score >= chunk.score => {}
                _ => {
                    by_skill_id.insert(chunk.skill_id.clone(), chunk);
                }
            }
        }
    }
    let mut merged: Vec<_> = by_skill_id.into_values().collect();
    merged.sort_by(compare_scored_rule_chunks);
    merged.truncate(limit);
    merged
}

fn unique_repo_scopes(repo_scopes: &[String]) -> Vec<String> {
    let mut unique = Vec::new();
    for scope in repo_scopes {
        let scope = scope.trim().to_ascii_lowercase();
        if scope.is_empty() {
            continue;
        }
        if !unique.iter().any(|existing| existing == &scope) {
            unique.push(scope);
        }
    }
    unique
}

fn search_filter(
    target_scope: Option<TargetScope<'_>>,
    repo_scope: Option<&str>,
) -> crate::context::index_db::QueryFilter {
    crate::context::index_db::QueryFilter {
        language: target_scope.and_then(|scope| scope.language_hint()),
        repo_scope: repo_scope.map(String::from),
    }
}

fn rule_title(content: &str, fallback: &str) -> String {
    content
        .lines()
        .find_map(|line| line.strip_prefix("Rule Name:").map(|s| s.trim().to_owned()))
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| fallback.to_owned())
}

fn lexical_terms(query: &str) -> Vec<String> {
    const STOP_WORDS: &[&str] = &[
        "about", "after", "again", "against", "all", "and", "any", "are", "around", "because",
        "been", "before", "being", "between", "but", "can", "cannot", "could", "does", "doing",
        "done", "each", "for", "from", "had", "has", "have", "how", "into", "its", "more", "must",
        "our", "out", "over", "rule", "rules", "should", "than", "that", "the", "their", "then",
        "there", "these", "this", "those", "through", "use", "using", "was", "were", "what",
        "when", "where", "which", "while", "with", "without", "would", "you", "your",
    ];

    let mut terms = Vec::new();
    for term in query
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .map(str::trim)
        .filter(|term| term.len() >= 3)
    {
        let term = term.to_ascii_lowercase();
        if STOP_WORDS.contains(&term.as_str()) || terms.iter().any(|existing| existing == &term) {
            continue;
        }
        terms.push(term);
    }
    terms
}

fn normalized_query_key(query: &str) -> String {
    query
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .map(str::trim)
        .filter(|term| !term.is_empty())
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

fn retrieval_query_variants<'a>(query: &'a str, lexical_query: &'a str) -> Vec<&'a str> {
    let query = query.trim();
    let lexical_query = lexical_query.trim();
    let mut variants = Vec::with_capacity(2);
    if !query.is_empty() {
        variants.push(query);
    }

    let query_key = normalized_query_key(query);
    let lexical_key = normalized_query_key(lexical_query);
    if !lexical_query.is_empty() && !lexical_key.is_empty() && lexical_key != query_key {
        variants.push(lexical_query);
    }

    variants
}

fn lexical_boost(chunk: &ScoredRuleChunk, terms: &[String]) -> f64 {
    if terms.is_empty() {
        return 0.0;
    }

    let title = rule_title(&chunk.content, &chunk.skill_id).to_ascii_lowercase();
    let content = chunk.content.to_ascii_lowercase();
    let mut title_hits = 0usize;
    let mut content_hits = 0usize;

    for term in terms {
        if title.contains(term) {
            title_hits += 1;
        }
        if content.contains(term) {
            content_hits += 1;
        }
    }

    let total = terms.len() as f64;
    let title_ratio = title_hits as f64 / total;
    let content_ratio = content_hits as f64 / total;
    let mut boost = 0.24f64.mul_add(title_ratio, 0.08 * content_ratio);
    if title_hits >= 2 {
        boost += 0.12;
    }
    if title_hits >= terms.len().min(3) {
        boost += 0.08;
    }
    boost.min(0.45)
}

pub fn rerank_scored_rule_chunks_by_lexical_query(
    mut chunks: Vec<ScoredRuleChunk>,
    lexical_query: &str,
    limit: usize,
) -> Vec<ScoredRuleChunk> {
    let terms = lexical_terms(lexical_query);
    for chunk in &mut chunks {
        chunk.score += lexical_boost(chunk, &terms);
    }

    chunks.sort_by(compare_scored_rule_chunks);
    chunks.truncate(limit);
    chunks
}

/// Options for the CLI/MCP search-style retrieval helper, which fans out
/// across repo scopes, merges duplicates, then applies a lexical re-rank.
pub struct RuleSearchRetrievalOptions<'a> {
    pub query: &'a str,
    pub lexical_query: &'a str,
    pub top_k: usize,
    pub confidence_map: Option<&'a std::collections::HashMap<String, f64>>,
    pub age_days_map: Option<&'a std::collections::HashMap<String, f32>>,
    /// Strict-cascade scope: one file, or the whole changeset for diff-shaped
    /// callers (`recall --diff` / `fix`). Also drives the SQL language filter.
    pub target_scope: Option<TargetScope<'a>>,
    pub repo_scopes: &'a [String],
    pub ann_enabled: bool,
    pub embedding_timeout: Option<std::time::Duration>,
    /// Retry a timed-out query embed once with a longer cold-absorbing budget
    /// (see `embed_query_aligned_to_index`). Set by the human-waiting CLI path
    /// so a cold first recall keeps semantic ranking; left `false` by the
    /// latency-critical hook/MCP callers that must fast-degrade to lexical.
    pub cold_start_retry: bool,
    pub adaptive_prune: bool,
}

/// Canonical multi-scope rule fan-out shared by the search path and the
/// orchestrator. Both dedup+cap repo scopes, clamp `top_k`, derive the
/// per-scope SQL filter, fan out across scope × query-variant, merge by
/// best-score dedup, then lexical re-rank and truncate; they differ only
/// in inputs (search expands an intent query-variant lane and passes no
/// eligible-skill set; the orchestrator passes one query plus an
/// `eligible_skill_ids` allow-list).
pub(crate) struct RuleFanoutQuery<'a> {
    /// Primary query string (path + intent, or just intent).
    pub query: &'a str,
    /// Lexical lane used for the final re-rank and for deciding whether
    /// to add a second (intent-only) retrieval variant. Pass the same
    /// string as `query` to collapse to a single-variant fan-out.
    pub lexical_query: &'a str,
    pub top_k: usize,
    pub confidence_map: Option<&'a std::collections::HashMap<String, f64>>,
    pub eligible_skill_ids: Option<&'a std::collections::HashSet<String>>,
    pub age_days_map: Option<&'a std::collections::HashMap<String, f32>>,
    pub target_scope: Option<TargetScope<'a>>,
    pub repo_scopes: &'a [String],
    pub ann_enabled: bool,
    pub embedding_timeout: Option<std::time::Duration>,
    /// See [`RuleSearchRetrievalOptions::cold_start_retry`]. Forwarded to the
    /// per-scope/per-variant `RetrievalOptions` so every concurrent embed in
    /// the fan-out honours the same cold-start policy.
    pub cold_start_retry: bool,
    pub adaptive_prune: bool,
}

pub(crate) async fn retrieve_rules_fanout(
    index_pool: &crate::SqlitePool,
    query: RuleFanoutQuery<'_>,
) -> Result<Vec<ScoredRuleChunk>, crate::CoreError> {
    let RuleFanoutQuery {
        query,
        lexical_query,
        top_k,
        confidence_map,
        eligible_skill_ids,
        age_days_map,
        target_scope,
        repo_scopes,
        ann_enabled,
        embedding_timeout,
        cold_start_retry,
        adaptive_prune,
    } = query;

    if top_k == 0 {
        return Ok(Vec::new());
    }
    let top_k = top_k.min(50);
    let repo_scopes: Vec<String> = unique_repo_scopes(repo_scopes)
        .into_iter()
        .take(4)
        .collect();
    let candidate_limit = top_k.saturating_mul(5).clamp(top_k, 50);
    // A `None` filter retrieves the whole per-project index, which is safe
    // because the index is the scope boundary: it only holds rules copied in
    // for the current project's scopes (see `filter_rules_for_repo_scopes`,
    // which copies nothing when there is no scope). With detected scopes,
    // narrow further per scope.
    let scope_filters: Vec<Option<String>> = if repo_scopes.is_empty() {
        vec![None]
    } else {
        repo_scopes.into_iter().map(Some).collect()
    };

    let query_variants = retrieval_query_variants(query, lexical_query);
    let mut retrievals = Vec::with_capacity(scope_filters.len() * query_variants.len());
    for repo_scope in &scope_filters {
        for query_variant in &query_variants {
            let filter = search_filter(target_scope, repo_scope.as_deref());
            retrievals.push(async move {
                retrieve_rules_with_confidence(
                    index_pool,
                    query_variant,
                    RetrievalOptions {
                        top_k: Some(candidate_limit),
                        confidence_map,
                        eligible_skill_ids,
                        age_days_map,
                        target_scope,
                        filter: Some(&filter),
                        ann_enabled,
                        embedding_timeout,
                        cold_start_retry,
                        adaptive_prune,
                        ..Default::default()
                    },
                )
                .await
            });
        }
    }
    let mut groups = Vec::with_capacity(retrievals.len());
    for group in futures_util::future::join_all(retrievals).await {
        groups.push(group?);
    }

    let merged = merge_scored_rule_chunks(groups, candidate_limit);
    Ok(rerank_scored_rule_chunks_by_lexical_query(
        merged,
        lexical_query,
        top_k,
    ))
}

pub async fn retrieve_rules_for_search(
    index_pool: &crate::SqlitePool,
    options: RuleSearchRetrievalOptions<'_>,
) -> Result<Vec<ScoredRuleChunk>, crate::CoreError> {
    let RuleSearchRetrievalOptions {
        query,
        lexical_query,
        top_k,
        confidence_map,
        age_days_map,
        target_scope,
        repo_scopes,
        ann_enabled,
        embedding_timeout,
        cold_start_retry,
        adaptive_prune,
    } = options;

    retrieve_rules_fanout(
        index_pool,
        RuleFanoutQuery {
            query,
            lexical_query,
            top_k,
            confidence_map,
            // The search path never constrains to an engine-eligible
            // allow-list; callers filter afterwards.
            eligible_skill_ids: None,
            age_days_map,
            target_scope,
            repo_scopes,
            ann_enabled,
            embedding_timeout,
            cold_start_retry,
            adaptive_prune,
        },
    )
    .await
}

/// Reciprocal Rank Fusion constant. Standard value from the original
/// Cormack-Clarke-Buettcher paper; 60 is a robust default that makes
/// lower-ranked but co-occurring results surface reliably.
const RRF_K: f64 = 60.0;

/// Map a file path (or bare filename) to a canonical language tag matching
/// the spelling used in skill tags, so `QueryFilter.language` round-trips
/// between the MCP caller and the indexed chunk metadata. Unknown extensions
/// return `None` (treated as "no language filter") rather than guessing a
/// language that would drop real hits.
pub fn detect_language_from_path(path: &str) -> Option<String> {
    let lower = path.to_ascii_lowercase();
    // Match on the last dotted suffix so compound names like `foo.d.ts`
    // collapse to `ts`.
    let ext = lower.rsplit('.').next()?;
    Some(
        match ext {
            "rs" => "rust",
            "ts" | "tsx" => "typescript",
            "js" | "jsx" | "mjs" | "cjs" => "javascript",
            "py" | "pyi" => "python",
            "go" => "go",
            "java" => "java",
            "kt" | "kts" => "kotlin",
            "swift" => "swift",
            "rb" => "ruby",
            "php" => "php",
            "cpp" | "cc" | "cxx" | "hpp" | "hh" => "cpp",
            "c" | "h" => "c",
            "cs" => "csharp",
            _ => return None,
        }
        .to_owned(),
    )
}

/// Count concreteness signals in a rule's content. Used to boost
/// concrete rules over slogan rules at ranking time.
///
/// Looks for backticked tokens (`useQuery`), path-like fragments
/// (`packages/router-core/`), and version literals (`v1.2`,
/// `Node 20.11`). Each kind capped at 3 hits so a giant code-fence rule
/// doesn't run away. Total saturated at 6 in the caller.
fn concreteness_score(content: &str) -> usize {
    let mut score = 0usize;
    let backticks = content.matches('`').count() / 2; // each token wraps in two backticks
    score += backticks.min(3);
    // Path-like fragments: at least one slash-separated word with a dot
    // extension (foo/bar.ts), or a dotted package name (`a.b.c`).
    let path_like = content
        .split_whitespace()
        .filter(|w| {
            w.contains('/')
                && w.split('/')
                    .next_back()
                    .is_some_and(|tail| tail.contains('.') && tail.len() > 3)
        })
        .count();
    score += path_like.min(3);
    // Version-ish: any `vN.N` or `N.N.N` substring.
    let version_like = content
        .split_whitespace()
        .filter(|w| {
            let trimmed = w.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '.');
            trimmed.starts_with('v')
                && trimmed.len() > 2
                && trimmed[1..]
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_digit())
                || trimmed
                    .split('.')
                    .filter(|s| s.parse::<u32>().is_ok())
                    .count()
                    >= 2
        })
        .count();
    score += version_like.min(2);
    score
}

/// Absolute floor for `ScoredRuleChunk.score`. Anything at or below
/// this is RRF rounding noise — usually a chunk that was admitted via
/// the file-pattern cascade safety net but has zero lexical or
/// semantic overlap with the query. Keeping these in the result set
/// burns agent tokens for negative value.
const MIN_RELEVANCE_SCORE: f64 = 0.001;

/// Adaptive top-K injection threshold. When the top-ranked rule's score is
/// below this, return zero rules instead of padding to k — weak rules on
/// simple tasks distract more than they help. RRF with k=60 yields a top-1
/// score around 0.008-0.015 for strong matches and 0.001-0.003 for
/// cascade-only tail; this 0.005 cut sits in the gap.
const ADAPTIVE_INJECT_THRESHOLD: f64 = 0.005;

/// Relative floor — drop tail rules whose score is less than this fraction
/// of the top-ranked rule's score. Catches pathological flat distributions
/// where many rules cluster within a few percent of each other and the
/// agent can't tell signal from noise. Keeps any rule worth ~1/3 of the
/// leader.
const RELATIVE_RELEVANCE_FLOOR: f64 = 0.35;

/// Absolute relevance floor for the explicit recall surfaces (`search_rules`
/// tool + CLI `recall`), applied by [`rules::apply_explicit_recall_threshold`]
/// on the final re-ranked score. When even the top hit is below this, the
/// whole result set is treated as noise and explicit recall returns nothing.
///
/// After the lexical-intent re-rank a relevant top hit sits well above 0.1
/// (exact-title-strict / starter hits at `2.0 + conf`), so strong matches are
/// never suppressed. A cascade-only / no-intent-overlap top hit gets no
/// lexical boost and stays in the raw fused RRF band (~0.001-0.005), below
/// this floor. The value sits in the gap, 2x the hook's
/// `ADAPTIVE_INJECT_THRESHOLD`.
const EXPLICIT_RECALL_MIN_RELEVANCE: f64 = 0.01;

/// Relative tail floor for the explicit recall gate — drop results below this
/// fraction of the surviving top hit. Looser than the in-retrieval
/// `RELATIVE_RELEVANCE_FLOOR` (0.35): an explicit user query keeps more of a
/// genuine result set, shedding only the clearly irrelevant tail (worth <1/5
/// of the leader).
const EXPLICIT_RECALL_RELATIVE_FLOOR: f64 = 0.20;

/// Minimum count of distinct salient (non-stop-word) query terms that a
/// candidate rule's directive (its `Rule Name:` title + leading body) must
/// share with the query intent to be considered intent-aligned.
///
/// Hybrid retrieval and the relative floors admit topically-adjacent rules
/// that address a different action/subject (e.g. a "return false vs panic"
/// directive pulling in "panic-message wording" rules via the shared anchor
/// `panic`). The bar is 2 so a lone topical-anchor overlap is insufficient
/// while an on-subject rule sharing the verb/object pair clears it.
/// Self-recall shares nearly every directive term and clears it easily.
///
/// This is necessary-but-not-sufficient: the absolute path also requires the
/// shared set to contain at least [`MIN_DISTINCTIVE_SHARED_TERMS`] non-generic
/// term, since two generic anchors alone could otherwise pass.
const MIN_INTENT_DIRECTIVE_OVERLAP: usize = 2;

/// Alternate (ratio) path to intent alignment: a candidate also passes when
/// the shared distinctive terms cover at least this fraction of the query's
/// salient terms, even if the absolute count is below
/// [`MIN_INTENT_DIRECTIVE_OVERLAP`]. Keeps short, sharp queries from
/// over-pruning (e.g. a two-word intent whose seeded rules each match one
/// term). Raising it to 0.6 regressed that fan-out; precision instead comes
/// from the [`MIN_DISTINCTIVE_SHARED_TERMS`] gate, which rejects a lone
/// generic anchor before this ratio is consulted.
const MIN_INTENT_DIRECTIVE_OVERLAP_RATIO: f64 = 0.5;

/// Minimum number of shared terms that are distinctive — not in the generic
/// topical/code-anchor set ([`is_generic_anchor`]) — required for a concern
/// match. Guards both the absolute-overlap and ratio paths.
///
/// Raw term overlap can't distinguish "shares the subject" from "name-drops
/// the same topic word": an all-anchor pair (`panic` + `test`) scores >= 2
/// just like a real verb/object pair. Requiring one distinctive shared term
/// forces the overlap to include a specific subject/action token (`false`,
/// `invalid`, `hijack`, `yaml`), so a match made only of generic anchors fails
/// regardless of count or ratio. Keyed off the shared set rather than a
/// rule-side coverage ratio, so it is robust to varied directive phrasing.
const MIN_DISTINCTIVE_SHARED_TERMS: usize = 1;

/// Score ceiling above which a candidate is exempt from the intent-alignment
/// gate and kept regardless of directive overlap. Candidates here earned
/// their place through a strong post-fusion signal (exact-title-strict
/// `2.0 + conf`, the cross-repo starter set, or the lexical-intent re-rank
/// boost), so the gate must not second-guess them. Sits above the boosted
/// strong-match RRF band (~0.1-0.45) but below the exact-title-strict floor,
/// exempting unambiguous winners while still scrutinising the
/// topically-adjacent middle band.
const INTENT_ALIGNMENT_EXEMPT_SCORE: f64 = 0.6;

#[cfg(test)]
mod tests {
    use super::rules::pattern_allows;
    use super::*;
    use crate::context::index_db::{QueryFilter, open_pool_at, upsert_rule_chunks};
    use crate::context::rule_source::RuleDocument;
    use crate::context::types::{PastVerdict, PastVerdictScope};
    use crate::contract::RecallPastVerdictsRequest;
    use crate::error::CoreError;
    use crate::observability::trajectory::{TrajectoryBuilder, TrajectoryStep};
    use async_trait::async_trait;
    use tempfile::TempDir;

    #[test]
    fn pattern_allows_table() {
        // Each row: (pattern_json, path, expected). Covers null/empty universal
        // pass-through, single glob, directory scope, Windows path
        // normalisation, and malformed-JSON over-recall.
        let cases: &[(Option<&str>, &str, bool)] = &[
            (None, "tokio/src/io/uring.rs", true),
            (Some(""), "tokio/src/io/uring.rs", true),
            (Some("[]"), "tokio/src/io/uring.rs", true),
            (Some(r#"["**/*.rs"]"#), "tokio/src/io/uring.rs", true),
            (Some(r#"["**/*.rs"]"#), ".github/workflows/ci.yml", false),
            (
                Some(r#"["tokio/src/io/**"]"#),
                "tokio/src/io/uring.rs",
                true,
            ),
            (
                Some(r#"["tokio/src/io/**"]"#),
                "tokio/src/runtime/mod.rs",
                false,
            ),
            (
                Some(r#"["tokio/src/io/**"]"#),
                "tokio\\src\\io\\uring.rs",
                true,
            ),
            (
                Some(r#"["tokio/src/io/**"]"#),
                "/tokio/src/io/uring.rs",
                true,
            ),
            // Invalid JSON shouldn't silently drop a rule — better to over-recall
            // than to lose signal on a parse error.
            (Some("not-json"), "any/path.rs", true),
            (Some("{}"), "any/path.rs", true),
        ];
        for (pat, path, expected) in cases {
            assert_eq!(
                pattern_allows(*pat, path),
                *expected,
                "pat={pat:?} path={path}"
            );
        }
    }

    // -- detect_language_from_path tests --

    #[test]
    fn detect_language_from_path_covers_common_extensions() {
        assert_eq!(
            detect_language_from_path("src/main.rs").as_deref(),
            Some("rust")
        );
        assert_eq!(
            detect_language_from_path("apps/web/index.tsx").as_deref(),
            Some("typescript")
        );
        assert_eq!(
            detect_language_from_path("scripts/build.py").as_deref(),
            Some("python")
        );
        assert_eq!(
            detect_language_from_path("api/handler.go").as_deref(),
            Some("go")
        );
    }

    #[test]
    fn detect_language_from_path_returns_none_for_unknown_ext() {
        assert!(detect_language_from_path("README.md").is_none());
        assert!(detect_language_from_path("no_extension").is_none());
    }

    #[test]
    fn shared_search_repo_scopes_are_case_insensitive() {
        assert_eq!(
            unique_repo_scopes(&[
                "Difflore-Fixtures/Vite".to_owned(),
                " ".to_owned(),
                "difflore-fixtures/vite".to_owned(),
                "ViteJS/Vite".to_owned(),
            ]),
            vec![
                "difflore-fixtures/vite".to_owned(),
                "vitejs/vite".to_owned()
            ]
        );
    }

    // -- Past verdict recall tests --

    struct ErroringRecaller;

    #[async_trait]
    impl PastVerdictRecaller for ErroringRecaller {
        async fn recall(
            &self,
            _req: RecallPastVerdictsRequest,
        ) -> Result<Vec<PastVerdict>, CoreError> {
            Err(CoreError::Internal("simulated failure".into()))
        }
    }

    struct StaticRecaller(Vec<PastVerdict>);

    #[async_trait]
    impl PastVerdictRecaller for StaticRecaller {
        async fn recall(
            &self,
            _req: RecallPastVerdictsRequest,
        ) -> Result<Vec<PastVerdict>, CoreError> {
            Ok(self.0.clone())
        }
    }

    struct RecordingRecaller(tokio::sync::Mutex<Option<RecallPastVerdictsRequest>>);

    #[async_trait]
    impl PastVerdictRecaller for RecordingRecaller {
        async fn recall(
            &self,
            req: RecallPastVerdictsRequest,
        ) -> Result<Vec<PastVerdict>, CoreError> {
            *self.0.lock().await = Some(req);
            Ok(Vec::new())
        }
    }

    fn verdict(id: &str, status: &str) -> PastVerdict {
        PastVerdict {
            extraction_id: id.to_owned(),
            code_snippet: format!("snippet for {id}"),
            issue_text: format!("issue for {id}"),
            status: status.to_owned(),
            reason: Some(format!("reason-{id}")),
            similarity: 0.87,
            created_at: "2026-04-10T00:00:00Z".to_owned(),
            signature: None,
            source_pr_number: None,
            source_pr_title: None,
            source_pr_url: None,
        }
    }

    fn scored(id: &str, score: f64) -> ScoredRuleChunk {
        ScoredRuleChunk {
            skill_id: id.to_owned(),
            content: format!("Rule ID: {id}\nRule Name: {id}\n\nbody"),
            score,
            confidence: 0.7,
        }
    }

    fn embedding_blob(embedding: &[f32]) -> Vec<u8> {
        embedding
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect()
    }

    #[test]
    fn merge_scored_rule_chunks_tie_breaks_by_skill_id() {
        let merged = merge_scored_rule_chunks(
            vec![vec![scored("rule-b", 0.5)], vec![scored("rule-a", 0.5)]],
            2,
        );
        let ids: Vec<_> = merged.iter().map(|r| r.skill_id.as_str()).collect();
        assert_eq!(ids, vec!["rule-a", "rule-b"]);
    }

    #[test]
    fn rerank_scored_rule_chunks_tie_breaks_by_skill_id() {
        let ranked = rerank_scored_rule_chunks_by_lexical_query(
            vec![scored("rule-b", 0.5), scored("rule-a", 0.5)],
            "",
            2,
        );
        let ids: Vec<_> = ranked.iter().map(|r| r.skill_id.as_str()).collect();
        assert_eq!(ids, vec!["rule-a", "rule-b"]);
    }

    #[test]
    fn retrieval_query_variants_adds_intent_lane_when_file_query_differs() {
        assert_eq!(
            retrieval_query_variants(
                "src/context.go Bind handlers must check returned error",
                "Bind handlers must check returned error",
            ),
            vec![
                "src/context.go Bind handlers must check returned error",
                "Bind handlers must check returned error",
            ],
        );
        assert_eq!(
            retrieval_query_variants("Bind handlers", "bind handlers"),
            vec!["Bind handlers"],
        );
        assert_eq!(retrieval_query_variants("", "please"), vec!["please"]);
    }

    #[tokio::test]
    async fn retrieve_rules_for_search_uses_intent_lane_to_escape_path_noise() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("idx.db");
        let pool = open_pool_at(&path).await.unwrap();
        let repo = "gin-gonic/gin";
        let mut rules = Vec::new();
        for i in 0..8 {
            let mut rule = rule_doc(
                &format!("path-noise-{i}"),
                "context go context go context go path-only convention",
                Some("go"),
                Some(repo),
            );
            rule.file_patterns = Some(r#"["**/*.go"]"#.to_owned());
            rules.push(rule);
        }
        let mut signal = rule_doc(
            "bind-error",
            "Bind handlers must check returned error before continuing",
            Some("go"),
            Some(repo),
        );
        signal.file_patterns = Some(r#"["**/*.go"]"#.to_owned());
        rules.push(signal);
        upsert_rule_chunks(&pool, &rules).await.unwrap();

        let hits = retrieve_rules_for_search(
            &pool,
            RuleSearchRetrievalOptions {
                query: "src/context.go",
                lexical_query: "Bind handlers must check returned error",
                top_k: 1,
                confidence_map: None,
                age_days_map: None,
                target_scope: Some(TargetScope::File("src/context.go")),
                repo_scopes: &[repo.to_owned()],
                ann_enabled: false,
                embedding_timeout: Some(std::time::Duration::from_millis(2500)),
                cold_start_retry: false,
                adaptive_prune: false,
            },
        )
        .await
        .unwrap();

        assert_eq!(
            hits.first().map(|hit| hit.skill_id.as_str()),
            Some("bind-error")
        );
    }

    #[tokio::test]
    async fn retrieve_rules_for_search_without_repo_scopes_uses_project_index() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("idx.db");
        let pool = open_pool_at(&path).await.unwrap();
        let rules = vec![rule_doc(
            "signal",
            "Avoid unwrap in request handlers; return structured errors",
            Some("rust"),
            Some("acme/widgets"),
        )];
        upsert_rule_chunks(&pool, &rules).await.unwrap();

        let hits = retrieve_rules_for_search(
            &pool,
            RuleSearchRetrievalOptions {
                query: "src/http/handler.rs Avoid unwrap in request handlers",
                lexical_query: "Avoid unwrap in request handlers",
                top_k: 1,
                confidence_map: None,
                age_days_map: None,
                target_scope: Some(TargetScope::File("src/http/handler.rs")),
                repo_scopes: &[],
                ann_enabled: false,
                embedding_timeout: Some(std::time::Duration::from_millis(2500)),
                cold_start_retry: false,
                adaptive_prune: false,
            },
        )
        .await
        .unwrap();

        assert_eq!(
            hits.first().map(|hit| hit.skill_id.as_str()),
            Some("signal")
        );
    }

    #[test]
    fn merge_past_verdicts_tie_breaks_by_extraction_id() {
        let merged = merge_past_verdicts(
            vec![
                vec![verdict("verdict-b", "approved")],
                vec![verdict("verdict-a", "approved")],
            ],
            2,
        );
        let ids: Vec<_> = merged.iter().map(|v| v.extraction_id.as_str()).collect();
        assert_eq!(ids, vec!["verdict-a", "verdict-b"]);
    }

    #[tokio::test]
    async fn test_retrieve_past_verdicts_returns_empty_on_error() {
        let recaller = ErroringRecaller;
        let emb = vec![0.1f32; 8];
        let out = retrieve_past_verdicts(
            &recaller,
            &emb,
            Some("repo-1"),
            PastVerdictScope::Team,
            5,
            None,
        )
        .await;
        assert!(
            out.is_empty(),
            "errors must be downgraded to an empty Vec, got {} items",
            out.len()
        );
    }

    #[tokio::test]
    async fn test_retrieve_past_verdicts_forwards_rows_on_success() {
        let recaller = StaticRecaller(vec![verdict("e1", "approved"), verdict("e2", "rejected")]);
        let emb = vec![0.0f32; 4];
        let out =
            retrieve_past_verdicts(&recaller, &emb, None, PastVerdictScope::Personal, 3, None)
                .await;
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].extraction_id, "e1");
        assert_eq!(out[1].status, "rejected");
    }

    #[tokio::test]
    async fn text_past_verdict_recall_forwards_team_scope() {
        let recaller = RecordingRecaller(tokio::sync::Mutex::new(None));

        let _ = retrieve_past_verdicts_by_text_with_team(
            &recaller,
            "router cache invalidation",
            Some("acme/widgets"),
            PastVerdictScope::Team,
            7,
            Some("src/router.ts"),
            Some("team-1"),
        )
        .await;

        let req = recaller.0.lock().await.clone().expect("request captured");
        assert_eq!(req.scope, "team");
        assert_eq!(req.team_id.as_deref(), Some("team-1"));
        assert_eq!(req.repo_id.as_deref(), Some("acme/widgets"));
        assert_eq!(req.target_file.as_deref(), Some("src/router.ts"));
        assert_eq!(req.k, 7);
    }

    #[tokio::test]
    async fn embedding_past_verdict_recall_forwards_team_scope() {
        let recaller = RecordingRecaller(tokio::sync::Mutex::new(None));
        let embedding = vec![0.25, 0.5, 0.75];

        let _ = retrieve_past_verdicts_with_team(
            &recaller,
            &embedding,
            Some("acme/widgets"),
            PastVerdictScope::Team,
            4,
            Some("src/router.ts"),
            Some("team-1"),
        )
        .await;

        let req = recaller.0.lock().await.clone().expect("request captured");
        assert_eq!(req.scope, "team");
        assert_eq!(req.team_id.as_deref(), Some("team-1"));
        assert_eq!(req.repo_id.as_deref(), Some("acme/widgets"));
        assert_eq!(req.target_file.as_deref(), Some("src/router.ts"));
        assert_eq!(req.embedding, embedding);
        assert_eq!(req.query_text, None);
        assert_eq!(req.k, 4);
    }

    // -- Hybrid retrieval tests --

    fn rule_doc(
        id: &str,
        content: &str,
        language: Option<&str>,
        repo_scope: Option<&str>,
    ) -> RuleDocument {
        RuleDocument {
            skill_id: id.to_owned(),
            title: id.to_owned(),
            content: content.to_owned(),
            confidence: 0.7,
            file_patterns: None,
            language: language.map(String::from),
            repo_scope: repo_scope.map(String::from),
        }
    }

    #[tokio::test]
    async fn rrf_fusion_prefers_results_ranked_high_by_both() {
        // A chunk that matches BOTH the keyword query and the semantic
        // embedding should outrank a chunk that matches only one path.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("idx.db");
        let pool = open_pool_at(&path).await.unwrap();

        let rules = vec![
            // Rule A — contains the query token AND lots of co-occurring
            // words that also skew the bag-of-words embedding.
            rule_doc(
                "A",
                "prefer structured_logging for observability when emitting structured_logging events",
                None,
                None,
            ),
            // Rule B — token-only hit.
            rule_doc(
                "B",
                "avoid structured_logging in tests; use a stub logger instead",
                None,
                None,
            ),
            // Rule C — no token hit, unrelated content.
            rule_doc(
                "C",
                "always write unit tests for every public api",
                None,
                None,
            ),
        ];
        upsert_rule_chunks(&pool, &rules).await.unwrap();

        let mut tb = TrajectoryBuilder::new();
        let hits = retrieve_rules_with_confidence(
            &pool,
            "structured_logging observability",
            RetrievalOptions {
                top_k: Some(3),
                trajectory: Some(&mut tb),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        // A should rank first because it wins both the FTS path (token
        // appears twice) and the bag-of-words embedding (token has the
        // highest contribution to the summed vector).
        assert!(!hits.is_empty());
        assert_eq!(hits[0].skill_id, "A", "A should RRF-win over B and C");

        // Verify the HybridFusion step was emitted.
        let has_fusion = tb
            .steps()
            .iter()
            .any(|s| matches!(s, TrajectoryStep::HybridFusion { .. }));
        assert!(has_fusion, "HybridFusion trajectory step must fire");
    }

    #[tokio::test]
    async fn sha1_embedder_path_weights_fts_higher() {
        // With the default (offline) SHA1 embedder is_semantic() = false
        // → the RRF weights shift to (0.2 emb, 0.8 fts). A token-only
        // match (no embedding overlap) must still rank before a
        // semantic-adjacent-but-token-absent rule.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("idx.db");
        let pool = open_pool_at(&path).await.unwrap();

        let rules = vec![
            // Exact rare-token hit, no overlap with the other rules.
            rule_doc(
                "keyword",
                "do not shadow with deprecated_zzz_api in request handlers",
                None,
                None,
            ),
            // Semantically-adjacent content but NO token overlap with the query.
            rule_doc(
                "semantic",
                "request handlers should use async primitives carefully",
                None,
                None,
            ),
        ];
        upsert_rule_chunks(&pool, &rules).await.unwrap();

        let hits = retrieve_rules_with_confidence(
            &pool,
            "deprecated_zzz_api",
            RetrievalOptions {
                top_k: Some(2),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert!(!hits.is_empty());
        assert_eq!(
            hits[0].skill_id, "keyword",
            "under SHA1 embedder, FTS hit should win over a generic semantic neighbour"
        );
    }

    #[tokio::test]
    async fn linear_scan_excludes_mismatched_embedding_dims() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("idx.db");
        let pool = open_pool_at(&path).await.unwrap();
        let query = "dim_mismatch_probe";
        let query_emb = crate::context::embedding::embed_text(query);
        let stale_embedding = vec![query_emb[0], query_emb[1]];
        let stale_blob = embedding_blob(&stale_embedding);

        sqlx::query(
            "INSERT INTO rule_chunks (id, skill_id, content, embedding, file_patterns, language, repo_scope)
             VALUES (?1, ?2, ?3, ?4, NULL, NULL, NULL)",
        )
        .bind("rule-stale")
        .bind("stale")
        .bind("unrelated content that should not match the query lexically")
        .bind(stale_blob)
        .execute(&pool)
        .await
        .unwrap();

        let hits = retrieve_rules_with_confidence(
            &pool,
            query,
            RetrievalOptions {
                top_k: Some(5),
                ann_enabled: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert!(
            hits.is_empty(),
            "stale chunks from a different embedding dim must not enter linear cosine ranking"
        );
    }

    #[tokio::test]
    async fn strict_cascade_does_not_fallback_to_foreign_file_patterns() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("idx.db");
        let pool = open_pool_at(&path).await.unwrap();
        let mut foreign = rule_doc(
            "foreign",
            "python request handlers should avoid sync database calls",
            Some("python"),
            Some("acme/widgets"),
        );
        foreign.file_patterns = Some(r#"["**/*.py"]"#.to_owned());
        upsert_rule_chunks(&pool, &[foreign]).await.unwrap();

        let filter = QueryFilter {
            language: None,
            repo_scope: Some("acme/widgets".to_owned()),
        };
        let hits = retrieve_rules_with_confidence(
            &pool,
            "request handlers database",
            RetrievalOptions {
                top_k: Some(5),
                target_scope: Some(TargetScope::File("src/server.rs")),
                filter: Some(&filter),
                ann_enabled: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert!(
            hits.is_empty(),
            "explicit **/*.py rule must not be recalled for src/server.rs"
        );
    }

    #[tokio::test]
    async fn strict_cascade_keeps_universal_rules_for_target_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("idx.db");
        let pool = open_pool_at(&path).await.unwrap();
        upsert_rule_chunks(
            &pool,
            &[rule_doc(
                "universal",
                "request handlers should return structured errors",
                None,
                Some("acme/widgets"),
            )],
        )
        .await
        .unwrap();

        let filter = QueryFilter {
            language: None,
            repo_scope: Some("acme/widgets".to_owned()),
        };
        let hits = retrieve_rules_with_confidence(
            &pool,
            "request handlers structured errors",
            RetrievalOptions {
                top_k: Some(5),
                target_scope: Some(TargetScope::File("src/server.rs")),
                filter: Some(&filter),
                ann_enabled: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert_eq!(
            hits.first().map(|hit| hit.skill_id.as_str()),
            Some("universal")
        );
    }

    #[tokio::test]
    async fn strict_cascade_changeset_recalls_rule_scoped_to_secondary_diff_file() {
        // The fallback-loop replacement regression: a cross-cutting rule whose
        // `file_patterns` carry both sides of a coupled change (schema globs +
        // migrations dir) must be recalled in ONE changeset query even when
        // the diff's primary file (src/api.ts) doesn't match — previously the
        // single-file cascade on the primary file dropped it and a per-file
        // fallback loop had to re-query.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("idx.db");
        let pool = open_pool_at(&path).await.unwrap();
        let mut coupled = rule_doc(
            "schema-needs-migration",
            "schema changes must ship a paired migration in the migrations directory",
            None,
            Some("acme/widgets"),
        );
        coupled.file_patterns = Some(r#"["db/schema/**", "migrations/**/*.sql"]"#.to_owned());
        upsert_rule_chunks(&pool, &[coupled]).await.unwrap();

        let filter = QueryFilter {
            language: None,
            repo_scope: Some("acme/widgets".to_owned()),
        };
        let diff_files = vec!["src/api.ts".to_owned(), "db/schema/users.sql".to_owned()];
        let hits = retrieve_rules_with_confidence(
            &pool,
            "schema changes migration",
            RetrievalOptions {
                top_k: Some(5),
                target_scope: Some(TargetScope::Changeset(&diff_files)),
                filter: Some(&filter),
                ann_enabled: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            hits.first().map(|hit| hit.skill_id.as_str()),
            Some("schema-needs-migration"),
            "changeset scope must recall a rule matched by a non-primary diff file"
        );

        // Same query scoped to ONLY the primary file must still drop it —
        // the changeset variant widens scope, the single-file cascade stays
        // strict (the hook hot path's misapply control).
        let single_file_hits = retrieve_rules_with_confidence(
            &pool,
            "schema changes migration",
            RetrievalOptions {
                top_k: Some(5),
                target_scope: Some(TargetScope::File("src/api.ts")),
                filter: Some(&filter),
                ann_enabled: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(
            single_file_hits.is_empty(),
            "single-file cascade must not widen to other files' patterns"
        );
    }

    #[tokio::test]
    async fn strict_cascade_changeset_drops_rule_when_no_diff_file_matches() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("idx.db");
        let pool = open_pool_at(&path).await.unwrap();
        let mut scoped = rule_doc(
            "schema-needs-migration",
            "schema changes must ship a paired migration in the migrations directory",
            None,
            Some("acme/widgets"),
        );
        scoped.file_patterns = Some(r#"["db/schema/**", "migrations/**/*.sql"]"#.to_owned());
        upsert_rule_chunks(&pool, &[scoped]).await.unwrap();

        let filter = QueryFilter {
            language: None,
            repo_scope: Some("acme/widgets".to_owned()),
        };
        let diff_files = vec!["src/api.ts".to_owned(), "docs/usage.md".to_owned()];
        let hits = retrieve_rules_with_confidence(
            &pool,
            "schema changes migration",
            RetrievalOptions {
                top_k: Some(5),
                target_scope: Some(TargetScope::Changeset(&diff_files)),
                filter: Some(&filter),
                ann_enabled: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(
            hits.is_empty(),
            "a changeset touching none of the rule's globs must not recall it"
        );
    }

    #[tokio::test]
    async fn retrieve_emits_retrieval_filter_step_when_filter_active() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("idx.db");
        let pool = open_pool_at(&path).await.unwrap();

        let rules = vec![
            rule_doc("rust-1", "rust-specific rule content", Some("rust"), None),
            rule_doc("py-1", "python-specific rule content", Some("python"), None),
        ];
        upsert_rule_chunks(&pool, &rules).await.unwrap();

        let mut tb = TrajectoryBuilder::new();
        let filter = QueryFilter {
            language: Some("rust".into()),
            repo_scope: None,
        };
        let _ = retrieve_rules_with_confidence(
            &pool,
            "rule",
            RetrievalOptions {
                top_k: Some(5),
                filter: Some(&filter),
                trajectory: Some(&mut tb),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let got = tb
            .steps()
            .iter()
            .find_map(|s| match s {
                TrajectoryStep::RetrievalFilter { before, after } => Some((*before, *after)),
                _ => None,
            })
            .expect("RetrievalFilter step must fire when filter is active");
        assert_eq!(got.0, 2, "before = 2 (total chunks)");
        assert_eq!(got.1, 1, "after = 1 (only rust chunk survives)");
    }
}
