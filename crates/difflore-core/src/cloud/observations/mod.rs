//! Durable flywheel observations for rule recall and edit outcomes.
//!
//! This is separate from the older PostToolUse observation classifier:
//! classifier observations describe "what changed"; these events describe
//! "which rule was shown, appeared to guide an edit, and was kept/reverted".

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
