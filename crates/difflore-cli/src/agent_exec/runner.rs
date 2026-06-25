//! Invoke local agent CLIs through gate4agent and capture their output.

use std::time::{Duration, Instant};

use gate4agent::{AgentEvent, ClaudeOptions, PipeProcessOptions, PipeSession, SessionConfig};
use tokio::sync::broadcast;
use tokio::time::timeout;

use super::types::{AgentKind, GateResult};

/// Spawn the right gate4agent pipe transport for `agent`, feed it `prompt`,
/// collect streamed assistant text, and enforce `time_budget`.
///
/// Errors are surfaced via the `errored` flag, never as a panic, so the caller
/// can downgrade a failed gate to a best-effort skip without ceremony.
pub(super) async fn run(agent: AgentKind, prompt: &str, time_budget: Duration) -> GateResult {
    if time_budget.is_zero() {
        return no_time_budget_result(agent);
    }

    let working_dir = match std::env::current_dir() {
        Ok(dir) => dir,
        Err(e) => return GateResult::errored_with(format!("cwd lookup failed: {e}")),
    };

    let started = Instant::now();
    let config = SessionConfig {
        tool: agent.cli_tool(),
        working_dir,
        env_vars: vec![(
            difflore_core::cloud::capture::DIFFLORE_CAPTURE_ENV.to_owned(),
            "false".to_owned(),
        )],
        name: None,
    };
    let options = gate_options(agent);

    let session = match timeout(time_budget, PipeSession::spawn(config, prompt, options)).await {
        Ok(Ok(session)) => session,
        Ok(Err(e)) => {
            return GateResult::errored_with(format!(
                "failed to spawn {} through gate4agent: {e} (is it installed and on PATH?)",
                agent.label(),
            ));
        }
        Err(_) => return timeout_result(agent, time_budget),
    };

    let Some(remaining) = time_budget
        .checked_sub(started.elapsed())
        .filter(|d| !d.is_zero())
    else {
        let _ = session.kill().await;
        return timeout_result(agent, time_budget);
    };

    let rx = session.subscribe();
    if let Ok(result) = timeout(remaining, collect_agent_output(agent, rx)).await {
        result
    } else {
        let _ = session.kill().await;
        timeout_result(agent, time_budget)
    }
}

fn gate_options(agent: AgentKind) -> PipeProcessOptions {
    let mut options = PipeProcessOptions {
        extra_args: Vec::new(),
        claude: ClaudeOptions::default(),
    };

    match agent {
        AgentKind::ClaudeCode => {
            // Preserve the old gate runner's low-latency Claude default while
            // letting gate4agent own the headless invocation details.
            options.claude.model = Some("haiku".to_owned());
        }
        AgentKind::Codex => {
            // Keep Codex gate calls read-only. gate4agent owns the rest of the
            // `codex exec --json` wire form.
            options.extra_args.push("--sandbox".to_owned());
            options.extra_args.push("read-only".to_owned());
        }
        AgentKind::GeminiCli | AgentKind::OpenCode => {}
    }

    options
}

async fn collect_agent_output(
    agent: AgentKind,
    mut rx: broadcast::Receiver<AgentEvent>,
) -> GateResult {
    let mut stdout = String::new();
    let mut error_message: Option<String> = None;

    loop {
        match rx.recv().await {
            Ok(AgentEvent::Text { text, .. }) => stdout.push_str(&text),
            Ok(AgentEvent::SessionEnd {
                result, is_error, ..
            }) => {
                if is_error {
                    error_message = Some(result);
                }
                break;
            }
            Ok(AgentEvent::Error { message }) => {
                error_message = Some(message);
                break;
            }
            Ok(AgentEvent::Exited { code }) => {
                if code != 0 {
                    error_message = Some(format!("exit_code={code}"));
                }
                break;
            }
            Ok(_) => {}
            Err(broadcast::error::RecvError::Closed) => break,
            Err(broadcast::error::RecvError::Lagged(n)) => {
                error_message = Some(format!(
                    "event stream lagged: {n} message(s) dropped before consumer caught up"
                ));
                break;
            }
        }
    }

    let stdout = stdout.trim().to_owned();
    if let Some(message) = error_message {
        return GateResult {
            stdout,
            stderr: String::new(),
            errored: true,
            error_message: format!("{} failed: {message}", agent.label()),
        };
    }

    if stdout.is_empty() {
        return GateResult::errored_with(format!("{} returned empty response", agent.label()));
    }

    GateResult {
        stdout,
        stderr: String::new(),
        errored: false,
        error_message: String::new(),
    }
}

fn timeout_result(agent: AgentKind, time_budget: Duration) -> GateResult {
    GateResult::errored_with(format!(
        "{} timed out after {}s",
        agent.label(),
        time_budget.as_secs(),
    ))
}

fn no_time_budget_result(agent: AgentKind) -> GateResult {
    GateResult::errored_with(format!("no time budget available for {}", agent.label()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn zero_budget_returns_timeout_without_spawning() {
        let result = run(AgentKind::ClaudeCode, "ignored", Duration::ZERO).await;
        assert!(result.errored);
        assert!(result.error_message.contains("no time budget"));
        assert!(result.stdout.is_empty());
    }

    #[tokio::test]
    async fn collect_agent_output_accumulates_text_until_session_end() {
        let (tx, rx) = broadcast::channel(8);
        tx.send(AgentEvent::Text {
            text: "hello ".to_owned(),
            is_delta: true,
        })
        .unwrap();
        tx.send(AgentEvent::Text {
            text: "world\n".to_owned(),
            is_delta: true,
        })
        .unwrap();
        tx.send(AgentEvent::SessionEnd {
            result: "exit_code=0".to_owned(),
            cost_usd: None,
            is_error: false,
        })
        .unwrap();

        let result = collect_agent_output(AgentKind::Codex, rx).await;
        assert!(!result.errored);
        assert_eq!(result.stdout, "hello world");
    }

    #[tokio::test]
    async fn collect_agent_output_preserves_partial_stdout_on_error() {
        let (tx, rx) = broadcast::channel(8);
        tx.send(AgentEvent::Text {
            text: "partial".to_owned(),
            is_delta: true,
        })
        .unwrap();
        tx.send(AgentEvent::SessionEnd {
            result: "exit_code=1".to_owned(),
            cost_usd: None,
            is_error: true,
        })
        .unwrap();

        let result = collect_agent_output(AgentKind::GeminiCli, rx).await;
        assert!(result.errored);
        assert_eq!(result.stdout, "partial");
        assert!(result.error_message.contains("exit_code=1"));
    }

    #[test]
    fn gate_options_preserve_agent_specific_defaults() {
        let claude = gate_options(AgentKind::ClaudeCode);
        assert_eq!(claude.claude.model.as_deref(), Some("haiku"));

        let codex = gate_options(AgentKind::Codex);
        assert_eq!(codex.extra_args, vec!["--sandbox", "read-only"]);

        let gemini = gate_options(AgentKind::GeminiCli);
        assert!(gemini.extra_args.is_empty());
        assert!(gemini.claude.model.is_none());
    }
}
