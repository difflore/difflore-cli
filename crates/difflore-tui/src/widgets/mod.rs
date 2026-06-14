//! Reusable TUI widgets: `ascii_bar_counts` draws the capacity bar,
//! `status_bar` paints the bottom plan strip, `center` holds the
//! centred-rect helpers, and `text::truncate` is the one canonical
//! string truncator. This module only declares and re-exports.

pub mod ascii_bar;
pub mod center;
pub mod status_bar;
pub mod text;

pub use ascii_bar::ascii_bar_counts;
pub use status_bar::{EventStripState, PlanStateView, SmartStatusBar};
pub use text::truncate;
