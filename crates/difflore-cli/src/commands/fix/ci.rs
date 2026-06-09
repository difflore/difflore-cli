use std::io::{self, Write};

use difflore_core::review::ReviewIssueRecord;

use crate::commands::util::exit_code;
use crate::style::{self, sym};

use super::{CONFIDENCE_THRESHOLD, file_loc, issue_rule_label};

pub(super) fn ci_blocking_suggestions<'a>(
    suggestions: &[&'a ReviewIssueRecord],
    strict: bool,
) -> Vec<&'a ReviewIssueRecord> {
    if strict {
        suggestions.to_vec()
    } else {
        suggestions
            .iter()
            .copied()
            .filter(|s| s.confidence >= CONFIDENCE_THRESHOLD)
            .collect()
    }
}

pub(super) fn exit_after_output(code: i32) -> ! {
    io::stdout().flush().ok();
    io::stderr().flush().ok();
    exit_code(code);
}

// Default: only fail on confident patches. Structured outputs share this
// exit-code contract.
pub(super) fn finish_ci_mode(suggestions: &[&ReviewIssueRecord], strict: bool, scope_label: &str) {
    let blocking = ci_blocking_suggestions(suggestions, strict);
    if blocking.is_empty() {
        let low = suggestions.len();
        if low > 0 && !strict {
            eprintln!(
                "{} no confident patches; {low} low-confidence suggestion(s) held back. \
                 Use --strict to fail on those too.",
                style::ok(sym::OK),
            );
        }
        if low == 0 {
            eprintln!(
                "{} no patches suggested in {scope_label}.",
                style::ok(sym::OK),
            );
        }
        return;
    }

    let patch_label = if strict {
        "patch(es)"
    } else {
        "confident patch(es)"
    };
    eprintln!(
        "{} {} {patch_label} suggested in {scope_label} — run {} to review.",
        style::warn(sym::WARN),
        blocking.len(),
        style::cmd("difflore fix"),
    );
    for issue in &blocking {
        eprintln!(
            "  {} {}  ·  {}",
            style::pewter(sym::BULLET),
            file_loc(issue),
            issue_rule_label(issue),
        );
    }
    exit_after_output(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue_at(file: Option<&str>, line: Option<i32>, msg: &str) -> ReviewIssueRecord {
        ReviewIssueRecord {
            severity: "warning".into(),
            rule: "R".into(),
            rule_id: None,
            message: msg.into(),
            file: file.map(str::to_owned),
            line,
            suggestion: Some("do the thing".into()),
            source_badge: None,
            perspectives: Vec::new(),
            confidence: 0.9,
        }
    }

    #[test]
    fn ci_mode_blocks_confident_suggestions_for_structured_output() {
        let high = issue_at(Some("src/foo.ts"), Some(10), "confident");
        let mut low = issue_at(Some("src/bar.ts"), Some(20), "low");
        low.confidence = 0.5;
        let suggestions = vec![&high, &low];

        let blocking = ci_blocking_suggestions(&suggestions, false);

        assert_eq!(blocking.len(), 1);
        assert_eq!(blocking[0].message, "confident");
    }

    #[test]
    fn ci_strict_blocks_low_confidence_structured_suggestions() {
        let mut low = issue_at(Some("src/bar.ts"), Some(20), "low");
        low.confidence = 0.5;
        let suggestions = vec![&low];

        let blocking = ci_blocking_suggestions(&suggestions, true);

        assert_eq!(blocking.len(), 1);
        assert_eq!(blocking[0].message, "low");
    }
}
