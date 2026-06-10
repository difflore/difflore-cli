//! MCP (Model Context Protocol) server implementation.
//!
//! Speaks JSON-RPC 2.0 over stdin/stdout. AI coding assistants (Claude Code,
//! Cursor, etc.) connect to `difflore mcp-server` as an MCP stdio transport
//! to query team rules and historical review verdicts while generating code.

mod hook;
mod hook_short_circuit;
mod pr_scope;
mod recall_sampler;
mod schemas;
mod serve_render;
mod server;
mod skill_docs;
mod tools;
mod trust_proof;

#[cfg(test)]
pub(crate) use hook::set_detected_repos_for_current_dir_for_test;
pub use hook::{HookRuleContext, fetch_relevant_rules_for_hook, run};
pub use pr_scope::{predict_pr_scope, predict_pr_scope_for_repos};
#[cfg(test)]
pub(crate) use tools::{HistoricalPr, predict_scope_from_corpus};
pub use tools::{
    detect_active_model, haiku_auto_disable_active, is_haiku_model, origin_to_kind,
    parse_file_patterns,
};

// Items re-exported in this scope so submodules can `use super::*;` and
// reach all internal helpers / types without enumerating sibling paths.
pub(crate) use hook::detect_git_remote_owner_repos;
pub(crate) use pr_scope::repo_scoped_plan_corpus;
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
