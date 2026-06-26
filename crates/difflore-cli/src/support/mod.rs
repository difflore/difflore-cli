//! Crate-level shared helpers used across commands, installer, and hook
//! paths. Nothing here is a user command — that's exactly why it lives
//! outside `commands/` (naming rule: command path = module path).

pub(crate) mod file_ext;
pub(crate) mod impact_payload;
pub(crate) mod proven_rule;
pub(crate) mod review_text;
pub(crate) mod stdio;
#[cfg(test)]
pub(crate) mod test_home;
pub(crate) mod util;
