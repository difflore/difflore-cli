//! Non-hot-path LLM dispatch via the user's installed agent CLI.
//!
//! Lets DiffLore make occasional LLM calls (candidate-name extraction,
//! `plan_pr` secondary judgment, etc.) without requiring a BYOK provider —
//! most users have no LLM key configured at install time. If the user is
//! already running an agent CLI (Claude Code, Codex, cursor-agent, Gemini CLI)
//! whose binary is on PATH, we shell out to it for the gate call: the CLI
//! handles auth via the user's existing session, and dispatch stays off the
//! hot path so its latency only affects best-effort gates.
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
//! - Not Windsurf-aware: Windsurf ships no headless CLI today. The
//!   enum still has a `Windsurf` variant for symmetry with the hook
//!   adapters, but `dispatch_gate` returns an errored `GateResult`
//!   immediately rather than try to spawn anything.

mod binary_finder;
mod runner;
mod types;

pub use types::{AgentKind, GateResult};

use std::time::Duration;

/// Dispatch a single LLM call to whichever agent CLI matches `agent`.
///
/// Never panics — even on a missing binary the result is errored, so callers
/// can downgrade a failed gate to "skip, don't block". `time_budget` is
/// enforced via `tokio::time::timeout` and the child is killed on drop.
///
/// Spawns a child process inheriting the parent's environment, sending
/// `prompt` as the last argv positional. A prompt that itself starts with `-`
/// may be misparsed by the CLI — sanitise prompts upstream of this call.
pub async fn dispatch_gate(agent: AgentKind, prompt: &str, time_budget: Duration) -> GateResult {
    runner::run(agent, prompt, time_budget).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn dispatch_gate_for_windsurf_errors_out_quickly() {
        // Windsurf is the only agent we can exercise without a real CLI on the
        // test host; confirms the no-CLI path doesn't deadlock or panic.
        let result = dispatch_gate(
            AgentKind::Windsurf,
            "anything",
            Duration::from_millis(50),
        )
        .await;
        assert!(result.errored);
        assert!(!result.error_message.is_empty());
    }

    #[tokio::test]
    async fn dispatch_gate_with_zero_budget_does_not_hang() {
        // Even a zero-duration budget must return promptly; pinning
        // this so a future refactor that uses `time_budget.saturating_*`
        // and accidentally treats Duration::ZERO as "no timeout" gets
        // caught here.
        let result = dispatch_gate(
            AgentKind::Windsurf,
            "anything",
            Duration::from_secs(0),
        )
        .await;
        assert!(result.errored);
    }
}
