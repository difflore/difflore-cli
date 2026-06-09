//! Session-mining: the **fourth** candidate-rule supply pipeline.
//!
//! Pattern borrowed from Activeloop hivemind's
//! `src/skillify/skillify-worker.ts`. While the existing
//! [`difflore_core::observation`] module classifies a single edit
//! deterministically, this module observes a *session* (the
//! conversation's prompts + assistant replies) and runs a small LLM
//! gate to decide whether the activity contains a reusable rule.
//!
//! Wiring layer-by-layer:
//!
//! * [`trigger::should_trigger_now`] — watermark gate, fires on
//!   SessionEnd / Stop or every N turns. Cheap; safe to call from the
//!   hook hot path.
//! * [`extract::extract_recent_session_pairs`] — pulls the last few
//!   user/assistant pairs from the platform transcript and strips
//!   tool calls + thinking blocks.
//! * [`gate::run_gate`] — STUB. Calls a small LLM (Haiku-class) with
//!   the extracted pairs + existing-rule digests and parses the
//!   verdict. Implementation deferred to the follow-up PR that wires
//!   it into the agent dispatch layer.
//! * [`worker::run_worker_detached`] — top-level entry the hook
//!   dispatcher spawns. Composes the three steps above and enqueues
//!   the resulting candidate via
//!   [`difflore_core::cloud::outbox::OutboxQueue`] with kind
//!   `session_mined_candidate`.
//!
//! ## Defaults
//!
//! * Every emitted candidate carries `requires_human_approval = true`
//!   on the wire — the cloud-side promoter must refuse to promote
//!   anything else. We never short-circuit into active rules.
//! * If `source_repo` can't be derived (no git remote, no usable cwd
//!   basename), the worker drops the candidate. Project Scope
//!   Invariant: scopeless candidates are not enqueued.

pub mod extract;
pub mod gate;
pub mod trigger;
pub mod worker;

pub use trigger::should_trigger_now;
pub use worker::run_worker_detached;
