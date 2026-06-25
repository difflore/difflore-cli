//! Non-hot-path LLM dispatch via the user's installed agent CLI.
//!
//! Lets DiffLore make occasional LLM calls (candidate-name extraction,
//! `plan_pr` secondary judgment, etc.) without requiring a BYOK provider —
//! most users have no LLM key configured at install time. If the user is
//! already running a gate4agent-supported CLI (Claude Code, Codex, Gemini CLI),
//! we call it through gate4agent for the gate call: the CLI handles auth via the
//! user's existing session, and dispatch stays off the hot path so its latency
//! only affects best-effort gates.
//!
//! ## Surface
//!
//! One public async function:
//!
//! ```rust,ignore
//! use std::time::Duration;
//! use difflore_cli::agent_exec::{AgentKind, dispatch_gate};
//!
//! async fn example() {
//!     let result = dispatch_gate(
//!         AgentKind::ClaudeCode,
//!         "Rate this PR description 1-5: ...",
//!         Duration::from_secs(30),
//!     ).await;
//!     if !result.errored {
//!         println!("model said: {}", result.stdout);
//!     }
//! }
//! ```
//!
//! ## Non-goals
//!
//! - Not a streaming API. The use cases are short ratings / yes-no
//!   gates / JSON envelopes — buffering the whole response is fine and
//!   simpler than wiring a streaming reader.
//! - Not a BYOK provider replacement. The existing
//!   `crate::commands::providers` flow stays the canonical path for
//!   power users who want per-model latency / cost knobs. Gate-runner-
//!   style CLI dispatch is the "no key required" fallback.

mod runner;
mod types;

pub use types::{AgentKind, GateResult};

use std::time::Duration;

/// Dispatch a single LLM call to whichever agent CLI matches `agent`.
///
/// Never panics — even on a missing binary the result is errored, so callers
/// can downgrade a failed gate to "skip, don't block". `time_budget` is
/// enforced via `tokio::time::timeout`; timed-out sessions are killed through
/// gate4agent.
pub async fn dispatch_gate(agent: AgentKind, prompt: &str, time_budget: Duration) -> GateResult {
    runner::run(agent, prompt, time_budget).await
}
