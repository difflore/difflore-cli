use crate::{hook_cache, hook_forward, hooks};

use super::fire_log::remember_hook_fire_maybe_deferred;
use super::stated_vs_actual::read_last_assistant_text;

pub(crate) async fn hook_output_for_raw(
    client_name: &str,
    adapter: &dyn hooks::PlatformAdapter,
    raw: &str,
    debug: bool,
    defer_log: bool,
    hot_state: Option<&hook_forward::State>,
) -> anyhow::Result<String> {
    let event = match adapter.parse_stdin(raw) {
        Ok(ev) => ev,
        Err(e) => {
            if debug {
                eprintln!("[difflore.hook] parse error: {e}");
            }
            return Ok(adapter.format_output(hooks::types::HookResult::noop()));
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
    match dispatch_hook_event_with_state(event, hot_state).await {
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
            // recorded alongside the file path.
            remember_hook_fire_maybe_deferred(
                client_name.to_owned(),
                event_label.clone(),
                result.rules_injected,
                event_file_path.clone(),
                Some(trace_started.elapsed().as_millis() as i64),
                defer_log,
            );
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
                defer_log,
            );
            Err(err)
        }
    }
}

/// Translate a canonical `HookEvent` into the right `DiffLore` action and return
/// an adapter-agnostic `HookResult`. A free function so the dispatch logic is
/// unit-testable without threading stdio.
async fn dispatch_hook_event_with_state(
    event: hooks::types::HookEvent,
    hot_state: Option<&hook_forward::State>,
) -> anyhow::Result<hooks::types::HookResult> {
    use hooks::types::{HookEvent, HookResult};

    match event {
        HookEvent::PreToolUseRead { .. } => {
            // No pre-read injection: a Read is too weak a signal to predict
            // whether a rule applies (most reads are exploratory and never
            // produce an edit), so it paid full retrieval token cost for a
            // near-zero hit rate. Rule surfacing happens at PostToolUse, where
            // the actual diff is in hand.
            Ok(HookResult::noop())
        }
        HookEvent::PostToolUse {
            tool_name,
            file_path,
            diff,
            session_id,
            new_text,
            old_text,
        } => {
            // Act only on file-mutating tools: acting on Read/Bash would flood
            // the agent with irrelevant rule context for zero value.
            if !matches!(tool_name.as_str(), "Edit" | "Write" | "MultiEdit") {
                return Ok(HookResult::noop());
            }
            let Some(file) = file_path.clone() else {
                return Ok(HookResult::noop());
            };

            // Check the skip cache before DB init or outbox enqueue; repeated
            // PostToolUse events should stay off the hot path.
            if hook_cache::should_skip_recent(&file, "post-edit") {
                return Ok(HookResult::noop());
            }

            let db = if let Some(state) = hot_state {
                state.db.clone()
            } else {
                match difflore_core::infra::db::init_db().await {
                    Ok(p) => p,
                    Err(_) => return Ok(HookResult::noop()),
                }
            };
            let index_pool = if let Some(state) = hot_state {
                state.index_pool.clone()
            } else {
                match difflore_core::context::index_db::get_pool_for_cwd().await {
                    Ok(p) => p,
                    Err(_) => return Ok(HookResult::noop()),
                }
            };

            maybe_emit_rule_cited_in_edit(
                &db,
                session_id.as_deref(),
                &file,
                diff.as_deref().or(new_text.as_deref()),
            )
            .await;

            // Classify this edit into a structured observation and enqueue via
            // the outbox. Failures are swallowed: observation capture must never
            // affect the rule-injection hook output.
            let obs_input = difflore_core::observability::classifier::ClassifyInput {
                tool: &tool_name,
                file_path: Some(&file),
                diff: diff.as_deref(),
                new_text: new_text.as_deref(),
                old_text: old_text.as_deref(),
                session_id: session_id.as_deref(),
                ts_ms: None,
            };
            if let Some(obs) = difflore_core::observability::classifier::classify(&obs_input) {
                let queue = difflore_core::cloud::outbox::OutboxQueue::new(db.clone());
                match serde_json::to_string(&obs) {
                    Ok(payload) => {
                        if let Err(e) = queue
                            .enqueue(difflore_core::cloud::outbox::kind::OBSERVATION, &payload)
                            .await
                        {
                            if difflore_core::infra::env::debug_telemetry() {
                                eprintln!("[difflore.hook] observation enqueue failed: {e}");
                            }
                        }
                    }
                    Err(e) => {
                        if difflore_core::infra::env::debug_telemetry() {
                            eprintln!("[difflore.hook] observation serialize failed: {e}");
                        }
                    }
                }
            }

            let retrieval_intent = post_edit_retrieval_intent(diff.as_deref(), new_text.as_deref());
            match difflore_core::mcp_server::fetch_relevant_rules_for_hook(
                &db,
                &index_pool,
                &file,
                &retrieval_intent,
                session_id.as_deref(),
            )
            .await
            {
                Ok(ctx) if ctx.rules_injected > 0 => {
                    hook_cache::remember_injection(&file, "post-edit", ctx.rules_injected);
                    // No unconditional system_message: the assistant's citation
                    // of "Rule N" in its reply is the visible signal. Surfacing
                    // "injected N rules" on every Edit pollutes the user's view
                    // even when none of the rules applied.
                    let mut result = HookResult::with_context(ctx.rendered);
                    result.rules_injected = Some(ctx.rules_injected);
                    Ok(result)
                }
                _ => Ok(HookResult::noop()),
            }
        }
        HookEvent::UserPromptSubmit { .. } => Ok(HookResult::noop()),
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
            maybe_emit_rule_actual_citations(session_id.as_deref(), transcript_path.as_deref())
                .await;
            let _accepted_count =
                maybe_emit_fix_outcomes(session_id.as_deref(), cwd.as_deref()).await;
            // Keep lifecycle hooks quiet: hosts render `systemMessage` as
            // event-name chatter that reads like an internal tool speaking.
            Ok(HookResult::noop())
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
                    Err(_) => return Ok(HookResult::noop()),
                }
            };
            let _ =
                difflore_core::context::orchestrator::ensure_cross_repo_starter_indexed(&db).await;

            // Since-last-session recap: if this repo gained rules since the last
            // SessionStart, surface a short note via `additional_context`. The
            // helper is self-budgeted and returns `None` on quiet sessions.
            //
            // `client_name` isn't threaded into the dispatcher; the banner uses
            // it only for the watermark's debug trail, so a generic label is fine.
            let banner_ctx = hooks::session_banner::BannerContext {
                cwd,
                client_name: "agent".to_owned(),
            };
            if let Some(banner) =
                hooks::session_banner::render_since_last_session_banner(&banner_ctx).await
            {
                return Ok(HookResult::with_context(banner));
            }
            Ok(HookResult::noop())
        }
    }
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

    let cited_numbers = rule_numbers_from_citation_text(&text);
    let mentions_learned_from = text.to_ascii_lowercase().contains("learned from");
    if cited_numbers.is_empty() && !mentions_learned_from {
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

    let mut rule_ids = rule_ids_for_citation_numbers(&rule_fire.rule_ids, &cited_numbers);
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

    let mut by_rule: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for edit in cited {
        by_rule
            .entry(edit.rule_id)
            .or_default()
            .push(edit.file_path);
    }

    let detected_repos = difflore_core::infra::git::detect_github_repo_full_names(cwd.unwrap_or("."));
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
        let accepted = files.iter().any(|file| git_file_has_diff(cwd, file));
        if accepted {
            accepted_count += 1;
        }
        let occurred_at = chrono::Utc::now();
        let file_path = accepted_file_path(cwd, &files);
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

fn accepted_file_path(cwd: Option<&str>, files: &[String]) -> Option<String> {
    files
        .iter()
        .find(|file| git_file_has_diff(cwd, file))
        .or_else(|| files.first())
        .cloned()
}

fn git_file_has_diff(cwd: Option<&str>, file_path: &str) -> bool {
    let cwd = cwd.unwrap_or(".");
    git_quiet_diff_has_changes(cwd, &["diff", "--quiet", "--", file_path])
        || git_quiet_diff_has_changes(cwd, &["diff", "--cached", "--quiet", "--", file_path])
}

fn git_quiet_diff_has_changes(cwd: &str, args: &[&str]) -> bool {
    std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
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

const fn hook_event_label(event: &hooks::types::HookEvent) -> &'static str {
    match event {
        hooks::types::HookEvent::PostToolUse { .. } => "post_tool_use",
        hooks::types::HookEvent::PreToolUseRead { .. } => "pre_tool_use_read",
        hooks::types::HookEvent::SessionStart { .. } => "session_start",
        hooks::types::HookEvent::UserPromptSubmit { .. } => "user_prompt_submit",
        hooks::types::HookEvent::Stop { .. } => "stop",
        hooks::types::HookEvent::SessionEnd { .. } => "session_end",
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
}
