use sqlx::SqlitePool;
use std::time::Duration;

use crate::error::CoreError;
use crate::review_store::{AddCommentInput, EnsureItemInput};

mod parse;
mod schema;

use crate::ingest::common::{CommentDurabilitySignal, comment_exists, comment_metadata_json};
use parse::{drop_excluded_prs, imported_external_id};
use schema::{DirectGraphResponse, GraphResponse, PrNode};

// Public types

pub struct ImportOptions {
    /// Repo that imported review memory should attach to locally/cloud-side.
    pub repo: String,
    /// Repo to read PR review history from. Usually the same as `repo`, but
    /// fork workflows can import upstream review history while attaching the
    /// resulting memory to the user's fork.
    pub source_repo: String,
    pub project_id: String,
    pub max_prs: usize,
    pub pr_numbers: Vec<i32>,
    /// PR numbers to EXCLUDE from import. Dropped before their comments
    /// become candidates, so excluded PRs contribute zero rules. Enables
    /// leak-free recall evaluation. Empty in the common case.
    pub exclude_prs: std::collections::HashSet<i32>,
    pub since: Option<String>,
    pub upload_to_cloud: bool,
    /// When true, also pull open PRs (still gated by `-review:none`).
    /// Default false → only merged PRs are imported.
    pub include_open: bool,
}

// Progress counters live one level up so the GitLab importer can reuse them;
// re-exported here so existing `ingest::github::ImportProgress` paths hold.
pub use crate::ingest::{ImportProgress, ProgressCallback};

const GRAPHQL_SEARCH_PAGE_SIZE: usize = 30;
const GRAPHQL_MIN_SEARCH_PAGE_SIZE: usize = 5;

// GitHub CLI bridge

const GRAPHQL_QUERY: &str = r"
query($q: String!, $first: Int!, $after: String) {
  search(query: $q, type: ISSUE, first: $first, after: $after) {
    issueCount
    pageInfo { hasNextPage endCursor }
    nodes {
      ... on PullRequest {
        number
        title
        mergedAt
        author { login }
        files(first: 100) {
          nodes { path }
        }
        comments(first: 100) {
          nodes {
            databaseId
            body
            author { login }
            url
            reactionGroups { content users { totalCount } }
          }
        }
        reviews(first: 100) {
          nodes {
            databaseId
            body
            author { login }
            url
            reactionGroups { content users { totalCount } }
          }
        }
        reviewThreads(first: 100) {
          nodes {
            isResolved
            comments(first: 100) {
              nodes {
                databaseId
                body
                author { login }
                path
                line
                url
                pullRequestReview { databaseId }
                reactionGroups { content users { totalCount } }
              }
            }
          }
        }
      }
    }
  }
}
";

const DIRECT_PR_GRAPHQL_QUERY: &str = r"
query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      number
      title
      mergedAt
      author { login }
      files(first: 100) {
        nodes { path }
      }
      comments(first: 100) {
        nodes {
          databaseId
          body
          author { login }
          url
          reactionGroups { content users { totalCount } }
        }
      }
      reviews(first: 100) {
        nodes {
          databaseId
          body
          author { login }
          url
          reactionGroups { content users { totalCount } }
        }
      }
      reviewThreads(first: 100) {
        nodes {
          isResolved
          comments(first: 100) {
            nodes {
              databaseId
              body
              author { login }
              path
              line
              url
              pullRequestReview { databaseId }
              reactionGroups { content users { totalCount } }
            }
          }
        }
      }
    }
  }
}
";
const GH_API_TIMEOUT_SECS: u64 = 45;
const GRAPHQL_MAX_ATTEMPTS: usize = 4;

/// Build the `search` query string with `-review:none` so the server returns
/// only PRs that carry review activity. NOTE: `reviews:>0` is NOT a valid
/// GitHub search qualifier (it silently matches nothing); `-review:none` is
/// the supported way to say "has at least one review". `merged:>={since}`
/// pushes the `--since` filter server-side too.
fn build_search_query(repo: &str, since: Option<&str>, include_open: bool) -> String {
    // `is:merged` excludes open PRs; with `--include-open` we drop the
    // gate so the search returns merged + open PRs that have any review
    // activity. `merged:>={since}` only makes sense for closed PRs, so
    // we swap it for `updated:>={since}` in the open-included path.
    let mut q = if include_open {
        format!("repo:{repo} is:pr -review:none sort:updated-desc")
    } else {
        format!("repo:{repo} is:pr is:merged -review:none sort:updated-desc")
    };
    if let Some(since) = since {
        if include_open {
            q.push_str(&format!(" updated:>={since}"));
        } else {
            q.push_str(&format!(" merged:>={since}"));
        }
    }
    q
}

async fn run_gh_api(args: Vec<String>) -> crate::Result<std::process::Output> {
    let mut command = tokio::process::Command::new("gh");
    command.args(&args).kill_on_drop(true);
    tokio::time::timeout(Duration::from_secs(GH_API_TIMEOUT_SECS), command.output())
        .await
        .map_err(|_| {
            CoreError::Internal(format!(
                "GitHub CLI timed out after {GH_API_TIMEOUT_SECS}s: gh {}",
                args.join(" ")
            ))
        })?
        .map_err(|e| CoreError::Internal(format!("GitHub CLI failed: {e}")))
}

async fn run_graphql_page(
    query_string: &str,
    first: u32,
    after: Option<&str>,
) -> crate::Result<GraphResponse> {
    let mut args: Vec<String> = vec![
        "api".into(),
        "graphql".into(),
        "-f".into(),
        format!("query={GRAPHQL_QUERY}"),
        "-f".into(),
        format!("q={query_string}"),
        "-F".into(),
        format!("first={first}"),
    ];
    if let Some(cursor) = after {
        args.push("-f".into());
        args.push(format!("after={cursor}"));
    }

    let mut last_retryable_error: Option<String> = None;
    for attempt in 0..GRAPHQL_MAX_ATTEMPTS {
        let output = run_gh_api(args.clone()).await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let message = format!("gh api graphql error: {stderr}");
            if is_retryable_graphql_error(&message) && attempt + 1 < GRAPHQL_MAX_ATTEMPTS {
                last_retryable_error = Some(message);
                sleep_before_graphql_retry(attempt).await;
                continue;
            }
            return Err(CoreError::Internal(message));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let parsed: GraphResponse = serde_json::from_str(&stdout).map_err(|e| {
            CoreError::Internal(format!(
                "Failed to parse GraphQL response: {e}: {}",
                truncate_chars(&stdout, 200)
            ))
        })?;
        if parsed.errors.is_empty() {
            return Ok(parsed);
        }
        let msg = parsed
            .errors
            .iter()
            .map(|e| e.message.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        let message = format!("GraphQL errors: {msg}");
        if is_retryable_graphql_error(&message) && attempt + 1 < GRAPHQL_MAX_ATTEMPTS {
            last_retryable_error = Some(message);
            sleep_before_graphql_retry(attempt).await;
            continue;
        }
        return Err(CoreError::Internal(message));
    }

    Err(CoreError::Internal(last_retryable_error.unwrap_or_else(
        || "GitHub GraphQL request failed".to_owned(),
    )))
}

async fn run_graphql_pr(repo: &str, number: i32) -> crate::Result<Option<PrNode>> {
    let Some((owner, name)) = repo.split_once('/') else {
        return Err(CoreError::Internal(format!(
            "invalid GitHub repo '{repo}', expected owner/name"
        )));
    };
    let args: Vec<String> = vec![
        "api".into(),
        "graphql".into(),
        "-f".into(),
        format!("query={DIRECT_PR_GRAPHQL_QUERY}"),
        "-f".into(),
        format!("owner={owner}"),
        "-f".into(),
        format!("name={name}"),
        "-F".into(),
        format!("number={number}"),
    ];

    let mut last_retryable_error: Option<String> = None;
    for attempt in 0..GRAPHQL_MAX_ATTEMPTS {
        let output = run_gh_api(args.clone()).await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let message = format!("gh api graphql error: {stderr}");
            if is_missing_direct_pr_error(&message) {
                return Ok(None);
            }
            if is_retryable_graphql_error(&message) && attempt + 1 < GRAPHQL_MAX_ATTEMPTS {
                last_retryable_error = Some(message);
                sleep_before_graphql_retry(attempt).await;
                continue;
            }
            return Err(CoreError::Internal(message));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let parsed: DirectGraphResponse = serde_json::from_str(&stdout).map_err(|e| {
            CoreError::Internal(format!(
                "Failed to parse GraphQL PR response: {e}: {}",
                truncate_chars(&stdout, 200)
            ))
        })?;
        if parsed.errors.is_empty() {
            return Ok(parsed
                .data
                .and_then(|data| data.repository)
                .and_then(|repo| repo.pull_request));
        }
        let msg = parsed
            .errors
            .iter()
            .map(|e| e.message.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        let message = format!("GraphQL errors: {msg}");
        if is_missing_direct_pr_error(&message) {
            return Ok(None);
        }
        if is_retryable_graphql_error(&message) && attempt + 1 < GRAPHQL_MAX_ATTEMPTS {
            last_retryable_error = Some(message);
            sleep_before_graphql_retry(attempt).await;
            continue;
        }
        return Err(CoreError::Internal(message));
    }

    Err(CoreError::Internal(last_retryable_error.unwrap_or_else(
        || "GitHub GraphQL PR request failed".to_owned(),
    )))
}

/// UTF-8-safe truncation: take up to `max` Unicode scalar values. Slicing
/// `&s[..N]` panics if `N` lands inside a multi-byte char; using char
/// iteration keeps us boundary-aware regardless of input encoding.
fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

async fn sleep_before_graphql_retry(attempt: usize) {
    // 5s, 10s, 20s, 40s — gives GitHub secondary-rate-limit windows enough
    // breathing room to expire before the next attempt.
    let secs = 5_u64 * (1_u64 << attempt.min(3));
    tokio::time::sleep(Duration::from_secs(secs)).await;
}

fn is_retryable_graphql_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    // Fast-path rejection: auth failures should not retry.
    if lower.contains("bad credentials") || lower.contains("resource not accessible") {
        return false;
    }
    lower.contains("http 502")
        || lower.contains("http 503")
        || lower.contains("http 504")
        || lower.contains("bad gateway")
        || lower.contains("service unavailable")
        || lower.contains("server error")
        || lower.contains("gateway timeout")
        || lower.contains("connect timeout")
        || lower.contains("timed out")
        || lower.contains("connection reset")
        || lower.contains("connection refused")
        || lower.contains("connection attempt failed")
        || lower.contains("failed to respond")
        || lower.contains("temporarily unavailable")
        || lower.contains("secondary rate limit")
        || lower.contains("abuse detection mechanism")
        || lower.contains("something went wrong")
}

fn is_graphql_node_limit_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("possible nodes")
        || lower.contains("node limit")
        || lower.contains("exceeds the maximum number of nodes")
        || (lower.contains("graphql errors") && lower.contains("nodes"))
}

fn is_missing_direct_pr_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("could not resolve to a pullrequest")
        || lower.contains("could not resolve to pullrequest")
        || (lower.contains("pullrequest") && lower.contains("not found"))
}

// Core import logic

pub async fn import_pr_reviews(
    db: &SqlitePool,
    opts: ImportOptions,
    on_progress: Option<ProgressCallback>,
) -> Result<ImportProgress, CoreError> {
    which::which("gh")
        .map_err(|_| CoreError::Internal("GitHub CLI (gh) is not installed".into()))?;

    validate_owner_repo(&opts.repo)?;
    if !opts.source_repo.is_empty() {
        validate_owner_repo(&opts.source_repo)?;
    }

    let mut progress = ImportProgress {
        prs_fetched: 0,
        prs_total: 0,
        comments_imported: 0,
        comments_skipped: 0,
        prs_missing: 0,
        missing_pr_numbers: Vec::new(),
    };

    // Paginate via GitHub search. The query filters server-side, so empty
    // PRs never hit the wire. Keep the page size below GitHub's nested
    // GraphQL node-limit cliff; each PR can carry files, reviews, comments,
    // and review threads.
    let mut collected: Vec<PrNode> = Vec::new();
    if opts.pr_numbers.is_empty() {
        let search_query =
            build_search_query(&opts.source_repo, opts.since.as_deref(), opts.include_open);
        let mut cursor: Option<String> = None;
        while collected.len() < opts.max_prs {
            let remaining = opts.max_prs - collected.len();
            let mut page_size = remaining.min(GRAPHQL_SEARCH_PAGE_SIZE);
            let resp = loop {
                match run_graphql_page(&search_query, page_size as u32, cursor.as_deref()).await {
                    Ok(resp) => break resp,
                    Err(error)
                        if page_size > GRAPHQL_MIN_SEARCH_PAGE_SIZE
                            && is_graphql_node_limit_error(&error.to_string()) =>
                    {
                        page_size = (page_size / 2).max(GRAPHQL_MIN_SEARCH_PAGE_SIZE);
                    }
                    Err(error) => return Err(error),
                }
            };
            let Some(data) = resp.data else {
                return Err(CoreError::Internal("GraphQL response missing data".into()));
            };
            let Some(connection) = data.search else {
                return Err(CoreError::Internal(
                    "GraphQL response missing search field".into(),
                ));
            };

            // Drop any non-PR nodes defensively; `is:pr` keeps this empty in
            // practice but the search connection is polymorphic.
            collected.extend(connection.nodes.into_iter().filter(|n| n.number.is_some()));

            if !connection.page_info.has_next_page || connection.page_info.end_cursor.is_none() {
                break;
            }
            cursor = connection.page_info.end_cursor;
        }

        // Trim to the user-requested cap (search may overshoot on the last page).
        collected.truncate(opts.max_prs);
    } else {
        let mut seen = std::collections::HashSet::new();
        for number in &opts.pr_numbers {
            if !seen.insert(*number) {
                continue;
            }
            // Honor `--exclude-prs` in the direct-PR path too: skip the fetch
            // entirely so an excluded PR neither contributes rules nor counts
            // as missing.
            if opts.exclude_prs.contains(number) {
                continue;
            }
            if let Some(pr) = run_graphql_pr(&opts.source_repo, *number).await? {
                collected.push(pr);
            } else {
                progress.prs_missing += 1;
                progress.missing_pr_numbers.push(*number);
            }
        }
    }

    // Leak-free eval: drop excluded PRs BEFORE their comments are turned into
    // candidates so an excluded PR contributes zero rules. Applied to both the
    // search and direct-PR paths (the search path can't pre-filter by number).
    drop_excluded_prs(&mut collected, &opts.exclude_prs);

    // Server-side `-review:none` guarantees every returned PR has at least one
    // review object, but LGTM-style approvals can still be empty-bodied with
    // no inline threads. Those survive the server filter but carry no human
    // signal worth importing — drop them client-side so progress stays
    // honest.
    let filtered: Vec<&PrNode> = collected
        .iter()
        .filter(|pr| {
            let has_inline = pr
                .review_threads
                .nodes
                .iter()
                .any(|t| !t.comments.nodes.is_empty());
            let has_issue_comment = pr.comments.nodes.iter().any(|c| !c.body.trim().is_empty());
            let has_review_body = pr.reviews.nodes.iter().any(|r| !r.body.trim().is_empty());
            has_inline || has_review_body || has_issue_comment
        })
        .collect();

    progress.prs_total = filtered.len();
    if let Some(ref cb) = on_progress {
        cb(&progress);
    }

    // Persist each content-carrying PR
    for pr in &filtered {
        // Earlier filters guarantee `number` is present (non-PR search nodes
        // were dropped at collection time).
        let Some(pr_number) = pr.number else { continue };
        let item_id = format!("gh-import:{}#{}", opts.repo, pr_number);
        let source_metadata = if opts.source_repo == opts.repo {
            None
        } else {
            Some(
                serde_json::json!({
                    "sourceRepoFullName": &opts.source_repo,
                    "attachedRepoFullName": &opts.repo,
                })
                .to_string(),
            )
        };

        // Pick a representative file_path: first inline comment's path, or
        // fall back to the PR title so the review_item remains queryable.
        let file_path = pr
            .review_threads
            .nodes
            .iter()
            .flat_map(|t| t.comments.nodes.iter())
            .find_map(|c| c.path.clone())
            .or_else(|| {
                pr.files
                    .nodes
                    .iter()
                    .find_map(|f| (!f.path.trim().is_empty()).then(|| f.path.clone()))
            })
            .unwrap_or_else(|| pr.title.clone());

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

        // Inline diff comments. Within each thread the comments are ordered
        // oldest-first, so a comment's later replies are simply the tail of
        // the thread after it. We capture those plus the thread's resolved
        // state and the comment's reactions as a per-comment durability
        // signal — the local-candidate gate reads them back from metadata
        // to score capture confidence (resolved/approved = adopted;
        // a later "actually no" reply = contradiction).
        for thread in &pr.review_threads.nodes {
            for (idx, comment) in thread.comments.nodes.iter().enumerate() {
                let Some(db_id) = comment.database_id else {
                    continue;
                };
                let ext_id = imported_external_id(&opts.repo, &opts.source_repo, db_id);
                if comment.body.trim().is_empty() {
                    continue;
                }
                if comment_exists(db, &ext_id).await? {
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

                crate::review_store::add_comment(
                    db,
                    AddCommentInput {
                        review_item_id: item_id.clone(),
                        external_comment_id: Some(ext_id),
                        line_number: comment.line,
                        content: comment.body.clone(),
                        author: comment.author.as_ref().map(|a| a.login.clone()),
                        comment_url: comment.url.clone(),
                        thread_id,
                        metadata: Some(comment_metadata_json(
                            comment.path.as_deref(),
                            &opts.source_repo,
                            &opts.repo,
                            None,
                            &signal,
                        )),
                    },
                )
                .await?;
                progress.comments_imported += 1;
            }
        }

        // Top-level review bodies
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
            let metadata = signal.to_metadata_value().map(|v| v.to_string());
            crate::review_store::add_comment(
                db,
                AddCommentInput {
                    review_item_id: item_id.clone(),
                    external_comment_id: Some(ext_id),
                    line_number: None,
                    content: review.body.clone(),
                    author: review.author.as_ref().map(|a| a.login.clone()),
                    comment_url: review.url.clone(),
                    thread_id: Some(db_id.to_string()),
                    metadata,
                },
            )
            .await?;
            progress.comments_imported += 1;
        }

        // PR discussion comments. Maintainers often leave repeatable release,
        // packaging, or test-process feedback here instead of as inline review
        // comments. Anchor them to the first changed file so local candidate
        // drafting can still derive scoped file patterns.
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
                    review_item_id: item_id.clone(),
                    external_comment_id: Some(ext_id),
                    line_number: None,
                    content: comment.body.clone(),
                    author: comment.author.as_ref().map(|a| a.login.clone()),
                    comment_url: comment.url.clone(),
                    thread_id: Some(format!("issue-comment-{db_id}")),
                    metadata: Some(comment_metadata_json(
                        Some(&file_path),
                        &opts.source_repo,
                        &opts.repo,
                        Some("issue_comment"),
                        &signal,
                    )),
                },
            )
            .await?;
            progress.comments_imported += 1;
        }

        progress.prs_fetched += 1;
        if let Some(ref cb) = on_progress {
            cb(&progress);
        }
    }

    Ok(progress)
}

/// Auto-detect the `owner/repo` slug from the git remote origin URL.
pub fn detect_repo_from_remote(project_path: &str) -> Result<String, CoreError> {
    let output = std::process::Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(project_path)
        .output()?;

    if !output.status.success() {
        return Err(CoreError::Internal("No git remote 'origin' found".into()));
    }

    let url = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    parse_repo_from_url(&url).ok_or_else(|| {
        CoreError::Internal(format!("Could not parse owner/repo from remote URL: {url}"))
    })
}

/// Validate that a string is a syntactically well-formed `owner/repo`
/// GitHub identifier. We reject anything other than ASCII alphanumerics,
/// `.`, `_`, and `-` to avoid shelling out unvalidated input via `gh`.
fn validate_owner_repo(s: &str) -> crate::Result<()> {
    let mut parts = s.splitn(2, '/');
    let owner = parts.next().unwrap_or("");
    let repo = parts.next().unwrap_or("");
    if owner.is_empty() || repo.is_empty() {
        return Err(CoreError::Internal(format!(
            "invalid repo identifier {s:?}: expected owner/repo"
        )));
    }
    if repo.contains('/') {
        return Err(CoreError::Internal(format!(
            "invalid repo identifier {s:?}: expected owner/repo"
        )));
    }
    let valid = |s: &str| {
        s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
    };
    if !valid(owner) || !valid(repo) {
        return Err(CoreError::Internal(format!(
            "invalid repo identifier {s:?}: contains disallowed characters"
        )));
    }
    Ok(())
}

fn parse_repo_from_url(url: &str) -> Option<String> {
    if let Some(rest) = url.strip_prefix("git@github.com:") {
        let repo = rest.trim_end_matches(".git");
        if repo.contains('/') {
            return Some(repo.to_owned());
        }
    }
    if let Some(rest) = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))
    {
        let repo = rest.trim_end_matches(".git");
        if repo.contains('/') {
            return Some(repo.to_owned());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_repo_from_url_table() {
        let cases: &[(&str, Option<&str>)] = &[
            (
                "git@github.com:octocat/Hello-World.git",
                Some("octocat/Hello-World"),
            ),
            (
                "https://github.com/octocat/Hello-World.git",
                Some("octocat/Hello-World"),
            ),
            (
                "https://github.com/octocat/Hello-World",
                Some("octocat/Hello-World"),
            ),
            ("not-a-url", None),
        ];
        for (input, expected) in cases {
            assert_eq!(
                parse_repo_from_url(input),
                expected.map(String::from),
                "input: {input}"
            );
        }
    }

    #[test]
    fn retryable_graphql_error_detects_transient_github_failures() {
        assert!(is_retryable_graphql_error("gh: HTTP 502"));
        assert!(is_retryable_graphql_error(
            "GraphQL errors: Something went wrong"
        ));
        assert!(is_retryable_graphql_error("request timed out"));
        assert!(is_retryable_graphql_error(
            "connectex: A connection attempt failed because the connected party did not properly respond"
        ));
        assert!(!is_retryable_graphql_error(
            "GraphQL errors: Could not resolve to a Repository"
        ));
        assert!(!is_retryable_graphql_error("Bad credentials"));
    }

    #[test]
    fn graphql_node_limit_error_is_detected_separately_from_transients() {
        assert!(is_graphql_node_limit_error(
            "GraphQL errors: Query has 520,050 possible nodes; maximum is 500,000."
        ));
        assert!(is_graphql_node_limit_error(
            "gh api graphql error: query exceeds the maximum number of nodes"
        ));
        assert!(!is_graphql_node_limit_error("Bad credentials"));
        assert!(!is_retryable_graphql_error(
            "GraphQL errors: Query has 520,050 possible nodes; maximum is 500,000."
        ));
    }

    #[test]
    fn direct_pr_missing_errors_are_reportable_without_aborting_batch() {
        assert!(is_missing_direct_pr_error(
            "GraphQL errors: Could not resolve to a PullRequest with the number of 404."
        ));
        assert!(is_missing_direct_pr_error(
            "gh api graphql error: PullRequest not found"
        ));
        assert!(!is_missing_direct_pr_error(
            "GraphQL errors: Could not resolve to a Repository with the name 'missing'"
        ));
        assert!(!is_missing_direct_pr_error("Bad credentials"));
    }
}
