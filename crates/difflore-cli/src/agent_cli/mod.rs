//! Non-hot-path LLM dispatch via the user's installed agent CLI.
//!
//! DiffLore wants to make occasional LLM calls (candidate-name
//! extraction, `plan_pr` secondary judgment, future session-mining
//! gates) without requiring the user to configure a BYOK provider —
//! the cloud-spec measurement showed 85–95% of users have no LLM key
//! configured at install time, so a feature gated on "user has Anthropic
//! / OpenAI / Gemini key" misses most of the user base.
//!
//! The workaround DiffLore borrows from Activeloop hivemind's
//! `gate-runner.ts`: if the user is already running inside (or alongside)
//! an agent CLI — Claude Code, Codex, cursor-agent, Gemini CLI — that
//! CLI's binary is on PATH (or in a well-known install location). We
//! shell out to it for the gate call, pay nothing for the LLM key
//! (the CLI handles auth via the user's existing session), and stay
//! out of the hot path so the CLI's latency only affects best-effort
//! gates, never the user-typed-command latency.
//!
//! ## Surface
//!
//! One public async function:
//!
//! ```rust,ignore
//! use std::time::Duration;
//! use difflore_cli::agent_cli::{AgentKind, dispatch_gate};
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
/// Returns a `GateResult` describing what the CLI printed and whether
/// it errored. Never panics — even on a missing binary, the result is
/// `{ errored: true, error_message: "...", stdout: "", stderr: "" }`
/// so callers can downgrade a failed gate to "skip, don't block".
///
/// `time_budget` is enforced via `tokio::time::timeout`; the child
/// process is killed on drop. Pick a budget that matches how
/// best-effort the gate is — DiffLore's typical gates run with 15-30s.
///
/// Side-effects: spawns a child process, inherits the parent's
/// environment, sends `prompt` as a single argv positional (the last
/// one) so prompt content that looks like a flag is dispatched as
/// argument data, not parsed by the CLI. If your prompt itself starts
/// with `-`, the dispatched CLI may misparse it — sanitise prompts
/// upstream of this call.
pub async fn dispatch_gate(agent: AgentKind, prompt: &str, time_budget: Duration) -> GateResult {
    runner::run(agent, prompt, time_budget).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn dispatch_gate_for_windsurf_errors_out_quickly() {
        // End-to-end-ish check that the public surface is wired up: a
        // Windsurf dispatch is the only agent we can fully exercise
        // without depending on a real CLI being installed on the test
        // host. Confirms `dispatch_gate` doesn't deadlock or panic on
        // the no-CLI path.
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
