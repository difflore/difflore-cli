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

pub mod agent_exec;
pub mod cli;
pub mod clients;
pub mod commands;
mod dispatch;
pub mod error;
pub mod hook;
pub mod installer;
mod onboarding;
pub mod post_install_scan;
pub mod runtime;
pub mod session_mine;
pub mod style;
mod support;
mod tui_entry;

use cli::{Cli, Commands, StatusLane};

pub async fn run() {
    // Detect color support once so paint helpers stay cheap.
    let _ = style::detect_color_support();

    let matches = cli::build_cli().get_matches();
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(c) => c,
        Err(e) => {
            e.exit();
        }
    };
    // Cached startup gate: provider/cloud checks are best-effort and never
    // block command execution.
    let _ = difflore_core::infra::startup::ensure_ready(false).await;

    // Legacy layout migration guard. If an old global `context-index.db` is
    // present, the guard fails closed and leaves the file untouched; kept
    // best-effort so startup continues with rebuilt per-project state.
    if let Err(e) = difflore_core::migration::run_if_needed().await {
        eprintln!(
            "warning: DiffLore skipped an old index migration ({e}). \
             Run `difflore doctor --report` to inspect."
        );
    }

    let command = if let Some(command) = cli.command {
        command
    } else {
        // First-run state machine: bare `difflore` runs the wizard/welcome
        // once and hands off to the TUI dashboard; returning users (and
        // every non-TTY context) fall through to the compact status surface.
        match onboarding::first_run_path(cli.no_interactive).await {
            onboarding::FirstRunPath::LaunchWizard => {
                if !onboarding::run_wizard().await.should_continue_tui() {
                    return;
                }
                tui_entry::run_dashboard().await;
                return;
            }
            onboarding::FirstRunPath::ShowWelcome => {
                if !onboarding::show_welcome_then_continue()
                    .await
                    .should_continue_tui()
                {
                    return;
                }
                tui_entry::run_dashboard().await;
                return;
            }
            onboarding::FirstRunPath::Skip => {}
        }
        Commands::Status {
            json: false,
            lane: StatusLane::All,
        }
    };
    Box::pin(dispatch::dispatch(command)).await;
}
