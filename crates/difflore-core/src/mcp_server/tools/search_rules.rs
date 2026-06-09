use serde_json::{Value, json};
use std::collections::HashSet;

use crate::context::retrieval::ScoredRuleChunk;
use crate::context::rule_source::RuleDocument;
use crate::context::types::RuleMatchEvidenceRecord;
use crate::context::{EmbeddingDiagnostics, gather_embedding_diagnostics_with_activity};
use crate::review_trajectory::TrajectoryStep;

use super::super::{
    AVG_FULL_RULE_TOKENS, McpState, build_cost_meta, emit_trajectory_step, estimate_tokens,
    rule_hits_by_origin,
};
use super::util::{
    MCP_EMBEDDING_TIMEOUT, MCP_TEXT_ARG_CHAR_LIMIT, build_empty_recall_retry_query,
    build_match_evidence, disabled_response, drain_mcp_query_outbox, enqueue_mcp_query_outbox,
    fetch_skills_by_ids, has_strict_file_patterns_match, parse_file_patterns,
    rerank_scored_rule_chunks_for_mcp_by_strict_file_matches, rule_injection_disabled,
    rule_preview, strict_file_match_ids_for_rules, validate_mcp_text_arg,
};

pub(crate) async fn tool_search_rules(
    state: &McpState,
    args: &Value,
) -> Result<Value, (i32, String)> {
    if let Some(reason) = rule_injection_disabled() {
        return Ok(disabled_response(reason));
    }
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
    // Clamp `top_k` to the schema range; invalid over-asks still get a
    // useful bounded response.
    let requested_top_k = args
        .get("top_k")
        .and_then(Value::as_u64)
        .map_or(5, |n| n.clamp(1, 50) as usize);
    // Low-rate sampler: occasionally widen default serves from 5 to 8 so
    // deeper ranks get measured. Explicit caller choices pass through.
    let top_k = super::super::recall_sampler::maybe_bump_top_k(
        requested_top_k,
        crate::env::deep_recall_sample_rate(),
    );
    let repo_scopes = repo_scopes_for_search_rules(&state.db, args).await?;

    let mut query = format!("{file} {intent}");

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
        match crate::context::orchestrator::ensure_rules_indexed_for_repo_scopes_with_embedding_timeout(
            &state.db,
            &index_pool,
            &repo_scopes,
            Some(MCP_EMBEDDING_TIMEOUT),
        )
        .await
        {
            Ok(count) => count,
            Err(e) => {
                eprintln!("[difflore-mcp] search_rules index freshness check failed: {e}");
                0
            }
        }
    };
    // Gathered once post-index so every `_meta` return site (empty +
    // success) reports the same embedding-health snapshot without
    // re-querying the index pool per branch.
    let embedding_diag: EmbeddingDiagnostics =
        gather_embedding_diagnostics_with_activity(&index_pool).await;

    let target_file = if file == "unknown" { None } else { Some(file) };
    let ranking_inputs = crate::context::rule_source::load_rule_ranking_inputs(&state.db).await;
    let candidate_limit = top_k.saturating_mul(5).clamp(top_k, 50);
    let mut scored = super::util::retrieve_rules_with_repo_scopes(
        &index_pool,
        super::util::RetrieveRulesArgs {
            query: &query,
            lexical_query: Some(intent),
            top_k: candidate_limit,
            target_file,
            repo_scopes: &repo_scopes,
            confidence_map: ranking_inputs.confidence_map.as_ref(),
            age_days_map: ranking_inputs.age_days_map.as_ref(),
            ann_enabled: true,
            embedding_timeout: Some(MCP_EMBEDDING_TIMEOUT),
            adaptive_prune: false,
        },
    )
    .await
    .map_err(|e| (-32603, format!("Rule retrieval failed: {e}")))?;
    let mut retrieval_attempts = 1;
    let mut retry_kind: Option<&str> = None;
    if scored.is_empty()
        && let Some(retry_query) = build_empty_recall_retry_query(file, intent)
    {
        retrieval_attempts = 2;
        retry_kind = Some("deterministic_empty_retry");
        let retry_scored = super::util::retrieve_rules_with_repo_scopes(
            &index_pool,
            super::util::RetrieveRulesArgs {
                query: &retry_query,
                lexical_query: None,
                top_k: candidate_limit,
                target_file,
                repo_scopes: &repo_scopes,
                confidence_map: ranking_inputs.confidence_map.as_ref(),
                age_days_map: ranking_inputs.age_days_map.as_ref(),
                ann_enabled: true,
                embedding_timeout: Some(MCP_EMBEDDING_TIMEOUT),
                adaptive_prune: false,
            },
        )
        .await
        .map_err(|e| (-32603, format!("Rule retrieval retry failed: {e}")))?;
        query = retry_query;
        scored = retry_scored;
    }
    merge_exact_title_strict_matches(
        &mut scored,
        exact_title_strict_match_chunks(&rules, intent, target_file, &repo_scopes),
    );
    let strict_skill_ids = strict_file_match_ids_for_rules(&rules, target_file);
    scored = rerank_scored_rule_chunks_for_mcp_by_strict_file_matches(
        scored,
        intent,
        top_k,
        &strict_skill_ids,
    );

    // Drop topically adjacent rules whose directive does not share the
    // query intent. Strong exact/title/lexical hits remain exempt.
    crate::context::retrieval::apply_intent_alignment_gate(&mut scored, intent);

    // Final relevance gate for explicit recall. Wrong-file or low-signal
    // queries should return no rules rather than weak filler; strong
    // matches clear this floor by a wide margin.
    crate::context::retrieval::apply_explicit_recall_threshold(&mut scored);

    // Cold-start fallback: only repos with no scoped memory can receive
    // strict-file cross-repo suggestions.
    let mut cross_repo_starter = false;
    if scored.is_empty()
        && rules_indexed == 0
        && let Some(tf) = target_file
    {
        let cross = super::util::cross_repo_starter_scored(
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

    if scored.is_empty() {
        crate::injection_log::record("mcp_tool", 0, target_file);
        // Track empty results without blocking the MCP response.
        {
            let file = file.to_owned();
            let intent = intent.to_owned();
            let repo_full_name: Option<String> = repo_scopes.first().cloned();
            let cloud = state.cloud.clone();
            let db = state.db.clone();
            enqueue_mcp_query_outbox(
                &state.db,
                super::util::McpQueryOutboxEntry {
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
        let empty_message = if repo_scopes.is_empty() {
            "No GitHub repo scope detected. Run inside a GitHub repo or pass repo_full_name; DiffLore will not inject global memory without a repo scope."
        } else if retry_kind.is_some() {
            "No rules found after a targeted retry. Pass a concrete file and intent, or add/import rules for this repo."
        } else {
            "No rules found for this file and intent."
        };
        let body = json!({
            "results": [],
            "message": empty_message,
            "retrieval": {
                "attempts": retrieval_attempts,
                "retry_kind": retry_kind,
                "repo_scopes": repo_scopes,
            }
        });
        let text = serde_json::to_string(&body)
            .unwrap_or_else(|_| "{\"results\":[],\"message\":\"No rules found.\"}".to_owned());
        let tokens_used = estimate_tokens(&text);
        if let Err(e) = crate::mcp_rule_serves::record(
            &state.db,
            &crate::mcp_rule_serves::McpRuleServeInput {
                tool: "search_rules",
                session_id: Some(session_id),
                repo_full_name: repo_scopes.first().map(String::as_str),
                file_path: target_file,
                query_text: &query,
                rule_ids: &[],
                top_k: i64::try_from(top_k).unwrap_or(i64::MAX),
                strict_match_count: 0,
                estimated_tokens: i64::try_from(tokens_used).unwrap_or(i64::MAX),
            },
        )
        .await
        {
            eprintln!("[difflore-mcp] search_rules serve record failed: {e}");
        }
        {
            let cloud = state.cloud.clone();
            let served_event = crate::cloud::observations::ObservationEvent::McpRuleServed {
                tool: "search_rules".to_owned(),
                session_id: session_id.to_owned(),
                repo_full_name: repo_scopes.first().cloned(),
                file_path: target_file.map(ToOwned::to_owned),
                query_hash: crate::mcp_rule_serves::query_hash(&query),
                rule_ids: Vec::new(),
                top_k: i64::try_from(top_k).unwrap_or(i64::MAX),
                was_empty: true,
                strict_match_count: 0,
                estimated_tokens: i64::try_from(tokens_used).unwrap_or(i64::MAX),
                served_at: chrono::Utc::now(),
            };
            tokio::spawn(async move {
                if let Err(e) =
                    crate::cloud::observations::enqueue_and_flush_default(served_event, &cloud)
                        .await
                {
                    eprintln!("[difflore-mcp] search_rules served event failed: {e}");
                }
            });
        }
        emit_trajectory_step(&TrajectoryStep::McpResponseSize {
            tool: "search_rules".to_owned(),
            total_tokens: tokens_used,
            rules_injected: 0,
        });
        return Ok(json!({
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
                },
                "embedding": serde_json::to_value(&embedding_diag).unwrap_or(Value::Null),
            }
        }));
    }

    // Batch-fetch skill metadata so each index entry gets title/origin/
    // file_patterns without N+1 queries. The pipeline already returned
    // `content` with the generated "Rule Name: …" header — we could parse
    // it but a single SELECT IN (...) is cheaper and always correct.
    let skill_ids: Vec<String> = scored.iter().map(|s| s.skill_id.clone()).collect();
    let meta_map_fut = fetch_skills_by_ids(&state.db, &skill_ids);
    let trust_evidence_fut =
        super::super::trust_proof::fetch_cloud_top_rule_trust_evidence(&state.cloud);
    let (meta_map_result, trust_evidence) = tokio::join!(meta_map_fut, trust_evidence_fut);
    let meta_map =
        meta_map_result.map_err(|e| (-32603, format!("Failed to fetch rule metadata: {e}")))?;

    let mut entries: Vec<RuleMatchEvidenceRecord> = Vec::with_capacity(scored.len());
    for s in &scored {
        // Missing metadata is a soft skip: if a chunk is stale (skill row
        // deleted but index not yet pruned) we'd rather drop it than
        // return garbage. Keeps the result list trustworthy.
        let Some(meta) = meta_map.get(&s.skill_id) else {
            continue;
        };
        let file_patterns = parse_file_patterns(meta.file_patterns.as_deref());
        let evidence = build_match_evidence(file, s.score, &file_patterns, meta.confidence_score);
        let proof = trust_evidence.get(&s.skill_id);
        entries.push(RuleMatchEvidenceRecord {
            id: meta.id.clone(),
            title: meta.name.clone(),
            origin: meta.origin.clone(),
            confidence: meta.confidence_score,
            similarity: s.score,
            file_patterns,
            preview: rule_preview(&meta.description, 120),
            source_repo: meta.source_repo.clone().filter(|r| !r.trim().is_empty()),
            cited_count: proof.map(|p| p.cited_count),
            trust_rate: proof.and_then(|p| p.trust_rate),
            evidence,
        });
    }

    let body = if cross_repo_starter {
        // Refine the message when cross-repo suggestions include pack rules.
        let has_pack = entries.iter().any(|e| e.origin == "pack");
        let message = if has_pack {
            "No memory is scoped to THIS repo yet. These are starter-pack suggestions (and transferable rules from your other repos), matched to this file — treat them as suggestions, not this repo's own judgment. Run `difflore import-reviews` to capture this repo's memory."
        } else {
            "No memory is scoped to THIS repo yet. These are transferable rules from your other repos, matched to this file — treat them as suggestions, not this repo's own judgment. Run `difflore import-reviews` to capture this repo's memory."
        };
        json!({
            "results": entries,
            "crossRepoStarter": true,
            "message": message,
        })
    } else {
        json!({ "results": entries })
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
    if let Err(e) = crate::mcp_rule_serves::record(
        &state.db,
        &crate::mcp_rule_serves::McpRuleServeInput {
            tool: "search_rules",
            session_id: Some(session_id),
            repo_full_name: repo_scopes.first().map(String::as_str),
            file_path: target_file,
            query_text: &query,
            rule_ids: &serve_rule_ids,
            top_k: i64::try_from(top_k).unwrap_or(i64::MAX),
            strict_match_count,
            estimated_tokens: i64::try_from(tokens_used).unwrap_or(i64::MAX),
        },
    )
    .await
    {
        eprintln!("[difflore-mcp] search_rules serve record failed: {e}");
    }
    {
        let cloud = state.cloud.clone();
        let served_event = crate::cloud::observations::ObservationEvent::McpRuleServed {
            tool: "search_rules".to_owned(),
            session_id: session_id.to_owned(),
            repo_full_name: repo_scopes.first().cloned(),
            file_path: target_file.map(ToOwned::to_owned),
            query_hash: crate::mcp_rule_serves::query_hash(&query),
            rule_ids: serve_rule_ids.clone(),
            top_k: i64::try_from(top_k).unwrap_or(i64::MAX),
            was_empty: serve_rule_ids.is_empty(),
            strict_match_count,
            estimated_tokens: i64::try_from(tokens_used).unwrap_or(i64::MAX),
            served_at: chrono::Utc::now(),
        };
        tokio::spawn(async move {
            if let Err(e) =
                crate::cloud::observations::enqueue_and_flush_default(served_event, &cloud).await
            {
                eprintln!("[difflore-mcp] search_rules served event failed: {e}");
            }
        });
    }
    crate::injection_log::record("mcp_tool", entries.len(), target_file);

    // Memory-pipeline stream for the lightweight index hit.
    for e in &entries {
        crate::activity_stream::record(crate::activity_stream::ActivityPayload::RuleRecalled {
            rule_id: e.id.clone(),
            rule_title: e.title.clone(),
            score: e.similarity as f32,
            took_ms: 0,
        });
    }
    crate::activity_stream::record(crate::activity_stream::ActivityPayload::RuleInjected {
        rule_count: u32::try_from(entries.len()).unwrap_or(u32::MAX),
        prompt_chars: u32::try_from(text.chars().count()).unwrap_or(u32::MAX),
        intent_summary: format!("{file} · {intent}"),
    });
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
            super::util::McpQueryOutboxEntry {
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
                eprintln!("[difflore-mcp] search_rules fired event failed: {e}");
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
                "topRelevance": scored.first().map_or(0.0, |s| s.score),
                "retrievalAttempts": retrieval_attempts,
                "retryKind": retry_kind,
                "repoScopes": repo_scopes,
            },
            "embedding": serde_json::to_value(&embedding_diag).unwrap_or(Value::Null),
        }
    }))
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
        let Some(repo) = crate::git::normalize_github_repo_full_name(raw) else {
            return Err((
                -32602,
                "Invalid repo_full_name; expected GitHub owner/repo or GitHub remote URL"
                    .to_owned(),
            ));
        };
        vec![repo]
    } else {
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
            // Exact title + strict file scope is stronger than fuzzy vector
            // recall, especially during embedding fallback/rate limits.
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
