use sqlx::sqlite::SqlitePool;

use super::types::{
    ContextDebugMetadataRecord, ContextDebugResult, ContextIndexStatusRecord,
    ContextPackMetadataRecord, ContextPackRecord, ContextPackSectionsRecord,
    ContextSourceItemRecord,
};
use crate::errors::CoreError;

use super::assembler;
use super::index_db;
use super::retrieval;
use super::rule_source;
use std::collections::HashSet;

const ORCHESTRATOR_EMBEDDING_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(2500);

fn is_sqlite_locked_error(err: &CoreError) -> bool {
    let msg = err.to_string().to_ascii_lowercase();
    msg.contains("database is locked") || msg.contains("database locked")
}

async fn upsert_rule_chunks_with_retry(
    index_pool: &SqlitePool,
    rules: &[rule_source::RuleDocument],
    embedding_timeout: Option<std::time::Duration>,
) -> Result<index_db::RuleChunksUpsertOutcome, CoreError> {
    const RETRY_DELAYS_MS: &[u64] = &[50, 150, 400];

    let mut attempt = 0usize;
    loop {
        match index_db::upsert_rule_chunks_with_profile_and_timeout(
            index_pool,
            rules,
            embedding_timeout,
        )
        .await
        {
            Ok(outcome) => return Ok(outcome),
            Err(err) if is_sqlite_locked_error(&err) && attempt < RETRY_DELAYS_MS.len() => {
                tokio::time::sleep(std::time::Duration::from_millis(RETRY_DELAYS_MS[attempt]))
                    .await;
                attempt += 1;
            }
            Err(err) => return Err(err),
        }
    }
}

async fn mark_rule_index_current_with_retry(
    index_pool: &SqlitePool,
    state: &rule_source::RuleIndexState,
) -> Result<(), CoreError> {
    const RETRY_DELAYS_MS: &[u64] = &[50, 150, 400];

    let mut attempt = 0usize;
    loop {
        match index_db::mark_rule_index_current(index_pool, state).await {
            Ok(()) => return Ok(()),
            Err(err) if is_sqlite_locked_error(&err) && attempt < RETRY_DELAYS_MS.len() => {
                tokio::time::sleep(std::time::Duration::from_millis(RETRY_DELAYS_MS[attempt]))
                    .await;
                attempt += 1;
            }
            Err(err) => return Err(err),
        }
    }
}

pub async fn ensure_rules_indexed(
    app_pool: &SqlitePool,
    index_pool: &SqlitePool,
) -> Result<usize, CoreError> {
    ensure_rules_indexed_with_embedding_timeout(app_pool, index_pool, None).await
}

pub async fn ensure_rules_indexed_with_embedding_timeout(
    app_pool: &SqlitePool,
    index_pool: &SqlitePool,
    embedding_timeout: Option<std::time::Duration>,
) -> Result<usize, CoreError> {
    let project_root = crate::db::current_project_root();
    let repo_scopes =
        crate::git::detect_github_repo_full_names(project_root.to_string_lossy().as_ref());
    ensure_rules_indexed_for_repo_scopes_with_embedding_timeout(
        app_pool,
        index_pool,
        &repo_scopes,
        embedding_timeout,
    )
    .await
}

pub async fn ensure_rules_indexed_for_repo_scopes_with_embedding_timeout(
    app_pool: &SqlitePool,
    index_pool: &SqlitePool,
    repo_scopes: &[String],
    embedding_timeout: Option<std::time::Duration>,
) -> Result<usize, CoreError> {
    let base_index_state = rule_source::load_rule_index_state(app_pool).await?;
    let all_rules = rule_source::load_rules_from_db(app_pool).await?;
    let rules = filter_rules_for_repo_scopes(all_rules, repo_scopes);
    // Identity of the in-scope rule SET, not just its size. A git remote
    // change can swap in a different but equally-sized set of rules with the
    // same `max_updated_at`; without this signature the freshness check would
    // wrongly skip a re-index and serve the previous scope's chunks.
    let scope_signature = rule_source::scope_signature_from_skill_ids(
        rules.iter().map(|rule| rule.skill_id.as_str()),
    );
    // When the remote embedder is failing, expect the persisted SHA1 profile so
    // a SHA1 index counts as current and we skip a futile full-corpus re-embed
    // on every recall / MCP serve / hook fire. Only relaxes freshness toward the
    // on-disk index; the count / scope / timestamp checks below still force a
    // real re-index when the corpus actually changed.
    let expected_embedding_profile = index_db::effective_embedding_profile_for_freshness(
        index_pool,
        &base_index_state.embedding_profile,
    )
    .await;
    let mut index_state = rule_source::RuleIndexState {
        rule_count: i64::try_from(rules.len()).unwrap_or(i64::MAX),
        max_updated_at: base_index_state.max_updated_at,
        embedding_profile: expected_embedding_profile,
        scope_signature,
    };
    let index_current = match index_db::rule_index_is_current(index_pool, &index_state).await {
        Ok(current) => current,
        Err(err) if is_sqlite_locked_error(&err) => {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            index_db::rule_index_is_current(index_pool, &index_state)
                .await
                .unwrap_or(false)
        }
        Err(err) => {
            return Err(CoreError::Internal(format!(
                "rule index freshness check failed: {err}"
            )));
        }
    };
    if index_current {
        return Ok(usize::try_from(index_state.rule_count).unwrap_or(usize::MAX));
    }

    let outcome = upsert_rule_chunks_with_retry(index_pool, &rules, embedding_timeout).await?;
    index_state.embedding_profile = outcome.embedding_profile;
    mark_rule_index_current_with_retry(index_pool, &index_state).await?;
    Ok(outcome.count)
}

/// Force a full rebuild of the per-project index for `repo_scopes`, bypassing
/// the freshness short-circuit. This always re-embeds the in-scope corpus and
/// prunes chunks whose `skill_id` is not in scope, even when persisted metadata
/// claims the index is current.
///
/// This fully heals the correctness-bearing state (rule_chunks, FTS, freshness
/// meta) but only incrementally updates the ANN graph for re-upserted rows;
/// pruned chunks' ANN vectors are not removed. That is safe: ANN hits are
/// joined back to `rule_chunks` and missing rows are skipped, so a stale ANN
/// entry can only affect ranking, never correctness.
///
/// Returns the number of chunks in the rebuilt index.
pub async fn rebuild_rules_index_for_repo_scopes(
    app_pool: &SqlitePool,
    index_pool: &SqlitePool,
    repo_scopes: &[String],
    embedding_timeout: Option<std::time::Duration>,
) -> Result<usize, CoreError> {
    let base_index_state = rule_source::load_rule_index_state(app_pool).await?;
    let all_rules = rule_source::load_rules_from_db(app_pool).await?;
    let rules = filter_rules_for_repo_scopes(all_rules, repo_scopes);
    let scope_signature = rule_source::scope_signature_from_skill_ids(
        rules.iter().map(|rule| rule.skill_id.as_str()),
    );
    // Two passes force a full rewrite (the upsert path can skip rows with an
    // unchanged signature/profile): pass 1 prunes all chunks, pass 2 re-embeds
    // every in-scope row and rebuilds FTS.
    upsert_rule_chunks_with_retry(index_pool, &[], embedding_timeout).await?;
    let outcome = upsert_rule_chunks_with_retry(index_pool, &rules, embedding_timeout).await?;
    let index_state = rule_source::RuleIndexState {
        rule_count: i64::try_from(rules.len()).unwrap_or(i64::MAX),
        max_updated_at: base_index_state.max_updated_at,
        embedding_profile: outcome.embedding_profile,
        scope_signature,
    };
    mark_rule_index_current_with_retry(index_pool, &index_state).await?;
    Ok(outcome.count)
}

/// Reserved per-project hash for the shared cross-repo "starter" index. Not a
/// real repo hash (those are 12 hex chars), so it never collides with a project
/// directory under `~/.difflore/projects/`.
const CROSS_REPO_STARTER_HASH: &str = "__cross_repo_starter__";

/// Freshness state for the shared cross-repo starter index. Scope-agnostic
/// (`scope_signature: None`): the starter always holds the whole corpus, so
/// corpus size + `max_updated_at` fully capture a change. Cheap to compute
/// (one COUNT/MAX query), letting the latency-critical hook check whether the
/// starter is current without loading every rule.
async fn cross_repo_starter_state(
    app_pool: &SqlitePool,
) -> Result<rule_source::RuleIndexState, CoreError> {
    let base = rule_source::load_rule_index_state(app_pool).await?;
    Ok(rule_source::RuleIndexState {
        rule_count: base.rule_count,
        max_updated_at: base.max_updated_at,
        embedding_profile: format!("sha1:local:{}", crate::context::embedding::EMBEDDING_DIM),
        scope_signature: None,
    })
}

pub async fn ensure_cross_repo_starter_indexed(
    app_pool: &SqlitePool,
) -> Result<SqlitePool, CoreError> {
    let pool = index_db::get_pool_for_project(CROSS_REPO_STARTER_HASH).await?;
    let state = cross_repo_starter_state(app_pool).await?;
    // An already-current starter (common on repeat cold-start recalls) skips
    // loading the whole corpus.
    if index_db::rule_index_is_current(&pool, &state)
        .await
        .unwrap_or(false)
    {
        return Ok(pool);
    }
    let all_rules = rule_source::load_rules_from_db(app_pool).await?;
    if all_rules.is_empty() {
        return Ok(pool);
    }
    index_db::upsert_rule_chunks_isolated(&pool, &all_rules).await?;
    index_db::mark_rule_index_current(&pool, &state).await?;
    Ok(pool)
}

/// Return the cross-repo starter pool only if it is already built and current.
/// This never builds on the hook hot path; callers use it only when a slower
/// path has already prepared the starter index.
pub async fn cross_repo_starter_index_if_current(
    app_pool: &SqlitePool,
) -> Result<Option<SqlitePool>, CoreError> {
    let pool = index_db::get_pool_for_project(CROSS_REPO_STARTER_HASH).await?;
    let state = cross_repo_starter_state(app_pool).await?;
    if index_db::rule_index_is_current(&pool, &state)
        .await
        .unwrap_or(false)
    {
        Ok(Some(pool))
    } else {
        Ok(None)
    }
}

fn filter_rules_for_repo_scopes(
    rules: Vec<rule_source::RuleDocument>,
    repo_scopes: &[String],
) -> Vec<rule_source::RuleDocument> {
    let scopes: HashSet<String> = repo_scopes
        .iter()
        .map(|scope| scope.trim().to_ascii_lowercase())
        .filter(|scope| !scope.is_empty())
        .collect();
    if scopes.is_empty() {
        // No repo scope -> copy nothing into the per-project index. The index is
        // the scope boundary for recall / fix / review; populating it with the
        // whole machine corpus when there is no repo identity would let a
        // no-remote checkout recall every repo's rules, the cross-repo leak the
        // project-scope invariant forbids.
        return Vec::new();
    }
    rules
        .into_iter()
        .filter(|rule| {
            rule.repo_scope
                .as_deref()
                .is_some_and(|scope| scopes.contains(&scope.to_ascii_lowercase()))
        })
        .collect()
}

/// Resolve the per-project index DB hash from a data.db `project_id`. Looks
/// up `projects.path` and derives the stable path hash. Falls back to the
/// current working directory's project root when the `project_id` is empty
/// or the row is missing — this keeps hook/MCP paths working when the
/// caller hasn't resolved a `project_id` yet.
async fn project_hash_for(app_pool: &SqlitePool, project_id: &str) -> String {
    if !project_id.is_empty()
        && let Ok(Some(row)) = sqlx::query_scalar!(
            r#"SELECT path as "path!: String" FROM projects WHERE id = ?1"#,
            project_id
        )
        .fetch_optional(app_pool)
        .await
    {
        return crate::db::project_hash_from_root(std::path::Path::new(&row));
    }
    // Fallback: derive from the current working directory when project_id
    // isn't known.
    let root = crate::db::current_project_root();
    crate::db::project_hash_from_root(&root)
}

/// Detect every GitHub `owner/repo` scope reachable from the project's
/// git remotes (`origin` first, then `upstream` — see
/// `git::detect_github_repo_full_names`). Returning the full `Vec` lets
/// callers retrieve rules from the current fork and its upstream.
async fn repo_scopes_for(app_pool: &SqlitePool, project_id: &str) -> Vec<String> {
    if !project_id.is_empty()
        && let Ok(Some(row)) = sqlx::query_scalar!(
            r#"SELECT path as "path!: String" FROM projects WHERE id = ?1"#,
            project_id
        )
        .fetch_optional(app_pool)
        .await
    {
        return crate::git::detect_github_repo_full_names(&row);
    }

    let root = crate::db::current_project_root();
    crate::git::detect_github_repo_full_names(&root.to_string_lossy())
}

fn rule_title(content: &str) -> Option<String> {
    content
        .lines()
        .find_map(|line| line.strip_prefix("Rule Name:").map(str::trim))
        .filter(|title| !title.is_empty())
        .map(ToOwned::to_owned)
}

fn rules_to_source_items(scored: &[retrieval::ScoredRuleChunk]) -> Vec<ContextSourceItemRecord> {
    scored
        .iter()
        .map(|s| ContextSourceItemRecord {
            source_type: "rule".into(),
            source_id: s.skill_id.clone(),
            relative_path: None,
            start_line: None,
            end_line: None,
            title: rule_title(&s.content),
            content: s.content.clone(),
            score: s.score,
        })
        .collect()
}

pub async fn prepare(
    app_pool: &SqlitePool,
    project_id: &str,
    engine: &str,
    query: &str,
    task_intent: Option<&str>,
) -> Result<ContextPackRecord, CoreError> {
    prepare_with_hint(app_pool, project_id, engine, query, task_intent, None).await
}

// Multi-scope rule fan-out for the orchestrator. Delegates to the shared
// retrieval helper with orchestrator defaults: a single query and an
// `eligible_skill_ids` allow-list.
#[allow(clippy::too_many_arguments)]
async fn retrieve_rules_for_repo_scopes(
    index_pool: &SqlitePool,
    query: &str,
    confidence_map: Option<&std::collections::HashMap<String, f64>>,
    eligible_skill_ids: Option<&HashSet<String>>,
    age_days_map: Option<&std::collections::HashMap<String, f32>>,
    target_file: Option<&str>,
    repo_scopes: &[String],
    top_k: usize,
) -> Result<Vec<retrieval::ScoredRuleChunk>, CoreError> {
    retrieval::retrieve_rules_fanout(
        index_pool,
        retrieval::RuleFanoutQuery {
            query,
            // Passing the same string collapses `retrieval_query_variants` to a
            // single variant and makes the re-rank key `query`.
            lexical_query: query,
            top_k,
            confidence_map,
            eligible_skill_ids,
            age_days_map,
            target_file,
            repo_scopes,
            ann_enabled: true,
            embedding_timeout: Some(ORCHESTRATOR_EMBEDDING_TIMEOUT),
            // Context-pack/fix assembly keeps fast-degrade; cold-start retry is
            // scoped to interactive recall.
            cold_start_retry: false,
            adaptive_prune: false,
        },
    )
    .await
}

/// Like `prepare` but with a `file_path` hint that drives the SQL-side
/// language filter on the rule index. `language IS NULL` rows stay eligible so
/// rules without inferred language metadata are not dropped.
pub async fn prepare_with_hint(
    app_pool: &SqlitePool,
    project_id: &str,
    engine: &str,
    query: &str,
    task_intent: Option<&str>,
    file_path_hint: Option<&str>,
) -> Result<ContextPackRecord, CoreError> {
    let repo_scopes = repo_scopes_for(app_pool, project_id).await;
    prepare_with_hint_and_repo_scopes(
        app_pool,
        project_id,
        engine,
        query,
        task_intent,
        file_path_hint,
        &repo_scopes,
    )
    .await
}

pub async fn prepare_with_hint_and_repo_scopes(
    app_pool: &SqlitePool,
    project_id: &str,
    engine: &str,
    query: &str,
    task_intent: Option<&str>,
    file_path_hint: Option<&str>,
    repo_scopes: &[String],
) -> Result<ContextPackRecord, CoreError> {
    prepare_with_hint_and_repo_scopes_with_top_k(
        app_pool,
        project_id,
        engine,
        query,
        task_intent,
        file_path_hint,
        repo_scopes,
        None,
    )
    .await
}

/// Same as [`prepare_with_hint_and_repo_scopes`] but lets the caller widen
/// (or narrow) the retrieved rule candidate pool via `top_k_override`.
///
/// `None` keeps the production default ([`crate::context::DEFAULT_TOP_K_RULES`]).
/// Review can request a deeper candidate pool for the applicability judge; the
/// assembler's `rule_token_budget` still bounds what reaches the prompt.
#[allow(clippy::too_many_arguments)]
pub async fn prepare_with_hint_and_repo_scopes_with_top_k(
    app_pool: &SqlitePool,
    project_id: &str,
    engine: &str,
    query: &str,
    task_intent: Option<&str>,
    file_path_hint: Option<&str>,
    repo_scopes: &[String],
    top_k_override: Option<usize>,
) -> Result<ContextPackRecord, CoreError> {
    let top_k = top_k_override.unwrap_or(crate::context::DEFAULT_TOP_K_RULES);
    let project_hash = project_hash_for(app_pool, project_id).await;
    let index_pool = index_db::get_pool_for_project(&project_hash).await?;
    // Index with the same scopes we retrieve below (not cwd-detected), so the
    // per-project index matches the retrieval scope. With no scope,
    // `filter_rules_for_repo_scopes` copies nothing -> empty index -> empty
    // retrieval, upholding the project-scope invariant on the review/fix path.
    ensure_rules_indexed_for_repo_scopes_with_embedding_timeout(
        app_pool,
        &index_pool,
        repo_scopes,
        Some(ORCHESTRATOR_EMBEDDING_TIMEOUT),
    )
    .await?;

    let eligible_rules = rule_source::load_rules_from_db_for_engine(app_pool, Some(engine)).await?;
    let eligible_ids: HashSet<String> = eligible_rules.iter().map(|r| r.skill_id.clone()).collect();
    let confidence_map: std::collections::HashMap<String, f64> = eligible_rules
        .iter()
        .map(|r| (r.skill_id.clone(), r.confidence))
        .collect();
    let ranking_inputs = rule_source::load_rule_ranking_inputs(app_pool).await;

    // Forward the file path as the strict-cascade `target_file`: mismatched
    // `file_patterns` are dropped before scoring, and the fan-out derives the
    // SQL-level language filter from the same path.
    let all_rule_results = retrieve_rules_for_repo_scopes(
        &index_pool,
        query,
        Some(&confidence_map),
        Some(&eligible_ids),
        ranking_inputs.age_days_map.as_ref(),
        file_path_hint,
        repo_scopes,
        top_k,
    )
    .await?;
    let rule_results: Vec<_> = all_rule_results
        .into_iter()
        .filter(|r| eligible_ids.contains(&r.skill_id))
        .collect();

    let matched_skill_ids: Vec<String> = rule_results.iter().map(|r| r.skill_id.clone()).collect();
    let examples_map = rule_source::load_rule_examples_batch(app_pool, &matched_skill_ids).await?;

    let intent = task_intent.unwrap_or("generation");

    // Token budgets from app settings, falling back to compile-time defaults.
    // Best-effort: a missing or unreadable settings file must not block
    // context preparation.
    let budgets = match crate::settings::get().await {
        Ok(s) => assembler::TokenBudgets::from_overrides(Some(s.context_engine.rule_token_budget)),
        Err(_) => assembler::TokenBudgets::default(),
    };

    let assembled = assembler::assemble_with_examples_and_budgets(
        &rule_results,
        query,
        intent,
        Some(&examples_map),
        budgets,
    );

    let rule_ctx = rules_to_source_items(&rule_results);

    let rules_text: Option<String> = if assembled.rule_sections.is_empty() {
        None
    } else {
        Some(
            assembled
                .rule_sections
                .iter()
                .map(|s| s.content.clone())
                .collect::<Vec<_>>()
                .join("\n\n"),
        )
    };

    let trace_id = uuid::Uuid::new_v4().to_string();

    Ok(ContextPackRecord {
        task_intent: intent.to_owned(),
        project_id: project_id.to_owned(),
        engine: engine.to_owned(),
        query: query.to_owned(),
        rule_context: rule_ctx,
        review_context: vec![],
        sections: ContextPackSectionsRecord {
            introduction: format!("Context for task: {intent}"),
            rules: rules_text,
            review: None,
            closing: "End of context.".into(),
        },
        token_budget: i64::try_from(budgets.rule).unwrap_or(i64::MAX),
        estimated_tokens: i64::try_from(assembled.estimated_tokens).unwrap_or(i64::MAX),
        trace_id,
        prompt_text: None,
        metadata: ContextPackMetadataRecord {
            rule_count: i64::try_from(assembled.rule_count).unwrap_or(i64::MAX),
            review_count: 0,
            review_reason: None,
            review_source_summary: None,
            selected_review_count: Some(0),
            recent_run_hint: None,
        },
    })
}

pub async fn prepare_review_fix(
    app_pool: &SqlitePool,
    project_id: &str,
    engine: &str,
    query: &str,
    review_item_id: &str,
) -> Result<ContextPackRecord, CoreError> {
    let item_row = sqlx::query!(
        "SELECT id, file_path, diff_content, status, source_kind FROM review_items WHERE id = ?1",
        review_item_id
    )
    .fetch_optional(app_pool)
    .await?
    .ok_or_else(|| CoreError::Internal(format!("Review item not found: {review_item_id}")))?;

    let comments = sqlx::query!(
        "SELECT content, line_number FROM review_comments WHERE review_item_id = ?1 ORDER BY line_number ASC",
        review_item_id,
    )
    .fetch_all(app_pool)
    .await?;

    let mut review_text = format!(
        "## Review Item: {}\nFile: {}\nStatus: {}\n\n### Diff\n```\n{}\n```\n",
        item_row.id, item_row.file_path, item_row.status, item_row.diff_content
    );
    if !comments.is_empty() {
        review_text.push_str("\n### Comments\n");
        for c in &comments {
            if c.line_number > 0 {
                review_text.push_str(&format!("- Line {}: {}\n", c.line_number, c.content));
            } else {
                review_text.push_str(&format!("- General: {}\n", c.content));
            }
        }
    }

    let review_ctx = vec![ContextSourceItemRecord {
        source_type: "review_item".into(),
        source_id: review_item_id.to_owned(),
        relative_path: Some(item_row.file_path.clone()),
        start_line: None,
        end_line: None,
        title: Some(format!("Review: {}", item_row.file_path)),
        content: review_text.clone(),
        score: 1.0,
    }];

    let mut pack = prepare(app_pool, project_id, engine, query, Some("review_fix")).await?;
    pack.review_context = review_ctx;
    pack.sections.review = Some(review_text);
    pack.metadata.review_count = 1;
    pack.metadata.review_reason = Some("targeted_fix".into());
    pack.metadata.review_source_summary = Some(format!(
        "{} ({} comment(s))",
        item_row.source_kind,
        comments.len()
    ));
    pack.metadata.selected_review_count = Some(1);
    Ok(pack)
}

pub async fn debug_retrieval(
    app_pool: &SqlitePool,
    project_id: &str,
    engine: &str,
    query: &str,
) -> Result<ContextDebugResult, CoreError> {
    let project_hash = project_hash_for(app_pool, project_id).await;
    let index_pool = index_db::get_pool_for_project(&project_hash).await?;
    let repo_scopes = repo_scopes_for(app_pool, project_id).await;
    // Index for the same repo scopes this path retrieves with.
    ensure_rules_indexed_for_repo_scopes_with_embedding_timeout(
        app_pool,
        &index_pool,
        &repo_scopes,
        Some(ORCHESTRATOR_EMBEDDING_TIMEOUT),
    )
    .await?;

    let eligible_rules = rule_source::load_rules_from_db_for_engine(app_pool, Some(engine)).await?;
    let eligible_ids: HashSet<String> = eligible_rules.iter().map(|r| r.skill_id.clone()).collect();
    let confidence_map: std::collections::HashMap<String, f64> = eligible_rules
        .iter()
        .map(|r| (r.skill_id.clone(), r.confidence))
        .collect();
    let ranking_inputs = rule_source::load_rule_ranking_inputs(app_pool).await;
    let all_rule_results = retrieve_rules_for_repo_scopes(
        &index_pool,
        query,
        Some(&confidence_map),
        Some(&eligible_ids),
        ranking_inputs.age_days_map.as_ref(),
        None,
        &repo_scopes,
        crate::context::DEFAULT_TOP_K_RULES,
    )
    .await?;
    let rule_results: Vec<_> = all_rule_results
        .into_iter()
        .filter(|r| eligible_ids.contains(&r.skill_id))
        .collect();

    let rule_candidates = rules_to_source_items(&rule_results);

    let total_tokens: usize = rule_candidates
        .iter()
        .map(|item| item.content.len().div_ceil(4))
        .sum();

    let trace_id = uuid::Uuid::new_v4().to_string();
    let embedding_profile = crate::context::embedding::active_embedding_profile().await;

    Ok(ContextDebugResult {
        project_id: project_id.to_owned(),
        query: query.to_owned(),
        engine: engine.to_owned(),
        status: "ready".into(),
        rule_candidates: rule_candidates.clone(),
        review_candidates: vec![],
        trace_id,
        estimated_tokens: total_tokens as i64,
        metadata: ContextDebugMetadataRecord {
            rule_count: rule_candidates.len() as i64,
            review_count: 0,
            reason: None,
            review_reason: None,
            review_source_summary: None,
            selected_review_count: Some(0),
            recent_run_hint: None,
            retrieval_mode: Some(format!("hybrid:{embedding_profile}")),
            rerank_strategy: Some("rrf_semantic_fts_confidence_age".into()),
            user_action_type: None,
            selected_rule_count: Some(rule_candidates.len() as i64),
        },
    })
}

pub async fn get_index_status(
    app_pool: &SqlitePool,
    project_id: &str,
) -> Result<ContextIndexStatusRecord, CoreError> {
    let project_hash = project_hash_for(app_pool, project_id).await;
    let index_pool = index_db::get_pool_for_project(&project_hash).await?;

    let rule_count: i64 = sqlx::query_scalar!("SELECT COUNT(*) FROM rule_chunks")
        .fetch_one(&index_pool)
        .await
        .unwrap_or(0);

    Ok(ContextIndexStatusRecord {
        project_id: project_id.to_owned(),
        status: "ready".into(),
        rule_chunk_count: rule_count,
        last_indexed_at: None,
        active_job_id: None,
        error: None,
        reason: None,
    })
}

pub async fn rebuild_index(
    app_pool: &SqlitePool,
    project_id: &str,
    _force: bool,
) -> Result<ContextIndexStatusRecord, CoreError> {
    let project_hash = project_hash_for(app_pool, project_id).await;
    let index_pool = index_db::get_pool_for_project(&project_hash).await?;

    let rule_count = ensure_rules_indexed(app_pool, &index_pool).await?;

    Ok(ContextIndexStatusRecord {
        project_id: project_id.to_owned(),
        status: "ready".into(),
        rule_chunk_count: rule_count as i64,
        last_indexed_at: Some(chrono::Utc::now().to_rfc3339()),
        active_job_id: None,
        error: None,
        reason: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(skill_id: &str, repo_scope: Option<&str>) -> rule_source::RuleDocument {
        rule_source::RuleDocument {
            skill_id: skill_id.to_owned(),
            title: format!("rule {skill_id}"),
            content: format!("body {skill_id}"),
            confidence: 0.7,
            file_patterns: None,
            language: None,
            repo_scope: repo_scope.map(str::to_owned),
        }
    }

    #[tokio::test]
    async fn cross_repo_starter_not_served_when_corpus_empty() {
        // Critical hook-safety property: with no rules, the starter index is
        // neither built nor served, so the cold-start fallback can never inject
        // a half-built or empty index into the agent.
        use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
        use std::str::FromStr;

        let _home = crate::db::shared_test_home();
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .foreign_keys(true);
        let app = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        crate::db::run_migrations(&app).await.unwrap();

        let pool = ensure_cross_repo_starter_indexed(&app).await.unwrap();
        let chunks: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM rule_chunks")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(chunks, 0, "an empty corpus must index no starter rules");
        assert!(
            cross_repo_starter_index_if_current(&app)
                .await
                .unwrap()
                .is_none(),
            "an empty / unbuilt starter must never be served to the hot path"
        );
    }

    #[test]
    fn filter_rules_for_repo_scopes_fails_closed_on_empty_scope() {
        let rules = vec![
            doc("a", Some("acme/widgets")),
            doc("b", Some("other/repo")),
            doc("c", None),
        ];
        // Invariant: with no repo scope, copy NOTHING into the per-project index
        // (no current-repo identity must never inject global / other-repo memory).
        assert!(filter_rules_for_repo_scopes(rules.clone(), &[]).is_empty());
        assert!(filter_rules_for_repo_scopes(rules.clone(), &["   ".to_owned()]).is_empty());

        // A real scope returns only that scope's rules (exact, case-insensitive),
        // never the unattributed (None) or other-repo rows.
        let scoped = filter_rules_for_repo_scopes(rules, &["ACME/Widgets".to_owned()]);
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].skill_id, "a");
    }
}
