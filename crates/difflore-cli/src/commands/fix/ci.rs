use std::io::{self, Write};

use difflore_core::review_engine::ReviewIssueRecord;

use crate::style::{self, sym};
use crate::support::util::exit_code;

use super::{CONFIDENCE_THRESHOLD, file_loc, issue_rule_label};

pub(super) fn ci_blocking_suggestions<'a>(
    suggestions: &[&'a ReviewIssueRecord],
    matched_rule_ids: &[String],
    strict: bool,
) -> Vec<&'a ReviewIssueRecord> {
    if strict {
        suggestions.to_vec()
    } else {
        suggestions
            .iter()
            .copied()
            .filter(|s| {
                s.confidence >= CONFIDENCE_THRESHOLD
                    || issue_cites_matched_rule(s, matched_rule_ids)
            })
            .collect()
    }
}

fn issue_cites_matched_rule(issue: &ReviewIssueRecord, matched_rule_ids: &[String]) -> bool {
    let Some(rule_id) = issue
        .rule_id
        .as_deref()
        .map(str::trim)
        .filter(|rule_id| !rule_id.is_empty())
    else {
        return false;
    };
    matched_rule_ids
        .iter()
        .any(|matched_id| matched_id.trim() == rule_id)
}

pub(super) fn exit_after_output(code: i32) -> ! {
    io::stdout().flush().ok();
    io::stderr().flush().ok();
    exit_code(code);
}

// Only fails on confident or rule-backed patches unless `strict`; structured
// outputs share this exit-code contract.
pub(super) fn finish_ci_mode(
    suggestions: &[&ReviewIssueRecord],
    matched_rule_ids: &[String],
    strict: bool,
    scope_label: &str,
) {
    let blocking = ci_blocking_suggestions(suggestions, matched_rule_ids, strict);
    if blocking.is_empty() {
        let low = suggestions.len();
        if low > 0 && !strict {
            eprintln!(
                "{} no confident/rule-backed patches; {low} low-confidence suggestion(s) held back. \
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
        "confident/rule-backed patch(es)"
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

        let blocking = ci_blocking_suggestions(&suggestions, &[], false);

        assert_eq!(blocking.len(), 1);
        assert_eq!(blocking[0].message, "confident");
    }

    #[test]
    fn ci_strict_blocks_low_confidence_structured_suggestions() {
        let mut low = issue_at(Some("src/bar.ts"), Some(20), "low");
        low.confidence = 0.5;
        let suggestions = vec![&low];

        let blocking = ci_blocking_suggestions(&suggestions, &[], true);

        assert_eq!(blocking.len(), 1);
        assert_eq!(blocking[0].message, "low");
    }

    #[test]
    fn ci_mode_blocks_low_confidence_suggestions_citing_matched_rules() {
        let mut rule_backed = issue_at(Some("src/rule.ts"), Some(30), "rule backed");
        rule_backed.confidence = 0.75;
        rule_backed.rule_id = Some("active-rule".to_owned());
        let suggestions = vec![&rule_backed];
        let matched_rule_ids = vec!["active-rule".to_owned()];

        let blocking = ci_blocking_suggestions(&suggestions, &matched_rule_ids, false);

        assert_eq!(blocking.len(), 1);
        assert_eq!(blocking[0].message, "rule backed");
    }

    #[test]
    fn ci_mode_does_not_block_hallucinated_rule_ids() {
        let mut hallucinated = issue_at(Some("src/rule.ts"), Some(30), "hallucinated");
        hallucinated.confidence = 0.75;
        hallucinated.rule_id = Some("hallucinated-rule".to_owned());
        let suggestions = vec![&hallucinated];
        let matched_rule_ids = vec!["active-rule".to_owned()];

        let blocking = ci_blocking_suggestions(&suggestions, &matched_rule_ids, false);

        assert!(blocking.is_empty());
    }
}
