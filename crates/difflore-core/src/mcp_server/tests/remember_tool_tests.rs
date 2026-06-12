//! In-process MCP roundtrip tests: call `handle_message` directly with a
//! hand-built JSON-RPC envelope to exercise dispatch + arg-parsing without
//! spawning a subprocess.

use super::super::*;
use serde_json::{Value, json};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::{collections::HashSet, str::FromStr};

/// Build a JSON-RPC envelope without a `params` field.
fn rpc(id: i64, method: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method })
}

/// Build a JSON-RPC envelope with params.
fn rpc_with(id: i64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

/// Build a `tools/call` envelope.
fn call_tool(method_id: i64, name: &str, arguments: Value) -> Value {
    rpc_with(
        method_id,
        "tools/call",
        json!({ "name": name, "arguments": arguments }),
    )
}

/// Dispatch a request and return the `result` value, panicking with the
/// full response when the call errored.
async fn call_ok(state: &McpState, req: &Value) -> Value {
    let resp = handle_message(state, req).await.unwrap();
    assert!(
        resp.get("error").is_none(),
        "expected success, got error response: {resp}"
    );
    resp["result"].clone()
}

/// Run a `tools/call` and decode `result.content[0].text` as JSON.
async fn call_tool_json(state: &McpState, id: i64, name: &str, arguments: Value) -> (Value, Value) {
    let req = call_tool(id, name, arguments);
    let result = call_ok(state, &req).await;
    let text = result["content"][0]["text"]
        .as_str()
        .expect("content[0].text present");
    let body: Value = serde_json::from_str(text).expect("content[0].text is JSON");
    (body, result)
}

async fn build_state() -> McpState {
    let _ = crate::infra::db::shared_test_home();
    let opts = SqliteConnectOptions::from_str("sqlite::memory:")
        .unwrap()
        .foreign_keys(true);
    let db = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .unwrap();
    crate::infra::db::run_migrations(&db).await.unwrap();
    let index_path = std::env::temp_dir().join(format!(
        "difflore-mcp-test-index-{}-{}.db",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let index_pool = crate::context::index_db::open_pool_at(&index_path)
        .await
        .unwrap();
    let cloud = crate::cloud::client::CloudClient::create().await;
    McpState {
        db,
        cloud,
        index_pool: Some(index_pool),
    }
}

#[tokio::test]
async fn tools_list_advertises_expected_tools() {
    // Pin every advertised tool name in one snapshot: if a tool is dropped
    // or renamed, the agent loses access.
    let state = build_state().await;
    let result = call_ok(&state, &rpc(1, "tools/list")).await;
    let names: Vec<String> = result["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str().map(String::from))
        .collect();
    for required in [
        "remember_rule",
        "search_rules",
        "get_rules",
        "get_past_verdicts",
        "rule_timeline",
    ] {
        assert!(
            names.contains(&required.to_owned()),
            "{required} missing from tools list: {names:?}"
        );
    }
    assert!(
        !names.contains(&"get_relevant_rules".to_owned()),
        "retired one-shot tool must not be advertised: {names:?}"
    );
    let search_rules = result["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"].as_str() == Some("search_rules"))
        .expect("search_rules tool");
    let description = search_rules["description"]
        .as_str()
        .expect("search_rules description");
    assert!(
        description.contains("citedCount") && description.contains("trustRate"),
        "search_rules should advertise accepted-use summary fields: {description}"
    );
    let get_rules = result["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"].as_str() == Some("get_rules"))
        .expect("get_rules tool");
    let get_rules_description = get_rules["description"]
        .as_str()
        .expect("get_rules description");
    assert!(
        get_rules_description.contains("connect the rule to that file"),
        "get_rules should tell agents to pass the current file: {get_rules_description}"
    );
    assert!(
        get_rules["inputSchema"]["properties"]["file"].is_object(),
        "get_rules schema should expose optional file scope: {get_rules}"
    );
    assert_eq!(
        get_rules["inputSchema"]["properties"]["ids"]["items"]["maxLength"].as_u64(),
        Some(128),
        "get_rules ids should advertise the per-id bound: {get_rules}"
    );
    let get_past_verdicts = result["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"].as_str() == Some("get_past_verdicts"))
        .expect("get_past_verdicts tool");
    assert_eq!(
        get_past_verdicts["inputSchema"]["required"],
        json!(["query"]),
        "get_past_verdicts schema should require semantic query: {get_past_verdicts}"
    );
}

#[tokio::test]
async fn remember_rule_writes_then_search_and_get_rules_recalls() {
    let state = build_state().await;

    let remember = call_ok(
        &state,
        &call_tool(
            2,
            "remember_rule",
            json!({
                "title": "Never call eval() on parsed YAML",
                "body": "Use `safe_load`. eval-on-yaml is RCE waiting to happen.",
                "file_patterns": ["**/*.py"],
                "severity": "high"
            }),
        ),
    )
    .await;
    let rule_id = remember["_meta"]["rule_id"]
        .as_str()
        .expect("rule_id in _meta")
        .to_owned();
    scope_rule_to_test_repo(&state, &rule_id).await;
    assert_eq!(remember["_meta"]["origin"].as_str(), Some("conversation"));
    assert_eq!(
        remember["_meta"]["captured_by_client"].as_str(),
        Some("mcp-server")
    );
    assert_eq!(remember["_meta"]["published"].as_bool(), Some(false));
    assert_eq!(
        remember["_meta"]["deduped"].as_bool(),
        Some(false),
        "fresh rule should not be marked deduped"
    );
    let body_text = remember["content"][0]["text"].as_str().unwrap();
    assert!(body_text.contains(&rule_id), "confirm text echoes rule_id");
    assert!(
        body_text.contains("local-only") || body_text.contains("publish"),
        "confirm text mentions publish workflow"
    );

    // An immediate search_rules -> get_rules flow must surface the
    // freshly-remembered rule: the "I told it to remember, therefore it now
    // applies" guarantee.
    let (search_body, search_result) = call_tool_json(
        &state,
        3,
        "search_rules",
        json!({
            "file": "src/loader.py",
            "intent": "parse a YAML config file",
            "repo_full_name": TEST_REPO
        }),
    )
    .await;
    let results = search_body["results"].as_array().expect("results array");
    assert!(
        results
            .iter()
            .any(|entry| entry["id"].as_str() == Some(rule_id.as_str())),
        "freshly remembered rule should be in search output, got: {results:?}"
    );
    assert_eq!(
        search_result["_meta"]["impact"]["retrievalAttempts"].as_i64(),
        Some(1),
        "normal search_rules calls should expose the attempt count: {search_result}"
    );
    assert!(
        search_result["_meta"]["impact"]["retryKind"].is_null(),
        "normal search_rules calls should not claim a retry: {search_result}"
    );

    let (get_body, _) = call_tool_json(
        &state,
        4,
        "get_rules",
        json!({ "ids": [rule_id], "file": "src/loader.py" }),
    )
    .await;
    let recall_text = serde_json::to_string(&get_body).unwrap();
    assert!(
        recall_text.contains("Never call eval") || recall_text.contains("yaml"),
        "freshly remembered rule should be in get_rules output, got: {recall_text}"
    );
}

#[tokio::test]
async fn retired_one_shot_rule_tool_is_not_callable() {
    let state = build_state().await;
    let resp = handle_message(
        &state,
        &call_tool(
            3,
            "get_relevant_rules",
            json!({ "file": "src/loader.py", "intent": "parse a YAML config file" }),
        ),
    )
    .await;
    let resp = resp.unwrap();
    assert_eq!(
        resp["error"]["message"].as_str(),
        Some("Unknown tool: get_relevant_rules"),
        "retired one-shot tool must not be callable: {resp}"
    );
}

#[tokio::test]
async fn remember_rule_dedup_returns_strengthened_meta() {
    // Same-title re-capture must return deduped=true and confidence bumped
    // to 0.65, signalling that we strengthened rather than duplicated.
    let state = build_state().await;

    let make_req = |id: i64| {
        call_tool(
            id,
            "remember_rule",
            json!({ "title": "MCP dedup test rule", "body": "First wording." }),
        )
    };

    let first = call_ok(&state, &make_req(11)).await;
    assert_eq!(first["_meta"]["deduped"].as_bool(), Some(false));
    let first_id = first["_meta"]["rule_id"].as_str().unwrap().to_owned();

    let second = call_ok(&state, &make_req(12)).await;
    assert_eq!(second["_meta"]["deduped"].as_bool(), Some(true));
    assert_eq!(
        second["_meta"]["rule_id"].as_str(),
        Some(first_id.as_str()),
        "dedup must return same rule_id"
    );
    let confidence = second["_meta"]["confidence"].as_f64().unwrap();
    assert!(
        (confidence - 0.65).abs() < 1e-9,
        "expected 0.65 after one bump, got {confidence}"
    );
    let body_text = second["content"][0]["text"].as_str().unwrap();
    assert!(
        body_text.to_lowercase().contains("strengthen"),
        "confirm text should mention strengthening, got: {body_text}"
    );
}

const TEST_REPO: &str = "acme/widgets";

async fn scope_rule_to_repo(state: &McpState, rule_id: &str, repo_full_name: &str) {
    let repo_scope = crate::context::rule_source::repo_scope_from_source_repo(Some(repo_full_name))
        .expect("test repo scope");
    sqlx::query(
        "UPDATE skills
         SET source_repo = ?1, updated_at = datetime('now')
         WHERE id = ?2",
    )
    .bind(repo_full_name)
    .bind(rule_id)
    .execute(&state.db)
    .await
    .expect("scope test rule to repo");
    if let Some(index_pool) = state.index_pool.as_ref() {
        crate::context::orchestrator::ensure_rules_indexed_for_repo_scopes_with_embedding_timeout(
            &state.db,
            index_pool,
            &[repo_full_name.to_owned()],
            None,
        )
        .await
        .expect("refresh indexed test rule scope");
        sqlx::query("UPDATE rule_chunks SET repo_scope = ?1 WHERE skill_id = ?2")
            .bind(repo_scope)
            .bind(rule_id)
            .execute(index_pool)
            .await
            .expect("scope indexed test rule to repo");
    }
}

async fn scope_rule_to_test_repo(state: &McpState, rule_id: &str) {
    scope_rule_to_repo(state, rule_id, TEST_REPO).await;
}

async fn scope_rule_to_current_git_repo(state: &McpState, rule_id: &str) {
    set_detected_repos_for_current_dir_for_test(vec![TEST_REPO.to_owned()]);
    scope_rule_to_repo(state, rule_id, TEST_REPO).await;
}

/// Insert N rules via `remember_rule` for a deterministic corpus.
async fn seed_rules(state: &McpState, items: &[(&str, &str)]) -> Vec<String> {
    let mut ids = Vec::new();
    for (i, (title, body)) in items.iter().enumerate() {
        let req = call_tool(
            900 + i as i64,
            "remember_rule",
            json!({ "title": title, "body": body }),
        );
        let result = call_ok(state, &req).await;
        let id = result["_meta"]["rule_id"]
            .as_str()
            .expect("rule_id present")
            .to_owned();
        scope_rule_to_test_repo(state, &id).await;
        ids.push(id);
    }
    ids
}

async fn remember_rule_with_patterns(
    state: &McpState,
    title: &str,
    body: &str,
    file_patterns: &[&str],
) -> String {
    let req = call_tool(
        901,
        "remember_rule",
        json!({ "title": title, "body": body, "file_patterns": file_patterns }),
    );
    let result = call_ok(state, &req).await;
    let id = result["_meta"]["rule_id"]
        .as_str()
        .expect("rule_id present")
        .to_owned();
    scope_rule_to_test_repo(state, &id).await;
    id
}

#[tokio::test]
async fn rebuild_index_force_reindexes_and_force_prunes_scope() {
    let state = build_state().await;
    let _ids = seed_rules(
        &state,
        &[
            (
                "Return 413 for oversized bodies",
                "Reject requests over the body size limit with HTTP 413.",
            ),
            (
                "Validate content-type before parse",
                "Check the content-type header before parsing a request body.",
            ),
        ],
    )
    .await;
    let index_pool = state.index_pool.as_ref().expect("index pool");

    // Force-rebuild bypasses the freshness gate: seed_rules already indexed
    // the corpus (so `ensure_*` would short-circuit), yet rebuild still runs
    // and returns the in-scope chunk count.
    let count = crate::context::orchestrator::rebuild_rules_index_for_repo_scopes(
        &state.db,
        index_pool,
        &[TEST_REPO.to_owned()],
        None,
    )
    .await
    .expect("rebuild scoped");
    assert!(
        count >= 2,
        "rebuild should index both scoped rules, got {count}"
    );

    // A second force-rebuild re-runs (not freshness-gated) and is stable,
    // the property that makes it a reliable recovery command.
    let count_again = crate::context::orchestrator::rebuild_rules_index_for_repo_scopes(
        &state.db,
        index_pool,
        &[TEST_REPO.to_owned()],
        None,
    )
    .await
    .expect("rebuild scoped again");
    assert_eq!(count, count_again, "force-rebuild is idempotent");

    // Force-prune: rebuilding for no scope clears the index even when the
    // freshness meta says it is populated — the recovery path that heals a
    // polluted index regardless of freshness.
    let pruned = crate::context::orchestrator::rebuild_rules_index_for_repo_scopes(
        &state.db,
        index_pool,
        &[],
        None,
    )
    .await
    .expect("rebuild empty scope");
    assert_eq!(
        pruned, 0,
        "rebuild with no scope force-prunes to an empty index"
    );
}

#[tokio::test]
async fn search_rules_returns_index_not_full_body() {
    let state = build_state().await;
    let full_body = "Search rules: avoid silent panic via expect() in request handlers; prefer `?` and structured errors so the request returns a 500 with a usable message rather than crashing the worker.";
    let rule_id =
        remember_rule_with_patterns(&state, "Search index rule A", full_body, &["src/**/*.rs"])
            .await;
    seed_rules(
        &state,
        &[
            (
                "Search index rule B",
                "Body B — test marker BODYBETA for uniqueness",
            ),
            (
                "Search index rule C",
                "Body C — a third rule, also distinct",
            ),
        ],
    )
    .await;

    let (body, _) = call_tool_json(
        &state,
        501,
        "search_rules",
        json!({
            "file": "src/lib.rs",
            "intent": "Search index rule A error handling patterns",
            "top_k": 3,
            "repo_full_name": TEST_REPO
        }),
    )
    .await;
    let results = body["results"].as_array().expect("results array");
    assert!(
        !results.is_empty(),
        "expected at least one index entry, body: {body}"
    );
    let target = results
        .iter()
        .find(|entry| entry["id"].as_str() == Some(rule_id.as_str()))
        .unwrap_or_else(|| panic!("expected target rule in top results: {results:?}"));
    let evidence = target["evidence"].as_array().expect("evidence array");
    assert!(
        evidence
            .iter()
            .any(|item| item["kind"].as_str() == Some("filePatternMatch")),
        "expected file-pattern evidence, got: {evidence:?}"
    );
    assert!(
        evidence
            .iter()
            .any(|item| item["kind"].as_str() == Some("retrievalMatch")),
        "expected retrieval-match evidence, got: {evidence:?}"
    );
    for entry in results {
        assert!(
            entry.get("id").and_then(|v| v.as_str()).is_some(),
            "id required"
        );
        assert!(
            entry.get("title").and_then(|v| v.as_str()).is_some(),
            "title required"
        );
        let preview = entry["preview"].as_str().expect("preview required");
        assert!(
            preview.chars().count() <= 120,
            "preview too long: {}",
            preview.chars().count()
        );
        assert!(
            preview != full_body,
            "index preview must not include full body verbatim"
        );
        assert!(entry.get("origin").and_then(|v| v.as_str()).is_some());
        assert!(entry.get("confidence").and_then(Value::as_f64).is_some());
        assert!(entry.get("similarity").is_some());
        assert!(
            entry
                .get("file_patterns")
                .and_then(|v| v.as_array())
                .is_some()
        );
        // The examples block must not appear: its presence would mean the
        // index path accidentally included the full body.
        for (_k, v) in entry.as_object().unwrap() {
            if let Some(s) = v.as_str() {
                assert!(
                    !s.contains("### Examples"),
                    "index entry leaked examples block"
                );
            }
        }
    }
    let serve_summary = crate::observability::mcp_rule_serves::summary(&state.db, 30)
        .await
        .expect("mcp serve summary");
    assert_eq!(serve_summary.calls, 1);
    assert_eq!(serve_summary.empty_calls, 0);
    assert!(
        serve_summary.rules_served >= 1,
        "search_rules should record served rule ids"
    );
    assert!(
        serve_summary.strict_matches >= 1,
        "file-scoped search_rules should record strict file matches"
    );
}

#[tokio::test]
async fn search_rules_does_not_count_universal_rule_as_strict_file_proof() {
    let state = build_state().await;
    let ids = seed_rules(
        &state,
        &[(
            "Universal search proof rule",
            "UNIVERSALPROOFSEARCH preserve focused telemetry evidence when polishing MCP proof.",
        )],
    )
    .await;
    let rule_id = ids[0].clone();

    let (_body, _) = call_tool_json(
        &state,
        502,
        "search_rules",
        json!({
            "file": "src/lib.rs",
            "intent": "UNIVERSALPROOFSEARCH telemetry evidence",
            "top_k": 1,
            "repo_full_name": TEST_REPO
        }),
    )
    .await;

    let rule_summary =
        crate::observability::mcp_rule_serves::summary_for_rule(&state.db, &rule_id, 30)
            .await
            .expect("rule serve summary");
    assert_eq!(rule_summary.calls, 1);
    assert_eq!(rule_summary.strict_match_calls, 0);
    let latest = rule_summary.latest.expect("latest serve evidence");
    assert_eq!(latest.file_path.as_deref(), Some("src/lib.rs"));
    assert!(!latest.strict_scoped);
}

#[tokio::test]
async fn search_rules_exposes_token_economics_in_meta() {
    let state = build_state().await;
    seed_rules(
        &state,
        &[
            (
                "Econ rule A",
                "Body A: avoid N+1 SQL in request handlers; prefer batch fetch.",
            ),
            (
                "Econ rule B",
                "Body B: cap retry attempts and expose backoff state.",
            ),
        ],
    )
    .await;

    let result = call_ok(
        &state,
        &call_tool(
            520,
            "search_rules",
            json!({
                "file": "src/handler.rs",
                "intent": "batching and retries",
                "repo_full_name": TEST_REPO
            }),
        ),
    )
    .await;
    let cost = &result["_meta"]["cost"];
    assert!(cost.is_object(), "cost meta must be present: {result}");
    let tokens_used = cost["tokens_used"].as_u64().expect("tokens_used number");
    assert!(tokens_used > 0, "tokens_used should be non-zero");
    // Index-style tools surface the vs-full comparison so the agent can see
    // progressive disclosure pays off.
    let tokens_if_full = cost["tokens_if_full"].as_u64().expect("tokens_if_full");
    assert!(
        tokens_if_full >= tokens_used,
        "tokens_if_full ({tokens_if_full}) must be >= tokens_used ({tokens_used})"
    );
    let saved = cost["tokens_saved_vs_full"].as_u64().expect("tokens_saved");
    assert_eq!(saved, tokens_if_full - tokens_used);
    let ratio = cost["savings_ratio"].as_f64().expect("savings_ratio");
    assert!(ratio > 0.0 && ratio <= 1.0, "ratio out of range: {ratio}");
    let impact = &result["_meta"]["impact"];
    assert_eq!(impact["retrievalAttempts"].as_i64(), Some(1));
    assert!(
        impact["retryKind"].is_null(),
        "normal search_rules calls should not claim a retry: {impact}"
    );
}

#[tokio::test]
async fn search_rules_empty_result_exposes_retry_attempt_meta() {
    let state = build_state().await;

    let (body, result) = call_tool_json(
        &state,
        522,
        "search_rules",
        json!({
            "file": "packages/router/src/parser.ts",
            "intent": "please search review memory for any relevant rules",
            "repo_full_name": "acme/empty"
        }),
    )
    .await;

    assert!(body["results"].as_array().unwrap().is_empty());
    let impact = &result["_meta"]["impact"];
    assert_eq!(impact["retrievalAttempts"].as_i64(), Some(2));
    assert_eq!(
        impact["retryKind"].as_str(),
        Some("deterministic_empty_retry")
    );
}

#[test]
fn rule_match_record_serializes_cloud_trust_proof() {
    let entry = crate::context::types::RuleMatchEvidenceRecord {
        id: "rule-413".to_owned(),
        title: "Return 413 for body size limit errors".to_owned(),
        origin: "cloud".to_owned(),
        confidence: 0.9,
        similarity: 0.8,
        file_patterns: vec!["binding/*.go".to_owned()],
        preview: "Return status 413 when max body bytes are exceeded.".to_owned(),
        source_repo: Some("gin-gonic/gin".to_owned()),
        cited_count: Some(2),
        trust_rate: Some(1.0),
        why: Some("strict-hit; band 9/10; source cloud".to_owned()),
        evidence: Vec::new(),
    };

    let value = serde_json::to_value(entry).expect("serializes");
    assert_eq!(value["citedCount"].as_i64(), Some(2));
    assert_eq!(value["trustRate"].as_f64(), Some(1.0));
    assert_eq!(
        value["why"].as_str(),
        Some("strict-hit; band 9/10; source cloud"),
        "whyRanked must serialize as the compact string"
    );
}

#[test]
fn rule_match_record_omits_missing_cloud_trust_proof() {
    let entry = crate::context::types::RuleMatchEvidenceRecord {
        id: "rule-local".to_owned(),
        title: "Local-only rule".to_owned(),
        origin: "conversation".to_owned(),
        confidence: 0.6,
        similarity: 0.7,
        file_patterns: vec!["**/*.rs".to_owned()],
        preview: "A local rule without cloud proof.".to_owned(),
        source_repo: None,
        cited_count: None,
        trust_rate: None,
        why: None,
        evidence: Vec::new(),
    };

    let value = serde_json::to_value(entry).expect("serializes");
    assert!(
        value.get("citedCount").is_none() && value.get("trustRate").is_none(),
        "proof fields should be omitted when cloud evidence is unavailable: {value}"
    );
    assert!(
        value.get("why").is_none(),
        "why must be omitted when arbitration metadata is unavailable: {value}"
    );
}

#[tokio::test]
async fn get_rules_surfaces_tokens_used_without_full_comparison() {
    let state = build_state().await;
    let ids = seed_rules(
        &state,
        &[(
            "Detail rule A",
            "Body A: unique token DETAILONE for detection.",
        )],
    )
    .await;

    let result = call_ok(
        &state,
        &call_tool(521, "get_rules", json!({ "ids": ids.clone() })),
    )
    .await;
    let cost = &result["_meta"]["cost"];
    assert!(cost["tokens_used"].as_u64().unwrap() > 0);
    // Detail-layer tools have no savings baseline: the response IS the full
    // payload.
    assert!(
        cost.get("tokens_if_full").is_none(),
        "detail tool should not claim savings: {cost}"
    );

    let serve_summary = crate::observability::mcp_rule_serves::summary(&state.db, 30)
        .await
        .expect("mcp serve summary");
    assert_eq!(serve_summary.calls, 1);
    assert_eq!(serve_summary.empty_calls, 0);
    assert_eq!(serve_summary.rules_served, 1);

    let rule_summary =
        crate::observability::mcp_rule_serves::summary_for_rule(&state.db, &ids[0], 30)
            .await
            .expect("rule serve summary");
    assert_eq!(rule_summary.calls, 1);
    assert_eq!(
        rule_summary.latest.as_ref().map(|e| e.tool.as_str()),
        Some("get_rules")
    );
}

#[tokio::test]
async fn get_rules_batches_multiple_ids() {
    let state = build_state().await;
    let ids = seed_rules(
        &state,
        &[
            ("Batch rule one", "Body one with unique token AAAAONE"),
            ("Batch rule two", "Body two with unique token BBBBTWO"),
        ],
    )
    .await;

    let (body, _) = call_tool_json(&state, 510, "get_rules", json!({ "ids": ids.clone() })).await;
    let results = body["results"].as_array().expect("results array");
    assert_eq!(results.len(), 2, "expected 2 results in order");
    // Order must match input.
    assert_eq!(results[0]["id"].as_str(), Some(ids[0].as_str()));
    assert_eq!(results[1]["id"].as_str(), Some(ids[1].as_str()));
    assert!(results[0]["body"].as_str().unwrap().contains("AAAAONE"));
    assert!(results[1]["body"].as_str().unwrap().contains("BBBBTWO"));
    assert!(body["missing_ids"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn get_rules_records_optional_file_scope_for_local_proof() {
    let state = build_state().await;
    let rule_id = remember_rule_with_patterns(
        &state,
        "Scoped detail rule",
        "Body with unique token DETAILSCOPED",
        &["src/**/*.rs"],
    )
    .await;

    let (_body, _) = call_tool_json(
        &state,
        511,
        "get_rules",
        json!({
            "ids": [rule_id.clone()],
            "file": "src/lib.rs",
            "session_id": "agent-session"
        }),
    )
    .await;

    let rule_summary =
        crate::observability::mcp_rule_serves::summary_for_rule(&state.db, &rule_id, 30)
            .await
            .expect("rule serve summary");
    assert_eq!(rule_summary.calls, 1);
    assert_eq!(rule_summary.strict_match_calls, 1);
    let latest = rule_summary.latest.expect("latest serve evidence");
    assert_eq!(latest.tool, "get_rules");
    assert_eq!(latest.file_path.as_deref(), Some("src/lib.rs"));
    assert!(latest.strict_scoped);
}

#[tokio::test]
async fn get_rules_does_not_count_universal_rule_as_strict_file_proof() {
    let state = build_state().await;
    let ids = seed_rules(
        &state,
        &[(
            "Universal detail rule",
            "Body with unique token DETAILUNIVERSAL",
        )],
    )
    .await;
    let rule_id = ids[0].clone();

    let (_body, _) = call_tool_json(
        &state,
        512,
        "get_rules",
        json!({
            "ids": [rule_id.clone()],
            "file": "src/lib.rs",
            "session_id": "agent-session"
        }),
    )
    .await;

    let rule_summary =
        crate::observability::mcp_rule_serves::summary_for_rule(&state.db, &rule_id, 30)
            .await
            .expect("rule serve summary");
    assert_eq!(rule_summary.calls, 1);
    assert_eq!(rule_summary.strict_match_calls, 0);
    let latest = rule_summary.latest.expect("latest serve evidence");
    assert_eq!(latest.tool, "get_rules");
    assert_eq!(latest.file_path.as_deref(), Some("src/lib.rs"));
    assert!(!latest.strict_scoped);
}

#[tokio::test]
async fn hook_injection_records_local_serve_proof() {
    let state = build_state().await;
    let rule_id = remember_rule_with_patterns(
        &state,
        "Hook proof rule",
        "When editing Gin HandleContext NoRoute tests, preserve group middleware behavior and assert engine middleware separately.",
        &["**/*.go"],
    )
    .await;
    scope_rule_to_current_git_repo(&state, &rule_id).await;

    let ctx = fetch_relevant_rules_for_hook(
        &state.db,
        state.index_pool.as_ref().expect("index pool"),
        "gin_test.go",
        "post-edit\nHandleContext NoRoute group middleware regression test",
        Some("hook-session"),
    )
    .await
    .expect("hook recall");

    assert!(
        ctx.rule_ids.contains(&rule_id),
        "hook should inject the matching rule, got {:?}",
        ctx.rule_ids
    );
    let rule_summary =
        crate::observability::mcp_rule_serves::summary_for_rule(&state.db, &rule_id, 30)
            .await
            .expect("hook serve summary");
    assert_eq!(rule_summary.calls, 1);
    assert_eq!(rule_summary.strict_match_calls, 1);
    let latest = rule_summary.latest.expect("latest hook serve evidence");
    assert_eq!(latest.tool, "hook_post_edit");
    assert_eq!(latest.file_path.as_deref(), Some("gin_test.go"));
    assert!(latest.estimated_tokens > 0);
}

#[tokio::test]
async fn hook_injection_renders_why_segment_on_header() {
    // whyRanked end to end on the hook path: the injected block's header
    // carries the compact why segment (strict **/*.go hit, top survivor →
    // band 10/10, remember_rule origin → conversation).
    let state = build_state().await;
    let rule_id = remember_rule_with_patterns(
        &state,
        "Return false rather than panic on invalid input",
        "When binding user input fails validation, return false so callers can surface a 4xx.",
        &["**/*.go"],
    )
    .await;
    scope_rule_to_current_git_repo(&state, &rule_id).await;

    let ctx = fetch_relevant_rules_for_hook(
        &state.db,
        state.index_pool.as_ref().expect("index pool"),
        "binding/form.go",
        "post-edit\nreturn false instead of panic on invalid input",
        Some("hook-session"),
    )
    .await
    .expect("hook recall");

    assert!(
        ctx.rule_ids.contains(&rule_id),
        "hook should inject the matching rule, got {:?}",
        ctx.rule_ids
    );
    assert!(
        ctx.rendered
            .contains("| why: strict-hit; band 10/10; source conversation"),
        "hook header must carry the compact why segment: {}",
        ctx.rendered.lines().next().unwrap_or_default()
    );
}

#[tokio::test]
async fn hook_post_edit_gate_keeps_on_subject_and_drops_adjacent_rule() {
    // C6 misapply unification, end to end on the hook path with the shipped
    // default (intent gate ON):
    //   * the on-subject rule survives the intent-alignment gate — no
    //     injection-empty-rate regression for aligned post-edit serves;
    //   * the topically-adjacent wrong-subject rule is dropped.
    let state = build_state().await;
    let on_subject = remember_rule_with_patterns(
        &state,
        "Return false rather than panic on invalid input",
        "When binding user input fails validation, return false so callers can surface a 4xx.",
        &["**/*.go"],
    )
    .await;
    scope_rule_to_current_git_repo(&state, &on_subject).await;
    let adjacent = remember_rule_with_patterns(
        &state,
        "Panic messages should describe the violated invariant",
        "Write panic messages that name the violated invariant and the offending value.",
        &["**/*.go"],
    )
    .await;
    scope_rule_to_current_git_repo(&state, &adjacent).await;

    let ctx = fetch_relevant_rules_for_hook(
        &state.db,
        state.index_pool.as_ref().expect("index pool"),
        "binding/form.go",
        "post-edit\nreturn false instead of panic on invalid input",
        Some("hook-session"),
    )
    .await
    .expect("hook recall");

    assert!(
        ctx.rule_ids.contains(&on_subject),
        "the intent-aligned rule must survive the post-edit gate, got {:?}",
        ctx.rule_ids
    );
    assert!(
        !ctx.rule_ids.contains(&adjacent),
        "the wrong-subject rule must be dropped by the post-edit gate, got {:?}",
        ctx.rule_ids
    );
    assert!(
        ctx.rules_injected >= 1,
        "gate must not empty an aligned post-edit injection"
    );
}

#[tokio::test]
async fn hook_intent_gate_applies_to_post_edit_but_not_bash_error_path() {
    // The C6 gate is post-edit only: bash-error recall keeps its own
    // copy/recall semantics (cli-spec misapply guard is described per face).
    let state = build_state().await;
    let adjacent = remember_rule_with_patterns(
        &state,
        "Panic messages should describe the violated invariant",
        "Write panic messages that name the violated invariant and the offending value.",
        &["**/*.go"],
    )
    .await;
    scope_rule_to_current_git_repo(&state, &adjacent).await;
    let index_pool = state.index_pool.as_ref().expect("index pool");

    let post_edit = fetch_relevant_rules_for_hook(
        &state.db,
        index_pool,
        "binding/form.go",
        "post-edit\nreturn false instead of panic on invalid input",
        Some("hook-session"),
    )
    .await
    .expect("post-edit recall");
    assert!(
        !post_edit.rule_ids.contains(&adjacent),
        "post-edit gate must drop the wrong-subject rule, got {:?}",
        post_edit.rule_ids
    );

    let bash_error = fetch_relevant_rules_for_bash_error(
        &state.db,
        index_pool,
        "binding/form.go",
        "bash-error command=go test error=return false instead of panic on invalid input",
        Some("hook-session"),
    )
    .await
    .expect("bash-error recall");
    assert!(
        bash_error.rule_ids.contains(&adjacent),
        "bash-error recall is exempt from the post-edit intent gate, got {:?}",
        bash_error.rule_ids
    );
}

#[tokio::test]
async fn search_rules_results_carry_compact_why_ranking() {
    let state = build_state().await;
    let rule_id = remember_rule_with_patterns(
        &state,
        "Avoid unwrap in request handlers",
        "Never unwrap request payloads in handlers; return structured errors instead.",
        &["src/**/*.rs"],
    )
    .await;

    let (body, _) = call_tool_json(
        &state,
        530,
        "search_rules",
        json!({
            "file": "src/http/handler.rs",
            "intent": "avoid unwrap in request handlers",
            "repo_full_name": TEST_REPO,
        }),
    )
    .await;

    let results = body["results"].as_array().expect("results array");
    let entry = results
        .iter()
        .find(|e| e["id"].as_str() == Some(rule_id.as_str()))
        .unwrap_or_else(|| panic!("seeded rule must be recalled: {body}"));
    let why = entry["why"].as_str().expect("why present on results");
    assert!(
        why.contains("strict-hit"),
        "strict src/**/*.rs hit must surface in why: {why}"
    );
    assert!(
        why.contains("band "),
        "band fact must surface in why: {why}"
    );
    assert!(
        why.contains("source conversation"),
        "source priority fact must surface in why: {why}"
    );
}

#[tokio::test]
async fn hook_rebuild_failure_returns_error_instead_of_serving_stale_index() {
    let state = build_state().await;
    let rule_id = remember_rule_with_patterns(
        &state,
        "Hook stale index guard rule",
        "When editing guardrail tests, prefer the fresh rule body over stale index rows.",
        &["**/*.rs"],
    )
    .await;
    scope_rule_to_current_git_repo(&state, &rule_id).await;
    let index_pool = state.index_pool.as_ref().expect("index pool");

    sqlx::query("UPDATE rule_chunks SET content = ?1 WHERE skill_id = ?2")
        .bind("Rule ID: stale\nRule Name: stale\nType: review_standard\n\nSTALE_INDEX_BODY")
        .bind(&rule_id)
        .execute(index_pool)
        .await
        .expect("seed stale chunk content");
    sqlx::query("DROP TABLE rule_chunks_fts")
        .execute(index_pool)
        .await
        .expect("drop FTS table so rebuild trigger fails");

    let err = fetch_relevant_rules_for_hook(
        &state.db,
        index_pool,
        "src/lib.rs",
        "post-edit guardrail tests",
        Some("hook-session"),
    )
    .await
    .expect_err("hook should not continue with stale index after rebuild failure");

    assert!(
        err.to_string().contains("hook rule index rebuild failed"),
        "error should make the index failure visible, got: {err}"
    );
}

#[tokio::test]
async fn get_rules_partial_missing() {
    let state = build_state().await;
    let ids = seed_rules(
        &state,
        &[("Partial rule", "Some unique descriptive text PARTIAL0")],
    )
    .await;
    let existing = ids[0].clone();

    let (body, _) = call_tool_json(
        &state,
        520,
        "get_rules",
        json!({ "ids": [existing.clone(), "nonexistent-skill-id"] }),
    )
    .await;
    assert_eq!(body["results"].as_array().unwrap().len(), 1);
    assert_eq!(body["results"][0]["id"].as_str(), Some(existing.as_str()));
    let missing = body["missing_ids"].as_array().unwrap();
    assert_eq!(missing.len(), 1);
    assert_eq!(missing[0].as_str(), Some("nonexistent-skill-id"));
}

#[tokio::test]
async fn get_rules_rejects_overlong_ids() {
    let state = build_state().await;
    let overlong = "x".repeat(129);
    let resp = handle_message(
        &state,
        &call_tool(523, "get_rules", json!({ "ids": [overlong] })),
    )
    .await
    .unwrap();
    assert_eq!(resp["error"]["code"].as_i64(), Some(-32602));
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("128"),
        "error should name the id length bound: {resp}"
    );
}

#[tokio::test]
async fn search_and_get_round_trip() {
    let state = build_state().await;
    seed_rules(
        &state,
        &[
            (
                "Round-trip rule alpha",
                "ALPHAMARKER body: prefer tokio::select over manual spawn",
            ),
            (
                "Round-trip rule beta",
                "BETAMARKER body: guard against panics in hot path",
            ),
            (
                "Round-trip rule gamma",
                "GAMMAMARKER body: avoid unwrap() in MCP handlers",
            ),
        ],
    )
    .await;

    // The intent must (a) carry the unique `ALPHAMARKER` token to pin this
    // query to THIS test's seeded rules (parallel tests share the DB and
    // would otherwise compete for top-K, flaking the query), and (b) restate
    // rule alpha's directive — the intent-alignment gate drops candidates
    // whose directive doesn't share the query's imperative + object.
    let (search_body, _) = call_tool_json(
        &state,
        530,
        "search_rules",
        json!({
            "file": "src/main.rs",
            "intent": "ALPHAMARKER prefer tokio::select over manual spawn",
            "repo_full_name": TEST_REPO
        }),
    )
    .await;
    let results = search_body["results"].as_array().unwrap();
    assert!(
        !results.is_empty(),
        "search should return at least one result"
    );
    let top_ids: Vec<String> = results
        .iter()
        .take(2)
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect();

    let (get_body, _) =
        call_tool_json(&state, 531, "get_rules", json!({ "ids": top_ids.clone() })).await;
    let fetched = get_body["results"].as_array().unwrap();
    assert_eq!(fetched.len(), top_ids.len());
    for (i, entry) in fetched.iter().enumerate() {
        assert_eq!(entry["id"].as_str(), Some(top_ids[i].as_str()));
        let full_body = entry["body"].as_str().unwrap();
        // The full body leads with the code-spec header `## Rule {id} —
        // {name}`, giving the search_rules -> get_rules flow a stable,
        // id-carrying detail contract.
        assert!(full_body.contains(&format!("## Rule {}", top_ids[i])));
        assert!(full_body.contains(top_ids[i].as_str()));
        assert!(full_body.contains("### Contract"));
    }
}

#[tokio::test]
async fn remember_rule_rejects_missing_args() {
    let state = build_state().await;
    let req = call_tool(4, "remember_rule", json!({ "title": "only-title" }));
    let resp = handle_message(&state, &req).await.unwrap();
    assert!(
        resp.get("error").is_some(),
        "expected JSON-RPC error for missing body, got: {resp}"
    );
}

#[tokio::test]
async fn remember_rule_rejects_oversized_body() {
    let state = build_state().await;
    let req = call_tool(
        41,
        "remember_rule",
        json!({
            "title": "too large",
            "body": "x".repeat(crate::skills::REMEMBER_BODY_CHAR_LIMIT + 1),
        }),
    );
    let resp = handle_message(&state, &req).await.unwrap();

    let error = resp
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
        .unwrap_or_default();
    assert!(
        error.contains("body"),
        "expected body cap error, got: {resp}"
    );
}

#[tokio::test]
async fn rule_timeline_orders_events_and_truncates_preview() {
    let state = build_state().await;

    let long_body = "A".repeat(500); // forces preview truncation
    let remember = call_ok(
        &state,
        &call_tool(
            601,
            "remember_rule",
            json!({ "title": "Timeline seed rule", "body": long_body }),
        ),
    )
    .await;
    let rule_id = remember["_meta"]["rule_id"]
        .as_str()
        .expect("rule_id in _meta")
        .to_owned();

    // Attach an example, producing the `extracted` event.
    let example_input = crate::domain::models::AddExampleInput {
        skill_id: rule_id.clone(),
        bad_code: "let x = unwrap();".into(),
        good_code: "let x = result?;".into(),
        description: Some("Prefer `?` over unwrap in request handlers".into()),
        source: Some("extracted".into()),
    };
    let example = crate::skills::add_example(&state.db, example_input)
        .await
        .unwrap();
    // Force the example timestamp strictly after the skill's installed_at;
    // SQLite's 1-sec resolution means same-second ties fall to id-tie-break
    // and make the ordering assertion non-deterministic.
    sqlx::query!(
        "UPDATE rule_examples SET created_at = datetime('now', '+1 second') WHERE id = ?1",
        example.id
    )
    .execute(&state.db)
    .await
    .unwrap();

    // Persist an explicit feedback signal; the timeline reads this from
    // rule_events rather than guessing from skills.updated_at.
    crate::skills::update_confidence(
        &state.db,
        crate::domain::models::UpdateConfidenceInput {
            skill_id: rule_id.clone(),
            signal: "accept".into(),
        },
    )
    .await
    .unwrap();
    sqlx::query!(
        "UPDATE rule_events SET created_at = datetime('now', '+2 second') WHERE skill_id = ?1",
        rule_id
    )
    .execute(&state.db)
    .await
    .unwrap();

    let (body, _) = call_tool_json(
        &state,
        602,
        "rule_timeline",
        json!({
            "rule_id": rule_id.clone(),
            "depth_before": 5,
            "depth_after": 5,
        }),
    )
    .await;
    let events = body["events"].as_array().expect("events array");

    assert!(
        events.len() >= 3,
        "expected >= 3 events (remember + extracted + feedback), got: {events:?}"
    );

    assert_eq!(events[0]["kind"].as_str(), Some("remember"));
    assert_eq!(events[0]["source"].as_str(), Some("conversation"));
    assert_eq!(events[0]["id"].as_str(), Some(rule_id.as_str()));
    assert!(
        events[0]["evidence"]
            .as_array()
            .expect("first event evidence")
            .iter()
            .any(|item| item["kind"].as_str() == Some("ruleCreated")),
        "creation event should carry ruleCreated evidence: {events:?}"
    );

    // Chronological ascending: each ts must be >= the previous one.
    for pair in events.windows(2) {
        let a = pair[0]["ts"].as_str().unwrap();
        let b = pair[1]["ts"].as_str().unwrap();
        assert!(a <= b, "events not sorted asc: {a} vs {b}");
    }

    for e in events {
        let preview = e["preview"].as_str().unwrap();
        assert!(
            preview.chars().count() <= 120,
            "preview too long ({} chars): {preview}",
            preview.chars().count()
        );
    }

    let has_extracted = events.iter().any(|e| {
        e["kind"].as_str() == Some("extracted") && e["source"].as_str() == Some("extracted")
    });
    assert!(has_extracted, "missing extracted example event: {events:?}");

    let feedback = events
        .iter()
        .find(|e| e["kind"].as_str() == Some("feedback_accept"))
        .expect("missing persisted feedback event");
    assert_eq!(feedback["source"].as_str(), Some("local_feedback"));
    assert!(
        feedback["preview"]
            .as_str()
            .unwrap()
            .contains("confidence 0.60 -> 0.65"),
        "feedback event should include persisted confidence delta: {feedback:?}"
    );

    let has_example_evidence = events.iter().any(|e| {
        e["evidence"].as_array().is_some_and(|items| {
            items
                .iter()
                .any(|item| item["kind"].as_str() == Some("ruleExample"))
        })
    });
    assert!(
        has_example_evidence,
        "missing example evidence payload: {events:?}"
    );

    // Dump one sample row under DIFFLORE_TEST_DUMP=1 to inspect the payload
    // shape without digging into the JSON-RPC envelope.
    if std::env::var_os("DIFFLORE_TEST_DUMP").is_some() {
        eprintln!(
            "[rule_timeline.sample] {}",
            serde_json::to_string_pretty(&events[0]).unwrap()
        );
    }

    assert!(
        body.get("stubbed_sources").is_none(),
        "rule_timeline should expose persisted events, not gap markers"
    );
}

#[tokio::test]
async fn rule_timeline_depth_caps_at_twenty() {
    // Over-asking for 999 each side must clamp to 20 (the advertised max)
    // without erroring, so an agent can't blow the response budget.
    let state = build_state().await;
    let remember = call_ok(
        &state,
        &call_tool(
            610,
            "remember_rule",
            json!({ "title": "Depth cap rule", "body": "Body for depth cap test." }),
        ),
    )
    .await;
    let rule_id = remember["_meta"]["rule_id"].as_str().unwrap().to_owned();

    let _ = call_ok(
        &state,
        &call_tool(
            611,
            "rule_timeline",
            json!({
                "rule_id": rule_id,
                "depth_before": 999,
                "depth_after": 999
            }),
        ),
    )
    .await;
}

#[tokio::test]
async fn rule_timeline_errors_on_unknown_rule() {
    let state = build_state().await;
    let req = call_tool(620, "rule_timeline", json!({ "rule_id": "does-not-exist" }));
    let resp = handle_message(&state, &req).await.unwrap();
    assert!(
        resp.get("error").is_some(),
        "expected JSON-RPC error for unknown rule_id, got: {resp}"
    );
}

#[tokio::test]
async fn rule_timeline_emits_source_repo_when_present() {
    let state = build_state().await;
    let remember = call_ok(
        &state,
        &call_tool(
            630,
            "remember_rule",
            json!({ "title": "Provenance rule", "body": "Body for provenance test." }),
        ),
    )
    .await;
    let rule_id = remember["_meta"]["rule_id"].as_str().unwrap().to_owned();

    // Backfill source_repo: public creation surfaces leave it null, so set
    // it explicitly to exercise the timeline's serialisation branch.
    sqlx::query!(
        "UPDATE skills SET source_repo = ?1 WHERE id = ?2",
        "github.com/example/repo",
        rule_id
    )
    .execute(&state.db)
    .await
    .unwrap();

    let (body, _) = call_tool_json(
        &state,
        631,
        "rule_timeline",
        json!({ "rule_id": rule_id.clone() }),
    )
    .await;
    assert_eq!(
        body["source_repo"].as_str(),
        Some("github.com/example/repo"),
        "expected source_repo in body: {body}"
    );
    assert_eq!(body["rule_id"].as_str(), Some(rule_id.as_str()));
}

#[tokio::test]
async fn rule_timeline_emits_capture_client_when_present() {
    let state = build_state().await;
    let remember = call_ok(
        &state,
        &call_tool(
            635,
            "remember_rule",
            json!({ "title": "Capture-client rule", "body": "Body for capture client test." }),
        ),
    )
    .await;
    let rule_id = remember["_meta"]["rule_id"].as_str().unwrap().to_owned();

    let (body, _) = call_tool_json(
        &state,
        636,
        "rule_timeline",
        json!({ "rule_id": rule_id.clone() }),
    )
    .await;
    assert_eq!(
        body["captured_by_client"].as_str(),
        Some("mcp-server"),
        "expected captured_by_client in body: {body}"
    );
    assert_eq!(body["rule_id"].as_str(), Some(rule_id.as_str()));
}

#[tokio::test]
async fn rule_timeline_omits_source_repo_when_absent() {
    let state = build_state().await;
    let remember = call_ok(
        &state,
        &call_tool(
            640,
            "remember_rule",
            json!({ "title": "No-provenance rule", "body": "Body for the no-provenance test." }),
        ),
    )
    .await;
    let rule_id = remember["_meta"]["rule_id"].as_str().unwrap().to_owned();
    sqlx::query!(
        "UPDATE skills SET source_repo = NULL WHERE id = ?1",
        rule_id
    )
    .execute(&state.db)
    .await
    .unwrap();

    let (body, _) = call_tool_json(
        &state,
        641,
        "rule_timeline",
        json!({ "rule_id": rule_id.clone() }),
    )
    .await;
    // When source_repo is NULL the serialiser must omit the key entirely
    // (not emit `null`), so agents don't have to special-case the falsy form.
    assert!(
        body.get("source_repo").is_none(),
        "expected source_repo to be omitted, got: {body}"
    );
}

#[tokio::test]
async fn rule_timeline_omits_source_repo_when_blank() {
    // Empty/whitespace strings must elide too, so a row can't print a stray
    // "from " line on downstream surfaces.
    let state = build_state().await;
    let remember = call_ok(
        &state,
        &call_tool(
            650,
            "remember_rule",
            json!({ "title": "Blank-provenance rule", "body": "Body for the blank-provenance test." }),
        ),
    )
    .await;
    let rule_id = remember["_meta"]["rule_id"].as_str().unwrap().to_owned();
    sqlx::query!(
        "UPDATE skills SET source_repo = ?1 WHERE id = ?2",
        "   ",
        rule_id
    )
    .execute(&state.db)
    .await
    .unwrap();

    let (body, _) =
        call_tool_json(&state, 651, "rule_timeline", json!({ "rule_id": rule_id })).await;
    assert!(
        body.get("source_repo").is_none(),
        "expected blank source_repo to be elided, got: {body}"
    );
}

#[tokio::test]
async fn resource_templates_list_advertises_verdicts_and_signatures() {
    let state = build_state().await;
    let result = call_ok(&state, &rpc(700, "resources/templates/list")).await;
    let templates: Vec<String> = result["resourceTemplates"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["uriTemplate"].as_str().map(String::from))
        .collect();
    assert!(
        templates.iter().any(|t| t == "difflore://verdicts/{id}"),
        "missing verdicts template: {templates:?}"
    );
    assert!(
        templates
            .iter()
            .any(|t| t == "difflore://signatures/{hash}"),
        "missing signatures template: {templates:?}"
    );
}

#[tokio::test]
async fn resources_list_advertises_explore_and_journey_skills() {
    let state = build_state().await;
    let result = call_ok(&state, &rpc(703, "resources/list")).await;
    let resources: Vec<String> = result["resources"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["uri"].as_str().map(String::from))
        .collect();
    assert!(
        resources
            .iter()
            .any(|t| t == "difflore://skills/smart-explore"),
        "missing smart-explore resource: {resources:?}"
    );
    assert!(
        resources
            .iter()
            .any(|t| t == "difflore://skills/rule-journey"),
        "missing rule-journey resource: {resources:?}"
    );
    // Every shipped plugin skill should also be mirrored as an MCP resource so
    // the advertised set stays in lockstep with plugin/skills/.
    for uri in [
        "difflore://skills/knowledge-agent",
        "difflore://skills/session-recap",
        "difflore://skills/difflore-onboard",
    ] {
        assert!(
            resources.iter().any(|t| t == uri),
            "missing {uri} resource: {resources:?}"
        );
    }
}

#[tokio::test]
async fn resource_read_smart_explore_returns_skill_markdown() {
    let state = build_state().await;
    let result = call_ok(
        &state,
        &rpc_with(
            704,
            "resources/read",
            json!({ "uri": "difflore://skills/smart-explore" }),
        ),
    )
    .await;
    let contents = result["contents"][0].clone();
    assert_eq!(
        contents["uri"].as_str(),
        Some("difflore://skills/smart-explore")
    );
    assert_eq!(contents["mimeType"].as_str(), Some("text/markdown"));
    assert!(contents["text"].as_str().unwrap().contains("rg --files"));
}

#[tokio::test]
async fn resource_read_rule_search_tells_agents_to_pass_file_to_get_rules() {
    let state = build_state().await;
    let result = call_ok(
        &state,
        &rpc_with(
            705,
            "resources/read",
            json!({ "uri": "difflore://skills/rule-search" }),
        ),
    )
    .await;
    let contents = result["contents"][0].clone();
    assert_eq!(
        contents["uri"].as_str(),
        Some("difflore://skills/rule-search")
    );
    assert_eq!(contents["mimeType"].as_str(), Some("text/markdown"));
    let text = contents["text"].as_str().expect("skill text");
    assert!(
        text.contains("get_rules(ids=[\"conv-a1f9c\"], file=\"src/worker.rs\")"),
        "rule-search skill should preserve file scope on get_rules: {text}"
    );
}

#[tokio::test]
async fn resource_read_verdict_returns_stub_with_deep_link() {
    let state = build_state().await;
    let result = call_ok(
        &state,
        &rpc_with(
            701,
            "resources/read",
            json!({ "uri": "difflore://verdicts/ext-abc123" }),
        ),
    )
    .await;
    let contents = result["contents"][0].clone();
    assert_eq!(
        contents["uri"].as_str(),
        Some("difflore://verdicts/ext-abc123")
    );
    assert_eq!(contents["mimeType"].as_str(), Some("application/json"));
    let body: Value = serde_json::from_str(contents["text"].as_str().unwrap()).unwrap();
    assert_eq!(body["id"].as_str(), Some("ext-abc123"));
    assert_eq!(body["kind"].as_str(), Some("past_verdict"));
    assert!(
        body["deep_link"]
            .as_str()
            .unwrap()
            .contains("verdicts/ext-abc123")
    );
    // The note is the no-local-cache disclosure, so agents don't pretend to
    // have detail they can't serve.
    assert!(body["note"].is_string());
    assert_eq!(body["status"].as_str(), Some("not_cached_locally"));
    assert!(
        !body["note"]
            .as_str()
            .expect("note")
            .to_ascii_lowercase()
            .contains("todo")
    );
}

#[tokio::test]
async fn resource_read_signature_echoes_hash_and_links_cloud() {
    let state = build_state().await;
    let result = call_ok(
        &state,
        &rpc_with(
            702,
            "resources/read",
            json!({ "uri": "difflore://signatures/deadbeefcafe" }),
        ),
    )
    .await;
    let contents = result["contents"][0].clone();
    let body: Value = serde_json::from_str(contents["text"].as_str().unwrap()).unwrap();
    assert_eq!(body["hash"].as_str(), Some("deadbeefcafe"));
    assert_eq!(body["kind"].as_str(), Some("signature"));
    assert!(
        body["deep_link"]
            .as_str()
            .unwrap()
            .contains("signatures/deadbeefcafe")
    );
}

#[test]
fn rag_eval_seed_fixture_keeps_minimum_contract() {
    let fixture: Value = serde_json::from_str(include_str!(
        // Path is relative to THIS file. After the R4 tests/ split this file
        // lives one directory deeper (mcp_server/tests/) than the old
        // mcp_server/tests.rs, hence the extra `../` to reach the crate-level
        // tests/fixtures/ directory.
        "../../../tests/fixtures/rag-eval-seed-cases.json"
    ))
    .expect("RAG seed fixture is valid JSON");
    assert_eq!(fixture["version"].as_i64(), Some(1));
    assert_eq!(fixture["status"].as_str(), Some("seed_fixture"));

    let rules = fixture["rules"].as_array().expect("rules array");
    let cases = fixture["cases"].as_array().expect("cases array");
    assert!(rules.len() >= 5, "seed fixture needs at least 5 rules");
    assert!(cases.len() >= 5, "seed fixture needs at least 5 cases");

    let rule_ids: HashSet<&str> = rules
        .iter()
        .map(|rule| {
            let id = rule["id"].as_str().expect("rule id");
            for required in ["title", "body", "sourceRepo"] {
                assert!(
                    rule[required]
                        .as_str()
                        .is_some_and(|value| !value.trim().is_empty()),
                    "rule {id} missing {required}"
                );
            }
            assert!(
                rule["filePatterns"]
                    .as_array()
                    .is_some_and(|patterns| !patterns.is_empty()),
                "rule {id} needs filePatterns"
            );
            id
        })
        .collect();

    let mut surfaces = HashSet::new();
    let mut has_documentation_negative_case = false;
    for case in cases {
        let id = case["id"].as_str().expect("case id");
        let surface = case["surface"].as_str().expect("case surface");
        surfaces.insert(surface);
        for required in ["query", "file", "intent", "rationale"] {
            assert!(
                case[required]
                    .as_str()
                    .is_some_and(|value| !value.trim().is_empty()),
                "case {id} missing {required}"
            );
        }
        assert!(
            case["metrics"]
                .as_array()
                .is_some_and(|metrics| !metrics.is_empty()),
            "case {id} needs metrics"
        );
        let expected = case["expectedRuleIds"]
            .as_array()
            .expect("expectedRuleIds array");
        let forbidden = case["forbiddenRuleIds"]
            .as_array()
            .expect("forbiddenRuleIds array");
        for rule_id in expected.iter().chain(forbidden.iter()) {
            let rule_id = rule_id.as_str().expect("case rule id string");
            assert!(
                rule_ids.contains(rule_id),
                "case {id} references unknown rule id {rule_id}"
            );
        }
        if expected.is_empty()
            && !forbidden.is_empty()
            && case["file"].as_str() == Some("README.md")
        {
            has_documentation_negative_case = true;
        }
    }
    for surface in ["search_rules", "hook_post_edit"] {
        assert!(
            surfaces.contains(surface),
            "seed fixture missing {surface} case"
        );
    }
    assert!(
        has_documentation_negative_case,
        "seed fixture must keep a documentation-only negative case"
    );
}

/// Overwrite the persisted corpus embedding profile on the project index
/// DB. Mirrors `index_db::write_meta` (which is `pub(super)` and unreachable
/// here) so a test can pin the corpus profile and construct a mismatch on
/// demand, independent of whatever embedder the test box resolves.
async fn overwrite_index_embedding_profile(pool: &sqlx::SqlitePool, profile: &str) {
    sqlx::query!(
        "INSERT INTO rule_index_meta (key, value)\n         VALUES (?1, ?2)\n         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        "embedding_profile",
        profile
    )
    .execute(pool)
    .await
    .expect("overwrite embedding_profile meta row");
}

/// A 1536d-profile corpus served to a 128d SHA1 fallback query embedder
/// must not silently look like a normal retrieval. Asserts the degradation
/// is surfaced both at the diagnostic-function level and in the
/// agent-visible `search_rules` `_meta.embedding` channel, with a
/// matched-profile positive control proving the signal is not a constant.
#[tokio::test]
async fn embedding_profile_mismatch_is_surfaced_not_silent() {
    let state = build_state().await;

    // remember_rule builds the project index and persists the active
    // embedder's profile into `rule_index_meta`.
    let _rule_id = remember_rule_with_patterns(
        &state,
        "Embedding lane guard rule",
        "When recall degrades to lexical fallback, surface it instead of silently serving noise.",
        &["src/**/*.rs"],
    )
    .await;
    let index_pool = state.index_pool.as_ref().expect("test index pool");

    // The test box normally has no cloud token, so the active embedder is
    // the local SHA1 lexical hash. A dev machine with a real cloud token
    // breaks the sha1-vs-cloud mismatch precondition, so the mismatch
    // assertions are guarded on it.
    let active = crate::context::embedding::active_embedding_profile().await;
    let sha1_fallback_active = active.starts_with("sha1:");

    if sha1_fallback_active {
        assert_eq!(
            active, "sha1:local:128",
            "test default embedder must be the 128d SHA1 fallback"
        );

        // A 1536d cloud-embedded corpus queried by the 128d SHA1 fallback.
        overwrite_index_embedding_profile(index_pool, "cloud:text-embedding-3-small:1536").await;

        // Read straight off the mutated meta row with no tool call in
        // between, so nothing can self-heal the profile before we observe it.
        let diag = crate::context::gather_embedding_diagnostics(index_pool).await;
        assert!(
            diag.degraded,
            "1536d corpus + 128d SHA1 query must be flagged degraded, got: {diag:?}"
        );
        assert!(
            !diag.vector_lane_available,
            "lexical query vs semantic corpus is a dead vector lane: {diag:?}"
        );
        assert!(
            !diag.profile_match,
            "sha1:local:128 must not match cloud:...:1536: {diag:?}"
        );
        assert!(
            matches!(
                diag.degraded_reason.as_deref(),
                Some("provider_fallback" | "dimension_mismatch")
            ),
            "degraded_reason must classify the sha1-vs-cloud fallback, got: {:?}",
            diag.degraded_reason
        );
        assert_eq!(
            diag.active_profile, "sha1:local:128",
            "diagnostic must report the active fallback embedder: {diag:?}"
        );
        assert_eq!(
            diag.index_profile.as_deref(),
            Some("cloud:text-embedding-3-small:1536"),
            "diagnostic must report the persisted semantic corpus profile: {diag:?}"
        );

        // The degradation must reach the agent-visible `_meta.embedding`
        // block. Assert the block is present and well-formed with every
        // contract key of the right type; the semantic verdict is pinned by
        // the function-level assertions above, since a tool call runs
        // `ensure_rules_indexed` whose rebuild path can reset the meta.
        let (_body, result) = call_tool_json(
            &state,
            770,
            "search_rules",
            json!({
                "file": "src/lib.rs",
                "intent": "embedding lane guard recall degradation",
                "top_k": 3,
                "repo_full_name": TEST_REPO
            }),
        )
        .await;
        let embedding = &result["_meta"]["embedding"];
        assert!(
            embedding.is_object(),
            "search_rules _meta must carry the embedding diagnostics block so \
             degradation is not silent: {result}"
        );
        for (key, kind) in [
            ("activeProfile", "string"),
            ("profileMatch", "bool"),
            ("degraded", "bool"),
            ("vectorLaneAvailable", "bool"),
        ] {
            let v = &embedding[key];
            let ok = match kind {
                "string" => v.is_string(),
                "bool" => v.is_boolean(),
                _ => false,
            };
            assert!(ok, "_meta.embedding.{key} must be a {kind}: {embedding}");
        }
        // `indexProfile` is Option<String> → string or null; never absent.
        assert!(
            embedding.get("indexProfile").is_some()
                && (embedding["indexProfile"].is_string() || embedding["indexProfile"].is_null()),
            "_meta.embedding.indexProfile must be present (string|null): {embedding}"
        );
        // `degradedReason` is Option<String> → string or null; never absent.
        assert!(
            embedding.get("degradedReason").is_some()
                && (embedding["degradedReason"].is_string()
                    || embedding["degradedReason"].is_null()),
            "_meta.embedding.degradedReason must be present (string|null): {embedding}"
        );
    }

    // Positive control: a matched profile is a healthy lane. This state is
    // stable across `ensure_rules_indexed` (matching profile → no rebuild →
    // no meta clobber), so the verdict is deterministic and proves the
    // degraded signal above is a real discriminator, not a constant.
    overwrite_index_embedding_profile(index_pool, &active).await;
    let healthy = crate::context::gather_embedding_diagnostics(index_pool).await;
    assert!(
        !healthy.degraded && healthy.vector_lane_available && healthy.profile_match,
        "matched profile must be a healthy lane: {healthy:?}"
    );

    let (_body, control) = call_tool_json(
        &state,
        771,
        "search_rules",
        json!({
            "file": "src/lib.rs",
            "intent": "embedding lane guard recall degradation",
            "top_k": 3,
            "repo_full_name": TEST_REPO
        }),
    )
    .await;
    let control_embedding = &control["_meta"]["embedding"];
    assert!(
        control_embedding.is_object(),
        "search_rules _meta must always carry the embedding block: {control}"
    );
    assert_eq!(
        control_embedding["degraded"].as_bool(),
        Some(false),
        "matched-profile control must report a non-degraded lane: {control_embedding}"
    );
    assert_eq!(
        control_embedding["vectorLaneAvailable"].as_bool(),
        Some(true),
        "matched-profile control must report an available vector lane: {control_embedding}"
    );
    assert_eq!(
        control_embedding["profileMatch"].as_bool(),
        Some(true),
        "matched-profile control must report a profile match: {control_embedding}"
    );
}
