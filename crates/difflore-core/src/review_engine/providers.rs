use crate::error::CoreError;
use gate4agent::{
    AgentEvent, ClaudeOptions, CliTool, PipeProcessOptions, PipeSession, SessionConfig,
};
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use super::SegmentedPrompt;

/// Sentinel scheme routing a provider through a local agent CLI
/// (`agent-cli://claude`, `agent-cli://codex`, etc.) via `gate4agent`
/// instead of an HTTP endpoint. No real URL uses this scheme.
pub const AGENT_CLI_SCHEME: &str = "agent-cli://";

/// Parse a provider `base_url` into a `CliTool` if it is an agent-CLI
/// sentinel. Returns `None` for HTTP base URLs.
///
/// Also accepts legacy per-tool schemes (`claude-cli://`, etc.) still
/// present in old SQLite rows; without them, those rows fall through to
/// the HTTP path where reqwest rejects the scheme as an auth failure.
fn parse_agent_cli(base_url: &str) -> Option<CliTool> {
    if let Some(rest) = base_url.strip_prefix(AGENT_CLI_SCHEME) {
        let tool = rest.split('/').next().unwrap_or(rest);
        return match tool {
            "claude" | "claude-code" => Some(CliTool::ClaudeCode),
            "codex" => Some(CliTool::Codex),
            "gemini" => Some(CliTool::Gemini),
            "opencode" => Some(CliTool::OpenCode),
            _ => None,
        };
    }
    // Legacy per-tool schemes.
    if base_url.starts_with("claude-cli://") || base_url.starts_with("claude-code-cli://") {
        return Some(CliTool::ClaudeCode);
    }
    if base_url.starts_with("codex-cli://") {
        return Some(CliTool::Codex);
    }
    if base_url.starts_with("gemini-cli://") {
        return Some(CliTool::Gemini);
    }
    if base_url.starts_with("opencode-cli://") {
        return Some(CliTool::OpenCode);
    }
    None
}

/// Canonical sentinel string for a given tool.
pub const fn agent_cli_sentinel(tool: CliTool) -> &'static str {
    match tool {
        CliTool::ClaudeCode => "agent-cli://claude",
        CliTool::Codex => "agent-cli://codex",
        CliTool::Gemini => "agent-cli://gemini",
        CliTool::OpenCode => "agent-cli://opencode",
    }
}

fn is_anthropic_provider(provider_name: &str, base_url: &str) -> bool {
    let official_host = reqwest::Url::parse(base_url)
        .is_ok_and(|u| u.host_str().is_some_and(|h| h == "api.anthropic.com"));
    if official_host {
        return true;
    }

    let name = provider_name.to_lowercase();
    let tokens = name
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();
    tokens.contains(&"anthropic")
        || tokens.contains(&"anth")
        || (tokens.contains(&"claude") && !tokens.contains(&"cli"))
}

fn anthropic_messages_url(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    if trimmed.ends_with("/v1/messages") {
        trimmed.to_owned()
    } else if trimmed.ends_with("/v1") {
        format!("{trimmed}/messages")
    } else {
        format!("{trimmed}/v1/messages")
    }
}

const fn auth_hint(tool: CliTool) -> &'static str {
    match tool {
        CliTool::ClaudeCode => {
            "; run `claude /login` once, or pick another provider with `difflore providers setup`"
        }
        CliTool::Codex => {
            "; run `codex login` once, or pick another provider with `difflore providers setup`"
        }
        CliTool::Gemini => {
            "; run `gemini auth login` once, or pick another provider with `difflore providers setup`"
        }
        CliTool::OpenCode => {
            "; check `opencode auth` status, or pick another provider with `difflore providers setup`"
        }
    }
}

/// Drive a local agent CLI (`claude` / `codex` / `gemini` / `opencode`) and
/// collect the assistant text. Codex and Claude use their native headless
/// commands so review works with normal local login state while suppressing
/// user hooks/MCP customizations. The remaining tools still route through
/// `gate4agent`.
pub(super) async fn call_agent_cli_provider(
    tool: CliTool,
    model: &str,
    system_prompt: &str,
    user_prompt: &str,
) -> crate::Result<String> {
    let prompt = if system_prompt.trim().is_empty() {
        user_prompt.to_owned()
    } else {
        format!("System instructions:\n{system_prompt}\n\nUser request:\n{user_prompt}")
    };

    let working_dir = std::env::current_dir()
        .map_err(|e| CoreError::Internal(format!("cwd lookup failed: {e}")))?;

    match tool {
        CliTool::Codex => return call_codex_cli_provider(model, &prompt, &working_dir).await,
        CliTool::ClaudeCode => {
            return call_claude_cli_provider(model, &prompt, &working_dir).await;
        }
        CliTool::Gemini | CliTool::OpenCode => {}
    }

    let mut extra_args: Vec<String> = Vec::new();
    let mut claude_opts = ClaudeOptions::default();

    if !model.is_empty() {
        match tool {
            CliTool::ClaudeCode => claude_opts.model = Some(model.to_owned()),
            CliTool::Codex | CliTool::Gemini => {
                extra_args.push("-m".into());
                extra_args.push(model.into());
            }
            CliTool::OpenCode => {
                extra_args.push("--model".into());
                extra_args.push(model.into());
            }
        }
    }

    let config = SessionConfig {
        tool,
        working_dir,
        env_vars: vec![(
            crate::cloud::capture::DIFFLORE_CAPTURE_ENV.to_owned(),
            "false".to_owned(),
        )],
        name: None,
    };
    let options = PipeProcessOptions {
        extra_args,
        claude: claude_opts,
    };

    let session = PipeSession::spawn(config, &prompt, options)
        .await
        .map_err(|e| {
            CoreError::Internal(format!(
                "failed to spawn {tool} CLI: {e} (is it installed and on PATH?)"
            ))
        })?;

    let mut rx = session.subscribe();
    let mut buf = String::new();
    let mut session_error: Option<String> = None;

    loop {
        match rx.recv().await {
            Ok(AgentEvent::Text { text, .. }) => buf.push_str(&text),
            Ok(AgentEvent::SessionEnd {
                result, is_error, ..
            }) => {
                if is_error {
                    session_error = Some(result);
                }
                break;
            }
            Ok(AgentEvent::Error { message }) => {
                session_error = Some(message);
                break;
            }
            Ok(AgentEvent::Exited { code }) => {
                if code != 0 && session_error.is_none() {
                    session_error = Some(format!("exit_code={code}"));
                }
                break;
            }
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            // Lagged means gate4agent's 256-event broadcast buffer dropped
            // some events because we couldn't keep up. Text events accumulate
            // contiguously so a gap means a hole in the assistant output —
            // surface it rather than silently truncate.
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                session_error = Some(format!(
                    "event stream lagged: {n} message(s) dropped before consumer caught up"
                ));
                break;
            }
        }
    }

    if let Some(err) = session_error {
        return Err(CoreError::Internal(format!(
            "{tool} CLI failed: {err}{}",
            auth_hint(tool)
        )));
    }

    if buf.trim().is_empty() {
        return Err(CoreError::Internal(format!(
            "{tool} CLI returned empty response{}",
            auth_hint(tool)
        )));
    }

    Ok(buf)
}

fn codex_output_path() -> PathBuf {
    std::env::temp_dir().join(format!(
        "difflore-codex-review-{}.txt",
        uuid::Uuid::new_v4()
    ))
}

fn push_agent_env(command: &mut Command) {
    command.env(crate::cloud::capture::DIFFLORE_CAPTURE_ENV, "false");
}

fn resolve_agent_cli_program(program: &str, tool: CliTool) -> crate::Result<PathBuf> {
    which::which(program).map_err(|e| {
        CoreError::Internal(format!(
            "failed to locate {tool} CLI `{program}` on PATH: {e} (install it, or pick another provider with `difflore providers setup`)"
        ))
    })
}

fn path_arg(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn codex_cli_args(model: &str, working_dir: &Path, output_path: &Path) -> Vec<String> {
    let mut args = vec![
        "-a".to_owned(),
        "never".to_owned(),
        "exec".to_owned(),
        "--ignore-user-config".to_owned(),
        "--ignore-rules".to_owned(),
        "--ephemeral".to_owned(),
        "--color".to_owned(),
        "never".to_owned(),
        "-s".to_owned(),
        "read-only".to_owned(),
    ];
    if !model.trim().is_empty() {
        args.push("-m".to_owned());
        args.push(model.to_owned());
    }
    args.extend([
        "-C".to_owned(),
        path_arg(working_dir),
        "--output-last-message".to_owned(),
        path_arg(output_path),
        "-".to_owned(),
    ]);
    args
}

fn claude_cli_args(model: &str) -> Vec<String> {
    let mut args = vec![
        "-p".to_owned(),
        "--no-session-persistence".to_owned(),
        "--permission-mode".to_owned(),
        "dontAsk".to_owned(),
        "--safe-mode".to_owned(),
        "--output-format".to_owned(),
        "text".to_owned(),
    ];
    if !model.trim().is_empty() {
        args.push("--model".to_owned());
        args.push(model.to_owned());
    }
    args
}

async fn call_codex_cli_provider(
    model: &str,
    prompt: &str,
    working_dir: &Path,
) -> crate::Result<String> {
    let output_path = TempFileCleanup::new(codex_output_path());
    let program = resolve_agent_cli_program("codex", CliTool::Codex)?;
    let mut command = Command::new(&program);
    command.current_dir(working_dir);
    command.args(codex_cli_args(model, working_dir, output_path.path()));
    push_agent_env(&mut command);

    let output = command_output_with_prompt(
        command,
        prompt,
        &format!("Codex CLI `{}`", program.display()),
    )
    .await?;
    if !output.status.success() {
        return Err(CoreError::Internal(format!(
            "Codex CLI failed: {}{}",
            process_output_summary(&output),
            auth_hint_for_output(CliTool::Codex, &output)
        )));
    }

    let final_message = tokio::fs::read_to_string(output_path.path())
        .await
        .unwrap_or_default();
    if final_message.trim().is_empty() {
        return Err(CoreError::Internal(
            "Codex CLI returned empty response; check `codex exec --help` and the active provider configuration".to_owned(),
        ));
    }
    Ok(final_message)
}

async fn call_claude_cli_provider(
    model: &str,
    prompt: &str,
    working_dir: &Path,
) -> crate::Result<String> {
    let program = resolve_agent_cli_program("claude", CliTool::ClaudeCode)?;
    let mut command = Command::new(&program);
    command.current_dir(working_dir);
    command.args(claude_cli_args(model));
    push_agent_env(&mut command);

    let output = command_output_with_prompt(
        command,
        prompt,
        &format!("Claude CLI `{}`", program.display()),
    )
    .await?;
    if !output.status.success() {
        return Err(CoreError::Internal(format!(
            "Claude Code CLI failed: {}{}",
            process_output_summary(&output),
            auth_hint_for_output(CliTool::ClaudeCode, &output)
        )));
    }

    let response = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if response.is_empty() {
        return Err(CoreError::Internal(
            "Claude Code CLI returned empty response; check `claude --help` and the active provider configuration".to_owned(),
        ));
    }
    Ok(response)
}

async fn command_output_with_prompt(
    mut command: Command,
    prompt: &str,
    label: &str,
) -> Result<Output, CoreError> {
    command.stdin(Stdio::piped());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    command.kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|e| CoreError::Internal(format!("failed to spawn {label}: {e}")))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| CoreError::Internal(format!("failed to open stdin pipe for {label}")))?;
    // Write the prompt while concurrently draining stdout/stderr. Writing the
    // whole prompt first and only then reading output deadlocks once the child
    // fills its stdout/stderr pipe buffer (~64 KB) before it finishes reading
    // stdin — review/fix prompts (rules + repo context + diff) routinely exceed
    // that, so both sides would block forever.
    let prompt_bytes = prompt.as_bytes();
    let write_fut = async move {
        let result = stdin.write_all(prompt_bytes).await;
        drop(stdin);
        result
    };
    let (write_result, output) = tokio::join!(write_fut, child.wait_with_output());
    let output = output.map_err(|e| {
        CoreError::Internal(format!("{label} failed while waiting for output: {e}"))
    })?;
    if let Err(e) = write_result
        && output.status.success()
    {
        return Err(CoreError::Internal(format!(
            "{label} exited before reading the prompt from stdin: {e}"
        )));
    }
    Ok(output)
}

struct TempFileCleanup {
    path: PathBuf,
}

impl TempFileCleanup {
    const fn new(path: PathBuf) -> Self {
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempFileCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn auth_hint_for_output(tool: CliTool, output: &Output) -> &'static str {
    if output_looks_auth_related(output) {
        auth_hint(tool)
    } else {
        ""
    }
}

fn output_looks_auth_related(output: &Output) -> bool {
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
    .to_ascii_lowercase();
    [
        "not authenticated",
        "authentication",
        "unauthorized",
        "api key",
        "log in",
        "login",
        "/login",
        "401",
    ]
    .iter()
    .any(|needle| combined.contains(needle))
}

fn process_output_summary(output: &Output) -> String {
    let code = output.status.code().map_or_else(
        || "terminated".to_owned(),
        |code| format!("exit_code={code}"),
    );
    let stdout = compact_process_stream(&output.stdout);
    let stderr = compact_process_stream(&output.stderr);
    match (stdout.is_empty(), stderr.is_empty()) {
        (true, true) => code,
        (false, true) => format!("{code}; stdout={stdout}"),
        (true, false) => format!("{code}; stderr={stderr}"),
        (false, false) => format!("{code}; stdout={stdout}; stderr={stderr}"),
    }
}

fn compact_process_stream(bytes: &[u8]) -> String {
    const MAX_CHARS: usize = 1200;
    let text = String::from_utf8_lossy(bytes);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let compact = trimmed
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if compact.chars().count() <= MAX_CHARS {
        compact
    } else {
        let mut out = compact.chars().take(MAX_CHARS).collect::<String>();
        out.push_str("...");
        out
    }
}

// ── Anthropic native client (Messages API with prompt caching) ──

/// Call the Anthropic Messages API (`/v1/messages`) with prompt caching.
///
/// The prompt splits into a cacheable `stable_prefix` (rules + repo
/// context, identical across a PR's perspectives) and a per-call
/// `dynamic_suffix` (verdicts + diff), sent as two content blocks of one
/// `user` message. The first block carries
/// `cache_control: { type: "ephemeral" }` so the backend reuses the
/// KV-cache across the PR's perspective calls.
async fn call_anthropic_provider(
    base_url: &str,
    api_key: &str,
    model: &str,
    system_prompt: &str,
    stable_prefix: &str,
    dynamic_suffix: &str,
) -> crate::Result<String> {
    // Bound stalled connections so a hung provider can't hang `review`/`fix`
    // indefinitely (the patch-generation path has no outer timeout). The total
    // cap stays generous so legitimately slow completions still finish.
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(180))
        .build()
        .map_err(|e| CoreError::Internal(format!("failed to build provider HTTP client: {e}")))?;

    let content = if dynamic_suffix.is_empty() {
        serde_json::json!([
            {
                "type": "text",
                "text": stable_prefix
            }
        ])
    } else {
        serde_json::json!([
            {
                "type": "text",
                "text": stable_prefix,
                "cache_control": { "type": "ephemeral" }
            },
            {
                "type": "text",
                "text": dynamic_suffix
            }
        ])
    };

    let body = serde_json::json!({
        "model": model,
        "max_tokens": 4096,
        "system": system_prompt,
        "messages": [{
            "role": "user",
            "content": content
        }]
    });

    let response = client
        .post(anthropic_messages_url(base_url))
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| CoreError::Internal(format!("Anthropic request failed: {e}")))?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(CoreError::Internal(format!(
            "Anthropic returned {status}: {text}"
        )));
    }

    #[derive(serde::Deserialize)]
    struct ContentBlock {
        text: Option<String>,
    }
    #[derive(serde::Deserialize)]
    #[allow(clippy::struct_field_names)] // reason: matches Anthropic's API field names
    struct AnthropicUsage {
        #[serde(default)]
        cache_read_input_tokens: Option<u32>,
        #[serde(default)]
        cache_creation_input_tokens: Option<u32>,
    }
    #[derive(serde::Deserialize)]
    struct AnthropicResponse {
        content: Vec<ContentBlock>,
        usage: Option<AnthropicUsage>,
    }

    let resp: AnthropicResponse = response
        .json()
        .await
        .map_err(|e| CoreError::Internal(format!("Failed to parse Anthropic response: {e}")))?;

    if crate::infra::env::debug_providers()
        && let Some(ref usage) = resp.usage
        && let Some(read) = usage.cache_read_input_tokens
    {
        eprintln!(
            "[anthropic] cache_read_input_tokens={}, cache_creation_input_tokens={}",
            read,
            usage.cache_creation_input_tokens.unwrap_or(0)
        );
    }

    resp.content
        .into_iter()
        .find_map(|block| block.text.filter(|text| !text.trim().is_empty()))
        .ok_or_else(|| CoreError::Internal("Anthropic returned empty content".into()))
}

// ── OpenAI-compatible client ──

async fn call_openai_provider(
    base_url: &str,
    api_key: &str,
    model: &str,
    system_prompt: &str,
    user_prompt: &str,
) -> crate::Result<String> {
    // Bound stalled connections so a hung provider can't hang `review`/`fix`
    // indefinitely (the patch-generation path has no outer timeout). The total
    // cap stays generous so legitimately slow completions still finish.
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(180))
        .build()
        .map_err(|e| CoreError::Internal(format!("failed to build provider HTTP client: {e}")))?;

    let body = serde_json::json!({
        "model": model,
        "messages": [
            { "role": "system", "content": system_prompt },
            { "role": "user", "content": user_prompt }
        ],
        "temperature": 0.1,
        "max_tokens": 4096
    });

    let response = client
        .post(format!(
            "{}/chat/completions",
            base_url.trim_end_matches('/')
        ))
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| CoreError::Internal(format!("AI provider request failed: {e}")))?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(CoreError::Internal(format!(
            "AI provider returned {status}: {text}"
        )));
    }

    #[derive(serde::Deserialize)]
    struct ChatChoice {
        message: ChatMessage,
    }
    #[derive(serde::Deserialize)]
    struct ChatMessage {
        content: Option<String>,
    }
    #[derive(serde::Deserialize)]
    struct ChatResponse {
        choices: Vec<ChatChoice>,
    }

    let chat: ChatResponse = response
        .json()
        .await
        .map_err(|e| CoreError::Internal(format!("Failed to parse AI response: {e}")))?;

    chat.choices
        .first()
        .and_then(|c| c.message.content.clone())
        .ok_or_else(|| CoreError::Internal("AI returned empty response".into()))
}

// ── Unified dispatch ──

pub(super) async fn call_ai_provider(
    provider_name: &str,
    base_url: &str,
    api_key: &str,
    model: &str,
    system_prompt: &str,
    user_prompt: &str,
) -> crate::Result<String> {
    if let Some(tool) = parse_agent_cli(base_url) {
        return call_agent_cli_provider(tool, model, system_prompt, user_prompt).await;
    }
    if is_anthropic_provider(provider_name, base_url) {
        let prompt = if system_prompt.trim().is_empty() {
            user_prompt.to_owned()
        } else {
            format!("System instructions:\n{system_prompt}\n\nUser request:\n{user_prompt}")
        };
        call_anthropic_provider(base_url, api_key, model, "", &prompt, "").await
    } else {
        call_openai_provider(base_url, api_key, model, system_prompt, user_prompt).await
    }
}

pub(super) async fn call_ai_provider_segmented(
    provider_name: &str,
    base_url: &str,
    api_key: &str,
    model: &str,
    segmented: &SegmentedPrompt,
    user_prompt: &str,
) -> crate::Result<String> {
    if let Some(tool) = parse_agent_cli(base_url) {
        // Agent CLIs have no prompt caching, so flatten the
        // stable/dynamic split into system (stable_prefix) + user
        // (dynamic_suffix + user_prompt).
        return call_agent_cli_provider(
            tool,
            model,
            &segmented.stable_prefix,
            &format!("{}\n\n{}", segmented.dynamic_suffix, user_prompt),
        )
        .await;
    }
    if is_anthropic_provider(provider_name, base_url) {
        call_anthropic_provider(
            base_url,
            api_key,
            model,
            "",
            &segmented.stable_prefix,
            &format!("{}\n\n{}", segmented.dynamic_suffix, user_prompt),
        )
        .await
    } else {
        let flat = format!("{}{}", segmented.stable_prefix, segmented.dynamic_suffix);
        call_openai_provider(base_url, api_key, model, &flat, user_prompt).await
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AGENT_CLI_SCHEME, agent_cli_sentinel, anthropic_messages_url, auth_hint_for_output,
        claude_cli_args, codex_cli_args, is_anthropic_provider, parse_agent_cli,
    };
    use gate4agent::CliTool;
    use std::path::Path;
    use std::process::{ExitStatus, Output};

    #[cfg(unix)]
    fn exit_status(code: i32) -> ExitStatus {
        use std::os::unix::process::ExitStatusExt as _;
        ExitStatus::from_raw(code << 8)
    }

    #[cfg(windows)]
    fn exit_status(code: u32) -> ExitStatus {
        use std::os::windows::process::ExitStatusExt as _;
        ExitStatus::from_raw(code)
    }

    fn process_output(stdout: &str, stderr: &str) -> Output {
        Output {
            status: exit_status(2),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[test]
    fn agent_cli_scheme_routes_each_supported_tool() {
        assert_eq!(
            parse_agent_cli("agent-cli://claude"),
            Some(CliTool::ClaudeCode)
        );
        assert_eq!(parse_agent_cli("agent-cli://codex"), Some(CliTool::Codex));
        assert_eq!(parse_agent_cli("agent-cli://gemini"), Some(CliTool::Gemini));
        assert_eq!(
            parse_agent_cli("agent-cli://opencode"),
            Some(CliTool::OpenCode)
        );
    }

    #[test]
    fn http_base_urls_are_not_agent_cli() {
        assert_eq!(parse_agent_cli("https://api.anthropic.com"), None);
        assert_eq!(parse_agent_cli("http://wucur.com:6543/v1"), None);
    }

    #[test]
    fn unknown_agent_cli_tool_is_rejected() {
        assert_eq!(parse_agent_cli("agent-cli://bogus"), None);
    }

    #[test]
    fn agent_cli_sentinel_round_trips_through_parse() {
        for tool in [
            CliTool::ClaudeCode,
            CliTool::Codex,
            CliTool::Gemini,
            CliTool::OpenCode,
        ] {
            let s = agent_cli_sentinel(tool);
            assert!(s.starts_with(AGENT_CLI_SCHEME));
            assert_eq!(parse_agent_cli(s), Some(tool));
        }
    }

    #[test]
    fn codex_cli_args_disable_user_config_and_capture_last_message() {
        let args = codex_cli_args(
            "gpt-test",
            Path::new("C:/repo"),
            Path::new("C:/tmp/out.txt"),
        );

        assert!(args.contains(&"--ignore-user-config".to_owned()));
        assert!(args.contains(&"--ignore-rules".to_owned()));
        assert!(args.contains(&"--ephemeral".to_owned()));
        assert!(args.windows(2).any(|pair| pair == ["-a", "never"]));
        assert_eq!(args.get(2).map(String::as_str), Some("exec"));
        assert!(args.windows(2).any(|pair| pair == ["-m", "gpt-test"]));
        assert!(args.windows(2).any(|pair| pair == ["-C", "C:/repo"]));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--output-last-message", "C:/tmp/out.txt"])
        );
        assert_eq!(args.last().map(String::as_str), Some("-"));
        assert!(!args.iter().any(|arg| arg.contains("reply with json")));
    }

    #[test]
    fn claude_cli_args_use_safe_stateless_text_mode() {
        let args = claude_cli_args("sonnet-test");

        assert!(args.contains(&"--no-session-persistence".to_owned()));
        assert!(args.contains(&"--safe-mode".to_owned()));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--permission-mode", "dontAsk"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--model", "sonnet-test"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--output-format", "text"])
        );
        assert_eq!(args.last().map(String::as_str), Some("sonnet-test"));
        assert!(!args.iter().any(|arg| arg.contains("reply with json")));
    }

    #[test]
    fn auth_hint_is_only_appended_for_auth_like_cli_failures() {
        let flag_error = process_output("", "error: unexpected argument '--ignore-user-config'");
        assert_eq!(auth_hint_for_output(CliTool::Codex, &flag_error), "");

        let auth_error = process_output("", "Not authenticated. Please run codex login.");
        assert!(auth_hint_for_output(CliTool::Codex, &auth_error).contains("codex login"));
    }

    #[test]
    fn official_anthropic_host_uses_native_messages_api() {
        assert!(is_anthropic_provider(
            "anything",
            "https://api.anthropic.com"
        ));
    }

    #[test]
    fn custom_claude_compatible_provider_name_uses_native_messages_api() {
        assert!(is_anthropic_provider(
            "claude-compatible",
            "http://wucur.com:6543"
        ));
    }

    #[test]
    fn abbreviated_anthropic_provider_name_uses_native_messages_api() {
        assert!(is_anthropic_provider(
            "proxy-anth",
            "http://wucur.com:6543/v1"
        ));
    }

    #[test]
    fn anth_substrings_inside_unrelated_words_stay_openai_compatible() {
        assert!(!is_anthropic_provider(
            "panther-ai",
            "http://wucur.com:6543/v1"
        ));
        assert!(!is_anthropic_provider(
            "elephant-proxy",
            "http://wucur.com:6543/v1"
        ));
    }

    #[test]
    fn openai_compatible_provider_name_stays_on_chat_completions() {
        assert!(!is_anthropic_provider(
            "openai-compatible",
            "http://wucur.com:6543"
        ));
    }

    #[test]
    fn anthropic_messages_url_appends_versioned_path_without_double_slash() {
        assert_eq!(
            anthropic_messages_url("http://wucur.com:6543/"),
            "http://wucur.com:6543/v1/messages"
        );
    }

    #[test]
    fn anthropic_messages_url_respects_existing_versioned_base_path() {
        assert_eq!(
            anthropic_messages_url("http://wucur.com:6543/v1"),
            "http://wucur.com:6543/v1/messages"
        );
        assert_eq!(
            anthropic_messages_url("http://wucur.com:6543/v1/messages"),
            "http://wucur.com:6543/v1/messages"
        );
    }
}
