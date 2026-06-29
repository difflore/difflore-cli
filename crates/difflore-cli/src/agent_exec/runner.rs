//! Invoke local agent CLIs and capture their output.

use std::time::{Duration, Instant};

use gate4agent::{AgentEvent, ClaudeOptions, PipeProcessOptions, PipeSession, SessionConfig};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::broadcast;
use tokio::time::{sleep, timeout};

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
    if agent == AgentKind::Codex {
        return run_codex_exec(prompt, time_budget).await;
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

async fn run_codex_exec(prompt: &str, time_budget: Duration) -> GateResult {
    let working_dir = match std::env::current_dir() {
        Ok(dir) => dir,
        Err(e) => return GateResult::errored_with(format!("cwd lookup failed: {e}")),
    };

    let mut command = Command::new("codex");
    command
        .arg("exec")
        .arg("--json")
        .arg("--sandbox")
        .arg("read-only")
        .arg("--skip-git-repo-check")
        .arg("--ignore-rules")
        .arg("--ephemeral")
        .arg("--color")
        .arg("never")
        .arg("-")
        .current_dir(working_dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    apply_agent_child_env(&mut command);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            return GateResult::errored_with(format!(
                "failed to spawn codex exec: {e} (is it installed and on PATH?)"
            ));
        }
    };

    if let Some(mut stdin) = child.stdin.take() {
        if let Err(e) = stdin.write_all(prompt.as_bytes()).await {
            let _ = kill_process_tree(&mut child).await;
            return GateResult::errored_with(format!("failed to write prompt to codex: {e}"));
        }
    }

    let stdout_task = child
        .stdout
        .take()
        .map(|stdout| tokio::spawn(read_pipe_to_string(stdout)));
    let stderr_task = child
        .stderr
        .take()
        .map(|stderr| tokio::spawn(read_pipe_to_string(stderr)));

    let status = tokio::select! {
        status = child.wait() => status,
        () = sleep(time_budget) => {
            let _ = kill_process_tree(&mut child).await;
            return timeout_result(AgentKind::Codex, time_budget);
        }
    };

    let stdout = join_pipe_output(stdout_task).await;
    let stderr = join_pipe_output(stderr_task).await;

    let status = match status {
        Ok(status) => status,
        Err(e) => {
            return GateResult {
                stdout: String::new(),
                stderr,
                errored: true,
                error_message: format!("codex wait failed: {e}"),
            };
        }
    };

    match parse_codex_exec_stdout(&stdout) {
        Ok(stdout) if status.success() => GateResult {
            stdout,
            stderr,
            errored: false,
            error_message: String::new(),
        },
        Ok(stdout) => GateResult {
            stdout,
            stderr: stderr.clone(),
            errored: true,
            error_message: format!(
                "codex failed: exit_code={}{}",
                status
                    .code()
                    .map_or_else(|| "unknown".to_owned(), |code| code.to_string()),
                first_stderr_line(&stderr)
                    .map(|line| format!(" stderr={line}"))
                    .unwrap_or_default()
            ),
        },
        Err(message) => GateResult {
            stdout: String::new(),
            stderr: stderr.clone(),
            errored: true,
            error_message: format!(
                "codex output parse failed: {message}{}",
                first_stderr_line(&stderr)
                    .map(|line| format!(" stderr={line}"))
                    .unwrap_or_default()
            ),
        },
    }
}

fn apply_agent_child_env(command: &mut Command) {
    command.env(difflore_core::cloud::capture::DIFFLORE_CAPTURE_ENV, "false");
    // Avoid inheriting the parent Codex desktop thread/session into the nested
    // one-shot CLI invocation used for local extraction.
    command.env_remove("CODEX_THREAD_ID");
    command.env_remove("CODEX_SHELL");
    command.env_remove("CODEX_INTERNAL_ORIGINATOR_OVERRIDE");

    if let Some(proxy) = agent_proxy_override() {
        command.env("HTTP_PROXY", &proxy);
        command.env("HTTPS_PROXY", &proxy);
        command.env("ALL_PROXY", proxy);
    }
}

fn agent_proxy_override() -> Option<String> {
    std::env::var("DIFFLORE_AGENT_PROXY")
        .or_else(|_| std::env::var("DIFFLORE_LOCAL_AGENT_PROXY"))
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

async fn read_pipe_to_string<R>(mut pipe: R) -> String
where
    R: AsyncRead + Unpin,
{
    let mut bytes = Vec::new();
    if pipe.read_to_end(&mut bytes).await.is_err() {
        return String::new();
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

async fn join_pipe_output(task: Option<tokio::task::JoinHandle<String>>) -> String {
    match task {
        Some(task) => task.await.unwrap_or_default(),
        None => String::new(),
    }
}

async fn kill_process_tree(child: &mut tokio::process::Child) -> std::io::Result<()> {
    #[cfg(windows)]
    if let Some(pid) = child.id() {
        let status = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
        if status.is_ok() {
            let _ = child.wait().await;
            return Ok(());
        }
    }
    child.kill().await
}

fn parse_codex_exec_stdout(stdout: &str) -> Result<String, String> {
    let mut messages = Vec::new();
    let mut errors = Vec::new();

    for line in stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("item.completed") => {
                if let Some(item) = value.get("item")
                    && matches!(
                        item.get("type").and_then(Value::as_str),
                        Some("agent_message" | "assistant_message")
                    )
                    && let Some(text) = item.get("text").and_then(Value::as_str)
                    && !text.trim().is_empty()
                {
                    messages.push(text.to_owned());
                }
            }
            Some("turn.failed") => {
                if let Some(error) = value.get("error").and_then(Value::as_str) {
                    errors.push(error.to_owned());
                }
            }
            _ => {}
        }
    }

    if !messages.is_empty() {
        return Ok(messages.join("\n").trim().to_owned());
    }
    if !errors.is_empty() {
        return Err(errors.join("; "));
    }
    Err("no assistant message in codex JSONL".to_owned())
}

fn first_stderr_line(stderr: &str) -> Option<String> {
    stderr
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| line.chars().take(240).collect())
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

    #[test]
    fn parse_codex_exec_stdout_reads_agent_message() {
        let stdout = r#"{"type":"thread.started","thread_id":"t"}
{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"{\"ok\":true}"}}
{"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":1}}"#;

        let parsed = parse_codex_exec_stdout(stdout).expect("parse codex stdout");

        assert_eq!(parsed, "{\"ok\":true}");
    }

    #[test]
    fn parse_codex_exec_stdout_reports_turn_failure_without_message() {
        let stdout = r#"{"type":"thread.started","thread_id":"t"}
{"type":"turn.failed","error":"authentication_failed"}"#;

        let err = parse_codex_exec_stdout(stdout).expect_err("turn failure");

        assert!(err.contains("authentication_failed"));
    }
}
