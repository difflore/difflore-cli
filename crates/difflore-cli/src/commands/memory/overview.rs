use difflore_core::memory_overview::{
    ActivityOverview, MemoryOverview, MemoryOverviewNextAction, MemoryOverviewOptions,
    OverviewReviewItem, OverviewRule, load_memory_overview,
};

use crate::runtime::CommandContext;
use crate::style;
use crate::support::util::json_compact_or;

use super::exit_structured_err;

const OVERVIEW_LATEST_LIMIT: usize = 5;
const OVERVIEW_ACTIVITY_DAYS: i64 = 30;

pub(crate) async fn handle_summary(ctx: &CommandContext, json: bool) {
    let repo_full_name = current_repo_full_name(ctx).await;
    let mut overview = load_memory_overview(
        &ctx.db,
        MemoryOverviewOptions {
            repo_full_name,
            latest_limit: OVERVIEW_LATEST_LIMIT,
            activity_days: OVERVIEW_ACTIVITY_DAYS,
        },
    )
    .await
    .unwrap_or_else(|err| {
        exit_structured_err(&format!("failed to load memory overview: {err}"), json)
    });
    overview.sync.logged_in = ctx.cloud().await.is_logged_in();

    if json {
        println!("{}", json_compact_or(&overview, "{}"));
        return;
    }

    print_overview(&overview);
}

async fn current_repo_full_name(ctx: &CommandContext) -> Option<String> {
    let configured_gitlab_hosts = difflore_core::ingest::gitlab::auth::configured_hosts().await;
    let project = ctx.project.to_string_lossy();
    difflore_core::infra::git::detect_repo_full_names_with_gitlab_hosts(
        project.as_ref(),
        &configured_gitlab_hosts,
    )
    .into_iter()
    .find_map(|repo| difflore_core::infra::git::RepoScope::canonical(&repo))
    .map(difflore_core::infra::git::RepoScope::into_string)
}

fn print_overview(overview: &MemoryOverview) {
    println!("{}", style::title("Memory"));
    print_remembered(overview);
    print_needs_review(overview);
    print_paused(overview);
    print_sync(overview);
    print_activity(&overview.activity);
    print_latest_remembered(&overview.remembered.latest);
    print_latest_review_items(&overview.needs_review.latest);
    print_next(&overview.next);
}

fn print_remembered(overview: &MemoryOverview) {
    let repo_label = match overview.remembered.active_for_repo {
        Some(count) => format!(
            ", {} for this repo",
            count_phrase(count, "active rule", "active rules")
        ),
        None => String::new(),
    };
    println!(
        "  remembered     {} available to agents{}",
        style::ident(&count_phrase(
            overview.remembered.active_total,
            "active rule",
            "active rules"
        )),
        repo_label
    );
}

fn print_needs_review(overview: &MemoryOverview) {
    let total = overview.needs_review.local_drafts
        + overview.needs_review.local_discoveries
        + overview.needs_review.autopilot_needs_review_groups;
    let rendered = if total > 0 {
        style::warn(&count_phrase(total, "item", "items")).to_string()
    } else {
        style::ident("0 items").to_string()
    };
    println!(
        "  needs review   {} ({}, {}, {})",
        rendered,
        count_phrase(
            overview.needs_review.local_drafts,
            "local draft",
            "local drafts"
        ),
        count_phrase(
            overview.needs_review.local_discoveries,
            "session discovery",
            "session discoveries"
        ),
        count_phrase(
            overview.needs_review.autopilot_needs_review_groups,
            "suggestion",
            "suggestions"
        )
    );
}

fn print_paused(overview: &MemoryOverview) {
    println!(
        "  paused         {} kept out of recall",
        style::ident(&count_phrase(
            overview.paused.count,
            "disabled rule",
            "disabled rules"
        ))
    );
}

fn print_sync(overview: &MemoryOverview) {
    let auth = if overview.sync.logged_in {
        style::ok("logged in").to_string()
    } else {
        style::pewter("local only").to_string()
    };
    let pending = overview.sync.approved_session_candidates_pending_upload
        + overview.sync.activity_records_pending_upload;
    println!(
        "  sync           {} | {} pending upload",
        auth,
        count_phrase(pending, "record", "records")
    );
}

fn print_activity(activity: &ActivityOverview) {
    let empty_note = if activity.recall_calls > 0 {
        format!(
            ", {} empty",
            count_phrase(activity.empty_recalls, "recall", "recalls")
        )
    } else {
        String::new()
    };
    println!(
        "  value          {}: {}, {} surfaced{}",
        format!("{}d", activity.window_days),
        count_phrase(activity.recall_calls, "recall", "recalls"),
        count_phrase(activity.rules_surfaced, "rule", "rules"),
        empty_note
    );
}

fn print_latest_remembered(items: &[OverviewRule]) {
    if items.is_empty() {
        return;
    }
    println!();
    println!("{}", style::title("Recently remembered"));
    for item in items.iter().take(OVERVIEW_LATEST_LIMIT) {
        let repo = item
            .source_repo
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or("global");
        println!(
            "  {} {} {}",
            style::pewter(style::sym::BULLET),
            style::ident(&item.title),
            style::pewter(&format!("{} | {}", item.item_id, repo))
        );
    }
}

fn print_latest_review_items(items: &[OverviewReviewItem]) {
    if items.is_empty() {
        return;
    }
    println!();
    println!("{}", style::title("Needs review"));
    for item in items.iter().take(OVERVIEW_LATEST_LIMIT) {
        println!(
            "  {} {} {}",
            style::warn(style::sym::WARN),
            style::ident(&item.title),
            style::pewter(&item.item_id)
        );
        println!("    inspect: {}", style::cmd(&item.commands.show));
    }
}

fn print_next(next: &MemoryOverviewNextAction) {
    println!();
    match next.command.as_deref() {
        Some(command) => println!("next: {}", style::cmd(command)),
        None => println!("next: {}", style::ok(&next.label)),
    }
    println!("      {}", style::pewter(&next.reason));
}

fn count_phrase(count: i64, singular: &str, plural_word: &str) -> String {
    let noun = if count == 1 { singular } else { plural_word };
    format!("{count} {noun}")
}
