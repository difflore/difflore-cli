use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

use crate::context::retrieval::ScoredRuleChunk;
use crate::context::rule_source::RuleDocument;
use crate::context::types::RuleMatchEvidenceRecord;
use crate::context::{EmbeddingDiagnostics, gather_embedding_diagnostics_with_activity};
use crate::observability::injection_log::InjectionDropReason;
use crate::observability::trajectory::TrajectoryStep;

use super::super::serve_render::{RuleServe, serve_and_record, serve_record_err_prefix};
use super::super::{
    AVG_FULL_RULE_TOKENS, McpState, build_cost_meta, emit_trajectory_step, estimate_tokens,
    rule_hits_by_origin,
};
use super::evidence::{
    build_match_evidence, explicit_recall_kind_gate_multiplier, fetch_skills_by_ids,
    has_strict_file_patterns_match, parse_file_patterns, rule_preview,
    strict_file_match_ids_for_meta, strict_file_match_ids_for_rules,
};
use super::serve_stats::{
    build_empty_recall_retry_query, drain_mcp_query_outbox, enqueue_mcp_query_outbox,
    rerank_scored_rule_chunks_for_mcp_by_strict_file_matches,
};
use super::validate::{
    MCP_TEXT_ARG_CHAR_LIMIT, disabled_response, rule_injection_disabled, validate_mcp_text_arg,
};

const fn public_relevance_score(score: f64) -> f64 {
    if score.is_finite() {
        score.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// Parsed-and-validated `search_rules` inputs. String fields borrow from the
/// incoming `args` so the orchestrator can pass them through to the retrieval
/// and response phases without re-deriving them.
struct SearchRulesQuery<'a> {
    file: &'a str,
    intent: &'a str,
    session_id: &'a str,
    /// `top_k` after the schema clamp and the deep-recall sampler bump.
    top_k: usize,
    /// `Some(file)` unless `file == "unknown"`; the gate/index target.
    target_file: Option<&'a str>,
    repo_scopes: Vec<String>,
}

/// Per-stage candidate counts threaded into the recall trace. Grouping them
/// avoids a wide positional argument list at every trace site.
#[derive(Clone, Copy, Default)]
struct CandidateCounts {
    candidate_limit: usize,
    retrieved: usize,
    after_retry: usize,
    after_exact_merge: usize,
    after_strict_rerank: usize,
    after_intent_gate: usize,
    after_relevance_gate: usize,
}

/// Output of the retrieval + gating pipeline: the surviving candidates plus
/// the metadata, trace counts, and flags the response phases need.
struct RetrievalOutcome {
    scored: Vec<ScoredRuleChunk>,
    meta_map: HashMap<String, super::evidence::SkillDetailRow>,
    strict_skill_ids: HashSet<String>,
    rules_indexed: usize,
    embedding_diag: EmbeddingDiagnostics,
    /// Effective query after any deterministic empty-recall retry.
    query: String,
    retrieval_attempts: u32,
    retry_kind: Option<&'static str>,
    cross_repo_starter: bool,
    counts: CandidateCounts,
}

pub(crate) async fn tool_search_rules(
    state: &McpState,
    args: &Value,
) -> Result<Value, (i32, String)> {
    if let Some(reason) = rule_injection_disabled() {
        crate::observability::injection_log::record_with_reason(
            "mcp_tool",
            0,
            None,
            Some(InjectionDropReason::Disabled),
        );
        return Ok(disabled_response(reason));
    }
    let parsed = parse_search_rules_args(&state.db, args).await?;
    let outcome = retrieve_and_gate_rules(state, &parsed).await?;
    if outcome.scored.is_empty() {
        build_empty_response(state, &parsed, &outcome).await
    } else {
        build_results_response(state, &parsed, outcome).await
    }
}

/// Parse + validate the request args, resolve the repo scopes, and apply the
/// `top_k` clamp + deep-recall sampler bump.
async fn parse_search_rules_args<'a>(
    db: &crate::SqlitePool,
    args: &'a Value,
) -> Result<SearchRulesQuery<'a>, (i32, String)> {
    let file = args
        .get("file")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let intent = args
        .get("intent")
        .and_then(|v| v.as_str())
        .ok_or((-32602, "Missing required parameter: intent".to_owned()))?;
    let session_id = args
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("mcp-server");
    validate_mcp_text_arg("file", file, MCP_TEXT_ARG_CHAR_LIMIT)?;
    validate_mcp_text_arg("intent", intent, MCP_TEXT_ARG_CHAR_LIMIT)?;
    // Clamp `top_k` to the schema range so over-asks get a bounded response.
    let requested_top_k = args
        .get("top_k")
        .and_then(Value::as_u64)
        .map_or(5, |n| n.clamp(1, 50) as usize);
    // Low-rate sampler: occasionally widen default serves from 5 to 8 so deeper
    // ranks get measured. Explicit caller choices pass through.
    let top_k = super::super::recall_sampler::maybe_bump_top_k(
        requested_top_k,
        crate::infra::env::deep_recall_sample_rate(),
    );
    let repo_scopes = repo_scopes_for_search_rules(db, args).await?;
    let target_file = if file == "unknown" { None } else { Some(file) };
    Ok(SearchRulesQuery {
        file,
        intent,
        session_id,
        top_k,
        target_file,
        repo_scopes,
    })
}

/// Retrieval + gating pipeline: load + index rules, run vector recall (with a
/// deterministic empty retry), merge exact-title strict matches, rerank, and
/// apply the intent-alignment + explicit-recall gates plus the cross-repo
/// cold-start fallback. Returns the survivors and the trace counts.
async fn retrieve_and_gate_rules(
    state: &McpState,
    parsed: &SearchRulesQuery<'_>,
) -> Result<RetrievalOutcome, (i32, String)> {
    let SearchRulesQuery {
        file,
        intent,
        top_k,
        target_file,
        repo_scopes,
        ..
    } = parsed;
    let (file, intent, top_k, target_file) = (*file, *intent, *top_k, *target_file);

    let mut query = crate::context::retrieval::build_recall_query_with_signals(file, intent);

    let rules = crate::context::rule_source::load_rules_from_db(&state.db)
        .await
        .map_err(|e| (-32603, format!("Failed to load rules: {e}")))?;
    let index_pool = state
        .resolve_index_pool()
        .await
        .map_err(|e| (-32603, format!("Failed to open index DB: {e}")))?;
    let rules_indexed = if repo_scopes.is_empty() {
        0
    } else {
        match crate::context::orchestrator::ensure_rules_indexed_for_repo_scopes_local_embeddings(
            &state.db,
            &index_pool,
            repo_scopes,
        )
        .await
        {
            Ok(count) => count,
            Err(e) => {
                if crate::infra::env::debug_telemetry() {
                    eprintln!("[difflore-mcp] search_rules index freshness check failed: {e}");
                }
                0
            }
        }
    };
    // Gathered once post-index so every `_meta` return site reports the same
    // embedding-health snapshot without re-querying the index pool per branch.
    let embedding_diag: EmbeddingDiagnostics =
        gather_embedding_diagnostics_with_activity(&index_pool).await;

    let ranking_inputs = crate::context::rule_source::load_rule_ranking_inputs(&state.db).await;
    let candidate_limit = top_k.saturating_mul(5).clamp(top_k, 50);
    let mut scored = super::serve_stats::retrieve_rules_with_repo_scopes(
        &index_pool,
        super::serve_stats::RetrieveRulesArgs {
            query: &query,
            lexical_query: Some(intent),
            top_k: candidate_limit,
            target_file,
            repo_scopes,
            confidence_map: ranking_inputs.confidence_map.as_ref(),
            age_days_map: ranking_inputs.age_days_map.as_ref(),
            ann_enabled: true,
            local_query_embedding: true,
            embedding_timeout: None,
            strict_file_scope: false,
            adaptive_prune: false,
        },
    )
    .await
    .map_err(|e| (-32603, format!("Rule retrieval failed: {e}")))?;
    let candidates_retrieved = scored.len();
    let mut retrieval_attempts = 1;
    let mut retry_kind: Option<&'static str> = None;
    if scored.is_empty()
        && let Some(retry_query) = build_empty_recall_retry_query(file, intent)
    {
        retrieval_attempts = 2;
        retry_kind = Some("deterministic_empty_retry");
        let retry_scored = super::serve_stats::retrieve_rules_with_repo_scopes(
            &index_pool,
            super::serve_stats::RetrieveRulesArgs {
                query: &retry_query,
                lexical_query: None,
                top_k: candidate_limit,
                target_file,
                repo_scopes,
                confidence_map: ranking_inputs.confidence_map.as_ref(),
                age_days_map: ranking_inputs.age_days_map.as_ref(),
                ann_enabled: true,
                local_query_embedding: true,
                embedding_timeout: None,
                strict_file_scope: false,
                adaptive_prune: false,
            },
        )
        .await
        .map_err(|e| (-32603, format!("Rule retrieval retry failed: {e}")))?;
        query = retry_query;
        scored = retry_scored;
    }
    let candidates_after_retry = scored.len();
    merge_exact_title_strict_matches(
        &mut scored,
        exact_title_strict_match_chunks(&rules, intent, target_file, repo_scopes),
    );
    let candidates_after_exact_merge = scored.len();
    // Batch-fetch skill metadata BEFORE the strict rerank (moved up from the
    // evidence-building site) so the post-gate arbitration below can read
    // origin/source facts without a second query. The id set is the full
    // candidate window, so every survivor of the rerank + gates is covered;
    // only the late cross-repo starter set needs a supplemental fetch.
    let candidate_ids: Vec<String> = scored.iter().map(|s| s.skill_id.clone()).collect();
    let meta_map = fetch_skills_by_ids(&state.db, &candidate_ids)
        .await
        .map_err(|e| (-32603, format!("Failed to fetch rule metadata: {e}")))?;
    let strict_skill_ids = strict_file_match_ids_for_rules(&rules, target_file);
    scored = rerank_scored_rule_chunks_for_mcp_by_strict_file_matches(
        scored,
        intent,
        top_k,
        &strict_skill_ids,
    );
    let candidates_after_strict_rerank = scored.len();

    // Drop topically adjacent rules whose directive does not share the query
    // intent. Strong exact/title/lexical hits remain exempt.
    crate::context::retrieval::apply_intent_alignment_gate(&mut scored, intent);
    let candidates_after_intent_gate = scored.len();

    // Final relevance gate for explicit recall: wrong-file or low-signal queries
    // return no rules rather than weak filler; strong matches clear it easily.
    crate::context::retrieval::apply_explicit_recall_threshold(&mut scored);
    let candidates_after_relevance_gate = scored.len();

    // Cold-start fallback: only repos with no scoped memory get strict-file
    // cross-repo suggestions.
    let mut cross_repo_starter = false;
    if should_try_cross_repo_starter(scored.is_empty(), repo_scopes, rules_indexed, target_file)
        && let Some(tf) = target_file
    {
        let cross = super::serve_stats::cross_repo_starter_scored(
            &state.db,
            &query,
            tf,
            ranking_inputs.confidence_map.as_ref(),
            ranking_inputs.age_days_map.as_ref(),
            top_k,
        )
        .await;
        if !cross.is_empty() {
            scored = cross;
            cross_repo_starter = true;
        }
    }

    Ok(RetrievalOutcome {
        scored,
        meta_map,
        strict_skill_ids,
        rules_indexed,
        embedding_diag,
        query,
        retrieval_attempts,
        retry_kind,
        cross_repo_starter,
        counts: CandidateCounts {
            candidate_limit,
            retrieved: candidates_retrieved,
            after_retry: candidates_after_retry,
            after_exact_merge: candidates_after_exact_merge,
            after_strict_rerank: candidates_after_strict_rerank,
            after_intent_gate: candidates_after_intent_gate,
            after_relevance_gate: candidates_after_relevance_gate,
        },
    })
}

const fn should_try_cross_repo_starter(
    scored_empty: bool,
    repo_scopes: &[String],
    rules_indexed: usize,
    target_file: Option<&str>,
) -> bool {
    scored_empty && !repo_scopes.is_empty() && rules_indexed == 0 && target_file.is_some()
}

/// Build the empty-result response: emit the no-rules telemetry (injection log,
/// outbox, serve ledger + cloud event, trajectory) and return the MCP payload.
async fn build_empty_response(
    state: &McpState,
    parsed: &SearchRulesQuery<'_>,
    outcome: &RetrievalOutcome,
) -> Result<Value, (i32, String)> {
    let SearchRulesQuery {
        file,
        intent,
        session_id,
        top_k,
        target_file,
        repo_scopes,
    } = parsed;
    let (file, intent, session_id, top_k, target_file) =
        (*file, *intent, *session_id, *top_k, *target_file);
    let RetrievalOutcome {
        rules_indexed,
        embedding_diag,
        query,
        retrieval_attempts,
        retry_kind,
        counts,
        ..
    } = outcome;
    let (rules_indexed, retrieval_attempts, retry_kind) =
        (*rules_indexed, *retrieval_attempts, *retry_kind);
    crate::observability::injection_log::record_with_reason(
        "mcp_tool",
        0,
        target_file,
        Some(if repo_scopes.is_empty() {
            InjectionDropReason::NoRepoScope
        } else {
            InjectionDropReason::RetrievalEmpty
        }),
    );
    // Track empty results without blocking the response.
    {
        let file = file.to_owned();
        let intent = intent.to_owned();
        let repo_full_name: Option<String> = repo_scopes.first().cloned();
        let cloud = state.cloud.clone();
        let db = state.db.clone();
        enqueue_mcp_query_outbox(
            &state.db,
            super::serve_stats::McpQueryOutboxEntry {
                file: &file,
                intent: &intent,
                rules_injected: 0,
                strict_match_count: 0,
                rule_titles: &[],
                rule_ids: &[],
                client_label: "mcp-server-search",
                repo_full_name: repo_full_name.as_deref(),
            },
        )
        .await;
        tokio::spawn(async move {
            let _ = drain_mcp_query_outbox(&db, &cloud, 8).await;
        });
    }
    let no_current_repo_rules = no_current_repo_rules(repo_scopes, rules_indexed);
    let empty_message = empty_recall_message(repo_scopes, rules_indexed, retry_kind);
    let body = json!({
        "results": [],
        "message": empty_message,
        "retrieval": {
            "attempts": retrieval_attempts,
            "retry_kind": retry_kind,
            "repo_scopes": repo_scopes,
            "no_current_repo_rules": no_current_repo_rules,
            "trace": recall_trace_json(*counts, 0, 0),
        }
    });
    let text = serde_json::to_string(&body)
        .unwrap_or_else(|_| "{\"results\":[],\"message\":\"No rules found.\"}".to_owned());
    let tokens_used = estimate_tokens(&text);
    let served_event = serve_and_record(
        &state.db,
        RuleServe {
            tool: "search_rules",
            session_id: Some(session_id),
            event_session_id: session_id,
            repo_full_name: repo_scopes.first().map(String::as_str),
            target_file,
            query: query.as_str(),
            rule_ids: &[],
            top_k: i64::try_from(top_k).unwrap_or(i64::MAX),
            strict_match_count: 0,
            estimated_tokens: i64::try_from(tokens_used).unwrap_or(i64::MAX),
        },
        serve_record_err_prefix("[difflore-mcp] search_rules serve record failed"),
    )
    .await;
    {
        let cloud = state.cloud.clone();
        tokio::spawn(async move {
            if let Err(e) =
                crate::cloud::observations::enqueue_and_flush_default(served_event, &cloud).await
            {
                if crate::infra::env::debug_telemetry() {
                    eprintln!("[difflore-mcp] search_rules served event failed: {e}");
                }
            }
        });
    }
    emit_trajectory_step(&TrajectoryStep::McpResponseSize {
        tool: "search_rules".to_owned(),
        total_tokens: tokens_used,
        rules_injected: 0,
    });
    Ok(json!({
        "content": [{ "type": "text", "text": text }],
        "_meta": {
            "cost": build_cost_meta(tokens_used, None),
            "impact": {
                "rulesInjected": 0,
                "rulesIndexed": rules_indexed,
                "kind": "rules_index",
                "retrievalAttempts": retrieval_attempts,
                "retryKind": retry_kind,
                "repoScopes": repo_scopes,
                "noCurrentRepoRules": no_current_repo_rules,
            },
            "trace": recall_trace_json(*counts, 0, 0),
            "embedding": serde_json::to_value(embedding_diag).unwrap_or(Value::Null),
        }
    }))
}

const fn no_current_repo_rules(repo_scopes: &[String], rules_indexed: usize) -> bool {
    !repo_scopes.is_empty() && rules_indexed == 0
}

const fn empty_recall_message(
    repo_scopes: &[String],
    rules_indexed: usize,
    retry_kind: Option<&'static str>,
) -> &'static str {
    if repo_scopes.is_empty() {
        "No GitHub repo scope detected. Run inside a GitHub repo or pass repo_full_name; DiffLore will not inject global memory without a repo scope."
    } else if no_current_repo_rules(repo_scopes, rules_indexed) {
        "No rules are scoped to THIS repo yet, so DiffLore served nothing as team memory. Run `difflore import-reviews` for this repo, or pass an explicit repo_full_name if this is not the repo you meant."
    } else if retry_kind.is_some() {
        "No rules found after a targeted retry. Pass a concrete file and intent, or add/import rules for this repo."
    } else {
        "No rules found for this file and intent."
    }
}

/// Build the non-empty response: supplement late metadata, run serve
/// arbitration, assemble the evidence entries + provenance, emit telemetry, and
/// return the MCP payload.
async fn build_results_response(
    state: &McpState,
    parsed: &SearchRulesQuery<'_>,
    outcome: RetrievalOutcome,
) -> Result<Value, (i32, String)> {
    let SearchRulesQuery {
        file,
        intent,
        session_id,
        top_k,
        target_file,
        repo_scopes,
    } = parsed;
    let (file, intent, session_id, top_k, target_file) =
        (*file, *intent, *session_id, *top_k, *target_file);
    let RetrievalOutcome {
        mut scored,
        mut meta_map,
        strict_skill_ids,
        rules_indexed,
        embedding_diag,
        query,
        retrieval_attempts,
        retry_kind,
        cross_repo_starter,
        counts,
    } = outcome;

    // Supplement metadata for candidates the early (pre-rerank) batch fetch
    // could not know about — only the cross-repo starter set, retrieved after
    // the gates, can introduce new ids here.
    let missing_meta_ids: Vec<String> = scored
        .iter()
        .filter(|s| !meta_map.contains_key(&s.skill_id))
        .map(|s| s.skill_id.clone())
        .collect();
    if !missing_meta_ids.is_empty() {
        let extra = fetch_skills_by_ids(&state.db, &missing_meta_ids)
            .await
            .map_err(|e| (-32603, format!("Failed to fetch rule metadata: {e}")))?;
        meta_map.extend(extra);
    }
    let strict_skill_ids = if cross_repo_starter {
        strict_file_match_ids_for_meta(&meta_map, target_file)
    } else {
        strict_skill_ids
    };
    for rule in &mut scored {
        if let Some(meta) = meta_map.get(&rule.skill_id) {
            rule.score *= explicit_recall_kind_gate_multiplier(meta, &strict_skill_ids);
        }
    }

    // Deterministic serve arbitration AFTER the double gate (intent alignment
    // + explicit recall threshold): strict hit → 10% score band → source
    // priority → confidence → skill_id. Reuses the metadata fetched above —
    // no additional queries. `DIFFLORE_DISABLE_SOURCE_PRIORITY` rolls the
    // re-sort back; the why facts remain available either way.
    let origin_by_skill_id: HashMap<String, String> = meta_map
        .iter()
        .map(|(id, row)| (id.clone(), row.origin.clone()))
        .collect();
    let why_map = crate::context::retrieval::arbitrate_rule_order(
        &mut scored,
        &strict_skill_ids,
        &origin_by_skill_id,
        crate::infra::env::source_priority_disabled(),
    );

    let skill_ids: Vec<String> = scored.iter().map(|s| s.skill_id.clone()).collect();
    let trust_evidence =
        super::super::trust_proof::fetch_cloud_top_rule_trust_evidence(&state.cloud).await;

    let mut entries: Vec<RuleMatchEvidenceRecord> = Vec::with_capacity(scored.len());
    let mut source_rank_by_id: HashMap<String, u8> = HashMap::new();
    for s in &scored {
        // Missing metadata is a soft skip: a stale chunk (skill row deleted but
        // index not yet pruned) is dropped rather than returned as garbage.
        let Some(meta) = meta_map.get(&s.skill_id) else {
            continue;
        };
        let file_patterns = parse_file_patterns(meta.file_patterns.as_deref());
        let evidence = build_match_evidence(file, s.score, &file_patterns, meta.confidence_score);
        let proof = trust_evidence.get(&s.skill_id);
        source_rank_by_id.insert(
            meta.id.clone(),
            crate::context::retrieval::source_rank(&meta.origin),
        );
        entries.push(RuleMatchEvidenceRecord {
            id: meta.id.clone(),
            title: meta.name.clone(),
            origin: meta.origin.clone(),
            confidence: meta.confidence_score,
            similarity: public_relevance_score(s.score),
            file_patterns,
            preview: rule_preview(&meta.description, 120),
            source_repo: meta.source_repo.clone().filter(|r| !r.trim().is_empty()),
            cited_count: proof.map(|p| p.cited_count),
            trust_rate: proof.and_then(|p| p.trust_rate),
            why: why_map
                .get(&s.skill_id)
                .map(crate::context::retrieval::RuleRankingWhy::compact),
            evidence,
        });
    }
    let metadata_missing_dropped = scored.len().saturating_sub(entries.len());
    let mut entries_json = serde_json::to_value(&entries).map_err(|e| {
        (
            -32603,
            format!("Failed to serialise search_rules entries: {e}"),
        )
    })?;
    if let Some(array) = entries_json.as_array_mut() {
        for item in array {
            let Some(object) = item.as_object_mut() else {
                continue;
            };
            let id = object
                .get("id")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            let origin = object
                .get("origin")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            let source_repo = object.get("sourceRepo").cloned().unwrap_or(Value::Null);
            let source_rank = id
                .as_deref()
                .and_then(|id| source_rank_by_id.get(id).copied())
                .map_or(Value::Null, |rank| json!(rank));
            object.insert("sourceRank".to_owned(), source_rank.clone());
            object.insert(
                "sourceProvenance".to_owned(),
                json!({
                    "origin": origin,
                    "sourceRepo": source_repo,
                    "sourceRank": source_rank,
                }),
            );
        }
    }

    let body = if cross_repo_starter {
        let message = "No rules are scoped to THIS repo yet. These are transferable rules from your other repos, matched to this file. Treat them as suggestions, not this repo's own judgment. Run `difflore import-reviews` to capture this repo's rules.";
        json!({
            "results": entries_json,
            "crossRepoStarter": true,
            "message": message,
        })
    } else {
        json!({ "results": entries_json })
    };
    let text = serde_json::to_string(&body).map_err(|e| {
        (
            -32603,
            format!("Failed to serialise search_rules response: {e}"),
        )
    })?;

    let tokens_used = estimate_tokens(&text);
    let serve_rule_ids: Vec<String> = entries.iter().take(10).map(|e| e.id.clone()).collect();
    let strict_match_count = target_file.map_or(0, |target_file| {
        entries
            .iter()
            .take(10)
            .filter(|entry| has_strict_file_patterns_match(&entry.file_patterns, target_file))
            .count() as i64
    });
    let served_event = serve_and_record(
        &state.db,
        RuleServe {
            tool: "search_rules",
            session_id: Some(session_id),
            event_session_id: session_id,
            repo_full_name: repo_scopes.first().map(String::as_str),
            target_file,
            query: &query,
            rule_ids: &serve_rule_ids,
            top_k: i64::try_from(top_k).unwrap_or(i64::MAX),
            strict_match_count,
            estimated_tokens: i64::try_from(tokens_used).unwrap_or(i64::MAX),
        },
        serve_record_err_prefix("[difflore-mcp] search_rules serve record failed"),
    )
    .await;
    {
        let cloud = state.cloud.clone();
        tokio::spawn(async move {
            if let Err(e) =
                crate::cloud::observations::enqueue_and_flush_default(served_event, &cloud).await
            {
                if crate::infra::env::debug_telemetry() {
                    eprintln!("[difflore-mcp] search_rules served event failed: {e}");
                }
            }
        });
    }
    crate::observability::injection_log::record("mcp_tool", entries.len(), target_file);

    // Memory-pipeline stream for the lightweight index hit.
    for e in &entries {
        crate::observability::activity_stream::record(
            crate::observability::activity_stream::ActivityPayload::RuleRecalled {
                rule_id: e.id.clone(),
                rule_title: e.title.clone(),
                score: e.similarity as f32,
                took_ms: 0,
            },
        );
    }
    crate::observability::activity_stream::record(
        crate::observability::activity_stream::ActivityPayload::RuleInjected {
            rule_count: u32::try_from(entries.len()).unwrap_or(u32::MAX),
            prompt_chars: u32::try_from(text.chars().count()).unwrap_or(u32::MAX),
            intent_summary: format!("{file} | {intent}"),
        },
    );
    // Estimate savings against fetching each full rule body.
    let tokens_if_full = Some(AVG_FULL_RULE_TOKENS * entries.len());

    emit_trajectory_step(&TrajectoryStep::McpResponseSize {
        tool: "search_rules".to_owned(),
        total_tokens: tokens_used,
        rules_injected: entries.len(),
    });
    let origin_step = rule_hits_by_origin(&state.db, &skill_ids).await;
    emit_trajectory_step(&origin_step);

    // Track index-stage recalls separately from full-rule detail recalls.
    {
        let file = file.to_owned();
        let intent = intent.to_owned();
        let rule_titles: Vec<String> = entries.iter().take(10).map(|e| e.title.clone()).collect();
        let rule_ids: Vec<String> = entries.iter().take(10).map(|e| e.id.clone()).collect();
        let rules_injected = entries.len();
        let repo_full_name: Option<String> = repo_scopes.first().cloned();
        let cloud = state.cloud.clone();
        let db = state.db.clone();
        let fired_event = crate::cloud::observations::ObservationEvent::RuleFired {
            rule_ids: rule_ids.clone(),
            file_path: target_file.map(ToOwned::to_owned),
            intent: Some(intent.clone()),
            session_id: session_id.to_owned(),
            fired_at: chrono::Utc::now(),
        };
        enqueue_mcp_query_outbox(
            &state.db,
            super::serve_stats::McpQueryOutboxEntry {
                file: &file,
                intent: &intent,
                rules_injected,
                strict_match_count: usize::try_from(strict_match_count).unwrap_or(usize::MAX),
                rule_titles: &rule_titles,
                rule_ids: &rule_ids,
                client_label: "mcp-server-search",
                repo_full_name: repo_full_name.as_deref(),
            },
        )
        .await;
        tokio::spawn(async move {
            if let Err(e) =
                crate::cloud::observations::enqueue_and_flush_default(fired_event, &cloud).await
            {
                if crate::infra::env::debug_telemetry() {
                    eprintln!("[difflore-mcp] search_rules fired event failed: {e}");
                }
            }
            let _ = drain_mcp_query_outbox(&db, &cloud, 8).await;
        });
    }

    Ok(json!({
        "content": [{ "type": "text", "text": text }],
        "_meta": {
            "cost": build_cost_meta(tokens_used, tokens_if_full),
            "impact": {
                "rulesInjected": entries.len(),
                "rulesIndexed": rules_indexed,
                "kind": "rules_index",
                "crossRepoStarter": cross_repo_starter,
                "noCurrentRepoRules": no_current_repo_rules(repo_scopes, rules_indexed),
                "topRelevance": scored.first().map_or(0.0, |s| public_relevance_score(s.score)),
                "retrievalAttempts": retrieval_attempts,
                "retryKind": retry_kind,
                "repoScopes": repo_scopes,
            },
            "trace": recall_trace_json(counts, metadata_missing_dropped, entries.len()),
            "embedding": serde_json::to_value(&embedding_diag).unwrap_or(Value::Null),
        }
    }))
}

fn recall_trace_json(
    counts: CandidateCounts,
    metadata_missing_dropped: usize,
    returned: usize,
) -> Value {
    let intent_alignment_dropped = counts
        .after_strict_rerank
        .saturating_sub(counts.after_intent_gate);
    let relevance_floor_dropped = counts
        .after_intent_gate
        .saturating_sub(counts.after_relevance_gate);
    json!({
        "candidateLimit": counts.candidate_limit,
        "candidatesRetrieved": counts.retrieved,
        "candidatesAfterRetry": counts.after_retry,
        "candidatesAfterExactMerge": counts.after_exact_merge,
        "candidatesAfterStrictRerank": counts.after_strict_rerank,
        "candidatesAfterIntentGate": counts.after_intent_gate,
        "candidatesAfterRelevanceGate": counts.after_relevance_gate,
        "metadataMissingDropped": metadata_missing_dropped,
        "returned": returned,
        "dropReasons": {
            "intentAlignment": intent_alignment_dropped,
            "relevanceFloor": relevance_floor_dropped,
            "staleMetadata": metadata_missing_dropped,
        },
    })
}

async fn repo_scopes_for_search_rules(
    db: &crate::SqlitePool,
    args: &Value,
) -> Result<Vec<String>, (i32, String)> {
    let base_scopes = if let Some(raw) = args
        .get("repo_full_name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        validate_mcp_text_arg("repo_full_name", raw, MCP_TEXT_ARG_CHAR_LIMIT)?;
        let configured_gitlab_hosts = crate::ingest::gitlab::auth::configured_hosts().await;
        let Some(repo) = crate::infra::git::normalize_repo_scope_with_gitlab_hosts(
            raw,
            &configured_gitlab_hosts,
        ) else {
            return Err((
                -32602,
                "Invalid repo_full_name; expected GitHub owner/repo, GitLab host/namespace/project, or a supported git remote URL"
                    .to_owned(),
            ));
        };
        vec![repo]
    } else {
        crate::mcp_server::hook::refresh_configured_gitlab_hosts_for_remote_detection().await;
        crate::mcp_server::hook::detect_git_remote_owner_repos()
    };

    crate::skills::expand_repo_scopes_with_source_aliases(db, &base_scopes)
        .await
        .map_err(|e| (-32603, format!("Failed to expand repo scopes: {e}")))
}

fn merge_exact_title_strict_matches(
    scored: &mut Vec<ScoredRuleChunk>,
    exact_matches: Vec<ScoredRuleChunk>,
) {
    for exact in exact_matches {
        match scored
            .iter_mut()
            .find(|candidate| candidate.skill_id == exact.skill_id)
        {
            Some(candidate) if candidate.score < exact.score => {
                candidate.score = exact.score;
            }
            Some(_) => {}
            None => scored.push(exact),
        }
    }
}

fn exact_title_strict_match_chunks(
    rules: &[RuleDocument],
    intent: &str,
    target_file: Option<&str>,
    repo_scopes: &[String],
) -> Vec<ScoredRuleChunk> {
    let Some(target_file) = target_file else {
        return Vec::new();
    };
    let intent_key = exact_title_key(intent);
    if intent_key.is_empty() {
        return Vec::new();
    }
    let repo_scopes: HashSet<String> = repo_scopes
        .iter()
        .map(|scope| scope.trim().to_ascii_lowercase())
        .filter(|scope| !scope.is_empty())
        .collect();
    if repo_scopes.is_empty() {
        return Vec::new();
    }

    rules
        .iter()
        .filter(|rule| exact_title_key(&rule.title) == intent_key)
        .filter(|rule| {
            rule.repo_scope
                .as_deref()
                .is_some_and(|scope| repo_scopes.contains(&scope.to_ascii_lowercase()))
        })
        .filter(|rule| {
            has_strict_file_patterns_match(
                &parse_file_patterns(rule.file_patterns.as_deref()),
                target_file,
            )
        })
        .map(|rule| ScoredRuleChunk {
            skill_id: rule.skill_id.clone(),
            content: rule.content.clone(),
            // Exact title + path-hint match outranks fuzzy vector recall,
            // especially during embedding fallback/rate limits.
            score: 2.0 + rule.confidence,
            confidence: rule.confidence,
        })
        .collect()
}

fn exact_title_key(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(id: &str, title: &str, repo_scope: &str, file_patterns: &str) -> RuleDocument {
        RuleDocument {
            skill_id: id.to_owned(),
            title: title.to_owned(),
            content: format!("Rule ID: {id}\nRule Name: {title}\n\nbody"),
            confidence: 0.7,
            file_patterns: Some(file_patterns.to_owned()),
            language: None,
            repo_scope: Some(repo_scope.to_owned()),
        }
    }

    #[test]
    fn exact_title_strict_match_requires_repo_and_file_scope() {
        let rules = vec![
            rule(
                "wanted",
                "Avoid glob symlink filters",
                "vitejs/vite",
                "[\"playground/**/*.js\"]",
            ),
            rule(
                "wrong-repo",
                "Avoid glob symlink filters",
                "other/repo",
                "[\"playground/**/*.js\"]",
            ),
            rule(
                "wrong-file",
                "Avoid glob symlink filters",
                "vitejs/vite",
                "[\"src/**/*.js\"]",
            ),
        ];

        let matches = exact_title_strict_match_chunks(
            &rules,
            "Avoid glob symlink filters",
            Some("playground/glob-import/dir/foo.js"),
            &["vitejs/vite".to_owned()],
        );

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].skill_id, "wanted");
    }

    #[test]
    fn empty_recall_message_warns_when_current_repo_has_no_rules() {
        let scopes = vec!["acme/widgets".to_owned()];
        let message = empty_recall_message(&scopes, 0, Some("deterministic_empty_retry"));

        assert!(message.contains("No rules are scoped to THIS repo yet"));
        assert!(message.contains("difflore import-reviews"));
        assert!(no_current_repo_rules(&scopes, 0));
    }

    #[test]
    fn empty_recall_message_keeps_no_scope_guard_distinct() {
        let scopes = Vec::new();
        let message = empty_recall_message(&scopes, 0, None);

        assert!(message.contains("No GitHub repo scope detected"));
        assert!(!no_current_repo_rules(&scopes, 0));
    }

    #[test]
    fn cross_repo_starter_requires_detected_repo_scope() {
        assert!(!should_try_cross_repo_starter(
            true,
            &[],
            0,
            Some("src/lib.rs")
        ));
        assert!(should_try_cross_repo_starter(
            true,
            &["acme/widgets".to_owned()],
            0,
            Some("src/lib.rs")
        ));
        assert!(!should_try_cross_repo_starter(
            true,
            &["acme/widgets".to_owned()],
            3,
            Some("src/lib.rs")
        ));
        assert!(!should_try_cross_repo_starter(
            true,
            &["acme/widgets".to_owned()],
            0,
            None
        ));
    }

    #[test]
    fn explicit_gate_clears_weak_candidates_after_exact_title_merge() {
        // Weak filler should collapse to the empty-result branch.
        let mut scored = vec![
            ScoredRuleChunk {
                skill_id: "codecov-noise".to_owned(),
                content: "Codecov badge convention".to_owned(),
                score: 0.004,
                confidence: 0.7,
            },
            ScoredRuleChunk {
                skill_id: "more-noise".to_owned(),
                content: "unrelated".to_owned(),
                score: 0.002,
                confidence: 0.7,
            },
        ];
        // No exact-title match to merge in (the wrong-file query case).
        merge_exact_title_strict_matches(&mut scored, Vec::new());
        crate::context::retrieval::apply_explicit_recall_threshold(&mut scored);
        assert!(
            scored.is_empty(),
            "wrong-file weak candidates must collapse to zero on the explicit path"
        );
    }

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
    fn intent_gate_drops_adjacent_subject_then_floor_keeps_on_subject() {
        // Intent alignment runs before the relevance floor.
        let intent = "return false instead of panic on invalid input";
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
        merge_exact_title_strict_matches(&mut scored, Vec::new());
        scored.sort_by(|a, b| b.score.total_cmp(&a.score));
        crate::context::retrieval::apply_intent_alignment_gate(&mut scored, intent);
        crate::context::retrieval::apply_explicit_recall_threshold(&mut scored);
        assert_eq!(
            scored
                .iter()
                .map(|s| s.skill_id.as_str())
                .collect::<Vec<_>>(),
            vec!["return-false-not-panic"],
            "only the intent-aligned directive survives the gate + floor"
        );
    }

    #[test]
    fn intent_gate_exempts_exact_title_strict_match() {
        // Strong exact-title strict-file matches are exempt.
        let intent = "return false instead of panic on invalid input";
        let mut scored = vec![directive_chunk(
            "topical-noise",
            "Panic messages should describe the violated invariant",
            0.11,
        )];
        merge_exact_title_strict_matches(
            &mut scored,
            vec![ScoredRuleChunk {
                skill_id: "exact".to_owned(),
                content: "Rule ID: exact\nRule Name: Completely unrelated heading\n\nbody"
                    .to_owned(),
                score: 2.7,
                confidence: 0.7,
            }],
        );
        scored.sort_by(|a, b| b.score.total_cmp(&a.score));
        crate::context::retrieval::apply_intent_alignment_gate(&mut scored, intent);
        crate::context::retrieval::apply_explicit_recall_threshold(&mut scored);
        assert_eq!(
            scored
                .iter()
                .map(|s| s.skill_id.as_str())
                .collect::<Vec<_>>(),
            vec!["exact"],
            "the strong exact-title match is exempt; the topical-noise tail is dropped"
        );
    }

    #[test]
    fn explicit_gate_preserves_exact_title_strict_match() {
        // Strong exact-title strict-file matches must survive the floor.
        let mut scored = vec![ScoredRuleChunk {
            skill_id: "weak".to_owned(),
            content: "weak".to_owned(),
            score: 0.003,
            confidence: 0.7,
        }];
        merge_exact_title_strict_matches(
            &mut scored,
            vec![ScoredRuleChunk {
                skill_id: "exact".to_owned(),
                content: "exact title strict match".to_owned(),
                score: 2.7,
                confidence: 0.7,
            }],
        );
        scored.sort_by(|a, b| b.score.total_cmp(&a.score));
        crate::context::retrieval::apply_explicit_recall_threshold(&mut scored);
        assert_eq!(
            scored
                .iter()
                .map(|s| s.skill_id.as_str())
                .collect::<Vec<_>>(),
            vec!["exact"],
            "the strong exact-title match survives; the weak tail is dropped"
        );
    }

    #[test]
    fn merge_exact_title_strict_matches_boosts_existing_candidate() {
        let mut scored = vec![ScoredRuleChunk {
            skill_id: "wanted".to_owned(),
            content: "old".to_owned(),
            score: 0.1,
            confidence: 0.2,
        }];

        merge_exact_title_strict_matches(
            &mut scored,
            vec![ScoredRuleChunk {
                skill_id: "wanted".to_owned(),
                content: "new".to_owned(),
                score: 2.7,
                confidence: 0.7,
            }],
        );

        assert_eq!(scored.len(), 1);
        assert!((scored[0].score - 2.7).abs() < f64::EPSILON);
    }
}
