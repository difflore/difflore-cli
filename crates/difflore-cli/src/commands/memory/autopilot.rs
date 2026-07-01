use std::collections::HashSet;

use serde::Serialize;
use serde_json::json;

use difflore_core::SqlitePool;
use difflore_core::memory_autopilot::{
    DEFAULT_AUTOPILOT_LIMIT, MemoryAutopilotLogFilter, MemoryAutopilotOptions,
    MemoryCandidateGroup, MemoryCandidateGroupState, MemoryConflictFilter,
    approve_memory_candidate_group, disable_memory_rule, load_autopilot_log, load_memory_conflicts,
    load_memory_digest, run_memory_autopilot,
};
use difflore_core::memory_autopilot_schedule::{
    AutopilotScheduleRequest, load_autopilot_schedule_status, mark_autopilot_dirty,
    note_autopilot_spawn_success, release_autopilot_lease, run_background_memory_autopilot,
    try_acquire_autopilot_lease, try_acquire_manual_autopilot_lease,
};
use difflore_core::memory_inbox::{parse_session_item_id, reject_session_mined_candidate};

use crate::runtime::CommandContext;
use crate::style;
use crate::support::util::{confirm_destructive, exit_code, json_compact_or};

use super::{count_phrase, exit_structured_err};

const DEFAULT_MEMORY_CLEANUP_LIMIT: usize = 1_000;
const MEMORY_CLEANUP_SCHEMA_VERSION: &str = "2026-06-23.memory.cleanup.v1";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct MemoryCleanupAction {
    item_id: String,
    content_hash: String,
    group_id: String,
    title: String,
    reason: String,
    reason_detail: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct MemoryCleanupRemoved {
    item_id: String,
    content_hash: String,
    group_id: String,
    title: String,
    reason: String,
    outbox_id: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct MemoryCleanupFailure {
    item_id: String,
    content_hash: String,
    group_id: String,
    title: String,
    reason: String,
    error: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct MemoryCleanupSummary {
    groups_scanned: usize,
    planned: usize,
    removed: usize,
    failed: usize,
    already_active: usize,
    duplicate_in_group: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct MemoryCleanupReport {
    schema_version: &'static str,
    dry_run: bool,
    limit: usize,
    summary: MemoryCleanupSummary,
    planned: Vec<MemoryCleanupAction>,
    removed: Vec<MemoryCleanupRemoved>,
    failed: Vec<MemoryCleanupFailure>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct RecommendedApproveFailure {
    group_id: String,
    title: String,
    error: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct RecommendedApproveReport {
    dry_run: bool,
    selected: usize,
    approved: Vec<difflore_core::memory_autopilot::MemoryAutopilotAction>,
    failed: Vec<RecommendedApproveFailure>,
    groups: Vec<MemoryCandidateGroup>,
}

pub(crate) async fn handle_autopilot(
    ctx: &CommandContext,
    dry_run: bool,
    max_auto_enable: Option<usize>,
    json: bool,
    background: bool,
    lease_owner: Option<String>,
) {
    if background {
        handle_background_autopilot(ctx, lease_owner, json).await;
        return;
    }

    let manual_lease_owner = if dry_run {
        None
    } else {
        let owner = autopilot_lease_owner("manual");
        match try_acquire_manual_autopilot_lease(&ctx.db, &owner).await {
            Ok(true) => Some(owner),
            Ok(false) => exit_structured_err(
                "memory autopilot is already running in the background; retry shortly",
                json,
            ),
            Err(err) => {
                exit_structured_err(&format!("failed to lock memory autopilot: {err}"), json)
            }
        }
    };

    let report = match run_memory_autopilot(
        &ctx.db,
        MemoryAutopilotOptions {
            dry_run,
            max_auto_enable: max_auto_enable.unwrap_or(DEFAULT_AUTOPILOT_LIMIT),
            curator_max_candidates: None,
        },
    )
    .await
    {
        Ok(report) => report,
        Err(err) => {
            if let Some(owner) = manual_lease_owner.as_deref() {
                let _ = release_autopilot_lease(&ctx.db, owner, "manual_failed").await;
            }
            exit_structured_err(&format!("failed to run memory autopilot: {err}"), json)
        }
    };
    if let Some(owner) = manual_lease_owner.as_deref() {
        let _ = release_autopilot_lease(&ctx.db, owner, "manual_finished").await;
    }

    if json {
        println!("{}", json_compact_or(&report, "{}"));
        return;
    }

    println!("{}", style::title("Memory Autopilot"));
    if let Ok(status) = load_autopilot_schedule_status(&ctx.db).await {
        println!(
            "  background        {} | runs {} ({} productive) | triggers {}",
            if status.enabled { "on" } else { "off" },
            style::ident(&status.run_count.to_string()),
            style::ident(&status.productive_run_count.to_string()),
            style::ident(&status.trigger_count.to_string())
        );
        if status.dirty {
            println!(
                "  pending work      dirty since {}",
                status.last_dirty_at.unwrap_or_default()
            );
        }
    }
    if report.dry_run {
        println!("  preview only; local memory was not changed");
    }
    if report.auto_enabled.is_empty() {
        println!("  no high-confidence memory groups were enabled");
    } else {
        let verb = if report.dry_run {
            "would enable"
        } else {
            "enabled"
        };
        println!(
            "  {} {}",
            verb,
            count_phrase(
                report.auto_enabled.len() as i64,
                "local rule",
                "local rules"
            )
        );
        for action in &report.auto_enabled {
            println!("  {}", action.title);
            println!(
                "    {} candidates  {}",
                action.item_ids.len(),
                action.reason
            );
            if let Some(rule_id) = &action.rule_id {
                println!("    rule: {}", style::ident(rule_id));
            }
        }
    }

    let needs_review = report.digest.counts.needs_review_groups;
    let recommended = report.digest.counts.recommended_groups;
    if recommended > 0 {
        println!();
        println!(
            "  recommended       {}",
            count_phrase(recommended as i64, "group", "groups")
        );
        println!("  inspect: {}", style::cmd("difflore memory recommended"));
    }
    if needs_review > 0 {
        println!();
        println!(
            "  left for review   {}",
            count_phrase(needs_review as i64, "group", "groups")
        );
        println!("  inspect: {}", style::cmd("difflore memory digest"));
        println!("  review: {}", style::cmd("difflore memory review"));
    }
    println!();
    println!("  log: {}", style::cmd("difflore memory log"));
    println!(
        "  disable: {}",
        style::cmd("difflore memory disable rule:<id>")
    );
}

pub(crate) async fn handle_cleanup(
    ctx: &CommandContext,
    apply: bool,
    limit: Option<usize>,
    json: bool,
) {
    let limit = limit.unwrap_or(DEFAULT_MEMORY_CLEANUP_LIMIT);
    let digest = load_memory_digest(&ctx.db, limit)
        .await
        .unwrap_or_else(|err| {
            exit_structured_err(&format!("failed to load memory cleanup plan: {err}"), json)
        });
    let planned = plan_memory_cleanup(&digest.candidate_groups);
    let mut removed = Vec::new();
    let mut failed = Vec::new();

    if apply {
        for action in &planned {
            match reject_session_mined_candidate(&ctx.db, &action.content_hash).await {
                Ok(rejected) => removed.push(removed_cleanup_item(action, rejected.outbox_id)),
                Err(err) => failed.push(failed_cleanup_item(action, err.to_string())),
            }
        }
        if !removed.is_empty() {
            mark_memory_autopilot_dirty_best_effort(&ctx.db, "memory_cleanup").await;
        }
    }

    let report = MemoryCleanupReport {
        schema_version: MEMORY_CLEANUP_SCHEMA_VERSION,
        dry_run: !apply,
        limit,
        summary: cleanup_summary(digest.candidate_groups.len(), &planned, &removed, &failed),
        planned,
        removed,
        failed,
    };

    if json {
        println!("{}", json_compact_or(&report, "{}"));
        if apply && !report.failed.is_empty() {
            exit_code(1);
        }
        return;
    }

    print_cleanup_report(&report);
    if apply && !report.failed.is_empty() {
        exit_code(1);
    }
}

pub(crate) async fn mark_memory_autopilot_dirty_best_effort(db: &SqlitePool, reason: &str) {
    if let Err(err) = mark_autopilot_dirty(db, reason).await
        && difflore_core::infra::env::debug_telemetry()
    {
        eprintln!("[difflore.memory_autopilot] mark dirty failed: {err}");
    }
}

pub(crate) async fn schedule_memory_autopilot_best_effort(
    db: &SqlitePool,
    reason: &str,
    cooldown_secs: i64,
) {
    let lease_owner = autopilot_lease_owner(reason);
    let acquired = match try_acquire_autopilot_lease(
        db,
        AutopilotScheduleRequest {
            reason,
            cooldown_secs,
            lease_owner: &lease_owner,
        },
    )
    .await
    {
        Ok(acquired) => acquired,
        Err(err) => {
            if difflore_core::infra::env::debug_telemetry() {
                eprintln!("[difflore.memory_autopilot] schedule failed: {err}");
            }
            return;
        }
    };
    if !acquired {
        return;
    }

    match crate::hook::forward::spawn::spawn_memory_autopilot_detached(&lease_owner) {
        Ok(()) => {
            if let Err(err) = note_autopilot_spawn_success(db, &lease_owner).await
                && difflore_core::infra::env::debug_telemetry()
            {
                eprintln!("[difflore.memory_autopilot] spawn note failed: {err}");
            }
        }
        Err(err) => {
            if difflore_core::infra::env::debug_telemetry() {
                eprintln!("[difflore.memory_autopilot] spawn failed: {err}");
            }
            let _ = release_autopilot_lease(db, &lease_owner, "spawn_failed").await;
        }
    }
}

async fn handle_background_autopilot(
    ctx: &CommandContext,
    lease_owner: Option<String>,
    json: bool,
) {
    let Some(lease_owner) = lease_owner.filter(|value| !value.trim().is_empty()) else {
        exit_structured_err("background autopilot requires --lease-owner", json);
    };
    let run = run_background_memory_autopilot(&ctx.db, &lease_owner)
        .await
        .unwrap_or_else(|err| {
            exit_structured_err(
                &format!("failed to run background memory autopilot: {err}"),
                json,
            )
        });
    if json {
        println!("{}", json_compact_or(&run, "{}"));
    }
}

fn autopilot_lease_owner(reason: &str) -> String {
    let now_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!(
        "{}:{}:{}",
        reason.replace(|ch: char| !ch.is_ascii_alphanumeric(), "_"),
        std::process::id(),
        now_ns
    )
}

pub(crate) async fn handle_digest(ctx: &CommandContext, limit: Option<usize>, json: bool) {
    let digest = load_memory_digest(&ctx.db, limit.unwrap_or(20))
        .await
        .unwrap_or_else(|err| {
            exit_structured_err(&format!("failed to load memory digest: {err}"), json)
        });

    if json {
        println!("{}", json_compact_or(&digest, "{}"));
        return;
    }

    println!("{}", style::title("Memory Digest"));
    println!(
        "  active rules      {}",
        count_phrase(digest.counts.active_rules, "rule", "rules")
    );
    println!(
        "  autopilot-ready   {}",
        count_phrase(digest.counts.auto_enable_groups as i64, "group", "groups")
    );
    println!(
        "  recommended       {}",
        count_phrase(digest.counts.recommended_groups as i64, "group", "groups")
    );
    println!(
        "  needs review      {}",
        count_phrase(digest.counts.needs_review_groups as i64, "group", "groups")
    );
    println!("  note              {}", style::pewter(&digest.note));
    if let Ok(status) = load_autopilot_schedule_status(&ctx.db).await {
        println!(
            "  background        {} | runs {} ({} productive) | triggers {}",
            if status.enabled { "on" } else { "off" },
            status.run_count,
            status.productive_run_count,
            status.trigger_count
        );
    }

    print_group_section(
        "Autopilot-ready",
        digest
            .candidate_groups
            .iter()
            .filter(|group| group.state == MemoryCandidateGroupState::AutoEnable),
    );
    print_group_section(
        "Recommended",
        digest
            .candidate_groups
            .iter()
            .filter(|group| group.state == MemoryCandidateGroupState::Recommended),
    );
    print_group_section(
        "Needs Review",
        digest
            .candidate_groups
            .iter()
            .filter(|group| group.state == MemoryCandidateGroupState::NeedsReview),
    );

    if digest.candidate_groups.is_empty() && digest.active_rules.is_empty() {
        println!();
        println!("  no local memory yet");
    }
    if !digest.next_actions.is_empty() {
        println!();
        println!("  next: {}", style::cmd(&digest.next_actions[0]));
    }
}

pub(crate) async fn handle_recommended(
    ctx: &CommandContext,
    all: bool,
    limit: Option<usize>,
    approve: bool,
    yes: bool,
    json: bool,
) {
    let digest = load_memory_digest(&ctx.db, 1_000)
        .await
        .unwrap_or_else(|err| {
            exit_structured_err(&format!("failed to load recommended memory: {err}"), json)
        });
    let mut recommended = digest
        .candidate_groups
        .into_iter()
        .filter(|group| group.state == MemoryCandidateGroupState::Recommended)
        .collect::<Vec<_>>();
    if let Some(limit) = limit {
        recommended.truncate(limit);
    } else if !all {
        recommended.truncate(20);
    }

    if json {
        if !approve {
            println!(
                "{}",
                json_compact_or(
                    &json!({
                        "schemaVersion": digest.schema_version,
                        "recommendedGroups": recommended,
                    }),
                    "{}",
                )
            );
            return;
        }
        let report = approve_recommended_groups(ctx, recommended, yes, json).await;
        println!("{}", json_compact_or(&report, "{}"));
        if !report.failed.is_empty() {
            exit_code(1);
        }
        return;
    }

    println!("{}", style::title("Recommended Memory"));
    if recommended.is_empty() {
        println!("  no recommended memory groups right now");
        println!(
            "  refresh: {}",
            style::cmd("difflore memory autopilot --dry-run")
        );
        return;
    }

    print_group_section("Recommended", recommended.iter());
    if approve {
        let report = approve_recommended_groups(ctx, recommended, yes, json).await;
        print_recommended_approve_report(&report);
        if !report.failed.is_empty() {
            exit_code(1);
        }
        return;
    }
    println!();
    println!(
        "  approve: {}",
        style::cmd("difflore memory recommended --approve")
    );
    println!("  review:  {}", style::cmd("difflore memory review"));
}

async fn approve_recommended_groups(
    ctx: &CommandContext,
    groups: Vec<MemoryCandidateGroup>,
    yes: bool,
    json: bool,
) -> RecommendedApproveReport {
    if groups.is_empty() {
        return RecommendedApproveReport {
            dry_run: false,
            selected: 0,
            approved: Vec::new(),
            failed: Vec::new(),
            groups,
        };
    }
    if !json
        && let Err(err) = confirm_destructive(
            yes,
            &format!("approve {} recommended memory group(s)?", groups.len()),
        )
    {
        exit_structured_err(&err.to_string(), json);
    }
    if json && !yes {
        return RecommendedApproveReport {
            dry_run: true,
            selected: groups.len(),
            approved: Vec::new(),
            failed: Vec::new(),
            groups,
        };
    }

    let mut approved = Vec::new();
    let mut failed = Vec::new();
    for group in &groups {
        match approve_memory_candidate_group(&ctx.db, &group.group_id).await {
            Ok(action) => approved.push(action),
            Err(err) => failed.push(RecommendedApproveFailure {
                group_id: group.group_id.clone(),
                title: group.title.clone(),
                error: err.to_string(),
            }),
        }
    }
    RecommendedApproveReport {
        dry_run: false,
        selected: groups.len(),
        approved,
        failed,
        groups,
    }
}

fn print_recommended_approve_report(report: &RecommendedApproveReport) {
    println!();
    println!("{}", style::title("Approval Result"));
    if report.dry_run {
        println!(
            "  preview only; pass {} to approve in JSON/non-interactive mode",
            style::cmd("--yes")
        );
        return;
    }
    println!(
        "  approved {} of {} recommended group(s)",
        report.approved.len(),
        report.selected
    );
    for action in &report.approved {
        let rule = action.rule_id.as_deref().unwrap_or("-");
        println!("  - {} -> {}", action.title, style::ident(rule));
    }
    for failure in &report.failed {
        println!(
            "  {} {}: {}",
            style::amber(style::sym::WARN),
            failure.title,
            failure.error
        );
    }
}

pub(crate) async fn handle_log(ctx: &CommandContext, limit: Option<usize>, json: bool) {
    let log = load_autopilot_log(
        &ctx.db,
        MemoryAutopilotLogFilter {
            limit: limit.unwrap_or(20),
        },
    )
    .await
    .unwrap_or_else(|err| exit_structured_err(&format!("failed to load memory log: {err}"), json));

    if json {
        println!("{}", json_compact_or(&log, "{}"));
        return;
    }

    println!("{}", style::title("Memory Log"));
    if log.events.is_empty() {
        println!("  no local autopilot events yet");
        return;
    }
    for event in &log.events {
        println!(
            "  {} {} {}",
            style::ident(&event.created_at),
            event.event_type,
            event.title
        );
        if let Some(rule_id) = &event.rule_id {
            println!("    rule: {}", style::ident(rule_id));
        }
        if !event.item_ids.is_empty() {
            println!("    items: {}", event.item_ids.join(", "));
        }
        println!("    why: {}", event.reason);
    }
}

pub(crate) async fn handle_conflicts(
    ctx: &CommandContext,
    limit: Option<usize>,
    status: Option<String>,
    json: bool,
) {
    let report = load_memory_conflicts(&ctx.db, MemoryConflictFilter { limit, status })
        .await
        .unwrap_or_else(|err| {
            exit_structured_err(&format!("failed to load memory conflicts: {err}"), json)
        });

    if json {
        println!("{}", json_compact_or(&report, "{}"));
        return;
    }

    println!("{}", style::title("Memory Conflicts"));
    if report.conflicts.is_empty() {
        println!("  no recorded conflicts");
        return;
    }
    for conflict in &report.conflicts {
        println!(
            "  {} {} vs {}",
            style::ident(&conflict.status),
            conflict.candidate_title,
            conflict.active_title
        );
        println!(
            "    active rule: {}",
            style::ident(&conflict.active_rule_id)
        );
        if let Some(repo) = &conflict.source_repo {
            println!("    repo: {repo}");
        }
        println!("    basis: {}", conflict.overlap_basis);
        println!("    recorded: {}", style::ident(&conflict.updated_at));
    }
}

pub(crate) async fn handle_disable(
    ctx: &CommandContext,
    rule_id: String,
    reason: Option<String>,
    json: bool,
) {
    let outcome = disable_memory_rule(&ctx.db, &rule_id, reason.as_deref())
        .await
        .unwrap_or_else(|err| {
            exit_structured_err(&format!("failed to disable memory rule: {err}"), json)
        });

    if json {
        println!("{}", json_compact_or(&outcome, "{}"));
        return;
    }

    println!(
        "{} Disabled local memory rule {}.",
        style::ok(style::sym::OK),
        style::ident(&outcome.rule_id)
    );
    println!("  It is no longer served to local agents.");
    println!(
        "  Re-enable manually with {}",
        style::cmd(&format!(
            "difflore memory approve draft:{}",
            outcome.rule_id
        ))
    );
}

fn plan_memory_cleanup(groups: &[MemoryCandidateGroup]) -> Vec<MemoryCleanupAction> {
    let mut actions = Vec::new();
    for group in groups {
        if group.state == MemoryCandidateGroupState::AlreadyActive {
            for item_id in &group.item_ids {
                if let Ok(content_hash) = parse_session_item_id(item_id) {
                    actions.push(cleanup_action(
                        group,
                        item_id,
                        content_hash,
                        "already_active",
                        "candidate already matches an active local rule",
                    ));
                }
            }
            continue;
        }

        let mut seen_hashes = HashSet::new();
        for item_id in &group.item_ids {
            let Ok(content_hash) = parse_session_item_id(item_id) else {
                continue;
            };
            if !seen_hashes.insert(content_hash.clone()) {
                actions.push(cleanup_action(
                    group,
                    item_id,
                    content_hash,
                    "duplicate_in_group",
                    "duplicate pending row inside the same candidate group",
                ));
            }
        }
    }
    actions
}

fn cleanup_action(
    group: &MemoryCandidateGroup,
    item_id: &str,
    content_hash: String,
    reason: &str,
    reason_detail: &str,
) -> MemoryCleanupAction {
    MemoryCleanupAction {
        item_id: item_id.to_owned(),
        content_hash,
        group_id: group.group_id.clone(),
        title: group.title.clone(),
        reason: reason.to_owned(),
        reason_detail: reason_detail.to_owned(),
    }
}

fn removed_cleanup_item(action: &MemoryCleanupAction, outbox_id: i64) -> MemoryCleanupRemoved {
    MemoryCleanupRemoved {
        item_id: action.item_id.clone(),
        content_hash: action.content_hash.clone(),
        group_id: action.group_id.clone(),
        title: action.title.clone(),
        reason: action.reason.clone(),
        outbox_id,
    }
}

fn failed_cleanup_item(action: &MemoryCleanupAction, error: String) -> MemoryCleanupFailure {
    MemoryCleanupFailure {
        item_id: action.item_id.clone(),
        content_hash: action.content_hash.clone(),
        group_id: action.group_id.clone(),
        title: action.title.clone(),
        reason: action.reason.clone(),
        error,
    }
}

fn cleanup_summary(
    groups_scanned: usize,
    planned: &[MemoryCleanupAction],
    removed: &[MemoryCleanupRemoved],
    failed: &[MemoryCleanupFailure],
) -> MemoryCleanupSummary {
    MemoryCleanupSummary {
        groups_scanned,
        planned: planned.len(),
        removed: removed.len(),
        failed: failed.len(),
        already_active: planned
            .iter()
            .filter(|action| action.reason == "already_active")
            .count(),
        duplicate_in_group: planned
            .iter()
            .filter(|action| action.reason == "duplicate_in_group")
            .count(),
    }
}

fn print_cleanup_report(report: &MemoryCleanupReport) {
    println!("{}", style::title("Memory Cleanup"));
    println!(
        "  scanned           {}",
        count_phrase(report.summary.groups_scanned as i64, "group", "groups")
    );
    if report.dry_run {
        println!("  preview only; local memory was not changed");
    }

    if report.summary.planned == 0 {
        println!("  nothing safe to clean");
        println!("  inspect: {}", style::cmd("difflore memory digest"));
        return;
    }

    if report.dry_run {
        println!(
            "  would remove      {}",
            count_phrase(
                report.summary.planned as i64,
                "pending item",
                "pending items"
            )
        );
    } else {
        println!(
            "  removed           {}",
            count_phrase(
                report.summary.removed as i64,
                "pending item",
                "pending items"
            )
        );
    }
    println!(
        "  already active    {}",
        count_phrase(
            report.summary.already_active as i64,
            "candidate",
            "candidates"
        )
    );
    println!(
        "  duplicate rows    {}",
        count_phrase(
            report.summary.duplicate_in_group as i64,
            "candidate",
            "candidates"
        )
    );

    let preview_limit = 20;
    println!();
    let planned_prefix = if report.dry_run {
        "Planned removals"
    } else {
        "Cleanup plan"
    };
    println!("{}", style::title(planned_prefix));
    for action in report.planned.iter().take(preview_limit) {
        println!("  - {} {}", style::ident(&action.item_id), action.title);
        println!("    {}", action.reason_detail);
    }
    if report.planned.len() > preview_limit {
        println!(
            "  ... {} more",
            count_phrase(
                (report.planned.len() - preview_limit) as i64,
                "candidate",
                "candidates"
            )
        );
    }

    if !report.failed.is_empty() {
        println!();
        println!("{}", style::title("Failures"));
        for failure in &report.failed {
            println!("  - {} {}", style::ident(&failure.item_id), failure.error);
        }
    }

    if report.dry_run {
        println!();
        println!("  apply: {}", style::cmd("difflore memory cleanup --apply"));
    }
}

fn print_group_section<'a>(title: &str, groups: impl Iterator<Item = &'a MemoryCandidateGroup>) {
    let groups = groups.collect::<Vec<_>>();
    if groups.is_empty() {
        return;
    }
    println!();
    println!("{}", style::title(title));
    for group in groups {
        println!("  {}", group.title);
        println!("    {} candidates  {}", group.item_ids.len(), group.reason);
        if let Some(repo) = &group.source_repo {
            println!("    repo: {repo}");
        }
        if !group.file_patterns.is_empty() {
            println!("    path hints: {}", group.file_patterns.join(", "));
        }
        if let Some(first) = group.item_ids.first() {
            println!(
                "    inspect: {}",
                style::cmd(&format!("difflore memory show {first}"))
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate_group(
        state: MemoryCandidateGroupState,
        item_ids: Vec<&str>,
    ) -> MemoryCandidateGroup {
        MemoryCandidateGroup {
            group_id: "group".to_owned(),
            title: "Test rule".to_owned(),
            state,
            reason: "test reason".to_owned(),
            confidence: None,
            item_ids: item_ids.into_iter().map(str::to_owned).collect(),
            source_repo: None,
            file_patterns: Vec::new(),
            origins: Vec::new(),
            verdicts: Vec::new(),
            sample: String::new(),
        }
    }

    #[test]
    fn cleanup_plan_removes_all_session_items_when_already_active() {
        let groups = vec![candidate_group(
            MemoryCandidateGroupState::AlreadyActive,
            vec!["session:a", "draft:ignored", "session:b"],
        )];

        let plan = plan_memory_cleanup(&groups);

        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].content_hash, "a");
        assert_eq!(plan[1].content_hash, "b");
        assert!(plan.iter().all(|action| action.reason == "already_active"));
    }

    #[test]
    fn cleanup_plan_keeps_one_exact_duplicate_for_review() {
        let groups = vec![candidate_group(
            MemoryCandidateGroupState::NeedsReview,
            vec!["session:a", "session:a", "session:b"],
        )];

        let plan = plan_memory_cleanup(&groups);

        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].content_hash, "a");
        assert_eq!(plan[0].reason, "duplicate_in_group");
    }
}
