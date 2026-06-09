#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::many_single_char_names
    )
)]

use clap::FromArgMatches;

pub mod cli;
pub mod commands;
mod dispatch;
pub mod error;
pub mod hook_cache;
pub mod hook_forward;
pub mod hook_runtime;
pub mod hooks;
pub mod mcp_install;
pub mod runtime;
pub mod style;

use cli::{Cli, Commands, StatusLane};

pub async fn run() {
    // Detect color support once so paint helpers stay cheap afterwards.
    let _ = style::detect_color_support();

    let matches = cli::build_cli().get_matches();
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(c) => c,
        Err(e) => {
            e.exit();
        }
    };
    // Cached startup gate: provider/cloud checks are best-effort
    // and never block command execution.
    let _ = difflore_core::startup::ensure_ready(false).await;

    // Retired layout migration guard: new installs create per-project
    // indexes directly. If an old global `context-index.db` is present,
    // the guard fails closed and leaves the file untouched. Keep this
    // best-effort so startup can continue with rebuilt per-project state.
    if let Err(e) = difflore_core::migration::run_if_needed().await {
        eprintln!(
            "[difflore] warning: retired per-project index migration refused old state ({e}). \
             Run `difflore doctor --report` to inspect."
        );
    }

    let command = if let Some(command) = cli.command {
        command
    } else {
        // First-run state machine: bare `difflore` can still run the
        // wizard/welcome once, then falls through to the compact status
        // surface instead of opening a separate TUI.
        match commands::welcome::first_run_path(cli.no_interactive).await {
            commands::welcome::FirstRunPath::LaunchWizard => {
                if !commands::welcome::run_wizard().await.should_continue_tui() {
                    return;
                }
            }
            commands::welcome::FirstRunPath::ShowWelcome => {
                if !commands::welcome::show_welcome_then_continue()
                    .await
                    .should_continue_tui()
                {
                    return;
                }
            }
            commands::welcome::FirstRunPath::Skip => {}
        }
        Commands::Status {
            json: false,
            lane: StatusLane::All,
        }
    };
    Box::pin(dispatch::dispatch(command)).await;
}
