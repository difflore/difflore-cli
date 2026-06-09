//! Actually invoke the agent CLI binary and capture its output.
//!
//! Split out from `mod.rs` so the public surface stays a single
//! function (`dispatch_gate`) while the spawn / timeout / capture
//! plumbing is testable independently. Arg construction is a free
//! function (`build_args`) so unit tests can verify the exact CLI we
//! synthesise for each agent without having to spawn anything.

use std::ffi::OsString;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command as TokioCommand;
use tokio::time::timeout;

use super::binary_finder::{command_name, find_binary};
use super::types::{AgentKind, GateResult};

/// Spawn the right CLI for `agent`, feed it `prompt`, capture stdout
/// plus stderr, enforce `time_budget`. Always returns a `GateResult`.
///
/// Errors are surfaced via the `errored` flag, never as a panic, so
/// the caller can downgrade a failed gate to a best-effort skip
/// without ceremony.
pub(super) async fn run(agent: AgentKind, prompt: &str, time_budget: Duration) -> GateResult {
    // Windsurf has no headless CLI today. Return early with a clear
    // error so the caller knows to fall back rather than try to debug
    // a missing-binary report.
    if command_name(agent).is_none() {
        return GateResult::errored_with(format!(
            "{} has no headless CLI; configure a different agent or BYOK provider",
            agent.label(),
        ));
    }

    let Some(binary) = find_binary(agent) else {
        return GateResult::errored_with(format!(
            "could not locate {} on PATH or in any known install location",
            agent.label(),
        ));
    };

    let args = build_args(agent, prompt);
    let via_stdin = prompt_via_stdin(agent);

    let mut command = TokioCommand::new(&binary);
    command.args(&args);
    // Inherit the parent's environment but kill the child if we get
    // dropped while waiting — prevents orphaned CLI processes if the
    // caller times out at a higher level and gives up on us.
    command.kill_on_drop(true);
    // Force-disable telemetry capture in the spawned agent so its hooks
    // do not re-emit observations while the parent is already observing.
    command.env(difflore_core::cloud::capture::DIFFLORE_CAPTURE_ENV, "false");
    // We spawn manually (rather than `command.output()`) so we can feed the
    // prompt over stdin for agents that read it there — capture both streams
    // ourselves.
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    // For stdin-capable agents, send the prompt through stdin so repo code and
    // diffs do not land in argv. Other agents keep stdin null and receive the
    // prompt as the last positional.
    command.stdin(if via_stdin {
        Stdio::piped()
    } else {
        Stdio::null()
    });

    let stdin_prompt = via_stdin.then_some(prompt);
    let spawn_result = match timeout(time_budget, spawn_and_capture(command, stdin_prompt)).await
    {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => {
            return GateResult::errored_with(format!(
                "failed to spawn {}: {e}",
                binary.display(),
            ));
        }
        Err(_) => {
            return GateResult::errored_with(format!(
                "{} timed out after {}s",
                agent.label(),
                time_budget.as_secs(),
            ));
        }
    };

    let stdout = String::from_utf8_lossy(&spawn_result.stdout).trim().to_owned();
    let stderr = String::from_utf8_lossy(&spawn_result.stderr).trim().to_owned();

    if spawn_result.status.success() {
        return GateResult {
            stdout,
            stderr,
            errored: false,
            error_message: String::new(),
        };
    }

    // Non-zero exit: still surface whatever the CLI printed — many
    // agents print a partial JSON / partial answer on stdout before
    // erroring out. Caller decides if `stdout` is usable.
    let code = spawn_result
        .status
        .code()
        .map_or_else(|| "no exit code".to_owned(), |c| format!("exit {c}"));
    GateResult {
        stdout,
        stderr,
        errored: true,
        error_message: format!("{} failed ({code})", agent.label()),
    }
}

/// Whether `agent`'s CLI reads its prompt from stdin (so we can keep the
/// prompt out of argv). Only agents with a confirmed stdin contract are listed:
/// Claude Code and Codex. Cursor and Gemini stay on argv until verified.
///
/// INVARIANT: an agent returns `true` here IFF `build_args` omits the prompt
/// positional for it. The `build_args_matches_prompt_via_stdin` test pins this.
fn prompt_via_stdin(agent: AgentKind) -> bool {
    matches!(agent, AgentKind::ClaudeCode | AgentKind::Codex)
}

/// Spawn `command`, optionally feed `stdin_prompt` to the child's stdin (then
/// close it so the CLI sees EOF), and capture stdout + stderr. Split out so
/// `run` can wrap the whole spawn-write-wait in a single `timeout`. A write
/// error on the child's stdin (e.g. the CLI exited before reading it) is
/// swallowed — we still want whatever the CLI managed to print.
async fn spawn_and_capture(
    mut command: TokioCommand,
    stdin_prompt: Option<&str>,
) -> std::io::Result<std::process::Output> {
    use tokio::io::AsyncWriteExt;
    let mut child = command.spawn()?;
    if let Some(prompt) = stdin_prompt
        && let Some(mut stdin) = child.stdin.take()
    {
        let _ = stdin.write_all(prompt.as_bytes()).await;
        let _ = stdin.shutdown().await;
    }
    child.wait_with_output().await
}

/// Build the argv. Flags come first; the `prompt` is appended as the LAST
/// positional ONLY for agents that do NOT read it from stdin (see
/// `prompt_via_stdin`) — stdin agents get an empty-of-prompt argv. Kept as a
/// free function so tests can assert the exact wire form without spawning.
pub(super) fn build_args(agent: AgentKind, prompt: &str) -> Vec<OsString> {
    let mut args: Vec<OsString> = Vec::new();
    match agent {
        AgentKind::ClaudeCode => {
            // Headless, no-persist, Haiku for cost/latency. `--no-session-
            // persistence` keeps the gate call from polluting the user's
            // active Claude Code session history; `--permission-mode
            // bypassPermissions` lets the call run without an interactive
            // tool-approval prompt (we're only asking the model to read
            // and respond, no tool use).
            args.push(OsString::from("-p"));
            args.push(OsString::from("--no-session-persistence"));
            args.push(OsString::from("--model"));
            args.push(OsString::from("haiku"));
            args.push(OsString::from("--permission-mode"));
            args.push(OsString::from("bypassPermissions"));
            // Prompt travels via stdin, not argv.
        }
        AgentKind::Codex => {
            // `exec` is codex's headless invocation. The dangerous-bypass
            // flag matches hivemind's gate-runner; we never let the gate
            // trigger tool calls, so the bypass is moot for our use but
            // necessary to suppress the per-tool approval prompt.
            args.push(OsString::from("exec"));
            args.push(OsString::from("--dangerously-bypass-approvals-and-sandbox"));
            // Prompt travels via stdin, not argv.
        }
        AgentKind::Cursor => {
            // `--print` exits after one response; `--force` skips any
            // confirmation flow; `--output-format text` makes the stdout
            // a plain string rather than JSON-wrapped, simpler for the
            // caller's downstream parsing.
            args.push(OsString::from("--print"));
            args.push(OsString::from("--model"));
            args.push(OsString::from("auto"));
            args.push(OsString::from("--force"));
            args.push(OsString::from("--output-format"));
            args.push(OsString::from("text"));
            args.push(OsString::from(prompt));
        }
        AgentKind::GeminiCli => {
            // TODO(2026-05-26): verify the right headless flag against
            // `gemini --help`. `-p` is the documented prompt flag and
            // also what the hooks adapter is named for, but the headless
            // / no-confirm story may need an extra flag in a future
            // gemini-cli release. Track in the gate-runner integration
            // test once we have one.
            args.push(OsString::from("-p"));
            args.push(OsString::from(prompt));
        }
        AgentKind::Windsurf => {
            // Unreachable: `run` short-circuits before reaching
            // `build_args` for Windsurf via the `command_name` check.
            // We return an empty argv (rather than panic / unreachable!)
            // so the type stays total and a future refactor doesn't
            // turn a misuse into an abort.
        }
    }
    args
}

/// Test-only escape hatch so unit tests can render argv as Strings
/// without sprinkling `to_string_lossy` everywhere.
#[cfg(test)]
pub(super) fn args_as_strings(args: &[OsString]) -> Vec<String> {
    args.iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_args_use_haiku_no_session_bypass() {
        // Prompt goes via stdin, not argv.
        let args = args_as_strings(&build_args(AgentKind::ClaudeCode, "hello"));
        assert_eq!(
            args,
            vec![
                "-p",
                "--no-session-persistence",
                "--model",
                "haiku",
                "--permission-mode",
                "bypassPermissions",
            ]
        );
        assert!(!args.contains(&"hello".to_owned()));
    }

    #[test]
    fn codex_args_use_exec_with_bypass() {
        // Prompt goes via stdin, not argv.
        let args = args_as_strings(&build_args(AgentKind::Codex, "hi"));
        assert_eq!(
            args,
            vec!["exec", "--dangerously-bypass-approvals-and-sandbox"]
        );
        assert!(!args.contains(&"hi".to_owned()));
    }

    #[test]
    fn cursor_args_force_text_output_with_auto_model() {
        let args = args_as_strings(&build_args(AgentKind::Cursor, "go"));
        assert_eq!(
            args,
            vec![
                "--print",
                "--model",
                "auto",
                "--force",
                "--output-format",
                "text",
                "go",
            ]
        );
    }

    #[test]
    fn gemini_args_use_print_flag() {
        let args = args_as_strings(&build_args(AgentKind::GeminiCli, "ok"));
        assert_eq!(args, vec!["-p", "ok"]);
    }

    #[test]
    fn windsurf_args_empty_because_unreachable_path() {
        // `run` short-circuits Windsurf before reaching build_args; the
        // empty argv is a defensive default and must stay empty so a
        // future regression that accidentally hits build_args doesn't
        // silently dispatch garbage arguments to some other CLI.
        let args = args_as_strings(&build_args(AgentKind::Windsurf, "ignored"));
        assert!(args.is_empty());
    }

    #[test]
    fn prompt_is_last_arg_for_argv_delivered_agents() {
        // For agents that take the prompt via argv (NOT stdin), it must be the
        // LAST positional: with no `--` separator a prompt starting with `-`
        // would otherwise be parsed as a flag. Pins current behaviour so a
        // future refactor that moves the prompt mid-args is flagged in CI.
        for (agent, prompt) in [
            (AgentKind::Cursor, "cursor-prompt"),
            (AgentKind::GeminiCli, "gemini-prompt"),
        ] {
            let args = args_as_strings(&build_args(agent, prompt));
            assert_eq!(args.last().map(String::as_str), Some(prompt), "agent {agent:?}");
        }
    }

    #[test]
    fn build_args_matches_prompt_via_stdin() {
        // Invariant: the prompt appears in argv iff the agent is not a stdin
        // agent. This pins the contract between prompt_via_stdin and build_args.
        let secret = "PROMPT-SENTINEL-1234";
        for agent in [
            AgentKind::ClaudeCode,
            AgentKind::Codex,
            AgentKind::Cursor,
            AgentKind::GeminiCli,
        ] {
            let args = args_as_strings(&build_args(agent, secret));
            let in_argv = args.iter().any(|a| a.contains(secret));
            assert_eq!(
                in_argv,
                !prompt_via_stdin(agent),
                "agent {agent:?}: prompt-in-argv should equal !prompt_via_stdin",
            );
        }
    }

    #[tokio::test]
    async fn windsurf_returns_errored_without_spawning() {
        // Windsurf must short-circuit with a meaningful error rather
        // than try to spawn a non-existent binary (which would surface
        // as a much less helpful "could not locate" message).
        let result = run(AgentKind::Windsurf, "ignored", Duration::from_millis(10)).await;
        assert!(result.errored);
        assert!(result.error_message.contains("no headless CLI"));
        assert!(result.stdout.is_empty());
    }
}
