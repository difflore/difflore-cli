//! Reusable TUI widgets: `ascii_bar_counts` draws the `FixRunsLow` capacity
//! bar, `status_bar` paints the bottom plan strip, and `truncate` is the one
//! canonical string truncator.

pub mod ascii_bar;
pub mod status_bar;

pub use ascii_bar::ascii_bar_counts;
pub use status_bar::{EventStripState, PlanStateView, PlanTier, SmartStatusBar};

/// Truncate a string so it never occupies more than `max` columns,
/// **including** the `…` marker — when characters are dropped the result is
/// exactly `max` chars (`max - 1` kept + the ellipsis), and an untruncated
/// string is returned verbatim. Counted by `chars()` so multi-byte glyphs
/// (CJK, emoji) aren't cut mid-codepoint. The single canonical truncator.
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}
