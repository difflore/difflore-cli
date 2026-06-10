use uuid::Uuid;

use super::rows::{ReviewCommentRow, ReviewItemRow, stored_review_comment_line_number};
use super::types::{
    AddCommentInput, EnsureItemInput, ListWithCommentsInput, ReviewCommentIdInput,
    ReviewCommentRecord, ReviewItemIdInput, ReviewItemRecord, ReviewItemWithComments,
    ReviewProjectInput, ReviewSourceInput, UpdateItemStatusInput,
};

pub(super) async fn fetch_comments_for_items(
    pool: &sqlx::SqlitePool,
    items: &[ReviewItemRecord],
) -> crate::Result<std::collections::HashMap<String, Vec<ReviewCommentRecord>>> {
    if items.is_empty() {
        return Ok(std::collections::HashMap::new());
    }

    let ids: Vec<String> = items.iter().map(|i| i.id.clone()).collect();
    let ids_json = serde_json::to_string(&ids).map_err(|e| {
        crate::errors::CoreError::Internal(format!("failed to encode review item ids: {e}"))
    })?;
    let comments: Vec<ReviewCommentRecord> = sqlx::query_as!(
        ReviewCommentRow,
        "SELECT id, review_item_id, external_comment_id, line_number, content, author, comment_url, \
         thread_id, metadata, created_at FROM review_comments \
         WHERE review_item_id IN (SELECT value FROM json_each(?1)) \
         ORDER BY created_at ASC",
        ids_json
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(ReviewCommentRecord::from)
    .collect();

    let mut by_item: std::collections::HashMap<String, Vec<ReviewCommentRecord>> =
        std::collections::HashMap::new();
    for c in comments {
        by_item.entry(c.review_item_id.clone()).or_default().push(c);
    }
    Ok(by_item)
}

pub(super) fn attach_comments(
    items: Vec<ReviewItemRecord>,
    mut by_item: std::collections::HashMap<String, Vec<ReviewCommentRecord>>,
) -> Vec<ReviewItemWithComments> {
    items
        .into_iter()
        .map(|item| {
            let comments = by_item.remove(&item.id).unwrap_or_default();
            ReviewItemWithComments { item, comments }
        })
        .collect()
}

pub async fn list_by_project(
    db: &sqlx::SqlitePool,
    input: ReviewProjectInput,
) -> crate::Result<Vec<ReviewItemRecord>> {
    let rows = sqlx::query_as!(
        ReviewItemRow,
        "SELECT id, session_id, project_id, file_path, diff_content, status, source, source_kind, \
         external_review_id, repo_full_name, pr_number, author, synced_at, metadata, created_at, reviewed_at \
         FROM review_items WHERE project_id = ? ORDER BY created_at DESC",
        input.project_id
    )
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(ReviewItemRecord::from).collect())
}

/// List the most recent review items across all sources (cross-source feed,
/// not a per-source filter).
pub async fn list_recent(
    db: &sqlx::SqlitePool,
    limit: i64,
) -> crate::Result<Vec<ReviewItemRecord>> {
    let rows = sqlx::query_as!(
        ReviewItemRow,
        "SELECT id, session_id, project_id, file_path, diff_content, status, source, source_kind, \
         external_review_id, repo_full_name, pr_number, author, synced_at, metadata, created_at, reviewed_at \
         FROM review_items ORDER BY created_at DESC LIMIT ?",
        limit
    )
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(ReviewItemRecord::from).collect())
}

pub async fn list_by_source(
    db: &sqlx::SqlitePool,
    input: ReviewSourceInput,
) -> crate::Result<Vec<ReviewItemRecord>> {
    let rows = sqlx::query_as!(
        ReviewItemRow,
        "SELECT id, session_id, project_id, file_path, diff_content, status, source, source_kind, \
         external_review_id, repo_full_name, pr_number, author, synced_at, metadata, created_at, reviewed_at \
         FROM review_items WHERE source = ? ORDER BY created_at DESC",
        input.source
    )
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(ReviewItemRecord::from).collect())
}

pub async fn list_by_source_with_comments(
    db: &sqlx::SqlitePool,
    input: ReviewSourceInput,
) -> crate::Result<Vec<ReviewItemWithComments>> {
    let items: Vec<ReviewItemRecord> = sqlx::query_as!(
        ReviewItemRow,
        "SELECT id, session_id, project_id, file_path, diff_content, status, source, source_kind, \
         external_review_id, repo_full_name, pr_number, author, synced_at, metadata, created_at, reviewed_at \
         FROM review_items WHERE source = ? ORDER BY created_at DESC",
        input.source
    )
    .fetch_all(db)
    .await?
    .into_iter()
    .map(ReviewItemRecord::from)
    .collect();

    let by_item = fetch_comments_for_items(db, &items).await?;
    Ok(attach_comments(items, by_item))
}

pub async fn list_with_comments(
    db: &sqlx::SqlitePool,
    input: ListWithCommentsInput,
) -> crate::Result<Vec<ReviewItemWithComments>> {
    let items: Vec<ReviewItemRecord> = sqlx::query_as!(
        ReviewItemRow,
        "SELECT id, session_id, project_id, file_path, diff_content, status, source, source_kind, \
         external_review_id, repo_full_name, pr_number, author, synced_at, metadata, created_at, reviewed_at \
         FROM review_items WHERE project_id = ? ORDER BY created_at DESC",
        input.project_id
    )
    .fetch_all(db)
    .await?
    .into_iter()
    .map(ReviewItemRecord::from)
    .collect();

    let by_item = fetch_comments_for_items(db, &items).await?;
    Ok(attach_comments(items, by_item))
}

pub async fn add_comment(
    db: &sqlx::SqlitePool,
    input: AddCommentInput,
) -> crate::Result<ReviewCommentRecord> {
    let id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let stored_line_number = stored_review_comment_line_number(input.line_number);

    sqlx::query!(
        "INSERT INTO review_comments (id, review_item_id, external_comment_id, line_number, content, \
         author, comment_url, thread_id, metadata, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        id,
        input.review_item_id,
        input.external_comment_id,
        stored_line_number,
        input.content,
        input.author,
        input.comment_url,
        input.thread_id,
        input.metadata,
        now
    )
    .execute(db)
    .await?;

    let row = sqlx::query_as!(
        ReviewCommentRow,
        "SELECT id, review_item_id, external_comment_id, line_number, content, author, comment_url, \
         thread_id, metadata, created_at FROM review_comments WHERE id = ?",
        id
    )
    .fetch_one(db)
    .await?;
    Ok(ReviewCommentRecord::from(row))
}

pub async fn ensure_item(
    db: &sqlx::SqlitePool,
    input: EnsureItemInput,
) -> crate::Result<ReviewItemRecord> {
    let id = input
        .id
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    let exists: bool =
        sqlx::query_scalar!("SELECT EXISTS(SELECT 1 FROM review_items WHERE id = ?)", id)
            .fetch_one(db)
            .await?
            != 0;

    if exists {
        sqlx::query!(
            "UPDATE review_items SET session_id = ?, project_id = ?, file_path = ?, diff_content = ?, \
             status = ?, source = ?, source_kind = ?, external_review_id = ?, repo_full_name = ?, \
             pr_number = ?, author = ?, synced_at = ?, metadata = ?, reviewed_at = ? \
             WHERE id = ?",
            input.session_id,
            input.project_id,
            input.file_path,
            input.diff_content,
            input.status,
            input.source,
            input.source_kind,
            input.external_review_id,
            input.repo_full_name,
            input.pr_number,
            input.author,
            input.synced_at,
            input.metadata,
            input.reviewed_at,
            id
        )
        .execute(db)
        .await?;
    } else {
        let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
        sqlx::query!(
            "INSERT INTO review_items (id, session_id, project_id, file_path, diff_content, status, source, \
             source_kind, external_review_id, repo_full_name, pr_number, author, synced_at, metadata, created_at, reviewed_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            id,
            input.session_id,
            input.project_id,
            input.file_path,
            input.diff_content,
            input.status,
            input.source,
            input.source_kind,
            input.external_review_id,
            input.repo_full_name,
            input.pr_number,
            input.author,
            input.synced_at,
            input.metadata,
            now,
            input.reviewed_at
        )
        .execute(db)
        .await?;
    }

    let row = sqlx::query_as!(
        ReviewItemRow,
        "SELECT id, session_id, project_id, file_path, diff_content, status, source, source_kind, \
         external_review_id, repo_full_name, pr_number, author, synced_at, metadata, created_at, reviewed_at \
         FROM review_items WHERE id = ?",
        id
    )
    .fetch_one(db)
    .await?;
    Ok(ReviewItemRecord::from(row))
}

pub async fn update_item_status(
    db: &sqlx::SqlitePool,
    input: UpdateItemStatusInput,
) -> crate::Result<()> {
    let result = sqlx::query!(
        "UPDATE review_items SET status = ? WHERE id = ?",
        input.status,
        input.id
    )
    .execute(db)
    .await?;
    if result.rows_affected() == 0 {
        return Err(crate::errors::CoreError::NotFound(format!(
            "review item '{}' not found.",
            input.id
        )));
    }
    Ok(())
}

pub async fn remove_item(db: &sqlx::SqlitePool, input: ReviewItemIdInput) -> crate::Result<()> {
    // Clear dependent comments first (zero rows is fine); the item delete
    // below is the row count we check for existence.
    sqlx::query!(
        "DELETE FROM review_comments WHERE review_item_id = ?",
        input.id
    )
    .execute(db)
    .await?;
    let result = sqlx::query!("DELETE FROM review_items WHERE id = ?", input.id)
        .execute(db)
        .await?;
    if result.rows_affected() == 0 {
        return Err(crate::errors::CoreError::NotFound(format!(
            "review item '{}' not found.",
            input.id
        )));
    }
    Ok(())
}

pub async fn remove_comment(
    db: &sqlx::SqlitePool,
    input: ReviewCommentIdInput,
) -> crate::Result<()> {
    let result = sqlx::query!("DELETE FROM review_comments WHERE id = ?", input.id)
        .execute(db)
        .await?;
    if result.rows_affected() == 0 {
        return Err(crate::errors::CoreError::NotFound(format!(
            "review comment '{}' not found.",
            input.id
        )));
    }
    Ok(())
}

pub async fn list_comments(
    db: &sqlx::SqlitePool,
    input: ReviewItemIdInput,
) -> crate::Result<Vec<ReviewCommentRecord>> {
    let rows = sqlx::query_as!(
        ReviewCommentRow,
        "SELECT id, review_item_id, external_comment_id, line_number, content, author, comment_url, \
         thread_id, metadata, created_at FROM review_comments WHERE review_item_id = ? ORDER BY created_at ASC",
        input.id
    )
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(ReviewCommentRecord::from).collect())
}
