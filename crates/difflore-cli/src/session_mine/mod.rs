//! Session-mining candidate-rule supply pipeline.
//!
//! Unlike [`difflore_core::observability::classifier`], which classifies a single edit
//! deterministically, this module observes a *session* (the conversation's
//! prompts + assistant replies) and runs a small LLM gate to decide whether
//! the activity contains a reusable rule.
//!
//! Layers:
//!
//! * [`trigger::should_trigger_now`] — watermark gate, fires on SessionEnd /
//!   Stop or every N turns. Cheap; safe on the hook hot path.
//! * [`extract::extract_recent_session_pairs`] — pulls the last few
//!   user/assistant pairs from the transcript, stripping tool calls and
//!   thinking blocks.
//! * [`gate::run_gate`] — calls a small LLM with the extracted pairs +
//!   existing-rule digests and parses the verdict.
//! * [`worker::run_worker_detached`] — top-level entry the hook dispatcher
//!   spawns; composes the steps above and enqueues the candidate via
//!   [`difflore_core::cloud::outbox::OutboxQueue`] with kind
//!   `session_mined_candidate`.
//!
//! ## Defaults
//!
//! * Every emitted candidate carries `requires_human_approval = true` on the
//!   wire; we never short-circuit into active rules.
//! * If `source_repo` can't be derived, the worker drops the candidate
//!   (project-scope invariant: scopeless candidates are not enqueued).

pub mod extract;
pub mod gate;
pub mod trigger;
pub mod worker;

pub use gate::GateMode;
pub use trigger::should_trigger_now;
pub use worker::{run_targeted_pairs_detached, run_targeted_pairs_once, run_worker_detached};
