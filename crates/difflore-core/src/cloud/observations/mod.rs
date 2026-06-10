//! Durable flywheel observations for rule recall and edit outcomes.
//!
//! Separate from the PostToolUse observation classifier: classifier
//! observations describe "what changed"; these describe which rule was shown,
//! appeared to guide an edit, and was kept or reverted.

mod dedup;
mod events;
mod storage;
mod sync;

pub use dedup::{RECENT_RULE_FIRE_WINDOW_MS, event_content_hash};
pub use events::{
    AcceptedFixOutcomeRuleSummary, AcceptedRecallLinkSummary, ActualCitationSummary, CitedEdit,
    ObservationEvent, ObservationUploadIssue, RuleFireSnapshot,
};
pub use storage::{
    ObservationEmitter, accepted_fix_proof_sources_default, actual_citation_summary_default,
    enqueue_and_flush_default, enqueue_default,
};
