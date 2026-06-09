//! What [`super::maybe_offer_import_reviews`] returns to its caller.
//!
//! The outcome is intentionally a flat enum — the install path uses it
//! only to decide whether to print the "Imported … run `difflore status`
//! to inspect" success line. No further branching at the caller; if
//! more state is ever needed, add a variant rather than overloading an
//! existing one so the caller's match stays exhaustive.

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
    /// Lets test scripts and "I'll do it later" users opt out without
    /// inventing a new flag on every CLI surface.
    ExplicitlySkipped,
}

/// Result of the post-install offer. The caller treats `Skipped` and
/// `Declined` as the same "do nothing more" outcome but the distinction
/// is preserved for logging / future telemetry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostInstallScanOutcome {
    /// Offer was never shown. See [`SkipReason`] for the precondition
    /// that wasn't met.
    Skipped { reason: SkipReason },

    /// Prompt was shown, user answered No (or hit Enter on the default
    /// being `Y` was overridden — we treat any answer starting with
    /// `n` as decline).
    Declined,

    /// Import completed (`difflore import-reviews` exited 0). The two
    /// counters echo what the import printed — they're advisory; the
    /// authoritative source is the local DB.
    ImportedReviews {
        pr_count: u32,
        rule_count: u32,
    },

    /// `difflore import-reviews` exited non-zero. The install flow
    /// treats this as recoverable — the user already saw stderr from
    /// the child process, no need to crash the install on top of it.
    ImportFailed { error: String },
}

impl PostInstallScanOutcome {
    /// True iff the user actually said yes and the import process ran
    /// to completion (success or failure). Useful for callers that
    /// want to gate follow-up prompts on "did we just consume their
    /// attention".
    #[must_use]
    pub const fn user_engaged(&self) -> bool {
        matches!(
            self,
            Self::ImportedReviews { .. } | Self::ImportFailed { .. }
        )
    }
}
