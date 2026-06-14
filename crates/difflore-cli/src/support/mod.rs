//! Crate-level shared helpers used across commands, installer, and hook
//! paths. Nothing here is a user command — that's exactly why it lives
//! outside `commands/` (naming rule: command path = module path).

pub(crate) mod impact_payload;
pub(crate) mod review_text;
pub(crate) mod util;
