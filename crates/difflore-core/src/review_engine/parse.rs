use super::{ReviewIssueRecord, default_confidence};
use crate::domain::models::FileIntent;

/// Numeric rank of a severity string. Higher = more severe.
/// Unknown severities fall below `info`.
pub(super) fn severity_rank(s: &str) -> u8 {
    match s {
        "error" => 3,
        "warning" => 2,
        "info" => 1,
        _ => 0,
    }
}

/// Parse a JSON issue array from AI response text, trying a direct parse, a
/// fenced code block, then a first-`[`-to-last-`]` scan.
pub(super) fn parse_issues(text: &str) -> Vec<ReviewIssueRecord> {
    if let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(text.trim()) {
        return map_issues(&arr);
    }

    if let Some(start) = text.find("```") {
        let after = &text[start + 3..];
        let content_start = after.find('\n').map_or(0, |i| i + 1);
        if let Some(end) = after[content_start..].find("```") {
            let block = &after[content_start..content_start + end];
            if let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(block.trim()) {
                return map_issues(&arr);
            }
        }
    }

    if let (Some(start), Some(end)) = (text.find('['), text.rfind(']'))
        && end > start
        && let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(&text[start..=end])
    {
        return map_issues(&arr);
    }

    vec![]
}

pub(super) fn map_issues(arr: &[serde_json::Value]) -> Vec<ReviewIssueRecord> {
    arr.iter()
        .filter_map(|item| {
            let obj = item.as_object()?;
            Some(ReviewIssueRecord {
                severity: obj
                    .get("severity")
                    .and_then(|v| v.as_str())
                    .unwrap_or("info")
                    .to_owned(),
                rule: obj
                    .get("rule")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_owned(),
                rule_id: obj.get("ruleId").and_then(|v| v.as_str()).map(String::from),
                message: obj
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned(),
                file: obj.get("file").and_then(|v| v.as_str()).map(String::from),
                line: obj
                    .get("line")
                    .and_then(serde_json::Value::as_i64)
                    .and_then(|n| i32::try_from(n).ok()),
                suggestion: obj
                    .get("suggestion")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                source_badge: None,
                perspectives: Vec::new(),
                confidence: default_confidence(),
            })
        })
        .collect()
}

/// Extract the optional verbatim source snippet attached to each issue, in
/// the same order `parse_issues` returns them. Accepts any of `existingCode` /
/// `existing_code` / `code` / `snippet` (models differ). Returns one
/// `Option<String>` per issue.
///
/// Kept separate from `parse_issues` so the cloud-facing `ReviewIssueRecord`
/// stays unchanged: the snippet is consumed locally by the hunk resolver only.
pub(super) fn extract_issue_snippets(text: &str) -> Vec<Option<String>> {
    let arr = parse_issue_array(text);
    arr.iter()
        .filter_map(|item| item.as_object())
        .map(|obj| {
            ["existingCode", "existing_code", "code", "snippet"]
                .iter()
                .find_map(|key| obj.get(*key))
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToOwned::to_owned)
        })
        .collect()
}

/// Shared three-tier JSON-array extraction (direct → ```code block``` →
/// first `[` to last `]`) used by `parse_issues`, `extract_issue_snippets`,
/// `parse_verify_response`, and the pipeline judge so they always see the
/// same elements in the same order. A malformed/unparseable fence falls
/// through to the bracket scan rather than short-circuiting.
pub(super) fn parse_issue_array(text: &str) -> Vec<serde_json::Value> {
    if let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(text.trim()) {
        return arr;
    }
    if let Some(start) = text.find("```") {
        let after = &text[start + 3..];
        let content_start = after.find('\n').map_or(0, |i| i + 1);
        if let Some(end) = after[content_start..].find("```") {
            let block = &after[content_start..content_start + end];
            if let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(block.trim()) {
                return arr;
            }
        }
    }
    if let (Some(start), Some(end)) = (text.find('['), text.rfind(']'))
        && end > start
        && let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(&text[start..=end])
    {
        return arr;
    }
    Vec::new()
}

/// Shared three-tier JSON-object extraction (direct → ```code block``` →
/// first `{` to last `}`), the object-shaped sibling of `parse_issue_array`.
/// As there, a malformed/unparseable fence falls through to the brace scan
/// rather than short-circuiting. A direct parse that yields valid-but-non-object
/// JSON commits to that tier (returns `None`), matching `parse_issue_array`'s
/// "a successful direct parse wins" behaviour. Returns `None` when no tier
/// yields an object.
fn parse_value_object(text: &str) -> Option<serde_json::Value> {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(text.trim()) {
        return v.is_object().then_some(v);
    }
    if let Some(start) = text.find("```") {
        let after = &text[start + 3..];
        let content_start = after.find('\n').map_or(0, |i| i + 1);
        if let Some(end) = after[content_start..].find("```") {
            let block = &after[content_start..content_start + end];
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(block.trim())
                && v.is_object()
            {
                return Some(v);
            }
        }
    }
    if let (Some(start), Some(end)) = (text.find('{'), text.rfind('}'))
        && end > start
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(&text[start..=end])
        && v.is_object()
    {
        return Some(v);
    }
    None
}

/// Parse a verify-pass response into a map of `id -> (confidence, keep)`.
/// Best-effort: unrecognized shapes produce `None`, and callers treat
/// that as "keep everything unchanged".
pub(super) fn parse_verify_response(
    text: &str,
) -> Option<std::collections::HashMap<usize, (f32, bool)>> {
    // Same three-tier extraction as `parse_issues` (direct → fenced block →
    // bracket scan), and — like `parse_issue_array` — a malformed/unparseable
    // fence falls through to the bracket scan rather than short-circuiting.
    let arr = parse_issue_array(text);
    if arr.is_empty() {
        return None;
    }

    let mut out = std::collections::HashMap::new();
    for item in arr {
        let Some(obj) = item.as_object() else {
            continue;
        };
        let id = obj
            .get("id")
            .and_then(serde_json::Value::as_u64)
            .map(|n| n as usize);
        let Some(id) = id else {
            continue;
        };
        let confidence = obj
            .get("confidence")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(1.0) as f32;
        let verdict = obj
            .get("verdict")
            .and_then(|v| v.as_str())
            .unwrap_or("keep");
        let keep = verdict != "drop";
        out.insert(id, (confidence.clamp(0.0, 1.0), keep));
    }
    Some(out)
}

/// Parse a review-summary response into `(one_line, walkthrough)`.
/// Best-effort — returns `None` on any structural mismatch.
pub(super) fn parse_summary_response(text: &str) -> Option<(String, Vec<FileIntent>)> {
    // Extract the JSON object (direct → code block → brace scan); like
    // `parse_issue_array`, a malformed/unparseable fence falls through to the
    // brace scan rather than short-circuiting.
    let obj = parse_value_object(text)?;

    let map = obj.as_object()?;
    let one_line = map
        .get("oneLineSummary")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned();
    let walkthrough = map
        .get("walkthroughByFile")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|item| {
                    let o = item.as_object()?;
                    Some(FileIntent {
                        file: o.get("file").and_then(|v| v.as_str())?.to_owned(),
                        intent: o
                            .get("intent")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_owned(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Some((one_line, walkthrough))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippets_align_positionally_with_parsed_issues() {
        let json = r#"[
            {"severity":"error","rule":"a","message":"m1","file":"x.rs","existingCode":"let a = 1;"},
            {"severity":"info","rule":"b","message":"m2","file":"y.rs"},
            {"severity":"warning","rule":"c","message":"m3","file":"z.rs","code":"foo()"}
        ]"#;
        let issues = parse_issues(json);
        let snippets = extract_issue_snippets(json);
        assert_eq!(issues.len(), 3);
        assert_eq!(snippets.len(), 3);
        assert_eq!(snippets[0].as_deref(), Some("let a = 1;"));
        assert_eq!(snippets[1], None);
        // alternate key `code` is also recognised.
        assert_eq!(snippets[2].as_deref(), Some("foo()"));
    }

    #[test]
    fn snippets_extracted_from_fenced_code_block() {
        let text = "here you go:\n```json\n[{\"rule\":\"r\",\"message\":\"m\",\"snippet\":\"x.y()\"}]\n```\n";
        let snippets = extract_issue_snippets(text);
        assert_eq!(snippets.len(), 1);
        assert_eq!(snippets[0].as_deref(), Some("x.y()"));
    }

    #[test]
    fn snippets_empty_when_unparseable() {
        assert!(extract_issue_snippets("not json at all").is_empty());
    }
}
