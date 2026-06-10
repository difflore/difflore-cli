//! Install-time touchpoint: after `difflore agents install` succeeds in a git
//! repo, offer to import the last N PRs from the current repo so the user sees
//! real DiffLore rules form from their own review comments before restarting
//! their agent.
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

pub use opts::{DEFAULT_MAX_PRS, PostInstallScanOpts};
pub use outcome::{PostInstallScanOutcome, SkipReason};

use crate::style::{self, sym};

/// Entry point. Runs the guard pre-flight, prompts if everything checks out,
/// and shells out to `difflore import-reviews` on Yes. Never errors back to the
/// caller — every failure mode collapses to a [`PostInstallScanOutcome`]
/// variant so the install flow keeps its success line clean.
///
/// Sync because the install path is sync; the prompt's blocking `read_line` is
/// safe even with a tokio runtime in scope, being a one-off prompt not a poll
/// loop.
pub fn maybe_offer_import_reviews(opts: &PostInstallScanOpts) -> PostInstallScanOutcome {
    if let Err(reason) = guards::run_guards(opts) {
        print_skip_hint(&reason);
        return PostInstallScanOutcome::Skipped { reason };
    }

    print_offer_header(opts.max_prs);
    let question = format!(
        "Want me to import the last {} PRs from this repo right now? (~2 min)",
        opts.max_prs
    );
    if !prompt::ask_yes_default_yes(&question) {
        print_decline_hint();
        return PostInstallScanOutcome::Declined;
    }

    let exe = match runner::resolve_self_binary() {
        Ok(p) => p,
        Err(e) => {
            eprintln!(
                "{} {e}",
                style::warn("post-install import skipped:"),
            );
            return PostInstallScanOutcome::ImportFailed { error: e };
        }
    };

    let outcome = runner::run_import(&exe, &opts.cwd, opts.max_prs);
    print_outcome_footer(&outcome);
    outcome
}

fn print_offer_header(max_prs: u32) {
    println!();
    println!(
        "🐝 {} {}",
        style::emerald("Want me to import the last"),
        style::emerald(&format!("{max_prs} PRs from this repo right now?")),
    );
    println!(
        "   {}",
        style::pewter("(~2 min, uses your gh CLI)"),
    );
    println!();
    println!(
        "   {}",
        style::pewter(
            "You'll see real DiffLore rules form from your team's actual review comments,"
        ),
    );
    println!(
        "   {}",
        style::pewter(
            "instead of an empty install. Decline is fine — run `difflore import-reviews`"
        ),
    );
    println!("   {}", style::pewter("anytime later."));
    println!();
}

fn print_decline_hint() {
    println!(
        "  {} No worries — run {} anytime to seed memory from this repo.",
        style::pewter(sym::BULLET),
        style::cmd("difflore import-reviews"),
    );
}

/// Print the success / failure summary line after the import returns. The
/// success line points at `difflore status`, the authoritative count source,
/// not the `pr_count` stashed in the outcome.
fn print_outcome_footer(outcome: &PostInstallScanOutcome) {
    match outcome {
        PostInstallScanOutcome::ImportedReviews { pr_count, .. } => {
            println!();
            println!(
                "📥 {} {} {}",
                style::emerald(&format!("Imported up to {pr_count} PRs;")),
                style::pewter("review candidates drafted locally."),
                style::pewter("Run"),
            );
            println!(
                "   {} {}",
                style::cmd("difflore status"),
                style::pewter("to inspect what landed."),
            );
        }
        PostInstallScanOutcome::ImportFailed { error } => {
            eprintln!();
            eprintln!(
                "{} {error}",
                style::warn("post-install import failed:"),
            );
            eprintln!(
                "  {} retry later with {}.",
                style::pewter(sym::BULLET),
                style::cmd("difflore import-reviews --max-prs 5"),
            );
        }
        // Skipped / Declined are handled before we get here.
        PostInstallScanOutcome::Skipped { .. } | PostInstallScanOutcome::Declined => {}
    }
}

/// One-line hint matching the skip reason. Quiet by design — these are not
/// errors, just contexts where the offer isn't meaningful.
fn print_skip_hint(reason: &SkipReason) {
    match reason {
        SkipReason::ExplicitlySkipped | SkipReason::RunningInCi | SkipReason::NonInteractive => {
            // Silent: scripts / CI shouldn't see chatty hints.
        }
        SkipReason::NotAGitRepo | SkipReason::NoGitHubRemote => {
            println!(
                "  {} not a GitHub-tracked repo — skipping the optional import offer. \
                 Run {} later from a checked-out repo.",
                style::pewter(sym::BULLET),
                style::cmd("difflore import-reviews"),
            );
        }
        SkipReason::GhNotInstalled => {
            println!(
                "  {} GitHub CLI (`gh`) not on PATH — skipping the optional import offer. \
                 Install gh from cli.github.com, then run {}.",
                style::pewter(sym::BULLET),
                style::cmd("difflore import-reviews"),
            );
        }
    }
}
