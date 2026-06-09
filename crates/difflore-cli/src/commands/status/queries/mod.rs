//! SQL queries and the DTO/row structs they materialise for `status`.
//!
//! Layer boundary: every byte of SQL lives here; pure transforms live in
//! `super::transform`; rendering lives in `super::presentation`.
//!
//! Split by query domain so each cohesive group of SQL + its DTOs + tests lives
//! together. All names that `status` (and its `transform`/`presentation`
//! siblings) consumed from `queries::*` are re-exported below, so the public
//! `queries::Foo` paths are unchanged:
//! - [`proof_counters`]: repo-scoped accepted/recall/MCP proof counters, plus
//!   the shared window constants and repo-alias normaliser.
//! - [`proven_rule`]: the "most accepted edits" proven-rule drilldown.
//! - [`hero`]: current-repo "best local proof" hero evidence.
//! - [`value_loop`]: buyer-grade learned-then-served value-loop evidence.
//! - [`source_proof`]: where a rule was originally learned (review comments).

mod hero;
mod proof_counters;
mod proven_rule;
mod source_proof;
mod value_loop;

#[cfg(test)]
mod test_support;

pub(super) use hero::{LocalHeroEvidence, local_hero_evidence};
pub(super) use proof_counters::{
    LocalAcceptedProof, LocalMcpRuleServe, LocalRecallProof, local_accepted_proof,
    local_mcp_rule_serves, local_recall_proof,
};
pub(super) use proven_rule::{ProvenRuleDrilldown, local_proven_rule_drilldown};
pub(super) use value_loop::{ValueLoopAcceptedRow, ValueLoopEvidence, local_value_loop_evidence};
