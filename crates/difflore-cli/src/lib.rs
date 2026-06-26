#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::many_single_char_names
    )
)]

macro_rules! print {
    ($($arg:tt)*) => {
        $crate::support::stdio::safe_print(format_args!($($arg)*))
    };
}

macro_rules! println {
    () => {
        $crate::support::stdio::safe_println(format_args!(""))
    };
    ($($arg:tt)*) => {
        $crate::support::stdio::safe_println(format_args!($($arg)*))
    };
}

use clap::FromArgMatches;

pub mod agent_exec;
pub mod cli;
pub mod clients;
pub mod commands;
mod dispatch;
pub mod error;
pub mod hook;
pub mod installer;
pub mod post_install_scan;
pub mod runtime;
pub mod session_mine;
pub mod style;
mod support;

use cli::{Cli, Commands, MemoryCommands, StatusLane};

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

    let command = match cli.command {
        Some(command) => command,
        None => Commands::Status {
            json: false,
            lane: StatusLane::All,
        },
    };

    // These commands must not pay the startup gate: the hook daemon and
    // background autopilot are internal workers, while capabilities is a fast
    // local contract read.
    if matches!(
        command,
        Commands::HookDaemon { .. }
            | Commands::OutboxDaemon { .. }
            | Commands::Memory {
                command: Some(MemoryCommands::Autopilot {
                    background: true,
                    ..
                }),
                ..
            }
            | Commands::Capabilities { .. }
    ) {
        Box::pin(dispatch::dispatch(command)).await;
        return;
    }

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

    Box::pin(dispatch::dispatch(command)).await;
}
