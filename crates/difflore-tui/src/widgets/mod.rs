//! Reusable TUI widgets.
//!
//! Only what is actually rendered lives here: `ascii_bar_counts` draws
//! the `FixRunsLow` capacity bar, `status_bar` paints the bottom plan
//! strip, and `truncate` is the one canonical string truncator. The
//! float `ascii_bar` and the `ascii_spark` sparkline had no callers and
//! were removed.

pub mod ascii_bar;
pub mod status_bar;

pub use ascii_bar::ascii_bar_counts;
pub use status_bar::{EventStripState, PlanStateView, PlanTier, SmartStatusBar};

/// Truncate a string so it never occupies more than `max` columns,
/// **including** the `…` marker — i.e. when characters are dropped the
/// result is exactly `max` chars (`max - 1` kept + the ellipsis), and an
/// untruncated string is returned verbatim. Counted by `chars()` so
/// multi-byte glyphs (CJK, emoji) don't get cut mid-codepoint.
///
/// This is the single canonical truncator. The activity stream, modal
/// renderers, and the rules tab used to ship hand-rolled copies; the
/// rules-tab copy (`truncate_display`) reserved *no* column for the
/// ellipsis (`take(max)` then append → `max + 1` chars) and overran
/// width-budgeted callers by one. Keeping one version means a future
/// tweak (e.g. word-boundary truncation) only lands here.
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}
