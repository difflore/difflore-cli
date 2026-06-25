//! What [`super::maybe_offer_import_reviews`] returns to its caller.

/// Reason post-install onboarding did not queue background work. Distinct from
/// [`PostInstallScanOutcome::Declined`] for compatibility with the old prompt
/// flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// `cwd` is not inside a git repo (or `git` itself failed). The
    /// review import only makes sense in a real repo.
    NotAGitRepo,
    /// `git remote get-url origin` did not point at a parseable
    /// owner/repo, so we can't even tell which GitHub repo to scan.
    NoGitHubRemote,
    /// `gh` is not on PATH. The post-install import path shells out to `gh`,
    /// so this offer cannot run on this machine yet.
    GhNotInstalled,
    /// `non_interactive` was set, so background onboarding should not run.
    NonInteractive,
    /// One of the common CI environment variables is set (`CI`,
    /// `GITHUB_ACTIONS`, `GITLAB_CI`). Same reasoning as
    /// `NonInteractive` but a separate variant so logs / tests can
    /// distinguish them.
    RunningInCi,
    /// `DIFFLORE_SKIP_POST_INSTALL_SCAN=1` (or `true`/`yes`) is set.
    ExplicitlySkipped,
}

/// Result of post-install onboarding. `Skipped` and `Declined` are both
/// "do nothing more"; the distinction is kept for compatibility.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostInstallScanOutcome {
    /// Offer was never shown. See [`SkipReason`] for the precondition
    /// that wasn't met.
    Skipped { reason: SkipReason },

    /// Legacy prompt path: prompt was shown and the user declined.
    Declined,

    /// Import worker was queued. The counters are advisory; the local DB is
    /// authoritative after the background worker finishes.
    ImportedReviews { pr_count: u32, rule_count: u32 },

    /// A background worker could not be queued. Treated as recoverable.
    ImportFailed { error: String },
}

impl PostInstallScanOutcome {
    /// True iff onboarding work was queued or failed while queuing.
    #[must_use]
    pub const fn user_engaged(&self) -> bool {
        matches!(
            self,
            Self::ImportedReviews { .. } | Self::ImportFailed { .. }
        )
    }
}
