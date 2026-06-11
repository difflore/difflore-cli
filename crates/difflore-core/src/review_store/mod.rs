mod queries;
mod rows;
mod types;

pub use queries::{
    add_comment, ensure_item, list_by_project, list_by_source, list_by_source_with_comments,
    list_comments, list_recent, list_with_comments, remove_comment, remove_item,
    update_item_status,
};
pub use types::{
    AddCommentInput, EnsureItemInput, ListWithCommentsInput, ReviewCommentIdInput,
    ReviewCommentMetadataRecord, ReviewCommentRecord, ReviewExplainabilityMetadataRecord,
    ReviewIssueSnippetRecord, ReviewItemIdInput, ReviewItemRecord, ReviewItemWithComments,
    ReviewProjectInput, ReviewSourceInput, UpdateItemStatusInput,
};

pub(super) const EXPLAINABILITY_SCHEMA_VERSION: u8 = 1;
const EXPLAINABILITY_TOP_ISSUES_LIMIT: usize = 5;

pub(super) const fn default_explainability_schema_version() -> u8 {
    EXPLAINABILITY_SCHEMA_VERSION
}

// Borrow-only serialize mirrors of the metadata records: these
// stringify and discard, so no ownership is needed. The owned structs
// in `types.rs` cover the deserialize side.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ReviewIssueSnippetRef<'a> {
    severity: &'a str,
    rule: &'a str,
    rule_id: Option<&'a str>,
    message: &'a str,
    file: Option<&'a str>,
    line: Option<i32>,
    suggestion: Option<&'a str>,
    confidence: f32,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ReviewExplainabilityRef<'a> {
    schema_version: u8,
    matched_rule_ids: &'a [String],
    matched_rule_titles: &'a [String],
    prompt_tokens_estimate: i32,
    trace_id: &'a str,
    issue_count: usize,
    summary: Option<&'a crate::domain::models::ReviewSummary>,
    top_issues: Vec<ReviewIssueSnippetRef<'a>>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ReviewCommentMetadataRef<'a> {
    severity: &'a str,
    rule: &'a str,
    rule_id: Option<&'a str>,
    confidence: f32,
    suggestion: Option<&'a str>,
}

pub fn build_explainability_metadata(
    result: &crate::review_engine::ReviewCheckResult,
) -> Option<String> {
    let top_issues = result
        .issues
        .iter()
        .take(EXPLAINABILITY_TOP_ISSUES_LIMIT)
        .map(|issue| ReviewIssueSnippetRef {
            severity: &issue.severity,
            rule: &issue.rule,
            rule_id: issue.rule_id.as_deref(),
            message: &issue.message,
            file: issue.file.as_deref(),
            line: issue.line,
            suggestion: issue.suggestion.as_deref(),
            confidence: issue.confidence,
        })
        .collect();

    serde_json::to_string(&ReviewExplainabilityRef {
        schema_version: EXPLAINABILITY_SCHEMA_VERSION,
        matched_rule_ids: &result.matched_rule_ids,
        matched_rule_titles: &result.matched_rule_titles,
        prompt_tokens_estimate: result.prompt_tokens_estimate,
        trace_id: &result.trace_id,
        issue_count: result.issues.len(),
        summary: result.summary.as_ref(),
        top_issues,
    })
    .ok()
}

pub fn build_review_comment_metadata(
    issue: &crate::review_engine::ReviewIssueRecord,
) -> Option<String> {
    serde_json::to_string(&ReviewCommentMetadataRef {
        severity: &issue.severity,
        rule: &issue.rule,
        rule_id: issue.rule_id.as_deref(),
        confidence: issue.confidence,
        suggestion: issue.suggestion.as_deref(),
    })
    .ok()
}

pub fn format_review_issue_comment(issue: &crate::review_engine::ReviewIssueRecord) -> String {
    let mut content = issue.message.clone();
    if let Some(suggestion) = issue.suggestion.as_deref()
        && !suggestion.trim().is_empty()
    {
        content.push_str("\nSuggested fix: ");
        content.push_str(suggestion.trim());
    }
    content
}

#[cfg(test)]
mod tests {
    use super::queries::attach_comments;
    use super::rows::{
        ReviewCommentRow, UNKNOWN_REVIEW_COMMENT_LINE_NUMBER, stored_review_comment_line_number,
    };
    use super::types::{ReviewCommentRecord, ReviewItemRecord};
    use super::{build_explainability_metadata, format_review_issue_comment};
    use std::collections::HashMap;

    fn make_item(id: &str) -> ReviewItemRecord {
        ReviewItemRecord {
            id: id.into(),
            session_id: None,
            project_id: Some("proj-1".into()),
            file_path: format!("src/{id}.rs"),
            diff_content: String::new(),
            status: "pending".into(),
            source: "local".into(),
            source_kind: "manual".into(),
            external_review_id: None,
            repo_full_name: None,
            pr_number: None,
            author: None,
            synced_at: None,
            metadata: None,
            created_at: "2026-04-10 00:00:00".into(),
            reviewed_at: None,
        }
    }

    fn make_comment(id: &str, item_id: &str) -> ReviewCommentRecord {
        ReviewCommentRecord {
            id: id.into(),
            review_item_id: item_id.into(),
            external_comment_id: None,
            line_number: Some(1),
            content: "nit".into(),
            author: None,
            comment_url: None,
            thread_id: None,
            metadata: None,
            created_at: "2026-04-10 00:00:00".into(),
        }
    }

    #[test]
    fn attach_comments_pairs_by_item_id() {
        let items = vec![make_item("a"), make_item("b")];
        let mut by_item: HashMap<String, Vec<ReviewCommentRecord>> = HashMap::new();
        by_item.insert(
            "a".into(),
            vec![make_comment("c1", "a"), make_comment("c2", "a")],
        );
        by_item.insert("b".into(), vec![make_comment("c3", "b")]);

        let result = attach_comments(items, by_item);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].item.id, "a");
        assert_eq!(result[0].comments.len(), 2);
        assert_eq!(result[1].item.id, "b");
        assert_eq!(result[1].comments.len(), 1);
    }

    #[test]
    fn attach_comments_defaults_to_empty_when_no_comments() {
        let items = vec![make_item("lonely")];
        let by_item: HashMap<String, Vec<ReviewCommentRecord>> = HashMap::new();
        let result = attach_comments(items, by_item);
        assert_eq!(result.len(), 1);
        assert!(result[0].comments.is_empty());
    }

    #[test]
    fn attach_comments_drops_unmatched_comment_buckets() {
        let items = vec![make_item("a")];
        let mut by_item: HashMap<String, Vec<ReviewCommentRecord>> = HashMap::new();
        by_item.insert("a".into(), vec![make_comment("c1", "a")]);
        // Orphan bucket for a non-existent item must be ignored, not crash.
        by_item.insert("ghost".into(), vec![make_comment("c2", "ghost")]);
        let result = attach_comments(items, by_item);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].comments.len(), 1);
        assert_eq!(result[0].comments[0].id, "c1");
    }

    #[test]
    fn review_comment_row_converts_line_number_and_preserves_fields() {
        let row = ReviewCommentRow {
            id: "c1".into(),
            review_item_id: "item".into(),
            external_comment_id: Some("gh-1".into()),
            line_number: 42i64,
            content: "hello".into(),
            author: Some("bob".into()),
            comment_url: Some("https://x".into()),
            thread_id: Some("t1".into()),
            metadata: None,
            created_at: "t".into(),
        };
        let rec: ReviewCommentRecord = row.into();
        assert_eq!(rec.line_number, Some(42));
        assert_eq!(rec.author.as_deref(), Some("bob"));
        assert_eq!(rec.external_comment_id.as_deref(), Some("gh-1"));
        assert_eq!(rec.thread_id.as_deref(), Some("t1"));
    }

    #[test]
    fn review_comment_row_treats_non_positive_line_numbers_as_unknown() {
        let zero = ReviewCommentRow {
            id: "c1".into(),
            review_item_id: "item".into(),
            external_comment_id: None,
            line_number: 0,
            content: "hello".into(),
            author: None,
            comment_url: None,
            thread_id: None,
            metadata: None,
            created_at: "t".into(),
        };
        let rec: ReviewCommentRecord = zero.into();
        assert_eq!(rec.line_number, None);
        assert_eq!(
            stored_review_comment_line_number(None),
            UNKNOWN_REVIEW_COMMENT_LINE_NUMBER
        );
        assert_eq!(
            stored_review_comment_line_number(Some(0)),
            UNKNOWN_REVIEW_COMMENT_LINE_NUMBER
        );
    }

    #[test]
    fn explainability_metadata_round_trips() {
        let result = crate::review_engine::ReviewCheckResult {
            issues: vec![crate::review_engine::ReviewIssueRecord {
                severity: "warning".into(),
                rule: "avoid-foo".into(),
                rule_id: Some("rule-1".into()),
                message: "Avoid foo.".into(),
                file: Some("src/lib.rs".into()),
                line: Some(7),
                suggestion: Some("Use bar.".into()),
                source_badge: None,
                perspectives: vec!["style".into()],
                confidence: 0.82,
            }],
            matched_rules: 1,
            matched_rule_ids: vec!["rule-1".into()],
            matched_rule_titles: vec!["Avoid foo".into()],
            prompt_tokens_estimate: 123,
            trace_id: "trace-1".into(),
            summary: Some(crate::domain::models::ReviewSummary {
                one_line_summary: "Touches validation.".into(),
                walkthrough_by_file: vec![],
                blocking_count: 0,
                non_blocking_count: 1,
            }),
            stats: None,
        };

        let json = build_explainability_metadata(&result).expect("metadata json");
        let item = ReviewItemRecord {
            metadata: Some(json),
            ..make_item("meta")
        };
        let parsed = item
            .explainability_metadata()
            .expect("parsed explainability metadata");
        assert_eq!(parsed.schema_version, super::EXPLAINABILITY_SCHEMA_VERSION);
        assert_eq!(parsed.matched_rule_ids, vec!["rule-1"]);
        assert_eq!(parsed.matched_rule_titles, vec!["Avoid foo"]);
        assert_eq!(parsed.issue_count, 1);
        assert_eq!(parsed.top_issues.len(), 1);
        assert_eq!(parsed.top_issues[0].rule, "avoid-foo");
        assert_eq!(
            parsed.summary.unwrap().one_line_summary,
            "Touches validation."
        );
    }

    #[test]
    fn format_review_issue_comment_appends_suggestion() {
        let issue = crate::review_engine::ReviewIssueRecord {
            severity: "warning".into(),
            rule: "rule".into(),
            rule_id: None,
            message: "Main message".into(),
            file: None,
            line: None,
            suggestion: Some("Do the thing.".into()),
            source_badge: None,
            perspectives: vec![],
            confidence: 1.0,
        };
        assert_eq!(
            format_review_issue_comment(&issue),
            "Main message\nSuggested fix: Do the thing."
        );
    }
}
