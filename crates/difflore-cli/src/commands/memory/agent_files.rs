use difflore_core::infra::git::RepoScope;
use serde_json::json;

use crate::runtime::CommandContext;
use crate::style;
use crate::support::util::json_compact_or;

use super::exit_structured_err;

pub(crate) async fn handle_import_agent_files(ctx: &CommandContext, json: bool) {
    let repo_scope = detect_primary_repo_scope(ctx).await.unwrap_or_else(|| {
        exit_structured_err(
            "memory import-agent-files requires a GitHub/GitLab origin remote",
            json,
        )
    });
    let report = difflore_core::ingest::agent_files::import_agent_files_for_repo(
        &ctx.db,
        &ctx.project,
        &repo_scope,
    )
    .await
    .unwrap_or_else(|err| {
        exit_structured_err(&format!("failed to import agent files: {err}"), json)
    });

    if report.review_rules_pending > 0 {
        super::mark_memory_autopilot_dirty_best_effort(&ctx.db, "agent_files").await;
        super::schedule_memory_autopilot_best_effort(
            &ctx.db,
            "agent_files",
            difflore_core::memory_autopilot_schedule::EXPLICIT_AUTOPILOT_COOLDOWN_SECS,
        )
        .await;
    }

    if json {
        println!(
            "{}",
            json_compact_or(
                &json!({
                    "repo": repo_scope.as_str(),
                    "docsScanned": report.docs_scanned,
                    "entriesSeen": report.entries_seen,
                    "reviewRulesActive": report.review_rules_active,
                    "reviewRulesPending": report.review_rules_pending,
                    "softPreferencesActive": report.soft_preferences_active,
                    "referenceEntriesSkipped": report.reference_entries_skipped,
                    "deduped": report.deduped,
                    "sourcesDetected": report.sources_detected,
                }),
                "{}"
            )
        );
        return;
    }

    println!(
        "{} imported {} agent-file entries for {}",
        style::ok("Memory"),
        report.entries_seen,
        style::ident(repo_scope.as_str())
    );
    println!(
        "  review rules       {} active, {} pending",
        style::ident(&report.review_rules_active.to_string()),
        style::ident(&report.review_rules_pending.to_string())
    );
    println!(
        "  soft preferences   {} active",
        style::ident(&report.soft_preferences_active.to_string())
    );
    if report.reference_entries_skipped > 0 {
        println!(
            "  skipped references {}",
            style::ident(&report.reference_entries_skipped.to_string())
        );
    }
    if report.deduped > 0 {
        println!(
            "  deduped            {}",
            style::ident(&report.deduped.to_string())
        );
    }
    if report.review_rules_pending > 0 {
        println!(
            "  review             {}",
            style::cmd("difflore memory inbox")
        );
    }
}

async fn detect_primary_repo_scope(ctx: &CommandContext) -> Option<RepoScope> {
    let configured_gitlab_hosts = difflore_core::ingest::gitlab::auth::configured_hosts().await;
    let project = ctx.project.to_string_lossy();
    difflore_core::infra::git::detect_repo_full_names_with_gitlab_hosts(
        project.as_ref(),
        &configured_gitlab_hosts,
    )
    .first()
    .map(String::as_str)
    .and_then(RepoScope::canonical)
}
