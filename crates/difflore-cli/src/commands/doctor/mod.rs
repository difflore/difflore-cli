use crate::commands::util::exit_code;
use crate::style;

pub(crate) mod drain;
pub(crate) mod fix;
pub(crate) mod labels;
pub(crate) mod memory_snapshot;
pub(crate) mod probes;
pub(crate) mod report;
pub(crate) mod table;

/// Bundle of every flag the doctor command currently exposes. Keeping
/// them in one struct lets `handle_doctor` keep a single signature as
/// the surface grows.
#[derive(Debug)]
pub(crate) struct DoctorArgs {
    pub report: bool,
    pub fix: bool,
    pub drain_abandoned: bool,
    pub older_than: String,
    pub no_dry_run: bool,
    pub json: bool,
}

pub(crate) async fn handle_doctor(ctx: &crate::runtime::CommandContext, args: DoctorArgs) {
    let DoctorArgs {
        report,
        fix: fix_mode,
        drain_abandoned,
        older_than,
        no_dry_run,
        json,
    } = args;

    if drain_abandoned {
        // Resurrection path — strictly opt-in. Dry-run by default; the
        // user must pass `--no-dry-run` to actually write.
        let cutoff = match drain::parse_older_than(&older_than) {
            Ok(d) => d,
            Err(msg) => {
                style::report_error(&msg, "", &[]);
                exit_code(2);
            }
        };
        let dry_run = !no_dry_run;
        match drain::run_drain(ctx, cutoff, dry_run).await {
            Ok(outcome) => drain::render_outcome(&outcome, json),
            Err(msg) => {
                style::report_error(&msg, "", &[]);
                exit_code(1);
            }
        }
        return;
    }

    if report {
        let md = report::build_doctor_report(ctx).await;
        let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S");
        // Park reports under `~/.difflore/reports/` so they don't
        // accumulate in the user's project root. Fall back to cwd on
        // the (rare) failure path so the user still gets their report.
        let dir = match difflore_core::paths::data_home() {
            Ok(d) => d.join("reports"),
            Err(_) => std::path::PathBuf::from("."),
        };
        if let Err(e) = std::fs::create_dir_all(&dir) {
            style::report_error(
                &format!("Failed to create reports dir {}: {e}", dir.display()),
                "",
                &[],
            );
            exit_code(1);
        }
        let path = dir.join(format!("difflore-bug-report-{ts}.md"));
        match std::fs::write(&path, &md) {
            Ok(()) => println!(
                "{} Bug report written to {}",
                style::emerald(style::sym::OK),
                style::ident(&path.display().to_string())
            ),
            Err(e) => {
                style::report_error(&format!("Failed to write report: {e}"), "", &[]);
                exit_code(1);
            }
        }
    } else {
        // Default doctor view: aligned-column table. The markdown report
        // stays behind `--report` for paste-into-issue workflows.
        let rendered = table::render_table(ctx).await;
        print!("{rendered}");
        // Only print the slow-drain warning when the same queues used by
        // `--drain-abandoned` cross their thresholds.
        if let Some(warning) = slow_drain_warning(ctx).await {
            println!();
            println!("  {warning}");
        }
        if fix_mode {
            // Run a fix pass after the diagnostic table so the user
            // sees the same surface they always do, then watches the
            // narrow auto-repairs apply against it.
            fix::run_fix_pass();
        } else if fix::has_fixable() {
            // Single-line nudge — only printed when there's something
            // to fix. Sized to never nag a healthy install.
            println!();
            println!(
                "  {} {} {} {}",
                style::emerald(style::sym::TIP),
                style::pewter("Run"),
                style::cmd("difflore doctor --fix"),
                style::pewter("to auto-repair these."),
            );
        }
    }
}

/// Thresholds for the passive slow-drain warning. Healthy installs should
/// clear `pending` rows well below these counts.
const SLOW_DRAIN_CLOUD_THRESHOLD: i64 = 500;
const SLOW_DRAIN_OBSERVATION_THRESHOLD: i64 = 200;

/// Inspect the two outbox queues and return a single-line warning when
/// either is over its slow-drain threshold. Returns `None` otherwise
/// so doctor's clean-install surface is unchanged.
async fn slow_drain_warning(ctx: &crate::runtime::CommandContext) -> Option<String> {
    use difflore_core::cloud::observations::ObservationEmitter;
    use difflore_core::cloud::outbox::OutboxQueue;

    let outbox = OutboxQueue::new(ctx.db.clone());
    let cloud_counts = outbox.pending_counts_by_kind().await.ok()?;
    let cloud_total: i64 = cloud_counts.iter().map(|(_, n)| *n).sum();

    let obs_pending = match ObservationEmitter::open_default().await {
        Ok(e) => e.pending_upload_count().await.unwrap_or(0),
        Err(_) => 0,
    };

    let cloud_hot = cloud_total > SLOW_DRAIN_CLOUD_THRESHOLD;
    let obs_hot = obs_pending > SLOW_DRAIN_OBSERVATION_THRESHOLD;
    if !cloud_hot && !obs_hot {
        return None;
    }

    let mut parts = Vec::new();
    if cloud_hot {
        parts.push(format!("{cloud_total} cloud upload{}", plural(cloud_total)));
    }
    if obs_hot && obs_pending > 0 {
        parts.push(format!("{obs_pending} agent event{}", plural(obs_pending)));
    }

    Some(format!(
        "{} {} {} — run `difflore cloud sync`; if it stays queued, attach `difflore doctor --report`.",
        style::amber(style::sym::WARN),
        style::pewter("upload queue:"),
        parts.join(" + "),
    ))
}

const fn plural(n: i64) -> &'static str {
    if n == 1 { "" } else { "s" }
}
