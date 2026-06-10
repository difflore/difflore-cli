//! Bare-`difflore` bridge into the `difflore-tui` dashboard.
//!
//! The TUI crate is a pure renderer: it receives a [`difflore_tui::WiringSnapshot`]
//! built here (the CLI owns the installer / provider / cloud / daemon probes)
//! and returns a [`difflore_tui::TuiExit`] describing what to run *after* the
//! alt-screen has torn down. This module maps that exit protocol back onto the
//! dispatch table, so interactive follow-ups (`difflore init`, `difflore cloud
//! login`, `difflore providers setup`) run in the restored terminal.

use crate::cli::{CloudCommands, Commands, InitCliArgs, ProviderCommands, StatusLane};
use crate::{dispatch, installer, runtime};

/// Launch the dashboard after the welcome flow said `ContinueTui`.
///
/// Never panics and never leaves the user with nothing: if the dashboard
/// cannot start (stderr piped, terminal too dumb, raw-mode failure), we fall
/// back to the compact status surface — the pre-R3 behaviour of bare
/// `difflore`.
pub(crate) async fn run_dashboard() {
    let wiring = collect_wiring_snapshot().await;
    match difflore_tui::run(None, wiring).await {
        Ok(exit) => {
            if let Some(command) = follow_up_command(exit) {
                Box::pin(dispatch::dispatch(command)).await;
            }
        }
        Err(err) => {
            eprintln!("warning: DiffLore dashboard could not start ({err})");
            eprintln!("Showing the compact status view instead.");
            Box::pin(dispatch::dispatch(Commands::Status {
                json: false,
                lane: StatusLane::All,
            }))
            .await;
        }
    }
}

/// Map the TUI's exit protocol onto the dispatch table. `Quit` means the
/// user is done — no follow-up command.
const fn follow_up_command(exit: difflore_tui::TuiExit) -> Option<Commands> {
    match exit {
        difflore_tui::TuiExit::Quit => None,
        difflore_tui::TuiExit::RunInit => Some(Commands::Init(InitCliArgs { check: false })),
        difflore_tui::TuiExit::RunCloudLogin => Some(Commands::Cloud {
            command: CloudCommands::Login {
                token: None,
                browser: false,
                github: false,
            },
        }),
        difflore_tui::TuiExit::RunProvidersAdd => Some(Commands::Providers {
            command: ProviderCommands::Setup,
        }),
    }
}

/// Probe what's wired up on this machine for the Setup tab and the
/// onboarding modal. Every probe is best-effort: a failed probe reads as
/// "not wired", never better than reality (mirrors the snapshot's default).
async fn collect_wiring_snapshot() -> difflore_tui::WiringSnapshot {
    let mcp = installer::collect_status_snapshot();
    let agents_detected = count_to_u8(mcp.clients.iter().filter(|c| c.detected).count());
    let agents_installed = count_to_u8(
        mcp.clients
            .iter()
            .filter(|c| matches!(c.state, installer::InstallState::Installed))
            .count(),
    );

    let ctx = runtime::CommandContext::new(runtime::OutputMode::Text).await;
    let provider_name = active_provider_name(&ctx.db).await;
    let cloud_logged_in = ctx.cloud().await.is_logged_in();

    let daemon_running = matches!(
        difflore_core::infra::daemon::status(),
        difflore_core::infra::daemon::DaemonStatus::Running { .. }
    );

    difflore_tui::WiringSnapshot {
        agents_installed,
        agents_detected,
        provider_name,
        cloud_logged_in,
        daemon_running,
        pre_commit_installed: pre_commit_hook_is_difflore(),
    }
}

/// Resolved active provider's display name, defaulting to the first
/// configured provider when none is explicitly active (same resolution the
/// doctor table uses).
async fn active_provider_name(pool: &difflore_core::SqlitePool) -> Option<String> {
    let providers = difflore_core::domain::providers::list(pool).await.ok()?;
    providers
        .iter()
        .find(|p| p.is_active)
        .or_else(|| providers.first())
        .map(|p| p.name.clone())
}

/// True when the current repo's `pre-commit` hook is DiffLore's. Resolves
/// the git dir via `git rev-parse --git-dir` so worktrees (where `.git` is a
/// file) find the right `hooks/` location; other tools' hooks don't count.
fn pre_commit_hook_is_difflore() -> bool {
    let cwd = difflore_core::infra::paths::current_project_root();
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .current_dir(&cwd)
        .output();
    let git_dir = match output {
        Ok(o) if o.status.success() => {
            let raw = String::from_utf8_lossy(&o.stdout).trim().to_owned();
            if raw.is_empty() {
                return false;
            }
            let p = std::path::PathBuf::from(&raw);
            if p.is_absolute() { p } else { cwd.join(p) }
        }
        _ => return false,
    };
    let hook_path = git_dir.join("hooks").join("pre-commit");
    match std::fs::read_to_string(hook_path) {
        Ok(body) => body.contains("difflore"),
        Err(_) => false,
    }
}

fn count_to_u8(count: usize) -> u8 {
    u8::try_from(count).unwrap_or(u8::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quit_runs_nothing_after_the_dashboard() {
        assert!(follow_up_command(difflore_tui::TuiExit::Quit).is_none());
    }

    #[test]
    fn tui_exits_map_onto_the_dispatch_table() {
        assert!(matches!(
            follow_up_command(difflore_tui::TuiExit::RunInit),
            Some(Commands::Init(InitCliArgs { check: false }))
        ));
        assert!(matches!(
            follow_up_command(difflore_tui::TuiExit::RunCloudLogin),
            Some(Commands::Cloud {
                command: CloudCommands::Login {
                    token: None,
                    browser: false,
                    github: false,
                },
            })
        ));
        assert!(matches!(
            follow_up_command(difflore_tui::TuiExit::RunProvidersAdd),
            Some(Commands::Providers {
                command: ProviderCommands::Setup,
            })
        ));
    }

    #[test]
    fn agent_counts_saturate_instead_of_wrapping() {
        assert_eq!(count_to_u8(3), 3);
        assert_eq!(count_to_u8(usize::MAX), u8::MAX);
    }
}
