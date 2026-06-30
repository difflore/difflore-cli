//! Command dispatch: `Commands` variant in, handler call out.
//!
//! Module path = user command path (`commands/cloud/sync.rs` ↔
//! `difflore cloud sync`). Registered deviations from that rule:
//!
//! | command | handler | why |
//! |---|---|---|
//! | `difflore try` | `commands/try_demo.rs` | `try` is a Rust keyword |
//! | `difflore agents …` | `src/installer/` | one engine installs MCP + hooks across every agent; not a per-command module |
//! | `difflore mcp-server` | `difflore_core::mcp_server` | hidden stdio transport; the server lives in core |
//! | bare `difflore` | `commands/status/` | compact local readiness summary |

use crate::cli::{
    AgentsCommands, AuthCommands, CloudCommands, Commands, DistCommands, DraftsCommands,
    EmbeddingsCommands, FixCliArgs, ImportReviewsCliArgs, InitCliArgs, LearnCliArgs,
    MemoryCommands, ProviderCommands, RecallCliArgs, ReviewCliArgs, SkillsCommands, SyncCliArgs,
};
use crate::commands;
use crate::commands::cloud::sync::handle_sync;
use crate::commands::doctor::handle_doctor;
use crate::commands::init::handle_init;
use crate::commands::providers::{
    handle_providers_add, handle_providers_list, handle_providers_remove,
    handle_providers_set_active,
};
use crate::{installer, runtime};

/// Build a `CommandContext` whose output mode follows the parsed `--json` flag.
///
/// Collapses the `CommandContext::new(OutputMode::from_json_flag(json))`
/// boilerplate that nearly every dispatch arm repeats, so the policy "a
/// json-flagged command gets a json-mode context" is encoded in exactly one
/// place.
async fn ctx(json: bool) -> runtime::CommandContext {
    runtime::CommandContext::new(runtime::OutputMode::from_json_flag(json)).await
}

pub(crate) async fn dispatch(command: Commands) {
    match command {
        Commands::Try => commands::try_demo::handle_try().await,
        Commands::Init(args) => dispatch_init(args).await,
        Commands::Status { json, lane } => commands::status::handle_status(json, lane)
            .await
            .unwrap_or_else(|e| crate::support::util::render_cli_error(e)),
        Commands::Capabilities { json } => commands::capabilities::handle_capabilities(json),
        Commands::ImportReviews(args) => dispatch_import_reviews(args).await,
        Commands::Memory { json, command } => dispatch_memory(json, command).await,
        Commands::Learn(args) => dispatch_learn(args).await,
        Commands::Recall(args) => dispatch_recall(args).await,
        Commands::Review(args) => Box::pin(dispatch_review(args)).await,
        Commands::Fix(args) => Box::pin(dispatch_fix(args)).await,
        Commands::Ask { query, file, json } => {
            let ctx = ctx(json).await;
            commands::ask::handle_ask(&ctx, query, file, json).await;
        }
        Commands::Export(args) => {
            let ctx = ctx(args.json).await;
            commands::export::handle_export(&ctx, args.into()).await;
        }
        Commands::Drafts { command } => dispatch_drafts(command).await,
        Commands::Cloud { command } => Box::pin(dispatch_cloud(command)).await,
        Commands::Auth { command } => dispatch_auth(command).await,
        Commands::Agents { command } => dispatch_agents(command).await,
        Commands::Update { dry_run, force } => {
            let ctx = runtime::CommandContext::new(runtime::OutputMode::Text).await;
            commands::update::handle_update(&ctx, commands::update::UpdateArgs { dry_run, force })
                .await;
        }
        Commands::Providers { command } => dispatch_providers(command).await,
        Commands::Embeddings { command } => dispatch_embeddings(command).await,
        Commands::Eval {
            samples,
            golden,
            json,
        } => {
            if golden {
                // Golden eval is corpus-free (embedded fixture, isolated index),
                // so it needs no CommandContext / local DB.
                commands::eval::handle_golden_eval(json).await;
            } else {
                let ctx = ctx(json).await;
                commands::eval::handle_eval(&ctx, samples, json).await;
            }
        }
        Commands::Trajectory { review_id, json } => {
            let ctx = ctx(json).await;
            commands::trajectory::handle_trajectory(
                &ctx,
                commands::trajectory::TrajectoryArgs { review_id, json },
            )
            .await;
        }
        Commands::Doctor {
            report,
            fix,
            drain_abandoned,
            older_than,
            no_dry_run,
            json,
        } => {
            let mode = if json {
                runtime::OutputMode::Json
            } else {
                runtime::OutputMode::Text
            };
            let ctx = runtime::CommandContext::new(mode).await;
            handle_doctor(
                &ctx,
                commands::doctor::DoctorArgs {
                    report,
                    fix,
                    drain_abandoned,
                    older_than,
                    no_dry_run,
                    json,
                },
            )
            .await;
        }
        Commands::McpServer => {
            let ctx = runtime::CommandContext::new(runtime::OutputMode::Json).await;
            maybe_host_hook_forwarder_in_mcp_server(&ctx.db);
            if let Err(e) = difflore_core::mcp_server::run(ctx.db).await {
                eprintln!("DiffLore memory server failed: {e}");
            }
        }
        Commands::HookDaemon { project_hash } => {
            // The warm hook-forward daemon. Spawned detached by the
            // `difflore-hook` shim on a cache miss; serves exactly one project
            // (its index pool is frozen from `project_hash`, not the daemon's
            // cwd). No CommandContext / startup gate: it must not run network
            // probes, and it manages its own db/index lifecycle.
            if let Err(e) = crate::hook::forward::run_server_for_hash(&project_hash).await {
                eprintln!("DiffLore hook daemon exited: {e:#}");
            }
        }
        Commands::OutboxDaemon {
            tick_interval_secs,
            batch_size,
        } => {
            if let Err(e) = difflore_core::infra::daemon::run(tick_interval_secs, batch_size).await
            {
                eprintln!("DiffLore outbox daemon exited: {e}");
            }
        }
        Commands::Skills { command } => dispatch_skills(command).await,
        // Single-variant subcommand matched inline; no dispatch helper needed.
        Commands::Dist {
            command: DistCommands::Verify { json },
        } => commands::dist::handle_verify(json),
    }
}

async fn dispatch_learn(args: LearnCliArgs) {
    let ctx = ctx(args.json).await;
    commands::learn::handle_learn(&ctx, args).await;
}

async fn dispatch_drafts(command: DraftsCommands) {
    match command {
        DraftsCommands::List { repo, limit, json } => {
            commands::drafts::handle_list(repo, limit, json).await;
        }
        DraftsCommands::Show { id, json } => {
            commands::drafts::handle_show(id, json).await;
        }
        DraftsCommands::Review { repo, limit } => {
            commands::drafts::handle_review(repo, limit).await;
        }
        DraftsCommands::Approve {
            id,
            all,
            repo,
            yes,
            json,
        } => {
            commands::drafts::handle_approve(id, all, repo, yes, json).await;
        }
        DraftsCommands::Reject {
            id,
            all,
            repo,
            yes,
            json,
        } => {
            commands::drafts::handle_reject(id, all, repo, yes, json).await;
        }
    }
}

async fn dispatch_memory(root_json: bool, command: Option<MemoryCommands>) {
    match command {
        None => {
            let ctx = ctx(root_json).await;
            commands::memory::handle_summary(&ctx, root_json).await;
        }
        Some(MemoryCommands::Inbox { all, limit, json }) => {
            let ctx = ctx(json).await;
            commands::memory::handle_inbox(&ctx, all, limit, json).await;
        }
        Some(MemoryCommands::Active { all, limit, json }) => {
            let ctx = ctx(json).await;
            commands::memory::handle_active(&ctx, all, limit, json).await;
        }
        Some(MemoryCommands::Activity { days, limit, json }) => {
            let ctx = ctx(json).await;
            commands::memory::handle_activity(&ctx, days, limit, json).await;
        }
        Some(MemoryCommands::Show { item_id, json }) => {
            let ctx = ctx(json).await;
            commands::memory::handle_show(&ctx, item_id, json).await;
        }
        Some(MemoryCommands::Remember {
            title,
            body,
            file_patterns,
            bad_code,
            good_code,
            severity,
            json,
        }) => {
            let ctx = ctx(json).await;
            commands::memory::handle_remember(
                &ctx,
                title,
                body,
                file_patterns,
                bad_code,
                good_code,
                severity,
                json,
            )
            .await;
        }
        Some(MemoryCommands::ImportAgentFiles { json }) => {
            let ctx = ctx(json).await;
            commands::memory::handle_import_agent_files(&ctx, json).await;
        }
        Some(MemoryCommands::Review { limit }) => {
            let ctx = runtime::CommandContext::new(runtime::OutputMode::Text).await;
            commands::memory::handle_review(&ctx, limit).await;
        }
        Some(MemoryCommands::Autopilot {
            dry_run,
            max_auto_enable,
            json,
            background,
            lease_owner,
        }) => {
            let ctx = ctx(json).await;
            commands::memory::handle_autopilot(
                &ctx,
                dry_run,
                max_auto_enable,
                json,
                background,
                lease_owner,
            )
            .await;
        }
        Some(MemoryCommands::Cleanup { apply, limit, json }) => {
            let ctx = ctx(json).await;
            commands::memory::handle_cleanup(&ctx, apply, limit, json).await;
        }
        Some(MemoryCommands::Digest { limit, json }) => {
            let ctx = ctx(json).await;
            commands::memory::handle_digest(&ctx, limit, json).await;
        }
        Some(MemoryCommands::Recommended { all, limit, json }) => {
            let ctx = ctx(json).await;
            commands::memory::handle_recommended(&ctx, all, limit, json).await;
        }
        Some(MemoryCommands::Log { limit, json }) => {
            let ctx = ctx(json).await;
            commands::memory::handle_log(&ctx, limit, json).await;
        }
        Some(MemoryCommands::Conflicts {
            limit,
            status,
            json,
        }) => {
            let ctx = ctx(json).await;
            commands::memory::handle_conflicts(&ctx, limit, status, json).await;
        }
        Some(MemoryCommands::Disable {
            rule_id,
            reason,
            json,
        }) => {
            let ctx = ctx(json).await;
            commands::memory::handle_disable(&ctx, rule_id, reason, json).await;
        }
        Some(MemoryCommands::Approve { item_id, json }) => {
            let ctx = ctx(json).await;
            commands::memory::handle_approve(&ctx, item_id, json).await;
        }
        Some(MemoryCommands::Reject { item_id, json }) => {
            let ctx = ctx(json).await;
            commands::memory::handle_reject(&ctx, item_id, json).await;
        }
        Some(MemoryCommands::TeamCandidates {
            team_id,
            limit,
            offset,
            status,
            json,
            command,
        }) => {
            let command_json = command.as_ref().is_some_and(|command| match command {
                crate::cli::TeamCandidateCommands::Count { json, .. }
                | crate::cli::TeamCandidateCommands::Show { json, .. }
                | crate::cli::TeamCandidateCommands::Approve { json, .. }
                | crate::cli::TeamCandidateCommands::Reject { json, .. } => *json,
            });
            let effective_json = root_json || json || command_json;
            let ctx = ctx(effective_json).await;
            commands::memory::handle_team_candidates(
                &ctx,
                commands::memory::TeamCandidateListArgs {
                    team_id,
                    limit,
                    offset,
                    status,
                    json: effective_json,
                    command,
                },
            )
            .await;
        }
        Some(MemoryCommands::Sync(args)) => {
            let ctx = ctx(args.json).await;
            commands::memory::handle_sync(&ctx, args.into()).await;
        }
        Some(MemoryCommands::ExportPackage {
            output,
            format,
            dry_run,
            json,
            local_only,
            max_rules,
        }) => {
            let ctx = ctx(json).await;
            commands::memory::handle_export_package(
                &ctx,
                output,
                format,
                dry_run,
                json,
                local_only,
                max_rules.map(|n| usize::try_from(n).unwrap_or(usize::MAX)),
            )
            .await;
        }
        Some(MemoryCommands::ImportPackage {
            source,
            dry_run,
            json,
        }) => {
            let ctx = ctx(json).await;
            commands::memory::handle_import_package(&ctx, source, dry_run, json).await;
        }
        Some(MemoryCommands::Drafts { command }) => dispatch_drafts(command).await,
    }
}

async fn dispatch_skills(command: SkillsCommands) {
    match command {
        SkillsCommands::Sweep {
            no_dry_run,
            decay_factor,
            days,
            quarantine_unguided,
        } => {
            let ctx = runtime::CommandContext::new(runtime::OutputMode::Json).await;
            commands::skills::sweep::handle_sweep(
                &ctx,
                commands::skills::sweep::SweepArgs {
                    dry_run: !no_dry_run,
                    decay_factor,
                    days,
                    quarantine_unguided,
                },
            )
            .await;
        }
        SkillsCommands::BackfillAttribution { no_dry_run } => {
            let ctx = runtime::CommandContext::new(runtime::OutputMode::Text).await;
            commands::skills::backfill::handle_backfill_attribution(
                &ctx,
                commands::skills::backfill::BackfillArgs {
                    dry_run: !no_dry_run,
                },
            )
            .await;
        }
    }
}

async fn dispatch_fix(args: FixCliArgs) {
    let ctx = ctx(args.json).await;
    Box::pin(commands::fix::handle_fix(&ctx, args.into())).await;
}

async fn dispatch_review(args: ReviewCliArgs) {
    let ctx = ctx(args.json).await;
    Box::pin(commands::fix::handle_fix(&ctx, args.into())).await;
}

async fn dispatch_sync(args: SyncCliArgs) {
    let ctx = ctx(args.json).await;
    handle_sync(&ctx, args.into()).await;
}

async fn dispatch_import_reviews(args: ImportReviewsCliArgs) {
    let ctx = ctx(args.json).await;
    commands::import_reviews::handle(&ctx, args.into())
        .await
        .unwrap_or_else(|e| crate::support::util::render_cli_error(e));
}

async fn dispatch_recall(args: RecallCliArgs) {
    let ctx = ctx(args.json).await;
    commands::recall::handle_recall(&ctx, args.into()).await;
}

async fn dispatch_providers(command: ProviderCommands) {
    match command {
        ProviderCommands::List { json } => {
            let ctx = ctx(json).await;
            handle_providers_list(&ctx, json)
                .await
                .unwrap_or_else(|e| crate::support::util::render_cli_error(e));
        }
        ProviderCommands::Setup => {
            let ctx = runtime::CommandContext::new(runtime::OutputMode::Text).await;
            commands::providers::setup::run_setup(&ctx.db).await;
        }
        ProviderCommands::Add { tool, model } => {
            let ctx = runtime::CommandContext::new(runtime::OutputMode::Text).await;
            handle_providers_add(&ctx, &tool, model.as_deref())
                .await
                .unwrap_or_else(|e| crate::support::util::render_cli_error(e));
        }
        ProviderCommands::SetActive { id } => {
            let ctx = runtime::CommandContext::new(runtime::OutputMode::Text).await;
            handle_providers_set_active(&ctx, &id)
                .await
                .unwrap_or_else(|e| crate::support::util::render_cli_error(e));
        }
        ProviderCommands::Remove { id, yes } => {
            let ctx = runtime::CommandContext::new(runtime::OutputMode::Text).await;
            handle_providers_remove(&ctx, &id, yes)
                .await
                .unwrap_or_else(|e| crate::support::util::render_cli_error(e));
        }
    }
}

async fn dispatch_cloud(command: CloudCommands) {
    match command {
        CloudCommands::Status { json } => commands::cloud::handle_status(json).await,
        CloudCommands::Login {
            token,
            browser,
            github,
        } => {
            commands::cloud::handle_login_dispatch(token, browser, github).await;
        }
        CloudCommands::Sync(args) => dispatch_sync(args).await,
        CloudCommands::Team { json } => commands::cloud::handle_team(json).await,
        CloudCommands::Publish {
            rule,
            team_id,
            enforcement,
            json,
        } => commands::cloud::handle_publish(rule, enforcement, team_id, json).await,
        CloudCommands::Unpublish {
            rule,
            team_id,
            json,
        } => commands::cloud::handle_unpublish(rule, team_id, json).await,
        CloudCommands::Impact { json } => {
            let ctx = ctx(json).await;
            Box::pin(commands::cloud::handle_impact(&ctx, json)).await;
        }
        CloudCommands::Logout => commands::cloud::handle_logout()
            .await
            .unwrap_or_else(|e| crate::support::util::render_cli_error(e)),
    }
}

async fn dispatch_auth(command: AuthCommands) {
    match command {
        // No CommandContext: the PAT store lives in the standalone auth db,
        // and credential commands must not run the startup network probes.
        AuthCommands::Gitlab {
            host,
            check,
            remove,
        } => commands::auth::gitlab::handle_gitlab(host, check, remove).await,
    }
}

fn maybe_host_hook_forwarder_in_mcp_server(db: &difflore_core::SqlitePool) {
    if crate::hook::forward::Mode::from_env() == crate::hook::forward::Mode::Never {
        return;
    }

    let project_hash = crate::hook::forward::protocol::current_project_hash();
    for hash in mcp_hosted_forwarder_hashes(&project_hash) {
        let db = db.clone();
        tokio::spawn(async move {
            if let Err(e) =
                crate::hook::forward::run_server_for_hash_for_process_lifetime(&hash, db).await
                && difflore_core::infra::env::trace_hook()
            {
                eprintln!("DiffLore MCP-hosted hook forwarder exited for {hash}: {e:#}");
            }
        });
    }
}

const MCP_HOSTED_FORWARDER_HASH_LIMIT: usize = 8;

fn mcp_hosted_forwarder_hashes(current: &str) -> Vec<String> {
    let mut hashes = vec![current.to_owned()];
    let Ok(home) = difflore_core::infra::paths::data_home() else {
        return hashes;
    };
    let projects = home.join("projects");
    let Ok(entries) = std::fs::read_dir(projects) else {
        return hashes;
    };
    let mut candidates = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name == current || !looks_like_project_hash(&name) {
            continue;
        }
        if difflore_core::context::index_db::index_db_path_for_project(&name).exists() {
            let modified = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            candidates.push(HostedForwarderCandidate {
                hash: name,
                modified,
            });
        }
    }
    sort_hosted_forwarder_candidates_by_recency(&mut candidates);
    hashes.extend(
        candidates
            .into_iter()
            .take(MCP_HOSTED_FORWARDER_HASH_LIMIT.saturating_sub(hashes.len()))
            .map(|candidate| candidate.hash),
    );
    hashes
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HostedForwarderCandidate {
    hash: String,
    modified: std::time::SystemTime,
}

fn sort_hosted_forwarder_candidates_by_recency(candidates: &mut [HostedForwarderCandidate]) {
    candidates.sort_by(|a, b| {
        b.modified
            .cmp(&a.modified)
            .then_with(|| a.hash.cmp(&b.hash))
    });
}

fn looks_like_project_hash(value: &str) -> bool {
    value.len() == 12 && value.chars().all(|ch| ch.is_ascii_hexdigit())
}

async fn dispatch_init(args: InitCliArgs) {
    let ctx = runtime::CommandContext::new(runtime::OutputMode::Text).await;
    if let Err(e) = handle_init(&ctx, commands::init::InitOptions { check: args.check }).await {
        crate::support::util::render_cli_error(e);
    }
}

async fn dispatch_agents(command: AgentsCommands) {
    match command {
        AgentsCommands::Install { dry_run } => {
            let fresh_install = installer::install_all(dry_run);
            // Post-install touchpoint: offer to seed memory from the current
            // repo's recent PRs. Only after a fresh, fully-successful real
            // install; the helper's guards silently skip CI / non-tty /
            // non-GitHub contexts and it never errors back into the install
            // flow.
            if fresh_install {
                let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
                let opts = crate::post_install_scan::PostInstallScanOpts::for_cwd(cwd);
                let _outcome = crate::post_install_scan::maybe_offer_import_reviews(&opts);
            }
        }
        AgentsCommands::Uninstall { dry_run } => installer::uninstall_all(dry_run),
        AgentsCommands::Status { json } => installer::status(json),
        AgentsCommands::Update { dry_run, force } => installer::update_all(dry_run, force),
    }
}

async fn dispatch_embeddings(command: EmbeddingsCommands) {
    match command {
        EmbeddingsCommands::Status { json } => {
            commands::embeddings::handle_status(json).await;
        }
        EmbeddingsCommands::Setup {
            provider_url,
            model,
            dim,
            key,
            no_key,
        } => {
            commands::embeddings::handle_setup(provider_url, model, dim, key, no_key).await;
        }
        EmbeddingsCommands::Disable => {
            commands::embeddings::handle_disable().await;
        }
        EmbeddingsCommands::Rebuild { json } => {
            commands::embeddings::handle_rebuild(json).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        HostedForwarderCandidate, looks_like_project_hash,
        sort_hosted_forwarder_candidates_by_recency,
    };
    use std::time::{Duration, SystemTime};

    #[test]
    fn project_hash_filter_accepts_only_twelve_hex_chars() {
        assert!(looks_like_project_hash("abcdef123456"));
        assert!(looks_like_project_hash("ABCDEF123456"));
        assert!(!looks_like_project_hash("abcdef12345"));
        assert!(!looks_like_project_hash("abcdef1234567"));
        assert!(!looks_like_project_hash("not-a-hash"));
        assert!(!looks_like_project_hash("zzzzzz123456"));
    }

    #[test]
    fn hosted_forwarder_candidates_sort_by_recent_mtime_then_hash() {
        let base = SystemTime::UNIX_EPOCH;
        let mut candidates = vec![
            HostedForwarderCandidate {
                hash: "cccccccccccc".to_owned(),
                modified: base + Duration::from_secs(10),
            },
            HostedForwarderCandidate {
                hash: "bbbbbbbbbbbb".to_owned(),
                modified: base + Duration::from_secs(30),
            },
            HostedForwarderCandidate {
                hash: "aaaaaaaaaaaa".to_owned(),
                modified: base + Duration::from_secs(30),
            },
        ];

        sort_hosted_forwarder_candidates_by_recency(&mut candidates);

        let hashes = candidates
            .into_iter()
            .map(|candidate| candidate.hash)
            .collect::<Vec<_>>();
        assert_eq!(
            hashes,
            vec![
                "aaaaaaaaaaaa".to_owned(),
                "bbbbbbbbbbbb".to_owned(),
                "cccccccccccc".to_owned(),
            ]
        );
    }
}
