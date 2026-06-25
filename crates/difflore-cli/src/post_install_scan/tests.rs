//! Integration-flavoured tests across the module boundary. Cover guard logic
//! without spawning `difflore import-reviews`.

use std::path::PathBuf;

use super::guards::{GuardSignals, run_guards_with};
use super::opts::{
    DEFAULT_MAX_PRS, DEFAULT_SINCE_DAYS, DEFAULT_WALL_TIMEOUT_SECS, PostInstallScanOpts,
};
use super::outcome::{PostInstallScanOutcome, SkipReason};

fn pristine_signals() -> GuardSignals {
    GuardSignals {
        stdin_is_tty: true,
        stdout_is_tty: true,
        gh_on_path: true,
        is_git_repo: true,
        has_github_remote: true,
        in_ci: false,
        explicit_skip: false,
    }
}

#[test]
fn opts_for_cwd_defaults_to_interactive_with_default_max_prs() {
    let opts = PostInstallScanOpts::for_cwd(PathBuf::from("/tmp/repo"));
    assert_eq!(opts.cwd, PathBuf::from("/tmp/repo"));
    assert!(!opts.non_interactive);
    assert_eq!(opts.max_prs, DEFAULT_MAX_PRS);
    assert_eq!(opts.max_prs, 50, "default max_prs documented as 50");
    assert_eq!(opts.since_days, DEFAULT_SINCE_DAYS);
    assert_eq!(opts.wall_timeout_secs, DEFAULT_WALL_TIMEOUT_SECS);
    assert_eq!(opts.since_days, 120);
    assert_eq!(opts.wall_timeout_secs, 20);
}

#[test]
fn guard_layer_allows_piped_stdout_for_background_scan() {
    let mut s = pristine_signals();
    s.stdout_is_tty = false;
    assert_eq!(run_guards_with(s, false), Ok(()));
}

#[test]
fn guard_layer_skips_when_non_interactive_is_explicit() {
    let outcome = PostInstallScanOutcome::Skipped {
        reason: run_guards_with(pristine_signals(), true).expect_err("flag must skip"),
    };
    assert_eq!(
        outcome,
        PostInstallScanOutcome::Skipped {
            reason: SkipReason::NonInteractive,
        }
    );
}

#[test]
fn guard_layer_skips_when_gh_cli_is_missing() {
    let mut s = pristine_signals();
    s.gh_on_path = false;
    let outcome = PostInstallScanOutcome::Skipped {
        reason: run_guards_with(s, false).expect_err("missing gh must skip"),
    };
    assert_eq!(
        outcome,
        PostInstallScanOutcome::Skipped {
            reason: SkipReason::GhNotInstalled,
        }
    );
}

#[test]
fn guard_layer_skips_when_running_in_ci() {
    let mut s = pristine_signals();
    s.in_ci = true;
    let outcome = PostInstallScanOutcome::Skipped {
        reason: run_guards_with(s, false).expect_err("CI must skip"),
    };
    assert_eq!(
        outcome,
        PostInstallScanOutcome::Skipped {
            reason: SkipReason::RunningInCi,
        }
    );
}

#[test]
fn guard_skip_priority_matches_blame_order() {
    // Pins the deliberate priority: explicit user opt-out wins over CI, which
    // wins over non-tty, etc. Reordering the guards breaks this.
    let mut s = pristine_signals();
    s.explicit_skip = true;
    s.in_ci = true;
    s.stdin_is_tty = false;
    s.is_git_repo = false;
    s.has_github_remote = false;
    s.gh_on_path = false;
    assert_eq!(run_guards_with(s, true), Err(SkipReason::ExplicitlySkipped));
}

#[test]
fn outcome_user_engaged_only_true_when_import_actually_ran() {
    assert!(!PostInstallScanOutcome::Declined.user_engaged());
    assert!(
        !PostInstallScanOutcome::Skipped {
            reason: SkipReason::NonInteractive,
        }
        .user_engaged()
    );
    assert!(
        PostInstallScanOutcome::ImportedReviews {
            pr_count: 5,
            rule_count: 3,
        }
        .user_engaged()
    );
    assert!(
        PostInstallScanOutcome::ImportFailed {
            error: "boom".into(),
        }
        .user_engaged()
    );
}
