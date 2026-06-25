use serde_json::{Value, json};

use crate::memory_autopilot::{MemoryAutopilotLogFilter, load_autopilot_log, load_memory_digest};
use crate::memory_inbox::{
    MemoryActivityFilter, MemoryListFilter, get_memory_item, load_memory_activity,
    load_memory_items,
};

use super::super::{McpState, build_cost_meta, estimate_tokens};

const DEFAULT_LIST_LIMIT: usize = 50;
const DEFAULT_ACTIVITY_LIMIT: usize = 20;
const DEFAULT_AUTOPILOT_DIGEST_LIMIT: usize = 20;
const DEFAULT_AUTOPILOT_LOG_LIMIT: usize = 20;

pub(crate) async fn tool_list_memory(
    state: &McpState,
    args: &Value,
) -> Result<Value, (i32, String)> {
    let limit = number_arg(args, "limit")
        .map_or(DEFAULT_LIST_LIMIT, |value| value.clamp(1, 1_000) as usize);
    let memory = load_memory_items(
        &state.db,
        MemoryListFilter {
            state: optional_string_arg(args, "state"),
            kind: optional_string_arg(args, "kind"),
            repo_full_name: optional_string_arg(args, "repo_full_name"),
            query: optional_string_arg(args, "query"),
            limit,
        },
    )
    .await
    .map_err(|e| (-32603, format!("Failed to list memory: {e}")))?;
    json_response("list_memory", &memory)
}

pub(crate) async fn tool_get_memory_item(
    state: &McpState,
    args: &Value,
) -> Result<Value, (i32, String)> {
    let item_id = args
        .get("id")
        .and_then(Value::as_str)
        .ok_or((-32602, "Missing required parameter: id".to_owned()))?;
    let detail = get_memory_item(&state.db, item_id)
        .await
        .map_err(|e| (-32603, format!("Failed to load memory item: {e}")))?
        .ok_or((-32602, format!("Memory item not found: {item_id}")))?;
    json_response("get_memory_item", &detail)
}

pub(crate) async fn tool_get_memory_activity(
    state: &McpState,
    args: &Value,
) -> Result<Value, (i32, String)> {
    let days = number_arg(args, "days").unwrap_or(30).clamp(1, 365);
    let limit = number_arg(args, "limit").map_or(DEFAULT_ACTIVITY_LIMIT, |value| {
        value.clamp(1, 1_000) as usize
    });
    let activity = load_memory_activity(
        &state.db,
        MemoryActivityFilter {
            rule_id: optional_string_arg(args, "rule_id"),
            repo_full_name: optional_string_arg(args, "repo_full_name"),
            days,
            limit,
        },
    )
    .await
    .map_err(|e| (-32603, format!("Failed to load memory activity: {e}")))?;
    json_response("get_memory_activity", &activity)
}

pub(crate) async fn tool_get_memory_digest(
    state: &McpState,
    args: &Value,
) -> Result<Value, (i32, String)> {
    let limit = number_arg(args, "limit").map_or(DEFAULT_AUTOPILOT_DIGEST_LIMIT, |value| {
        value.clamp(1, 1_000) as usize
    });
    let digest = load_memory_digest(&state.db, limit)
        .await
        .map_err(|e| (-32603, format!("Failed to load memory digest: {e}")))?;
    json_response("get_memory_digest", &digest)
}

pub(crate) async fn tool_get_memory_autopilot_log(
    state: &McpState,
    args: &Value,
) -> Result<Value, (i32, String)> {
    let limit = number_arg(args, "limit").map_or(DEFAULT_AUTOPILOT_LOG_LIMIT, |value| {
        value.clamp(1, 1_000) as usize
    });
    let log = load_autopilot_log(&state.db, MemoryAutopilotLogFilter { limit })
        .await
        .map_err(|e| (-32603, format!("Failed to load memory autopilot log: {e}")))?;
    json_response(
        "get_memory_autopilot_log",
        &json!({
            "schemaVersion": log.schema_version,
            "limit": limit,
            "events": log.events,
            "note": "Read-only audit log. Background Memory Autopilot runs automatically; ask the user to run the DiffLore CLI for review, disable, approve, reject, sync, archive, delete, or manual catch-up/debug actions.",
        }),
    )
}

fn optional_string_arg(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn number_arg(args: &Value, key: &str) -> Option<i64> {
    args.get(key)
        .and_then(|value| value.as_i64().or_else(|| value.as_u64().map(|v| v as i64)))
}

fn json_response<T: serde::Serialize>(tool: &str, body: &T) -> Result<Value, (i32, String)> {
    let text = serde_json::to_string(body)
        .map_err(|e| (-32603, format!("Failed to serialise {tool} response: {e}")))?;
    let tokens = estimate_tokens(&text);
    Ok(json!({
        "content": [{
            "type": "text",
            "text": text,
        }],
        "_meta": {
            "cost": build_cost_meta(tokens, None),
            "governance": "read_only_for_ai; use CLI commands for review, disable, approve, reject, sync, archive, delete, or manual catch-up/debug memory actions",
        }
    }))
}
