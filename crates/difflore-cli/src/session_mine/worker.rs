//! Session-mine top-level worker.
//!
//! Composes [`super::extract`], [`super::gate`] and the cloud-outbox
//! enqueue path. The hook dispatcher spawns [`run_worker_detached`]
//! when the trigger fires.
//!
//! Failure policy: every error is swallowed (logged to stderr at
//! most). Session-mine is an out-of-band evidence channel and must
//! never block the user's hook output or surface a panic into the
//! agent session.

use difflore_core::cloud::outbox::{OutboxQueue, kind as outbox_kind};
use difflore_core::cloud::session_mined::{
    SessionMinedCandidate, SessionMinedCandidateArgs, SessionMinedLocalTriageStatus,
};
use difflore_core::infra::db::current_project_root;
use difflore_core::infra::git::RepoScope;
use difflore_core::memory_autopilot::session_mined_candidates_semantically_match;
use difflore_core::memory_inbox::set_candidate_distinct_evidence_count;
use sqlx::Row;

use super::extract::Pair;
use super::gate::{
    ExistingRule, GATE_PROMPT_PREAMBLE, GateArgs, GateDispatchFailureClass, GateError, GateMode,
    GateVerdict,
};
use super::trigger::GateCaptureStatus;

#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

/// Cap on existing rules forwarded to the gate prompt. Bounds the SQL
/// round-trip and cloning cost when a team has thousands of rules.
const MAX_EXISTING_RULES_FOR_GATE: usize = 24;

#[cfg(test)]
static WORKER_GATE_CALLS: AtomicUsize = AtomicUsize::new(0);

#[cfg(test)]
fn reset_worker_gate_call_count() {
    WORKER_GATE_CALLS.store(0, Ordering::SeqCst);
}

#[cfg(test)]
fn worker_gate_call_count() -> usize {
    WORKER_GATE_CALLS.load(Ordering::SeqCst)
}

/// True when `pairs` come from difflore's own gate session rather than a real
/// agent conversation. The gate runs as a read-only `codex exec` child whose
/// transcript is later fed back to the miner; recognising it here is what stops
/// the gate → hook → mine → gate recursion (a runaway that burned ~107M tokens
/// in one ~80-min incident). Matches on the gate prompt's distinctive preamble
/// ([`GATE_PROMPT_PREAMBLE`]), which appears as the first user turn of any gate
/// session — independent of the (gate4agent-dropped) `DIFFLORE_CAPTURE` env, so
/// it holds even when that guard is bypassed.
fn is_self_spawned_gate_session(pairs: &[Pair]) -> bool {
    pairs
        .iter()
        .any(|pair| pair.user_prompt.contains(GATE_PROMPT_PREAMBLE))
}

fn auto_gate_capture_is_paused(cwd: Option<&str>, mode: GateMode) -> bool {
    if matches!(mode, GateMode::ManualLearn) {
        return false;
    }
    match super::trigger::gate_capture_status_for_project(cwd) {
        Ok(GateCaptureStatus::Paused {
            reason,
            retry_after_ms,
            ..
        }) => {
            if difflore_core::infra::env::debug_telemetry() {
                eprintln!(
                    "[difflore.session_mine] skipping {} gate during capture pause (retry in {}ms): {}",
                    mode.label(),
                    retry_after_ms,
                    reason
                );
            }
            true
        }
        Ok(GateCaptureStatus::Ready) | Err(_) => false,
    }
}

/// Per-rule body snippet cap in the gate's "existing rules" digest.
const EXISTING_RULE_BODY_SNIPPET_CHARS: usize = 280;

/// Spawn the worker as a detached tokio task, returning immediately.
///
/// `client_name` is the platform string the hook reports
/// (`"claude-code"`, `"cursor"`, …), used for extract dispatch.
/// `cwd` derives `source_repo` via the git remote; `None` falls back
/// to `current_project_root()`.
pub fn run_worker_detached(
    client_name: String,
    transcript_path: Option<String>,
    session_id: Option<String>,
    cwd: Option<String>,
    schedule_autopilot_after_mine: bool,
) {
    // Prefer the existing tokio runtime (hook dispatcher is
    // `#[tokio::main]`); outside a runtime (e.g. test harness),
    // `spawn` would panic, so fall back to a dedicated thread.
    let task = async move {
        if let Err(e) = run_worker_inner(
            &client_name,
            transcript_path.as_deref(),
            session_id.as_deref(),
            cwd.as_deref(),
            schedule_autopilot_after_mine,
        )
        .await
        {
            if difflore_core::infra::env::debug_telemetry() {
                eprintln!("[difflore.session_mine] worker failed: {e}");
            }
        }
    };
    if tokio::runtime::Handle::try_current().is_ok() {
        tokio::spawn(task);
    } else {
        // No runtime: run on a temporary one so callers get the same
        // observable behaviour without panicking on `spawn`.
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    if difflore_core::infra::env::debug_telemetry() {
                        eprintln!("[difflore.session_mine] cannot build fallback runtime: {e}");
                    }
                    return;
                }
            };
            rt.block_on(task);
        });
    }
}

pub fn run_targeted_pairs_detached(
    client_name: String,
    pairs: Vec<Pair>,
    session_id: Option<String>,
    cwd: Option<String>,
    mode: GateMode,
) {
    let task = async move {
        if let Err(e) = run_pairs_inner(
            &client_name,
            pairs,
            session_id.as_deref(),
            cwd.as_deref(),
            false,
            mode,
        )
        .await
        {
            if difflore_core::infra::env::debug_telemetry() {
                eprintln!("[difflore.session_mine] targeted worker failed: {e}");
            }
        }
    };
    if tokio::runtime::Handle::try_current().is_ok() {
        tokio::spawn(task);
    } else {
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    if difflore_core::infra::env::debug_telemetry() {
                        eprintln!("[difflore.session_mine] cannot build fallback runtime: {e}");
                    }
                    return;
                }
            };
            rt.block_on(task);
        });
    }
}

pub async fn run_targeted_pairs_once(
    client_name: &str,
    pairs: Vec<Pair>,
    session_id: Option<&str>,
    cwd: Option<&str>,
    mode: GateMode,
) -> Result<(), String> {
    run_pairs_inner(client_name, pairs, session_id, cwd, false, mode).await
}

/// Body of the worker, separated from the spawn helper so tests can
/// exercise it with a controlled environment.
async fn run_worker_inner(
    client_name: &str,
    transcript_path: Option<&str>,
    session_id: Option<&str>,
    cwd: Option<&str>,
    schedule_autopilot_after_mine: bool,
) -> Result<(), String> {
    let pairs = extract_pairs(client_name, transcript_path);
    let recipe_pairs = pairs.clone();
    run_pairs_inner(
        client_name,
        pairs,
        session_id,
        cwd,
        schedule_autopilot_after_mine,
        GateMode::Session,
    )
    .await?;
    run_pairs_inner(
        client_name,
        recipe_pairs,
        session_id,
        cwd,
        schedule_autopilot_after_mine,
        GateMode::Recipe,
    )
    .await
}

async fn run_pairs_inner(
    client_name: &str,
    pairs: Vec<Pair>,
    session_id: Option<&str>,
    cwd: Option<&str>,
    schedule_autopilot_after_mine: bool,
    mode: GateMode,
) -> Result<(), String> {
    if pairs.is_empty() {
        // No conversational data to mine.
        return Ok(());
    }

    // Recursion guard. Each gate runs as a read-only `codex exec` child, and
    // that child fires the same SessionEnd / Stop hooks we register globally —
    // so the gate session's own transcript flows back here to be mined. Mining
    // it would spawn another gate, whose exit fires the hook again: an
    // unbounded gate → hook → mine → gate loop. We defend at the consumption
    // point so the break holds regardless of env/transport propagation: never
    // mine difflore's own gate sessions.
    if is_self_spawned_gate_session(&pairs) {
        if difflore_core::infra::env::debug_telemetry() {
            eprintln!(
                "[difflore.session_mine] skipping self-spawned gate session (recursion guard)"
            );
        }
        return Ok(());
    }

    if auto_gate_capture_is_paused(cwd, mode) {
        return Ok(());
    }

    let Some(source_repo) = resolve_source_repo(cwd).await else {
        // Project Scope Invariant: never enqueue a scopeless
        // candidate. We no-op rather than fabricate a `source_repo`.
        return Ok(());
    };
    let source_repo = source_repo.into_string();

    let session_id = session_id.unwrap_or("").trim().to_owned();
    if session_id.is_empty() {
        return Ok(());
    }

    // One DB handle for both reading existing rules and enqueuing on
    // Keep. Best-effort: log and drop the session on failure.
    let db = match difflore_core::infra::db::init_db().await {
        Ok(p) => p,
        Err(e) => {
            if difflore_core::infra::env::debug_telemetry() {
                eprintln!("[difflore.session_mine] DB open failed: {e}");
            }
            return Ok(());
        }
    };

    let existing_rules = load_existing_rules(&db, &source_repo).await;
    let ts_ms = chrono::Utc::now().timestamp_millis();
    let gate_model = format!("{client_name}:gate:{}", mode.label());
    let args = GateArgs {
        session_id: &session_id,
        source_repo: &source_repo,
        pairs: &pairs,
        existing_rules: &existing_rules,
        gate_model: &gate_model,
        client_name,
        ts_ms,
    };
    let verdict = match run_gate_for_worker(args, mode).await {
        Ok(v) => {
            clear_gate_capture_stall(cwd);
            v
        }
        Err(e) => {
            if difflore_core::infra::env::debug_telemetry() {
                eprintln!("[difflore.session_mine] gate failed: {e}");
            }
            handle_gate_error(cwd, &e);
            return Ok(());
        }
    };

    match verdict {
        GateVerdict::Keep { candidate } => match enqueue_candidate(&db, &candidate).await {
            Ok(_) => {
                mark_and_maybe_schedule_autopilot(&db, schedule_autopilot_after_mine).await;
                Ok(())
            }
            Err(e) => {
                if difflore_core::infra::env::debug_telemetry() {
                    eprintln!("[difflore.session_mine] enqueue failed: {e}");
                }
                Ok(())
            }
        },
        GateVerdict::Merge {
            gate_model,
            rule_id,
            title,
            updated_body,
            file_patterns,
        } => {
            let candidate = match merge_candidate_from_verdict(&MergeCandidateInput {
                session_id: &session_id,
                ts_ms,
                source_repo: &source_repo,
                gate_model: &gate_model,
                existing_rules: &existing_rules,
                rule_id: &rule_id,
                gate_title: title.as_deref(),
                updated_body: &updated_body,
                mined_file_patterns: &file_patterns,
            }) {
                Ok(candidate) => candidate,
                Err(e) => {
                    if difflore_core::infra::env::debug_telemetry() {
                        eprintln!("[difflore.session_mine] MERGE candidate build failed: {e}");
                    }
                    return Ok(());
                }
            };
            match enqueue_candidate(&db, &candidate).await {
                Ok(_) => {
                    mark_and_maybe_schedule_autopilot(&db, schedule_autopilot_after_mine).await;
                    Ok(())
                }
                Err(e) => {
                    if difflore_core::infra::env::debug_telemetry() {
                        eprintln!("[difflore.session_mine] enqueue failed: {e}");
                    }
                    Ok(())
                }
            }
        }
        GateVerdict::Skip { reason } => {
            if difflore_core::infra::env::debug_telemetry() {
                eprintln!("[difflore.session_mine] gate SKIP: {reason}");
            }
            Ok(())
        }
    }
}

#[cfg(not(test))]
async fn run_gate_for_worker(args: GateArgs<'_>, mode: GateMode) -> Result<GateVerdict, GateError> {
    super::gate::run_gate_with_mode(args, mode).await
}

#[cfg(test)]
async fn run_gate_for_worker(
    _args: GateArgs<'_>,
    _mode: GateMode,
) -> Result<GateVerdict, GateError> {
    WORKER_GATE_CALLS.fetch_add(1, Ordering::SeqCst);
    Ok(GateVerdict::Skip {
        reason: "test gate stub".to_owned(),
    })
}

fn handle_gate_error(cwd: Option<&str>, error: &GateError) {
    match error.dispatch_failure_class() {
        Some(GateDispatchFailureClass::Persistent) => {
            let _ = super::trigger::record_gate_capture_stall_for_project(cwd, &error.to_string());
        }
        Some(GateDispatchFailureClass::Transient) | None => {
            clear_gate_capture_stall(cwd);
        }
    }
}

fn clear_gate_capture_stall(cwd: Option<&str>) {
    let _ = super::trigger::clear_gate_capture_stall_for_project(cwd);
}

async fn mark_and_maybe_schedule_autopilot(
    db: &difflore_core::SqlitePool,
    schedule_autopilot: bool,
) {
    crate::commands::memory::mark_memory_autopilot_dirty_best_effort(db, "session_mined_candidate")
        .await;
    if schedule_autopilot {
        crate::commands::memory::schedule_memory_autopilot_best_effort(
            db,
            "session_end",
            difflore_core::memory_autopilot_schedule::SESSION_END_AUTOPILOT_COOLDOWN_SECS,
        )
        .await;
    }
}

/// Read active rules and project them into the `ExistingRule` shape
/// the gate expects. Rules with `source_repo` or a `repo_owner`/`repo_name`
/// pair are kept only when they match `source_repo`; rules without that
/// metadata are included permissively. Failures collapse to an empty list,
/// which the gate treats as valid input.
async fn load_existing_rules(db: &sqlx::SqlitePool, source_repo: &str) -> Vec<ExistingRule> {
    let Ok(rows) = sqlx::query(
        "SELECT COALESCE(NULLIF(cloud_id, ''), id) AS rule_id,
                id, name, description, repo_owner, repo_name, source_repo, file_patterns \
         FROM skills WHERE status = 'active' ORDER BY installed_at DESC",
    )
    .fetch_all(db)
    .await
    else {
        return Vec::new();
    };
    let scope = source_repo.to_ascii_lowercase();
    rows.into_iter()
        .filter_map(|row| {
            let rule_id: String = row.try_get("rule_id").ok()?;
            if !looks_like_cloud_rule_id(&rule_id) {
                // Session-mined candidates are approved in the cloud, where a
                // MERGE target must be a published cloud rule UUID. Local
                // rules published from this device keep their local `skills.id`
                // but carry the cloud UUID in `skills.cloud_id`; local-only
                // rows have neither and must not be exposed as merge targets.
                return None;
            }
            let title: String = row.try_get("name").ok()?;
            let description: String = row.try_get("description").unwrap_or_default();
            let repo_owner: Option<String> = row.try_get("repo_owner").ok().flatten();
            let repo_name: Option<String> = row.try_get("repo_name").ok().flatten();
            let source_repo_col: Option<String> = row.try_get("source_repo").ok().flatten();

            if !rule_matches_source_repo(
                repo_owner.as_deref(),
                repo_name.as_deref(),
                source_repo_col.as_deref(),
                &scope,
            ) {
                return None;
            }

            let file_patterns_raw: Option<String> = row.try_get("file_patterns").ok().flatten();
            Some(ExistingRule {
                rule_id,
                title,
                body_snippet: description
                    .chars()
                    .take(EXISTING_RULE_BODY_SNIPPET_CHARS)
                    .collect(),
                file_patterns: parse_file_patterns(file_patterns_raw.as_deref()),
                source_repo: clean_optional(source_repo_col),
            })
        })
        .take(MAX_EXISTING_RULES_FOR_GATE)
        .collect()
}

struct MergeCandidateInput<'a> {
    session_id: &'a str,
    ts_ms: i64,
    source_repo: &'a str,
    gate_model: &'a str,
    existing_rules: &'a [ExistingRule],
    rule_id: &'a str,
    gate_title: Option<&'a str>,
    updated_body: &'a str,
    mined_file_patterns: &'a [String],
}

fn merge_candidate_from_verdict(
    input: &MergeCandidateInput<'_>,
) -> Result<SessionMinedCandidate, String> {
    let target = input
        .existing_rules
        .iter()
        .find(|rule| rule.rule_id == input.rule_id);
    let title = target
        .and_then(|rule| non_empty_owned(&rule.title))
        .or_else(|| input.gate_title.and_then(non_empty_owned))
        .ok_or_else(|| format!("MERGE:{} missing title", input.rule_id))?;
    // Inherit the target rule's canonical scope when present, else fall back to
    // the worker's detected scope. Both branches resolve to a `RepoScope`, so
    // the candidate's `source_repo` write stays funnelled through the newtype.
    let candidate_source_repo = target
        .and_then(|rule| rule.source_repo.as_deref())
        .and_then(RepoScope::canonical)
        .or_else(|| RepoScope::canonical(input.source_repo))
        .ok_or_else(|| {
            format!(
                "MERGE:{} non-canonical source_repo: {}",
                input.rule_id, input.source_repo
            )
        })?;
    let file_patterns = merge_file_patterns(input.mined_file_patterns, target);

    SessionMinedCandidate::try_new(SessionMinedCandidateArgs {
        session_id: input.session_id.to_owned(),
        ts_ms: input.ts_ms,
        source_repo: candidate_source_repo,
        title,
        body: input.updated_body.to_owned(),
        file_patterns,
        gate_model: input.gate_model.to_owned(),
        gate_verdict: format!("MERGE:{}", input.rule_id),
    })
    .map_err(|e| format!("MERGE:{} invalid candidate: {e}", input.rule_id))
}

fn merge_file_patterns(
    mined_file_patterns: &[String],
    target: Option<&ExistingRule>,
) -> Vec<String> {
    let mut out = Vec::new();
    push_unique_patterns(&mut out, mined_file_patterns.iter().map(String::as_str));
    if let Some(rule) = target {
        push_unique_patterns(&mut out, rule.file_patterns.iter().map(String::as_str));
    }
    out
}

fn push_unique_patterns<'a>(out: &mut Vec<String>, patterns: impl Iterator<Item = &'a str>) {
    for pattern in patterns {
        let pattern = pattern.trim();
        if pattern.is_empty() || out.iter().any(|existing| existing == pattern) {
            continue;
        }
        out.push(pattern.to_owned());
    }
}

fn rule_matches_source_repo(
    repo_owner: Option<&str>,
    repo_name: Option<&str>,
    source_repo: Option<&str>,
    scope_lc: &str,
) -> bool {
    let repo_pair_matches = match (repo_owner, repo_name) {
        (Some(owner), Some(name)) => format!("{owner}/{name}").to_ascii_lowercase() == scope_lc,
        _ => false,
    };
    let has_repo_pair = repo_owner.is_some() && repo_name.is_some();
    let source_repo_matches = source_repo
        .map(str::trim)
        .filter(|repo| !repo.is_empty())
        .is_some_and(|repo| repo.to_ascii_lowercase() == scope_lc);
    let has_source_repo = source_repo
        .map(str::trim)
        .is_some_and(|repo| !repo.is_empty());

    if has_repo_pair || has_source_repo {
        repo_pair_matches || source_repo_matches
    } else {
        true
    }
}

fn parse_file_patterns(raw: Option<&str>) -> Vec<String> {
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<String>>(raw)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|pattern| non_empty_owned(&pattern))
        .collect()
}

fn clean_optional(value: Option<String>) -> Option<String> {
    value.and_then(|value| non_empty_owned(&value))
}

fn looks_like_cloud_rule_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    for (idx, byte) in bytes.iter().enumerate() {
        if matches!(idx, 8 | 13 | 18 | 23) {
            if *byte != b'-' {
                return false;
            }
            continue;
        }
        if !byte.is_ascii_hexdigit() {
            return false;
        }
    }
    true
}

fn non_empty_owned(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_owned())
    }
}

fn extract_pairs(client_name: &str, transcript_path: Option<&str>) -> Vec<Pair> {
    let platform = super::extract::Platform::from_client_name(client_name);
    let args = super::extract::ExtractArgs {
        platform,
        transcript_path,
        session_id: None,
        max_pairs: 10,
    };
    super::extract::extract_recent_session_pairs(args).unwrap_or_default()
}

/// Resolve `source_repo` per the Project Scope Invariant.
///
/// Detection must use the same configured GitLab hosts as recall. If no
/// canonical repo scope can be detected, fail closed instead of fabricating a
/// basename that can never match the eventual recall scope.
async fn resolve_source_repo(cwd: Option<&str>) -> Option<RepoScope> {
    let configured_gitlab_hosts = difflore_core::ingest::gitlab::auth::configured_hosts().await;
    resolve_source_repo_with_gitlab_hosts(cwd, &configured_gitlab_hosts)
}

fn resolve_source_repo_with_gitlab_hosts(
    cwd: Option<&str>,
    configured_gitlab_hosts: &[String],
) -> Option<RepoScope> {
    let path = cwd.map_or_else(current_project_root, std::path::PathBuf::from);
    let path_str = path.to_string_lossy().to_string();

    difflore_core::infra::git::detect_repo_full_names_with_gitlab_hosts(
        &path_str,
        configured_gitlab_hosts,
    )
    .into_iter()
    .find_map(|repo| RepoScope::canonical(&repo))
}

/// Serialize the candidate and append it to the cloud outbox under
/// `kind = "session_mined_candidate"`.
pub async fn enqueue_candidate(
    db: &sqlx::SqlitePool,
    candidate: &SessionMinedCandidate,
) -> Result<i64, String> {
    candidate
        .validate()
        .map_err(|e| format!("session-mine: invalid candidate: {e}"))?;
    if let Some(existing_row_id) = fold_existing_session_candidate(db, candidate).await? {
        return Ok(existing_row_id);
    }
    let payload =
        serde_json::to_string(candidate).map_err(|e| format!("session-mine: serialize: {e}"))?;
    let queue = OutboxQueue::new(db.clone());
    queue
        .enqueue(outbox_kind::SESSION_MINED_CANDIDATE, &payload)
        .await
        .map_err(|e| format!("session-mine: enqueue: {e}"))
}

async fn fold_existing_session_candidate(
    db: &sqlx::SqlitePool,
    candidate: &SessionMinedCandidate,
) -> Result<Option<i64>, String> {
    let rows = sqlx::query(
        "SELECT id, payload_json \
         FROM cloud_outbox \
         WHERE kind = ?1 \
         ORDER BY id DESC",
    )
    .bind(outbox_kind::SESSION_MINED_CANDIDATE)
    .fetch_all(db)
    .await
    .map_err(|e| format!("session-mine: dedup scan: {e}"))?;

    for row in rows {
        let row_id: i64 = row
            .try_get("id")
            .map_err(|e| format!("session-mine: dedup row id: {e}"))?;
        let payload_json: String = row
            .try_get("payload_json")
            .map_err(|e| format!("session-mine: dedup payload: {e}"))?;
        let Ok(existing) = serde_json::from_str::<SessionMinedCandidate>(&payload_json) else {
            continue;
        };
        if !session_mined_candidates_semantically_match(&existing, candidate) {
            continue;
        }

        let target_hash = match existing.local_triage.as_ref().map(|triage| &triage.status) {
            Some(SessionMinedLocalTriageStatus::DroppedLowSignal) => return Ok(Some(row_id)),
            Some(
                SessionMinedLocalTriageStatus::SupersededBy
                | SessionMinedLocalTriageStatus::ClusteredInto,
            ) => {
                let Some(reference) = existing
                    .local_triage
                    .as_ref()
                    .and_then(|triage| triage.reference.as_deref())
                else {
                    return Ok(Some(row_id));
                };
                reference
            }
            Some(SessionMinedLocalTriageStatus::Unknown) | None => existing.content_hash.as_str(),
        };
        let Some(current_count) =
            current_session_evidence_count(db, target_hash, &candidate.source_repo).await?
        else {
            return Ok(Some(row_id));
        };
        let next_count = if same_evidence_day(&existing, candidate) {
            current_count
        } else {
            current_count.saturating_add(1)
        };
        set_candidate_distinct_evidence_count(db, target_hash, next_count)
            .await
            .map_err(|e| format!("session-mine: evidence bump: {e}"))?;
        return Ok(Some(row_id));
    }

    Ok(None)
}

async fn current_session_evidence_count(
    db: &sqlx::SqlitePool,
    content_hash: &str,
    source_repo: &str,
) -> Result<Option<usize>, String> {
    let rows = sqlx::query(
        "SELECT payload_json \
         FROM cloud_outbox \
         WHERE kind = ?1",
    )
    .bind(outbox_kind::SESSION_MINED_CANDIDATE)
    .fetch_all(db)
    .await
    .map_err(|e| format!("session-mine: evidence scan: {e}"))?;
    for row in rows {
        let payload_json: String = row
            .try_get("payload_json")
            .map_err(|e| format!("session-mine: evidence payload: {e}"))?;
        let Ok(candidate) = serde_json::from_str::<SessionMinedCandidate>(&payload_json) else {
            continue;
        };
        if candidate.content_hash == content_hash
            && candidate.source_repo.eq_ignore_ascii_case(source_repo)
        {
            return Ok(Some(existing_evidence_count(&candidate)));
        }
    }
    Ok(None)
}

fn existing_evidence_count(candidate: &SessionMinedCandidate) -> usize {
    candidate
        .local_evidence
        .as_ref()
        .map_or(1, |evidence| evidence.distinct_evidence_count)
        .max(1)
}

fn same_evidence_day(existing: &SessionMinedCandidate, incoming: &SessionMinedCandidate) -> bool {
    existing.session_id == incoming.session_id
        && natural_day(existing.ts_ms) == natural_day(incoming.ts_ms)
}

const fn natural_day(ts_ms: i64) -> i64 {
    ts_ms.div_euclid(86_400_000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::support::test_home::pin_test_home;
    use difflore_core::cloud::session_mined::{SessionMinedCandidateArgs, SessionMinedLocalTriage};
    use tempfile::TempDir;

    async fn migrated_pool() -> sqlx::SqlitePool {
        let db = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect(":memory:")
            .await
            .expect("memory db");
        difflore_core::infra::db::run_migrations(&db)
            .await
            .expect("migrate");
        db
    }

    fn pair(user_prompt: &str) -> Pair {
        Pair {
            user_prompt: user_prompt.to_owned(),
            assistant_text: "ok".to_owned(),
        }
    }

    #[test]
    fn recursion_guard_flags_difflore_gate_sessions() {
        // A gate session's first user turn is the gate prompt itself, so the
        // miner must recognise it and refuse to mine (else gate → hook → mine
        // → gate recurses and burns tokens). Built from the same preamble
        // const build_prompt emits, so the guard can never drift from it.
        let gate = pair(&format!(
            "{GATE_PROMPT_PREAMBLE} Decide whether the following short session ..."
        ));
        assert!(is_self_spawned_gate_session(&[gate]));
    }

    #[test]
    fn recursion_guard_allows_real_sessions() {
        let real = pair("Refactor the auth middleware to return 401 on expired tokens.");
        assert!(!is_self_spawned_gate_session(&[real]));
        // Empty transcript is not a gate session either.
        assert!(!is_self_spawned_gate_session(&[]));
    }

    #[tokio::test]
    async fn stalled_project_blocks_correction_and_recipe_gate_dispatch() {
        pin_test_home();
        reset_worker_gate_call_count();
        let project = TempDir::new().expect("project tempdir");
        init_git_repo_with_origin(project.path(), "https://github.com/acme/app.git");
        let cwd = project.path().to_str().expect("utf8 temp path");
        super::super::trigger::record_gate_capture_stall_for_project(
            Some(cwd),
            "codex unauthorized",
        )
        .expect("record stall");

        assert!(auto_gate_capture_is_paused(Some(cwd), GateMode::Session));
        assert!(auto_gate_capture_is_paused(Some(cwd), GateMode::Correction));
        assert!(auto_gate_capture_is_paused(Some(cwd), GateMode::Recipe));
        assert!(
            !auto_gate_capture_is_paused(Some(cwd), GateMode::ManualLearn),
            "manual /learn remains user-requested and should not be paused by auto capture backoff"
        );

        run_targeted_pairs_once(
            "codex",
            vec![Pair {
                user_prompt: "No, keep parser errors typed for this repo.".to_owned(),
                assistant_text: "I changed parseConfig() to return strings.".to_owned(),
            }],
            Some("sess-correction"),
            Some(cwd),
            GateMode::Correction,
        )
        .await
        .expect("stalled targeted correction should no-op cleanly");
        assert_eq!(
            worker_gate_call_count(),
            0,
            "stalled targeted correction must return before invoking the gate"
        );

        super::super::trigger::clear_gate_capture_stall_for_project(Some(cwd))
            .expect("clear stall");
        run_targeted_pairs_once(
            "codex",
            vec![Pair {
                user_prompt: "No, keep parser errors typed for this repo.".to_owned(),
                assistant_text: "I changed parseConfig() to return strings.".to_owned(),
            }],
            Some("sess-correction"),
            Some(cwd),
            GateMode::Correction,
        )
        .await
        .expect("unstalled targeted correction should reach the test gate stub");
        assert_eq!(
            worker_gate_call_count(),
            1,
            "ready targeted correction should invoke the gate once"
        );
    }

    fn candidate() -> SessionMinedCandidate {
        session_candidate(
            "sess_w",
            1_714_000_000_000,
            "Reject scopeless rules",
            "Sessions without a resolvable source_repo must drop their candidate \
                   instead of enqueueing a scopeless row.",
            vec!["src/**/*.rs"],
        )
    }

    fn session_candidate(
        session_id: &str,
        ts_ms: i64,
        title: &str,
        body: &str,
        file_patterns: Vec<&str>,
    ) -> SessionMinedCandidate {
        session_candidate_in_repo(
            "owner/repo",
            session_id,
            ts_ms,
            title,
            body,
            file_patterns,
            "KEEP",
        )
    }

    fn session_candidate_in_repo(
        source_repo: &str,
        session_id: &str,
        ts_ms: i64,
        title: &str,
        body: &str,
        file_patterns: Vec<&str>,
        gate_verdict: &str,
    ) -> SessionMinedCandidate {
        SessionMinedCandidate::try_new(SessionMinedCandidateArgs {
            session_id: session_id.to_owned(),
            ts_ms,
            source_repo: RepoScope::canonical(source_repo).expect("canonical scope"),
            title: title.to_owned(),
            body: body.to_owned(),
            file_patterns: file_patterns.into_iter().map(str::to_owned).collect(),
            gate_model: "claude:haiku".to_owned(),
            gate_verdict: gate_verdict.to_owned(),
        })
        .expect("test fixture must be valid")
    }

    async fn insert_raw_session_candidate(
        db: &sqlx::SqlitePool,
        candidate: &SessionMinedCandidate,
    ) -> i64 {
        let payload = serde_json::to_string(candidate).expect("payload");
        sqlx::query(
            "INSERT INTO cloud_outbox (kind, payload_json, status, created_at) \
             VALUES (?1, ?2, 'pending', ?3)",
        )
        .bind(outbox_kind::SESSION_MINED_CANDIDATE)
        .bind(payload)
        .bind(candidate.ts_ms)
        .execute(db)
        .await
        .expect("insert raw candidate")
        .last_insert_rowid()
    }

    fn existing_rule() -> ExistingRule {
        ExistingRule {
            rule_id: "11111111-1111-4111-8111-111111111111".to_owned(),
            title: "Preserve async cleanup".to_owned(),
            body_snippet: "Existing body".to_owned(),
            file_patterns: vec!["crates/difflore-cli/src/**/*.rs".to_owned()],
            source_repo: Some("upstream/repo".to_owned()),
        }
    }

    #[test]
    fn enqueue_helper_validates_payload_before_touching_the_db() {
        // Lock the validation gate so a refactor cannot let an invalid
        // payload onto the outbox path. (No live SqlitePool needed.)
        let mut bad = candidate();
        bad.requires_human_approval = false;
        let err = bad.validate().unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("requires_human_approval"),
            "draft-flag rejection must surface in the error message: {msg}"
        );
    }

    #[test]
    fn candidate_round_trips_through_json_with_kind_string() {
        // The wire shape is load-bearing for the cloud-side endpoint,
        // so lock the JSON round-trip and the outbox kind string.
        let cand = candidate();
        let payload = serde_json::to_string(&cand).expect("serialize");
        let kind = outbox_kind::SESSION_MINED_CANDIDATE;
        assert_eq!(kind, "session_mined_candidate");

        let decoded: SessionMinedCandidate = serde_json::from_str(&payload).expect("decode");
        assert_eq!(decoded.source_repo, "owner/repo");
        assert!(decoded.requires_human_approval);
        assert_eq!(decoded.origin, "session_mined");
    }

    #[tokio::test]
    async fn enqueue_candidate_folds_semantic_duplicate_and_bumps_evidence() {
        let db = migrated_pool().await;
        let first = session_candidate(
            "sess-a",
            1_714_000_000_000,
            "Tauri dev startup uses npm run tauri dev",
            "Use npm run tauri dev for local desktop development because it starts both Vite \
             and the Tauri shell together instead of launching a raw binary with missing assets.",
            vec!["src-tauri/**/*.rs"],
        );
        let second = session_candidate(
            "sess-b",
            1_714_086_400_000,
            "Tauri dev startup uses npm run tauri dev",
            "Use npm run tauri dev for local desktop development because it starts both Vite \
             and the Tauri shell together instead of launching a raw binary with missing assets. \
             The later session observed the same behavior.",
            vec!["src-tauri/src/**/*.rs"],
        );

        let first_id = enqueue_candidate(&db, &first).await.expect("first enqueue");
        let second_id = enqueue_candidate(&db, &second)
            .await
            .expect("second enqueue");

        assert_eq!(second_id, first_id);
        let row_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM cloud_outbox WHERE kind = 'session_mined_candidate'",
        )
        .fetch_one(&db)
        .await
        .expect("row count");
        assert_eq!(row_count, 1);
        let evidence_count: i64 = sqlx::query_scalar(
            "SELECT CAST(json_extract(payload_json, '$.localEvidence.distinctEvidenceCount') AS INTEGER) \
             FROM cloud_outbox WHERE id = ?1",
        )
        .bind(first_id)
        .fetch_one(&db)
        .await
        .expect("evidence count");
        assert_eq!(evidence_count, 2);
    }

    #[tokio::test]
    async fn enqueue_candidate_dedups_merge_verdicts_too() {
        let db = migrated_pool().await;
        let first = session_candidate(
            "sess-a",
            1_714_000_000_000,
            "Tauri dev startup uses npm run tauri dev",
            "Use npm run tauri dev for local desktop development because it starts both Vite \
             and the Tauri shell together instead of launching a raw binary with missing assets.",
            vec!["src-tauri/**/*.rs"],
        );
        let merge = session_candidate_in_repo(
            "owner/repo",
            "sess-b",
            1_714_086_400_000,
            "Tauri dev startup uses npm run tauri dev",
            "Use npm run tauri dev for local desktop development because it starts both Vite \
             and the Tauri shell together instead of launching a raw binary with missing assets.",
            vec!["src-tauri/src/**/*.rs"],
            "MERGE:rule-123",
        );

        let first_id = enqueue_candidate(&db, &first).await.expect("first enqueue");
        let merge_id = enqueue_candidate(&db, &merge).await.expect("merge enqueue");

        assert_eq!(merge_id, first_id);
        let row_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM cloud_outbox WHERE kind = 'session_mined_candidate'",
        )
        .fetch_one(&db)
        .await
        .expect("row count");
        assert_eq!(row_count, 1);
        let evidence_count: i64 = sqlx::query_scalar(
            "SELECT CAST(json_extract(payload_json, '$.localEvidence.distinctEvidenceCount') AS INTEGER) \
             FROM cloud_outbox WHERE id = ?1",
        )
        .bind(first_id)
        .fetch_one(&db)
        .await
        .expect("evidence count");
        assert_eq!(
            evidence_count, 2,
            "MERGE-verdict duplicates must strengthen the existing canonical evidence"
        );
    }

    #[tokio::test]
    async fn enqueue_candidate_does_not_bump_foreign_repo_canonical_reference() {
        let db = migrated_pool().await;
        let foreign = session_candidate_in_repo(
            "other/repo",
            "sess-foreign",
            1_714_000_000_000,
            "Tauri dev startup uses npm run tauri dev",
            "Use npm run tauri dev for local desktop development because it starts both Vite \
             and the Tauri shell together instead of launching a raw binary with missing assets.",
            vec!["src-tauri/**/*.rs"],
            "KEEP",
        );
        let foreign_id = insert_raw_session_candidate(&db, &foreign).await;
        let mut hidden = session_candidate(
            "sess-hidden",
            1_714_000_000_000,
            "Tauri dev startup uses npm run tauri dev",
            "Use npm run tauri dev for local desktop development because it starts both Vite \
             and the Tauri shell together instead of launching a raw binary with missing assets.",
            vec!["src-tauri/**/*.rs"],
        );
        hidden.local_triage = Some(SessionMinedLocalTriage {
            status: SessionMinedLocalTriageStatus::SupersededBy,
            reason: "foreign reference fixture".to_owned(),
            reference: Some(foreign.content_hash.clone()),
            at: 1_714_000_000_001,
        });
        let hidden_id = insert_raw_session_candidate(&db, &hidden).await;
        let incoming = session_candidate(
            "sess-incoming",
            1_714_086_400_000,
            "Tauri dev startup uses npm run tauri dev",
            "Use npm run tauri dev for local desktop development because it starts both Vite \
             and the Tauri shell together instead of launching a raw binary with missing assets.",
            vec!["src-tauri/src/**/*.rs"],
        );

        let repeated_id = enqueue_candidate(&db, &incoming)
            .await
            .expect("incoming enqueue");

        assert_eq!(repeated_id, hidden_id);
        let foreign_evidence: Option<i64> = sqlx::query_scalar(
            "SELECT CAST(json_extract(payload_json, '$.localEvidence.distinctEvidenceCount') AS INTEGER) \
             FROM cloud_outbox WHERE id = ?1",
        )
        .bind(foreign_id)
        .fetch_one(&db)
        .await
        .expect("foreign evidence");
        assert_eq!(foreign_evidence, None);
    }

    #[tokio::test]
    async fn enqueue_candidate_does_not_resurrect_hidden_row_without_reference() {
        let db = migrated_pool().await;
        let mut hidden = session_candidate(
            "sess-hidden",
            1_714_000_000_000,
            "Temporary scratch helper cleanup",
            "Remove the temporary scratch helper after the local debug run.",
            vec!["tmp/scratch/helper.ts"],
        );
        hidden.local_triage = Some(SessionMinedLocalTriage {
            status: SessionMinedLocalTriageStatus::SupersededBy,
            reason: "legacy malformed hidden row".to_owned(),
            reference: None,
            at: 1_714_000_000_001,
        });
        let hidden_id = insert_raw_session_candidate(&db, &hidden).await;
        let incoming = session_candidate(
            "sess-incoming",
            1_714_086_400_000,
            "Temporary scratch helper cleanup",
            "Remove the temporary scratch helper after the local debug run.",
            vec!["tmp/scratch/helper.ts"],
        );

        let repeated_id = enqueue_candidate(&db, &incoming)
            .await
            .expect("incoming enqueue");

        assert_eq!(repeated_id, hidden_id);
        let row_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM cloud_outbox WHERE kind = 'session_mined_candidate'",
        )
        .fetch_one(&db)
        .await
        .expect("row count");
        assert_eq!(row_count, 1);
        let hidden_evidence: Option<i64> = sqlx::query_scalar(
            "SELECT CAST(json_extract(payload_json, '$.localEvidence.distinctEvidenceCount') AS INTEGER) \
             FROM cloud_outbox WHERE id = ?1",
        )
        .bind(hidden_id)
        .fetch_one(&db)
        .await
        .expect("hidden evidence");
        assert_eq!(hidden_evidence, None);
    }

    #[tokio::test]
    async fn enqueue_candidate_keeps_genuinely_new_lesson() {
        let db = migrated_pool().await;
        let first = candidate();
        let second = session_candidate(
            "sess-new",
            1_714_086_400_000,
            "Use ExternalLink for cross deployment navigation",
            "Use ExternalLink for navigation targets served outside the current TanStack router.",
            vec!["src/modules/ExternalLink.tsx"],
        );

        enqueue_candidate(&db, &first).await.expect("first enqueue");
        enqueue_candidate(&db, &second)
            .await
            .expect("second enqueue");

        let row_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM cloud_outbox WHERE kind = 'session_mined_candidate'",
        )
        .fetch_one(&db)
        .await
        .expect("row count");
        assert_eq!(row_count, 2);
    }

    #[tokio::test]
    async fn enqueue_candidate_does_not_resurface_dropped_noise() {
        let db = migrated_pool().await;
        let noise = session_candidate(
            "sess-noise",
            1_714_000_000_000,
            "Temporary scratch helper cleanup",
            "Remove the temporary scratch helper after the local debug run.",
            vec!["tmp/scratch/helper.ts"],
        );
        let row_id = enqueue_candidate(&db, &noise).await.expect("first enqueue");
        difflore_core::memory_inbox::set_candidate_triage(
            &db,
            &noise.content_hash,
            SessionMinedLocalTriageStatus::DroppedLowSignal,
            "single-session temporary scratch file",
            None,
        )
        .await
        .expect("drop candidate");
        let repeated = session_candidate(
            "sess-noise-2",
            1_714_086_400_000,
            "Temporary scratch helper cleanup",
            "Remove the temporary scratch helper after the local debug run.",
            vec!["tmp/scratch/helper.ts"],
        );

        let repeated_id = enqueue_candidate(&db, &repeated)
            .await
            .expect("repeat enqueue");

        assert_eq!(repeated_id, row_id);
        let row_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM cloud_outbox WHERE kind = 'session_mined_candidate'",
        )
        .fetch_one(&db)
        .await
        .expect("row count");
        assert_eq!(row_count, 1);
        let status: String = sqlx::query_scalar(
            "SELECT json_extract(payload_json, '$.localTriage.status') FROM cloud_outbox",
        )
        .fetch_one(&db)
        .await
        .expect("triage status");
        assert_eq!(status, "dropped_low_signal");
    }

    #[test]
    fn merge_candidate_uses_mined_file_evidence_and_target_source_repo() {
        let existing = vec![existing_rule()];
        let mined = vec!["crates/difflore-cli/src/session_mine/worker.rs".to_owned()];

        let candidate = merge_candidate_from_verdict(&MergeCandidateInput {
            session_id: "sess_merge",
            ts_ms: 1_714_000_000_000,
            source_repo: "local/repo",
            gate_model: "claude-code:gate",
            existing_rules: &existing,
            rule_id: "11111111-1111-4111-8111-111111111111",
            gate_title: Some("Gate title"),
            updated_body: "Merged body the cloud should apply.",
            mined_file_patterns: &mined,
        })
        .expect("valid merge candidate");

        assert_eq!(
            candidate.gate_verdict,
            "MERGE:11111111-1111-4111-8111-111111111111"
        );
        assert_eq!(candidate.source_repo, "upstream/repo");
        assert_eq!(candidate.title, "Preserve async cleanup");
        assert_eq!(candidate.body, "Merged body the cloud should apply.");
        assert_eq!(
            candidate.file_patterns,
            vec![
                "crates/difflore-cli/src/session_mine/worker.rs",
                "crates/difflore-cli/src/**/*.rs"
            ]
        );
        assert!(candidate.requires_human_approval);
    }

    #[test]
    fn merge_candidate_falls_back_to_target_file_patterns_when_gate_omits_scope() {
        let existing = vec![existing_rule()];

        let candidate = merge_candidate_from_verdict(&MergeCandidateInput {
            session_id: "sess_merge",
            ts_ms: 1_714_000_000_000,
            source_repo: "local/repo",
            gate_model: "claude-code:gate",
            existing_rules: &existing,
            rule_id: "11111111-1111-4111-8111-111111111111",
            gate_title: None,
            updated_body: "Merged body.",
            mined_file_patterns: &[],
        })
        .expect("target file_patterns keep merge candidate scoped");

        assert_eq!(
            candidate.file_patterns,
            vec!["crates/difflore-cli/src/**/*.rs"]
        );
        assert_eq!(
            candidate.gate_verdict,
            "MERGE:11111111-1111-4111-8111-111111111111"
        );
    }

    #[tokio::test]
    async fn load_existing_rules_carries_scope_metadata() {
        let db = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect(":memory:")
            .await
            .expect("memory db");
        sqlx::query(
            "CREATE TABLE skills (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                description TEXT NOT NULL,
                repo_owner TEXT,
                repo_name TEXT,
                cloud_id TEXT,
                source_repo TEXT,
                file_patterns TEXT,
                status TEXT NOT NULL,
                installed_at TEXT NOT NULL
            )",
        )
        .execute(&db)
        .await
        .expect("schema");
        sqlx::query(
            "INSERT INTO skills
             (id, name, description, repo_owner, repo_name, cloud_id, source_repo, file_patterns, status, installed_at)
             VALUES
             ('11111111-1111-4111-8111-111111111111', 'Scoped', 'Body', NULL, NULL, NULL, 'Owner/Repo', '[\"src/**/*.rs\"]', 'active', '2026-01-02'),
             ('local-rule-slug', 'Published local', 'Local Body', NULL, NULL, '33333333-3333-4333-8333-333333333333', 'Owner/Repo', '[\"src/local/**/*.rs\"]', 'active', '2026-01-03'),
             ('local-only-rule', 'Local only', 'Body', NULL, NULL, NULL, 'Owner/Repo', '[\"src/local-only/**/*.rs\"]', 'active', '2026-01-04'),
             ('22222222-2222-4222-8222-222222222222', 'Foreign', 'Body', NULL, NULL, NULL, 'other/repo', '[\"other/**/*.rs\"]', 'active', '2026-01-05')",
        )
        .execute(&db)
        .await
        .expect("insert");

        let rules = load_existing_rules(&db, "owner/repo").await;

        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].rule_id, "33333333-3333-4333-8333-333333333333");
        assert_eq!(rules[0].title, "Published local");
        assert_eq!(rules[0].source_repo.as_deref(), Some("Owner/Repo"));
        assert_eq!(rules[0].file_patterns, vec!["src/local/**/*.rs"]);
        assert_eq!(rules[1].rule_id, "11111111-1111-4111-8111-111111111111");
        assert_eq!(rules[1].file_patterns, vec!["src/**/*.rs"]);
    }

    #[test]
    fn cloud_rule_id_check_accepts_uuid_shape_only() {
        assert!(looks_like_cloud_rule_id(
            "11111111-1111-4111-8111-111111111111"
        ));
        assert!(!looks_like_cloud_rule_id("rule-merge"));
        assert!(!looks_like_cloud_rule_id(
            "11111111-1111-4111-8111-11111111111x"
        ));
    }

    #[test]
    fn resolve_source_repo_fails_closed_without_supported_remote() {
        let dir = TempDir::new().unwrap();
        let path = dir.path();
        assert_eq!(
            resolve_source_repo_with_gitlab_hosts(Some(path.to_str().unwrap()), &[]),
            None,
            "session-mine must not fabricate a basename source_repo"
        );
    }

    #[tokio::test]
    async fn self_managed_gitlab_session_scope_recalls_only_matching_repo_rules() {
        let gitlab_repo = TempDir::new().unwrap();
        init_git_repo_with_origin(
            gitlab_repo.path(),
            "ssh://git@gitlab.corp.example:8443/group/project.git",
        );
        let github_repo = TempDir::new().unwrap();
        init_git_repo_with_origin(github_repo.path(), "https://github.com/group/project.git");

        let gitlab_hosts = vec!["gitlab.corp.example:8443".to_owned()];
        let gitlab_scope = resolve_source_repo_with_gitlab_hosts(
            Some(gitlab_repo.path().to_str().unwrap()),
            &gitlab_hosts,
        )
        .expect("configured self-managed GitLab host must resolve");
        assert_eq!(
            gitlab_scope.as_str(),
            "gitlab.corp.example:8443/group/project"
        );
        assert_eq!(
            resolve_source_repo_with_gitlab_hosts(Some(gitlab_repo.path().to_str().unwrap()), &[]),
            None,
            "self-managed GitLab must fail closed when the host is not configured"
        );

        let github_scope =
            resolve_source_repo_with_gitlab_hosts(Some(github_repo.path().to_str().unwrap()), &[])
                .expect("GitHub remote must still resolve without configured GitLab hosts");
        assert_eq!(github_scope.as_str(), "group/project");
        assert_ne!(
            gitlab_scope.as_str(),
            github_scope.as_str(),
            "provider/host dimension must prevent same-namespace repo collisions"
        );

        let db = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect(":memory:")
            .await
            .expect("memory db");
        sqlx::query(
            "CREATE TABLE skills (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                description TEXT NOT NULL,
                repo_owner TEXT,
                repo_name TEXT,
                cloud_id TEXT,
                source_repo TEXT,
                file_patterns TEXT,
                status TEXT NOT NULL,
                installed_at TEXT NOT NULL
            )",
        )
        .execute(&db)
        .await
        .expect("schema");
        sqlx::query(
            "INSERT INTO skills
             (id, name, description, repo_owner, repo_name, cloud_id, source_repo, file_patterns, status, installed_at)
             VALUES
             ('11111111-1111-4111-8111-111111111111', 'GitLab scoped', 'Body', NULL, NULL, NULL, 'gitlab.corp.example:8443/group/project', '[\"src/**/*.rs\"]', 'active', '2026-01-02'),
             ('22222222-2222-4222-8222-222222222222', 'GitHub scoped', 'Body', NULL, NULL, NULL, 'group/project', '[\"src/**/*.rs\"]', 'active', '2026-01-03')",
        )
        .execute(&db)
        .await
        .expect("insert");

        let recalled = load_existing_rules(&db, gitlab_scope.as_str()).await;

        assert_eq!(recalled.len(), 1);
        assert_eq!(recalled[0].title, "GitLab scoped");
        assert_eq!(
            recalled[0].source_repo.as_deref(),
            Some("gitlab.corp.example:8443/group/project")
        );
    }

    fn init_git_repo_with_origin(path: &std::path::Path, origin_url: &str) {
        run_git_test(path, &["init"]);
        run_git_test(path, &["remote", "add", "origin", origin_url]);
    }

    fn run_git_test(path: &std::path::Path, args: &[&str]) {
        let output = difflore_core::infra::git::git_command(path)
            .args(args)
            .output()
            .expect("git command must run");
        assert!(
            output.status.success(),
            "git {:?} failed: stdout={} stderr={}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
