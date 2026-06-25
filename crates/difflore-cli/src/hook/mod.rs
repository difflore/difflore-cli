//! Hook domain: everything between an AI client's lifecycle hook firing and
//! DiffLore's response landing back on its stdout.
//!
//! * [`adapters`] — per-client JSON dialects normalised into one `HookEvent`.
//! * [`runtime`] — event dispatch: rule injection, observation capture,
//!   fire logging, session-mine triggering.
//! * [`banner`] — the since-last-session recap surfaced on `SessionStart`.
//! * [`cache`] — short-window dedup so repeated edits stay off the hot path.
//! * [`forward`] — local-socket forwarder keeping a warm process around, plus
//!   the wire [`forward::protocol`] the `difflore-hook` shim binary reuses.

pub mod adapters;
pub mod banner;
pub mod cache;
pub mod forward;
pub mod runtime;
