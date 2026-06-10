//! Pull recent user-prompt / assistant-text pairs out of a platform
//! transcript so the gate can mine them. Strips tool calls, thinking blocks,
//! and other agentic scaffolding so only the conversation reaches the gate.
//!
//! Platform support today:
//!
//! * Claude Code — JSONL transcript at `transcript_path` (from the hook stdin
//!   payload). Full implementation here.
//! * Cursor / Gemini / Windsurf — return `Ok(vec![])` (adapters not yet
//!   written). The worker skips empty pairs, so this is a no-op, not an error.

use std::path::Path;

/// One conversation pair handed to the gate. `user_prompt` is the user message
/// before the assistant turn; `assistant_text` is the reply with tool calls and
/// thinking blocks stripped.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Pair {
    pub user_prompt: String,
    pub assistant_text: String,
}

/// Hard cap on a single pair (user + assistant combined); beyond this is
/// truncated with an ellipsis so the gate payload stays bounded.
pub const PAIR_MAX_CHARS: usize = 2_000;

/// Hard cap on the entire extraction (sum of all pairs). Aligns with a
/// ~10 K-token gate budget after JSON envelope overhead.
pub const TOTAL_MAX_CHARS: usize = 40_000;

/// Source platform discriminator. Only Claude Code has a complete extractor;
/// the rest return empty so the worker no-ops on unsupported transcripts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    ClaudeCode,
    Cursor,
    GeminiCli,
    Windsurf,
}

impl Platform {
    /// Look up the platform by the name string the hook adapter reports.
    pub fn from_client_name(client: &str) -> Self {
        match client.to_ascii_lowercase().as_str() {
            "cursor" => Self::Cursor,
            "gemini-cli" | "gemini_cli" | "gemini" => Self::GeminiCli,
            "windsurf" => Self::Windsurf,
            _ => Self::ClaudeCode,
        }
    }
}

/// Inputs accepted by [`extract_recent_session_pairs`]. Bundled so future
/// platforms can add fields without breaking call sites.
#[derive(Debug, Clone, Copy)]
pub struct ExtractArgs<'a> {
    pub platform: Platform,
    /// Path to the platform's transcript JSONL (Claude Code) or equivalent.
    /// May be missing for older hook payloads.
    pub transcript_path: Option<&'a str>,
    /// Hook session id, used for trace logging.
    pub session_id: Option<&'a str>,
    /// Upper bound on pair count (the worker passes ~10).
    pub max_pairs: usize,
}

/// Read the recent session pairs from the transcript and strip framework noise.
/// Returns an empty vec when the transcript is missing, malformed, or the
/// platform isn't supported; the worker short-circuits on `is_empty()`.
pub fn extract_recent_session_pairs(args: ExtractArgs<'_>) -> std::io::Result<Vec<Pair>> {
    if args.max_pairs == 0 {
        return Ok(Vec::new());
    }
    match args.platform {
        Platform::ClaudeCode => extract_from_claude_code(&args),
        // TODO(extract): wire Cursor / Gemini / Windsurf transcripts. Empty
        // result == worker no-op, the safe default until the adapters land.
        _ => Ok(Vec::new()),
    }
}

/// Cap on hook-supplied transcript reads. Only recent pairs are needed, so
/// reading the tail is sufficient; a truncated first line is skipped.
const MAX_TRANSCRIPT_BYTES: u64 = 32 * 1024 * 1024;

fn read_transcript_tail_capped(path: &str) -> std::io::Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    if len > MAX_TRANSCRIPT_BYTES {
        file.seek(SeekFrom::Start(len - MAX_TRANSCRIPT_BYTES))?;
    }
    let mut buf = Vec::new();
    file.take(MAX_TRANSCRIPT_BYTES).read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Claude Code: JSONL transcript, one event per line (same shape as
/// `hook_runtime::stated_vs_actual` consumes). Re-implements the walk rather
/// than reusing that helper because we need ordered user+assistant pairs, not
/// just the last assistant blob.
fn extract_from_claude_code(args: &ExtractArgs<'_>) -> std::io::Result<Vec<Pair>> {
    let Some(transcript_path) = args.transcript_path else {
        return Ok(Vec::new());
    };
    if transcript_path.trim().is_empty() {
        return Ok(Vec::new());
    }
    if !Path::new(transcript_path).exists() {
        return Ok(Vec::new());
    }

    let body = read_transcript_tail_capped(transcript_path)?;
    let mut pending_user: Option<String> = None;
    let mut pairs: Vec<Pair> = Vec::new();
    let mut total_chars: usize = 0;

    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        match row_role(&value) {
            Some(Role::User) => {
                pending_user = extract_text_content(&value);
            }
            Some(Role::Assistant) => {
                let assistant_text = extract_text_content(&value).unwrap_or_default();
                if assistant_text.trim().is_empty() {
                    continue;
                }
                let Some(user_prompt) = pending_user.take() else {
                    // Assistant turn with no preceding user turn (system event).
                    continue;
                };
                if user_prompt.trim().is_empty() {
                    continue;
                }
                let pair = build_capped_pair(&user_prompt, &assistant_text);
                let pair_len = pair.user_prompt.chars().count()
                    + pair.assistant_text.chars().count();
                if total_chars.saturating_add(pair_len) > TOTAL_MAX_CHARS {
                    // Stop before blowing the budget; the earlier pairs form a
                    // coherent prefix rather than a truncated tail.
                    break;
                }
                total_chars += pair_len;
                pairs.push(pair);
            }
            _ => {}
        }
    }

    // Take the most recent N pairs (transcript is chronological).
    let len = pairs.len();
    let start = len.saturating_sub(args.max_pairs);
    Ok(pairs.split_off(start))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    User,
    Assistant,
}

fn row_role(value: &serde_json::Value) -> Option<Role> {
    // Claude Code marks the row type two ways depending on version: top-level
    // `"type"` and/or nested `"message":{"role":...}`. Mirrors
    // `stated_vs_actual::read_last_assistant_text` so the readers don't diverge.
    let top = value.get("type").and_then(serde_json::Value::as_str);
    let nested = value
        .get("message")
        .and_then(|m| m.get("role"))
        .and_then(serde_json::Value::as_str);
    let any = top.or(nested)?;
    match any {
        "user" => Some(Role::User),
        "assistant" => Some(Role::Assistant),
        _ => None,
    }
}

/// Extract conversational text from a Claude Code message row, stripping tool
/// calls and thinking blocks. Returns `None` for a row with no usable text.
fn extract_text_content(value: &serde_json::Value) -> Option<String> {
    let content = value.get("message").and_then(|m| m.get("content"))?;
    if let Some(s) = content.as_str() {
        return Some(s.to_owned());
    }
    let arr = content.as_array()?;
    let mut buf = String::new();
    for part in arr {
        let part_type = part
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        // Keep only plain text parts. `thinking` is the reasoning trace and
        // must never leave the local machine via the gate payload.
        if part_type != "text" {
            continue;
        }
        if let Some(text) = part.get("text").and_then(serde_json::Value::as_str) {
            if !buf.is_empty() {
                buf.push('\n');
            }
            buf.push_str(text);
        }
    }
    if buf.is_empty() { None } else { Some(buf) }
}

fn build_capped_pair(user_prompt: &str, assistant_text: &str) -> Pair {
    let user = truncate_chars(user_prompt.trim(), PAIR_MAX_CHARS / 2);
    let assistant = truncate_chars(assistant_text.trim(), PAIR_MAX_CHARS / 2);
    Pair {
        user_prompt: user,
        assistant_text: assistant,
    }
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_jsonl(lines: &[&str]) -> tempfile::NamedTempFile {
        let mut f = tempfile::Builder::new()
            .suffix(".jsonl")
            .tempfile()
            .expect("tempfile");
        for line in lines {
            writeln!(f, "{line}").expect("write jsonl line");
        }
        f
    }

    fn cc_args(transcript_path: Option<&str>, max_pairs: usize) -> ExtractArgs<'_> {
        ExtractArgs {
            platform: Platform::ClaudeCode,
            transcript_path,
            session_id: Some("sess_t"),
            max_pairs,
        }
    }

    #[test]
    fn missing_transcript_path_returns_empty_not_error() {
        // The hook may fire before the transcript exists on disk; that must
        // degrade to empty + no-op, not an io::Error.
        let args = cc_args(None, 10);
        let pairs = extract_recent_session_pairs(args).expect("no panic");
        assert!(pairs.is_empty());

        let args = cc_args(Some("/definitely/does/not/exist"), 10);
        let pairs = extract_recent_session_pairs(args).expect("no panic");
        assert!(pairs.is_empty());
    }

    #[test]
    fn extracts_pairs_in_order_and_strips_tool_calls() {
        // Two turns with interleaved tool_use parts; keep only text, paired
        // with the preceding user prompt.
        let f = write_jsonl(&[
            r#"{"type":"user","message":{"role":"user","content":"please fix the bug"}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Edit","input":{}},{"type":"text","text":"fixed the panic in unwrap"}]}}"#,
            r#"{"type":"user","message":{"role":"user","content":"add tests"}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"hmm"},{"type":"text","text":"added two test cases"}]}}"#,
        ]);
        let path = f.path().to_str().unwrap().to_owned();
        let pairs = extract_recent_session_pairs(cc_args(Some(&path), 10)).expect("ok");
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0].user_prompt, "please fix the bug");
        assert_eq!(pairs[0].assistant_text, "fixed the panic in unwrap");
        assert_eq!(pairs[1].user_prompt, "add tests");
        assert_eq!(pairs[1].assistant_text, "added two test cases");
    }

    #[test]
    fn drops_thinking_blocks_from_assistant_text() {
        // Hard contract: thinking content MUST NOT reach the gate payload —
        // a leak would expose the agent's internal reasoning to the provider.
        let f = write_jsonl(&[
            r#"{"type":"user","message":{"role":"user","content":"q"}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"private chain-of-thought"},{"type":"text","text":"final answer"}]}}"#,
        ]);
        let path = f.path().to_str().unwrap().to_owned();
        let pairs = extract_recent_session_pairs(cc_args(Some(&path), 10)).expect("ok");
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].assistant_text, "final answer");
        assert!(
            !pairs[0]
                .assistant_text
                .contains("private chain-of-thought"),
            "thinking blocks must never reach the gate"
        );
    }

    #[test]
    fn max_pairs_limits_returned_window_to_recent_n() {
        let f = write_jsonl(&[
            r#"{"type":"user","message":{"role":"user","content":"u1"}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"a1"}]}}"#,
            r#"{"type":"user","message":{"role":"user","content":"u2"}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"a2"}]}}"#,
            r#"{"type":"user","message":{"role":"user","content":"u3"}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"a3"}]}}"#,
        ]);
        let path = f.path().to_str().unwrap().to_owned();
        let pairs = extract_recent_session_pairs(cc_args(Some(&path), 2)).expect("ok");
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0].user_prompt, "u2");
        assert_eq!(pairs[1].user_prompt, "u3");
    }

    #[test]
    fn empty_assistant_text_pair_is_dropped() {
        // A row with only tool_use parts has empty assistant text; the pair
        // must be skipped, not emitted with an empty string field.
        let f = write_jsonl(&[
            r#"{"type":"user","message":{"role":"user","content":"go"}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Edit","input":{}}]}}"#,
        ]);
        let path = f.path().to_str().unwrap().to_owned();
        let pairs = extract_recent_session_pairs(cc_args(Some(&path), 10)).expect("ok");
        assert!(pairs.is_empty());
    }

    #[test]
    fn cursor_platform_returns_empty_until_adapter_lands() {
        // Locks in the no-adapter contract so a future adapter must update
        // this test rather than silently changing behaviour.
        let args = ExtractArgs {
            platform: Platform::Cursor,
            transcript_path: Some("/tmp/whatever"),
            session_id: Some("sess"),
            max_pairs: 10,
        };
        let pairs = extract_recent_session_pairs(args).expect("ok");
        assert!(pairs.is_empty());
    }
}
