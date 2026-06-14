#![allow(clippy::expect_used)]
#![allow(unsafe_code)]

use difflore_core::contract::{ImportedCommentUpload, ImportedReviewUpload};
use difflore_core::review_store::{
    AddCommentInput, EnsureItemInput, ReviewCommentRecord, ReviewItemRecord, ReviewItemWithComments,
};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::str::FromStr;
use std::sync::OnceLock;
use tempfile::TempDir;

fn ensure_test_home() {
    static HOME: OnceLock<TempDir> = OnceLock::new();
    HOME.get_or_init(|| {
        let dir = TempDir::new().expect("create import-reviews test home tempdir");
        // SAFETY: OnceLock guarantees this runs once per test process and
        // the TempDir is retained for the process lifetime.
        unsafe {
            std::env::set_var("DIFFLORE_HOME", dir.path());
        }
        dir
    });
}

pub(super) async fn fresh_import_pool() -> sqlx::SqlitePool {
    ensure_test_home();
    let opts = SqliteConnectOptions::from_str("sqlite::memory:")
        .expect("parse sqlite memory URL")
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .expect("open in-memory db");
    difflore_core::infra::db::run_migrations(&pool)
        .await
        .expect("apply migrations");
    pool
}

pub(super) async fn seed_imported_review_comments(
    db: &sqlx::SqlitePool,
    comments: &[(&str, &str)],
) {
    seed_imported_review_comments_with_resolution(db, comments, true).await;
}

/// Like [`seed_imported_review_comments`] but lets a test pick whether the
/// thread is resolved. Unresolved directives carry no adoption signal, so the
/// capture gate routes them to a pending candidate rather than auto-activating;
/// pass `false` to exercise the medium-confidence path.
pub(super) async fn seed_imported_review_comments_with_resolution(
    db: &sqlx::SqlitePool,
    comments: &[(&str, &str)],
    resolved: bool,
) {
    let item_id = "gh-import:acme/widgets#7";
    let project_path = std::path::PathBuf::from(std::env::var("DIFFLORE_HOME").expect("home"))
        .join("fixtures")
        .join("acme-widgets");
    std::fs::create_dir_all(&project_path).expect("create project fixture dir");
    let project = difflore_core::domain::projects::add(
        db,
        difflore_core::domain::models::AddProjectInput {
            path: project_path.to_string_lossy().to_string(),
        },
    )
    .await
    .expect("insert project");
    difflore_core::review_store::ensure_item(
        db,
        EnsureItemInput {
            id: Some(item_id.to_owned()),
            session_id: None,
            project_id: project.id,
            file_path: "src/http/request.rs".to_owned(),
            diff_content: String::new(),
            status: "imported".to_owned(),
            source: "github".to_owned(),
            source_kind: "github_import".to_owned(),
            external_review_id: Some(item_id.to_owned()),
            repo_full_name: Some("acme/widgets".to_owned()),
            pr_number: Some(7),
            author: Some("alice".to_owned()),
            synced_at: None,
            metadata: None,
            reviewed_at: None,
        },
    )
    .await
    .expect("insert imported review item");

    for (idx, (content, path)) in comments.iter().enumerate() {
        let comment = difflore_core::review_store::add_comment(
            db,
            AddCommentInput {
                review_item_id: item_id.to_owned(),
                external_comment_id: Some(format!("discussion-{idx}")),
                line_number: Some(i32::try_from(idx + 1).expect("small idx")),
                content: (*content).to_owned(),
                author: Some("reviewer".to_owned()),
                comment_url: Some(format!(
                    "https://github.com/acme/widgets/pull/7#discussion_r{idx}"
                )),
                thread_id: Some("review-7".to_owned()),
                metadata: Some(
                    serde_json::json!({
                        "filePath": path,
                        "sourceRepoFullName": "acme/widgets",
                        "attachedRepoFullName": "acme/widgets",
                        // `resolved` is the v1 adoption proxy: when true the
                        // capture gate treats the directive as adopted and auto-activates it.
                        "resolved": resolved,
                    })
                    .to_string(),
                ),
            },
        )
        .await
        .expect("insert imported review comment");
        sqlx::query("UPDATE review_comments SET created_at = ?1 WHERE id = ?2")
            .bind(format!("2026-05-09 00:00:{idx:02}"))
            .bind(&comment.id)
            .execute(db)
            .await
            .expect("stabilize comment order");
    }
}

/// Seed one imported review item under `repo` for a given PR number with a
/// single resolved directive comment. Unlike [`seed_imported_review_comments`]
/// (which pins PR #7), the PR number is controllable to exercise per-PR
/// filtering such as `--exclude-prs`.
pub(super) async fn seed_pr_with_directive(
    db: &sqlx::SqlitePool,
    repo: &str,
    pr_number: i32,
    directive: &str,
    path: &str,
) {
    let item_id = format!("gh-import:{repo}#{pr_number}");
    let project_path = std::path::PathBuf::from(std::env::var("DIFFLORE_HOME").expect("home"))
        .join("fixtures")
        .join(format!("{repo}-{pr_number}").replace('/', "-"));
    std::fs::create_dir_all(&project_path).expect("create project fixture dir");
    let project = difflore_core::domain::projects::add(
        db,
        difflore_core::domain::models::AddProjectInput {
            path: project_path.to_string_lossy().to_string(),
        },
    )
    .await
    .expect("insert project");
    difflore_core::review_store::ensure_item(
        db,
        EnsureItemInput {
            id: Some(item_id.clone()),
            session_id: None,
            project_id: project.id,
            file_path: path.to_owned(),
            diff_content: String::new(),
            status: "imported".to_owned(),
            source: "github".to_owned(),
            source_kind: "github_import".to_owned(),
            external_review_id: Some(item_id.clone()),
            repo_full_name: Some(repo.to_owned()),
            pr_number: Some(pr_number),
            author: Some("alice".to_owned()),
            synced_at: None,
            metadata: None,
            reviewed_at: None,
        },
    )
    .await
    .expect("insert imported review item");
    difflore_core::review_store::add_comment(
        db,
        AddCommentInput {
            review_item_id: item_id.clone(),
            external_comment_id: Some(format!("discussion-{pr_number}")),
            line_number: Some(1),
            content: directive.to_owned(),
            author: Some("reviewer".to_owned()),
            comment_url: Some(format!(
                "https://github.com/{repo}/pull/{pr_number}#discussion_r1"
            )),
            thread_id: Some(format!("review-{pr_number}")),
            metadata: Some(
                serde_json::json!({
                    "filePath": path,
                    "sourceRepoFullName": repo,
                    "attachedRepoFullName": repo,
                    "resolved": true,
                })
                .to_string(),
            ),
        },
    )
    .await
    .expect("insert imported review comment");
}

pub(super) async fn seed_gitlab_pr_with_directive(
    db: &sqlx::SqlitePool,
    host: &str,
    repo: &str,
    pr_number: i32,
    directive: &str,
    path: &str,
) {
    let item_id = format!("gl-import:{host}:{repo}#{pr_number}");
    let project_path = std::path::PathBuf::from(std::env::var("DIFFLORE_HOME").expect("home"))
        .join("fixtures")
        .join(format!("{host}-{repo}-{pr_number}").replace(['/', ':'], "-"));
    std::fs::create_dir_all(&project_path).expect("create project fixture dir");
    let project = difflore_core::domain::projects::add(
        db,
        difflore_core::domain::models::AddProjectInput {
            path: project_path.to_string_lossy().to_string(),
        },
    )
    .await
    .expect("insert project");
    difflore_core::review_store::ensure_item(
        db,
        EnsureItemInput {
            id: Some(item_id.clone()),
            session_id: None,
            project_id: project.id,
            file_path: path.to_owned(),
            diff_content: String::new(),
            status: "imported".to_owned(),
            source: "gitlab".to_owned(),
            source_kind: "gitlab_import".to_owned(),
            external_review_id: Some(item_id.clone()),
            repo_full_name: Some(repo.to_owned()),
            pr_number: Some(pr_number),
            author: Some("alice".to_owned()),
            synced_at: None,
            metadata: Some(
                serde_json::json!({
                    "gitlabHost": host,
                    "sourceRepoFullName": repo,
                })
                .to_string(),
            ),
            reviewed_at: None,
        },
    )
    .await
    .expect("insert imported review item");
    difflore_core::review_store::add_comment(
        db,
        AddCommentInput {
            review_item_id: item_id.clone(),
            external_comment_id: Some(format!("discussion-{pr_number}")),
            line_number: Some(1),
            content: directive.to_owned(),
            author: Some("reviewer".to_owned()),
            comment_url: Some(format!(
                "https://{host}/{repo}/-/merge_requests/{pr_number}#note_1"
            )),
            thread_id: Some(format!("review-{pr_number}")),
            metadata: Some(
                serde_json::json!({
                    "filePath": path,
                    "gitlabHost": host,
                    "sourceRepoFullName": repo,
                    "attachedRepoFullName": repo,
                    "resolved": true,
                })
                .to_string(),
            ),
        },
    )
    .await
    .expect("insert imported review comment");
}

pub(super) fn review(pr: i32, comments: usize) -> ImportedReviewUpload {
    ImportedReviewUpload {
        provider: Some("github".to_owned()),
        provider_host: None,
        repo_full_name: "difflore-fixtures/example".to_owned(),
        source_repo_full_name: Some("upstream/example".to_owned()),
        pr_number: pr,
        pr_title: Some(format!("PR {pr}")),
        comments: (0..comments)
            .map(|i| ImportedCommentUpload {
                event_type: None,
                file_path: Some("src/lib.rs".to_owned()),
                line_number: i as i32 + 1,
                content: format!("comment {i}"),
                author: Some("reviewer".to_owned()),
                comment_url: format!("https://example.test/{pr}#{i}"),
                thread_id: Some(format!("thread-{pr}-{i}")),
                occurred_at: Some("2026-04-30T00:00:00Z".to_owned()),
            })
            .collect(),
    }
}

pub(super) fn imported_item(repo: Option<&str>, metadata: Option<&str>) -> ReviewItemWithComments {
    ReviewItemWithComments {
        item: ReviewItemRecord {
            id: "gh-import:user/fork#7".into(),
            session_id: None,
            project_id: Some("project-1".into()),
            file_path: "src/lib.rs".into(),
            diff_content: String::new(),
            status: "imported".into(),
            source: "github".into(),
            source_kind: "github_import".into(),
            external_review_id: Some("gh-import:user/fork#7".into()),
            repo_full_name: repo.map(str::to_owned),
            pr_number: Some(7),
            author: Some("author".into()),
            synced_at: None,
            metadata: metadata.map(str::to_owned),
            created_at: "2026-04-30 00:00:00".into(),
            reviewed_at: None,
        },
        comments: vec![ReviewCommentRecord {
            id: "comment-1".into(),
            review_item_id: "gh-import:user/fork#7".into(),
            external_comment_id: Some("user/fork:upstream/project:100".into()),
            line_number: Some(12),
            content: "keep the source provenance".into(),
            author: Some("reviewer".into()),
            comment_url: Some("https://github.com/upstream/project/pull/7#discussion_r100".into()),
            thread_id: Some("thread-1".into()),
            metadata: None,
            created_at: "2026-04-30 00:00:00".into(),
        }],
    }
}
