//! LLM gate for session-mined candidates.
//!
//! Calls the user's installed agent CLI (Claude Code / Codex / cursor-
//! agent / Gemini) via [`crate::agent_exec::dispatch_gate`] to decide
//! whether a session contains a reusable rule.
//!
//! ## Prompt shape
//!
//! The prompt is plain text (no system / user split — single-shot CLIs
//! don't expose a system slot reliably across all four agents) and
//! looks like:
//!
//! ```text
//! You are a code-review-rules librarian. Decide whether the following
//! short session contains a reusable, transferable rule about how to
//! write code in this team's repo.
//!
//! EXISTING RULES (do not duplicate):
//! - rule-1: Prefer typed deserialization …
//! - rule-2: …
//!
//! SESSION (prompt + final assistant text only — tool calls stripped):
//! USER: please fix the bug
//! ASSISTANT: fixed the panic in unwrap …
//! USER: add tests
//! ASSISTANT: added two test cases …
//!
//! DECISION CRITERIA:
//! - KEEP if the activity contains a non-obvious, reusable rule
//!   (file_pattern + behavior).
//! - MERGE <existing-id> if it strengthens or refines an existing rule.
//! - SKIP if it's one-off / generic / obvious / already covered.
//!
//! RESPOND WITH STRICT JSON (no prose, no markdown fence):
//! { "verdict": "KEEP" | "MERGE" | "SKIP",
//!   "rule_id": "<existing id if MERGE, else null>",
//!   "title": "<≤120 chars, only if KEEP/MERGE>",
//!   "body":  "<≤2000 chars rule body, only if KEEP/MERGE>",
//!   "file_patterns": ["<glob1>", "..."],
//!   "reason": "<short justification, used only if SKIP>" }
//! ```
//!
//! ## Tolerance
//!
//! Agent CLIs occasionally wrap their JSON in a markdown code fence
//! despite the instruction; the parser strips a single leading
//! ```` ```json ```` (or ```` ``` ````) fence and trailing ```` ``` ```` before deserializing. If
//! the model emits prose before the JSON, we scan for the first `{`
//! and try from there. If parsing still fails, the gate returns
//! [`GateError::ParseFailure`] and the worker drops the candidate.

use std::time::Duration;

use super::extract::Pair;
use crate::agent_exec::{AgentKind, GateResult, dispatch_gate};
use difflore_core::cloud::session_mined::{
    SessionMinedCandidate, SessionMinedCandidateArgs,
};

/// Total prompt budget. Roughly ~30 KB ≈ ~7-8 K tokens after the JSON
/// envelope overhead, leaving headroom for the model's response. Pairs
/// are taken from the end (most recent first) when truncation kicks in.
const PROMPT_MAX_CHARS: usize = 30_000;

/// Maximum existing-rule digests we include in the prompt. Beyond this
/// the gate will be unable to reason about coverage anyway, and the
/// prompt budget gets dominated by digests rather than session content.
const MAX_EXISTING_RULES_IN_PROMPT: usize = 24;

/// How long we'll wait for the agent CLI to return. 90s is generous —
/// Haiku-class models usually finish in 5-15s but cold CLI starts
/// (especially `claude` on Windows) can add ~30s. Anything over 90s
/// means something is wrong; the worker drops the candidate.
const GATE_TIMEOUT: Duration = Duration::from_secs(90);

/// Snapshot of an existing rule passed into the gate so it can decide
/// MERGE vs KEEP. The cloud side already exposes this shape via
/// `get_rules`; the worker just forwards the necessary subset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExistingRule {
    pub rule_id: String,
    /// Title shown in MCP — the gate uses it to decide overlap.
    pub title: String,
    /// Trimmed body so full bodies don't blow the prompt budget once the
    /// user has 50+ rules.
    pub body_snippet: String,
}

#[derive(Debug, Clone)]
pub struct GateArgs<'a> {
    pub session_id: &'a str,
    pub source_repo: &'a str,
    pub pairs: &'a [Pair],
    pub existing_rules: &'a [ExistingRule],
    /// Provider:model identifier recorded on the candidate (e.g.
    /// `"claude-code:gate"`). Not used to pick the agent CLI — that is
    /// derived from `client_name` — but stored on the
    /// [`SessionMinedCandidate`] for audits.
    pub gate_model: &'a str,
    /// Hook adapter client name (`"claude-code"`, `"cursor"`, …). Maps
    /// to an [`AgentKind`] via `AgentKind::from_client_name`; unknown
    /// values default to Claude Code (the most common install).
    pub client_name: &'a str,
    /// Unix-ms timestamp to stamp on the produced candidate when the
    /// gate keeps the session. Passed in (rather than read from the
    /// clock) so tests can pin a deterministic value.
    pub ts_ms: i64,
}

/// What the gate decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateVerdict {
    /// Brand-new rule — enqueue as a fresh candidate.
    Keep { candidate: SessionMinedCandidate },
    /// Extension of an existing rule — enqueue with `MERGE:<id>`
    /// verdict so the cloud-side merge code knows which rule to
    /// extend. `updated_body` is the gate's proposed replacement
    /// body for the merged rule.
    Merge { rule_id: String, updated_body: String },
    /// Nothing reusable in the session — log + drop.
    Skip { reason: String },
}

/// Errors the gate can surface to the worker.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GateError {
    /// Empty input — caller must filter before calling. The gate
    /// refuses to spend a CLI invocation on an empty session.
    #[error("session-mine gate received no conversation pairs")]
    EmptyInput,
    /// Agent CLI dispatch failed (binary missing, timeout, non-zero
    /// exit, etc). `message` is the human-readable reason from
    /// [`crate::agent_exec::GateResult::error_message`].
    #[error("session-mine gate dispatch failed: {message}")]
    Dispatch { message: String },
    /// Agent returned output we couldn't parse as the JSON contract.
    /// `reason` describes the malformation; `raw` is the (possibly
    /// truncated) raw stdout for debugging.
    #[error("session-mine gate parse failed: {reason}")]
    ParseFailure { reason: String, raw: String },
    /// Gate returned KEEP but with field values that violate the
    /// [`SessionMinedCandidate`] invariants (e.g. empty file_patterns).
    /// The worker should drop the candidate; do not retry.
    #[error("session-mine gate produced invalid candidate: {reason}")]
    InvalidCandidate { reason: String },
}

/// Run the gate against `args.pairs` and `args.existing_rules` and
/// return a verdict.
pub async fn run_gate(args: GateArgs<'_>) -> Result<GateVerdict, GateError> {
    if args.pairs.is_empty() {
        return Err(GateError::EmptyInput);
    }

    let agent = AgentKind::from_client_name(args.client_name).unwrap_or(AgentKind::ClaudeCode);
    let prompt = build_prompt(args.pairs, args.existing_rules);

    let result: GateResult = dispatch_gate(agent, &prompt, GATE_TIMEOUT).await;
    if result.errored {
        let message = if result.error_message.is_empty() {
            "agent CLI reported error with no message".to_owned()
        } else {
            result.error_message
        };
        return Err(GateError::Dispatch { message });
    }

    let parsed = parse_gate_json(&result.stdout)?;
    parsed_to_verdict(parsed, &args)
}

/// Parsed JSON shape from the gate. Optional fields are normalized to
/// `None` on `null` or absence so the verdict mapper doesn't have to
/// repeat the dance.
#[derive(Debug, Clone, PartialEq, Eq)]
struct GateJson {
    verdict: String,
    rule_id: Option<String>,
    title: Option<String>,
    body: Option<String>,
    file_patterns: Vec<String>,
    reason: Option<String>,
}

/// Build the gate prompt. Pairs are added newest-last; if the running
/// total would exceed [`PROMPT_MAX_CHARS`], earlier pairs are dropped
/// (we'd rather give the gate a coherent recent prefix than a sliced
/// middle). Existing rule digests are capped at
/// [`MAX_EXISTING_RULES_IN_PROMPT`].
fn build_prompt(pairs: &[Pair], existing_rules: &[ExistingRule]) -> String {
    let mut out = String::with_capacity(PROMPT_MAX_CHARS / 2);
    out.push_str(
        "You are a code-review-rules librarian. Decide whether the following short session \
contains a reusable, transferable rule about how to write code in this team's repo.\n\n",
    );

    out.push_str("EXISTING RULES (do not duplicate):\n");
    if existing_rules.is_empty() {
        out.push_str("- (none yet)\n");
    } else {
        for rule in existing_rules.iter().take(MAX_EXISTING_RULES_IN_PROMPT) {
            let snippet = rule.body_snippet.trim();
            let snippet_short = truncate_chars(snippet, 200);
            let title = truncate_chars(rule.title.trim(), 120);
            out.push_str("- ");
            out.push_str(rule.rule_id.trim());
            out.push_str(": ");
            out.push_str(&title);
            if !snippet_short.is_empty() {
                out.push_str(" — ");
                out.push_str(&snippet_short);
            }
            out.push('\n');
        }
    }
    out.push('\n');

    // Render pairs newest-last but drop oldest first if we overflow.
    let mut rendered_pairs: Vec<String> = pairs
        .iter()
        .map(|p| {
            format!(
                "USER: {}\nASSISTANT: {}\n",
                p.user_prompt.trim(),
                p.assistant_text.trim(),
            )
        })
        .collect();

    let body_budget = PROMPT_MAX_CHARS.saturating_sub(out.chars().count() + 1_200); // 1.2KB for footer
    let mut session_block = String::new();
    while !rendered_pairs.is_empty() {
        let candidate_len: usize = rendered_pairs.iter().map(|s| s.chars().count()).sum();
        if candidate_len <= body_budget {
            break;
        }
        rendered_pairs.remove(0);
    }
    if rendered_pairs.is_empty() {
        // Even the last pair on its own exceeds the budget; include a
        // hard-truncated version of the most recent so the gate has
        // something to reason about.
        if let Some(last) = pairs.last() {
            let truncated = truncate_chars(
                &format!(
                    "USER: {}\nASSISTANT: {}\n",
                    last.user_prompt.trim(),
                    last.assistant_text.trim(),
                ),
                body_budget,
            );
            session_block.push_str(&truncated);
        }
    } else {
        for rendered in &rendered_pairs {
            session_block.push_str(rendered);
        }
    }

    out.push_str("SESSION (prompt + final assistant text only — tool calls stripped):\n");
    out.push_str(&session_block);
    out.push('\n');

    out.push_str(
        "DECISION CRITERIA:\n\
- KEEP if the activity contains a non-obvious, reusable rule (file_pattern + behavior).\n\
- MERGE <existing-id> if it strengthens or refines an existing rule.\n\
- SKIP if it's one-off / generic / obvious / already covered.\n\
\n\
RESPOND WITH STRICT JSON (no prose, no markdown fence):\n\
{ \"verdict\": \"KEEP\" | \"MERGE\" | \"SKIP\",\n\
  \"rule_id\": \"<existing id if MERGE, else null>\",\n\
  \"title\": \"<≤120 chars, only if KEEP/MERGE>\",\n\
  \"body\": \"<≤2000 chars rule body, only if KEEP/MERGE>\",\n\
  \"file_patterns\": [\"<glob1>\", \"...\"],\n\
  \"reason\": \"<short justification, only if SKIP>\" }\n",
    );

    // Final hard cap as a defence-in-depth — if the math above missed
    // a corner case we still respect PROMPT_MAX_CHARS exactly.
    if out.chars().count() > PROMPT_MAX_CHARS {
        out = truncate_chars(&out, PROMPT_MAX_CHARS);
    }
    out
}

/// Parse the agent CLI's stdout, tolerant of:
///
/// * a leading/trailing markdown fence (```` ```json ```` or ```` ``` ````);
/// * prose preceding the JSON (we scan for the first `{`);
/// * `null` values for `rule_id`, `title`, `body`, `reason`;
/// * `file_patterns` absent (treated as empty).
fn parse_gate_json(raw: &str) -> Result<GateJson, GateError> {
    let cleaned = strip_markdown_fence(raw.trim());
    let body = locate_json_object(&cleaned).ok_or_else(|| GateError::ParseFailure {
        reason: "no JSON object found in agent output".to_owned(),
        raw: truncate_chars(raw, 400),
    })?;

    let value: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| GateError::ParseFailure {
            reason: format!("invalid JSON: {e}"),
            raw: truncate_chars(raw, 400),
        })?;

    let verdict = value
        .get("verdict")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| GateError::ParseFailure {
            reason: "missing 'verdict' field".to_owned(),
            raw: truncate_chars(raw, 400),
        })?
        .to_owned();

    let rule_id = optional_string(&value, "rule_id");
    let title = optional_string(&value, "title");
    let body_field = optional_string(&value, "body");
    let reason = optional_string(&value, "reason");

    let file_patterns: Vec<String> = value
        .get("file_patterns")
        .and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::trim))
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();

    Ok(GateJson {
        verdict,
        rule_id,
        title,
        body: body_field,
        file_patterns,
        reason,
    })
}

fn optional_string(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Strip a single surrounding ```` ```json ```` / ```` ``` ```` fence pair if present.
/// Idempotent on un-fenced input.
fn strip_markdown_fence(s: &str) -> String {
    let trimmed = s.trim();
    if let Some(rest) = trimmed.strip_prefix("```") {
        // After the opening backticks there may be a language tag
        // (e.g. `json`). Drop everything up to and including the first
        // newline, then trim the closing fence off the end.
        let after_lang = rest
            .find('\n')
            .map_or("", |idx| &rest[idx + 1..]);
        let stripped = after_lang
            .trim_end()
            .strip_suffix("```")
            .unwrap_or(after_lang)
            .trim_end();
        return stripped.to_owned();
    }
    trimmed.to_owned()
}

/// Find the substring starting at the first `{` and ending at the
/// matching `}` (naively counted). Returns `None` if the braces don't
/// balance. Good enough for one-shot JSON object responses.
fn locate_json_object(s: &str) -> Option<String> {
    let trimmed = s.trim();
    let start = trimmed.find('{')?;
    let bytes = trimmed.as_bytes();
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escape = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(trimmed[start..=i].to_owned());
                }
            }
            _ => {}
        }
    }
    None
}

fn parsed_to_verdict(parsed: GateJson, args: &GateArgs<'_>) -> Result<GateVerdict, GateError> {
    let verdict_uc = parsed.verdict.to_ascii_uppercase();
    match verdict_uc.as_str() {
        "KEEP" => {
            let title = parsed
                .title
                .ok_or_else(|| GateError::InvalidCandidate {
                    reason: "KEEP verdict missing title".to_owned(),
                })?;
            let body = parsed
                .body
                .ok_or_else(|| GateError::InvalidCandidate {
                    reason: "KEEP verdict missing body".to_owned(),
                })?;
            if parsed.file_patterns.is_empty() {
                return Err(GateError::InvalidCandidate {
                    reason: "KEEP verdict missing file_patterns".to_owned(),
                });
            }
            let candidate = SessionMinedCandidate::try_new(SessionMinedCandidateArgs {
                session_id: args.session_id.to_owned(),
                ts_ms: args.ts_ms,
                source_repo: args.source_repo.to_owned(),
                title,
                body,
                file_patterns: parsed.file_patterns,
                gate_model: args.gate_model.to_owned(),
                gate_verdict: "KEEP".to_owned(),
            })
            .map_err(|e| GateError::InvalidCandidate {
                reason: e.to_string(),
            })?;
            Ok(GateVerdict::Keep { candidate })
        }
        "MERGE" => {
            let rule_id = parsed
                .rule_id
                .ok_or_else(|| GateError::InvalidCandidate {
                    reason: "MERGE verdict missing rule_id".to_owned(),
                })?;
            let updated_body =
                parsed.body.ok_or_else(|| GateError::InvalidCandidate {
                    reason: "MERGE verdict missing body".to_owned(),
                })?;
            Ok(GateVerdict::Merge {
                rule_id,
                updated_body,
            })
        }
        "SKIP" => Ok(GateVerdict::Skip {
            reason: parsed.reason.unwrap_or_else(|| "no reason given".to_owned()),
        }),
        other => Err(GateError::ParseFailure {
            reason: format!("unknown verdict '{other}'"),
            raw: String::new(),
        }),
    }
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
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

    fn pair(user: &str, assistant: &str) -> Pair {
        Pair {
            user_prompt: user.to_owned(),
            assistant_text: assistant.to_owned(),
        }
    }

    fn args<'a>(
        pairs: &'a [Pair],
        existing: &'a [ExistingRule],
    ) -> GateArgs<'a> {
        GateArgs {
            session_id: "sess_test",
            source_repo: "owner/repo",
            pairs,
            existing_rules: existing,
            gate_model: "claude-code:gate",
            client_name: "claude-code",
            ts_ms: 1_714_000_000_000,
        }
    }

    // Prompt assembly
    #[test]
    fn prompt_includes_existing_rules_section_with_ids_and_titles() {
        let rules = vec![
            ExistingRule {
                rule_id: "rule-1".to_owned(),
                title: "Prefer typed deserialization".to_owned(),
                body_snippet: "Use serde structs instead of Value::as_str.".to_owned(),
            },
            ExistingRule {
                rule_id: "rule-2".to_owned(),
                title: "Hard-deny dbg!".to_owned(),
                body_snippet: "Workspace forbids debug macros in committed code."
                    .to_owned(),
            },
        ];
        let pairs = vec![pair("hi", "hello")];
        let prompt = build_prompt(&pairs, &rules);

        assert!(prompt.contains("EXISTING RULES"), "section header present");
        assert!(prompt.contains("rule-1: Prefer typed deserialization"));
        assert!(prompt.contains("rule-2: Hard-deny dbg!"));
        assert!(prompt.contains("SESSION ("));
        assert!(prompt.contains("DECISION CRITERIA"));
        assert!(prompt.contains("STRICT JSON"));
    }

    #[test]
    fn prompt_uses_none_yet_placeholder_when_no_existing_rules() {
        let pairs = vec![pair("u", "a")];
        let prompt = build_prompt(&pairs, &[]);
        assert!(prompt.contains("- (none yet)"), "explicit no-rules marker");
    }

    #[test]
    fn prompt_renders_pairs_in_order_with_user_assistant_labels() {
        let pairs = vec![pair("first q", "first a"), pair("second q", "second a")];
        let prompt = build_prompt(&pairs, &[]);
        let first_idx = prompt.find("first q").expect("first q present");
        let second_idx = prompt.find("second q").expect("second q present");
        assert!(first_idx < second_idx, "pairs in chronological order");
        assert!(prompt.contains("USER: first q"));
        assert!(prompt.contains("ASSISTANT: first a"));
    }

    #[test]
    fn prompt_drops_oldest_pairs_when_over_budget() {
        // Build many pairs that would obviously exceed the 30 KB cap if
        // all were included. The oldest must be dropped, the newest
        // kept — the "coherent recent prefix" contract.
        let mut pairs: Vec<Pair> = Vec::new();
        for i in 0..400 {
            let body = "x".repeat(200);
            pairs.push(pair(&format!("user-{i}"), &body));
        }
        let prompt = build_prompt(&pairs, &[]);
        assert!(prompt.chars().count() <= PROMPT_MAX_CHARS);
        // Last pair must still be in the prompt.
        assert!(prompt.contains("user-399"));
        // First pair must have been dropped.
        assert!(!prompt.contains("user-0\n"));
    }

    #[test]
    fn prompt_caps_existing_rules_at_max_for_budget() {
        let mut rules = Vec::new();
        for i in 0..(MAX_EXISTING_RULES_IN_PROMPT + 10) {
            rules.push(ExistingRule {
                rule_id: format!("rule-{i}"),
                title: format!("title-{i}"),
                body_snippet: format!("snippet-{i}"),
            });
        }
        let pairs = vec![pair("u", "a")];
        let prompt = build_prompt(&pairs, &rules);
        // First MAX in
        assert!(prompt.contains("rule-0:"));
        assert!(prompt.contains(&format!(
            "rule-{}:",
            MAX_EXISTING_RULES_IN_PROMPT - 1
        )));
        // Beyond MAX out
        assert!(!prompt.contains(&format!("rule-{MAX_EXISTING_RULES_IN_PROMPT}:")));
    }

    // JSON parsing
    #[test]
    fn parse_keep_minimal_shape() {
        let raw = r#"{"verdict":"KEEP","title":"Always validate","body":"Validate before enqueue.","file_patterns":["src/**/*.rs"]}"#;
        let parsed = parse_gate_json(raw).expect("parses");
        assert_eq!(parsed.verdict, "KEEP");
        assert_eq!(parsed.title.as_deref(), Some("Always validate"));
        assert_eq!(parsed.body.as_deref(), Some("Validate before enqueue."));
        assert_eq!(parsed.file_patterns, vec!["src/**/*.rs"]);
        assert!(parsed.rule_id.is_none());
    }

    #[test]
    fn parse_merge_shape_carries_rule_id() {
        let raw = r#"{"verdict":"MERGE","rule_id":"rule-7","title":"Refine X","body":"Updated body","file_patterns":[]}"#;
        let parsed = parse_gate_json(raw).expect("parses");
        assert_eq!(parsed.verdict, "MERGE");
        assert_eq!(parsed.rule_id.as_deref(), Some("rule-7"));
        assert_eq!(parsed.body.as_deref(), Some("Updated body"));
    }

    #[test]
    fn parse_skip_shape_carries_reason() {
        let raw = r#"{"verdict":"SKIP","reason":"one-off bug fix"}"#;
        let parsed = parse_gate_json(raw).expect("parses");
        assert_eq!(parsed.verdict, "SKIP");
        assert_eq!(parsed.reason.as_deref(), Some("one-off bug fix"));
    }

    #[test]
    fn parse_tolerates_markdown_json_fence() {
        let raw = "```json\n{\"verdict\":\"SKIP\",\"reason\":\"covered\"}\n```";
        let parsed = parse_gate_json(raw).expect("parses through fence");
        assert_eq!(parsed.verdict, "SKIP");
        assert_eq!(parsed.reason.as_deref(), Some("covered"));
    }

    #[test]
    fn parse_tolerates_plain_markdown_fence() {
        let raw = "```\n{\"verdict\":\"SKIP\",\"reason\":\"x\"}\n```";
        let parsed = parse_gate_json(raw).expect("parses");
        assert_eq!(parsed.verdict, "SKIP");
    }

    #[test]
    fn parse_tolerates_prose_before_json() {
        let raw =
            "Sure, here's my answer:\n{\"verdict\":\"SKIP\",\"reason\":\"too narrow\"}\nThanks!";
        let parsed = parse_gate_json(raw).expect("parses through prose");
        assert_eq!(parsed.verdict, "SKIP");
        assert_eq!(parsed.reason.as_deref(), Some("too narrow"));
    }

    #[test]
    fn parse_treats_null_optional_fields_as_none() {
        let raw =
            r#"{"verdict":"KEEP","rule_id":null,"title":"T","body":"B","file_patterns":["a.rs"]}"#;
        let parsed = parse_gate_json(raw).expect("parses");
        assert!(parsed.rule_id.is_none());
        assert_eq!(parsed.title.as_deref(), Some("T"));
    }

    #[test]
    fn parse_rejects_malformed_payload_with_clean_error() {
        let raw = "this is not JSON at all";
        let err = parse_gate_json(raw).unwrap_err();
        match err {
            GateError::ParseFailure { reason, .. } => {
                assert!(
                    reason.contains("no JSON object"),
                    "expected 'no JSON object' diagnostic, got: {reason}"
                );
            }
            other => panic!("expected ParseFailure, got {other:?}"),
        }
    }

    #[test]
    fn parse_rejects_missing_verdict_field() {
        let raw = r#"{"title":"T","body":"B"}"#;
        let err = parse_gate_json(raw).unwrap_err();
        match err {
            GateError::ParseFailure { reason, .. } => {
                assert!(reason.contains("verdict"), "diagnostic mentions verdict: {reason}");
            }
            other => panic!("expected ParseFailure, got {other:?}"),
        }
    }

    #[test]
    fn parse_rejects_invalid_json_body() {
        // A `{` is found but the object is unterminated → no balanced
        // closing brace → "no JSON object". Acceptable failure mode;
        // pin it so a future fix doesn't accidentally turn this into a
        // partial parse.
        let raw = r#"{"verdict":"KEEP""#;
        let err = parse_gate_json(raw).unwrap_err();
        assert!(matches!(err, GateError::ParseFailure { .. }));
    }

    // Verdict mapping
    #[test]
    fn keep_verdict_builds_session_mined_candidate() {
        let parsed = GateJson {
            verdict: "KEEP".to_owned(),
            rule_id: None,
            title: Some("Validate before enqueue".to_owned()),
            body: Some("Session-mined candidates must validate before reaching the outbox.".to_owned()),
            file_patterns: vec!["crates/**/*.rs".to_owned()],
            reason: None,
        };
        let pairs = vec![pair("u", "a")];
        let a = args(&pairs, &[]);
        let verdict = parsed_to_verdict(parsed, &a).expect("verdict");
        match verdict {
            GateVerdict::Keep { candidate } => {
                assert_eq!(candidate.source_repo, "owner/repo");
                assert_eq!(candidate.session_id, "sess_test");
                assert_eq!(candidate.gate_verdict, "KEEP");
                assert_eq!(candidate.gate_model, "claude-code:gate");
                assert!(candidate.requires_human_approval);
                assert_eq!(candidate.file_patterns, vec!["crates/**/*.rs"]);
            }
            other => panic!("expected Keep, got {other:?}"),
        }
    }

    #[test]
    fn merge_verdict_carries_rule_id_and_updated_body() {
        let parsed = GateJson {
            verdict: "MERGE".to_owned(),
            rule_id: Some("rule-42".to_owned()),
            title: Some("Extended".to_owned()),
            body: Some("Refined body".to_owned()),
            file_patterns: vec![],
            reason: None,
        };
        let pairs = vec![pair("u", "a")];
        let a = args(&pairs, &[]);
        let verdict = parsed_to_verdict(parsed, &a).expect("verdict");
        assert_eq!(
            verdict,
            GateVerdict::Merge {
                rule_id: "rule-42".to_owned(),
                updated_body: "Refined body".to_owned(),
            }
        );
    }

    #[test]
    fn skip_verdict_falls_back_to_default_reason() {
        let parsed = GateJson {
            verdict: "SKIP".to_owned(),
            rule_id: None,
            title: None,
            body: None,
            file_patterns: vec![],
            reason: None,
        };
        let pairs = vec![pair("u", "a")];
        let a = args(&pairs, &[]);
        let verdict = parsed_to_verdict(parsed, &a).expect("verdict");
        assert_eq!(
            verdict,
            GateVerdict::Skip {
                reason: "no reason given".to_owned(),
            }
        );
    }

    #[test]
    fn keep_missing_title_or_body_is_invalid_candidate() {
        // The gate occasionally returns KEEP with placeholder nulls.
        // Map to InvalidCandidate (worker drops, no retry) rather than
        // poisoning the outbox with a half-formed payload.
        let parsed = GateJson {
            verdict: "KEEP".to_owned(),
            rule_id: None,
            title: None,
            body: Some("body".to_owned()),
            file_patterns: vec!["a.rs".to_owned()],
            reason: None,
        };
        let pairs = vec![pair("u", "a")];
        let a = args(&pairs, &[]);
        let err = parsed_to_verdict(parsed, &a).unwrap_err();
        assert!(matches!(err, GateError::InvalidCandidate { .. }));
    }

    #[test]
    fn keep_with_empty_file_patterns_is_invalid_candidate() {
        let parsed = GateJson {
            verdict: "KEEP".to_owned(),
            rule_id: None,
            title: Some("T".to_owned()),
            body: Some("B".to_owned()),
            file_patterns: vec![],
            reason: None,
        };
        let pairs = vec![pair("u", "a")];
        let a = args(&pairs, &[]);
        let err = parsed_to_verdict(parsed, &a).unwrap_err();
        match err {
            GateError::InvalidCandidate { reason } => {
                assert!(reason.contains("file_patterns"));
            }
            other => panic!("expected InvalidCandidate, got {other:?}"),
        }
    }

    #[test]
    fn unknown_verdict_string_is_parse_failure() {
        let parsed = GateJson {
            verdict: "REJECT".to_owned(),
            rule_id: None,
            title: None,
            body: None,
            file_patterns: vec![],
            reason: None,
        };
        let pairs = vec![pair("u", "a")];
        let a = args(&pairs, &[]);
        let err = parsed_to_verdict(parsed, &a).unwrap_err();
        assert!(matches!(err, GateError::ParseFailure { .. }));
    }

    // Public surface
    #[tokio::test]
    async fn run_gate_rejects_empty_input_without_spawning() {
        // No pairs → no CLI call. The worker upstream already filters,
        // but pin the contract so a future caller can't waste a CLI
        // invocation on an empty session.
        let a = args(&[], &[]);
        let err = run_gate(a).await.unwrap_err();
        assert_eq!(err, GateError::EmptyInput);
    }

    #[test]
    fn existing_rule_shape_clones_and_compares_cheaply() {
        let r = ExistingRule {
            rule_id: "rule-1".to_owned(),
            title: "Prefer typed parse".to_owned(),
            body_snippet: "..".to_owned(),
        };
        assert_eq!(r.clone(), r);
    }
}
