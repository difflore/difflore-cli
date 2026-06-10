//! Per-tool MCP handlers. Each submodule owns one `tool_*` entry point;
//! shared helpers used by 2+ tools live in `util`.
//!
//! Error contract: malformed JSON-RPC requests and real handler failures
//! return JSON-RPC errors (`Err((code, message))`). Intentional policy
//! suppressions, such as haiku-model auto-disable, return `Ok` with an
//! empty result plus explanatory metadata so agents can continue without
//! treating the server as broken.

use serde_json::{Value, json};

use super::McpState;

pub(super) mod get_rules;
pub(super) mod past_verdicts;
pub(super) mod plan_pr;
pub(super) mod remember_rule;
pub(super) mod rule_timeline;
pub(super) mod search_rules;
pub(super) mod util;

pub(super) use get_rules::tool_get_rules;
pub(super) use past_verdicts::tool_get_past_verdicts;
pub(super) use plan_pr::tool_plan_pr;
pub(crate) use plan_pr::{HistoricalPr, load_pr_corpus, predict_scope_from_corpus};
pub(super) use remember_rule::tool_remember_rule;
pub(super) use rule_timeline::tool_rule_timeline;
pub(super) use search_rules::tool_search_rules;
#[cfg(test)]
pub(crate) use util::{disabled_response, rule_injection_disabled};
// Public surface for `difflore doctor`: lets the CLI report whether
// rule injection is currently auto-suppressed by the haiku detector.
pub use util::{
    detect_active_model, haiku_auto_disable_active, is_haiku_model, origin_to_kind,
    parse_file_patterns,
};

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

    match tool_name {
        "search_rules" => tool_search_rules(state, &arguments).await,
        "get_rules" => tool_get_rules(state, &arguments).await,
        "get_past_verdicts" => tool_get_past_verdicts(state, &arguments).await,
        "remember_rule" => tool_remember_rule(state, &arguments).await,
        "rule_timeline" => tool_rule_timeline(state, &arguments).await,
        "plan_pr" => tool_plan_pr(state, &arguments).await,
        _ => Err((-32602, format!("Unknown tool: {tool_name}"))),
    }
}
