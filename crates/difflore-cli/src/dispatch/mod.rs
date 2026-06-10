use crate::cli::{
    AgentsCommands, CloudCommands, Commands, DistCommands, EmbeddingsCommands, FixCliArgs,
    ImportReviewsCliArgs, InitCliArgs, PacksCommands, ProviderCommands, RecallCliArgs,
    SkillsCommands, SyncCliArgs,
};
use crate::commands;
use crate::commands::doctor::handle_doctor;
use crate::commands::init::handle_init;
use crate::commands::providers::{
    handle_providers_add, handle_providers_list, handle_providers_remove,
    handle_providers_set_active,
};
use crate::commands::sync::handle_sync;
use crate::{mcp_install, runtime};

pub(crate) async fn dispatch(command: Commands) {
    match command {
        Commands::Try => commands::try_demo::handle_try().await,
        Commands::Init(args) => dispatch_init(args).await,
        Commands::Status { json, lane } => commands::status::handle_status(json, lane).await,
        Commands::ImportReviews(args) => dispatch_import_reviews(args).await,
        Commands::Recall(args) => dispatch_recall(args).await,
        Commands::Fix(args) => dispatch_fix(args).await,
        Commands::Ask { query, file, json } => {
            let ctx = runtime::CommandContext::new(runtime::OutputMode::from_json_flag(json)).await;
            commands::ask::handle_ask(&ctx, query, file, json).await;
        }
        Commands::Cloud { command } => Box::pin(dispatch_cloud(command)).await,
        Commands::Agents { command } => dispatch_agents(command).await,
        Commands::Providers { command } => dispatch_providers(command).await,
        Commands::Embeddings { command } => dispatch_embeddings(command).await,
        Commands::Eval { samples, json } => {
            let ctx = runtime::CommandContext::new(runtime::OutputMode::from_json_flag(json)).await;
            commands::eval::handle_eval(&ctx, samples, json).await;
        }
        Commands::Trajectory { review_id, json } => {
            let ctx = runtime::CommandContext::new(runtime::OutputMode::from_json_flag(json)).await;
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
            if let Err(e) = difflore_core::mcp_server::run(ctx.db).await {
                eprintln!("DiffLore memory server failed: {e}");
            }
        }
        Commands::Skills { command } => dispatch_skills(command).await,
        Commands::Packs { command } => dispatch_packs(command).await,
        // Single-variant subcommand matched inline; no dispatch helper needed.
        Commands::Dist {
            command: DistCommands::Verify { json },
        } => commands::dist::handle_verify(json),
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

async fn dispatch_packs(command: PacksCommands) {
    match command {
        PacksCommands::List {
            registry,
            installed,
            json,
        } => {
            commands::packs::handle_list(registry, installed, json).await;
        }
        PacksCommands::Show {
            pack_id,
            registry,
            json,
        } => {
            commands::packs::handle_show(pack_id, registry, json).await;
        }
        PacksCommands::Install {
            pack_id,
            registry,
            dry_run,
            yes,
            json,
        } => {
            commands::packs::handle_install(pack_id, registry, dry_run, yes, json).await;
        }
        PacksCommands::Installed { json } => {
            commands::packs::handle_installed(json).await;
        }
        PacksCommands::Uninstall { pack_id, yes, json } => {
            commands::packs::handle_uninstall(pack_id, yes, json).await;
        }
        PacksCommands::Publish {
            path,
            registry,
            json,
        } => {
            commands::packs::handle_publish(path, registry, json).await;
        }
    }
}

async fn dispatch_fix(args: FixCliArgs) {
    let ctx = runtime::CommandContext::new(runtime::OutputMode::from_json_flag(args.json)).await;
    commands::fix::handle_fix(&ctx, args.into()).await;
}

async fn dispatch_sync(args: SyncCliArgs) {
    let ctx = runtime::CommandContext::new(runtime::OutputMode::from_json_flag(args.json)).await;
    handle_sync(&ctx, args.into()).await;
}

async fn dispatch_import_reviews(args: ImportReviewsCliArgs) {
    let ctx = runtime::CommandContext::new(runtime::OutputMode::from_json_flag(args.json)).await;
    commands::import_reviews::handle(&ctx, args.into()).await;
}

async fn dispatch_recall(args: RecallCliArgs) {
    let ctx = runtime::CommandContext::new(runtime::OutputMode::from_json_flag(args.json)).await;
    commands::recall::handle_recall(&ctx, args.into()).await;
}

async fn dispatch_providers(command: ProviderCommands) {
    match command {
        ProviderCommands::List { json } => {
            let ctx = runtime::CommandContext::new(runtime::OutputMode::from_json_flag(json)).await;
            handle_providers_list(&ctx, json).await;
        }
        ProviderCommands::Setup => {
            let ctx = runtime::CommandContext::new(runtime::OutputMode::Text).await;
            commands::providers::setup::run_setup(&ctx.db).await;
        }
        ProviderCommands::Add { tool, model } => {
            let ctx = runtime::CommandContext::new(runtime::OutputMode::Text).await;
            handle_providers_add(&ctx, &tool, model.as_deref()).await;
        }
        ProviderCommands::SetActive { id } => {
            let ctx = runtime::CommandContext::new(runtime::OutputMode::Text).await;
            handle_providers_set_active(&ctx, &id).await;
        }
        ProviderCommands::Remove { id, yes } => {
            let ctx = runtime::CommandContext::new(runtime::OutputMode::Text).await;
            handle_providers_remove(&ctx, &id, yes).await;
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
            let ctx = runtime::CommandContext::new(runtime::OutputMode::from_json_flag(json)).await;
            Box::pin(commands::cloud::handle_impact(&ctx, json)).await;
        }
        CloudCommands::Logout => commands::cloud::handle_logout().await,
    }
}

async fn dispatch_init(args: InitCliArgs) {
    let ctx = runtime::CommandContext::new(runtime::OutputMode::Text).await;
    handle_init(&ctx, commands::init::InitOptions { check: args.check }).await;
}

async fn dispatch_agents(command: AgentsCommands) {
    match command {
        AgentsCommands::Install { dry_run } => mcp_install::install_all(dry_run),
        AgentsCommands::Uninstall { dry_run } => mcp_install::uninstall_all(dry_run),
        AgentsCommands::Status { json } => mcp_install::status(json),
        AgentsCommands::Update { dry_run, force } => mcp_install::update_all(dry_run, force),
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
