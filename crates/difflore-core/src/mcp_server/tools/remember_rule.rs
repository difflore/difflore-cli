use serde_json::{Value, json};

use crate::domain::models::RememberRuleInput;
use crate::observability::trajectory::TrajectoryStep;
use crate::skills;

use super::super::{McpState, build_cost_meta, emit_trajectory_step, estimate_tokens};
use super::serve_stats::{MCP_EMBEDDING_TIMEOUT, drain_mcp_query_outbox, enqueue_mcp_query_outbox};

const MAX_REMEMBER_TITLE_CHARS: usize = 200;

pub(crate) async fn tool_remember_rule(
    state: &McpState,
    args: &Value,
) -> Result<Value, (i32, String)> {
    let title = args
        .get("title")
        .and_then(|v| v.as_str())
        .ok_or((-32602, "Missing required parameter: title".to_owned()))?
        .trim();
    let body = args
        .get("body")
        .and_then(|v| v.as_str())
        .ok_or((-32602, "Missing required parameter: body".to_owned()))?
        .trim();
    if title.is_empty() {
        return Err((-32602, "title must not be empty".to_owned()));
    }
    if body.is_empty() {
        return Err((-32602, "body must not be empty".to_owned()));
    }
    // Soft cap on title length so audit-list output stays one-line.
    if title.chars().count() > MAX_REMEMBER_TITLE_CHARS {
        return Err((
            -32602,
            format!("title must be {MAX_REMEMBER_TITLE_CHARS} chars or fewer"),
        ));
    }
    if body.chars().count() > skills::REMEMBER_BODY_CHAR_LIMIT {
        return Err((
            -32602,
            format!(
                "body must be {} chars or fewer",
                skills::REMEMBER_BODY_CHAR_LIMIT
            ),
        ));
    }

    let file_patterns = args
        .get("file_patterns")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.trim().to_owned()))
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
        })
        .filter(|v| !v.is_empty());
    if let Some(patterns) = file_patterns.as_ref() {
        if patterns.len() > skills::REMEMBER_FILE_PATTERN_LIMIT {
            return Err((
                -32602,
                format!(
                    "file_patterns accepts at most {} entries",
                    skills::REMEMBER_FILE_PATTERN_LIMIT
                ),
            ));
        }
        if patterns
            .iter()
            .any(|p| p.chars().count() > skills::REMEMBER_FILE_PATTERN_CHAR_LIMIT)
        {
            return Err((
                -32602,
                format!(
                    "file_patterns entries must be {} chars or fewer",
                    skills::REMEMBER_FILE_PATTERN_CHAR_LIMIT
                ),
            ));
        }
    }

    let bad_code = args
        .get("bad_code")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(String::from);
    let good_code = args
        .get("good_code")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(String::from);
    for (label, value) in [
        ("bad_code", bad_code.as_deref()),
        ("good_code", good_code.as_deref()),
    ] {
        if value.is_some_and(|v| v.chars().count() > skills::REMEMBER_EXAMPLE_CHAR_LIMIT) {
            return Err((
                -32602,
                format!(
                    "{label} must be {} chars or fewer",
                    skills::REMEMBER_EXAMPLE_CHAR_LIMIT
                ),
            ));
        }
    }
    let severity = args
        .get("severity")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(String::from);

    let input = RememberRuleInput {
        title: title.to_owned(),
        body: body.to_owned(),
        file_patterns,
        bad_code,
        good_code,
        severity,
        // MCP path is always the conversation channel.
        origin: Some("conversation".to_owned()),
    };

    let detected_repos = crate::mcp_server::hook::detect_git_remote_owner_repos();

    let outcome = skills::remember(&state.db, input)
        .await
        .map_err(|e| (-32603, format!("Failed to remember rule: {e}")))?;
    let skill = &outcome.skill;

    if let Some(repo_full_name) = detected_repos
        .first()
        .map(String::as_str)
        .filter(|r| !r.trim().is_empty())
    {
        // MCP recall is repo-scoped. Without attaching the remembered
        // conversation rule to the current repo, the next search_rules /
        // get_rules call filters out the very rule the user just
        // asked the agent to remember.
        let skill_id = skill.id.as_str();
        if let Err(e) = sqlx::query!(
            "UPDATE skills
             SET source_repo = CASE
                 WHEN source_repo IS NULL OR trim(source_repo) = '' THEN ?1
                 ELSE source_repo
             END
             WHERE id = ?2",
            repo_full_name,
            skill_id,
        )
        .execute(&state.db)
        .await
        {
            if crate::infra::env::debug_telemetry() {
                eprintln!("[difflore-mcp] remember_rule source_repo update failed: {e}");
            }
        }
    }

    // Re-index so the next `search_rules` call can recall the rule. Route
    // through the shared project-scoped refresh so embedding profiles and repo
    // filtering stay consistent with CLI recall and MCP search.
    if let Ok(index_pool) = state.resolve_index_pool().await {
        if let Err(e) = crate::context::orchestrator::ensure_rules_indexed_with_embedding_timeout(
            &state.db,
            &index_pool,
            Some(MCP_EMBEDDING_TIMEOUT),
        )
        .await
        {
            if crate::infra::env::debug_telemetry() {
                eprintln!("[difflore-mcp] remember_rule index refresh failed: {e}");
            }
        }
    }

    // Soft warning when the user is approaching the daily cap. The agent
    // sees this in the tool result and will (per its description) echo
    // it back to the user — important UX so a runaway capture rate
    // doesn't become a silent flood.
    let warn_suffix = if outcome.captures_today >= skills::REMEMBER_WARN_THRESHOLD {
        format!(
            "\n\nWarning: {} conversation captures today (cap: {}). \
             Audit with `difflore status --json`.",
            outcome.captures_today,
            skills::REMEMBER_DAILY_LIMIT,
        )
    } else {
        String::new()
    };

    let confirm = if outcome.deduped {
        // Dedup path — tell the user we strengthened an existing rule
        // rather than silently swallowing the re-capture or creating a
        // confusing duplicate row. We don't show "was X" because the
        // bump is `MIN(1.0, current + 0.05)` — when the current value
        // is already near the cap the displayed delta is wrong.
        format!(
            "Already had a matching rule **{}** (`{}`) - strengthened for future matches. \
             Inspect local memory with `difflore status --json`.",
            skill.name, skill.id,
        )
    } else {
        let pattern_hint = if skill.tags.iter().any(|t| t.contains('*')) {
            " (file-pattern scoped)"
        } else {
            " (repo-wide)"
        };
        format!(
            "Remembered as **{}** (`{}`){}.\n\n\
             The rule is local on this device until your next cloud sync publishes eligible memory with the team. \
             Next time DiffLore reviews a matching file or your agent calls `search_rules` then `get_rules`, this rule will be in scope. \
             Inspect local memory with `difflore status --json`.",
            skill.name, skill.id, pattern_hint,
        )
    };

    // Track this conversation capture in the MCP response-size stream.
    let confirm_tokens = estimate_tokens(&confirm) + estimate_tokens(&warn_suffix);
    emit_trajectory_step(&TrajectoryStep::McpResponseSize {
        tool: "remember_rule".to_owned(),
        total_tokens: confirm_tokens,
        rules_injected: usize::from(!outcome.deduped),
    });
    emit_trajectory_step(&TrajectoryStep::RuleHitByOrigin {
        manual: 0,
        conversation: 1,
        pr_review: 0,
        extracted: 0,
        cloud: 0,
    });

    // Fire-and-forget telemetry so the cloud Dashboard sees the new rule
    // origin in near-real-time. Same outbox-fallback as rule retrieval:
    // logged-out events are persisted locally and drained on next login
    // instead of being silently lost.
    {
        let cloud = state.cloud.clone();
        let db = state.db.clone();
        let rule_id = skill.id.clone();
        let rule_name = skill.name.clone();
        let repo_full_name: Option<String> = detected_repos.first().cloned();
        enqueue_mcp_query_outbox(
            &state.db,
            super::serve_stats::McpQueryOutboxEntry {
                file: "remember_rule",
                intent: &rule_name,
                rules_injected: 1,
                strict_match_count: 0,
                rule_titles: std::slice::from_ref(&rule_name),
                rule_ids: std::slice::from_ref(&rule_id),
                client_label: "mcp-server",
                repo_full_name: repo_full_name.as_deref(),
            },
        )
        .await;
        tokio::spawn(async move {
            let _ = drain_mcp_query_outbox(&db, &cloud, 8).await;
        });
    }

    Ok(json!({
        "content": [{
            "type": "text",
            "text": format!("{confirm}{warn_suffix}"),
        }],
        "_meta": {
            "cost": build_cost_meta(confirm_tokens, None),
            "rule_id": skill.id,
            "origin": skill.origin,
            "published": false,
            "deduped": outcome.deduped,
            "dedup_window_hit": outcome.dedup_window_hit,
            "confidence": outcome.confidence_after,
            "captures_today": outcome.captures_today,
            "daily_limit": skills::REMEMBER_DAILY_LIMIT,
            "impact": {
                "rulesAdded": i32::from(!outcome.deduped),
                "kind": if outcome.deduped { "strengthened" } else { "remember" },
            }
        }
    }))
}
