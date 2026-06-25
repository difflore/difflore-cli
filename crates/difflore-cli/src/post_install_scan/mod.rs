//! Install-time touchpoint: after `difflore agents install` succeeds in a git
//! repo, queue a bounded background import of recent PRs from the current repo
//! so the user can see DiffLore rules form from their own review comments
//! without blocking install.
//!
//! The single public function [`maybe_offer_import_reviews`] may short-circuit
//! silently: most fail-stops (no `gh`, no GitHub remote, CI env) are not errors,
//! just "this isn't the right context for the offer." See
//! [`outcome::SkipReason`] for the full list.
//!
//! Scope invariant: the import only ever runs against the current repo (the one
//! `git remote get-url origin` resolves to), and never uploads anything to the
//! cloud — that's a later, user-driven decision.

pub mod guards;
pub mod opts;
pub mod outcome;
pub mod prompt;
pub mod runner;

#[cfg(test)]
mod tests;

pub use opts::{
    DEFAULT_MAX_PRS, DEFAULT_SINCE_DAYS, DEFAULT_WALL_TIMEOUT_SECS, PostInstallScanOpts,
};
pub use outcome::{PostInstallScanOutcome, SkipReason};

use crate::style::{self, sym};

/// Entry point. Runs the guard pre-flight and queues `difflore import-reviews`
/// in a bounded background worker. Never errors back to the
/// caller — every failure mode collapses to a [`PostInstallScanOutcome`]
/// variant so the install flow keeps its success line clean.
///
/// Sync because the install path is sync.
pub fn maybe_offer_import_reviews(opts: &PostInstallScanOpts) -> PostInstallScanOutcome {
    let review_guard = guards::run_guards(opts);
    if let Err(reason) = &review_guard
        && matches!(
            reason,
            SkipReason::ExplicitlySkipped | SkipReason::RunningInCi | SkipReason::NonInteractive
        )
    {
        return PostInstallScanOutcome::Skipped {
            reason: reason.clone(),
        };
    }

    let exe = match runner::resolve_self_binary() {
        Ok(p) => p,
        Err(e) => {
            eprintln!(
                "{} {e}",
                style::warn("post-install memory onboarding skipped:")
            );
            return PostInstallScanOutcome::ImportFailed { error: e };
        }
    };

    let agent_files_queued = match runner::run_agent_file_import(&exe, &opts.cwd) {
        Ok(()) => true,
        Err(e) => {
            eprintln!(
                "{} {e}",
                style::warn("post-install agent-file import skipped:")
            );
            false
        }
    };

    let outcome = match review_guard {
        Ok(()) => {
            let since = runner::since_date_utc(opts.since_days);
            runner::run_import(
                &exe,
                &opts.cwd,
                opts.max_prs,
                &since,
                opts.wall_timeout_secs,
            )
        }
        Err(reason) => PostInstallScanOutcome::Skipped { reason },
    };
    print_outcome_footer(&outcome, agent_files_queued);
    outcome
}

/// Print the success / failure summary line after the import returns. The
/// success line points at `difflore status`, the authoritative count source,
/// not the `pr_count` stashed in the outcome.
fn print_outcome_footer(outcome: &PostInstallScanOutcome, agent_files_queued: bool) {
    match outcome {
        PostInstallScanOutcome::ImportedReviews { pr_count, .. } => {
            println!();
            let queued = if agent_files_queued {
                format!("Queued memory onboarding for agent files and up to {pr_count} PRs.")
            } else {
                format!("Queued a bounded import for up to {pr_count} PRs.")
            };
            println!("📥 {} {}", style::emerald(&queued), style::pewter("Run"));
            println!(
                "   {} {}",
                style::cmd("difflore status"),
                style::pewter("or"),
            );
            println!(
                "   {} {}",
                style::cmd("difflore memory import-agent-files"),
                style::pewter("to seed local agent-file memory anytime."),
            );
        }
        PostInstallScanOutcome::ImportFailed { error } => {
            eprintln!();
            eprintln!(
                "{} {error}",
                style::warn("post-install memory import failed:")
            );
            eprintln!(
                "  {} retry later with {}.",
                style::pewter(sym::BULLET),
                style::cmd("difflore import-reviews --max-prs 50"),
            );
        }
        PostInstallScanOutcome::Skipped { .. } | PostInstallScanOutcome::Declined => {
            if agent_files_queued {
                println!();
                println!(
                    "📥 {} {}",
                    style::emerald("Queued agent-file memory onboarding."),
                    style::pewter("Run"),
                );
                println!(
                    "   {} {}",
                    style::cmd("difflore status"),
                    style::pewter("to inspect what landed."),
                );
            }
        }
    }
}
