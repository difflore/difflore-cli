//! What [`super::maybe_offer_import_reviews`] returns to its caller.

/// Reason the offer was *not* shown to the user. Distinct from
/// [`PostInstallScanOutcome::Declined`] — the user never saw the prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// `cwd` is not inside a git repo (or `git` itself failed). The
    /// "import the last 5 PRs" promise only makes sense in a real repo.
    NotAGitRepo,
    /// `git remote get-url origin` did not point at a parseable
    /// owner/repo, so we can't even tell which GitHub repo to scan.
    NoGitHubRemote,
    /// `gh` is not on PATH. The import path shells out to `gh`, so an
    /// unauthenticated machine has nothing useful to offer here.
    GhNotInstalled,
    /// stdout/stdin aren't both ttys, or `non_interactive` was set, so
    /// there's no human to answer the prompt.
    NonInteractive,
    /// One of the common CI environment variables is set (`CI`,
    /// `GITHUB_ACTIONS`, `GITLAB_CI`). Same reasoning as
    /// `NonInteractive` but a separate variant so logs / tests can
    /// distinguish them.
    RunningInCi,
    /// `DIFFLORE_SKIP_POST_INSTALL_SCAN=1` (or `true`/`yes`) is set.
    ExplicitlySkipped,
}

/// Result of the post-install offer. `Skipped` and `Declined` are both
/// "do nothing more"; the distinction is kept for logging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostInstallScanOutcome {
    /// Offer was never shown. See [`SkipReason`] for the precondition
    /// that wasn't met.
    Skipped { reason: SkipReason },

    /// Prompt was shown and the user declined (any answer starting with `n`).
    Declined,

    /// Import completed (`difflore import-reviews` exited 0). The counters
    /// echo what the import printed and are advisory; the local DB is
    /// authoritative.
    ImportedReviews { pr_count: u32, rule_count: u32 },

    /// `difflore import-reviews` exited non-zero. Treated as recoverable —
    /// the user already saw the child process's stderr.
    ImportFailed { error: String },
}

impl PostInstallScanOutcome {
    /// True iff the user said yes and the import process ran to completion
    /// (success or failure). Lets callers gate follow-up prompts.
    #[must_use]
    pub const fn user_engaged(&self) -> bool {
        matches!(
            self,
            Self::ImportedReviews { .. } | Self::ImportFailed { .. }
        )
    }
}
