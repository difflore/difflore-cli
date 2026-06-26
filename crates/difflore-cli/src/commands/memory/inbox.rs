use std::io::{self, BufRead, IsTerminal, Read, Write};

use difflore_core::domain::models::RememberRuleInput;
use serde_json::json;

use difflore_core::memory_autopilot::promote_candidate_with_curator_recommendation;
use difflore_core::memory_autopilot_schedule::{
    MemoryAutopilotScheduleStatus, load_autopilot_schedule_status,
};
use difflore_core::memory_inbox::{
    MemoryActivity, MemoryActivityFilter, MemoryInbox, MemoryList, MemoryListFilter,
    MemoryListItem, MemoryRuleItem, OutboxQueueCount, SessionMinedDiscovery,
    approve_session_mined_candidate, find_session_mined_by_content_hash, load_memory_activity,
    load_memory_inbox, load_memory_inbox_default, load_memory_items, parse_session_item_id,
    reject_session_mined_candidate,
};
use difflore_core::skills::{CandidateRule, list_candidates, reject_candidate};

use crate::runtime::CommandContext;
use crate::style;
use crate::support::util::{exit_err, json_compact_or};

use super::types::{MemoryCloudSummary, MemoryInboxOutput, MemoryNextAction, MemorySummaryOutput};
use super::{count_phrase, exit_structured_err, plural};

const ACTIVE_BACKFILL_LIMIT: usize = 1_000;

pub(crate) async fn handle_summary(ctx: &CommandContext, json: bool) {
    let inbox = load_inbox(ctx, json).await;
    let autopilot = load_autopilot_status(ctx, json).await;
    let cloud = MemoryCloudSummary::load().await;
    let next = next_action(&inbox, &cloud);

    if json {
        let output = MemorySummaryOutput::from_parts(&inbox, autopilot, cloud, next);
        println!("{}", json_compact_or(&output, "{}"));
        return;
    }

    print_summary(&inbox, &autopilot, &cloud, &next);
}

pub(crate) async fn handle_inbox(
    ctx: &CommandContext,
    all: bool,
    limit: Option<usize>,
    json: bool,
) {
    let inbox = load_inbox_with_display_limit(ctx, all, limit, json).await;
    let autopilot = load_autopilot_status(ctx, json).await;
    let cloud = MemoryCloudSummary::load().await;
    let next = next_action(&inbox, &cloud);

    if json {
        let output = MemoryInboxOutput::from_parts(&inbox, autopilot, cloud, next);
        println!("{}", json_compact_or(&output, "{}"));
        return;
    }

    print_inbox(&inbox, &autopilot, &cloud, &next);
}

pub(crate) async fn handle_active(
    ctx: &CommandContext,
    all: bool,
    limit: Option<usize>,
    json: bool,
) {
    let display_limit = limit.unwrap_or(50);
    let mut memory = load_memory_items(
        &ctx.db,
        MemoryListFilter {
            state: Some("active".to_owned()),
            kind: Some("rule".to_owned()),
            repo_full_name: None,
            query: None,
            limit: if all {
                display_limit
            } else {
                ACTIVE_BACKFILL_LIMIT
            },
        },
    )
    .await
    .unwrap_or_else(|err| {
        exit_structured_err(&format!("failed to load active memory: {err}"), json)
    });
    if !all {
        let current_repo_keys = current_repo_scope_keys(ctx).await;
        filter_active_to_current_repo(&mut memory, &current_repo_keys, display_limit);
    }

    if json {
        println!("{}", json_compact_or(&memory, "{}"));
        return;
    }

    print_active(&memory, all);
}

pub(crate) async fn handle_activity(
    ctx: &CommandContext,
    days: i64,
    limit: Option<usize>,
    json: bool,
) {
    let activity = load_memory_activity(
        &ctx.db,
        MemoryActivityFilter {
            rule_id: None,
            repo_full_name: None,
            days,
            limit: limit.unwrap_or(20),
        },
    )
    .await
    .unwrap_or_else(|err| {
        exit_structured_err(&format!("failed to load memory activity: {err}"), json)
    });

    if json {
        println!("{}", json_compact_or(&activity, "{}"));
        return;
    }

    print_activity(&activity);
}

pub(crate) async fn handle_show(ctx: &CommandContext, item_id: String, json: bool) {
    if item_id.trim().starts_with("rule:") {
        let detail = difflore_core::memory_inbox::get_memory_item(&ctx.db, &item_id)
            .await
            .unwrap_or_else(|err| {
                exit_structured_err(&format!("failed to load active memory rule: {err}"), json)
            })
            .unwrap_or_else(|| {
                exit_structured_err(&format!("memory item `{item_id}` not found"), json)
            });
        if json {
            println!("{}", json_compact_or(&detail, "{}"));
            return;
        }
        print_memory_detail(&detail);
        return;
    }

    if let Some(draft_id) = item_id.strip_prefix("draft:") {
        let draft = load_draft(ctx, draft_id, json).await;
        if json {
            println!(
                "{}",
                json_compact_or(&json!({ "kind": "draft", "draft": draft }), "{}")
            );
            return;
        }
        print_draft(&draft);
        return;
    }

    let content_hash = parse_session_item_id(&item_id)
        .unwrap_or_else(|err| exit_structured_err(&err.to_string(), json));
    let discovery = find_session_mined_by_content_hash(&ctx.db, &content_hash)
        .await
        .unwrap_or_else(|err| {
            exit_structured_err(
                &format!("failed to load session-mined discovery: {err}"),
                json,
            )
        })
        .unwrap_or_else(|| {
            exit_structured_err(&format!("memory inbox item `{item_id}` not found"), json)
        });

    if json {
        println!(
            "{}",
            json_compact_or(
                &json!({ "kind": "sessionMinedDiscovery", "item": discovery }),
                "{}"
            )
        );
        return;
    }

    print_session_discovery(&discovery);
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_remember(
    ctx: &CommandContext,
    title: String,
    body: Option<String>,
    file_patterns: Vec<String>,
    bad_code: Option<String>,
    good_code: Option<String>,
    severity: Option<String>,
    json: bool,
) {
    let title = title.trim().to_owned();
    if title.is_empty() {
        exit_structured_err("memory remember requires a non-empty --title", json);
    }
    let body = match body {
        Some(value) => value,
        None => read_remember_body_from_stdin(json),
    };
    let body = body.trim().to_owned();
    if body.is_empty() {
        exit_structured_err(
            "memory remember requires --body or a non-empty stdin body",
            json,
        );
    }

    let file_patterns = file_patterns
        .into_iter()
        .map(|pattern| pattern.trim().to_owned())
        .filter(|pattern| !pattern.is_empty())
        .collect::<Vec<_>>();

    let input = RememberRuleInput {
        title,
        body,
        file_patterns: if file_patterns.is_empty() {
            None
        } else {
            Some(file_patterns)
        },
        bad_code: non_empty_owned(bad_code),
        good_code: non_empty_owned(good_code),
        severity: non_empty_owned(severity),
        kind: None,
        category: None,
        origin: Some("conversation".to_owned()),
        captured_by_client: Some("cli".to_owned()),
    };

    let outcome = difflore_core::skills::remember(&ctx.db, input)
        .await
        .unwrap_or_else(|err| {
            exit_structured_err(&format!("failed to save memory rule: {err}"), json)
        });
    let source_repo = attach_current_repo_scope(ctx, &outcome.skill.id, json).await;
    let item_id = format!("rule:{}", outcome.skill.id);
    let show_command = format!("difflore memory show {item_id}");
    let disable_command = format!("difflore memory disable {item_id}");

    if json {
        println!(
            "{}",
            json_compact_or(
                &json!({
                    "itemId": item_id,
                    "id": outcome.skill.id,
                    "state": "active",
                    "active": true,
                    "servedToAgents": true,
                    "requiresUserApproval": false,
                    "autoApproved": true,
                    "deduped": outcome.deduped,
                    "dedupWindowHit": outcome.dedup_window_hit,
                    "confidence": outcome.confidence_after,
                    "capturesToday": outcome.captures_today,
                    "sourceRepo": source_repo,
                    "commands": {
                        "show": show_command,
                        "disable": disable_command,
                    }
                }),
                "{}"
            )
        );
        return;
    }

    println!("{}", style::title("Memory Rule Saved"));
    println!("  id: {}", style::ident(&item_id));
    println!("  active: yes (user request treated as approval)");
    println!("  source_repo: {}", source_repo.as_deref().unwrap_or("-"));
    if outcome.deduped {
        println!("  note: strengthened an existing active rule");
    }
    println!();
    println!("  inspect: {}", style::cmd(&show_command));
    println!("  disable: {}", style::cmd(&disable_command));
}

pub(crate) async fn handle_review(ctx: &CommandContext, limit: Option<usize>) {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        exit_err(
            "interactive memory review requires a terminal. Use `difflore memory inbox`, \
             `difflore memory approve <item-id>`, or \
             `difflore memory reject <item-id>`.",
        );
    }

    let max_items = limit.unwrap_or(50);
    if max_items == 0 {
        println!("No memory items selected; pass --limit greater than 0.");
        return;
    }
    let counts = load_memory_inbox(&ctx.db, 1).await.unwrap_or_else(|err| {
        exit_structured_err(&format!("failed to load local memory inbox: {err}"), false)
    });
    let draft_pending = count_to_usize(counts.local_draft_count());
    let session_pending = count_to_usize(counts.session_mined_count());
    let total_pending = draft_pending.saturating_add(session_pending);

    let drafts = load_drafts(ctx, Some(max_items), false).await;
    let remaining = max_items.saturating_sub(drafts.len());
    let review_inbox = if remaining > 0 {
        Some(
            load_memory_inbox(&ctx.db, remaining)
                .await
                .unwrap_or_else(|err| {
                    exit_structured_err(&format!("failed to load local memory inbox: {err}"), false)
                }),
        )
    } else {
        None
    };
    let discoveries = review_inbox
        .map(|inbox| inbox.local_discoveries.latest)
        .unwrap_or_default();
    let reviewing = drafts.len() + discoveries.len();
    if reviewing == 0 {
        if session_pending > 0 {
            println!("Candidate memory rows exist, but none could be displayed. Run:");
            println!("  {}", style::cmd("difflore memory inbox"));
        } else {
            println!("No memory items waiting for local review.");
        }
        return;
    }

    println!("{}", review_progress_heading(reviewing, total_pending));
    println!();
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    for (idx, draft) in drafts.iter().enumerate() {
        println!(
            "{} of {}",
            style::pewter(&(idx + 1).to_string()),
            style::pewter(&reviewing.to_string())
        );
        print_draft_review_summary(draft);

        loop {
            print!(
                "  action [{} approve local, {} reject, {} skip, {} view, {} quit]: ",
                style::cmd("a"),
                style::cmd("r"),
                style::cmd("s"),
                style::cmd("v"),
                style::cmd("q"),
            );
            flush_stdout();

            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                exit_err("failed to read memory review choice");
            }
            let item_id = format!("draft:{}", draft.id);
            match line.trim().to_ascii_lowercase().as_str() {
                "a" | "approve" => {
                    handle_approve(ctx, item_id, false).await;
                    break;
                }
                "r" | "reject" => {
                    handle_reject(ctx, item_id, false).await;
                    break;
                }
                "s" | "skip" | "" => {
                    println!("  skipped draft:{}\n", draft.id);
                    break;
                }
                "v" | "view" => print_draft(draft),
                "q" | "quit" => {
                    println!("Stopped with remaining memory items still pending.");
                    return;
                }
                _ => println!("  enter a, r, s, v, or q."),
            }
        }
    }

    for (idx, discovery) in discoveries.iter().enumerate() {
        println!(
            "{} of {}",
            style::pewter(&(drafts.len() + idx + 1).to_string()),
            style::pewter(&reviewing.to_string())
        );
        print_session_discovery_summary(discovery);

        loop {
            print!(
                "  action [{} approve local, {} reject, {} skip, {} view, {} quit]: ",
                style::cmd("a"),
                style::cmd("r"),
                style::cmd("s"),
                style::cmd("v"),
                style::cmd("q"),
            );
            flush_stdout();

            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                exit_err("failed to read memory review choice");
            }
            match line.trim().to_ascii_lowercase().as_str() {
                "a" | "approve" => {
                    approve_session_item(ctx, &discovery.item_id, false).await;
                    break;
                }
                "r" | "reject" => {
                    reject_session_item(ctx, &discovery.item_id, false).await;
                    break;
                }
                "s" | "skip" | "" => {
                    println!("  skipped {}\n", discovery.item_id);
                    break;
                }
                "v" | "view" => print_session_discovery(discovery),
                "q" | "quit" => {
                    println!("Stopped with remaining memory items still pending.");
                    return;
                }
                _ => println!("  enter a, r, s, v, or q."),
            }
        }
    }
}

pub(crate) async fn handle_approve(ctx: &CommandContext, item_id: String, json: bool) {
    if let Some(draft_id) = item_id.strip_prefix("draft:") {
        let activated = promote_candidate_with_curator_recommendation(&ctx.db, draft_id)
            .await
            .unwrap_or_else(|err| exit_structured_err(&err.to_string(), json));
        if json {
            println!(
                "{}",
                json_compact_or(
                    &json!({
                        "action": "approved",
                        "kind": "draft",
                        "itemId": item_id,
                        "ruleId": activated.id,
                        "cloudRequired": false,
                    }),
                    "{}"
                )
            );
        } else {
            println!(
                "{} Approved local memory draft {} into active rule {}.",
                style::ok(style::sym::OK),
                style::ident(draft_id),
                style::ident(&activated.id)
            );
        }
        return;
    }
    approve_session_item(ctx, &item_id, json).await;
}

pub(crate) async fn handle_reject(ctx: &CommandContext, item_id: String, json: bool) {
    if let Some(draft_id) = item_id.strip_prefix("draft:") {
        reject_candidate(&ctx.db, draft_id)
            .await
            .unwrap_or_else(|err| exit_structured_err(&err.to_string(), json));
        if json {
            println!(
                "{}",
                json_compact_or(
                    &json!({
                        "action": "rejected",
                        "kind": "draft",
                        "itemId": item_id,
                        "cloudRequired": false,
                    }),
                    "{}"
                )
            );
        } else {
            println!(
                "{} Rejected local memory draft {}; it will not be shared.",
                style::ok(style::sym::OK),
                style::ident(draft_id)
            );
        }
        return;
    }
    reject_session_item(ctx, &item_id, json).await;
}

async fn load_inbox(ctx: &CommandContext, json: bool) -> MemoryInbox {
    let inbox = load_memory_inbox_default(&ctx.db)
        .await
        .unwrap_or_else(|err| {
            exit_structured_err(&format!("failed to load local memory inbox: {err}"), json)
        });
    prioritize_current_repo_inbox(ctx, inbox, json).await
}

async fn load_autopilot_status(ctx: &CommandContext, json: bool) -> MemoryAutopilotScheduleStatus {
    load_autopilot_schedule_status(&ctx.db)
        .await
        .unwrap_or_else(|err| {
            exit_structured_err(
                &format!("failed to load memory autopilot status: {err}"),
                json,
            )
        })
}

async fn load_inbox_with_display_limit(
    ctx: &CommandContext,
    all: bool,
    limit: Option<usize>,
    json: bool,
) -> MemoryInbox {
    if all {
        let counts = load_inbox(ctx, json).await;
        let max_count = [
            counts.active_rule_count(),
            counts.local_draft_count(),
            counts.session_mined_count(),
        ]
        .into_iter()
        .max()
        .unwrap_or(0);
        let limit = usize::try_from(max_count).unwrap_or(usize::MAX).max(1);
        return load_memory_inbox_for_display(ctx, limit, json).await;
    }

    match limit {
        Some(limit) => load_memory_inbox_for_display(ctx, limit, json).await,
        None => load_inbox(ctx, json).await,
    }
}

async fn load_memory_inbox_for_display(
    ctx: &CommandContext,
    limit: usize,
    json: bool,
) -> MemoryInbox {
    let inbox = load_memory_inbox(&ctx.db, limit)
        .await
        .unwrap_or_else(|err| {
            exit_structured_err(&format!("failed to load local memory inbox: {err}"), json)
        });
    prioritize_current_repo_inbox(ctx, inbox, json).await
}

async fn prioritize_current_repo_inbox(
    ctx: &CommandContext,
    mut inbox: MemoryInbox,
    json: bool,
) -> MemoryInbox {
    let current_repo_keys = current_repo_scope_keys(ctx).await;
    if current_repo_keys.is_empty() {
        return inbox;
    }
    let display_limit = inbox_display_limit(&inbox);
    let backfill_limit = inbox_backfill_limit(&inbox);
    if backfill_limit > display_limit {
        inbox = load_memory_inbox(&ctx.db, backfill_limit)
            .await
            .unwrap_or_else(|err| {
                exit_structured_err(&format!("failed to load local memory inbox: {err}"), json)
            });
    }
    prioritize_current_repo_rule_items(&mut inbox.active_rules.latest, &current_repo_keys);
    prioritize_current_repo_rule_items(&mut inbox.local_drafts.latest, &current_repo_keys);
    truncate_inbox_display(&mut inbox, display_limit);
    inbox
}

fn inbox_display_limit(inbox: &MemoryInbox) -> usize {
    [
        inbox.active_rules.latest.len(),
        inbox.local_drafts.latest.len(),
        inbox.local_discoveries.latest.len(),
    ]
    .into_iter()
    .max()
    .unwrap_or(0)
}

fn inbox_backfill_limit(inbox: &MemoryInbox) -> usize {
    [
        inbox.active_rule_count(),
        inbox.local_draft_count(),
        inbox.session_mined_count(),
    ]
    .into_iter()
    .filter_map(|count| usize::try_from(count).ok())
    .max()
    .unwrap_or(0)
}

fn truncate_inbox_display(inbox: &mut MemoryInbox, display_limit: usize) {
    inbox.active_rules.latest.truncate(display_limit);
    inbox.local_drafts.latest.truncate(display_limit);
    inbox.local_discoveries.latest.truncate(display_limit);
}

async fn current_repo_scope_keys(ctx: &CommandContext) -> std::collections::HashSet<String> {
    let configured_gitlab_hosts = difflore_core::ingest::gitlab::auth::configured_hosts().await;
    let project = ctx.project.to_string_lossy();
    let detected_repo_remotes = difflore_core::infra::git::detect_repo_full_names_with_gitlab_hosts(
        project.as_ref(),
        &configured_gitlab_hosts,
    );
    let repo_scopes = difflore_core::skills::expand_repo_scopes_with_source_aliases(
        &ctx.db,
        &detected_repo_remotes,
    )
    .await
    .unwrap_or(detected_repo_remotes);
    repo_scope_keys(&repo_scopes)
}

fn repo_scope_keys(scopes: &[String]) -> std::collections::HashSet<String> {
    let mut keys = std::collections::HashSet::new();
    for scope in scopes {
        let normalized = normalize_repo_key(scope);
        if !normalized.is_empty() {
            keys.insert(normalized);
        }
        if let Some(canonical) = difflore_core::infra::git::RepoScope::canonical(scope) {
            keys.insert(normalize_repo_key(canonical.as_str()));
        }
    }
    keys
}

fn prioritize_current_repo_rule_items(
    items: &mut [MemoryRuleItem],
    current_repo_keys: &std::collections::HashSet<String>,
) {
    items.sort_by_key(|item| {
        i32::from(
            !item
                .source_repo
                .as_deref()
                .map(normalize_repo_key)
                .is_some_and(|source| current_repo_keys.contains(&source)),
        )
    });
}

fn filter_active_to_current_repo(
    memory: &mut MemoryList,
    current_repo_keys: &std::collections::HashSet<String>,
    display_limit: usize,
) {
    if current_repo_keys.is_empty() {
        memory.items.clear();
        memory.counts.active = 0;
        return;
    }

    memory.items.retain(|item| {
        item.source_repo
            .as_deref()
            .map(normalize_repo_key)
            .is_some_and(|source| current_repo_keys.contains(&source))
    });
    memory.counts.active = i64::try_from(memory.items.len()).unwrap_or(i64::MAX);
    memory.items.truncate(display_limit);
}

fn normalize_repo_key(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

async fn load_draft(ctx: &CommandContext, id: &str, json: bool) -> CandidateRule {
    list_candidates(&ctx.db, None, None)
        .await
        .unwrap_or_else(|err| {
            exit_structured_err(
                &format!("failed to list pending memory drafts: {err}"),
                json,
            )
        })
        .into_iter()
        .find(|draft| draft.id == id)
        .unwrap_or_else(|| exit_structured_err(&format!("memory draft `{id}` not found"), json))
}

async fn load_drafts(ctx: &CommandContext, limit: Option<usize>, json: bool) -> Vec<CandidateRule> {
    list_candidates(&ctx.db, None, limit)
        .await
        .unwrap_or_else(|err| {
            exit_structured_err(
                &format!("failed to list pending memory drafts: {err}"),
                json,
            )
        })
}

fn read_remember_body_from_stdin(json: bool) -> String {
    if io::stdin().is_terminal() {
        exit_structured_err(
            "memory remember requires --body when stdin is a terminal",
            json,
        );
    }
    let mut body = String::new();
    io::stdin().read_to_string(&mut body).unwrap_or_else(|err| {
        exit_structured_err(
            &format!("failed to read memory body from stdin: {err}"),
            json,
        )
    });
    body
}

fn non_empty_owned(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

async fn attach_current_repo_scope(
    ctx: &CommandContext,
    rule_id: &str,
    json: bool,
) -> Option<String> {
    let configured_gitlab_hosts = difflore_core::ingest::gitlab::auth::configured_hosts().await;
    let project = ctx.project.to_string_lossy();
    let detected_repo_remotes = difflore_core::infra::git::detect_repo_full_names_with_gitlab_hosts(
        project.as_ref(),
        &configured_gitlab_hosts,
    );
    let repo_scope = detected_repo_remotes
        .first()
        .map(String::as_str)
        .and_then(difflore_core::infra::git::RepoScope::canonical)?;
    let repo_full_name = repo_scope.as_str().to_owned();
    sqlx::query(
        "UPDATE skills
         SET source_repo = CASE
             WHEN source_repo IS NULL OR trim(source_repo) = '' THEN ?1
             ELSE source_repo
         END
         WHERE id = ?2",
    )
    .bind(&repo_full_name)
    .bind(rule_id)
    .execute(&ctx.db)
    .await
    .unwrap_or_else(|err| {
        exit_structured_err(
            &format!("failed to attach repo scope to memory rule: {err}"),
            json,
        )
    });
    Some(repo_full_name)
}

async fn approve_session_item(ctx: &CommandContext, item_id: &str, json: bool) {
    let content_hash = parse_session_item_id(item_id)
        .unwrap_or_else(|err| exit_structured_err(&err.to_string(), json));
    let approved = approve_session_mined_candidate(&ctx.db, &content_hash)
        .await
        .unwrap_or_else(|err| exit_structured_err(&err.to_string(), json));

    if json {
        println!(
            "{}",
            json_compact_or(
                &json!({
                    "action": "approved",
                    "kind": "sessionMinedCandidate",
                    "itemId": approved.item_id,
                    "ruleId": approved.rule.id,
                    "deduped": approved.deduped,
                    "confidenceAfter": approved.confidence_after,
                    "cloudRequired": false,
                }),
                "{}"
            )
        );
        return;
    }

    println!(
        "  {} approved {} into active local rule {}",
        style::ok(style::sym::OK),
        style::ident(&approved.item_id),
        style::ident(&approved.rule.id)
    );
    println!("  available now to local agents; cloud sync is optional\n");
}

async fn reject_session_item(ctx: &CommandContext, item_id: &str, json: bool) {
    let content_hash = parse_session_item_id(item_id)
        .unwrap_or_else(|err| exit_structured_err(&err.to_string(), json));
    let rejected = reject_session_mined_candidate(&ctx.db, &content_hash)
        .await
        .unwrap_or_else(|err| exit_structured_err(&err.to_string(), json));

    if json {
        println!(
            "{}",
            json_compact_or(
                &json!({
                    "action": "rejected",
                    "kind": "sessionMinedCandidate",
                    "itemId": rejected.item_id,
                    "cloudRequired": false,
                }),
                "{}"
            )
        );
        return;
    }

    println!(
        "  {} rejected {} locally; it will not be shared\n",
        style::ok(style::sym::OK),
        style::ident(&rejected.item_id)
    );
}

fn next_action(inbox: &MemoryInbox, _cloud: &MemoryCloudSummary) -> MemoryNextAction {
    if inbox.local_draft_count() > 0 {
        return MemoryNextAction::new(
            "difflore memory review",
            "review pending local drafts; background autopilot handles high-confidence items automatically",
        );
    }

    if inbox.session_mined_count() > 0 {
        return MemoryNextAction::new(
            "difflore memory review",
            "review candidate memories before they become active local rules",
        );
    }

    if inbox.active_rule_count() == 0 {
        return MemoryNextAction::new(
            "difflore import-reviews",
            "no active local memory is available to agents",
        );
    }

    MemoryNextAction::new(
        "difflore recall --diff",
        "active memory is available to preview against your current diff",
    )
}

fn print_summary(
    inbox: &MemoryInbox,
    autopilot: &MemoryAutopilotScheduleStatus,
    cloud: &MemoryCloudSummary,
    next: &MemoryNextAction,
) {
    println!("{}", style::title("Memory"));
    println!(
        "  active rules       {} available to agents",
        style::ident(&inbox.active_rule_count().to_string())
    );
    println!(
        "  local drafts       {} pending local review {}",
        style::ident(&inbox.local_draft_count().to_string()),
        plural(inbox.local_draft_count(), "draft", "drafts")
    );
    println!(
        "  candidate memories {} found from recent agent sessions (not active yet)",
        style::ident(&inbox.session_mined_count().to_string()),
    );
    println!(
        "  autopilot          {} | runs {} ({} useful) | triggers {}",
        if autopilot.enabled { "on" } else { "off" },
        style::ident(&autopilot.run_count.to_string()),
        style::ident(&autopilot.productive_run_count.to_string()),
        style::ident(&autopilot.trigger_count.to_string())
    );
    if inbox.session_mined_count() > 0 {
        println!(
            "  inspect            {}",
            style::cmd("difflore memory inbox")
        );
        if let Some(discovery) = inbox.local_discoveries.latest.first() {
            println!(
                "                     {}",
                style::cmd(&format!("difflore memory show {}", discovery.item_id))
            );
        }
        println!("  background log     {}", style::cmd("difflore memory log"));
        println!(
            "  review one         {}",
            style::cmd("difflore memory approve session:<content_hash>")
        );
        println!(
            "  team sync          {} optional after local approval",
            style::cmd("difflore memory sync")
        );
    } else if inbox.local_draft_count() > 0 {
        println!(
            "  review queue       {}",
            style::cmd("difflore memory review")
        );
        if let Some(draft) = inbox.local_drafts.latest.first() {
            println!(
                "                     {}",
                style::cmd(&format!("difflore memory approve draft:{}", draft.id))
            );
        }
    }

    let queue_line = compact_queue_line(inbox);
    if !queue_line.is_empty() {
        println!("  optional sync      {queue_line}");
    }
    print_team_line(cloud);
    println!(
        "  usage              served to agents {} locally",
        style::ident(&inbox.usage.local_agent_serves.to_string())
    );
    println!();
    println!("  next: {}", style::cmd(&next.command));
    println!("        {}", style::pewter(&next.reason));
}

fn print_inbox(
    inbox: &MemoryInbox,
    autopilot: &MemoryAutopilotScheduleStatus,
    cloud: &MemoryCloudSummary,
    next: &MemoryNextAction,
) {
    println!("{}", style::title("Memory Inbox"));
    println!(
        "  autopilot {} | runs {} ({} useful) | triggers {}",
        if autopilot.enabled { "on" } else { "off" },
        style::ident(&autopilot.run_count.to_string()),
        style::ident(&autopilot.productive_run_count.to_string()),
        style::ident(&autopilot.trigger_count.to_string())
    );
    println!();

    println!("{}", style::title("Active rules"));
    println!(
        "  {} available to agents",
        count_phrase(inbox.active_rule_count(), "rule", "rules")
    );
    print_rule_items(&inbox.active_rules.latest);
    println!();

    println!("{}", style::title("Local drafts"));
    println!(
        "  {} pending local review {}",
        inbox.local_draft_count(),
        plural(inbox.local_draft_count(), "draft", "drafts")
    );
    print_rule_items(&inbox.local_drafts.latest);
    if inbox.local_draft_count() > 0 {
        println!("  background log: {}", style::cmd("difflore memory log"));
        println!(
            "  review remaining: {}",
            style::cmd("difflore memory review")
        );
        println!(
            "  approve one: {}",
            style::cmd("difflore memory approve draft:<id>")
        );
        println!(
            "  reject one: {}",
            style::cmd("difflore memory reject draft:<id>")
        );
    }
    println!();

    println!("{}", style::title("Candidate memories"));
    println!(
        "  {} found from recent agent sessions (not active yet)",
        count_phrase(
            inbox.session_mined_count(),
            "candidate memory",
            "candidate memories"
        )
    );
    if inbox.session_mined_count() > 0 {
        println!("  Agents cannot use these until you approve them locally.");
    }
    for discovery in &inbox.local_discoveries.latest {
        print_session_discovery_summary(discovery);
    }
    if inbox.session_mined_count() > 0 {
        println!();
        println!(
            "  inspect one: {}",
            style::cmd("difflore memory show session:<content_hash>")
        );
        println!("  background log: {}", style::cmd("difflore memory log"));
        println!(
            "  review remaining: {}",
            style::cmd("difflore memory review")
        );
        println!(
            "  approve one: {}",
            style::cmd("difflore memory approve session:<content_hash>")
        );
        println!(
            "  reject one: {}",
            style::cmd("difflore memory reject session:<content_hash>")
        );
        println!(
            "  team sync: {} optional",
            style::cmd("difflore memory sync")
        );
    }
    println!();

    println!("{}", style::title("Optional sync queues"));
    print_cloud_outbox_counts(&inbox.queues.cloud_outbox);
    for count in &inbox.queues.observations_outbox {
        println!(
            "  {} {} {}",
            count.count,
            count.status,
            observation_label(&count.event_type)
        );
    }
    if inbox.queues.cloud_outbox.is_empty() && inbox.queues.observations_outbox.is_empty() {
        println!("  no pending optional sync rows");
    }
    println!();

    println!("{}", style::title("Team review"));
    print_team_status(cloud, false);
    println!();

    if !inbox.warnings.is_empty() {
        println!("{}", style::title("Blockers"));
        for warning in &inbox.warnings {
            println!("  {} {}", style::warn(style::sym::WARN), warning.message);
        }
        println!();
    }

    println!("{}", style::title("Usage"));
    println!(
        "  served to agents {} times locally",
        inbox.usage.local_agent_serves
    );
    println!(
        "  latest proof lives in {}",
        style::cmd(&inbox.usage.proof_surface)
    );
    println!();

    println!("next: {}", style::cmd(&next.command));
    println!("      {}", style::pewter(&next.reason));
}

fn print_active(memory: &MemoryList, all: bool) {
    println!("{}", style::title("Active Memory"));
    println!(
        "  scope              {}",
        if all {
            style::pewter("all repos").to_string()
        } else {
            style::pewter("current repo").to_string()
        }
    );
    println!(
        "  {} available to agents",
        count_phrase(memory.counts.active, "rule", "rules")
    );
    if memory.items.is_empty() {
        println!("  no active local rules yet");
        let next = if all {
            "difflore import-reviews"
        } else {
            "difflore memory active --all"
        };
        println!("  next: {}", style::cmd(next));
        println!(
            "        {}",
            style::pewter("or approve pending items from `difflore memory inbox`")
        );
        return;
    }
    for item in &memory.items {
        print_memory_item_summary(item);
    }
    println!();
    println!("  preview: {}", style::cmd("difflore recall --diff"));
    println!("  activity: {}", style::cmd("difflore memory activity"));
}

fn print_activity(activity: &MemoryActivity) {
    println!("{}", style::title("Memory Activity"));
    println!(
        "  window             last {} {}",
        activity.days,
        plural(activity.days, "day", "days")
    );
    println!("  tool calls         {}", activity.summary.calls);
    println!("  surfaced rules     {}", activity.summary.rules_served);
    println!("  strict matches     {}", activity.summary.strict_matches);
    println!("  empty recalls      {}", activity.summary.empty_calls);
    println!("  note               {}", style::pewter(&activity.note));
    if activity.recent.is_empty() {
        println!();
        println!("  no recent local activity");
        return;
    }
    println!();
    println!("{}", style::title("Recent"));
    for event in &activity.recent {
        let rules = if event.rule_ids.is_empty() {
            "-".to_owned()
        } else {
            event.rule_ids.join(", ")
        };
        println!(
            "  {} {}  rules={}  file={}",
            style::ident(&event.at),
            event.phase,
            rules,
            event.file_path.as_deref().unwrap_or("-")
        );
        println!(
            "    tool={}  repo={}  strict={}",
            event.tool,
            event.repo_full_name.as_deref().unwrap_or("-"),
            event.strict_match_count
        );
    }
}

fn print_memory_item_summary(item: &MemoryListItem) {
    println!("  {} {}", style::ident(&item.item_id), item.title);
    println!(
        "    state={}  source_repo={}  origin={}",
        item.state,
        item.source_repo.as_deref().unwrap_or("-"),
        item.origin.as_deref().unwrap_or("-")
    );
    if !item.file_patterns.is_empty() {
        println!("    path hints: {}", item.file_patterns.join(", "));
    }
    if let Some(hint) = non_empty(item.review_hint.as_deref()) {
        println!("    hint: {hint}");
    }
    println!("    inspect: {}", style::cmd(&item.commands.show));
}

fn print_rule_items(items: &[MemoryRuleItem]) {
    for item in items {
        println!("  {} {}", style::ident(&item.id), item.name);
        println!(
            "    origin={}  source_repo={}  updated={}",
            item.origin,
            item.source_repo.as_deref().unwrap_or("-"),
            item.updated_at
        );
        if !item.file_patterns.is_empty() {
            println!("    path_hints={}", item.file_patterns.join(", "));
        }
    }
}

fn print_memory_detail(detail: &difflore_core::memory_inbox::MemoryItemDetail) {
    println!("{}", style::title("Memory Item"));
    println!("  id: {}", style::ident(&detail.item.item_id));
    println!("  kind: {}", detail.item.kind);
    println!("  state: {}", detail.item.state);
    println!(
        "  active: {}",
        if detail.item.active { "yes" } else { "no" }
    );
    println!(
        "  source_repo: {}",
        detail.item.source_repo.as_deref().unwrap_or("-")
    );
    println!("  origin: {}", detail.item.origin.as_deref().unwrap_or("-"));
    if !detail.item.file_patterns.is_empty() {
        println!("  path hints: {}", detail.item.file_patterns.join(", "));
    }
    if let Some(activity) = &detail.activity {
        println!(
            "  activity: {} calls, {} surfaced rules in last 30 days",
            activity.calls, activity.rules_served
        );
    }
    println!();
    println!("  body:");
    println!("{}", indent_block(&detail.body, "    "));
}

fn print_session_discovery_summary(discovery: &SessionMinedDiscovery) {
    println!("  {} {}", style::ident(&discovery.item_id), discovery.title);
    println!(
        "    state: {}  source: {}",
        candidate_state(&discovery.status),
        discovery.source_repo
    );
    println!("    review hint: {}", review_hint(&discovery.gate_verdict));
    if !discovery.file_patterns.is_empty() {
        println!("    path hints: {}", discovery.file_patterns.join(", "));
    }
    if let Some(error) = non_empty(discovery.last_error.as_deref()) {
        println!("    sync error: {}", truncate(error, 160));
    }
    println!(
        "    inspect: {}",
        style::cmd(&format!("difflore memory show {}", discovery.item_id))
    );
    println!(
        "    approve: {}",
        style::cmd(&format!("difflore memory approve {}", discovery.item_id))
    );
}

fn print_session_discovery(discovery: &SessionMinedDiscovery) {
    println!("{}", style::title("Candidate memory"));
    println!("  id: {}", style::ident(&discovery.item_id));
    println!("  active: no (waiting for local approval)");
    println!("  state: {}", candidate_state(&discovery.status));
    println!("  source: {}", discovery.source_repo);
    println!("  review hint: {}", review_hint(&discovery.gate_verdict));
    if !discovery.file_patterns.is_empty() {
        println!("  path hints: {}", discovery.file_patterns.join(", "));
    }
    if let Some(error) = non_empty(discovery.last_error.as_deref()) {
        println!("  sync error: {error}");
    }
    println!();
    println!("  proposed rule title:");
    println!("{}", indent_block(&discovery.title, "    "));
    println!();
    println!("  proposed rule body:");
    println!("{}", indent_block(&discovery.body, "    "));
    println!();
    println!("  approve:");
    println!(
        "    {}",
        style::cmd(&format!("difflore memory approve {}", discovery.item_id))
    );
    println!("    activates this rule for local agents now");
    println!("  reject:");
    println!(
        "    {}",
        style::cmd(&format!("difflore memory reject {}", discovery.item_id))
    );
    println!("    discards this candidate locally; it will not be shared");
    println!("  team sync: {}", style::cmd("difflore memory sync"));
    println!("    optional; shares approved local memory with your team");
}

fn print_draft_review_summary(draft: &CandidateRule) {
    println!(
        "  {} {}",
        style::ident(&format!("draft:{}", draft.id)),
        draft.name
    );
    println!(
        "    source: {}  origin: {}",
        draft.source_repo.as_deref().unwrap_or("-"),
        draft.origin
    );
    if !draft.file_patterns.is_empty() {
        println!("    path hints: {}", draft.file_patterns.join(", "));
    }
    println!(
        "    inspect: {}",
        style::cmd(&format!("difflore memory show draft:{}", draft.id))
    );
    println!(
        "    approve: {}",
        style::cmd(&format!("difflore memory approve draft:{}", draft.id))
    );
}

fn print_draft(draft: &CandidateRule) {
    println!("{}", style::title("Local memory draft"));
    println!("  id: {}", style::ident(&format!("draft:{}", draft.id)));
    println!(
        "  source_repo: {}",
        draft.source_repo.as_deref().unwrap_or("-")
    );
    println!("  origin: {}", draft.origin);
    println!("  captured: {}", draft.installed_at);
    if !draft.file_patterns.is_empty() {
        println!("  path hints: {}", draft.file_patterns.join(", "));
    }
    println!();
    println!("  drafted rule:");
    println!(
        "{}",
        indent_block(
            draft.drafted_rule.as_deref().unwrap_or(&draft.description),
            "    "
        )
    );
    println!();
    println!("  approve:");
    println!(
        "    {}",
        style::cmd(&format!("difflore memory approve draft:{}", draft.id))
    );
    println!("    activates this rule for local agents now");
    println!("  reject:");
    println!(
        "    {}",
        style::cmd(&format!("difflore memory reject draft:{}", draft.id))
    );
    println!("    discards this draft locally; it will not be shared");
}

fn print_cloud_outbox_counts(counts: &[OutboxQueueCount]) {
    for count in counts {
        println!(
            "  {} {} {}",
            count.count,
            count.status,
            cloud_outbox_label(&count.kind)
        );
    }
}

fn compact_queue_line(inbox: &MemoryInbox) -> String {
    let mut parts = Vec::new();
    let memory = inbox.memory_candidates_pending();
    if memory > 0 {
        parts.push(format!(
            "{memory} memory {}",
            plural(memory, "candidate", "candidates")
        ));
    }
    let cloud_observations = inbox.cloud_observations_pending();
    if cloud_observations > 0 {
        parts.push(format!(
            "{cloud_observations} activity {}",
            plural(cloud_observations, "record", "records")
        ));
    }
    let observation_events = inbox.observation_events_pending();
    if observation_events > 0 {
        parts.push(format!(
            "{observation_events} activity {}",
            plural(observation_events, "event", "events")
        ));
    }
    parts.join(", ")
}

fn print_team_line(cloud: &MemoryCloudSummary) {
    print_team_status(cloud, true);
}

fn print_team_status(cloud: &MemoryCloudSummary, with_label: bool) {
    let prefix = if with_label {
        "  team review        "
    } else {
        "  "
    };
    if !cloud.logged_in {
        println!(
            "{prefix}optional team sync starts with {}",
            style::cmd("difflore cloud login")
        );
        return;
    }
    println!(
        "{prefix}optional: {} shares approved local memory with the team",
        style::cmd("difflore memory sync")
    );
}

fn cloud_outbox_label(kind: &str) -> &'static str {
    match kind {
        difflore_core::cloud::outbox::kind::SESSION_MINED_CANDIDATE => "memory candidate rows",
        difflore_core::cloud::outbox::kind::OBSERVATION => "activity rows",
        difflore_core::cloud::outbox::kind::MCP_QUERY => "MCP query rows",
        difflore_core::cloud::outbox::kind::IMPORTED_REVIEWS => "imported review rows",
        difflore_core::cloud::outbox::kind::ACCEPTED_EDIT => "accepted edit rows",
        difflore_core::cloud::outbox::kind::REVIEW_METRICS => "review metric rows",
        difflore_core::cloud::outbox::kind::TRAJECTORY => "trajectory rows",
        difflore_core::cloud::outbox::kind::LEGACY_FIX_ACCEPTANCE => "legacy fix acceptance rows",
        _ => "outbox rows",
    }
}

fn observation_label(event_type: &str) -> &'static str {
    match event_type {
        "mcp_rule_served" => "MCP serve observations",
        "rule_fired" => "rule-fire observations",
        "rule_cited_in_edit" => "rule-cited observations",
        "rule_actually_cited" => "actual-citation observations",
        "fix_outcome" => "fix outcome observations",
        _ => "activity observations",
    }
}

fn count_to_usize(count: i64) -> usize {
    usize::try_from(count.max(0)).unwrap_or(usize::MAX)
}

fn review_progress_heading(reviewing: usize, pending: usize) -> String {
    if reviewing < pending {
        format!(
            "Reviewing {reviewing} of {pending} pending memory {} locally.",
            plural(pending as i64, "item", "items")
        )
    } else {
        format!(
            "Reviewing {reviewing} pending memory {} locally.",
            plural(reviewing as i64, "item", "items")
        )
    }
}

fn candidate_state(status: &str) -> String {
    match status.trim().to_ascii_lowercase().as_str() {
        "pending" | "processing" => "waiting for local approval".to_owned(),
        "abandoned" => "waiting for local approval; optional sync failed".to_owned(),
        "confirmed" => "sent to team review".to_owned(),
        "" => "unknown".to_owned(),
        other => other.to_owned(),
    }
}

fn review_hint(verdict: &str) -> String {
    let trimmed = verdict.trim();
    if trimmed.eq_ignore_ascii_case("KEEP") {
        return "review as a new memory".to_owned();
    }
    if trimmed.eq_ignore_ascii_case("DROP") {
        return "probably reject".to_owned();
    }
    if let Some(target) = trimmed.strip_prefix("MERGE:") {
        return format!("merge with existing memory `{}`", target.trim());
    }
    if trimmed.is_empty() {
        return "needs review".to_owned();
    }
    format!("needs review ({trimmed})")
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn indent_block(text: &str, prefix: &str) -> String {
    text.lines()
        .map(|line| format!("{prefix}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn truncate(value: &str, limit: usize) -> String {
    let mut chars = value.chars();
    let truncated: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
}

fn flush_stdout() {
    if let Err(e) = io::stdout().flush() {
        exit_err(&format!("failed to write prompt: {e}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use difflore_core::memory_inbox::{
        MemoryQueueSection, MemoryRuleSection, MemoryUsage, SessionMinedSection,
    };

    fn empty_inbox(active: i64, drafts: i64, session: i64) -> MemoryInbox {
        MemoryInbox {
            active_rules: MemoryRuleSection {
                count: active,
                latest: Vec::new(),
            },
            local_drafts: MemoryRuleSection {
                count: drafts,
                latest: Vec::new(),
            },
            local_discoveries: SessionMinedSection {
                count: session,
                latest: Vec::new(),
            },
            queues: MemoryQueueSection {
                cloud_outbox: Vec::new(),
                observations_outbox: Vec::new(),
            },
            usage: MemoryUsage {
                local_agent_serves: 0,
                proof_surface: "difflore status --json".to_owned(),
            },
            warnings: Vec::new(),
        }
    }

    fn cloud(logged_in: bool) -> MemoryCloudSummary {
        MemoryCloudSummary {
            logged_in,
            team_ready: None,
            blocker: None,
            note: None,
        }
    }

    fn rule(id: &str, source_repo: Option<&str>) -> MemoryRuleItem {
        MemoryRuleItem {
            id: id.to_owned(),
            name: id.to_owned(),
            origin: "pr_review".to_owned(),
            source_repo: source_repo.map(str::to_owned),
            file_patterns: Vec::new(),
            updated_at: "2026-01-01T00:00:00Z".to_owned(),
        }
    }

    fn active_item(id: &str, source_repo: Option<&str>) -> MemoryListItem {
        MemoryListItem {
            item_id: format!("rule:{id}"),
            kind: "rule".to_owned(),
            state: "active".to_owned(),
            active: true,
            served_to_agents: true,
            approval_required: false,
            title: id.to_owned(),
            summary: None,
            origin: Some("agent_file:test".to_owned()),
            source_repo: source_repo.map(str::to_owned),
            file_patterns: Vec::new(),
            updated_at: Some("2026-01-01 00:00:00".to_owned()),
            review_hint: Some("served to agents when recall matches".to_owned()),
            commands: difflore_core::memory_inbox::MemoryItemCommands {
                show: format!("difflore memory show rule:{id}"),
                approve: None,
                reject: None,
            },
        }
    }

    #[test]
    fn current_repo_rule_items_are_prioritized_for_display() {
        let keys = repo_scope_keys(&["acme/api".to_owned()]);
        let mut items = vec![
            rule("web", Some("acme/web")),
            rule("api", Some("ACME/API")),
            rule("none", None),
        ];

        prioritize_current_repo_rule_items(&mut items, &keys);

        let ids: Vec<&str> = items.iter().map(|item| item.id.as_str()).collect();
        assert_eq!(ids, ["api", "web", "none"]);
    }

    #[test]
    fn active_memory_filters_to_current_repo_before_display_limit() {
        let keys = repo_scope_keys(&["acme/api".to_owned()]);
        let mut memory = MemoryList {
            counts: difflore_core::memory_inbox::MemoryStateCounts {
                active: 4,
                drafts: 0,
                candidates: 0,
            },
            items: vec![
                active_item("api-1", Some("acme/api")),
                active_item("web", Some("acme/web")),
                active_item("api-2", Some("ACME/API")),
                active_item("none", None),
            ],
            warnings: Vec::new(),
            note: String::new(),
        };

        filter_active_to_current_repo(&mut memory, &keys, 1);

        assert_eq!(memory.counts.active, 2);
        assert_eq!(memory.items.len(), 1);
        assert_eq!(memory.items[0].item_id, "rule:api-1");
    }

    #[test]
    fn next_action_prioritizes_drafts() {
        let next = next_action(&empty_inbox(0, 1, 4), &cloud(true));
        assert_eq!(next.command, "difflore memory review");
        assert_eq!(next.contract.safety_tier, 2);
        assert!(next.contract.requires_user_intent);
    }

    #[test]
    fn next_action_reviews_session_candidates_when_logged_out() {
        let next = next_action(&empty_inbox(0, 0, 4), &cloud(false));
        assert_eq!(next.command, "difflore memory review");
        assert_eq!(next.blocked_by, None);
    }

    #[test]
    fn next_action_reviews_session_candidates_when_logged_in() {
        let next = next_action(&empty_inbox(0, 0, 4), &cloud(true));
        assert_eq!(next.command, "difflore memory review");
    }

    #[test]
    fn next_action_for_active_memory_has_json_contract() {
        let next = next_action(&empty_inbox(3, 0, 0), &cloud(true));
        assert_eq!(next.command, "difflore recall --diff");
        assert_eq!(
            next.contract.json_command.as_deref(),
            Some("difflore recall --diff --json")
        );
        assert_eq!(next.contract.safety_tier, 0);
    }

    #[test]
    fn review_progress_heading_distinguishes_displayed_from_total_pending() {
        assert_eq!(
            review_progress_heading(50, 75),
            "Reviewing 50 of 75 pending memory items locally."
        );
        assert_eq!(
            review_progress_heading(1, 1),
            "Reviewing 1 pending memory item locally."
        );
    }
}
