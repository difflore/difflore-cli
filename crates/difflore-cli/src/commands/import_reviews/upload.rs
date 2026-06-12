use difflore_core::contract::{
    ImportedCommentEventType, ImportedCommentUpload, ImportedReviewUpload,
    UploadImportedReviewsRequest,
};
use difflore_core::infra::git::canonical_gitlab_repo_scope;
use difflore_core::ingest::github::ImportProgress;
use difflore_core::ingest::gitlab::auth::normalize_gitlab_host;
use difflore_core::review_store::{self, ReviewItemWithComments};
use sqlx::SqlitePool;

use crate::runtime::CommandContext;
use crate::style;
use crate::support::util::exit_err;

const CLOUD_IMPORT_MAX_REVIEWS_PER_BATCH: usize = 20;
const CLOUD_IMPORT_MAX_COMMENTS_PER_BATCH: usize = 20;

pub(super) fn build_upload_batches(
    reviews: &[ImportedReviewUpload],
) -> Vec<Vec<ImportedReviewUpload>> {
    let mut batches: Vec<Vec<ImportedReviewUpload>> = Vec::new();
    let mut current: Vec<ImportedReviewUpload> = Vec::new();
    let mut current_comments = 0usize;

    for review in reviews {
        for comments in review.comments.chunks(CLOUD_IMPORT_MAX_COMMENTS_PER_BATCH) {
            if comments.is_empty() {
                continue;
            }
            let mut split = review.clone();
            split.comments = comments.to_vec();
            let split_comments = split.comments.len();

            if !current.is_empty()
                && (current.len() >= CLOUD_IMPORT_MAX_REVIEWS_PER_BATCH
                    || current_comments + split_comments > CLOUD_IMPORT_MAX_COMMENTS_PER_BATCH)
            {
                batches.push(std::mem::take(&mut current));
                current_comments = 0;
            }

            current_comments += split_comments;
            current.push(split);
        }
    }

    if !current.is_empty() {
        batches.push(current);
    }

    batches
}

pub(super) fn source_repo_from_metadata(
    metadata: Option<&str>,
    repo_full_name: &str,
) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(metadata?).ok()?;
    let source_repo = value
        .get("sourceRepoFullName")
        .or_else(|| value.get("sourceRepo"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())?;

    if source_repo == repo_full_name {
        None
    } else {
        Some(source_repo.to_owned())
    }
}

pub(super) fn gitlab_host_from_metadata(metadata: Option<&str>) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(metadata?).ok()?;
    value
        .get("gitlabHost")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| normalize_gitlab_host(s).ok())
}

/// Read the inline comment's file path back out of the per-comment metadata
/// JSON (the `filePath` key written at import time). `None` for top-level
/// review bodies and comments without a recorded path — losing it here used
/// to null `file_path` on every uploaded comment, degrading the file-pattern
/// quality of cloud-extracted rules.
pub(super) fn comment_file_path_from_metadata(metadata: Option<&str>) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(metadata?).ok()?;
    value
        .get("filePath")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Derive the webhook-aligned event type for one imported comment so the
/// cloud labels `pr_review_comments.event_type` from the CLI's own import
/// provenance instead of re-deriving it server-side.
///
/// Order matters: the GitHub ingest path anchors PR discussion comments to
/// the first changed file (`sourceKind: "issue_comment"` plus a borrowed
/// `filePath` in the comment metadata), so the source-kind check must run
/// before the file-path check or those comments would be mislabeled as
/// inline review comments. GitLab MR-level discussion notes carry
/// `sourceKind: "mr_comment"` and map to the same discussion bucket.
///
/// Top-level review bodies have neither key (their metadata is durability
/// signal only, or absent), so they are recognized by the GitHub URL
/// fragment. `None` means "the local metadata cannot identify the bucket":
/// the optional field is omitted from the payload and the cloud falls back
/// to its own derivation, keeping older rows and unknown shapes behaviorally
/// unchanged.
pub(super) fn comment_event_type(
    metadata: Option<&str>,
    file_path: Option<&str>,
    comment_url: &str,
) -> Option<ImportedCommentEventType> {
    let source_kind = metadata
        .and_then(|m| serde_json::from_str::<serde_json::Value>(m).ok())
        .and_then(|v| {
            v.get("sourceKind")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        });
    if matches!(source_kind.as_deref(), Some("issue_comment" | "mr_comment")) {
        return Some(ImportedCommentEventType::IssueComment);
    }
    if file_path.is_some() {
        return Some(ImportedCommentEventType::PullRequestReviewComment);
    }
    let fragment = comment_url
        .split_once('#')
        .map_or("", |(_, fragment)| fragment);
    if fragment.starts_with("pullrequestreview-") {
        return Some(ImportedCommentEventType::PullRequestReview);
    }
    if fragment.starts_with("issuecomment-") {
        return Some(ImportedCommentEventType::IssueComment);
    }
    if fragment.starts_with("discussion_r") {
        return Some(ImportedCommentEventType::PullRequestReviewComment);
    }
    None
}

pub(super) fn imported_review_upload(
    item: &ReviewItemWithComments,
) -> Option<ImportedReviewUpload> {
    let repo_full_name = item.item.repo_full_name.clone()?;
    let provider = item.item.source.clone();
    let provider_host = if provider == "gitlab" {
        gitlab_host_from_metadata(item.item.metadata.as_deref())
    } else {
        None
    };
    let source_repo_full_name =
        source_repo_from_metadata(item.item.metadata.as_deref(), &repo_full_name);
    let upload_repo_full_name = if provider == "gitlab" {
        provider_host
            .as_deref()
            .and_then(|host| canonical_gitlab_repo_scope(host, &repo_full_name))
            .unwrap_or_else(|| repo_full_name.clone())
    } else {
        repo_full_name
    };
    let upload_source_repo_full_name = if provider == "gitlab" {
        source_repo_full_name.and_then(|source_repo| {
            provider_host
                .as_deref()
                .and_then(|host| canonical_gitlab_repo_scope(host, &source_repo))
        })
    } else {
        source_repo_full_name
    };
    let comments: Vec<ImportedCommentUpload> = item
        .comments
        .iter()
        .map(|c| {
            let file_path = comment_file_path_from_metadata(c.metadata.as_deref());
            let comment_url = c.comment_url.clone().unwrap_or_default();
            ImportedCommentUpload {
                event_type: comment_event_type(
                    c.metadata.as_deref(),
                    file_path.as_deref(),
                    &comment_url,
                ),
                file_path,
                line_number: c.line_number.unwrap_or(0),
                content: c.content.clone(),
                author: c.author.clone(),
                comment_url,
                thread_id: c.thread_id.clone(),
                occurred_at: Some(c.created_at.clone()),
            }
        })
        .collect();

    if comments.is_empty() {
        return None;
    }

    Some(ImportedReviewUpload {
        provider: Some(provider),
        provider_host,
        repo_full_name: upload_repo_full_name,
        source_repo_full_name: upload_source_repo_full_name,
        pr_number: item.item.pr_number.unwrap_or(0),
        pr_title: Some(item.item.file_path.clone()),
        comments,
    })
}

pub(super) fn matches_upload_target(
    item: &ReviewItemWithComments,
    source: &str,
    repo: &str,
    gitlab_host: Option<&str>,
) -> bool {
    let Some(item_repo) = item.item.repo_full_name.as_deref() else {
        return false;
    };

    if source != "gitlab" {
        return item_repo == repo;
    }

    let Some(target_host) = gitlab_host else {
        return false;
    };
    let Some(target_scope) = canonical_gitlab_repo_scope(target_host, repo) else {
        return false;
    };
    let Some(item_host) = gitlab_host_from_metadata(item.item.metadata.as_deref()) else {
        return false;
    };
    canonical_gitlab_repo_scope(&item_host, item_repo).as_deref() == Some(target_scope.as_str())
}

pub(super) const fn cloud_upload_next_step_commands() -> &'static [(&'static str, &'static str)] {
    &[
        ("difflore cloud sync", "# pull the new rules down"),
        ("difflore status", "# see what is ready locally"),
        ("difflore recall --diff", "# check what agents can recall"),
        (
            "difflore cloud impact",
            "# show recall and accepted-edit activity",
        ),
        (
            "difflore fix --preview",
            "# optional local patch suggestions",
        ),
    ]
}

pub(super) fn print_next_steps(uploaded_reviews: usize) {
    println!(
        "{} Uploaded {} reviews to cloud for AI extraction.",
        style::emerald(style::sym::OK),
        uploaded_reviews
    );
    println!();
    println!(
        "  {} Cloud is extracting patterns. In a few minutes:",
        style::emerald(style::sym::TIP),
    );
    for (cmd, hint) in cloud_upload_next_step_commands() {
        println!("    {} {}", style::cmd(cmd), style::pewter(hint));
    }
}

/// Upload imported reviews for `repo` (filtered to the given import
/// `source`: "github" or "gitlab"). The payload shape is provider-neutral —
/// GitHub keeps `owner/repo`, while GitLab uploads canonical
/// `host/group/project` repo keys so cloud upserts cannot collide across
/// providers or self-managed hosts.
pub(super) async fn run_upload(
    ctx: &CommandContext,
    db: &SqlitePool,
    source: &str,
    repo: &str,
    gitlab_host: Option<&str>,
    import_result: &ImportProgress,
    json: bool,
) -> Result<usize, String> {
    if import_result.comments_imported == 0 && import_result.comments_skipped > 0 {
        eprintln!(
            "No new review comments imported from {repo}; retrying cloud upload from local imported comments."
        );
    }

    let items = match review_store::list_by_source_with_comments(
        db,
        review_store::ReviewSourceInput {
            source: source.into(),
        },
    )
    .await
    {
        Ok(v) => v,
        Err(e) => exit_err(&format!("failed to load imported reviews: {e}")),
    };

    if items.is_empty() {
        eprintln!("No imported reviews to upload.");
        return Ok(0);
    }

    let upload_reviews: Vec<ImportedReviewUpload> = items
        .iter()
        .filter(|item| matches_upload_target(item, source, repo, gitlab_host))
        .filter_map(imported_review_upload)
        .collect();

    if upload_reviews.is_empty() {
        eprintln!("No reviews with comments to upload.");
        return Ok(0);
    }

    let cloud = ctx.cloud().await;

    // Pre-flight: without a session every batch fails with a generic "failed
    // batch" line, so bail with one actionable message instead of N silent
    // auth failures.
    if !cloud.is_logged_in() {
        style::report_error(
            "`--upload` requires a cloud session, but no token is on this machine.",
            "",
            &[
                style::Hint::try_("difflore cloud login".to_owned()),
                style::Hint::try_(
                    "rerun `difflore import-reviews --upload` once login completes".to_owned(),
                ),
            ],
        );
        return Err("not logged in to cloud; `--upload` cannot proceed".to_owned());
    }

    let total_reviews = upload_reviews.len();
    let batches = build_upload_batches(&upload_reviews);
    let total_batches = batches.len();
    let mut uploaded_reviews = 0usize;
    let mut failed_batches = 0usize;

    for (idx, batch) in batches.into_iter().enumerate() {
        let req = UploadImportedReviewsRequest { reviews: batch };
        let comments = req.reviews.iter().map(|r| r.comments.len()).sum::<usize>();
        if cloud.upload_imported_reviews(&req).await {
            uploaded_reviews += req.reviews.len();
            eprintln!(
                "  uploaded batch {}/{} ({} reviews, {} comments)",
                idx + 1,
                total_batches,
                req.reviews.len(),
                comments
            );
        } else {
            failed_batches += 1;
            eprintln!(
                "  failed batch {}/{} ({} reviews, {} comments)",
                idx + 1,
                total_batches,
                req.reviews.len(),
                comments
            );
        }
    }

    if failed_batches == 0 {
        // Under --json the success summary belongs only in the structured
        // payload; printing the human banner here would prepend non-JSON text to
        // stdout and break `... --json | jq`. Per-batch progress goes to stderr.
        if !json {
            print_next_steps(uploaded_reviews);
        }
    } else {
        style::report_error(
            "Cloud upload was incomplete; local import succeeded but extraction is not fully queued.",
            "",
            &[
                style::Hint::try_("difflore cloud status".to_owned()),
                style::Hint::try_(
                    "rerun `difflore import-reviews --upload` to retry failed batches".to_owned(),
                ),
            ],
        );
        return Err(format!(
            "uploaded {uploaded_reviews}/{total_reviews} imported reviews; {failed_batches}/{total_batches} cloud batches failed",
        ));
    }
    Ok(uploaded_reviews)
}
