//! `difflore skills sweep`.
//!
//! Thin wrapper around [`difflore_core::skills::sweep`] that prints the
//! `SweepReport` (and optional `QuarantineReport`) as JSON for piping into
//! tools like jq. Defaults to dry-run.

use std::fmt::Display;

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
    external_links: ExternalLinkCleanupReport,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ExternalLinkCleanupReport {
    review_standard_links: u64,
    ok: bool,
    failed_engines: u64,
    by_engine: Vec<ExternalLinkEngineReport>,
    dry_run: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ExternalLinkEngineReport {
    engine: &'static str,
    review_standard_links: u64,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

pub(crate) async fn handle_sweep(ctx: &CommandContext, args: SweepArgs) {
    if let Err(e) = run(&ctx.db, args).await {
        crate::support::util::exit_err(&format!("skills sweep failed: {e}"));
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

    let external_links = cleanup_external_review_standard_links(args.dry_run);

    let envelope = SweepCliReport {
        sweep,
        quarantine,
        external_links,
    };
    // Serialization cannot fail on this concrete shape, but report it rather
    // than unwrap if it somehow does.
    match serde_json::to_string_pretty(&envelope) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("{} report serialise failed: {e}", style::warn(sym::WARN)),
    }
    Ok(())
}

fn cleanup_external_review_standard_links(dry_run: bool) -> ExternalLinkCleanupReport {
    cleanup_external_review_standard_links_with(dry_run, |engine, dry_run| {
        difflore_core::skills::fs::purge_review_standard_engine_links(engine, dry_run)
    })
}

fn cleanup_external_review_standard_links_with<E, F>(
    dry_run: bool,
    mut purge: F,
) -> ExternalLinkCleanupReport
where
    E: Display,
    F: FnMut(&str, bool) -> Result<usize, E>,
{
    let mut by_engine = Vec::new();
    let mut total = 0_u64;
    let mut failed_engines = 0_u64;
    for engine in ["codex", "claude", "gemini", "cursor"] {
        let (count, ok, error) = match purge(engine, dry_run) {
            Ok(count) => (u64::try_from(count).unwrap_or(u64::MAX), true, None),
            Err(e) => {
                failed_engines = failed_engines.saturating_add(1);
                let error = e.to_string();
                eprintln!(
                    "{} external review-standard link cleanup failed for {engine}: {error}",
                    style::warn(sym::WARN)
                );
                (0, false, Some(error))
            }
        };
        total = total.saturating_add(count);
        by_engine.push(ExternalLinkEngineReport {
            engine,
            review_standard_links: count,
            ok,
            error,
        });
    }
    ExternalLinkCleanupReport {
        review_standard_links: total,
        ok: failed_engines == 0,
        failed_engines,
        by_engine,
        dry_run,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_link_cleanup_reports_engine_failures_separately_from_zero() {
        let report = cleanup_external_review_standard_links_with(false, |engine, dry_run| {
            assert!(!dry_run);
            match engine {
                "codex" => Ok(2),
                "claude" => Err("permission denied"),
                "gemini" => Ok(0),
                "cursor" => Ok(1),
                _ => unreachable!("unexpected engine {engine}"),
            }
        });

        assert!(!report.ok);
        assert_eq!(report.failed_engines, 1);
        assert_eq!(report.review_standard_links, 3);

        let claude = report
            .by_engine
            .iter()
            .find(|entry| entry.engine == "claude")
            .expect("claude report");
        assert!(!claude.ok);
        assert_eq!(claude.review_standard_links, 0);
        assert_eq!(claude.error.as_deref(), Some("permission denied"));

        let gemini = report
            .by_engine
            .iter()
            .find(|entry| entry.engine == "gemini")
            .expect("gemini report");
        assert!(gemini.ok);
        assert_eq!(gemini.review_standard_links, 0);
        assert!(gemini.error.is_none());
    }
}
