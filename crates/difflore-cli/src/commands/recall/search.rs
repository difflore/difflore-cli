//! Shared local retrieval helpers for `recall` and tests.

#[cfg(test)]
fn search_filter(
    target_file: Option<&str>,
    repo_scope: Option<&str>,
) -> difflore_core::context::index_db::QueryFilter {
    difflore_core::context::index_db::QueryFilter {
        language: target_file
            .and_then(difflore_core::context::retrieval::detect_language_from_path),
        repo_scope: repo_scope.map(String::from),
    }
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

// Canonical dedup/limit helper, shared with the orchestrator and MCP tool
// helpers so the policy stays in lock-step across all three callsites.
use difflore_core::context::retrieval::merge_scored_rule_chunks;

const CLI_SEARCH_EMBEDDING_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(2500);

fn normalize_rule_title_for_match(title: &str) -> String {
    title
        .split(|ch: char| !ch.is_alphanumeric())
        .filter(|part| !part.is_empty())
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

// Shared core predicate (also used by `difflore export`) so the two
// project-scope checks cannot drift: empty scopes match nothing, scope-less
// rules are not wildcards, comparison is case-insensitive exact.
fn rule_matches_repo_scope(
    rule: &difflore_core::context::rule_source::RuleDocument,
    repo_scopes: &[String],
) -> bool {
    difflore_core::export::repo_scope_matches(rule.repo_scope.as_deref(), repo_scopes)
}

fn exact_title_matches(
    rules: &[difflore_core::context::rule_source::RuleDocument],
    query: &str,
    repo_scopes: &[String],
    limit: usize,
) -> Vec<difflore_core::context::retrieval::ScoredRuleChunk> {
    let query_title = normalize_rule_title_for_match(query);
    if query_title.is_empty() {
        return Vec::new();
    }
    // Allow up to 4 repo scopes so fork+upstream (and rare nested fork chains)
    // all participate.
    let repo_scopes: Vec<String> = unique_repo_scopes(repo_scopes)
        .into_iter()
        .take(4)
        .collect();
    let mut out: Vec<_> = rules
        .iter()
        .filter(|rule| normalize_rule_title_for_match(&rule.title) == query_title)
        .filter(|rule| rule_matches_repo_scope(rule, &repo_scopes))
        .map(|rule| difflore_core::context::retrieval::ScoredRuleChunk {
            skill_id: rule.skill_id.clone(),
            content: rule.content.clone(),
            // Exact title lookup is a trust path: a title copied from
            // `rules list` into `recall` must surface. Score far above hybrid
            // scores, but only for exact title equality.
            score: 10.0 + rule.confidence.clamp(0.0, 1.0),
            confidence: rule.confidence,
        })
        .collect();
    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out.truncate(limit);
    out
}

pub(crate) fn rule_title(content: &str, fallback: &str) -> String {
    let raw = content
        .lines()
        .find_map(|line| line.strip_prefix("Rule Name:").map(|s| s.trim().to_owned()))
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| fallback.to_owned());
    // Display-time cleanup so titles minted by older binaries (which captured
    // CodeRabbit emphasis/banners verbatim) render cleanly without a DB
    // migration. Idempotent for already-clean titles.
    crate::support::review_text::clean_display_title(&raw, fallback)
}

#[cfg(test)]
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

#[cfg(test)]
fn lexical_boost(
    chunk: &difflore_core::context::retrieval::ScoredRuleChunk,
    terms: &[String],
) -> f64 {
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

#[cfg(test)]
fn rerank_scored_rule_chunks(
    mut chunks: Vec<difflore_core::context::retrieval::ScoredRuleChunk>,
    lexical_query: &str,
    limit: usize,
) -> Vec<difflore_core::context::retrieval::ScoredRuleChunk> {
    let terms = lexical_terms(lexical_query);
    for chunk in &mut chunks {
        chunk.score += lexical_boost(chunk, &terms);
    }

    chunks.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    chunks.truncate(limit);
    chunks
}

pub(crate) fn merge_exact_title_matches(
    rules: &[difflore_core::context::rule_source::RuleDocument],
    intent: &str,
    repo_scopes: &[String],
    scored: Vec<difflore_core::context::retrieval::ScoredRuleChunk>,
    top_k: usize,
) -> Vec<difflore_core::context::retrieval::ScoredRuleChunk> {
    merge_scored_rule_chunks(
        [
            exact_title_matches(rules, intent, repo_scopes, top_k),
            scored,
        ],
        top_k,
    )
}

#[allow(clippy::too_many_arguments)]
// reason: search passes through the same independent retrieval filters as recall/MCP.
pub(crate) async fn retrieve_rules_for_search(
    index_pool: &difflore_core::SqlitePool,
    query: &str,
    lexical_query: &str,
    top_k: usize,
    confidence_map: Option<&std::collections::HashMap<String, f64>>,
    age_days_map: Option<&std::collections::HashMap<String, f32>>,
    target_file: Option<&str>,
    repo_scopes: &[String],
) -> Result<Vec<difflore_core::context::retrieval::ScoredRuleChunk>, difflore_core::CoreError> {
    difflore_core::context::retrieval::retrieve_rules_for_search(
        index_pool,
        difflore_core::context::retrieval::RuleSearchRetrievalOptions {
            query,
            lexical_query,
            top_k,
            confidence_map,
            age_days_map,
            target_file,
            repo_scopes,
            ann_enabled: true,
            embedding_timeout: Some(CLI_SEARCH_EMBEDDING_TIMEOUT),
            // This helper backs `difflore recall` only (a human is waiting), so
            // a cold first query embed gets one longer-budget retry to keep
            // semantic ranking instead of falling straight back to lexical.
            cold_start_retry: true,
            adaptive_prune: false,
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scored(id: &str, score: f64) -> difflore_core::context::retrieval::ScoredRuleChunk {
        difflore_core::context::retrieval::ScoredRuleChunk {
            skill_id: id.to_owned(),
            content: id.to_owned(),
            score,
            confidence: 0.7,
        }
    }

    fn scored_rule(
        id: &str,
        title: &str,
        body: &str,
        score: f64,
    ) -> difflore_core::context::retrieval::ScoredRuleChunk {
        difflore_core::context::retrieval::ScoredRuleChunk {
            skill_id: id.to_owned(),
            content: format!("Rule ID: {id}\nRule Name: {title}\nType: review_standard\n\n{body}"),
            score,
            confidence: 0.7,
        }
    }

    fn rule_doc(
        id: &str,
        title: &str,
        repo_scope: Option<&str>,
    ) -> difflore_core::context::rule_source::RuleDocument {
        difflore_core::context::rule_source::RuleDocument {
            skill_id: id.to_owned(),
            title: title.to_owned(),
            content: format!("Rule ID: {id}\nRule Name: {title}\nType: review_standard\n\nbody"),
            confidence: 0.7,
            file_patterns: None,
            language: None,
            repo_scope: repo_scope.map(ToOwned::to_owned),
        }
    }

    #[test]
    fn search_filter_uses_language_and_repo_scope() {
        let filter = search_filter(Some("packages/vite/src/node/index.ts"), Some("vitejs/vite"));
        assert_eq!(filter.language.as_deref(), Some("typescript"));
        assert_eq!(filter.repo_scope.as_deref(), Some("vitejs/vite"));
    }

    #[test]
    fn unique_repo_scopes_preserves_order_and_dedupes() {
        assert_eq!(
            unique_repo_scopes(&[
                "Difflore-Fixtures/Vite".to_owned(),
                String::new(),
                "vitejs/vite".to_owned(),
                "difflore-fixtures/vite".to_owned(),
            ]),
            vec![
                "difflore-fixtures/vite".to_owned(),
                "vitejs/vite".to_owned()
            ]
        );
    }

    #[test]
    fn merge_scored_rule_chunks_keeps_best_duplicate_and_truncates() {
        let merged = merge_scored_rule_chunks(
            vec![
                vec![scored("same", 0.1), scored("low", 0.05)],
                vec![scored("same", 0.2), scored("high", 0.3)],
            ],
            2,
        );

        let ids: Vec<_> = merged.iter().map(|s| s.skill_id.as_str()).collect();
        assert_eq!(ids, vec!["high", "same"]);
        assert!((merged[1].score - 0.2).abs() < f64::EPSILON);
    }

    #[test]
    fn rerank_scored_rule_chunks_prefers_exact_intent_title_matches() {
        let reranked = rerank_scored_rule_chunks(
            vec![
                scored_rule(
                    "generic",
                    "Test new binding paths",
                    "Add tests for success and failure cases.",
                    0.88,
                ),
                scored_rule(
                    "target",
                    "Return 413 for body size limit errors",
                    "When binding fails with MaxBytesError, return HTTP 413.",
                    0.82,
                ),
            ],
            "MaxBytesError should return 413 bind body size",
            2,
        );

        assert_eq!(reranked[0].skill_id, "target");
    }

    #[test]
    fn exact_title_matches_stays_inside_repo_scope() {
        let rules = vec![
            rule_doc("global", "Use named constants", None),
            rule_doc("repo", "Use named constants", Some("tanstack/router")),
            rule_doc("other", "Use named constants", Some("vitejs/vite")),
        ];

        let matches = exact_title_matches(
            &rules,
            "Use named constants",
            &["TanStack/router".to_owned()],
            5,
        );

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].skill_id, "repo");
    }

    #[test]
    fn exact_title_matches_returns_empty_without_current_repo_scope() {
        let rules = vec![
            rule_doc("global", "Use named constants", None),
            rule_doc("other", "Use named constants", Some("vitejs/vite")),
        ];

        let matches = exact_title_matches(&rules, "Use named constants", &[], 5);

        assert!(matches.is_empty());
    }

    #[test]
    fn merge_exact_title_matches_adds_repo_scoped_title_hit_to_ranked_results() {
        let rules = vec![
            rule_doc("exact", "Use named constants", Some("tanstack/router")),
            rule_doc("other-repo", "Use named constants", Some("vitejs/vite")),
        ];

        let merged = merge_exact_title_matches(
            &rules,
            "Use named constants",
            &["tanstack/router".to_owned()],
            vec![scored_rule(
                "semantic",
                "Prefer constants for repeated values",
                "Avoid repeated string literals in tests.",
                0.95,
            )],
            2,
        );

        let ids: Vec<_> = merged.iter().map(|hit| hit.skill_id.as_str()).collect();
        assert_eq!(ids, vec!["exact", "semantic"]);
    }

    #[test]
    fn lexical_terms_keep_negation_and_domain_words() {
        assert_eq!(
            lexical_terms("do not use real git in tests; use command stubs"),
            vec!["not", "real", "git", "tests", "command", "stubs"]
        );
    }
}
