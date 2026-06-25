use super::{ReviewIssueRecord, ReviewPerspective};
use crate::context::assembler::PastVerdictSection;
use crate::context::types::PastVerdict;

/// A single team rule in the canonical form used for the cacheable team-rules
/// digest. Minimal and deterministic so the digest is hash-stable across
/// reviews and an Anthropic `cache_control` hint can reuse the prefix.
#[derive(Debug, Clone)]
pub struct TeamRuleDigest {
    pub id: String,
    pub content: String,
}

/// System prompt split into a cacheable stable prefix and a per-review dynamic
/// suffix. The stable prefix is hash-stable across reviews from the same team so
/// providers that support prompt caching (e.g. Anthropic `cache_control:
/// ephemeral`) can skip re-tokenising it.
///
/// Concatenating `stable_prefix + dynamic_suffix` yields a flat system prompt;
/// `build_system_prompt` relies on this for byte-identical compatibility.
#[derive(Debug, Clone)]
pub struct SegmentedPrompt {
    /// base instructions → perspective addendum → sorted team rules → repo
    /// context facts.
    pub stable_prefix: String,
    /// past verdicts → current diff → user instructions.
    pub dynamic_suffix: String,
}

/// Base instructions for the review system prompt, shared verbatim by the
/// compatibility shim and `build_segmented_prompt`.
const REVIEW_BASE_INSTRUCTIONS: &str = r#"You are a code review assistant. Review the provided diff against the given rules and return issues as a JSON array.

Each issue must be a JSON object with these fields:
- severity: "error" | "warning" | "info"
- rule: the rule name that was violated
- ruleId: stable rule ID when the matched rule provides one (optional, string)
- message: clear description of the issue
- file: repo-relative path of the affected file as it appears in the diff header (e.g. "src/app.ts" — strip the "a/" or "b/" prefix; REQUIRED for downstream patch generation)
- line: line number in the diff (optional, number)
- existingCode: copy the EXACT affected source line(s) verbatim from the diff, without the leading +/- marker (optional, string; helps pinpoint the precise location)
- suggestion: how to fix it (optional, string)

Matched rules are the user's review memory and should be treated as authoritative review criteria. If the diff directly matches a rule's bad pattern, contradicts a rule's recommendation, or removes code a rule says is required, report that issue even when the change is small or the code still compiles. Do not return [] when a matched rule clearly applies to the diff.

Return ONLY a JSON array. No markdown, no explanation, no code blocks. Just the raw JSON array.
If no issues are found, return an empty array: []"#;

/// Render the team-rules digest section. Rules are sorted by `id` so the output
/// is deterministic across runs, keeping the stable prefix hash-stable.
///
/// Returns an empty string when `rules` is empty.
pub(super) fn render_team_rules_digest(rules: &[TeamRuleDigest]) -> String {
    if rules.is_empty() {
        return String::new();
    }
    let mut sorted: Vec<&TeamRuleDigest> = rules.iter().collect();
    sorted.sort_by(|a, b| a.id.cmp(&b.id));

    let mut s = String::new();
    s.push_str("\n\n## Team Rules Digest\n");
    for r in sorted {
        s.push_str("\n- id: ");
        s.push_str(&r.id);
        s.push('\n');
        s.push_str("  content: ");
        s.push_str(&r.content);
        s.push('\n');
    }
    s
}

/// Render the optional repo context facts section. Empty input is treated the
/// same as `None` to preserve byte-identical reassembly.
pub(super) fn render_repo_context_section(repo_context_facts: Option<&str>) -> String {
    match repo_context_facts {
        Some(facts) if !facts.is_empty() => {
            let mut s = String::new();
            s.push_str("\n\n## Repo Context\n");
            s.push_str(facts);
            s
        }
        _ => String::new(),
    }
}

/// Render the per-review dynamic suffix. Empty inputs produce an empty string.
///
/// `past_verdicts` is review-memory recall placed at the front of the segment
/// so the LLM reads prior verdicts before the current diff; omitted when `None`
/// or empty.
pub(super) fn render_dynamic_suffix(
    diff: &str,
    user_instructions: &str,
    past_verdicts: Option<&[PastVerdict]>,
) -> String {
    let has_diff = !diff.is_empty();
    let has_instructions = !user_instructions.is_empty();
    let verdicts_rendered = match past_verdicts {
        Some(v) if !v.is_empty() => PastVerdictSection::new(v.to_vec()).render(),
        _ => String::new(),
    };
    let has_verdicts = !verdicts_rendered.is_empty();

    if !has_diff && !has_instructions && !has_verdicts {
        return String::new();
    }

    let mut s = String::new();
    if has_verdicts {
        s.push_str("\n\n");
        s.push_str(verdicts_rendered.trim_end());
    }
    if has_diff {
        s.push_str("\n\n## Current Diff\n```diff\n");
        s.push_str(diff);
        s.push_str("\n```");
    }
    if has_instructions {
        s.push_str("\n\n## User Instructions\n");
        s.push_str(user_instructions);
    }
    s
}

/// Build a `SegmentedPrompt` split into a hash-stable cacheable prefix and a
/// per-review dynamic suffix. See [`SegmentedPrompt`] for layout.
pub fn build_segmented_prompt(
    perspective: Option<ReviewPerspective>,
    team_rules: &[TeamRuleDigest],
    diff: &str,
    user_instructions: &str,
    repo_context_facts: Option<&str>,
    past_verdicts: Option<&[PastVerdict]>,
) -> SegmentedPrompt {
    let mut stable_prefix = String::with_capacity(REVIEW_BASE_INSTRUCTIONS.len() + 1024);
    stable_prefix.push_str(REVIEW_BASE_INSTRUCTIONS);
    if let Some(p) = perspective {
        stable_prefix.push_str(p.system_prompt_addendum());
    }
    stable_prefix.push_str(&render_team_rules_digest(team_rules));
    stable_prefix.push_str(&render_repo_context_section(repo_context_facts));

    let dynamic_suffix = render_dynamic_suffix(diff, user_instructions, past_verdicts);

    SegmentedPrompt {
        stable_prefix,
        dynamic_suffix,
    }
}

/// Build the system prompt for review check.
///
/// When `perspective` is `Some`, the perspective-specific addendum is
/// appended to the base prompt. When `None`, the returned string is
/// byte-identical to the flat single-pass prompt.
///
/// Delegates to `build_segmented_prompt` with empty extras and reassembles the
/// two halves.
#[cfg(test)]
pub(super) fn build_system_prompt(perspective: Option<ReviewPerspective>) -> String {
    let seg = build_segmented_prompt(perspective, &[], "", "", None, None);
    format!("{}{}", seg.stable_prefix, seg.dynamic_suffix)
}

/// Build the user prompt with diff + matched rules
pub(super) fn build_user_prompt(
    diff: &str,
    rules_text: Option<&str>,
    file_path: Option<&str>,
) -> String {
    let mut prompt = String::new();

    if let Some(rules) = rules_text {
        prompt.push_str("## Review Rules\n\n");
        prompt.push_str("Each matched rule may include a `Rule ID:` line. When you cite a matched rule, copy that exact value into `ruleId`.\n\n");
        prompt.push_str("Use these rules as concrete checks against the diff. Prefer one precise issue over [] when a rule directly applies.\n\n");
        prompt.push_str(rules);
        prompt.push_str("\n\n");
    }

    if let Some(path) = file_path {
        prompt.push_str(&format!("## File: {path}\n\n"));
    }

    prompt.push_str("## Diff to Review\n\n```diff\n");
    prompt.push_str(diff);
    prompt.push_str("\n```\n");

    prompt
}

/// System prompt used by the self-check verification pass. Short and
/// strict so the cheap model doesn't hallucinate new issues.
pub(super) const VERIFY_SYSTEM_PROMPT: &str = r#"You are a strict code-review verifier. Given the diff and a list of candidate issues, for EACH issue decide whether it is a true positive.

Return ONLY a JSON array. Each element must be an object:
{"id": <index>, "confidence": <float 0..1>, "verdict": "keep"|"drop", "reason": "<short>"}

Be strict — drop obvious false positives. Keep an issue when the changed line directly matches the cited rule's bad pattern or contradicts the cited rule's recommendation, even if surrounding pre-existing code has similar style. Do NOT invent new issues.
Return the raw JSON array only, no markdown, no explanation."#;

/// Byte-bounded slice that backs up to the nearest UTF-8 char boundary, so a
/// multi-byte sequence straddling `max_bytes` can't trigger a slice panic.
/// The diff is untrusted file content, so it routinely contains multi-byte
/// chars (comments, identifiers, CJK, emoji) near any fixed byte budget.
pub(crate) fn clamp_str_to_char_boundary(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Build the verification user-prompt: the diff (trimmed) + the
/// candidate issues enumerated with stable `id` indices so the model's
/// response can be matched back deterministically.
pub(super) fn build_verify_user_prompt(diff: &str, issues: &[ReviewIssueRecord]) -> String {
    const DIFF_LIMIT: usize = 8_000;
    let trimmed = clamp_str_to_char_boundary(diff, DIFF_LIMIT);

    let mut s = String::new();
    s.push_str("## Diff\n```diff\n");
    s.push_str(trimmed);
    s.push_str("\n```\n\n## Candidate issues\n");
    for (i, issue) in issues.iter().enumerate() {
        s.push_str(&format!(
            "- id: {}\n  severity: {}\n  rule: {}\n  file: {}\n  line: {}\n  message: {}\n  suggestion: {}\n",
            i,
            issue.severity,
            issue.rule,
            issue.file.as_deref().unwrap_or(""),
            issue.line.map(|n| n.to_string()).unwrap_or_default(),
            issue.message,
            issue.suggestion.as_deref().unwrap_or(""),
        ));
    }
    s
}

pub(super) const SUMMARY_SYSTEM_PROMPT: &str = r#"You are a code-review summarizer. Given a diff, produce a concise one-line PR summary plus per-file intent descriptions.

Return ONLY a JSON object with this exact shape:
{
  "oneLineSummary": "<one sentence>",
  "walkthroughByFile": [
    {"file": "<path>", "intent": "<one sentence describing what this file's change does>"}
  ]
}
No markdown, no code blocks, no extra commentary."#;

pub(super) fn build_summary_user_prompt(diff: &str, files: &[String]) -> String {
    const DIFF_LIMIT: usize = 8_000;
    let trimmed = clamp_str_to_char_boundary(diff, DIFF_LIMIT);
    let mut s = String::new();
    s.push_str("## Files touched\n");
    for f in files {
        s.push_str("- ");
        s.push_str(f);
        s.push('\n');
    }
    s.push_str("\n## Diff\n```diff\n");
    s.push_str(trimmed);
    s.push_str("\n```\n");
    s
}

#[cfg(test)]
mod prompt_truncation_tests {
    use super::*;

    #[test]
    fn clamp_returns_whole_string_when_under_limit() {
        assert_eq!(clamp_str_to_char_boundary("hello", 8_000), "hello");
    }

    #[test]
    fn clamp_never_splits_a_multibyte_char() {
        // 'é' is 2 bytes. A string of them means most byte offsets land
        // mid-codepoint; clamping must back up to a boundary, never panic.
        let s = "é".repeat(5_000); // 10_000 bytes
        for max in [1, 2, 3, 7_999, 8_000, 8_001] {
            let out = clamp_str_to_char_boundary(&s, max);
            assert!(out.len() <= max);
            assert!(s.starts_with(out)); // valid UTF-8 prefix, no panic
        }
    }

    #[test]
    fn verify_and_summary_prompts_do_not_panic_on_multibyte_diff() {
        // Regression: these used `&diff[..8000]` (byte slice) which panicked
        // when byte 8000 fell inside a multi-byte char — routine in real code.
        let diff = "// 注释 ".repeat(2_000); // multibyte, well over 8 KB
        let _ = build_verify_user_prompt(&diff, &[]);
        let _ = build_summary_user_prompt(&diff, &["src/lib.rs".to_owned()]);
    }
}
