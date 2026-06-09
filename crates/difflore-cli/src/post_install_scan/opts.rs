//! Inputs to [`super::maybe_offer_import_reviews`].
//!
//! Kept in its own module so test harnesses can construct an
//! [`PostInstallScanOpts`] without pulling the whole module surface.

use std::path::PathBuf;

/// Default `--max-prs` for the post-install offer. Small enough to keep
/// the implied wait under ~2 minutes on a real PR-history repo; large
/// enough to almost always yield at least one extracted candidate.
pub const DEFAULT_MAX_PRS: u32 = 5;

/// Knobs for the post-install "import last N PRs?" offer. All fields are
/// public so the eventual one-line call site in `mcp_install/install.rs`
/// can use struct-update syntax without needing a builder.
#[derive(Debug, Clone)]
pub struct PostInstallScanOpts {
    /// Working directory the offer operates against. The guard layer
    /// uses this to detect the git repo + the gh-resolvable owner/repo.
    /// Almost always `std::env::current_dir()?` at the call site.
    pub cwd: PathBuf,

    /// When set, skip the prompt and treat the offer as declined. Lets
    /// the install path force-skip in scripts / `--no-interactive` mode
    /// without having to inspect ttys at the call site.
    pub non_interactive: bool,

    /// How many recent PRs to import on Yes. Forwarded as
    /// `difflore import-reviews --max-prs <N>`.
    pub max_prs: u32,
}

impl PostInstallScanOpts {
    /// Build a default opts bundle for `cwd`. Equivalent to the most
    /// common call site: "interactive, in this cwd, default cap".
    #[must_use]
    pub const fn for_cwd(cwd: PathBuf) -> Self {
        Self {
            cwd,
            non_interactive: false,
            max_prs: DEFAULT_MAX_PRS,
        }
    }
}
