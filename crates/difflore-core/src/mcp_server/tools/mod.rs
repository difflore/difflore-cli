//! Per-tool MCP handlers. Each submodule owns one `tool_*` entry point;
//! shared helpers used by 2+ tools live in `validate` (argument checks +
//! injection gating), `evidence` (serve-proof records + rule rendering),
//! and `serve_stats` (serve telemetry + retrieval pipeline).
//!
//! Error contract: malformed JSON-RPC requests and real handler failures
//! return JSON-RPC errors (`Err((code, message))`). Intentional policy
//! suppressions, such as haiku-model auto-disable, return `Ok` with an
//! empty result plus explanatory metadata so agents can continue without
//! treating the server as broken.

use serde_json::{Value, json};

use super::McpState;

pub(super) mod evidence;
pub(super) mod get_rules;
pub(super) mod memory;
pub(super) mod past_verdicts;
pub(super) mod plan_pr;
pub(super) mod remember_rule;
pub(super) mod rule_timeline;
pub(super) mod search_rules;
pub(super) mod serve_stats;
pub(super) mod validate;

pub(super) use get_rules::tool_get_rules;
pub(super) use memory::{
    tool_get_memory_activity, tool_get_memory_autopilot_log, tool_get_memory_digest,
    tool_get_memory_item, tool_list_memory,
};
pub(super) use past_verdicts::tool_get_past_verdicts;
pub(super) use plan_pr::tool_plan_pr;
pub(crate) use plan_pr::{HistoricalPr, load_pr_corpus, predict_scope_from_corpus};
pub(super) use remember_rule::tool_remember_rule;
pub(super) use rule_timeline::tool_rule_timeline;
pub(super) use search_rules::tool_search_rules;
#[cfg(test)]
pub(crate) use validate::{disabled_response, rule_injection_disabled};
// Public surface for `difflore doctor`: lets the CLI report whether
// rule injection is currently auto-suppressed by the haiku detector.
pub use evidence::{origin_to_kind, parse_file_patterns};
pub use validate::{detect_active_model, haiku_auto_disable_active, is_haiku_model};

pub const CONTROL_PLANE_DENIED_TOOL_NAMES: &[&str] = &[
    "approve_memory",
    "reject_memory",
    "disable_rule",
    "delete_memory",
    "cloud_sync",
    "cloud_publish",
    "cloud_unpublish",
    "agents_install",
    "agents_update",
    "provider_add",
    "provider_set_active",
    "embeddings_setup",
    "fix_apply",
];

pub const ALLOWED_MCP_TOOL_NAMES: &[&str] = &[
    "search_rules",
    "get_rules",
    "get_past_verdicts",
    "list_memory",
    "get_memory_item",
    "get_memory_activity",
    "get_memory_digest",
    "get_memory_autopilot_log",
    "remember_rule",
    "rule_timeline",
    "plan_pr",
];

pub(super) async fn handle_tools_call(
    state: &McpState,
    params: Option<&Value>,
) -> Result<Value, (i32, String)> {
    let params = params.ok_or((-32602, "Missing params".to_owned()))?;
    let tool_name = params
        .get("name")
        .and_then(|n| n.as_str())
        .ok_or((-32602, "Missing tool name".to_owned()))?;
    let arguments = params.get("arguments").cloned().unwrap_or(json!({}));

    if CONTROL_PLANE_DENIED_TOOL_NAMES.contains(&tool_name) {
        return Err((
            -32602,
            format!(
                "{tool_name} is intentionally not exposed through MCP. Use the explicit DiffLore CLI command so a human controls memory, cloud, config, auth, agent install, and file mutations."
            ),
        ));
    }
    if !ALLOWED_MCP_TOOL_NAMES.contains(&tool_name) {
        return Err((
            -32602,
            format!(
                "Unknown or unapproved MCP tool: {tool_name}. DiffLore MCP defaults to an explicit allowlist for read/proposal tools."
            ),
        ));
    }

    match tool_name {
        "search_rules" => tool_search_rules(state, &arguments).await,
        "get_rules" => tool_get_rules(state, &arguments).await,
        "get_past_verdicts" => tool_get_past_verdicts(state, &arguments).await,
        "list_memory" => tool_list_memory(state, &arguments).await,
        "get_memory_item" => tool_get_memory_item(state, &arguments).await,
        "get_memory_activity" => tool_get_memory_activity(state, &arguments).await,
        "get_memory_digest" => tool_get_memory_digest(state, &arguments).await,
        "get_memory_autopilot_log" => tool_get_memory_autopilot_log(state, &arguments).await,
        "remember_rule" => tool_remember_rule(state, &arguments).await,
        "rule_timeline" => tool_rule_timeline(state, &arguments).await,
        "plan_pr" => tool_plan_pr(state, &arguments).await,
        _ => Err((-32602, format!("Unknown tool: {tool_name}"))),
    }
}
