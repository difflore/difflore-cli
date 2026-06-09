//! Install-time "Instant Value" touchpoint: after `difflore agents install`
//! succeeds in a git repo, offer the user to import the last N PRs from
//! the current repo so they see real DiffLore rules form from their own
//! review comments before they restart their agent. Pattern borrowed
//! from Activeloop hivemind's `runAuthGate` install-time session-scan
//! offer.
//!
//! Public surface is one function — [`maybe_offer_import_reviews`]. The
//! function name leads with `maybe` because the offer is allowed to
//! short-circuit silently: most fail-stops (no `gh`, no GitHub remote,
//! CI env) are not errors, just "this isn't the right context for the
//! pitch." See [`outcome::SkipReason`] for the full list.
//!
//! Scope invariant: the import only ever runs against the *current*
//! repo (the one `git remote get-url origin` resolves to). The offer
//! never opts the user into uploading anything to the cloud — that's
//! a later, user-driven decision (`difflore cloud sync`, `--upload`).

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

/// Entry point. Runs the guard pre-flight, prompts the user if
/// everything checks out, and shells out to `difflore import-reviews`
/// on Yes. Never errors back to the caller — every failure mode
/// collapses to a [`PostInstallScanOutcome`] variant so the install
/// flow can keep its success line clean.
///
/// The function is sync because the install path is sync. If a tokio
/// runtime is in scope at the call site the prompt's blocking
/// `read_line` is still safe — it's a one-off interactive prompt, not
/// a poll loop.
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

/// Render the pre-prompt blurb with a low-pressure offer.
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

/// Brief one-liner the user sees after answering No. We *don't* re-
/// print the full pitch — they've already declined once.
fn print_decline_hint() {
    println!(
        "  {} No worries — run {} anytime to seed memory from this repo.",
        style::pewter(sym::BULLET),
        style::cmd("difflore import-reviews"),
    );
}

/// Print the success / failure summary line after the import process
/// returns. The success line points at `difflore status` because
/// that's the authoritative count source, not whatever `pr_count` we
/// stashed in the outcome.
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

/// One-line hint matching the skip reason. Quiet by design — these are
/// not errors and we don't want to scream at users whose machines are
/// simply not in the "offer is meaningful" context.
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
