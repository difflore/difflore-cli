//! Inputs to [`super::maybe_offer_import_reviews`], in their own module so
//! test harnesses can construct a [`PostInstallScanOpts`] without pulling in
//! the whole module surface.

use std::path::PathBuf;

/// Default `--max-prs` for the bounded post-install import.
pub const DEFAULT_MAX_PRS: u32 = 50;

/// Only look at recently merged PRs during install-time seeding.
pub const DEFAULT_SINCE_DAYS: i64 = 120;

/// Hidden child-process wall-clock budget for the background import.
pub const DEFAULT_WALL_TIMEOUT_SECS: u64 = 20;

/// Knobs for the post-install "import last N PRs?" offer.
#[derive(Debug, Clone)]
pub struct PostInstallScanOpts {
    /// Working directory the offer operates against; used to detect the git
    /// repo and the gh-resolvable owner/repo.
    pub cwd: PathBuf,

    /// When set, skip the prompt and treat the offer as declined (for scripts
    /// and `--no-interactive` mode).
    pub non_interactive: bool,

    /// How many recent PRs to import in the bounded background worker.
    pub max_prs: u32,

    /// How many days of history to include.
    pub since_days: i64,

    /// Child-process wall-clock cap, in seconds.
    pub wall_timeout_secs: u64,
}

impl PostInstallScanOpts {
    /// Build a default opts bundle for `cwd`: interactive, default cap.
    #[must_use]
    pub const fn for_cwd(cwd: PathBuf) -> Self {
        Self {
            cwd,
            non_interactive: false,
            max_prs: DEFAULT_MAX_PRS,
            since_days: DEFAULT_SINCE_DAYS,
            wall_timeout_secs: DEFAULT_WALL_TIMEOUT_SECS,
        }
    }
}
