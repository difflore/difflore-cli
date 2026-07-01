use crate::hook::{adapters, banner, cache, forward};

use super::drift_report::{read_last_assistant_text, stated_vs_actual_warning};
use super::fire_log::remember_hook_fire_maybe_deferred;
use difflore_core::contract::RecordAcceptedEditRequest;
use difflore_core::domain::rule_fingerprint::rule_fingerprint;
use difflore_core::observability::injection_log::InjectionDropReason;

const HOOK_SPILL_REPLAY_MAX: usize = 16;
const AGENT_ACCEPTANCE_SOURCE: &str = "agent_retained_edit";
const AGENT_ACCEPTANCE_CLIENT: &str = "difflore_hook";
const ACCEPTED_EDIT_MAX_CODE_BYTES: usize = 100_000;

fn maybe_spawn_outbox_daemon() {
    if difflore_core::infra::env::truthy(difflore_core::infra::env::DIFFLORE_DISABLE_OUTBOX_DAEMON)
    {
        return;
    }
    if matches!(
        difflore_core::infra::daemon::status(),
        difflore_core::infra::daemon::DaemonStatus::Running { .. }
    ) {
        return;
    }
    if let Err(e) = forward::spawn::spawn_outbox_daemon_detached()
        && difflore_core::infra::env::debug_telemetry()
    {
        eprintln!("[difflore.hook] outbox daemon spawn skipped: {e}");
    }
}

pub(crate) async fn hook_output_for_raw(
    client_name: &str,
    adapter: &dyn adapters::PlatformAdapter,
    raw: &str,
    debug: bool,
    defer_log: bool,
    hot_state: Option<&forward::State>,
    forward_miss: bool,
) -> anyhow::Result<String> {
    let event = match adapter.parse_stdin(raw) {
        Ok(ev) => ev,
        Err(e) => {
            if debug {
                eprintln!("[difflore.hook] parse error: {e}");
            }
            difflore_core::observability::injection_log::record_with_reason(
                "hook",
                0,
                None,
                Some(InjectionDropReason::ParseError),
            );
            return Ok(adapter.format_output(adapters::types::HookResult::noop()));
        }
    };
    let event_label = hook_event_label(&event).to_owned();
    // Capture file_path before the event moves into dispatch so we can stamp it
    // on the fire-log entry (lets the audit answer "did rules surface for the
    // right file?").
    let event_file_path = event.target_file_path();

    // Do not run the general startup gate from lifecycle hooks: even cached, a
    // stale entry can trigger provider/cloud probes and make a PreToolUse hook
    // wait on the network. The dispatcher below opens the local DB/index lazily
    // and degrades to noop on any local failure.

    // Capture the wire-form event name before the event moves into the
    // dispatcher; the adapter echoes it back in the response envelope (Claude
    // Code rejects responses whose hookEventName doesn't match the firing event).
    let event_name = event.wire_name();

    let trace_started = std::time::Instant::now();
    let trace = difflore_core::infra::env::trace_hook();
    match dispatch_hook_event_with_state(client_name, event, hot_state, forward_miss).await {
        Ok(mut result) => {
            if trace {
                eprintln!(
                    "[difflore.hook.trace] dispatch_hook_event_total={}ms",
                    trace_started.elapsed().as_millis()
                );
            }
            if result.event_name.is_none() {
                result.event_name = Some(event_name.to_owned());
            }
            // Persist the fire after dispatch so the injection count can be
            // recorded alongside the file path. Recent duplicate events should
            // remain a true cold-path skip and avoid initializing any on-disk
            // stores just to record an audit line.
            if result.drop_reason != Some(InjectionDropReason::RecentDuplicate) {
                remember_hook_fire_maybe_deferred(
                    client_name.to_owned(),
                    event_label.clone(),
                    result.rules_injected,
                    event_file_path.clone(),
                    Some(trace_started.elapsed().as_millis() as i64),
                    result.drop_reason,
                    defer_log,
                );
            }
            Ok(adapter.format_output(result))
        }
        Err(err) => {
            if trace {
                eprintln!(
                    "[difflore.hook.trace] dispatch_hook_event_error={}ms",
                    trace_started.elapsed().as_millis()
                );
            }
            // Record the fire even on dispatch failure (with no injection count)
            // so the doctor's 24h count reflects all reaches; otherwise transport
            // errors look like the hook silently dropped the event.
            remember_hook_fire_maybe_deferred(
                client_name.to_owned(),
                event_label.clone(),
                None,
                event_file_path.clone(),
                Some(trace_started.elapsed().as_millis() as i64),
                Some(InjectionDropReason::RetrievalError),
                defer_log,
            );
            Err(err)
        }
    }
}

/// Translate a canonical `HookEvent` into the right `DiffLore` action and return
/// an adapter-agnostic `HookResult`. A free function so the dispatch logic is
/// unit-testable without threading stdio. `client_name` is the platform string
/// the hook reports (`"claude-code"`, `"cursor"`, …); session-mine and the
/// session banner use it for transcript-format dispatch and debug trails.
async fn dispatch_hook_event_with_state(
    client_name: &str,
    event: adapters::types::HookEvent,
    hot_state: Option<&forward::State>,
    forward_miss: bool,
) -> anyhow::Result<adapters::types::HookResult> {
    use adapters::types::{HookEvent, HookResult};

    match event {
        HookEvent::PreToolUseRead { .. } => {
            // No pre-read injection: a Read is too weak a signal to predict
            // whether a rule applies (most reads are exploratory and never
            // produce an edit), so it paid full retrieval token cost for a
            // near-zero hit rate. Rule surfacing happens at PostToolUse, where
            // the actual diff is in hand.
            Ok(HookResult::noop_with_reason(
                InjectionDropReason::PreReadDisabled,
            ))
        }
        HookEvent::PostToolUse {
            tool_name,
            cwd,
            file_path,
            target_files,
            diff,
            session_id,
            new_text,
            old_text,
        } => {
            handle_post_tool_use(
                hot_state,
                PostToolUseEvent {
                    tool_name,
                    cwd,
                    file_path,
                    target_files,
                    diff,
                    session_id,
                    new_text,
                    old_text,
                },
            )
            .await
        }
        HookEvent::UserPromptSubmit {
            prompt,
            session_id,
            transcript_path,
            cwd,
        } => {
            let correction_pair =
                correction_pair_for_prompt(&prompt, transcript_path.as_deref(), cwd.as_deref());

            // Session-mine mid-session cadence: one cheap state-file bump per
            // prompt, with the actual mining worker detached only every
            // TURNS_PER_FIRE prompts. This keeps normal prompt handling off the
            // DB / LLM path.
            if background_capture_enabled()
                && let Ok(state_path) =
                    crate::session_mine::trigger::state_file_for_project(cwd.as_deref())
                && crate::session_mine::trigger::should_trigger_after_user_prompt(
                    &state_path,
                    session_id.as_deref(),
                )
            {
                crate::session_mine::run_worker_detached(
                    client_name.to_owned(),
                    transcript_path,
                    session_id.clone(),
                    cwd.clone(),
                    false,
                );
            }
            if let Some(pair) = correction_pair {
                crate::session_mine::run_targeted_pairs_detached(
                    client_name.to_owned(),
                    vec![pair],
                    session_id,
                    cwd,
                    crate::session_mine::GateMode::Correction,
                );
            }
            if let Some(nudge) = super::remember_nudge::nudge_for_prompt(&prompt) {
                return Ok(nudge);
            }
            if let Some(nudge) = super::pre_submit_nudge::nudge_for_prompt(&prompt) {
                return Ok(nudge);
            }
            Ok(HookResult::noop_with_reason(
                InjectionDropReason::NotApplicable,
            ))
        }
        HookEvent::Stop {
            session_id,
            transcript_path,
            cwd,
        }
        | HookEvent::SessionEnd {
            session_id,
            transcript_path,
            cwd,
        } => {
            let capture_enabled = background_capture_enabled();
            // Strictly-advisory stated-vs-actual drift check, computed up front
            // because `transcript_path`/`cwd` are moved into the session-mine
            // worker below. If the agent's closing message claimed file edits
            // absent from `git diff`, we surface a short user-visible note at the
            // end. Must never block — any error/no-mismatch yields `None`.
            let drift_warning = match (transcript_path.as_deref(), cwd.as_deref()) {
                (Some(transcript_path), Some(cwd)) => {
                    stated_vs_actual_warning(transcript_path, cwd)
                }
                _ => None,
            };
            if capture_enabled {
                maybe_emit_rule_actual_citations(session_id.as_deref(), transcript_path.as_deref())
                    .await;
                let _accepted_count =
                    maybe_emit_fix_outcomes(session_id.as_deref(), cwd.as_deref()).await;
            }
            // Session-mine: an expiring conversation mines its last few pairs
            // into a candidate rule. Throttled by a cooldown — some hosts fire
            // `Stop` every turn (Claude Code), so an unconditional force-mine
            // would spawn a gate agent-CLI call per turn. The worker is detached
            // and best-effort — it must never delay or fail the hook output.
            if capture_enabled
                && let Ok(state_path) =
                    crate::session_mine::trigger::state_file_for_project(cwd.as_deref())
                && crate::session_mine::trigger::should_trigger_session_end(&state_path)
            {
                crate::session_mine::run_worker_detached(
                    client_name.to_owned(),
                    transcript_path,
                    session_id,
                    cwd,
                    true,
                );
            }
            if capture_enabled && let Ok(db) = difflore_core::infra::db::init_db().await {
                // The background lease is intentionally dirty-gated; arm it
                // before scheduling so session-end triggers do not skip as
                // `not_dirty` while pending local memory exists.
                crate::commands::memory::mark_memory_autopilot_dirty_best_effort(
                    &db,
                    "session_end",
                )
                .await;
                crate::commands::memory::schedule_memory_autopilot_best_effort(
                    &db,
                    "session_end",
                    difflore_core::memory_autopilot_schedule::SESSION_END_AUTOPILOT_COOLDOWN_SECS,
                )
                .await;
            }
            if let Some(warning) = drift_warning {
                let mut result = HookResult::noop_with_reason(InjectionDropReason::NotApplicable);
                result.system_message = Some(warning);
                return Ok(result);
            }
            // Keep lifecycle hooks quiet: hosts render `systemMessage` as
            // event-name chatter that reads like an internal tool speaking.
            Ok(HookResult::noop_with_reason(
                InjectionDropReason::NotApplicable,
            ))
        }
        HookEvent::SessionStart { cwd, .. } => {
            // Warm the shared cross-repo starter index here, off the
            // latency-critical PostToolUse path. Lets a repo with no scoped
            // memory still get transferable, file-matched rules from the user's
            // other repos on later edits (PostToolUse only uses the starter if
            // it is already current — it never builds it). Best-effort and
            // freshness-gated: a cheap no-op once built, rebuilt only when the
            // corpus changed, failures swallowed.
            let db = if let Some(state) = hot_state {
                state.db.clone()
            } else {
                match difflore_core::infra::db::init_db().await {
                    Ok(p) => p,
                    Err(_) => {
                        return Ok(HookResult::noop_with_reason(
                            InjectionDropReason::DbUnavailable,
                        ));
                    }
                }
            };
            let _ =
                difflore_core::context::orchestrator::ensure_cross_repo_starter_indexed(&db).await;

            // Since-last-session recap: if this repo gained rules since the last
            // SessionStart, surface a short note via `additional_context`. The
            // helper is self-budgeted and returns `None` on quiet sessions.
            let banner_ctx = banner::BannerContext {
                cwd,
                client_name: client_name.to_owned(),
                forward_miss,
            };
            if let Some(banner) = banner::render_since_last_session_banner(&banner_ctx).await {
                return Ok(HookResult::with_context(banner));
            }
            Ok(HookResult::noop_with_reason(
                InjectionDropReason::NotApplicable,
            ))
        }
    }
}

/// Destructured `HookEvent::PostToolUse` payload, grouped into one struct so
/// the phase helpers can take it by value without tripping the argument-count
/// lint.
struct PostToolUseEvent {
    tool_name: String,
    cwd: Option<String>,
    file_path: Option<String>,
    target_files: Vec<String>,
    diff: Option<String>,
    session_id: Option<String>,
    new_text: Option<String>,
    old_text: Option<String>,
}

/// PostToolUse handler: the rule-retrieval path that fires after a file-mutating
/// tool (or a Bash command). Reads as a short sequence of named phases — early
/// guards, project/skip resolution, the observation/outbox side-channel, then
/// the actual rule fetch — so the control flow is no longer one match arm.
async fn handle_post_tool_use(
    hot_state: Option<&forward::State>,
    event: PostToolUseEvent,
) -> anyhow::Result<adapters::types::HookResult> {
    use adapters::types::HookResult;

    let PostToolUseEvent {
        tool_name,
        cwd,
        file_path,
        target_files,
        diff,
        session_id,
        new_text,
        old_text,
    } = event;

    if tool_name == "Bash" {
        return super::bash_error::recall_for_bash_error(
            hot_state,
            diff.as_deref(),
            session_id.as_deref(),
            cwd.as_deref(),
        )
        .await;
    }

    // Act only on file-mutating tools: acting on Read would flood
    // the agent with irrelevant rule context for zero value.
    if !matches!(tool_name.as_str(), "Edit" | "Write" | "MultiEdit") {
        return Ok(HookResult::noop_with_reason(
            InjectionDropReason::NonMutatingTool,
        ));
    }
    let files = ordered_target_files(file_path.as_deref(), &target_files);
    if files.is_empty() {
        return Ok(HookResult::noop_with_reason(
            InjectionDropReason::MissingTargetFile,
        ));
    }
    let mut project_ctx =
        super::project::resolve_hook_project_context(cwd.as_deref(), &files).await;
    if difflore_core::infra::env::trace_hook() {
        eprintln!(
            "[difflore.hook.trace] resolved project reason={} repo_root={} project_hash={} repo_scopes={}",
            project_ctx.reason,
            project_ctx
                .repo_root
                .as_deref()
                .map_or_else(|| "<none>".to_owned(), |p| p.display().to_string()),
            project_ctx.project_hash.as_deref().unwrap_or("<none>"),
            project_ctx.repo_scopes.join(",")
        );
    }
    let file = files[0].clone();
    let recall_file = project_ctx.recall_file.clone();
    let retrieval_intent = post_edit_retrieval_intent(diff.as_deref(), new_text.as_deref());

    // Check the skip cache before DB init or outbox enqueue; repeated
    // PostToolUse events should stay off the hot path.
    let cache_project_hash = project_ctx.project_hash.clone();
    let should_skip = if let Some(hash) = cache_project_hash.as_deref() {
        cache::should_skip_recent_for_project_hash_with_signal(
            &file,
            "post-edit",
            hash,
            Some(&retrieval_intent),
        )
    } else {
        cache::should_skip_recent_with_signal(&file, "post-edit", Some(&retrieval_intent))
    };
    if should_skip {
        return Ok(HookResult::noop_with_reason(
            InjectionDropReason::RecentDuplicate,
        ));
    }

    // Build the observation payload before opening SQLite so a DB-open
    // failure can still fall back to the durable hook spill directory.
    let obs_input = difflore_core::observability::classifier::ClassifyInput {
        tool: &tool_name,
        file_path: Some(&file),
        diff: diff.as_deref(),
        new_text: new_text.as_deref(),
        old_text: old_text.as_deref(),
        session_id: session_id.as_deref(),
        ts_ms: None,
    };
    let obs_payload =
        difflore_core::observability::classifier::classify(&obs_input).and_then(|obs| {
            match serde_json::to_string(&obs) {
                Ok(payload) => Some(payload),
                Err(e) => {
                    if difflore_core::infra::env::debug_telemetry() {
                        eprintln!("[difflore.hook] observation serialize failed: {e}");
                    }
                    None
                }
            }
        });

    let db = if let Some(state) = hot_state {
        state.db.clone()
    } else if let Ok(p) = difflore_core::infra::db::init_db().await {
        p
    } else {
        if let Some(payload) = obs_payload.as_deref()
            && difflore_core::cloud::capture::capture_enabled()
        {
            let _ = difflore_core::cloud::outbox::spill_observation_payload(
                payload,
                "init_db failed before hook observation enqueue",
            );
            maybe_spawn_outbox_daemon();
        }
        return Ok(HookResult::noop_with_reason(
            InjectionDropReason::DbUnavailable,
        ));
    };

    enqueue_observation_and_replay_outbox(&db, obs_payload.as_deref()).await;

    super::project::refresh_repo_scopes(&mut project_ctx).await;
    let Ok(index_pool) = super::project::index_pool_for_project_context(
        hot_state,
        project_ctx.project_hash.as_deref(),
    )
    .await
    else {
        return Ok(HookResult::noop_with_reason(
            InjectionDropReason::IndexUnavailable,
        ));
    };

    maybe_emit_rule_cited_in_edit(
        &db,
        session_id.as_deref(),
        &file,
        diff.as_deref().or(new_text.as_deref()),
    )
    .await;

    match difflore_core::mcp_server::fetch_relevant_rules_for_hook_with_repo_scopes(
        &db,
        &index_pool,
        &recall_file,
        &retrieval_intent,
        session_id.as_deref(),
        &project_ctx.repo_scopes,
    )
    .await
    {
        Ok(ctx) if ctx.rules_injected > 0 => {
            if let Some(hash) = cache_project_hash.as_deref() {
                cache::remember_injection_for_project_hash_with_signal(
                    &file,
                    "post-edit",
                    ctx.rules_injected,
                    hash,
                    Some(&retrieval_intent),
                );
            } else {
                cache::remember_injection(
                    &file,
                    "post-edit",
                    ctx.rules_injected,
                    Some(&retrieval_intent),
                );
            }
            // No unconditional system_message: the assistant's citation
            // of "Rule N" in its reply is the visible signal. Surfacing
            // "injected N rules" on every Edit pollutes the user's view
            // even when none of the rules applied.
            let mut result = HookResult::with_context(ctx.rendered);
            result.rules_injected = Some(ctx.rules_injected);
            Ok(result)
        }
        Ok(ctx) => Ok(HookResult::noop_with_reason(
            ctx.drop_reason
                .unwrap_or(InjectionDropReason::RetrievalEmpty),
        )),
        Err(_) => Ok(HookResult::noop_with_reason(
            InjectionDropReason::RetrievalError,
        )),
    }
}

/// Replay any spilled observations and enqueue this edit's observation into the
/// cloud outbox, spawning the outbox daemon when there is work to flush.
/// Best-effort: every failure spills to disk (or is swallowed) so it can never
/// block the retrieval path.
async fn enqueue_observation_and_replay_outbox(
    db: &difflore_core::SqlitePool,
    obs_payload: Option<&str>,
) {
    let queue = difflore_core::cloud::outbox::OutboxQueue::new(db.clone());
    let mut has_outbox_work = false;
    if let Ok(report) =
        difflore_core::cloud::outbox::replay_spilled_observations(&queue, HOOK_SPILL_REPLAY_MAX)
            .await
    {
        has_outbox_work |= report.replayed > 0;
    }
    // Enqueue this edit's observation. Failures spill to disk so a
    // later healthy hook/daemon can replay them into `cloud_outbox`.
    if let Some(payload) = obs_payload {
        match queue
            .enqueue(difflore_core::cloud::outbox::kind::OBSERVATION, payload)
            .await
        {
            Ok(id) => {
                has_outbox_work |= id > 0;
            }
            Err(e) => {
                if difflore_core::infra::env::debug_telemetry() {
                    eprintln!("[difflore.hook] observation enqueue failed: {e}");
                }
                if difflore_core::cloud::capture::capture_enabled()
                    && difflore_core::cloud::outbox::spill_observation_payload(
                        payload,
                        e.to_string(),
                    )
                    .is_ok()
                {
                    has_outbox_work = true;
                }
            }
        }
    }
    if has_outbox_work {
        maybe_spawn_outbox_daemon();
    }
}

fn background_capture_enabled() -> bool {
    difflore_core::cloud::capture::capture_enabled()
}

fn correction_pair_for_prompt(
    prompt: &str,
    transcript_path: Option<&str>,
    cwd: Option<&str>,
) -> Option<crate::session_mine::extract::Pair> {
    if !background_capture_enabled() || !super::correction_nudge::has_implicit_correction(prompt) {
        return None;
    }
    let assistant_text = read_last_assistant_text(transcript_path?)?;
    if assistant_text.trim().is_empty() {
        return None;
    }
    let state_path = crate::session_mine::trigger::state_file_for_project(cwd).ok()?;
    if !crate::session_mine::trigger::should_trigger_correction(&state_path) {
        return None;
    }
    Some(crate::session_mine::extract::Pair {
        user_prompt: prompt.trim().to_owned(),
        assistant_text,
    })
}

async fn maybe_emit_rule_cited_in_edit(
    db: &difflore_core::SqlitePool,
    session_id: Option<&str>,
    file_path: &str,
    diff_excerpt: Option<&str>,
) {
    let Ok(emitter) = difflore_core::cloud::observations::ObservationEmitter::open_default().await
    else {
        return;
    };
    let session_id = session_id.unwrap_or("");
    let Ok(Some(rule_id)) = emitter
        .strongest_recent_rule_id(
            db,
            session_id,
            file_path,
            difflore_core::cloud::observations::RECENT_RULE_FIRE_WINDOW_MS,
        )
        .await
    else {
        return;
    };

    let excerpt = truncate_chars(diff_excerpt.unwrap_or(""), 500);
    let event = difflore_core::cloud::observations::ObservationEvent::RuleCitedInEdit {
        rule_id,
        session_id: session_id.to_owned(),
        file_path: file_path.to_owned(),
        diff_excerpt: excerpt,
        cited_at: chrono::Utc::now(),
    };
    let _ = emitter.enqueue(&event).await;
}

async fn maybe_emit_rule_actual_citations(session_id: Option<&str>, transcript_path: Option<&str>) {
    let Some(transcript_path) = transcript_path else {
        return;
    };
    let Some(text) = read_last_assistant_text(transcript_path) else {
        return;
    };
    if text.trim().is_empty() {
        return;
    }

    let cited_fingerprints = rule_fingerprints_from_citation_text(&text);
    let cited_numbers = rule_numbers_from_citation_text(&text);
    let mentions_learned_from = text.to_ascii_lowercase().contains("learned from");
    if cited_fingerprints.is_empty() && cited_numbers.is_empty() && !mentions_learned_from {
        return;
    }

    let session_id = session_id.unwrap_or("");
    let Ok(emitter) = difflore_core::cloud::observations::ObservationEmitter::open_default().await
    else {
        return;
    };
    let Ok(Some(rule_fire)) = emitter.latest_rule_fire_for_session(session_id).await else {
        return;
    };
    if rule_fire.rule_ids.is_empty() {
        return;
    }

    let mut rule_ids =
        rule_ids_for_citations(&rule_fire.rule_ids, &cited_fingerprints, &cited_numbers);
    if mentions_learned_from && let Ok(db) = difflore_core::infra::db::init_db().await {
        for id in rule_ids_for_learned_sources(&db, &rule_fire.rule_ids, &text).await {
            rule_ids.insert(id);
        }
    }
    if rule_ids.is_empty() {
        return;
    }

    let excerpt = truncate_chars(&text, 500);
    let cited_at = chrono::Utc::now();
    let mut emitted = false;
    for rule_id in rule_ids {
        if emitter
            .has_rule_actual_citation(session_id, &rule_id)
            .await
            .unwrap_or(false)
        {
            continue;
        }
        let event = difflore_core::cloud::observations::ObservationEvent::RuleActuallyCited {
            rule_id,
            session_id: session_id.to_owned(),
            file_path: rule_fire.file_path.clone(),
            citation_excerpt: excerpt.clone(),
            cited_at,
        };
        if emitter.enqueue(&event).await.is_ok() {
            emitted = true;
        }
    }

    if emitted {
        let client = difflore_core::cloud::client::CloudClient::create().await;
        let _ = emitter.flush_to_cloud(&client).await;
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct FingerprintedRuleCitation {
    position: usize,
    fingerprint: String,
}

fn rule_fingerprints_from_citation_text(
    text: &str,
) -> std::collections::BTreeSet<FingerprintedRuleCitation> {
    let lower = text.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut out = std::collections::BTreeSet::new();
    let mut search_from = 0usize;

    while let Some(relative) = lower[search_from..].find("df:") {
        let start = search_from + relative;
        let before_ok = start == 0 || !bytes[start - 1].is_ascii_alphanumeric();
        let mut pos = start + "df:".len();
        if !before_ok {
            search_from = pos;
            continue;
        }

        let digit_start = pos;
        while bytes.get(pos).is_some_and(u8::is_ascii_digit) {
            pos += 1;
        }
        let Some(b'-') = bytes.get(pos).copied() else {
            search_from = pos.max(start + 1);
            continue;
        };
        let Ok(position) = lower[digit_start..pos].parse::<usize>() else {
            search_from = pos.max(start + 1);
            continue;
        };
        if position == 0 {
            search_from = pos.max(start + 1);
            continue;
        }
        pos += 1;

        let fingerprint_start = pos;
        while bytes.get(pos).is_some_and(u8::is_ascii_hexdigit) {
            pos += 1;
        }
        let fingerprint = &lower[fingerprint_start..pos];
        if fingerprint.len() == 4 {
            out.insert(FingerprintedRuleCitation {
                position,
                fingerprint: fingerprint.to_owned(),
            });
        }
        search_from = pos.max(start + 1);
    }

    out
}

fn rule_numbers_from_citation_text(text: &str) -> std::collections::BTreeSet<usize> {
    let lower = text.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut out = std::collections::BTreeSet::new();

    // Scan for both "rule N" (legacy) and "memory N" (current product language).
    // Hook output instructs agents to cite as "applying Memory N", but older
    // transcripts and external skills still use "Rule N"; accept both so the
    // citation telemetry doesn't lose ground.
    for needle in ["rule", "memory"] {
        let mut search_from = 0usize;
        while let Some(relative) = lower[search_from..].find(needle) {
            let start = search_from + relative;
            let mut pos = start + needle.len();
            let before_ok = start == 0 || !bytes[start - 1].is_ascii_alphanumeric();
            let after_word = bytes.get(pos).is_some_and(u8::is_ascii_alphabetic);
            if !before_ok || after_word {
                search_from = pos;
                continue;
            }

            while bytes
                .get(pos)
                .is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n' | b'#' | b'`'))
            {
                pos += 1;
            }

            let digit_start = pos;
            while bytes.get(pos).is_some_and(u8::is_ascii_digit) {
                pos += 1;
            }
            if digit_start < pos
                && let Ok(n) = lower[digit_start..pos].parse::<usize>()
                && n > 0
            {
                out.insert(n);
            }
            search_from = pos.max(start + 1);
        }
    }

    out
}

fn rule_ids_for_citations(
    candidate_rule_ids: &[String],
    fingerprinted: &std::collections::BTreeSet<FingerprintedRuleCitation>,
    rule_numbers: &std::collections::BTreeSet<usize>,
) -> std::collections::BTreeSet<String> {
    let mut out = std::collections::BTreeSet::new();
    let mut fingerprinted_positions = std::collections::BTreeSet::new();

    for citation in fingerprinted {
        fingerprinted_positions.insert(citation.position);
        if let Some(rule_id) = rule_id_for_fingerprinted_citation(candidate_rule_ids, citation) {
            out.insert(rule_id);
        }
    }

    for n in rule_numbers {
        if fingerprinted_positions.contains(n) {
            continue;
        }
        if let Some(rule_id) = candidate_rule_ids.get(n.saturating_sub(1)) {
            out.insert(rule_id.clone());
        }
    }

    out
}

fn rule_id_for_fingerprinted_citation(
    candidate_rule_ids: &[String],
    citation: &FingerprintedRuleCitation,
) -> Option<String> {
    if let Some(rule_id) = candidate_rule_ids.get(citation.position.saturating_sub(1))
        && rule_fingerprint(rule_id) == citation.fingerprint
    {
        return Some(rule_id.clone());
    }

    let mut matches = candidate_rule_ids
        .iter()
        .filter(|rule_id| rule_fingerprint(rule_id) == citation.fingerprint);
    let first = matches.next()?.clone();
    matches.next().is_none().then_some(first)
}

#[cfg(test)]
fn rule_ids_for_citation_numbers(
    candidate_rule_ids: &[String],
    rule_numbers: &std::collections::BTreeSet<usize>,
) -> std::collections::BTreeSet<String> {
    rule_numbers
        .iter()
        .filter_map(|n| candidate_rule_ids.get(n.saturating_sub(1)).cloned())
        .collect()
}

async fn rule_ids_for_learned_sources(
    db: &difflore_core::SqlitePool,
    candidate_rule_ids: &[String],
    text: &str,
) -> Vec<String> {
    if candidate_rule_ids.is_empty() {
        return Vec::new();
    }
    let lower_text = text.to_ascii_lowercase();
    if !lower_text.contains("learned from") {
        return Vec::new();
    }

    let Ok(ids_json) = serde_json::to_string(candidate_rule_ids) else {
        return Vec::new();
    };
    let Ok(rows) = sqlx::query!(
        "SELECT id, source_repo FROM skills \
         WHERE id IN (SELECT value FROM json_each(?1)) \
           AND source_repo IS NOT NULL AND source_repo != ''",
        ids_json,
    )
    .fetch_all(db)
    .await
    else {
        return Vec::new();
    };

    let mut by_repo: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for row in rows {
        let id: String = row.id;
        let source_repo = row.source_repo.unwrap_or_default();
        let source_repo = source_repo.trim().to_ascii_lowercase();
        if id.is_empty() || source_repo.is_empty() {
            continue;
        }
        by_repo.entry(source_repo).or_default().push(id);
    }

    by_repo
        .into_iter()
        .filter_map(|(source_repo, ids)| {
            if ids.len() == 1 && lower_text.contains(&source_repo) {
                ids.into_iter().next()
            } else {
                None
            }
        })
        .collect()
}

/// Emit `FixOutcome` rows for every rule cited in an edit this session and
/// return how many were marked `accepted` (the edit is still present in the
/// working tree / index). The count feeds the end-of-session recap. Returns `0`
/// on any early bail (no emitter, no cited edits) so the recap stays silent for
/// sessions with no signal.
async fn maybe_emit_fix_outcomes(session_id: Option<&str>, cwd: Option<&str>) -> usize {
    let session_id = session_id.unwrap_or("");
    let Ok(emitter) = difflore_core::cloud::observations::ObservationEmitter::open_default().await
    else {
        return 0;
    };
    let Ok(cited) = emitter.cited_edits_for_session(session_id).await else {
        return 0;
    };
    if cited.is_empty() {
        return 0;
    }
    let app_db = difflore_core::infra::db::init_db().await.ok();

    let mut by_rule: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for edit in cited {
        by_rule
            .entry(edit.rule_id)
            .or_default()
            .push(edit.file_path);
    }

    let configured_gitlab_hosts = difflore_core::ingest::gitlab::auth::configured_hosts().await;
    let detect_cwd = cwd.unwrap_or(".").to_owned();
    // Forks git; offload from the async worker thread.
    let detected_repos = tokio::task::spawn_blocking(move || {
        difflore_core::infra::git::detect_repo_full_names_with_gitlab_hosts(
            &detect_cwd,
            &configured_gitlab_hosts,
        )
    })
    .await
    .unwrap_or_default();
    let repo_full_name = detected_repos.first().map(String::as_str);
    // 30-minute cross-link window: the accepted edit must follow the MCP serve
    // closely enough that the agent context still plausibly included the rule.
    // Wider than `RECENT_RULE_FIRE_WINDOW_MS` because `Stop`/`SessionEnd` can
    // fire well after the last serve, but tight enough not to stitch unrelated
    // edits.
    const SERVE_CROSS_LINK_WINDOW_MS: i64 = 30 * 60 * 1000;
    // Count only freshly enqueued outcomes; a rule that already had one this
    // session (the `has_fix_outcome` skip below) is not double-counted.
    let mut accepted_count = 0usize;
    for (rule_id, files) in by_rule {
        if emitter
            .has_fix_outcome(session_id, &rule_id)
            .await
            .unwrap_or(false)
        {
            continue;
        }
        // Each `git diff --quiet` forks git, and this loop scales with the
        // edited-file count, so resolve acceptance off the async worker thread.
        let probe_cwd = cwd.map(ToOwned::to_owned);
        let probe_files = files.clone();
        let (accepted, file_path) = tokio::task::spawn_blocking(move || {
            let cwd = probe_cwd.as_deref();
            let accepted = probe_files.iter().any(|file| git_file_has_diff(cwd, file));
            let file_path = accepted_file_path(cwd, &probe_files);
            (accepted, file_path)
        })
        .await
        .unwrap_or((false, files.first().cloned()));
        if accepted {
            accepted_count += 1;
            if let Some(db) = app_db.as_ref() {
                maybe_enqueue_agent_accepted_edit_proof(
                    &rule_id,
                    cwd,
                    file_path.as_deref(),
                    repo_full_name,
                    db,
                )
                .await;
            }
        }
        let occurred_at = chrono::Utc::now();
        // Best-effort: a SQL failure here downgrades to "no inline link" rather
        // than crashing the hook. The audit path still falls back on the
        // file_path/session_id heuristic.
        let mcp_serve_event_ids = emitter
            .recent_mcp_serve_event_ids(
                &rule_id,
                repo_full_name,
                file_path.as_deref(),
                occurred_at.timestamp_millis(),
                SERVE_CROSS_LINK_WINDOW_MS,
            )
            .await
            .unwrap_or_default();
        let event = difflore_core::cloud::observations::ObservationEvent::FixOutcome {
            rule_id,
            session_id: session_id.to_owned(),
            file_path,
            accepted,
            occurred_at,
            mcp_serve_event_ids,
        };
        let _ = emitter.enqueue(&event).await;
    }

    let client = difflore_core::cloud::client::CloudClient::create().await;
    let _ = emitter.flush_to_cloud(&client).await;
    accepted_count
}

async fn maybe_enqueue_agent_accepted_edit_proof(
    rule_id: &str,
    cwd: Option<&str>,
    file_path: Option<&str>,
    repo_full_name: Option<&str>,
    db: &difflore_core::SqlitePool,
) {
    let Some(file_path) = file_path else {
        return;
    };
    let accepted_edit_rule_ids =
        resolve_hook_accepted_edit_rule_ids(db, &[rule_id.to_owned()]).await;

    let cwd = cwd.map(ToOwned::to_owned);
    let file_path = file_path.to_owned();
    let repo_full_name = repo_full_name.map(ToOwned::to_owned);
    let request = tokio::task::spawn_blocking(move || {
        build_agent_accepted_edit_request(cwd.as_deref(), &file_path, repo_full_name.as_deref())
    })
    .await
    .ok()
    .flatten();
    let Some(mut request) = request else {
        return;
    };
    request.rule_ids = accepted_edit_rule_ids;

    let Ok(payload) = serde_json::to_string(&request) else {
        return;
    };
    let queue = difflore_core::cloud::outbox::OutboxQueue::new(db.clone());
    if queue
        .enqueue(difflore_core::cloud::outbox::kind::ACCEPTED_EDIT, &payload)
        .await
        .is_ok()
    {
        maybe_spawn_outbox_daemon();
    }
}

async fn resolve_hook_accepted_edit_rule_ids(
    db: &difflore_core::SqlitePool,
    rule_ids: &[String],
) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for rule_id in rule_ids {
        let rule_id = rule_id.trim();
        if rule_id.is_empty() {
            continue;
        }
        let resolved = match difflore_core::team::resolve_known_cloud_rule_id(db, rule_id).await {
            Ok(Some(cloud_rule_id)) => cloud_rule_id,
            Ok(None) | Err(_) => rule_id.to_owned(),
        };
        if seen.insert(resolved.clone()) {
            out.push(resolved);
        }
    }
    out
}

fn build_agent_accepted_edit_request(
    cwd: Option<&str>,
    file_path: &str,
    repo_full_name: Option<&str>,
) -> Option<RecordAcceptedEditRequest> {
    let (repo_root, repo_file) = super::project::git_repo_context_for_file(cwd, file_path)?;
    if !git_file_has_diff(cwd, file_path) {
        return None;
    }
    let before_code = git_show_head_file(&repo_root, &repo_file).unwrap_or_default();
    let after_code = read_worktree_file(&repo_root, &repo_file).unwrap_or_default();
    if before_code == after_code
        || before_code.len() > ACCEPTED_EDIT_MAX_CODE_BYTES
        || after_code.len() > ACCEPTED_EDIT_MAX_CODE_BYTES
    {
        return None;
    }
    let diff_signature =
        difflore_core::contract::accepted_edit_diff_signature(&before_code, &after_code);
    Some(RecordAcceptedEditRequest {
        before_code,
        after_code,
        file_path: Some(repo_file.clone()),
        repo_full_name: repo_full_name.map(str::to_owned),
        target_pr_number: None,
        language: difflore_core::context::retrieval::detect_language_from_path(&repo_file),
        acceptance_source: Some(AGENT_ACCEPTANCE_SOURCE.to_owned()),
        client: Some(AGENT_ACCEPTANCE_CLIENT.to_owned()),
        diff_signature: Some(diff_signature),
        rule_ids: Vec::new(),
    })
}

fn git_show_head_file(repo_root: &std::path::Path, repo_file: &str) -> Option<String> {
    let output = difflore_core::infra::git::git_command(repo_root)
        .args(["show", &format!("HEAD:{repo_file}")])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

fn read_worktree_file(repo_root: &std::path::Path, repo_file: &str) -> Option<String> {
    std::fs::read_to_string(repo_root.join(repo_file)).ok()
}

fn accepted_file_path(cwd: Option<&str>, files: &[String]) -> Option<String> {
    files
        .iter()
        .find(|file| git_file_has_diff(cwd, file))
        .or_else(|| files.first())
        .cloned()
}

fn git_file_has_diff(cwd: Option<&str>, file_path: &str) -> bool {
    let Some((repo_root, repo_file)) = super::project::git_repo_context_for_file(cwd, file_path)
    else {
        return false;
    };
    git_quiet_diff_has_changes(&repo_root, &repo_file, false)
        || git_quiet_diff_has_changes(&repo_root, &repo_file, true)
}

fn git_quiet_diff_has_changes(repo_root: &std::path::Path, repo_file: &str, cached: bool) -> bool {
    let mut cmd = difflore_core::infra::git::git_command(repo_root);
    cmd.arg("diff");
    if cached {
        cmd.arg("--cached");
    }
    cmd.args(["--quiet", "--", repo_file])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .ok()
        .and_then(|status| status.code())
        == Some(1)
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_owned();
    }
    s.chars().take(max_chars).collect()
}

fn post_edit_retrieval_intent(diff: Option<&str>, new_text: Option<&str>) -> String {
    let signal = diff
        .filter(|s| !s.trim().is_empty())
        .or_else(|| new_text.filter(|s| !s.trim().is_empty()));
    match signal {
        Some(text) => format!("post-edit\n{}", truncate_chars(text, 1200)),
        None => "post-edit".to_owned(),
    }
}

fn ordered_target_files(file_path: Option<&str>, target_files: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(file_path) = file_path
        && !file_path.trim().is_empty()
    {
        out.push(file_path.trim().to_owned());
    }
    for file in target_files {
        let trimmed = file.trim();
        if trimmed.is_empty() || out.iter().any(|existing| existing == trimmed) {
            continue;
        }
        out.push(trimmed.to_owned());
    }
    out
}

const fn hook_event_label(event: &adapters::types::HookEvent) -> &'static str {
    match event {
        adapters::types::HookEvent::PostToolUse { .. } => "post_tool_use",
        adapters::types::HookEvent::PreToolUseRead { .. } => "pre_tool_use_read",
        adapters::types::HookEvent::SessionStart { .. } => "session_start",
        adapters::types::HookEvent::UserPromptSubmit { .. } => "user_prompt_submit",
        adapters::types::HookEvent::Stop { .. } => "stop",
        adapters::types::HookEvent::SessionEnd { .. } => "session_end",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn post_edit_retrieval_intent_prefers_diff_signal() {
        let intent = post_edit_retrieval_intent(
            Some("case errors.As(err, &maxBytesErr)"),
            Some("fallback text"),
        );

        assert!(intent.starts_with("post-edit\n"));
        assert!(intent.contains("maxBytesErr"));
        assert!(!intent.contains("fallback text"));
    }

    #[test]
    fn post_edit_retrieval_intent_falls_back_to_new_text() {
        let intent = post_edit_retrieval_intent(Some("   "), Some("StatusRequestEntityTooLarge"));

        assert_eq!(intent, "post-edit\nStatusRequestEntityTooLarge");
    }

    #[test]
    fn rule_numbers_from_citation_text_extracts_explicit_rule_references() {
        let nums = rule_numbers_from_citation_text(
            "Applying Rule 2: Preserve status mapping. Rule #10 also applies; rules are useful.",
        );

        assert!(nums.contains(&2));
        assert!(nums.contains(&10));
        assert_eq!(nums.len(), 2);
    }

    #[test]
    fn rule_numbers_from_citation_text_extracts_memory_label_too() {
        // Detection must accept both "Rule" and "Memory" so the citation
        // telemetry doesn't drop to zero.
        let nums = rule_numbers_from_citation_text(
            "Applying Memory 3: Don't strip null. Memory #7 also applies.",
        );

        assert!(nums.contains(&3));
        assert!(nums.contains(&7));
        assert_eq!(nums.len(), 2);
    }

    #[test]
    fn rule_numbers_from_citation_text_handles_mixed_rule_and_memory() {
        // A single response might cite both labels.
        let nums = rule_numbers_from_citation_text("Applying Rule 1 and Memory 4 together.");

        assert!(nums.contains(&1));
        assert!(nums.contains(&4));
        assert_eq!(nums.len(), 2);
    }

    #[test]
    fn rule_ids_for_citation_numbers_maps_to_latest_injected_order() {
        let ids = vec!["r1".to_owned(), "r2".to_owned(), "r3".to_owned()];
        let nums = rule_numbers_from_citation_text("Rule 2 guided this edit; Rule 9 did not.");
        let cited = rule_ids_for_citation_numbers(&ids, &nums);

        assert!(cited.contains("r2"));
        assert_eq!(cited.len(), 1);
    }

    #[test]
    fn rule_fingerprints_from_citation_text_extracts_tokens() {
        let token = difflore_core::domain::rule_fingerprint::memory_citation_token(2, "rule-b");
        let citations =
            rule_fingerprints_from_citation_text(&format!("Applying Memory 2 [{token}]."));

        assert!(citations.contains(&FingerprintedRuleCitation {
            position: 2,
            fingerprint: rule_fingerprint("rule-b"),
        }));
        assert_eq!(citations.len(), 1);
    }

    #[test]
    fn rule_ids_for_citations_prefers_fingerprint_over_position() {
        let ids = vec![
            "rule-a".to_owned(),
            "rule-b".to_owned(),
            "rule-c".to_owned(),
        ];
        let token_for_rule_c_at_old_position =
            difflore_core::domain::rule_fingerprint::memory_citation_token(2, "rule-c");
        let text = format!("Applying Memory 2 [{token_for_rule_c_at_old_position}] here.");
        let cited = rule_ids_for_citations(
            &ids,
            &rule_fingerprints_from_citation_text(&text),
            &rule_numbers_from_citation_text(&text),
        );

        assert!(cited.contains("rule-c"));
        assert!(!cited.contains("rule-b"));
        assert_eq!(cited.len(), 1);
    }

    #[test]
    fn rule_ids_for_citations_keeps_legacy_numeric_fallback() {
        let ids = vec!["rule-a".to_owned(), "rule-b".to_owned()];
        let text = "Applying Memory 2 without a fingerprint.";
        let cited = rule_ids_for_citations(
            &ids,
            &rule_fingerprints_from_citation_text(text),
            &rule_numbers_from_citation_text(text),
        );

        assert!(cited.contains("rule-b"));
        assert_eq!(cited.len(), 1);
    }

    #[test]
    fn rule_ids_for_citations_does_not_numeric_fallback_failed_fingerprint_position() {
        let ids = vec!["rule-a".to_owned(), "rule-b".to_owned()];
        let text = "Applying Memory 2 [df:2-dead] here.";
        let cited = rule_ids_for_citations(
            &ids,
            &rule_fingerprints_from_citation_text(text),
            &rule_numbers_from_citation_text(text),
        );

        assert!(cited.is_empty());
    }

    #[test]
    fn accepted_file_path_falls_back_to_first_cited_file() {
        let files = vec![
            "not-a-real-file.rs".to_owned(),
            "also-not-real.rs".to_owned(),
        ];

        assert_eq!(
            accepted_file_path(Some("not-a-real-worktree"), &files).as_deref(),
            Some("not-a-real-file.rs")
        );
    }

    #[test]
    fn git_file_has_diff_uses_target_files_own_repo() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cwd_repo = tmp.path().join("cwd-repo");
        let file_repo = tmp.path().join("file-repo");
        std::fs::create_dir_all(&cwd_repo).expect("cwd repo dir");
        std::fs::create_dir_all(file_repo.join("src")).expect("file repo dir");
        if !init_repo_with_commit(&cwd_repo, "seed.txt") {
            return;
        }
        if !init_repo_with_commit(&file_repo, "src/app.rs") {
            return;
        }

        let target = file_repo.join("src/app.rs");
        std::fs::write(&target, "changed\n").expect("modify target");

        assert!(
            git_file_has_diff(
                Some(cwd_repo.to_str().expect("cwd utf8")),
                target.to_str().expect("target utf8")
            ),
            "absolute target file should be diffed from its own repo, not the hook cwd repo"
        );
    }

    #[test]
    fn agent_accepted_edit_request_captures_worktree_before_after() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join("src")).expect("repo dir");
        if !init_repo_with_commit(&repo, "src/app.rs") {
            return;
        }

        std::fs::write(repo.join("src/app.rs"), "changed\n").expect("modify target");

        let req = build_agent_accepted_edit_request(
            Some(repo.to_str().expect("repo utf8")),
            "src/app.rs",
            Some("acme/app"),
        )
        .expect("request");

        assert_eq!(req.before_code, "seed\n");
        assert_eq!(req.after_code, "changed\n");
        assert_eq!(req.file_path.as_deref(), Some("src/app.rs"));
        assert_eq!(req.repo_full_name.as_deref(), Some("acme/app"));
        assert_eq!(
            req.acceptance_source.as_deref(),
            Some(AGENT_ACCEPTANCE_SOURCE)
        );
        assert_eq!(req.client.as_deref(), Some(AGENT_ACCEPTANCE_CLIENT));
        assert_eq!(req.language.as_deref(), Some("rust"));
        assert!(req.diff_signature.is_some());
        assert!(req.rule_ids.is_empty());
    }

    #[test]
    fn agent_accepted_edit_request_returns_none_without_diff() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join("src")).expect("repo dir");
        if !init_repo_with_commit(&repo, "src/app.rs") {
            return;
        }

        assert!(
            build_agent_accepted_edit_request(
                Some(repo.to_str().expect("repo utf8")),
                "src/app.rs",
                Some("acme/app"),
            )
            .is_none()
        );
    }

    #[test]
    fn agent_accepted_edit_request_captures_staged_new_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join("src")).expect("repo dir");
        if !init_repo_with_commit(&repo, "src/app.rs") {
            return;
        }

        std::fs::write(repo.join("src/new.rs"), "pub fn new() {}\n").expect("new file");
        if !run_git(&repo, &["add", "src/new.rs"]) {
            return;
        }

        let req = build_agent_accepted_edit_request(
            Some(repo.to_str().expect("repo utf8")),
            "src/new.rs",
            Some("acme/app"),
        )
        .expect("request");

        assert_eq!(req.before_code, "");
        assert_eq!(req.after_code, "pub fn new() {}\n");
        assert_eq!(req.file_path.as_deref(), Some("src/new.rs"));
    }

    #[test]
    fn agent_accepted_edit_request_skips_oversized_code() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join("src")).expect("repo dir");
        if !init_repo_with_commit(&repo, "src/app.rs") {
            return;
        }

        std::fs::write(
            repo.join("src/app.rs"),
            "x".repeat(ACCEPTED_EDIT_MAX_CODE_BYTES + 1),
        )
        .expect("large file");

        assert!(
            build_agent_accepted_edit_request(
                Some(repo.to_str().expect("repo utf8")),
                "src/app.rs",
                Some("acme/app"),
            )
            .is_none()
        );
    }

    #[tokio::test]
    async fn resolve_hook_accepted_edit_rule_ids_prefers_cloud_mappings_and_keeps_local_ids() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("pool");
        sqlx::query("CREATE TABLE auth (key TEXT PRIMARY KEY, value TEXT)")
            .execute(&pool)
            .await
            .expect("auth");
        sqlx::query("CREATE TABLE skills (id TEXT PRIMARY KEY, cloud_id TEXT)")
            .execute(&pool)
            .await
            .expect("skills");
        sqlx::query("INSERT INTO skills (id, cloud_id) VALUES (?1, ?2)")
            .bind("local-rule")
            .bind("6105b2dd-5b7b-41a4-9af0-5e14c2b245fc")
            .execute(&pool)
            .await
            .expect("insert skill");

        let ids = resolve_hook_accepted_edit_rule_ids(
            &pool,
            &[
                "local-rule".to_owned(),
                "missing-rule".to_owned(),
                "6105b2dd-5b7b-41a4-9af0-5e14c2b245fc".to_owned(),
            ],
        )
        .await;

        assert_eq!(
            ids,
            vec!["6105b2dd-5b7b-41a4-9af0-5e14c2b245fc", "missing-rule"]
        );
    }

    #[test]
    fn rule_numbers_from_citation_text_respects_word_boundaries_and_guards() {
        // Substring inside a larger word must not match: "overrule 4" is not a
        // citation.
        assert!(
            rule_numbers_from_citation_text("overrule 4 was ignored").is_empty(),
            "must not match 'rule' inside 'overrule'"
        );
        // Plural "rules N" is not the singular citation form the hook
        // instructs ("applying Memory N"), so it is intentionally excluded.
        assert!(
            rule_numbers_from_citation_text("rules 7 apply here").is_empty(),
            "plural 'rules N' must not be counted"
        );
        // Zero is not a valid 1-based citation index.
        assert!(
            rule_numbers_from_citation_text("Rule 0 is bogus").is_empty(),
            "n must be > 0"
        );
        // Hash / backtick separators between the label and the number are
        // skipped, so the number is still captured.
        let nums = rule_numbers_from_citation_text("Applying Memory `5` and Rule #6.");
        assert!(nums.contains(&5), "backtick-wrapped number captured");
        assert!(nums.contains(&6), "hash-prefixed number captured");
        assert_eq!(nums.len(), 2);
    }

    fn init_repo_with_commit(repo: &std::path::Path, file: &str) -> bool {
        if !run_git(repo, &["init"]) {
            return false;
        }
        let _ = run_git(repo, &["config", "user.email", "test@example.com"]);
        let _ = run_git(repo, &["config", "user.name", "DiffLore Test"]);
        let path = repo.join(file);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("file parent");
        }
        std::fs::write(path, "seed\n").expect("seed file");
        run_git(repo, &["add", "-A"]) && run_git(repo, &["commit", "-m", "seed"])
    }

    fn run_git(repo: &std::path::Path, args: &[&str]) -> bool {
        difflore_core::infra::git::git_command(repo)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .ok()
            .is_some_and(|status| status.success())
    }
}
