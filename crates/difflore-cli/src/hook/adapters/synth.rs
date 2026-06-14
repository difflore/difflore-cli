//! Shared diff-synthesis helpers for hook adapters.
//!
//! Turns an IDE tool payload into a tiny "diff-like" string the rule
//! retriever can grep against. Input wire shapes vary per IDE, but the
//! output is always rows of `-old` / `+new` / `+content` / `$ command`.

use serde_json::Value;

/// Render an Edit-style hunk: `old` lines prefixed `-`, `new` lines
/// prefixed `+`. No `@@` header — retrieval is text-based. Empty inputs
/// yield an empty string so callers can `is_empty()`-check.
pub(crate) fn diff_old_new(old: &str, new: &str) -> String {
    let mut out = String::new();
    push_prefixed(&mut out, '-', old);
    push_prefixed(&mut out, '+', new);
    out
}

/// Append a hunk to an existing buffer, for concatenating an `edits[]`
/// array into one diff blob.
pub(crate) fn append_old_new(out: &mut String, old: &str, new: &str) {
    push_prefixed(out, '-', old);
    push_prefixed(out, '+', new);
}

/// Render a Write-style synthetic diff (every line prefixed `+`), used
/// when there is no prior content to compare against.
pub(crate) fn diff_content(content: &str) -> String {
    let mut out = String::new();
    push_prefixed(&mut out, '+', content);
    out
}

/// Render a shell-execution synthetic diff: `$ <cmd>` then each `out`
/// line prefixed `+`. Returns `None` when both inputs are blank.
pub(crate) fn diff_shell(command: Option<&str>, output: Option<&str>) -> Option<String> {
    let cmd = command.unwrap_or("").trim();
    let out_text = output.unwrap_or("").trim();
    if cmd.is_empty() && out_text.is_empty() {
        return None;
    }
    let mut s = String::new();
    if !cmd.is_empty() {
        s.push_str("$ ");
        s.push_str(cmd);
        s.push('\n');
    }
    push_prefixed(&mut s, '+', out_text);
    Some(s)
}

/// Pull `(old_text, new_text)` from a tool-input JSON value.
///
/// Handles three shapes: `MultiEdit` (`edits[]`, folded blank-line-
/// separated into one pair), flat `{ old_string, new_string }`, and
/// Write `{ content }` (fills only `new`). Returns `(None, None)` when
/// the input is `None` or no shape matches.
pub(crate) fn extract_edit_strings(tool_input: Option<&Value>) -> (Option<String>, Option<String>) {
    let Some(input) = tool_input else {
        return (None, None);
    };
    if let Some(arr) = input.get("edits").and_then(|v| v.as_array()) {
        let mut old_acc = String::new();
        let mut new_acc = String::new();
        for e in arr {
            if let Some(s) = e.get("old_string").and_then(|v| v.as_str()) {
                if !old_acc.is_empty() {
                    old_acc.push_str("\n\n");
                }
                old_acc.push_str(s);
            }
            if let Some(s) = e.get("new_string").and_then(|v| v.as_str()) {
                if !new_acc.is_empty() {
                    new_acc.push_str("\n\n");
                }
                new_acc.push_str(s);
            }
        }
        let old = (!old_acc.is_empty()).then_some(old_acc);
        let new = (!new_acc.is_empty()).then_some(new_acc);
        return (old, new);
    }
    let old = input
        .get("old_string")
        .and_then(|v| v.as_str())
        .map(String::from);
    let new = input
        .get("new_string")
        .or_else(|| input.get("content"))
        .and_then(|v| v.as_str())
        .map(String::from);
    (old, new)
}

fn push_prefixed(buf: &mut String, prefix: char, text: &str) {
    for line in text.lines() {
        buf.push(prefix);
        buf.push_str(line);
        buf.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn diff_old_new_emits_minus_then_plus_lines() {
        let d = diff_old_new("a\nb", "c\nd");
        assert_eq!(d, "-a\n-b\n+c\n+d\n");
    }

    #[test]
    fn diff_content_emits_plus_lines_only() {
        assert_eq!(diff_content("x\ny"), "+x\n+y\n");
    }

    #[test]
    fn diff_shell_returns_none_when_both_blank() {
        assert!(diff_shell(None, None).is_none());
        assert!(diff_shell(Some(""), Some("   ")).is_none());
    }

    #[test]
    fn diff_shell_emits_dollar_command_then_plus_output() {
        let s = diff_shell(Some("ls -la"), Some("a\nb")).unwrap();
        assert!(s.contains("$ ls -la"));
        assert!(s.contains("+a"));
        assert!(s.contains("+b"));
    }

    #[test]
    fn extract_edit_strings_handles_multiedit_array() {
        let input = json!({
            "edits": [
                { "old_string": "A", "new_string": "B" },
                { "old_string": "C", "new_string": "D" }
            ]
        });
        let (old, new) = extract_edit_strings(Some(&input));
        assert_eq!(old.as_deref(), Some("A\n\nC"));
        assert_eq!(new.as_deref(), Some("B\n\nD"));
    }

    #[test]
    fn extract_edit_strings_handles_flat_old_new() {
        let input = json!({ "old_string": "x", "new_string": "y" });
        let (old, new) = extract_edit_strings(Some(&input));
        assert_eq!(old.as_deref(), Some("x"));
        assert_eq!(new.as_deref(), Some("y"));
    }

    #[test]
    fn extract_edit_strings_falls_back_to_content_for_write() {
        let input = json!({ "content": "hello" });
        let (old, new) = extract_edit_strings(Some(&input));
        assert!(old.is_none());
        assert_eq!(new.as_deref(), Some("hello"));
    }

    #[test]
    fn extract_edit_strings_none_input_returns_none_pair() {
        assert_eq!(extract_edit_strings(None), (None, None));
    }
}
