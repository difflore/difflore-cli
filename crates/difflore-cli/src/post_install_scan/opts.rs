//! Inputs to [`super::maybe_offer_import_reviews`], in their own module so
//! test harnesses can construct a [`PostInstallScanOpts`] without pulling in
//! the whole module surface.

use std::path::PathBuf;

/// Default `--max-prs` for the post-install offer. Small enough to keep the
/// wait under ~2 minutes on a real PR-history repo, large enough to almost
/// always yield at least one extracted candidate.
pub const DEFAULT_MAX_PRS: u32 = 5;

/// Knobs for the post-install "import last N PRs?" offer.
#[derive(Debug, Clone)]
pub struct PostInstallScanOpts {
    /// Working directory the offer operates against; used to detect the git
    /// repo and the gh-resolvable owner/repo.
    pub cwd: PathBuf,

    /// When set, skip the prompt and treat the offer as declined (for scripts
    /// and `--no-interactive` mode).
    pub non_interactive: bool,

    /// How many recent PRs to import on Yes. Forwarded as
    /// `difflore import-reviews --max-prs <N>`.
    pub max_prs: u32,
}

impl PostInstallScanOpts {
    /// Build a default opts bundle for `cwd`: interactive, default cap.
    #[must_use]
    pub const fn for_cwd(cwd: PathBuf) -> Self {
        Self {
            cwd,
            non_interactive: false,
            max_prs: DEFAULT_MAX_PRS,
        }
    }
}
