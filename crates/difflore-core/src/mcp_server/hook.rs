use serde_json::Value;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use tokio::io::{AsyncBufReadExt, AsyncReadExt as _, AsyncWriteExt, BufReader};

use crate::cloud::client::CloudClient;
use crate::context::retrieval::RuleRankingWhy;
use crate::context::{EmbeddingDiagnostics, gather_embedding_diagnostics_with_activity};
use crate::error::CoreError;
use crate::observability::injection_log::InjectionDropReason;
use crate::observability::trajectory::TrajectoryStep;

const MAX_JSONRPC_LINE_BYTES: u64 = 16 * 1024 * 1024;
const HOOK_SERVE_RECORD_ERR_PREFIX: &str = "[difflore-hook] failed to record mcp_rule_serves row";

/// Run the MCP server event loop. Reads JSON-RPC messages line-by-line
/// from stdin and writes responses to stdout. Runs until stdin is closed.
pub async fn run(db: SqlitePool) -> Result<(), Box<dyn std::error::Error>> {
    let cloud = CloudClient::create().await;
    // Index pools are resolved per-call (by project), so there is no single
    // pool to open here; the first tool call lazily creates one.
    let state = McpState {
        db,
        cloud,
        index_pool: None,
    };

    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut reader = BufReader::new(stdin);
    let mut line = String::new();

    loop {
        line.clear();
        let n = match read_jsonrpc_line_capped(&mut reader, &mut line).await {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                let err = jsonrpc_error(Value::Null, -32700, &e.to_string());
                let out = jsonrpc_line_bytes(&err);
                stdout.write_all(&out).await?;
                stdout.flush().await?;
                break;
            }
            Err(e) => return Err(Box::new(e)),
        };
        if n == 0 {
            break;
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let msg: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                let err = jsonrpc_error(Value::Null, -32700, &format!("Parse error: {e}"));
                let out = jsonrpc_line_bytes(&err);
                stdout.write_all(&out).await?;
                stdout.flush().await?;
                continue;
            }
        };

        if let Some(response) = handle_message(&state, &msg).await {
            let out = jsonrpc_line_bytes(&response);
            stdout.write_all(&out).await?;
            stdout.flush().await?;
        }
    }

    Ok(())
}

async fn read_jsonrpc_line_capped<R>(reader: &mut R, line: &mut String) -> std::io::Result<usize>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let n = reader
        .take(MAX_JSONRPC_LINE_BYTES + 1)
        .read_line(line)
        .await?;
    if n as u64 > MAX_JSONRPC_LINE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("JSON-RPC line exceeds {MAX_JSONRPC_LINE_BYTES} bytes"),
        ));
    }
    Ok(n)
}

fn jsonrpc_line_bytes(value: &Value) -> Vec<u8> {
    match serde_json::to_vec(value) {
        Ok(mut out) => {
            out.push(b'\n');
            out
        }
        Err(e) => {
            if crate::infra::env::debug_telemetry() {
                eprintln!("[difflore-mcp] failed to serialize JSON-RPC response: {e}");
            }
            b"{\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{\"code\":-32603,\"message\":\"serialize failed\"}}\n".to_vec()
        }
    }
}

/// Injectable rule context produced by `fetch_relevant_rules_for_hook`. Kept
/// separate from the MCP server's JSON-RPC formatting so the hook path doesn't
/// pay the envelope cost.
#[derive(Debug, Clone)]
pub struct HookRuleContext {
    /// Markdown rule block for the assistant's next-turn context. Empty when no
    /// rules matched.
    pub rendered: String,
    pub rules_injected: usize,
    /// Rule ids in injection order, for follow-up hints like
    /// `rule_timeline(rule_id=...)`.
    pub rule_ids: Vec<String>,
    /// Structured local audit reason when `rules_injected == 0`.
    pub drop_reason: Option<InjectionDropReason>,
}

fn hook_embedding_health_header(diag: &EmbeddingDiagnostics) -> String {
    if !diag.degraded && diag.vector_lane_available {
        return String::new();
    }

    let reason = diag
        .degraded_reason
        .as_deref()
        .unwrap_or("unknown_embedding_state");
    format!(
        "> DiffLore retrieval health: embeddingDegraded={} vectorLaneAvailable={} reason={reason}. \
         Treat injected memories as lower-confidence unless strict file/source evidence applies.\n\n",
        diag.degraded, diag.vector_lane_available
    )
}

/// Cross-repo starter rules the hook injects in the cold-start case. Kept
/// small since these are transferable suggestions from other repos, not this
/// repo's own ratified judgment.
const CROSS_REPO_STARTER_HOOK_TOP_K: usize = 3;

/// Hook-path rule fetch. Returns rendered text + injection count without the
/// JSON-RPC envelope so an in-process consumer can pull rules in one call.
/// Keep retrieval, formatting, telemetry, and token accounting aligned with
/// the rule detail paths when changing this surface.
///
/// `intent` is a short string describing why we're asking (e.g. `"post-edit"`)
/// and feeds into the same retrieval query the MCP tool uses.
pub async fn fetch_relevant_rules_for_hook(
    db: &SqlitePool,
    index_pool: &SqlitePool,
    file: &str,
    intent: &str,
    session_id: Option<&str>,
) -> Result<HookRuleContext, CoreError> {
    fetch_relevant_rules_for_hook_inner(db, index_pool, file, intent, session_id, None).await
}

pub async fn fetch_relevant_rules_for_hook_with_repo_scopes(
    db: &SqlitePool,
    index_pool: &SqlitePool,
    file: &str,
    intent: &str,
    session_id: Option<&str>,
    repo_scopes: &[String],
) -> Result<HookRuleContext, CoreError> {
    fetch_relevant_rules_for_hook_inner(db, index_pool, file, intent, session_id, Some(repo_scopes))
        .await
}

/// Bash-error recall uses the same retrieval/ranking path as hook rule recall,
/// but it must not inherit post-edit cache behavior or copy that talks about a
/// "current change".
pub async fn fetch_relevant_rules_for_bash_error(
    db: &SqlitePool,
    index_pool: &SqlitePool,
    file: &str,
    intent: &str,
    session_id: Option<&str>,
) -> Result<HookRuleContext, CoreError> {
    fetch_relevant_rules_for_hook_inner(db, index_pool, file, intent, session_id, None).await
}

pub async fn fetch_relevant_rules_for_bash_error_with_repo_scopes(
    db: &SqlitePool,
    index_pool: &SqlitePool,
    file: &str,
    intent: &str,
    session_id: Option<&str>,
    repo_scopes: &[String],
) -> Result<HookRuleContext, CoreError> {
    fetch_relevant_rules_for_hook_inner(db, index_pool, file, intent, session_id, Some(repo_scopes))
        .await
}

async fn fetch_relevant_rules_for_hook_inner(
    db: &SqlitePool,
    index_pool: &SqlitePool,
    file: &str,
    intent: &str,
    session_id: Option<&str>,
    repo_scopes_override: Option<&[String]>,
) -> Result<HookRuleContext, CoreError> {
    let trace = crate::infra::env::trace_hook();
    let started = std::time::Instant::now();
    let mut last = started;
    let mut mark = |label: &str| {
        if trace {
            let now = std::time::Instant::now();
            eprintln!(
                "[difflore.hook.trace] {label}: +{}ms total={}ms",
                now.duration_since(last).as_millis(),
                now.duration_since(started).as_millis()
            );
            last = now;
        }
    };

    // Short-circuit empty-prone post-edit extensions before the index
    // round-trip. Two lanes reach this function: post-edit and bash-error.
    // (The retired "pre-read" intent has no remaining producer: the CLI
    // dispatcher noops PreToolUse(Read) before any retrieval call, and the
    // hook-forward IPC ships raw events — never intent strings — behind a
    // version-pinned socket, so an old shim cannot inject one either.)
    let ext_key = super::hook_short_circuit::extension_key(file);
    let short_circuit_mode = crate::infra::env::hook_short_circuit_mode();
    let short_circuit_cache = super::hook_short_circuit::global_cache();
    let is_bash_error_path = intent == "bash-error" || intent.starts_with("bash-error ");
    let is_post_edit_path = !is_bash_error_path;
    let short_circuit_now = is_post_edit_path
        && !ext_key.is_empty()
        && match short_circuit_mode {
            crate::infra::env::HookShortCircuitMode::Off => false,
            crate::infra::env::HookShortCircuitMode::Force => true,
            crate::infra::env::HookShortCircuitMode::Auto => {
                short_circuit_cache.should_short_circuit(&ext_key)
            }
        };
    if short_circuit_now {
        // Return empty without recording, so the extension can recover when
        // the corpus improves.
        if trace {
            eprintln!(
                "[difflore.hook.trace] short_circuit ext={ext_key} mode={short_circuit_mode:?} elapsed=0ms"
            );
        }
        return Ok(HookRuleContext {
            rendered: String::new(),
            rules_injected: 0,
            rule_ids: Vec::new(),
            drop_reason: Some(InjectionDropReason::ShortCircuit),
        });
    }

    let query = crate::context::retrieval::build_recall_query_with_signals(file, intent);
    // Scope to the calling repo/project only. On 0 hits we return no
    // rules; runtime recall must not fall back to another project.
    let detected_repos = if let Some(repo_scopes) = repo_scopes_override {
        repo_scopes.to_vec()
    } else {
        refresh_configured_gitlab_hosts_for_remote_detection().await;
        detect_git_remote_owner_repos()
    };
    let repo_scopes = crate::skills::expand_repo_scopes_with_source_aliases(db, &detected_repos)
        .await
        .unwrap_or(detected_repos);

    let scoped_count = if repo_scopes.is_empty() {
        0
    } else {
        crate::context::orchestrator::ensure_rules_indexed_for_repo_scopes_local_embeddings(
            db,
            index_pool,
            &repo_scopes,
        )
        .await
        .map_err(|e| CoreError::Internal(format!("hook rule index rebuild failed: {e}")))?
    };
    mark("ensure_rules_indexed");
    let embedding_diag = gather_embedding_diagnostics_with_activity(index_pool).await;
    mark("embedding_diagnostics");

    let target_file = if file == "unknown" { None } else { Some(file) };
    // Ranking inputs are best-effort; SQL failures fall back to defaults.
    let ranking_inputs = crate::context::rule_source::load_rule_ranking_inputs(db).await;
    mark("load_rule_ranking_inputs");
    // Hooks render at most 5 rules to keep unsolicited context small. A
    // low-rate sampler occasionally widens the candidate window so deeper
    // ranks get measured without changing normal hook behavior.
    let hook_top_k = super::recall_sampler::maybe_bump_top_k(
        5usize,
        crate::infra::env::deep_recall_sample_rate(),
    );
    let candidate_limit = hook_top_k.saturating_mul(5).clamp(hook_top_k, 50);
    let mut scored = tools::serve_stats::retrieve_rules_with_repo_scopes(
        index_pool,
        tools::serve_stats::RetrieveRulesArgs {
            query: &query,
            lexical_query: None,
            top_k: candidate_limit,
            target_file,
            repo_scopes: &repo_scopes,
            confidence_map: ranking_inputs.confidence_map.as_ref(),
            age_days_map: ranking_inputs.age_days_map.as_ref(),
            ann_enabled: true,
            local_query_embedding: true,
            embedding_timeout: None,
            strict_file_scope: true,
            adaptive_prune: true,
        },
    )
    .await?;
    mark("retrieve_rules");

    // `score` is the fused embedding+FTS score, not raw cosine. Keep a
    // small floor so a near-zero best candidate does not get injected just
    // because it ranked first in a weak set.
    const HOOK_MIN_RAW_SCORE: f64 = 0.005;
    scored.retain(|r| r.score >= HOOK_MIN_RAW_SCORE);
    let candidate_ids: Vec<String> = scored.iter().map(|s| s.skill_id.clone()).collect();
    let meta_map = tools::evidence::fetch_skills_by_ids(db, &candidate_ids)
        .await
        .unwrap_or_default();
    let strict_skill_ids = tools::evidence::strict_file_match_ids_for_meta(&meta_map, target_file);
    if is_post_edit_path {
        scored.retain(|rule| {
            meta_map
                .get(&rule.skill_id)
                .is_none_or(|row| hook_auto_injection_allowed(row, &strict_skill_ids))
        });
    }
    scored = tools::serve_stats::rerank_scored_rule_chunks_for_mcp_by_strict_file_matches(
        scored,
        intent,
        hook_top_k,
        &strict_skill_ids,
    );

    // C6 misapply unification: run the same intent-alignment gate the explicit
    // `search_rules` path applies, on the post-edit lane only (bash-error
    // keeps its narrower copy/recall semantics). The post-edit
    // intent carries the diff excerpt, so the gate aligns rule directives
    // against the actual change. Behind `DIFFLORE_HOOK_INTENT_GATE`
    // (default: see `env::DEFAULT_HOOK_INTENT_GATE`).
    //
    // External-messaging note: gate ON ⇒ "misapply guard covers explicit
    // recall AND hook injection"; gate OFF ⇒ the claim must be split per
    // surface ("intent alignment on explicit recall; hook injection guarded
    // by file patterns + score floors only"). Keep README/cli-spec wording in
    // sync with the shipped default.
    if is_post_edit_path && crate::infra::env::hook_intent_gate_enabled() {
        crate::context::retrieval::apply_intent_alignment_gate(&mut scored, intent);
    }

    // Deterministic serve arbitration (10% score band → path hint → source
    // priority → confidence → skill_id), reusing the metadata batch fetched
    // above — zero additional queries. `DIFFLORE_DISABLE_SOURCE_PRIORITY`
    // rolls the re-sort back; the why facts stay available either way.
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

    // Optional cold-start fallback for repos with no scoped memory. Path hints
    // may boost results from an already-built starter index, and results remain
    // labeled as cross-repo suggestions.
    let mut cross_repo_starter = false;
    if scored.is_empty()
        && scoped_count == 0
        && crate::infra::env::hook_cross_repo_starter_enabled()
        && let Some(tf) = target_file
    {
        let cross = tools::serve_stats::cross_repo_starter_scored(
            db,
            &query,
            tf,
            ranking_inputs.confidence_map.as_ref(),
            ranking_inputs.age_days_map.as_ref(),
            CROSS_REPO_STARTER_HOOK_TOP_K,
        )
        .await;
        if !cross.is_empty() {
            scored = cross;
            cross_repo_starter = true;
        }
        mark("cross_repo_starter");
    }

    let (hook_label, hook_tool) = if is_bash_error_path {
        ("bash-error", "hook_bash_error")
    } else {
        ("post-edit", "hook_post_edit")
    };

    if scored.is_empty() {
        // Record the empty serve locally and enqueue the matching event.
        let served_event = serve_and_record(
            db,
            RuleServe {
                tool: hook_tool,
                session_id,
                event_session_id: session_id.unwrap_or("hook"),
                repo_full_name: repo_scopes.first().map(String::as_str),
                target_file,
                query: &query,
                rule_ids: &[],
                top_k: i64::try_from(hook_top_k).unwrap_or(i64::MAX),
                strict_match_count: 0,
                estimated_tokens: 0,
            },
            hook_serve_record_err_prefix(),
        )
        .await;
        let _ = crate::cloud::observations::enqueue_default(served_event).await;
        if is_post_edit_path && !ext_key.is_empty() {
            short_circuit_cache.record(&ext_key, true);
        }
        return Ok(HookRuleContext {
            rendered: String::new(),
            rules_injected: 0,
            rule_ids: Vec::new(),
            drop_reason: Some(InjectionDropReason::RetrievalEmpty),
        });
    }

    let skill_ids_all: Vec<String> = scored.iter().map(|s| s.skill_id.clone()).collect();
    let examples_fut = crate::context::rule_source::load_rule_examples_batch(db, &skill_ids_all);
    let trust_evidence_fut =
        super::trust_proof::fetch_default_cloud_top_rule_trust_evidence_for_hook();
    let (examples_result, trust_evidence) = tokio::join!(examples_fut, trust_evidence_fut);
    let examples_map = examples_result.unwrap_or_default();
    mark("load_rule_examples_batch");

    // Hard token budget for hook injection. We use the same rough
    // `chars / 4` estimate used elsewhere in the MCP path.
    const HOOK_INJECTION_TOKEN_BUDGET: usize = 1500;
    let mut text = hook_embedding_health_header(&embedding_diag);
    if cross_repo_starter {
        text.push_str(
            "> No memory is scoped to THIS repo yet. The memories below are transferable rules \
             from your OTHER repos, matched to this file — starter suggestions, not this repo's \
             own judgment. Run `difflore import-reviews` to capture this repo's memory.\n\n",
        );
    }
    let mut injected = 0usize;
    let mut skill_ids: Vec<String> = Vec::with_capacity(scored.len());
    let max_score_hot = scored
        .iter()
        .map(|r| r.score)
        .fold(f64::NEG_INFINITY, f64::max);
    for rule in &scored {
        let rel = if max_score_hot > 0.0 {
            rule.score / max_score_hot
        } else {
            0.0
        };
        // Shared rule rendering; the hook only changes example labels
        // and the memory number. The why segment (when arbitration metadata
        // exists — cross-repo starter rules carry none) is part of the block
        // text, so the budget gate below accounts for its ~5–10 tokens.
        let why = why_map.get(&rule.skill_id).map(RuleRankingWhy::compact);
        let rule_text = render_rule_block(&RuleBlockArgs {
            position: injected + 1,
            rel,
            rule,
            trust_evidence: &trust_evidence,
            examples: examples_map.get(&rule.skill_id),
            example_bad_label: "- Bad:",
            example_good_label: "- Good:",
            why: why.as_deref(),
        });

        // First rule is always emitted regardless of budget — even an
        // oversized single rule is a stronger signal than zero rules.
        // Subsequent rules get gated.
        let projected_tokens = estimate_tokens(&text) + estimate_tokens(&rule_text);
        if injected > 0 && projected_tokens > HOOK_INJECTION_TOKEN_BUDGET {
            break;
        }
        text.push_str(&rule_text);
        skill_ids.push(rule.skill_id.clone());
        injected += 1;
    }

    let n = injected;
    let applies_to = if is_bash_error_path {
        "current failure"
    } else {
        "current change"
    };
    text.push_str(&counterfactual_citation_instruction(
        n, hook_label, applies_to,
    ));
    // Standing wrap-up nudge: the per-edit citation above is silent unless the
    // assistant chooses to cite. This guarantees the *instruction* to surface
    // DiffLore's value rides the same (already-non-noisy) injection channel, so
    // the user reliably learns a memory helped at task end without us emitting a
    // lifecycle `systemMessage`. Fires only when rules actually applied.
    text.push_str(
        "\n> When you finish this task, add one quiet difflore recap line only when \
         `difflore status` shows accepted edits captured for this task. \
         Do not prefix it with `session-recap:`; if you use a label, \
         write lowercase `difflore:`. Do not mention top memory, recall counts, \
         ready-for-agent counts, or zero/negative metrics. Skip it if nothing here applied.",
    );

    // Mirror the MCP path's telemetry emission for hook recalls.
    emit_trajectory_step(&TrajectoryStep::McpResponseSize {
        tool: hook_tool.to_owned(),
        total_tokens: estimate_tokens(&text),
        rules_injected: n,
    });
    let origin_step = rule_hits_by_origin(db, &skill_ids).await;
    emit_trajectory_step(&origin_step);
    let strict_match_count =
        tools::evidence::strict_file_match_count_for_ids(&meta_map, &skill_ids, target_file);
    // Record the non-empty serve and enqueue the matching event.
    let served_event = serve_and_record(
        db,
        RuleServe {
            tool: hook_tool,
            session_id,
            event_session_id: session_id.unwrap_or("hook"),
            repo_full_name: repo_scopes.first().map(String::as_str),
            target_file,
            query: &query,
            rule_ids: &skill_ids,
            top_k: i64::try_from(hook_top_k).unwrap_or(i64::MAX),
            strict_match_count,
            estimated_tokens: estimate_tokens(&text) as i64,
        },
        hook_serve_record_err_prefix(),
    )
    .await;
    let _ = crate::cloud::observations::enqueue_default(served_event).await;
    mark("emit_telemetry");

    let _ = crate::cloud::observations::enqueue_default(
        crate::cloud::observations::ObservationEvent::RuleFired {
            rule_ids: skill_ids.clone(),
            file_path: target_file.map(ToOwned::to_owned),
            intent: Some(intent.to_owned()),
            session_id: session_id.unwrap_or("hook").to_owned(),
            fired_at: chrono::Utc::now(),
        },
    )
    .await;

    if is_post_edit_path && !ext_key.is_empty() {
        short_circuit_cache.record(&ext_key, false);
    }

    Ok(HookRuleContext {
        rendered: text,
        rules_injected: n,
        rule_ids: skill_ids,
        drop_reason: None,
    })
}

fn counterfactual_citation_instruction(n: usize, hook_label: &str, applies_to: &str) -> String {
    format!(
        "\n> DiffLore surfaced {} team memor{} via {hook_label} hook as silent context. \
         Cite a memory only if your {applies_to} would be materially different without it; \
         include its number AND the `learned from <repo>` source if the header shows one — \
         e.g. \"applying Memory 2: Don't strip null from coalesce (learned from acme/widgets)\". \
         Otherwise ignore — do not narrate or list memories that do not apply.",
        n,
        if n == 1 { "y" } else { "ies" },
    )
}

use super::serve_render::{RuleBlockArgs, RuleServe, render_rule_block, serve_and_record};

fn hook_serve_record_err_prefix() -> Option<&'static str> {
    crate::infra::env::debug_telemetry().then_some(HOOK_SERVE_RECORD_ERR_PREFIX)
}

fn hook_auto_injection_allowed(
    row: &tools::evidence::SkillDetailRow,
    strict_skill_ids: &std::collections::HashSet<String>,
) -> bool {
    // A mined PR-review rule without a file-pattern hit is often a workflow or
    // meta-review note. Keep it discoverable through explicit search/get_rules,
    // but do not silently steer code generation with it on post-edit hooks.
    (row.origin != "pr_review" || strict_skill_ids.contains(&row.id))
        && tools::evidence::kind_gate_allows_silent_injection(row, strict_skill_ids)
}
use super::{
    McpState, emit_trajectory_step, estimate_tokens, handle_message, jsonrpc_error,
    rule_hits_by_origin, tools,
};

fn repo_detection_cache() -> &'static Mutex<HashMap<PathBuf, Vec<String>>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, Vec<String>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn configured_gitlab_hosts_cache() -> &'static Mutex<Vec<String>> {
    static CACHE: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(Vec::new()))
}

fn normalize_gitlab_host_cache(hosts: Vec<String>) -> Vec<String> {
    let mut normalized = hosts
        .into_iter()
        .filter_map(|host| crate::ingest::gitlab::auth::normalize_gitlab_host(&host).ok())
        .collect::<Vec<_>>();
    normalized.sort_unstable();
    normalized.dedup();
    normalized
}

fn set_configured_gitlab_hosts_for_remote_detection(hosts: Vec<String>) {
    let hosts = normalize_gitlab_host_cache(hosts);
    let mut guard = configured_gitlab_hosts_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if *guard == hosts {
        return;
    }
    *guard = hosts;
    repo_detection_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
}

pub(crate) async fn refresh_configured_gitlab_hosts_for_remote_detection() {
    set_configured_gitlab_hosts_for_remote_detection(
        crate::ingest::gitlab::auth::configured_hosts().await,
    );
}

#[cfg(test)]
pub(crate) fn set_configured_gitlab_hosts_for_remote_detection_for_test(hosts: Vec<String>) {
    set_configured_gitlab_hosts_for_remote_detection(hosts);
}

/// Drop every cached remote-detection result. Tests that drive the real
/// cwd-based detection path need this so a previously-cached (and possibly
/// empty) entry for the test's working directory cannot mask the production
/// `refresh` + git-remote-parse chain under test.
#[cfg(test)]
pub(crate) fn clear_repo_detection_cache_for_test() {
    repo_detection_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
}

pub(crate) fn detect_git_remote_owner_repos() -> Vec<String> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    {
        let guard = repo_detection_cache()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(repos) = guard.get(&cwd) {
            if !repos.is_empty() {
                return repos.clone();
            }
        }
    }

    let repos = detect_git_remote_owner_repos_uncached();
    let mut guard = repo_detection_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard.insert(cwd, repos.clone());
    repos
}

#[cfg(test)]
pub(crate) fn set_detected_repos_for_current_dir_for_test(repos: Vec<String>) {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut guard = repo_detection_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard.insert(cwd, repos);
}

/// Parse supported hosted repo scopes from `origin` then `upstream` remotes,
/// dropping duplicates while preserving origin-first order. Empty when no git
/// repo or no supported hosted git remotes. Keeps MCP recall scoped to the
/// project.
fn detect_git_remote_owner_repos_uncached() -> Vec<String> {
    let configured_gitlab_hosts = configured_gitlab_hosts_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let mut repos = Vec::new();
    for remote in ["origin", "upstream"] {
        let output = match crate::infra::git::git_command(".")
            .args(["remote", "get-url", remote])
            .output()
        {
            Ok(output) if output.status.success() => output,
            _ => continue,
        };
        let url = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let Some(repo) = parse_github_owner_repo_with_gitlab_hosts(&url, &configured_gitlab_hosts)
        else {
            continue;
        };
        if !repos.iter().any(|existing| existing == &repo) {
            repos.push(repo);
        }
    }
    repos
}

/// `Some(scope)` for supported hosted SSH/HTTPS remotes (GitHub owner/repo or
/// GitLab host/namespace path, with or without `.git`); `None` otherwise.
#[cfg(test)]
pub(crate) fn parse_github_owner_repo(url: &str) -> Option<String> {
    let configured_gitlab_hosts = configured_gitlab_hosts_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    parse_github_owner_repo_with_gitlab_hosts(url, &configured_gitlab_hosts)
}

fn parse_github_owner_repo_with_gitlab_hosts(
    url: &str,
    configured_gitlab_hosts: &[String],
) -> Option<String> {
    crate::infra::git::parse_repo_remote_url_with_gitlab_hosts(url, configured_gitlab_hosts)
}

#[cfg(test)]
mod tests {
    use crate::context::EmbeddingDiagnostics;

    use super::{counterfactual_citation_instruction, hook_embedding_health_header};

    fn diag(
        degraded: bool,
        vector_lane_available: bool,
        reason: Option<&str>,
    ) -> EmbeddingDiagnostics {
        EmbeddingDiagnostics {
            active_profile: "sha1:local:128".to_owned(),
            index_profile: Some("cloud:managed".to_owned()),
            profile_match: false,
            degraded,
            degraded_reason: reason.map(str::to_owned),
            vector_lane_available,
        }
    }

    #[test]
    fn hook_header_surfaces_embedding_degradation_to_agent_text() {
        let rendered = hook_embedding_health_header(&diag(true, false, Some("provider_fallback")));
        assert!(
            rendered.contains("embeddingDegraded=true"),
            "hook header must surface degraded state: {rendered}"
        );
        assert!(
            rendered.contains("vectorLaneAvailable=false"),
            "hook header must surface vector lane availability: {rendered}"
        );
        assert!(
            rendered.contains("provider_fallback"),
            "hook header must preserve stable reason token: {rendered}"
        );
    }

    #[test]
    fn hook_header_stays_quiet_for_healthy_embedding_lane() {
        let rendered = hook_embedding_health_header(&EmbeddingDiagnostics {
            active_profile: "sha1:local:128".to_owned(),
            index_profile: Some("sha1:local:128".to_owned()),
            profile_match: true,
            degraded: false,
            degraded_reason: None,
            vector_lane_available: true,
        });
        assert!(
            rendered.is_empty(),
            "healthy lane should not spend hook tokens"
        );
    }

    #[test]
    fn citation_instruction_uses_counterfactual_threshold() {
        let rendered = counterfactual_citation_instruction(2, "post-tool", "current change");

        assert!(rendered.contains("would be materially different without it"));
        assert!(rendered.contains("Otherwise ignore"));
        assert!(!rendered.contains("actually applies"));
    }
}
