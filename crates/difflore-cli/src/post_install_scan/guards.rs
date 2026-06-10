//! Pre-flight guards for the post-install import offer.
//!
//! The offer only makes sense in a narrow context: a real git repo that
//! we can resolve a `owner/repo` slug for, on an interactive terminal,
//! with `gh` already installed. Anything else and we silently skip —
//! the install flow's success line should still feel clean.
//!
//! Each guard returns a [`Result<(), SkipReason>`] so [`run_guards`] can
//! chain them in priority order without nested matches.

use std::path::Path;
use std::process::Command;

use super::opts::PostInstallScanOpts;
use super::outcome::SkipReason;

/// Override env var. Set to `1` / `true` / `yes` (case-insensitive) to
/// force the post-install scan to skip even on a healthy interactive
/// shell. Lets test scripts opt out without needing a `--no-*` flag.
pub const SKIP_ENV_VAR: &str = "DIFFLORE_SKIP_POST_INSTALL_SCAN";

/// CI env vars we treat as "not a real user terminal" regardless of tty state.
/// Buildkite/Drone/Travis follow the `CI=true` convention, so the first entry
/// catches them too.
const CI_ENV_VARS: &[&str] = &["CI", "GITHUB_ACTIONS", "GITLAB_CI"];

/// Inputs to [`run_guards_with`]. [`run_guards`] fills these from real env +
/// tty checks; tests provide deterministic fixtures.
#[derive(Debug, Clone, Copy)]
pub struct GuardSignals {
    pub stdin_is_tty: bool,
    pub stdout_is_tty: bool,
    pub gh_on_path: bool,
    pub is_git_repo: bool,
    pub has_github_remote: bool,
    pub in_ci: bool,
    pub explicit_skip: bool,
}

/// Pure decision: returns the first failing reason in priority order — explicit
/// user skip first, then CI, then objective preconditions.
pub const fn run_guards_with(
    signals: GuardSignals,
    non_interactive: bool,
) -> Result<(), SkipReason> {
    if signals.explicit_skip {
        return Err(SkipReason::ExplicitlySkipped);
    }
    if signals.in_ci {
        return Err(SkipReason::RunningInCi);
    }
    if non_interactive || !signals.stdin_is_tty || !signals.stdout_is_tty {
        return Err(SkipReason::NonInteractive);
    }
    if !signals.is_git_repo {
        return Err(SkipReason::NotAGitRepo);
    }
    if !signals.has_github_remote {
        return Err(SkipReason::NoGitHubRemote);
    }
    if !signals.gh_on_path {
        return Err(SkipReason::GhNotInstalled);
    }
    Ok(())
}

/// Production entry point. Probes the real env / fs / PATH using `opts`
/// and runs [`run_guards_with`] on the result.
pub fn run_guards(opts: &PostInstallScanOpts) -> Result<(), SkipReason> {
    use std::io::IsTerminal;

    let signals = GuardSignals {
        stdin_is_tty: std::io::stdin().is_terminal(),
        stdout_is_tty: std::io::stdout().is_terminal(),
        gh_on_path: which::which("gh").is_ok(),
        is_git_repo: is_git_repo(&opts.cwd),
        has_github_remote: has_github_remote(&opts.cwd),
        in_ci: detect_ci(),
        explicit_skip: detect_explicit_skip(),
    };
    run_guards_with(signals, opts.non_interactive)
}

/// Honour any of `DIFFLORE_SKIP_POST_INSTALL_SCAN=1|true|yes` (case
/// insensitive). Anything else, including unset, is treated as
/// "don't skip explicitly".
fn detect_explicit_skip() -> bool {
    match std::env::var(SKIP_ENV_VAR) {
        Ok(v) => is_truthy(&v),
        Err(_) => false,
    }
}

/// True iff any common CI env var is set to a truthy value. Presence alone
/// isn't enough: a vendor that sets `CI=` (empty) must not trigger.
fn detect_ci() -> bool {
    CI_ENV_VARS
        .iter()
        .any(|name| std::env::var(name).is_ok_and(|v| is_truthy(&v)))
}

fn is_truthy(v: &str) -> bool {
    matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes")
}

/// Probe for a git repo via `git rev-parse`. A `.git` dir check is unreliable
/// because git worktrees use a `.git` file.
fn is_git_repo(cwd: &Path) -> bool {
    let Ok(output) = Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(cwd)
        .output()
    else {
        return false;
    };
    output.status.success()
        && String::from_utf8_lossy(&output.stdout).trim() == "true"
}

/// True iff the origin remote parses as a GitHub `owner/repo` slug. Parsing is
/// delegated to core to stay in sync with `difflore import-reviews`.
fn has_github_remote(cwd: &Path) -> bool {
    let path = cwd.to_string_lossy().into_owned();
    difflore_core::github_import::detect_repo_from_remote(&path).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_signals() -> GuardSignals {
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
    fn happy_path_passes_all_guards() {
        assert_eq!(run_guards_with(base_signals(), false), Ok(()));
    }

    #[test]
    fn explicit_skip_short_circuits_everything() {
        // With every other failure mode set, explicit_skip still wins.
        let mut s = base_signals();
        s.explicit_skip = true;
        s.in_ci = true;
        s.stdin_is_tty = false;
        assert_eq!(
            run_guards_with(s, false),
            Err(SkipReason::ExplicitlySkipped)
        );
    }

    #[test]
    fn ci_env_var_blocks_offer_even_on_interactive_terminal() {
        let mut s = base_signals();
        s.in_ci = true;
        assert_eq!(run_guards_with(s, false), Err(SkipReason::RunningInCi));
    }

    #[test]
    fn non_interactive_flag_or_pipe_skips_with_non_interactive_reason() {
        // Caller asked for non-interactive explicitly.
        assert_eq!(
            run_guards_with(base_signals(), true),
            Err(SkipReason::NonInteractive)
        );

        // Or stdin is piped.
        let mut s = base_signals();
        s.stdin_is_tty = false;
        assert_eq!(
            run_guards_with(s, false),
            Err(SkipReason::NonInteractive)
        );

        // Or stdout is piped.
        let mut s = base_signals();
        s.stdout_is_tty = false;
        assert_eq!(
            run_guards_with(s, false),
            Err(SkipReason::NonInteractive)
        );
    }

    #[test]
    fn missing_git_repo_skips_before_gh_check() {
        let mut s = base_signals();
        s.is_git_repo = false;
        s.gh_on_path = false; // also missing — repo check should still win
        assert_eq!(run_guards_with(s, false), Err(SkipReason::NotAGitRepo));
    }

    #[test]
    fn missing_github_remote_skips_after_repo_check() {
        let mut s = base_signals();
        s.has_github_remote = false;
        assert_eq!(run_guards_with(s, false), Err(SkipReason::NoGitHubRemote));
    }

    #[test]
    fn missing_gh_cli_is_lowest_priority_skip_reason() {
        let mut s = base_signals();
        s.gh_on_path = false;
        assert_eq!(run_guards_with(s, false), Err(SkipReason::GhNotInstalled));
    }

    #[test]
    fn truthy_helper_matches_common_ci_values() {
        for v in ["1", "true", "TRUE", "yes", "Yes", " true "] {
            assert!(is_truthy(v), "expected truthy: {v:?}");
        }
        for v in ["", "0", "false", "no", "off", "FALSE "] {
            assert!(!is_truthy(v), "expected falsy: {v:?}");
        }
    }
}
