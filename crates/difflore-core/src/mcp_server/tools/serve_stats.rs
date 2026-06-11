//! Serve telemetry and rule-retrieval machinery shared by the MCP tool
//! handlers (split out of the former `tools/util.rs`): the `mcp_query`
//! outbox feed and the repo-scoped retrieval / rerank pipeline.

use serde_json::json;
use sqlx::SqlitePool;
use std::collections::{HashMap, HashSet};

use crate::error::CoreError;

use super::evidence::{fetch_skills_by_ids, strict_file_match_ids_for_meta};

pub(crate) const MCP_EMBEDDING_TIMEOUT: std::time::Duration =
    std::time::Duration::from_millis(1500);

pub(crate) struct McpQueryOutboxEntry<'a> {
    pub file: &'a str,
    pub intent: &'a str,
    pub rules_injected: usize,
    pub strict_match_count: usize,
    pub rule_titles: &'a [String],
    pub rule_ids: &'a [String],
    pub client_label: &'a str,
    pub repo_full_name: Option<&'a str>,
}

pub(crate) async fn enqueue_mcp_query_outbox(db: &SqlitePool, entry: McpQueryOutboxEntry<'_>) {
    let payload = json!({
        "file": entry.file,
        "intent": entry.intent,
        "rules_injected": entry.rules_injected,
        "strict_match_count": entry.strict_match_count,
        "rule_titles": entry.rule_titles,
        "rule_ids": entry.rule_ids,
        "client_label": entry.client_label,
        "repo_full_name": entry.repo_full_name,
    });
    match serde_json::to_string(&payload) {
        Ok(payload_str) => {
            let queue = crate::cloud::outbox::OutboxQueue::new(db.clone());
            if let Err(e) = queue
                .enqueue(crate::cloud::outbox::kind::MCP_QUERY, &payload_str)
                .await
            {
                if crate::infra::env::debug_telemetry() {
                    eprintln!("[difflore-mcp] enqueue mcp_query outbox failed: {e}");
                }
            }
        }
        Err(e) => {
            if crate::infra::env::debug_telemetry() {
                eprintln!("[difflore-mcp] serialize mcp_query outbox failed: {e}");
            }
        }
    }
}

pub(crate) async fn drain_mcp_query_outbox(
    db: &SqlitePool,
    cloud: &crate::cloud::client::CloudClient,
    max_items: usize,
) -> (usize, usize) {
    let queue = crate::cloud::outbox::OutboxQueue::new(db.clone());
    match crate::cloud::outbox::drain_outbox(&queue, cloud, max_items).await {
        Ok(summary) => summary,
        Err(e) => {
            if crate::infra::env::debug_telemetry() {
                eprintln!("[difflore-mcp] drain mcp_query outbox failed: {e}");
            }
            (0, 0)
        }
    }
}

/// Build a `QueryFilter` from the agent's target file plus an optional GitHub
/// `owner/repo`. `language` comes from the file extension so the SQL pre-filter
/// drops rules tagged for other languages; an unknown extension falls through
/// to NULL. `repo_scope` scopes retrieval to the current repo only — a NULL
/// scope is not a runtime global fallback.
pub(crate) fn filter_from_file(
    target_file: Option<&str>,
    repo_scope: Option<&str>,
) -> crate::context::index_db::QueryFilter {
    crate::context::index_db::QueryFilter {
        language: target_file.and_then(crate::context::retrieval::detect_language_from_path),
        repo_scope: repo_scope.map(String::from),
    }
}

fn unique_repo_scopes(repo_scopes: &[String]) -> Vec<String> {
    let mut seen = HashSet::with_capacity(repo_scopes.len());
    let mut unique = Vec::new();
    for scope in repo_scopes {
        if let Some(scope) = normalize_repo_scope(scope)
            && seen.insert(scope.clone())
        {
            unique.push(scope);
        }
    }
    unique
}

fn normalize_repo_scope(scope: &str) -> Option<String> {
    let scope = scope.trim();
    if scope.is_empty() {
        return None;
    }
    Some(scope.to_ascii_lowercase())
}

use crate::context::retrieval::merge_scored_rule_chunks;

/// Args for [`retrieve_rules_with_repo_scopes`]. Bundles ranking metadata and
/// tuning knobs so each call site can name what it passes instead of relying on
/// positional booleans.
pub(crate) struct RetrieveRulesArgs<'a> {
    pub query: &'a str,
    pub lexical_query: Option<&'a str>,
    pub top_k: usize,
    pub target_file: Option<&'a str>,
    pub repo_scopes: &'a [String],
    pub confidence_map: Option<&'a HashMap<String, f64>>,
    pub age_days_map: Option<&'a HashMap<String, f32>>,
    pub ann_enabled: bool,
    pub embedding_timeout: Option<std::time::Duration>,
    pub adaptive_prune: bool,
}

pub(crate) async fn retrieve_rules_with_repo_scopes(
    index_pool: &SqlitePool,
    args: RetrieveRulesArgs<'_>,
) -> Result<Vec<crate::context::retrieval::ScoredRuleChunk>, CoreError> {
    let RetrieveRulesArgs {
        query,
        lexical_query,
        top_k,
        target_file,
        repo_scopes,
        confidence_map,
        age_days_map,
        ann_enabled,
        embedding_timeout,
        adaptive_prune,
    } = args;
    if top_k == 0 {
        return Ok(Vec::new());
    }
    let top_k = top_k.min(50);
    // Multi-scope: when both `origin` and `upstream` are detected, query
    // each scope independently, cap fan-out for latency, and merge by
    // best-score dedup. This keeps MCP recall aligned with the CLI path.
    let repo_scopes: Vec<String> = unique_repo_scopes(repo_scopes)
        .into_iter()
        .take(4)
        .collect();
    if repo_scopes.is_empty() {
        return Ok(Vec::new());
    }
    let scope_filters: Vec<Option<String>> = repo_scopes.into_iter().map(Some).collect();

    let query_variants = retrieval_query_variants(query, lexical_query);
    let mut groups = Vec::with_capacity(scope_filters.len() * query_variants.len());
    // Hook/MCP surfaces are single-file by contract; adapt the path into the
    // generalised scope here so the latency-critical callers stay unchanged.
    let target_scope = target_file.map(crate::context::retrieval::TargetScope::File);
    for repo_scope in &scope_filters {
        let filter = filter_from_file(target_file, repo_scope.as_deref());
        for query_variant in &query_variants {
            groups.push(
                crate::context::retrieval::retrieve_rules_with_confidence(
                    index_pool,
                    query_variant,
                    crate::context::retrieval::RetrievalOptions {
                        top_k: Some(top_k),
                        confidence_map,
                        age_days_map,
                        target_scope,
                        filter: Some(&filter),
                        ann_enabled,
                        embedding_timeout,
                        adaptive_prune,
                        ..Default::default()
                    },
                )
                .await?,
            );
        }
    }

    Ok(merge_scored_rule_chunks(groups, top_k))
}

fn retrieval_query_variants<'a>(query: &'a str, lexical_query: Option<&'a str>) -> Vec<&'a str> {
    let query = query.trim();
    let lexical_query = lexical_query.unwrap_or("").trim();
    let mut variants = Vec::with_capacity(2);
    if !query.is_empty() {
        variants.push(query);
    }
    if !lexical_query.is_empty()
        && !normalized_query_key(lexical_query).is_empty()
        && normalized_query_key(lexical_query) != normalized_query_key(query)
    {
        variants.push(lexical_query);
    }
    variants
}

fn normalized_query_key(query: &str) -> String {
    split_query_tokens(query).join(" ")
}

pub(crate) fn rerank_scored_rule_chunks_for_mcp(
    chunks: Vec<crate::context::retrieval::ScoredRuleChunk>,
    lexical_query: &str,
    limit: usize,
) -> Vec<crate::context::retrieval::ScoredRuleChunk> {
    crate::context::retrieval::rerank_scored_rule_chunks_by_lexical_query(
        chunks,
        lexical_query,
        limit,
    )
}

pub(crate) fn rerank_scored_rule_chunks_for_mcp_by_strict_file_matches(
    chunks: Vec<crate::context::retrieval::ScoredRuleChunk>,
    lexical_query: &str,
    limit: usize,
    strict_skill_ids: &HashSet<String>,
) -> Vec<crate::context::retrieval::ScoredRuleChunk> {
    let lexical_limit = chunks.len();
    let mut reranked = rerank_scored_rule_chunks_for_mcp(chunks, lexical_query, lexical_limit);
    if !strict_skill_ids.is_empty() {
        reranked.sort_by(|a, b| {
            strict_skill_ids
                .contains(&b.skill_id)
                .cmp(&strict_skill_ids.contains(&a.skill_id))
        });
    }
    reranked.truncate(limit);
    reranked
}

/// Cold-start helper shared by the hook and `search_rules`: fetch
/// transferable rules whose `file_patterns` strict-match `target_file`.
///
/// Returns empty unless the starter index is already built. Callers must
/// label results as cross-repo suggestions.
pub(crate) async fn cross_repo_starter_scored(
    db: &SqlitePool,
    query: &str,
    target_file: &str,
    confidence_map: Option<&HashMap<String, f64>>,
    age_days_map: Option<&HashMap<String, f32>>,
    top_k: usize,
) -> Vec<crate::context::retrieval::ScoredRuleChunk> {
    let Ok(Some(starter_pool)) =
        crate::context::orchestrator::cross_repo_starter_index_if_current(db).await
    else {
        return Vec::new();
    };
    let candidate_limit = top_k.saturating_mul(5).clamp(top_k.max(1), 50);
    let Ok(candidates) = crate::context::retrieval::retrieve_rules_for_search(
        &starter_pool,
        crate::context::retrieval::RuleSearchRetrievalOptions {
            query,
            lexical_query: query,
            top_k: candidate_limit,
            confidence_map,
            age_days_map,
            target_scope: Some(crate::context::retrieval::TargetScope::File(target_file)),
            // Every repo's rules are eligible; the strict file-pattern gate
            // below keeps only the transferable ones.
            repo_scopes: &[],
            ann_enabled: false,
            embedding_timeout: Some(MCP_EMBEDDING_TIMEOUT),
            // Latency-critical MCP path: fast-degrade to lexical, never retry a
            // cold embed (that would block the agent's tool call).
            cold_start_retry: false,
            adaptive_prune: true,
        },
    )
    .await
    else {
        return Vec::new();
    };
    let ids: Vec<String> = candidates
        .iter()
        .map(|chunk| chunk.skill_id.clone())
        .collect();
    let meta = fetch_skills_by_ids(db, &ids).await.unwrap_or_default();
    let strict = strict_file_match_ids_for_meta(&meta, Some(target_file));
    let mut keep: Vec<_> = candidates
        .into_iter()
        .filter(|chunk| strict.contains(&chunk.skill_id))
        .collect();
    keep.truncate(top_k);
    keep
}

#[cfg(test)]
pub(crate) fn retain_only_strict_file_scoped_chunks(
    chunks: &mut Vec<crate::context::retrieval::ScoredRuleChunk>,
    target_file: Option<&str>,
    strict_skill_ids: &HashSet<String>,
) {
    if target_file.is_some() {
        chunks.retain(|chunk| strict_skill_ids.contains(&chunk.skill_id));
    }
}

pub(crate) fn build_empty_recall_retry_query(file: &str, intent: &str) -> Option<String> {
    let mut terms = Vec::new();
    let mut seen = HashSet::new();

    let file = file.trim();
    if !file.is_empty() && file != "unknown" {
        for component in file
            .split(['/', '\\'])
            .rev()
            .take(3)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
        {
            push_query_tokens(component, &mut seen, &mut terms, true);
        }
        if let Some(ext) = file.rsplit_once('.').map(|(_, ext)| ext.trim()) {
            match ext.to_ascii_lowercase().as_str() {
                "ts" | "tsx" => push_unique_query_term("typescript", &mut seen, &mut terms),
                "js" | "jsx" => push_unique_query_term("javascript", &mut seen, &mut terms),
                "rs" => push_unique_query_term("rust", &mut seen, &mut terms),
                "py" => push_unique_query_term("python", &mut seen, &mut terms),
                "go" => push_unique_query_term("go", &mut seen, &mut terms),
                _ => {}
            }
        }
    }

    for token in intent_tokens(intent).into_iter().take(12) {
        push_unique_query_term(&token, &mut seen, &mut terms);
    }

    if terms.is_empty() {
        return None;
    }
    let retry = terms.join(" ");
    let original = normalize_retry_comparison(&format!("{file} {intent}"));
    let normalized_retry = normalize_retry_comparison(&retry);
    (!normalized_retry.is_empty() && normalized_retry != original).then_some(retry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_server::tools::evidence::strict_file_match_ids_for_rules;

    fn scored_rule(
        id: &str,
        title: &str,
        body: &str,
        score: f64,
    ) -> crate::context::retrieval::ScoredRuleChunk {
        crate::context::retrieval::ScoredRuleChunk {
            skill_id: id.to_owned(),
            content: format!("Rule ID: {id}\nRule Name: {title}\nType: review_standard\n\n{body}"),
            score,
            confidence: 0.7,
        }
    }

    fn rule_doc(
        id: &str,
        title: &str,
        body: &str,
        confidence: f64,
        repo_scope: &str,
    ) -> crate::context::rule_source::RuleDocument {
        crate::context::rule_source::RuleDocument {
            skill_id: id.to_owned(),
            title: title.to_owned(),
            content: format!(
                "Rule ID: {id}\nRule Name: {title}\nType: review_standard\nSource: {repo_scope}\nTags: [\"rust\"]\n\n{body}"
            ),
            confidence,
            file_patterns: Some("[\"src/**/*.rs\"]".to_owned()),
            language: Some("rust".to_owned()),
            repo_scope: Some(repo_scope.to_owned()),
        }
    }

    fn ids(chunks: &[crate::context::retrieval::ScoredRuleChunk]) -> Vec<&str> {
        chunks.iter().map(|chunk| chunk.skill_id.as_str()).collect()
    }

    fn score_for(chunks: &[crate::context::retrieval::ScoredRuleChunk], id: &str) -> f64 {
        chunks
            .iter()
            .find(|chunk| chunk.skill_id == id)
            .map(|chunk| chunk.score)
            .expect("expected score for rule id")
    }

    #[test]
    fn repo_scopes_are_normalized_and_deduped_in_input_order() {
        assert_eq!(
            unique_repo_scopes(&[
                "  Difflore-Fixtures/Vite  ".to_owned(),
                "difflore-fixtures/vite".to_owned(),
                " ".to_owned(),
                "ViteJS/Vite".to_owned(),
                "vitejs/vite ".to_owned(),
            ]),
            vec![
                "difflore-fixtures/vite".to_owned(),
                "vitejs/vite".to_owned()
            ]
        );
    }

    #[test]
    fn mcp_intent_rerank_promotes_specific_reviewer_rule_over_generic_overview() {
        let reranked = rerank_scored_rule_chunks_for_mcp(
            vec![
                scored_rule(
                    "overview",
                    "Review: preserve source PR regression coverage pattern",
                    "When touching **/*.go, preserve broad regression coverage from the source PR.",
                    0.0206,
                ),
                scored_rule(
                    "specific",
                    "Review: replace magic number 100 with http.StatusContinue",
                    "When touching context.go, the PR only replaces magic number 100 with http.StatusContinue in bodyAllowedForStatus.",
                    0.0180,
                ),
            ],
            "test(context): use http.StatusContinue constant instead of magic number 100",
            2,
        );

        assert_eq!(reranked[0].skill_id, "specific");
    }

    #[test]
    fn mcp_strict_file_rerank_promotes_file_scoped_rule_before_universal_rule() {
        let strict_skill_ids: HashSet<String> = ["strict-file".to_owned()].into();
        let reranked = rerank_scored_rule_chunks_for_mcp_by_strict_file_matches(
            vec![
                scored_rule(
                    "universal",
                    "Review: general handler guidance",
                    "When touching handlers, preserve the existing structure.",
                    0.90,
                ),
                scored_rule(
                    "strict-file",
                    "Review: src/http/handler.rs unwrap guard",
                    "When touching src/http/handler.rs, never unwrap request payloads.",
                    0.20,
                ),
            ],
            "general handler guidance",
            2,
            &strict_skill_ids,
        );

        assert_eq!(ids(&reranked), vec!["strict-file", "universal"]);
    }

    #[test]
    fn mcp_file_scope_filter_drops_universal_and_wrong_file_hits() {
        let strict_skill_ids: HashSet<String> = ["strict-file".to_owned()].into();
        let mut chunks = vec![
            scored_rule(
                "universal",
                "Review: general handler guidance",
                "When touching handlers, preserve the existing structure.",
                0.90,
            ),
            scored_rule(
                "wrong-file",
                "Review: docs guidance",
                "When touching docs, keep examples current.",
                0.80,
            ),
            scored_rule(
                "strict-file",
                "Review: src/http/handler.rs unwrap guard",
                "When touching src/http/handler.rs, never unwrap request payloads.",
                0.20,
            ),
        ];

        retain_only_strict_file_scoped_chunks(
            &mut chunks,
            Some("src/http/handler.rs"),
            &strict_skill_ids,
        );

        assert_eq!(ids(&chunks), vec!["strict-file"]);
    }

    #[test]
    fn strict_file_match_ids_for_rules_only_counts_explicit_matching_globs() {
        let repo_scope = "acme/widgets";
        let mut strict = rule_doc(
            "strict",
            "Avoid unwrap in handlers",
            "Never unwrap request handlers.",
            0.7,
            repo_scope,
        );
        strict.file_patterns = Some("[\"src/http/*.rs\"]".to_owned());
        let mut universal = rule_doc(
            "universal",
            "General handler guidance",
            "Preserve existing handler structure.",
            0.9,
            repo_scope,
        );
        universal.file_patterns = None;
        let mut other_file = rule_doc(
            "other-file",
            "CLI guidance",
            "Keep CLI parsing explicit.",
            0.9,
            repo_scope,
        );
        other_file.file_patterns = Some("[\"src/cli/*.rs\"]".to_owned());

        let strict_ids = strict_file_match_ids_for_rules(
            &[strict, universal, other_file],
            Some("src/http/handler.rs"),
        );

        assert_eq!(strict_ids, ["strict".to_owned()].into());
    }

    #[tokio::test]
    async fn cli_search_helper_and_mcp_helper_share_age_decay_ranking() {
        let _home = crate::infra::db::shared_test_home();
        let index_path = std::env::temp_dir().join(format!(
            "difflore-cli-mcp-age-parity-{}-{}.db",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let index_pool = crate::context::index_db::open_pool_at(&index_path)
            .await
            .unwrap();
        let repo_scope = "acme/widgets";
        let query = "src/http/handler.rs unwrap request handlers";
        let intent = "unwrap request handlers";
        let repo_scopes = vec![repo_scope.to_owned()];
        let rules = vec![
            rule_doc(
                "old-style",
                "Format unwrap request handlers",
                "Format unwrap request handlers consistently; lint spacing before returning errors.",
                1.0,
                repo_scope,
            ),
            rule_doc(
                "fresh-correction",
                "Avoid unwrap in request handlers",
                "Never unwrap request handlers; return a structured error instead.",
                0.6,
                repo_scope,
            ),
        ];
        crate::context::index_db::upsert_rule_chunks(&index_pool, &rules)
            .await
            .unwrap();

        let confidence_map: HashMap<String, f64> = [
            ("old-style".to_owned(), 1.0),
            ("fresh-correction".to_owned(), 0.6),
        ]
        .into();
        let age_days_map: HashMap<String, f32> = [
            ("old-style".to_owned(), 730.0),
            ("fresh-correction".to_owned(), 1.0),
        ]
        .into();
        let top_k = 2usize;

        let cli_without_age = crate::context::retrieval::retrieve_rules_for_search(
            &index_pool,
            crate::context::retrieval::RuleSearchRetrievalOptions {
                query,
                lexical_query: intent,
                top_k,
                confidence_map: Some(&confidence_map),
                age_days_map: None,
                target_scope: Some(crate::context::retrieval::TargetScope::File(
                    "src/http/handler.rs",
                )),
                repo_scopes: &repo_scopes,
                ann_enabled: false,
                embedding_timeout: Some(MCP_EMBEDDING_TIMEOUT),
                cold_start_retry: false,
                adaptive_prune: false,
            },
        )
        .await
        .unwrap();
        let cli_with_age = crate::context::retrieval::retrieve_rules_for_search(
            &index_pool,
            crate::context::retrieval::RuleSearchRetrievalOptions {
                query,
                lexical_query: intent,
                top_k,
                confidence_map: Some(&confidence_map),
                age_days_map: Some(&age_days_map),
                target_scope: Some(crate::context::retrieval::TargetScope::File(
                    "src/http/handler.rs",
                )),
                repo_scopes: &repo_scopes,
                ann_enabled: false,
                embedding_timeout: Some(MCP_EMBEDDING_TIMEOUT),
                cold_start_retry: false,
                adaptive_prune: false,
            },
        )
        .await
        .unwrap();

        let candidate_limit = top_k.saturating_mul(5).clamp(top_k, 50);
        let mcp_candidates = retrieve_rules_with_repo_scopes(
            &index_pool,
            RetrieveRulesArgs {
                query,
                lexical_query: Some(intent),
                top_k: candidate_limit,
                target_file: Some("src/http/handler.rs"),
                repo_scopes: &repo_scopes,
                confidence_map: Some(&confidence_map),
                age_days_map: Some(&age_days_map),
                ann_enabled: false,
                embedding_timeout: Some(MCP_EMBEDDING_TIMEOUT),
                adaptive_prune: false,
            },
        )
        .await
        .unwrap();
        let mcp_with_age = rerank_scored_rule_chunks_for_mcp(mcp_candidates, intent, top_k);

        assert_eq!(ids(&cli_with_age), ids(&mcp_with_age));
        for id in ids(&cli_with_age) {
            let cli_score = score_for(&cli_with_age, id);
            let mcp_score = score_for(&mcp_with_age, id);
            assert!(
                (cli_score - mcp_score).abs() < 1e-12,
                "score mismatch for {id}: cli={cli_score} mcp={mcp_score}"
            );
        }
        assert!(
            score_for(&cli_with_age, "old-style") < score_for(&cli_without_age, "old-style"),
            "old style rule should decay when the shared age_days_map is supplied"
        );
    }

    #[tokio::test]
    async fn mcp_retrieval_without_repo_scopes_does_not_inject_global_memory() {
        let _home = crate::infra::db::shared_test_home();
        let index_path = std::env::temp_dir().join(format!(
            "difflore-mcp-empty-scope-{}-{}.db",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let index_pool = crate::context::index_db::open_pool_at(&index_path)
            .await
            .unwrap();
        let rules = vec![rule_doc(
            "signal",
            "Avoid unwrap in request handlers",
            "Avoid unwrap in request handlers; return structured errors",
            0.8,
            "acme/widgets",
        )];
        crate::context::index_db::upsert_rule_chunks(&index_pool, &rules)
            .await
            .unwrap();

        let hits = retrieve_rules_with_repo_scopes(
            &index_pool,
            RetrieveRulesArgs {
                query: "src/http/handler.rs Avoid unwrap in request handlers",
                lexical_query: Some("Avoid unwrap in request handlers"),
                top_k: 5,
                target_file: Some("src/http/handler.rs"),
                repo_scopes: &[],
                confidence_map: None,
                age_days_map: None,
                ann_enabled: false,
                embedding_timeout: Some(MCP_EMBEDDING_TIMEOUT),
                adaptive_prune: false,
            },
        )
        .await
        .unwrap();

        assert!(
            hits.is_empty(),
            "MCP recall must not fall back to global memory when no repo scope is available"
        );
    }
}

fn push_query_tokens(
    value: &str,
    seen: &mut HashSet<String>,
    terms: &mut Vec<String>,
    allow_short: bool,
) {
    for token in split_query_tokens(value) {
        if allow_short || token.chars().count() >= 3 {
            push_unique_query_term(&token, seen, terms);
        }
    }
}

fn intent_tokens(intent: &str) -> Vec<String> {
    split_query_tokens(intent)
        .into_iter()
        .filter(|token| token.chars().count() >= 3)
        .filter(|token| !MCP_RETRY_STOPWORDS.contains(&token.as_str()))
        .collect()
}

fn split_query_tokens(value: &str) -> Vec<String> {
    value
        .split(|c: char| !c.is_ascii_alphanumeric())
        .map(|token| token.trim().to_ascii_lowercase())
        .filter(|token| !token.is_empty())
        .collect()
}

fn push_unique_query_term(token: &str, seen: &mut HashSet<String>, terms: &mut Vec<String>) {
    if seen.insert(token.to_owned()) {
        terms.push(token.to_owned());
    }
}

fn normalize_retry_comparison(value: &str) -> String {
    split_query_tokens(value).join(" ")
}

const MCP_RETRY_STOPWORDS: &[&str] = &[
    "about",
    "agent",
    "against",
    "apply",
    "applicable",
    "before",
    "check",
    "code",
    "context",
    "could",
    "diff",
    "does",
    "file",
    "find",
    "from",
    "give",
    "have",
    "help",
    "here",
    "into",
    "memory",
    "need",
    "please",
    "relevant",
    "review",
    "rules",
    "search",
    "should",
    "this",
    "that",
    "what",
    "when",
    "with",
];

#[cfg(test)]
mod filter_from_file_tests {
    use super::{build_empty_recall_retry_query, filter_from_file};

    #[test]
    fn ts_file_yields_typescript_language() {
        let f = filter_from_file(Some("packages/vite/src/node/plugins/resolve.ts"), None);
        assert_eq!(f.language.as_deref(), Some("typescript"));
        assert!(f.repo_scope.is_none());
    }

    #[test]
    fn rs_file_yields_rust_language() {
        let f = filter_from_file(Some("crates/difflore-core/src/lib.rs"), None);
        assert_eq!(f.language.as_deref(), Some("rust"));
    }

    #[test]
    fn unknown_extension_degrades_to_no_language_filter() {
        let f = filter_from_file(Some(".gitignore"), None);
        assert!(f.language.is_none());
    }

    #[test]
    fn no_target_file_uses_default_filter() {
        let f = filter_from_file(None, None);
        assert!(f.is_empty());
    }

    #[test]
    fn repo_scope_passes_through_when_provided() {
        let f = filter_from_file(Some("src/foo.ts"), Some("vitejs/vite"));
        assert_eq!(f.language.as_deref(), Some("typescript"));
        assert_eq!(f.repo_scope.as_deref(), Some("vitejs/vite"));
    }

    #[test]
    fn repo_scope_alone_with_no_file_still_scopes() {
        let f = filter_from_file(None, Some("tokio-rs/tokio"));
        assert!(f.language.is_none());
        assert_eq!(f.repo_scope.as_deref(), Some("tokio-rs/tokio"));
    }

    #[test]
    fn empty_recall_retry_query_keeps_file_and_distinctive_intent_terms() {
        let retry = build_empty_recall_retry_query(
            "packages/router/src/parser.ts",
            "Please search review memory for deeply nested optional route parsing with no exact wording",
        )
        .expect("retry query");

        assert!(retry.contains("parser"));
        assert!(retry.contains("typescript"));
        assert!(retry.contains("nested"));
        assert!(retry.contains("route"));
        assert!(!retry.contains("please"));
        assert!(!retry.contains("review"));
    }

    #[test]
    fn empty_recall_retry_query_returns_none_when_it_cannot_improve() {
        assert!(build_empty_recall_retry_query("unknown", "please search rules").is_none());
    }

    #[test]
    fn retrieval_query_variants_dedupes_path_and_intent_lanes() {
        assert_eq!(
            super::retrieval_query_variants(
                "src/context.go Bind handlers must check returned error",
                Some("Bind handlers must check returned error"),
            ),
            vec![
                "src/context.go Bind handlers must check returned error",
                "Bind handlers must check returned error",
            ],
        );
        assert_eq!(
            super::retrieval_query_variants("Bind handlers", Some("bind handlers")),
            vec!["Bind handlers"],
        );
        assert_eq!(
            super::retrieval_query_variants("", Some("please")),
            vec!["please"],
        );
    }
}
