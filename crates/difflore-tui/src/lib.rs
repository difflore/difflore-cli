#![cfg_attr(test, allow(clippy::unwrap_used))]
//! `DiffLore` TUI — terminal dashboard for the rule library.
//!
//! Four tabs: Memory · Fixes · Cloud ↗ · Setup.
//!
//! All editorial intent (edit rule body, publish to team, accept / dismiss
//! extraction) deep-links to difflore.dev. The TUI is read-only beyond pane
//! focus and filtering.

mod app;
mod error;
pub mod modals;
pub mod plan;
mod tabs;
pub mod theme;
pub mod widgets;

pub use error::{Result, TuiError};

use std::path::PathBuf;

/// Snapshot of "what's wired up on this machine", consumed by the Settings tab.
/// Loaded in the CLI (which has the `mcp_install` + provider deps); the TUI is
/// a pure renderer. The default of "nothing detected" is intentional so a
/// missing snapshot never looks better than reality.
#[derive(Debug, Default, Clone)]
pub struct WiringSnapshot {
    pub agents_installed: u8,
    pub agents_detected: u8,
    pub provider_name: Option<String>,
    pub cloud_logged_in: bool,
    /// True when the background outbox daemon is running. Surfaced in Setup but
    /// never as a blocker.
    pub daemon_running: bool,
    /// True when the repo's `.git/hooks/pre-commit` shells into
    /// `difflore hook run`. Other tools' pre-commits don't count.
    pub pre_commit_installed: bool,
}

/// Action the TUI wants its caller to take after the alt-screen tears down.
/// Returning a value instead of shelling out from inside raw mode lets the
/// caller restore the terminal first, then run the subprocess.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum TuiExit {
    /// User pressed `q` / `Esc`. No follow-up.
    #[default]
    Quit,
    /// Caller should run `difflore init` interactively in the restored terminal.
    RunInit,
    /// Settings → `l`. Caller should run `difflore cloud login`.
    RunCloudLogin,
    /// Settings → `a`. Caller should run `difflore providers setup`.
    RunProvidersAdd,
}

pub async fn run(project_dir: Option<PathBuf>, wiring: WiringSnapshot) -> Result<TuiExit> {
    // Refuse to launch outside a TTY: enable_raw_mode succeeds but the
    // alt-screen dump becomes ANSI noise in pipes / CI logs. Point the caller
    // at non-TTY alternatives instead.
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() || !std::io::stderr().is_terminal() {
        return Err(TuiError::NotTty(String::from(
            "TUI needs a real terminal — stdout/stderr must both be TTYs here.\n\n  \
             For scripts / pipes, use:\n  \
               difflore rules list      · browse rules\n  \
               difflore impact          · daily summary\n  \
               difflore doctor          · status snapshot",
        )));
    }
    let root = project_dir.unwrap_or_else(difflore_core::infra::db::current_project_root);
    let app = app::App::new(root, wiring).await;
    app.run().await
}
