//! `difflore doctor --drain-abandoned`.
//!
//! Resurrects stale `abandoned` rows from both outbox queues
//! (`cloud_outbox` in `data.db`, `observation_events` in
//! `observations_outbox.db`) so the next drain pass can retry them.
//!
//! Safety profile:
//! - **dry-run by default** — the user must pass `--no-dry-run` to
//!   actually mutate.
//! - **--older-than cutoff** — only rows whose last attempt (or
//!   creation, if never attempted) is older than the cutoff are
//!   touched.
//! - **auth-gated** — refuses to run the real path with no live
//!   session: re-pending rows that will just 401 again helps nobody.
//! - **no schema change, no row deletion, no cloud-DB access.**

use std::time::Duration;

use difflore_core::cloud::observations::ObservationEmitter;
use difflore_core::cloud::outbox::{DrainSummary, OutboxQueue};

use crate::runtime::CommandContext;
use crate::style;

/// Outcome bundle for `run_drain` so the caller can render either the
/// human or JSON surface from a single struct.
#[derive(Debug)]
pub(crate) struct DrainOutcome {
    pub(crate) cloud_outbox: DrainSummary,
    pub(crate) observation_events: DrainSummary,
    pub(crate) dry_run: bool,
    pub(crate) cutoff_unix_ms: i64,
    pub(crate) older_than: Duration,
}

impl DrainOutcome {
    pub(crate) const fn total(&self) -> i64 {
        self.cloud_outbox.total + self.observation_events.total
    }

    fn queues_touched(&self) -> usize {
        usize::from(self.cloud_outbox.total > 0) + usize::from(self.observation_events.total > 0)
    }
}

/// Parse an `--older-than` duration string. Accepts `30d`, `7d`,
/// `24h`, `1h`, `30m`. Unknown forms fail loudly instead of choosing a
/// default.
pub(crate) fn parse_older_than(raw: &str) -> Result<Duration, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("--older-than requires a value (e.g. 30d, 7d, 24h, 1h, 30m)".to_owned());
    }
    let (num_str, unit) = raw.split_at(
        raw.find(|c: char| !c.is_ascii_digit())
            .ok_or_else(|| format!("--older-than '{raw}' is missing a unit (d, h, m)"))?,
    );
    if num_str.is_empty() {
        return Err(format!("--older-than '{raw}' is missing a number"));
    }
    let n: u64 = num_str
        .parse()
        .map_err(|_| format!("--older-than '{raw}' is not a valid number"))?;
    if n == 0 {
        return Err("--older-than must be > 0".to_owned());
    }
    let secs = match unit {
        "d" => n.checked_mul(86_400),
        "h" => n.checked_mul(3_600),
        "m" => n.checked_mul(60),
        other => {
            return Err(format!(
                "--older-than unit '{other}' not recognised — use d, h, or m"
            ));
        }
    }
    .ok_or_else(|| format!("--older-than '{raw}' overflows"))?;
    Ok(Duration::from_secs(secs))
}

/// Drive a drain (dry-run by default) and return the outcome. Caller
/// renders. The function itself is silent so doctor's two output
/// surfaces (text + JSON) can share it without an I/O side-effect race.
pub(crate) async fn run_drain(
    ctx: &CommandContext,
    older_than: Duration,
    dry_run: bool,
) -> Result<DrainOutcome, String> {
    // Refuse the write path without a saved cloud session. Dry-run still
    // works while logged out so users can inspect counts.
    if !dry_run && !ctx.cloud().await.is_logged_in() {
        return Err("refusing to drain without a saved cloud session. \
             Run `difflore cloud login` first — re-queueing rows that 401 \
             helps nobody."
            .to_owned());
    }

    let now_ms = chrono::Utc::now().timestamp_millis();
    let cutoff_ms =
        now_ms.saturating_sub(i64::try_from(older_than.as_millis()).unwrap_or(i64::MAX));

    let outbox = OutboxQueue::new(ctx.db.clone());
    let cloud_outbox = outbox
        .drain_abandoned_older_than(cutoff_ms, dry_run)
        .await
        .map_err(|e| format!("drain cloud_outbox: {e}"))?;

    let observation_events = match ObservationEmitter::open_default().await {
        Ok(emitter) => {
            emitter
                .drain_abandoned_older_than(cutoff_ms, dry_run)
                .await?
        }
        // No observations DB on disk means this queue has nothing to drain.
        Err(_) => DrainSummary::default(),
    };

    Ok(DrainOutcome {
        cloud_outbox,
        observation_events,
        dry_run,
        cutoff_unix_ms: cutoff_ms,
        older_than,
    })
}

/// Render the outcome to stdout. Human or JSON depending on `json`.
pub(crate) fn render_outcome(outcome: &DrainOutcome, json: bool) {
    if json {
        let payload = serde_json::json!({
            "dryRun": outcome.dry_run,
            "olderThanSecs": outcome.older_than.as_secs(),
            "cutoffUnixMs": outcome.cutoff_unix_ms,
            "totalRows": outcome.total(),
            "cloudOutbox": {
                "total": outcome.cloud_outbox.total,
                "perKind": outcome.cloud_outbox.per_kind.iter()
                    .map(|(k, n)| serde_json::json!({"kind": k, "count": n}))
                    .collect::<Vec<_>>(),
            },
            "observationEvents": {
                "total": outcome.observation_events.total,
                "perKind": outcome.observation_events.per_kind.iter()
                    .map(|(k, n)| serde_json::json!({"kind": k, "count": n}))
                    .collect::<Vec<_>>(),
            },
        });
        println!("{payload}");
        return;
    }

    let action_word = if outcome.dry_run {
        "would recover"
    } else {
        "recovered"
    };
    let headline = if outcome.dry_run {
        "Previewing stale upload recovery"
    } else {
        "Recovered stale uploads"
    };
    println!(
        "{} {} older than {}",
        style::emerald(style::sym::TIP),
        style::ok(headline),
        humanize_duration(outcome.older_than),
    );

    print_queue_breakdown("team sync queue", &outcome.cloud_outbox);
    print_queue_breakdown("agent evidence queue", &outcome.observation_events);

    let queues = outcome.queues_touched();
    let total = outcome.total();
    let queue_word = if queues == 1 { "queue" } else { "queues" };
    println!();
    if total == 0 {
        println!("  {}", style::pewter("no eligible rows - nothing to do"),);
    } else if outcome.dry_run {
        println!(
            "  {} {action_word} {} rows across {queues} {queue_word}; pass --no-dry-run to apply",
            style::pewter("summary:"),
            total,
        );
    } else {
        println!(
            "  {} {action_word} {} rows across {queues} {queue_word}",
            style::pewter("summary:"),
            total,
        );
    }
}

fn print_queue_breakdown(label: &str, summary: &DrainSummary) {
    println!("  {}", style::pewter(label));
    if summary.per_kind.is_empty() {
        println!("    {}", style::pewter("(no abandoned rows in this queue)"),);
        return;
    }
    for (kind, count) in &summary.per_kind {
        println!("    {kind:<24} {count}");
    }
}

fn humanize_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 86_400 && secs.is_multiple_of(86_400) {
        format!("{}d", secs / 86_400)
    } else if secs >= 3_600 && secs.is_multiple_of(3_600) {
        format!("{}h", secs / 3_600)
    } else if secs.is_multiple_of(60) {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_older_than_accepts_documented_units() {
        assert_eq!(parse_older_than("30d").unwrap().as_secs(), 30 * 86_400);
        assert_eq!(parse_older_than("7d").unwrap().as_secs(), 7 * 86_400);
        assert_eq!(parse_older_than("24h").unwrap().as_secs(), 24 * 3_600);
        assert_eq!(parse_older_than("1h").unwrap().as_secs(), 3_600);
        assert_eq!(parse_older_than("30m").unwrap().as_secs(), 30 * 60);
    }

    #[test]
    fn parse_older_than_rejects_garbage() {
        assert!(parse_older_than("").is_err());
        assert!(parse_older_than("d").is_err());
        assert!(parse_older_than("30").is_err());
        assert!(parse_older_than("30days").is_err());
        assert!(parse_older_than("0d").is_err());
        assert!(parse_older_than("-30d").is_err());
    }

    #[test]
    fn humanize_duration_picks_largest_clean_unit() {
        assert_eq!(humanize_duration(Duration::from_secs(86_400 * 30)), "30d");
        assert_eq!(humanize_duration(Duration::from_secs(3_600)), "1h");
        assert_eq!(humanize_duration(Duration::from_secs(1_800)), "30m");
        assert_eq!(humanize_duration(Duration::from_secs(90)), "90s");
    }
}
