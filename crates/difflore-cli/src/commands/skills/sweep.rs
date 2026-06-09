//! `difflore skills sweep`.
//!
//! Thin wrapper around [`difflore_core::skills::sweep`] that prints the
//! `SweepReport` (and optional `QuarantineReport`) as JSON so external
//! tooling can pipe it into rtk or jq. Defaults to dry-run; the core
//! sweep is idempotent within a single pass but writing 1800+ skills
//! without preview would be ugly.

use difflore_core::SqlitePool;
use difflore_core::skills::{
    QuarantineReport, SweepOpts, SweepReport, quarantine_unguided_conv_reviews, sweep_stale_skills,
};
use serde::Serialize;

use crate::runtime::CommandContext;
use crate::style::{self, sym};

#[derive(Debug, Clone, Copy)]
pub(crate) struct SweepArgs {
    pub dry_run: bool,
    pub decay_factor: f32,
    pub days: u32,
    pub quarantine_unguided: bool,
}

/// Aggregated JSON envelope so callers always get a single object.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SweepCliReport {
    sweep: SweepReport,
    quarantine: Option<QuarantineReport>,
}

pub(crate) async fn handle_sweep(ctx: &CommandContext, args: SweepArgs) {
    if let Err(e) = run(&ctx.db, args).await {
        eprintln!("{} skills sweep failed: {e}", style::warn(sym::WARN));
    }
}

async fn run(db: &SqlitePool, args: SweepArgs) -> difflore_core::Result<()> {
    let opts = SweepOpts {
        stale_install_days: args.days,
        stale_serve_days: args.days,
        decay_factor: args.decay_factor,
        dry_run: args.dry_run,
        min_floor: 0.05,
    };

    let sweep = sweep_stale_skills(db, opts).await?;

    let quarantine = if args.quarantine_unguided {
        Some(quarantine_unguided_conv_reviews(db, args.dry_run).await?)
    } else {
        None
    };

    let envelope = SweepCliReport { sweep, quarantine };
    // serde_json::to_string_pretty cannot fail on this concrete shape;
    // fall back to a debug-print only if it somehow does.
    match serde_json::to_string_pretty(&envelope) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("{} report serialise failed: {e}", style::warn(sym::WARN)),
    }
    Ok(())
}
