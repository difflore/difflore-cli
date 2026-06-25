use serde_json::{Value, json};
use sqlx::SqlitePool;

use crate::cloud::client::CloudClient;
use crate::context::index_db;
use crate::error::CoreError;
use crate::observability::trajectory::TrajectoryStep;
use crate::skills;

use super::schemas::{SKILL_RESOURCES, resource_templates_list, resources_list, tools_list};

pub(super) const PROTOCOL_VERSION: &str = "2024-11-05";
pub(super) const SERVER_NAME: &str = "difflore";
/// Reported to MCP clients in `serverInfo.version`. Sourced from
/// `CARGO_PKG_VERSION` so it can't drift from the crate version.
pub(super) const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

#[allow(clippy::needless_pass_by_value)] // reason: json! macro consumes the Value into the new object
pub(super) fn jsonrpc_result(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

#[allow(clippy::needless_pass_by_value)] // reason: json! macro consumes id into the new object
pub(crate) fn jsonrpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
}

pub(crate) const fn estimate_tokens(text: &str) -> usize {
    text.len() / 4
}

/// Approximate cost of one full rule body for MCP cost metadata.
pub(crate) const AVG_FULL_RULE_TOKENS: usize = 200;

/// Assemble the `_meta.cost` block for an MCP response.
pub(crate) fn build_cost_meta(tokens_used: usize, tokens_if_full: Option<usize>) -> Value {
    match tokens_if_full {
        Some(full) if full > tokens_used => {
            let saved = full - tokens_used;
            let ratio = saved as f64 / full as f64;
            json!({
                "tokens_used": tokens_used,
                "tokens_if_full": full,
                "tokens_saved_vs_full": saved,
                "savings_ratio": (ratio * 100.0).round() / 100.0,
            })
        }
        _ => json!({ "tokens_used": tokens_used }),
    }
}

/// Fire-and-forget structured log of a trajectory step. Debug telemetry
/// prints JSON when `DIFFLORE_DEBUG_TELEMETRY=1`.
pub(crate) fn emit_trajectory_step(step: &TrajectoryStep) {
    if crate::infra::env::debug_telemetry()
        && let Ok(json) = serde_json::to_string(step)
    {
        eprintln!("[difflore.trajectory] {json}");
    }
}

/// Look up the `origin` column for each `skill_id` and aggregate into
/// `TrajectoryStep::RuleHitByOrigin`. Unknown / missing origins are silently
/// dropped. IDs are passed as a single bound JSON parameter so the query stays
/// injection-safe even if a `skill_id` contains SQL-looking text.
pub(crate) async fn rule_hits_by_origin(db: &SqlitePool, skill_ids: &[String]) -> TrajectoryStep {
    let mut manual = 0u32;
    let mut conversation = 0u32;
    let mut pr_review = 0u32;
    let mut extracted = 0u32;
    let mut cloud = 0u32;

    if !skill_ids.is_empty() {
        let ids_json = serde_json::to_string(skill_ids).unwrap_or_else(|_| "[]".to_owned());
        if let Ok(rows) = sqlx::query!(
            "SELECT origin FROM skills WHERE id IN (SELECT value FROM json_each(?1))",
            ids_json
        )
        .fetch_all(db)
        .await
        {
            for row in rows {
                let origin = row.origin;
                match origin.as_str() {
                    "manual" => manual += 1,
                    "conversation" => conversation += 1,
                    "pr_review" => pr_review += 1,
                    "extracted" => extracted += 1,
                    "cloud" => cloud += 1,
                    _ => {}
                }
            }
        }
    }

    TrajectoryStep::RuleHitByOrigin {
        manual,
        conversation,
        pr_review,
        extracted,
        cloud,
    }
}

pub(crate) struct McpState {
    pub(crate) db: SqlitePool,
    pub(crate) cloud: CloudClient,
    pub(crate) index_pool: Option<SqlitePool>,
    // Per-project index pools are resolved on demand and cached by the
    // index DB helper.
}

impl McpState {
    /// Resolve the per-project index pool for the current working dir.
    pub(crate) async fn resolve_index_pool(&self) -> Result<SqlitePool, CoreError> {
        if let Some(pool) = &self.index_pool {
            return Ok(pool.clone());
        }
        index_db::get_pool_for_cwd().await
    }
}

pub(crate) async fn handle_message(state: &McpState, msg: &Value) -> Option<Value> {
    let method = msg.get("method")?.as_str()?;
    // Notifications (no id) — acknowledge silently by returning None.
    let id = msg.get("id").cloned()?;

    let result = match method {
        "initialize" => handle_initialize(),
        "tools/list" => handle_tools_list(),
        "tools/call" => super::tools::handle_tools_call(state, msg.get("params")).await,
        "resources/list" => handle_resources_list(),
        "resources/templates/list" => handle_resource_templates_list(),
        "resources/read" => handle_resources_read(state, msg.get("params")).await,
        "ping" => Ok(json!({})),
        _ => Err((-32601, format!("Method not found: {method}"))),
    };

    Some(match result {
        Ok(val) => jsonrpc_result(id, val),
        Err((code, message)) => jsonrpc_error(id, i64::from(code), &message),
    })
}

#[allow(clippy::unnecessary_wraps)] // reason: dispatched via Result match in handle_message
pub(super) fn handle_initialize() -> Result<Value, (i32, String)> {
    Ok(json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {
            "tools": {},
            "resources": {}
        },
        "serverInfo": {
            "name": SERVER_NAME,
            "version": SERVER_VERSION
        }
    }))
}

#[allow(clippy::unnecessary_wraps)] // reason: dispatched via Result match in handle_message
pub(super) fn handle_tools_list() -> Result<Value, (i32, String)> {
    Ok(json!({ "tools": tools_list() }))
}

#[allow(clippy::unnecessary_wraps)] // reason: dispatched via Result match in handle_message
pub(super) fn handle_resources_list() -> Result<Value, (i32, String)> {
    Ok(json!({ "resources": resources_list() }))
}

#[allow(clippy::unnecessary_wraps)] // reason: dispatched via Result match in handle_message
pub(super) fn handle_resource_templates_list() -> Result<Value, (i32, String)> {
    Ok(json!({ "resourceTemplates": resource_templates_list() }))
}

pub(super) async fn handle_resources_read(
    state: &McpState,
    params: Option<&Value>,
) -> Result<Value, (i32, String)> {
    let uri = params
        .and_then(|p| p.get("uri"))
        .and_then(|u| u.as_str())
        .unwrap_or("");

    match uri {
        "difflore://rules/active" => {
            // Scoped to the current project's git remotes. This is a
            // deliberate narrowing fix: the resource used to export the whole
            // machine corpus, leaking every project's rules to whichever
            // agent read it (project-scope invariant violation).
            let root = crate::infra::db::current_project_root();
            let configured_gitlab_hosts = crate::ingest::gitlab::auth::configured_hosts().await;
            let repo_scopes = crate::infra::git::detect_repo_full_names_with_gitlab_hosts(
                &root.to_string_lossy(),
                &configured_gitlab_hosts,
            );
            let md = skills::export_rules_markdown(&state.db, &repo_scopes)
                .await
                .unwrap_or_else(|e| format!("Error loading rules: {e}"));
            Ok(json!({
                "contents": [{
                    "uri": uri,
                    "mimeType": "text/markdown",
                    "text": md
                }]
            }))
        }
        "difflore://memory/inbox" => {
            let memory = crate::memory_inbox::load_memory_items(
                &state.db,
                crate::memory_inbox::MemoryListFilter {
                    state: None,
                    kind: None,
                    repo_full_name: None,
                    query: None,
                    limit: 1_000,
                },
            )
            .await
            .map_err(|e| (-32603, format!("Error loading memory inbox: {e}")))?;
            Ok(json!({
                "contents": [{
                    "uri": uri,
                    "mimeType": "application/json",
                    "text": serde_json::to_string(&memory).unwrap_or_else(|_| "{}".into()),
                }]
            }))
        }
        _ => {
            if let Some(skill) = SKILL_RESOURCES.iter().find(|r| r.uri == uri) {
                Ok(json!({
                    "contents": [{
                        "uri": uri,
                        "mimeType": "text/markdown",
                        "text": skill.markdown,
                    }]
                }))
            } else if let Some(id) = parse_verdict_uri(uri) {
                let json_body = build_verdict_resource(state, &id).await;
                Ok(json!({
                    "contents": [{
                        "uri": uri,
                        "mimeType": "application/json",
                        "text": serde_json::to_string(&json_body).unwrap_or_else(|_| "{}".into()),
                    }]
                }))
            } else if let Some(hash) = parse_signature_uri(uri) {
                let json_body = build_signature_resource(state, &hash);
                Ok(json!({
                    "contents": [{
                        "uri": uri,
                        "mimeType": "application/json",
                        "text": serde_json::to_string(&json_body).unwrap_or_else(|_| "{}".into()),
                    }]
                }))
            } else {
                Err((-32602, format!("Unknown resource URI: {uri}")))
            }
        }
    }
}

/// Parse `difflore://verdicts/{id}` → Some("id"). Empty / missing id → None.
pub(crate) fn parse_verdict_uri(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("difflore://verdicts/")?;
    let id = rest.trim_matches('/');
    if id.trim().is_empty() || id.contains('/') {
        return None;
    }
    Some(id.to_owned())
}

/// Parse `difflore://signatures/{hash}` → Some("hash"). Empty → None.
pub(crate) fn parse_signature_uri(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("difflore://signatures/")?;
    let hash = rest.trim_matches('/');
    if hash.trim().is_empty() || hash.contains('/') {
        return None;
    }
    Some(hash.to_owned())
}

/// Build the JSON body for `difflore://verdicts/{id}`. Full verdict JSON isn't
/// cached locally yet, so the resource returns a stable pointer plus an
/// explicit action. Text is product-facing: MCP clients surface it directly.
pub(super) async fn build_verdict_resource(state: &McpState, id: &str) -> Value {
    let cloud_dashboard = state.cloud.base_url().trim_end_matches("/api").to_owned();
    let deep_link = format!("{cloud_dashboard}/verdicts/{id}");
    let logged_in = state.cloud.is_logged_in();
    json!({
        "id": id,
        "kind": "past_verdict",
        "deep_link": deep_link,
        "logged_in": logged_in,
        "status": "not_cached_locally",
        "action": if logged_in { "open_deep_link" } else { "login_then_open_deep_link" },
        "note": if logged_in {
            "Detailed verdict JSON is not cached on this device yet. Open deep_link in the dashboard, or use `get_past_verdicts` for semantic recall."
        } else {
            "Detailed verdict JSON is not cached on this device. Use `get_past_verdicts` for local recall, or sign in with `difflore cloud login` to open deep_link in the dashboard."
        },
    })
}

pub(super) fn build_signature_resource(state: &McpState, hash: &str) -> Value {
    let cloud_dashboard = state.cloud.base_url().trim_end_matches("/api").to_owned();
    let deep_link = format!("{cloud_dashboard}/signatures/{hash}");
    json!({
        "hash": hash,
        "kind": "signature",
        "see": "cloud dashboard",
        "deep_link": deep_link,
        "note": "Signature clustering data is cloud-only; this resource exists so agents can cite signatures by URI (MCP resource mention) without resolving them locally.",
    })
}
