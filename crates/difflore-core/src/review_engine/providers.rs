use crate::error::CoreError;
use gate4agent::{
    AgentEvent, ClaudeOptions, CliTool, PipeProcessOptions, PipeSession, SessionConfig,
};
use serde::Deserialize;
use std::process::Stdio;
use tokio::io::AsyncWriteExt;

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

#[derive(Deserialize)]
struct ClaudePrintResult {
    result: Option<String>,
    is_error: Option<bool>,
    subtype: Option<String>,
}

fn truncate_for_error(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

fn parse_claude_print_stdout(stdout: &str) -> crate::Result<String> {
    let parsed: ClaudePrintResult = serde_json::from_str(stdout.trim()).map_err(|e| {
        CoreError::Internal(format!(
            "Claude Code CLI returned non-JSON output: {e}; stdout={}",
            truncate_for_error(&scrub_secrets(stdout), 300)
        ))
    })?;
    if parsed.is_error == Some(true) || parsed.subtype.as_deref() == Some("error") {
        return Err(CoreError::Internal(format!(
            "Claude Code CLI returned an error response: {}",
            truncate_for_error(&scrub_secrets(parsed.result.as_deref().unwrap_or("")), 300)
        )));
    }
    parsed
        .result
        .filter(|result| !result.trim().is_empty())
        .ok_or_else(|| CoreError::Internal("Claude Code CLI returned empty response".into()))
}

fn claude_cli_failure_detail(stdout: &str, stderr: &str) -> String {
    let stderr = stderr.trim();
    if !stderr.is_empty() {
        return stderr.to_owned();
    }

    let stdout = stdout.trim();
    if let Ok(parsed) = serde_json::from_str::<ClaudePrintResult>(stdout)
        && let Some(result) = parsed.result.filter(|result| !result.trim().is_empty())
    {
        return result;
    }

    stdout.to_owned()
}

fn is_transient_claude_failure(exit_code: Option<i32>, detail: &str) -> bool {
    if matches!(exit_code, Some(124 | 137)) {
        return true;
    }
    let lower = detail.to_ascii_lowercase();
    lower.contains("timeout")
        || lower.contains("connection reset")
        || lower.contains("temporarily")
        || lower.contains("rate limit")
}

async fn call_claude_cli_direct(model: &str, prompt: &str) -> crate::Result<String> {
    // Guard against argv injection: a `model` starting with `-` would be
    // read as a flag by the claude CLI.
    if model.starts_with('-') {
        return Err(CoreError::Internal(format!(
            "invalid model identifier {model:?}: must not start with '-'"
        )));
    }

    // Up to 2 attempts on transient failures. Re-spawn each time
    // because a tokio Child can only be awaited once.
    let mut last_err: Option<CoreError> = None;
    for attempt in 0..2_u32 {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        }

        let mut cmd = tokio::process::Command::new("claude");
        cmd.arg("--print")
            .arg("--output-format")
            .arg("json")
            .arg("--no-session-persistence")
            .arg("--disable-slash-commands")
            .arg("--tools")
            .arg("")
            .arg("--exclude-dynamic-system-prompt-sections")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if !model.trim().is_empty() {
            cmd.arg("--model").arg(model);
        }

        for (key, _) in std::env::vars() {
            if key.starts_with("CLAUDECODE") || key.starts_with("CLAUDE_CODE_") {
                cmd.env_remove(key);
            }
        }

        let Ok(mut child) = cmd.spawn() else {
            last_err = Some(CoreError::Internal(
                "failed to spawn Claude Code CLI (is it installed and on PATH?)".to_owned(),
            ));
            // Spawn failure is not transient — return immediately.
            break;
        };
        let Some(mut stdin) = child.stdin.take() else {
            last_err = Some(CoreError::Internal(
                "failed to open Claude Code CLI stdin".to_owned(),
            ));
            break;
        };
        if let Err(e) = stdin.write_all(prompt.as_bytes()).await {
            last_err = Some(CoreError::Internal(format!(
                "failed to write Claude Code CLI prompt: {e}"
            )));
            break;
        }
        drop(stdin);

        let output = match child.wait_with_output().await {
            Ok(o) => o,
            Err(e) => {
                last_err = Some(CoreError::Internal(format!(
                    "failed to read Claude Code CLI output: {e}"
                )));
                break;
            }
        };
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if output.status.success() {
            return parse_claude_print_stdout(&stdout);
        }

        let detail = claude_cli_failure_detail(&stdout, &stderr);
        let exit_code = output.status.code();
        let scrubbed = scrub_secrets(detail.trim());
        let err = CoreError::Internal(format!(
            "Claude Code CLI failed: {}{}",
            truncate_for_error(&scrubbed, 180),
            auth_hint(CliTool::ClaudeCode)
        ));
        if is_transient_claude_failure(exit_code, &detail) && attempt + 1 < 2 {
            last_err = Some(err);
            continue;
        }
        return Err(err);
    }

    Err(last_err.unwrap_or_else(|| CoreError::Internal("Claude Code CLI failed".into())))
}

/// Replace common secret token prefixes (`sk-`, `Bearer `, `ghp_`,
/// `github_pat_`) with `[REDACTED]` so error output never leaks keys.
/// Each prefix must be followed by enough opaque characters to look
/// like a real secret.
fn scrub_secrets(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if let Some(consumed) = try_scrub_prefix(bytes, i) {
            out.push_str("[REDACTED]");
            i += consumed;
            continue;
        }
        let ch_end = next_utf8_boundary(bytes, i);
        out.push_str(&input[i..ch_end]);
        i = ch_end;
    }
    out
}

fn next_utf8_boundary(bytes: &[u8], i: usize) -> usize {
    let first = bytes[i];
    let width = match first {
        0x00..=0xBF => 1, // includes continuation bytes defensively
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    };
    (i + width).min(bytes.len())
}

fn try_scrub_prefix(bytes: &[u8], i: usize) -> Option<usize> {
    const LITERAL_PREFIXES: &[&[u8]] = &[b"sk-", b"ghp_", b"github_pat_"];
    for prefix in LITERAL_PREFIXES {
        if bytes[i..].starts_with(prefix) {
            let body_start = i + prefix.len();
            let body_len = count_secret_body(&bytes[body_start..]);
            if body_len >= 10 {
                return Some(prefix.len() + body_len);
            }
        }
    }
    // Case-insensitive "Bearer " followed by token chars.
    if i + 7 <= bytes.len() {
        let head = &bytes[i..i + 6];
        if head.eq_ignore_ascii_case(b"Bearer") {
            let mut j = i + 6;
            // Require at least one whitespace.
            let ws_start = j;
            while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
                j += 1;
            }
            if j > ws_start {
                let body_start = j;
                let body_len = count_secret_body(&bytes[body_start..]);
                if body_len >= 8 {
                    return Some(body_start + body_len - i);
                }
            }
        }
    }
    None
}

fn count_secret_body(bytes: &[u8]) -> usize {
    let mut n = 0;
    while n < bytes.len() {
        let b = bytes[n];
        if b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'+' | b'/' | b'=') {
            n += 1;
        } else {
            break;
        }
    }
    n
}

/// Drive a local agent CLI (`claude` / `codex` / `gemini` / `opencode`)
/// through `gate4agent` and collect the streamed assistant text;
/// `gate4agent` handles each tool's headless flag dance.
///
/// For Claude with `ANTHROPIC_API_KEY` set, `--bare` is load-bearing:
/// without it, `claude` auto-discovers MCP servers, skills, memory, and
/// `CLAUDE.md` from the environment, corrupting review behaviour.
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
    // The Claude --print direct path works reliably on Windows, where
    // gate4agent's session spawn returns exit_code=1 with empty stderr.
    // Default to direct; opt out with DIFFLORE_CLAUDE_DIRECT=0.
    if matches!(tool, CliTool::ClaudeCode)
        && std::env::var("DIFFLORE_CLAUDE_DIRECT")
            .map_or(true, |v| v != "0" && !v.eq_ignore_ascii_case("false"))
    {
        return call_claude_cli_direct(model, &prompt).await;
    }

    let working_dir = std::env::current_dir()
        .map_err(|e| CoreError::Internal(format!("cwd lookup failed: {e}")))?;

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

    if matches!(tool, CliTool::ClaudeCode)
        && crate::infra::env::var(crate::infra::env::ANTHROPIC_API_KEY).is_some()
    {
        extra_args.push("--bare".into());
    }

    let config = SessionConfig {
        tool,
        working_dir,
        env_vars: Vec::new(),
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
    let client = reqwest::Client::new();

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
    let client = reqwest::Client::new();

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
        AGENT_CLI_SCHEME, agent_cli_sentinel, anthropic_messages_url, is_anthropic_provider,
        parse_agent_cli, parse_claude_print_stdout, scrub_secrets,
    };
    use gate4agent::CliTool;

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
    fn parses_claude_print_json_result() {
        let out = parse_claude_print_stdout(r#"{"type":"result","is_error":false,"result":"OK"}"#)
            .unwrap();

        assert_eq!(out, "OK");
    }

    #[test]
    fn rejects_claude_print_error_result() {
        let err = parse_claude_print_stdout(
            r#"{"type":"result","subtype":"error","is_error":true,"result":"auth failed"}"#,
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("Claude Code CLI returned an error response"));
    }

    #[test]
    fn scrub_secrets_redacts_standard_base64_token_bodies() {
        let scrubbed =
            scrub_secrets("provider failed: Bearer abc.def+ghi/jkl== and ghp_abcdEFGH1234+/= tail");

        assert_eq!(scrubbed, "provider failed: [REDACTED] and [REDACTED] tail");
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
