//! Comment parsing, durability-signal derivation, and PR filtering/dedup
//! helpers.
//!
//! Turns the raw GraphQL wire shapes in `schema.rs` into the comment metadata
//! and candidate set the importer persists. Holds no HTTP / `gh`-CLI concerns.
//! The provider-neutral signal/metadata shapes live in
//! [`crate::ingest::common`]; this module only owns the GitHub-specific
//! constructor from GraphQL reaction groups.

use sqlx::SqlitePool;

use super::schema::{ActorNode, PrNode, ReactionGroupNode};
use super::{ImportOptions, non_empty_path, representative_file_path};
use crate::ingest::ImportProgress;
use crate::ingest::common::{CommentDurabilitySignal, comment_exists, comment_metadata_json};
use crate::review_store::{AddCommentInput, EnsureItemInput};

impl CommentDurabilitySignal {
    /// Derive the reaction half of the signal from GitHub's GraphQL
    /// `reactionGroups` shape. Thread resolution and later replies are filled
    /// in by the caller, which sees the whole thread.
    pub(super) fn from_reaction_groups(groups: &[ReactionGroupNode]) -> Self {
        let mut signal = Self::default();
        for group in groups {
            let count = group.users.total_count.max(0);
            signal.reactions_total += count;
            match group.content.as_deref() {
                Some("THUMBS_UP") => signal.thumbs_up += count,
                Some("THUMBS_DOWN") => signal.thumbs_down += count,
                _ => {}
            }
        }
        signal
    }
}

pub(super) fn imported_external_id(repo: &str, source_repo: &str, db_id: i64) -> String {
    if repo == source_repo {
        db_id.to_string()
    } else {
        format!("{repo}:{source_repo}:{db_id}")
    }
}

pub(super) fn changed_file_paths(pr: &PrNode) -> Vec<String> {
    let mut paths = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for file in &pr.files.nodes {
        if let Some(path) = non_empty_path(Some(file.path.as_str()))
            && seen.insert(path.to_owned())
        {
            paths.push(path.to_owned());
        }
    }
    paths
}

pub(super) fn item_metadata_json(opts: &ImportOptions, pr: &PrNode) -> Option<String> {
    let mut metadata = serde_json::Map::new();
    if opts.source_repo != opts.repo {
        metadata.insert(
            "sourceRepoFullName".to_owned(),
            serde_json::Value::String(opts.source_repo.clone()),
        );
        metadata.insert(
            "attachedRepoFullName".to_owned(),
            serde_json::Value::String(opts.repo.clone()),
        );
    }
    let paths = changed_file_paths(pr);
    if !paths.is_empty() {
        metadata.insert("changedFilePaths".to_owned(), serde_json::json!(paths));
    }
    if metadata.is_empty() {
        None
    } else {
        Some(serde_json::Value::Object(metadata).to_string())
    }
}

fn actor_source_kind(actor: Option<&ActorNode>) -> Option<String> {
    let actor = actor?;
    let login = actor.login.trim();
    if login.is_empty() {
        return None;
    }
    match actor.type_name.as_deref() {
        Some("Bot") => Some(format!("bot:{login}")),
        // No __typename in the payload means the author type is unverified;
        // omit the claim so downstream classification re-derives it instead
        // of trusting a baked-in "human" label.
        None => None,
        Some(_) => Some("human".to_owned()),
    }
}

fn github_comment_metadata_json(
    file_path: Option<&str>,
    source_repo: Option<&str>,
    attached_repo: &str,
    source_kind: Option<&str>,
    signal: &CommentDurabilitySignal,
    actor: Option<&ActorNode>,
) -> String {
    let mut value = serde_json::from_str::<serde_json::Value>(&comment_metadata_json(
        file_path,
        source_repo,
        attached_repo,
        source_kind,
        signal,
    ))
    .unwrap_or_else(|_| serde_json::json!({}));
    if let Some(obj) = value.as_object_mut()
        && let Some(actor) = actor
    {
        if let Some(type_name) = actor.type_name.as_deref() {
            obj.insert(
                "authorType".to_owned(),
                serde_json::Value::String(type_name.to_owned()),
            );
            obj.insert(
                "authorIsBot".to_owned(),
                serde_json::Value::Bool(type_name == "Bot"),
            );
        }
    }
    value.to_string()
}

/// Drop any fetched PR whose `number` is in `exclude_prs`, in place. Runs
/// before comments become candidates so an excluded PR contributes zero rules
/// — the leak-free guarantee `--exclude-prs` relies on for recall evaluation.
pub(super) fn drop_excluded_prs(
    collected: &mut Vec<PrNode>,
    exclude_prs: &std::collections::HashSet<i32>,
) {
    if exclude_prs.is_empty() {
        return;
    }
    collected.retain(|pr| pr.number.is_none_or(|n| !exclude_prs.contains(&n)));
}

/// Persist one content-carrying PR: `ensure_item` for the PR, then one
/// `add_comment` per importable inline thread comment, top-level review body,
/// and PR discussion comment. Mirrors the GitLab importer's
/// `persist_merge_request` so everything downstream stays provider-neutral;
/// `import_pr_reviews` keeps only fetch/pagination/filtering.
///
/// `pr_number` is threaded in by the caller, which already proved the search
/// node carries one before adding the PR to the persistable set.
pub(super) async fn persist_pull_request(
    db: &SqlitePool,
    opts: &ImportOptions,
    pr: &PrNode,
    pr_number: i32,
    progress: &mut ImportProgress,
) -> crate::Result<()> {
    let item_id = format!("gh-import:{}#{}", opts.repo, pr_number);
    let source_metadata = item_metadata_json(opts, pr);

    // Pick a representative real file path: first inline comment's path,
    // then first changed file. Leave it empty when the PR has no path
    // anchor rather than writing a PR title into a path-typed field.
    let file_path = representative_file_path(pr);

    crate::review_store::ensure_item(
        db,
        EnsureItemInput {
            id: Some(item_id.clone()),
            session_id: None,
            project_id: opts.project_id.clone(),
            file_path: file_path.clone(),
            diff_content: String::new(),
            status: "imported".into(),
            source: "github".into(),
            source_kind: "github_import".into(),
            external_review_id: Some(item_id.clone()),
            repo_full_name: Some(opts.repo.clone()),
            pr_number: Some(pr_number),
            author: pr.author.as_ref().map(|a| a.login.clone()),
            synced_at: None,
            metadata: source_metadata,
            reviewed_at: None,
        },
    )
    .await?;

    persist_inline_comments(db, opts, pr, &item_id, progress).await?;
    persist_review_bodies(db, opts, pr, &item_id, progress).await?;
    persist_discussion_comments(db, opts, pr, &item_id, &file_path, progress).await?;

    Ok(())
}

/// Inline diff comments. Within each thread the comments are ordered
/// oldest-first, so a comment's later replies are simply the tail of the
/// thread after it. We capture those plus the thread's resolved state and the
/// comment's reactions as a per-comment durability signal — the
/// local-candidate gate reads them back from metadata to score capture
/// confidence (resolved/approved = adopted; a later "actually no" reply =
/// contradiction).
async fn persist_inline_comments(
    db: &SqlitePool,
    opts: &ImportOptions,
    pr: &PrNode,
    item_id: &str,
    progress: &mut ImportProgress,
) -> crate::Result<()> {
    for thread in &pr.review_threads.nodes {
        for (idx, comment) in thread.comments.nodes.iter().enumerate() {
            let Some(db_id) = comment.database_id else {
                continue;
            };
            let legacy_ext_id = imported_external_id(&opts.repo, &opts.source_repo, db_id);
            let ext_id = format!("inline-comment-{legacy_ext_id}");
            if comment.body.trim().is_empty() {
                continue;
            }
            if comment_exists(db, &ext_id).await? || comment_exists(db, &legacy_ext_id).await? {
                progress.comments_skipped += 1;
                continue;
            }
            let thread_id = comment
                .pull_request_review
                .as_ref()
                .and_then(|r| r.database_id)
                .map(|id| id.to_string());

            let mut signal =
                CommentDurabilitySignal::from_reaction_groups(&comment.reaction_groups);
            signal.resolved = thread.is_resolved;
            signal.later_replies = thread
                .comments
                .nodes
                .iter()
                .skip(idx + 1)
                .map(|reply| reply.body.clone())
                .filter(|body| !body.trim().is_empty())
                .collect();
            signal.earlier_thread_authors = earlier_thread_authors(&thread.comments.nodes, idx);
            signal.earlier_thread_source_kinds =
                earlier_thread_source_kinds(&thread.comments.nodes, idx);

            crate::review_store::add_comment(
                db,
                AddCommentInput {
                    review_item_id: item_id.to_owned(),
                    external_comment_id: Some(ext_id),
                    line_number: comment.line,
                    content: comment.body.clone(),
                    author: comment.author.as_ref().map(|a| a.login.clone()),
                    comment_url: comment.url.clone(),
                    thread_id,
                    metadata: Some(github_comment_metadata_json(
                        comment.path.as_deref(),
                        Some(&opts.source_repo),
                        &opts.repo,
                        actor_source_kind(comment.author.as_ref()).as_deref(),
                        &signal,
                        comment.author.as_ref(),
                    )),
                },
            )
            .await?;
            progress.comments_imported += 1;
        }
    }
    Ok(())
}

/// Distinct authors of the content-carrying comments before index `idx` in
/// the same review thread, oldest-first. Reply linkage for the candidate
/// gate's trust classifier: a human comment whose thread was opened (or
/// joined earlier) by a bot reviewer is classified as `human_override_bot`.
fn earlier_thread_authors(
    comments: &[super::schema::ReviewCommentNode],
    idx: usize,
) -> Vec<String> {
    const EARLIER_AUTHOR_LIMIT: usize = 10;
    let mut authors: Vec<String> = Vec::new();
    for earlier in comments.iter().take(idx) {
        if earlier.body.trim().is_empty() {
            continue;
        }
        let Some(login) = earlier
            .author
            .as_ref()
            .map(|a| a.login.trim())
            .filter(|login| !login.is_empty())
        else {
            continue;
        };
        if authors.iter().any(|seen| seen == login) {
            continue;
        }
        authors.push(login.to_owned());
        if authors.len() >= EARLIER_AUTHOR_LIMIT {
            break;
        }
    }
    authors
}

fn earlier_thread_source_kinds(
    comments: &[super::schema::ReviewCommentNode],
    idx: usize,
) -> Vec<String> {
    const EARLIER_SOURCE_KIND_LIMIT: usize = 10;
    let mut kinds: Vec<String> = Vec::new();
    for earlier in comments.iter().take(idx) {
        if earlier.body.trim().is_empty() {
            continue;
        }
        let Some(kind) = actor_source_kind(earlier.author.as_ref()) else {
            continue;
        };
        if kinds.iter().any(|seen| seen == &kind) {
            continue;
        }
        kinds.push(kind);
        if kinds.len() >= EARLIER_SOURCE_KIND_LIMIT {
            break;
        }
    }
    kinds
}

/// Top-level review bodies.
async fn persist_review_bodies(
    db: &SqlitePool,
    opts: &ImportOptions,
    pr: &PrNode,
    item_id: &str,
    progress: &mut ImportProgress,
) -> crate::Result<()> {
    for review in &pr.reviews.nodes {
        if review.body.trim().is_empty() {
            continue;
        }
        let Some(db_id) = review.database_id else {
            continue;
        };
        // Prefix with `review-` so a review body and an inline comment
        // with the same databaseId can never collide in the dedupe
        // lookup (inline comments and reviews live in separate tables
        // on GitHub's side).
        let ext_id = format!(
            "review-{}",
            imported_external_id(&opts.repo, &opts.source_repo, db_id)
        );
        if comment_exists(db, &ext_id).await? {
            progress.comments_skipped += 1;
            continue;
        }
        // Top-level review bodies have no thread to resolve and no
        // ordered replies, so only the review's own reactions feed the
        // durability signal; everything else stays neutral.
        let signal = CommentDurabilitySignal::from_reaction_groups(&review.reaction_groups);
        crate::review_store::add_comment(
            db,
            AddCommentInput {
                review_item_id: item_id.to_owned(),
                external_comment_id: Some(ext_id),
                line_number: None,
                content: review.body.clone(),
                author: review.author.as_ref().map(|a| a.login.clone()),
                comment_url: review.url.clone(),
                thread_id: Some(db_id.to_string()),
                metadata: Some(github_comment_metadata_json(
                    None,
                    Some(&opts.source_repo),
                    &opts.repo,
                    actor_source_kind(review.author.as_ref()).as_deref(),
                    &signal,
                    review.author.as_ref(),
                )),
            },
        )
        .await?;
        progress.comments_imported += 1;
    }
    Ok(())
}

/// PR discussion comments. Maintainers often leave repeatable release,
/// packaging, or test-process feedback here instead of as inline review
/// comments. Anchor them to the first changed file so local candidate
/// drafting can still derive scoped file patterns.
async fn persist_discussion_comments(
    db: &SqlitePool,
    opts: &ImportOptions,
    pr: &PrNode,
    item_id: &str,
    file_path: &str,
    progress: &mut ImportProgress,
) -> crate::Result<()> {
    for comment in &pr.comments.nodes {
        if comment.body.trim().is_empty() {
            continue;
        }
        let Some(db_id) = comment.database_id else {
            continue;
        };
        let ext_id = format!(
            "issue-comment-{}",
            imported_external_id(&opts.repo, &opts.source_repo, db_id)
        );
        if comment_exists(db, &ext_id).await? {
            progress.comments_skipped += 1;
            continue;
        }
        // PR discussion comments aren't in a resolvable review thread, so
        // only their reactions contribute a durability signal.
        let signal = CommentDurabilitySignal::from_reaction_groups(&comment.reaction_groups);
        crate::review_store::add_comment(
            db,
            AddCommentInput {
                review_item_id: item_id.to_owned(),
                external_comment_id: Some(ext_id),
                line_number: None,
                content: comment.body.clone(),
                author: comment.author.as_ref().map(|a| a.login.clone()),
                comment_url: comment.url.clone(),
                thread_id: Some(format!("issue-comment-{db_id}")),
                metadata: Some(github_comment_metadata_json(
                    non_empty_path(Some(file_path)),
                    Some(&opts.source_repo),
                    &opts.repo,
                    Some("issue_comment"),
                    &signal,
                    comment.author.as_ref(),
                )),
            },
        )
        .await?;
        progress.comments_imported += 1;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::schema::{ReactionGroupNode, ReactionUsersNode, ReviewThreadNode};
    use super::*;

    #[test]
    fn reaction_groups_roll_up_into_thumbs_and_total() {
        let groups = vec![
            ReactionGroupNode {
                content: Some("THUMBS_UP".to_owned()),
                users: ReactionUsersNode { total_count: 3 },
            },
            ReactionGroupNode {
                content: Some("THUMBS_DOWN".to_owned()),
                users: ReactionUsersNode { total_count: 1 },
            },
            ReactionGroupNode {
                content: Some("HEART".to_owned()),
                users: ReactionUsersNode { total_count: 2 },
            },
        ];
        let signal = CommentDurabilitySignal::from_reaction_groups(&groups);
        assert_eq!(signal.thumbs_up, 3);
        assert_eq!(signal.thumbs_down, 1);
        assert_eq!(signal.reactions_total, 6);
    }

    #[test]
    fn older_api_shape_without_reaction_or_resolved_fields_degrades_gracefully() {
        // A review-thread node missing both `isResolved` and `reactionGroups`
        // (an older GitHub GraphQL shape) must still deserialize, defaulting
        // to the neutral signal rather than erroring.
        let json = r#"{ "comments": { "nodes": [ { "databaseId": 1, "body": "x" } ] } }"#;
        let thread: ReviewThreadNode = serde_json::from_str(json).unwrap();
        assert!(!thread.is_resolved);
        let comment = &thread.comments.nodes[0];
        assert!(comment.reaction_groups.is_empty());
        let signal = CommentDurabilitySignal::from_reaction_groups(&comment.reaction_groups);
        assert_eq!(signal.reactions_total, 0);
    }

    #[test]
    fn earlier_thread_authors_capture_reply_linkage_for_trust_classifier() {
        let thread: ReviewThreadNode = serde_json::from_str(
            r#"{
                "comments": { "nodes": [
                    { "databaseId": 1, "body": "Bot finding: validate first.",
                      "author": { "login": "coderabbitai[bot]" } },
                    { "databaseId": 2, "body": "   ",
                      "author": { "login": "blank-body" } },
                    { "databaseId": 3, "body": "Agreed, apply this everywhere.",
                      "author": { "login": "alice" } }
                ] }
            }"#,
        )
        .expect("thread fixture deserializes");

        // Thread starter has no earlier context.
        assert!(earlier_thread_authors(&thread.comments.nodes, 0).is_empty());
        // The human reply sees the bot author; the empty-body comment (never
        // imported) contributes no linkage.
        assert_eq!(
            earlier_thread_authors(&thread.comments.nodes, 2),
            vec!["coderabbitai[bot]".to_owned()]
        );
    }

    #[test]
    fn earlier_thread_source_kinds_preserve_graphql_bot_type_for_plain_login() {
        let thread: ReviewThreadNode = serde_json::from_str(
            r#"{
                "comments": { "nodes": [
                    { "databaseId": 1, "body": "Bot finding: validate first.",
                      "author": { "__typename": "Bot", "login": "review-assistant" } },
                    { "databaseId": 2, "body": "Agreed, apply this everywhere.",
                      "author": { "__typename": "User", "login": "alice" } }
                ] }
            }"#,
        )
        .expect("thread fixture deserializes");

        assert_eq!(
            earlier_thread_source_kinds(&thread.comments.nodes, 1),
            vec!["bot:review-assistant".to_owned()]
        );
    }

    #[test]
    fn drop_excluded_prs_removes_excluded_numbers_so_they_contribute_zero_rules() {
        // Build PrNodes through serde so the many defaulted fields fill in;
        // only `number` matters for the exclude filter.
        let pr = |number: i32| -> PrNode {
            serde_json::from_str(&format!(r#"{{ "number": {number} }}"#))
                .expect("PrNode deserializes from a number")
        };
        let mut collected = vec![pr(10), pr(20), pr(30)];

        let exclude: std::collections::HashSet<i32> = std::iter::once(20).collect();
        drop_excluded_prs(&mut collected, &exclude);

        let remaining: Vec<i32> = collected.iter().filter_map(|p| p.number).collect();
        assert_eq!(
            remaining,
            vec![10, 30],
            "excluded PR #20 must be dropped before its comments become candidates"
        );
        assert!(
            !remaining.contains(&20),
            "an excluded PR number must yield zero rules"
        );
    }

    #[test]
    fn drop_excluded_prs_is_a_noop_when_exclude_set_is_empty() {
        let pr = |number: i32| -> PrNode {
            serde_json::from_str(&format!(r#"{{ "number": {number} }}"#))
                .expect("PrNode deserializes")
        };
        let mut collected = vec![pr(1), pr(2)];
        drop_excluded_prs(&mut collected, &std::collections::HashSet::new());
        let remaining: Vec<i32> = collected.iter().filter_map(|p| p.number).collect();
        assert_eq!(
            remaining,
            vec![1, 2],
            "empty exclude set must keep every PR"
        );
    }

    #[test]
    fn item_metadata_carries_changed_file_paths_for_top_level_scope() {
        let pr: PrNode = serde_json::from_str(
            r#"{
                "number": 7,
                "files": {
                    "nodes": [
                        { "path": "src/http/request.rs" },
                        { "path": "src/http/request.rs" },
                        { "path": "web/components/Button.tsx" }
                    ]
                }
            }"#,
        )
        .expect("PR fixture deserializes");
        let opts = ImportOptions {
            repo: "acme/app".to_owned(),
            source_repo: "acme/app".to_owned(),
            project_id: "project-1".to_owned(),
            max_prs: 50,
            pr_numbers: Vec::new(),
            exclude_prs: std::collections::HashSet::new(),
            since: None,
            include_open: false,
        };

        let value: serde_json::Value = serde_json::from_str(
            &item_metadata_json(&opts, &pr).expect("changed paths produce metadata"),
        )
        .expect("metadata json");

        assert_eq!(
            value["changedFilePaths"],
            serde_json::json!(["src/http/request.rs", "web/components/Button.tsx"])
        );
        assert!(
            value.get("sourceRepoFullName").is_none(),
            "same-repo imports should not invent fork metadata"
        );
    }

    #[test]
    fn github_comment_metadata_marks_graphql_bot_actor_even_without_bot_login_shape() {
        let signal = CommentDurabilitySignal::default();
        let actor = ActorNode {
            type_name: Some("Bot".to_owned()),
            login: "review-assistant".to_owned(),
        };

        let metadata: serde_json::Value = serde_json::from_str(&github_comment_metadata_json(
            Some("src/http/request.rs"),
            Some("owner/repo"),
            "owner/repo",
            actor_source_kind(Some(&actor)).as_deref(),
            &signal,
            Some(&actor),
        ))
        .expect("metadata parses");

        assert_eq!(metadata["authorType"], "Bot");
        assert_eq!(metadata["authorIsBot"], true);
        assert_eq!(metadata["sourceKind"], "bot:review-assistant");
    }

    #[test]
    fn github_comment_metadata_omits_source_kind_when_author_type_is_unverified() {
        let signal = CommentDurabilitySignal::default();
        let actor = ActorNode {
            type_name: None,
            login: "some-plain-login".to_owned(),
        };

        let metadata: serde_json::Value = serde_json::from_str(&github_comment_metadata_json(
            Some("src/http/request.rs"),
            Some("owner/repo"),
            "owner/repo",
            actor_source_kind(Some(&actor)).as_deref(),
            &signal,
            Some(&actor),
        ))
        .expect("metadata parses");

        assert!(
            metadata.get("sourceKind").is_none(),
            "unverified author type must not bake a human claim into metadata"
        );
        assert!(metadata.get("authorType").is_none());
        assert!(metadata.get("authorIsBot").is_none());
    }
}
