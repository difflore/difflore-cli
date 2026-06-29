use serde_json::{Value, json};

use crate::observability::trajectory::TrajectoryStep;

use super::super::serve_render::{RuleServe, serve_and_record, serve_record_err_prefix};
use super::super::{
    McpState, build_cost_meta, emit_trajectory_step, estimate_tokens, rule_hits_by_origin,
};
use super::evidence::{
    explicit_recall_application_guidance, explicit_recall_application_kind, fetch_skills_by_ids,
    parse_file_patterns, render_full_rule_with_examples, strict_file_match_count_for_ids,
};

const MAX_GET_RULE_IDS: usize = 20;
const MAX_GET_RULE_ID_CHARS: usize = 128;

pub(crate) async fn tool_get_rules(state: &McpState, args: &Value) -> Result<Value, (i32, String)> {
    let session_id = args
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("mcp-server");
    let file = args
        .get("file")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty() && *v != "unknown");
    let raw_ids = args
        .get("ids")
        .and_then(|v| v.as_array())
        .ok_or((-32602, "Missing required parameter: ids".to_owned()))?;
    if raw_ids.len() > MAX_GET_RULE_IDS {
        return Err((
            -32602,
            format!("ids accepts at most {MAX_GET_RULE_IDS} entries per call"),
        ));
    }
    let mut ids = Vec::with_capacity(raw_ids.len());
    for value in raw_ids {
        let Some(raw) = value.as_str() else {
            continue;
        };
        let id = raw.trim();
        if id.is_empty() {
            continue;
        }
        if id.chars().count() > MAX_GET_RULE_ID_CHARS {
            return Err((
                -32602,
                format!("ids entries must be {MAX_GET_RULE_ID_CHARS} chars or fewer"),
            ));
        }
        ids.push(id.to_owned());
    }
    if ids.is_empty() {
        return Err((
            -32602,
            "ids must be a non-empty array of strings".to_owned(),
        ));
    }

    let meta_map = fetch_skills_by_ids(&state.db, &ids)
        .await
        .map_err(|e| (-32603, format!("Failed to fetch rules: {e}")))?;

    // Batch-load examples keyed by the present skill ids only, so we
    // don't round-trip on IDs that won't render.
    let present_ids: Vec<String> = ids
        .iter()
        .filter(|id| meta_map.contains_key(id.as_str()))
        .cloned()
        .collect();
    let examples_map =
        crate::context::rule_source::load_rule_examples_batch(&state.db, &present_ids)
            .await
            .unwrap_or_default();

    let mut results = Vec::with_capacity(ids.len());
    let mut missing = Vec::new();
    for id in &ids {
        match meta_map.get(id.as_str()) {
            Some(row) => {
                let examples = examples_map.get(id.as_str());
                let body = render_full_rule_with_examples(row, examples);
                let application_kind = explicit_recall_application_kind(row);
                let application_guidance = explicit_recall_application_guidance(row);
                let example_entries: Vec<Value> = examples
                    .map(|ex| {
                        ex.iter()
                            .map(|e| {
                                json!({
                                    "bad_code": e.bad_code,
                                    "good_code": e.good_code,
                                    "description": e.description,
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                // Surface source_repo at the top level so an agent
                // need not grep the "Source: " line in `body`.
                results.push(json!({
                    "id": row.id,
                    "title": row.name,
                    "origin": row.origin,
                    "confidence": row.confidence_score,
                    "application_kind": application_kind,
                    "application_guidance": application_guidance,
                    "file_patterns": parse_file_patterns(row.file_patterns.as_deref()),
                    "source_repo": row.source_repo
                        .as_deref()
                        .filter(|r| !r.trim().is_empty()),
                    "body": body,
                    "examples": example_entries,
                }));
            }
            None => missing.push(id.clone()),
        }
    }

    let body = json!({
        "results": results,
        "missing_ids": missing,
    });
    let text = serde_json::to_string(&body).map_err(|e| {
        (
            -32603,
            format!("Failed to serialise get_rules response: {e}"),
        )
    })?;

    let tokens_used = estimate_tokens(&text);
    // Telemetry-only here (repo_full_name attribution), but warm the host cache
    // anyway so the recorded scope is accurate for self-managed GitLab on a
    // cold-cache MCP process. Mirrors the other tool detect sites.
    crate::mcp_server::hook::refresh_configured_gitlab_hosts_for_remote_detection().await;
    let detected_repos = crate::mcp_server::hook::detect_git_remote_owner_repos();
    let detail_query = format!("get_rules:{}", ids.join(","));
    let strict_match_count = strict_file_match_count_for_ids(&meta_map, &present_ids, file);
    let served_event = serve_and_record(
        &state.db,
        RuleServe {
            tool: "get_rules",
            session_id: Some(session_id),
            event_session_id: session_id,
            repo_full_name: detected_repos.first().map(String::as_str),
            target_file: file,
            query: &detail_query,
            rule_ids: &present_ids,
            top_k: i64::try_from(ids.len()).unwrap_or(i64::MAX),
            strict_match_count,
            estimated_tokens: i64::try_from(tokens_used).unwrap_or(i64::MAX),
        },
        serve_record_err_prefix("[difflore-mcp] get_rules serve record failed"),
    )
    .await;
    {
        let cloud = state.cloud.clone();
        tokio::spawn(async move {
            if let Err(e) =
                crate::cloud::observations::enqueue_and_flush_default(served_event, &cloud).await
            {
                if crate::infra::env::debug_telemetry() {
                    eprintln!("[difflore-mcp] get_rules served event failed: {e}");
                }
            }
        });
    }
    emit_trajectory_step(&TrajectoryStep::McpResponseSize {
        tool: "get_rules".to_owned(),
        total_tokens: tokens_used,
        rules_injected: results.len(),
    });
    let origin_step = rule_hits_by_origin(&state.db, &present_ids).await;
    emit_trajectory_step(&origin_step);

    // Detail-layer tool: the response is already the full payload, so
    // there's no narrower response to measure savings against; emit
    // `tokens_used` only.
    Ok(json!({
        "content": [{ "type": "text", "text": text }],
        "_meta": {
            "cost": build_cost_meta(tokens_used, None),
            "impact": {
                "rulesInjected": results.len(),
                "rulesMissing": missing.len(),
                "kind": "rules_detail",
            }
        }
    }))
}
