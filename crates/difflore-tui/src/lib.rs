#![cfg_attr(test, allow(clippy::unwrap_used))]
//! `DiffLore` TUI — terminal dashboard for the rule library.
//!
//! Per the 2026-04-27 redesign the surface is four tabs:
//!   Rules · Activity · Team ↗ · Settings
//!
//! All editorial intent (edit rule body, publish to team, accept /
//! dismiss extraction) deep-links to difflore.dev. The TUI is
//! deliberately read-only beyond pane focus and filtering.

mod app;
mod error;
mod layout;
pub mod modals;
pub mod state;
mod tabs;
pub mod theme;
pub mod widgets;

pub use error::{Result, TuiError};

use std::path::PathBuf;

/// Snapshot of "what's wired up on this machine", consumed by the
/// Settings tab as the conversion compass. Loading lives in the CLI
/// (which has access to `mcp_install` + provider DB); the TUI is just a
/// renderer so it can stay free of CLI-only deps.
///
/// Defaulting to "nothing detected, nothing configured" is intentional
/// — a missing snapshot should never look better than reality.
#[derive(Debug, Default, Clone)]
pub struct WiringSnapshot {
    pub agents_installed: u8,
    pub agents_detected: u8,
    pub provider_name: Option<String>,
    pub cloud_logged_in: bool,
    /// True when the background outbox daemon is running. Optional per
    /// the launch brief — surfaced in Setup but never as a blocker.
    pub daemon_running: bool,
    /// True when the repo's `.git/hooks/pre-commit` shells into
    /// `difflore hook run`. Other tools' pre-commits don't count.
    pub pre_commit_installed: bool,
}

/// Action the TUI wants its caller to take after the alt-screen tears
/// down. Returning a value (instead of shelling out from inside raw
/// mode) keeps terminal state sane: the caller restores the cursor +
/// scrollback first, then runs whatever subprocess.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum TuiExit {
    /// User pressed `q` / `Esc`. No follow-up.
    #[default]
    Quit,
    /// Settings → `i`. Caller should run `difflore init` interactively
    /// in the user's restored terminal.
    RunInit,
    /// Settings → `l`. Caller should run `difflore cloud login`.
    RunCloudLogin,
    /// Settings → `a`. Caller should run `difflore providers setup`.
    RunProvidersAdd,
}

pub async fn run(project_dir: Option<PathBuf>, wiring: WiringSnapshot) -> Result<TuiExit> {
    // Refuse to launch in a non-TTY context: enable_raw_mode succeeds
    // but the alt-screen dump becomes useless ANSI noise in pipes /
    // CI logs / `claude` shells. Tell the caller what to use instead.
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
    let root = project_dir.unwrap_or_else(difflore_core::db::current_project_root);
    let app = app::App::new(root, wiring).await;
    app.run().await
}
