//! MCP (Model Context Protocol) server implementation.
//!
//! Speaks JSON-RPC 2.0 over stdin/stdout. AI coding assistants (Claude Code,
//! Cursor, etc.) connect to `difflore mcp-server` as an MCP stdio transport
//! to query team rules and historical review verdicts while generating code.

mod hook;
mod hook_short_circuit;
mod recall_sampler;
mod schemas;
mod serve_render;
mod server;
mod tools;
mod trust_proof;

pub use hook::{HookRuleContext, fetch_relevant_rules_for_hook, run};
pub(crate) use tools::{HistoricalPr, predict_scope_from_corpus};
pub use tools::{
    detect_active_model, haiku_auto_disable_active, is_haiku_model, origin_to_kind,
    parse_file_patterns,
};

/// Predict a PR's likely edit scope from the local imported review corpus.
///
/// This is the same core algorithm behind the MCP `plan_pr` tool, exposed so
/// local CLI flows can reuse the memory signal without duplicating retrieval
/// logic or depending on the cloud service.
pub async fn predict_pr_scope(
    db: &sqlx::SqlitePool,
    intent: &str,
    top_k: usize,
) -> serde_json::Value {
    let corpus = tools::load_pr_corpus(db).await;
    predict_scope_from_corpus(&corpus, intent, top_k.clamp(1, 20))
}

/// Repo-scoped sibling of [`predict_pr_scope`].
///
/// When the caller knows the current GitHub repo (including fork/upstream
/// aliases), use only same-repo historical PRs so file hints do not bleed
/// across unrelated projects. If no rows match the repo scope, return an empty
/// prediction and let CLI/project-structure advisories carry the experience.
pub async fn predict_pr_scope_for_repos(
    db: &sqlx::SqlitePool,
    intent: &str,
    top_k: usize,
    repo_scopes: &[String],
) -> serde_json::Value {
    let corpus = tools::load_pr_corpus(db).await;
    let scoped = repo_scoped_plan_corpus(&corpus, repo_scopes);
    let no_repo_scope_memory = !repo_scopes.is_empty() && scoped.is_empty();
    let mut prediction = if no_repo_scope_memory {
        predict_scope_from_corpus(&[], intent, top_k.clamp(1, 20))
    } else if scoped.is_empty() {
        predict_scope_from_corpus(&corpus, intent, top_k.clamp(1, 20))
    } else {
        predict_scope_from_corpus(&scoped, intent, top_k.clamp(1, 20))
    };
    if let Some(obj) = prediction.as_object_mut() {
        obj.insert(
            "repo_scope".to_owned(),
            serde_json::json!({
                "requested": repo_scopes,
                "matched_prs": scoped.len(),
                "no_repo_scope_memory": no_repo_scope_memory,
            }),
        );
    }
    prediction
}

pub(crate) fn repo_scoped_plan_corpus(
    corpus: &[HistoricalPr],
    repo_scopes: &[String],
) -> Vec<HistoricalPr> {
    let scopes = repo_scopes
        .iter()
        .map(|repo| repo.trim().to_ascii_lowercase())
        .filter(|repo| !repo.is_empty())
        .collect::<std::collections::BTreeSet<_>>();
    if scopes.is_empty() {
        return Vec::new();
    }
    corpus
        .iter()
        .filter(|pr| scopes.contains(&pr.repo.to_ascii_lowercase()))
        .cloned()
        .collect()
}

#[cfg(test)]
mod repo_scope_tests {
    use super::*;

    #[test]
    fn repo_scoped_plan_corpus_keeps_current_repo_aliases() {
        let corpus = vec![
            historical_pr("gin-gonic/gin", 4542),
            historical_pr("tanstack/router", 7150),
            historical_pr("hibrandonevans/gin", 4542),
        ];

        let scoped = repo_scoped_plan_corpus(
            &corpus,
            &["hibrandonevans/gin".to_owned(), "gin-gonic/gin".to_owned()],
        );

        assert_eq!(scoped.len(), 2);
        assert!(scoped.iter().all(|pr| pr.repo.ends_with("/gin")));
    }

    #[test]
    fn repo_scoped_plan_corpus_returns_empty_without_matches() {
        let corpus = vec![historical_pr("tanstack/router", 7150)];

        let scoped = repo_scoped_plan_corpus(&corpus, &["gin-gonic/gin".to_owned()]);

        assert!(scoped.is_empty());
    }

    fn historical_pr(repo: &str, pr_number: i32) -> HistoricalPr {
        HistoricalPr {
            repo: repo.to_owned(),
            pr_number,
            text: String::new(),
            files: Vec::new(),
            tokens: Vec::new(),
        }
    }
}

// Items re-exported in this scope so submodules can `use super::*;` and
// reach all internal helpers / types without enumerating sibling paths.
pub(crate) use hook::detect_git_remote_owner_repos;
#[cfg(test)]
pub(crate) use hook::parse_github_owner_repo;
pub(crate) use server::{
    AVG_FULL_RULE_TOKENS, McpState, build_cost_meta, emit_trajectory_step, estimate_tokens,
    handle_message, jsonrpc_error, rule_hits_by_origin,
};
#[cfg(test)]
pub(crate) use server::{parse_signature_uri, parse_verdict_uri};
#[cfg(test)]
pub(crate) use tools::{disabled_response, rule_injection_disabled};

#[cfg(test)]
mod tests;
