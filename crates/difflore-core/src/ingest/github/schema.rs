//! GitHub GraphQL response shapes (wire format only).
//!
//! One paginated query returns merged PRs along with their reviews and
//! reviewThreads in O(N/page) calls, skipping empty PRs without extra HTTP
//! round-trips.
//!
//! These structs are pure deserialization glue; the logic that consumes them
//! lives in `parse.rs` and `mod.rs`.

use serde::Deserialize;

/// Shared accessor for the top-level `errors` array so the retry driver can
/// treat the search and direct-PR response shapes uniformly.
pub(super) trait GraphqlResponse {
    fn error_messages(&self) -> Vec<&str>;
}

#[derive(Debug, Deserialize)]
pub(super) struct GraphResponse {
    pub(super) data: Option<GraphData>,
    #[serde(default)]
    pub(super) errors: Vec<GraphError>,
}

impl GraphqlResponse for GraphResponse {
    fn error_messages(&self) -> Vec<&str> {
        self.errors.iter().map(|e| e.message.as_str()).collect()
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct GraphError {
    pub(super) message: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct GraphData {
    pub(super) search: Option<SearchConnection>,
}

#[derive(Debug, Deserialize)]
pub(super) struct DirectGraphResponse {
    pub(super) data: Option<DirectGraphData>,
    #[serde(default)]
    pub(super) errors: Vec<GraphError>,
}

impl GraphqlResponse for DirectGraphResponse {
    fn error_messages(&self) -> Vec<&str> {
        self.errors.iter().map(|e| e.message.as_str()).collect()
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct DirectGraphData {
    pub(super) repository: Option<RepositoryNode>,
}

#[derive(Debug, Deserialize)]
pub(super) struct RepositoryNode {
    #[serde(rename = "pullRequest")]
    pub(super) pull_request: Option<PrNode>,
}

#[derive(Debug, Deserialize)]
pub(super) struct SearchConnection {
    #[serde(rename = "pageInfo")]
    pub(super) page_info: PageInfo,
    /// Nodes are `... on PullRequest` fragments. Because our query string
    /// filters `is:pr`, every node has `PullRequest` shape — but we allow
    /// `Option` fields so stray non-PR nodes (if the filter ever leaks)
    /// simply deserialize to empty and get dropped.
    pub(super) nodes: Vec<PrNode>,
}

#[derive(Debug, Deserialize)]
pub(super) struct PageInfo {
    #[serde(rename = "hasNextPage")]
    pub(super) has_next_page: bool,
    #[serde(rename = "endCursor")]
    pub(super) end_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct PrNode {
    pub(super) number: Option<i32>,
    pub(super) author: Option<ActorNode>,
    #[serde(default = "empty_files")]
    pub(super) files: FilesConnection,
    #[serde(default = "empty_issue_comments")]
    pub(super) comments: IssueCommentsConnection,
    #[serde(default = "empty_reviews")]
    pub(super) reviews: ReviewsConnection,
    #[serde(rename = "reviewThreads", default = "empty_threads")]
    pub(super) review_threads: ReviewThreadsConnection,
}

const fn empty_reviews() -> ReviewsConnection {
    ReviewsConnection { nodes: vec![] }
}

const fn empty_files() -> FilesConnection {
    FilesConnection { nodes: vec![] }
}

const fn empty_issue_comments() -> IssueCommentsConnection {
    IssueCommentsConnection { nodes: vec![] }
}

const fn empty_threads() -> ReviewThreadsConnection {
    ReviewThreadsConnection { nodes: vec![] }
}

#[derive(Debug, Deserialize)]
pub(super) struct ActorNode {
    pub(super) login: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct FilesConnection {
    pub(super) nodes: Vec<FileNode>,
}

#[derive(Debug, Deserialize)]
pub(super) struct FileNode {
    pub(super) path: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct IssueCommentsConnection {
    pub(super) nodes: Vec<IssueCommentNode>,
}

#[derive(Debug, Deserialize)]
pub(super) struct IssueCommentNode {
    #[serde(rename = "databaseId")]
    pub(super) database_id: Option<i64>,
    pub(super) body: String,
    pub(super) author: Option<ActorNode>,
    pub(super) url: Option<String>,
    /// Correctness/durability signal: 👍/👎 aggregate. Defaults to a
    /// neutral (all-zero) shape so an older API response (or a node that
    /// omits `reactionGroups`) never fails to deserialize.
    #[serde(rename = "reactionGroups", default)]
    pub(super) reaction_groups: Vec<ReactionGroupNode>,
}

#[derive(Debug, Deserialize)]
pub(super) struct ReviewsConnection {
    pub(super) nodes: Vec<ReviewNode>,
}

#[derive(Debug, Deserialize)]
pub(super) struct ReviewNode {
    #[serde(rename = "databaseId")]
    pub(super) database_id: Option<i64>,
    pub(super) body: String,
    pub(super) author: Option<ActorNode>,
    pub(super) url: Option<String>,
    #[serde(rename = "reactionGroups", default)]
    pub(super) reaction_groups: Vec<ReactionGroupNode>,
}

#[derive(Debug, Deserialize)]
pub(super) struct ReviewThreadsConnection {
    pub(super) nodes: Vec<ReviewThreadNode>,
}

#[derive(Debug, Deserialize)]
pub(super) struct ReviewThreadNode {
    /// Adoption proxy: a resolved thread means a maintainer marked the
    /// discussion settled (almost always: the suggestion was applied).
    /// `default` keeps older API shapes that omit the field neutral
    /// (`false` → no positive adoption signal, never a crash).
    #[serde(rename = "isResolved", default)]
    pub(super) is_resolved: bool,
    pub(super) comments: ReviewCommentsConnection,
}

#[derive(Debug, Deserialize)]
pub(super) struct ReviewCommentsConnection {
    pub(super) nodes: Vec<ReviewCommentNode>,
}

#[derive(Debug, Deserialize)]
pub(super) struct ReviewCommentNode {
    #[serde(rename = "databaseId")]
    pub(super) database_id: Option<i64>,
    pub(super) body: String,
    pub(super) author: Option<ActorNode>,
    pub(super) path: Option<String>,
    pub(super) line: Option<i32>,
    pub(super) url: Option<String>,
    #[serde(rename = "pullRequestReview")]
    pub(super) pull_request_review: Option<ReviewRef>,
    #[serde(rename = "reactionGroups", default)]
    pub(super) reaction_groups: Vec<ReactionGroupNode>,
}

#[derive(Debug, Deserialize)]
pub(super) struct ReviewRef {
    #[serde(rename = "databaseId")]
    pub(super) database_id: Option<i64>,
}

/// One GitHub `ReactionGroup` (`content` + `users.totalCount`). We only
/// distinguish thumbs-up / thumbs-down from the rest; everything else
/// rolls up into the neutral total. All fields default so a partial or
/// older API response degrades to "no reactions" rather than failing.
#[derive(Debug, Default, Deserialize)]
pub(super) struct ReactionGroupNode {
    #[serde(default)]
    pub(super) content: Option<String>,
    #[serde(default)]
    pub(super) users: ReactionUsersNode,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct ReactionUsersNode {
    #[serde(rename = "totalCount", default)]
    pub(super) total_count: i64,
}
