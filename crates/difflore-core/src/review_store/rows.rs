use super::types::{ReviewCommentRecord, ReviewItemRecord};

pub(super) const UNKNOWN_REVIEW_COMMENT_LINE_NUMBER: i32 = -1;

#[derive(sqlx::FromRow)]
pub(super) struct ReviewItemRow {
    pub(super) id: String,
    pub(super) session_id: Option<String>,
    pub(super) project_id: Option<String>,
    pub(super) file_path: String,
    pub(super) diff_content: String,
    pub(super) status: String,
    pub(super) source: String,
    pub(super) source_kind: String,
    pub(super) external_review_id: Option<String>,
    pub(super) repo_full_name: Option<String>,
    pub(super) pr_number: Option<i64>,
    pub(super) author: Option<String>,
    pub(super) synced_at: Option<String>,
    pub(super) metadata: Option<String>,
    pub(super) created_at: String,
    pub(super) reviewed_at: Option<String>,
}

impl From<ReviewItemRow> for ReviewItemRecord {
    fn from(r: ReviewItemRow) -> Self {
        Self {
            id: r.id,
            session_id: r.session_id,
            project_id: r.project_id,
            file_path: r.file_path,
            diff_content: r.diff_content,
            status: r.status,
            source: r.source,
            source_kind: r.source_kind,
            external_review_id: r.external_review_id,
            repo_full_name: r.repo_full_name,
            pr_number: r.pr_number.and_then(|v| i32::try_from(v).ok()),
            author: r.author,
            synced_at: r.synced_at,
            metadata: r.metadata,
            created_at: r.created_at,
            reviewed_at: r.reviewed_at,
        }
    }
}

#[derive(sqlx::FromRow)]
pub(super) struct ReviewCommentRow {
    pub(super) id: String,
    pub(super) review_item_id: String,
    pub(super) external_comment_id: Option<String>,
    pub(super) line_number: i64,
    pub(super) content: String,
    pub(super) author: Option<String>,
    pub(super) comment_url: Option<String>,
    pub(super) thread_id: Option<String>,
    pub(super) metadata: Option<String>,
    pub(super) created_at: String,
}

impl From<ReviewCommentRow> for ReviewCommentRecord {
    fn from(r: ReviewCommentRow) -> Self {
        Self {
            id: r.id,
            review_item_id: r.review_item_id,
            external_comment_id: r.external_comment_id,
            line_number: normalize_review_comment_line_number(r.line_number),
            content: r.content,
            author: r.author,
            comment_url: r.comment_url,
            thread_id: r.thread_id,
            metadata: r.metadata,
            created_at: r.created_at,
        }
    }
}

pub(super) fn normalize_review_comment_line_number(raw: i64) -> Option<i32> {
    i32::try_from(raw).ok().filter(|line| *line > 0)
}

pub(super) fn stored_review_comment_line_number(line_number: Option<i32>) -> i32 {
    line_number
        .filter(|line| *line > 0)
        .unwrap_or(UNKNOWN_REVIEW_COMMENT_LINE_NUMBER)
}
