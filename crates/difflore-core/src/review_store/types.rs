use super::default_explainability_schema_version;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewItemRecord {
    pub id: String,
    pub session_id: Option<String>,
    pub project_id: Option<String>,
    pub file_path: String,
    pub diff_content: String,
    pub status: String,
    pub source: String,
    pub source_kind: String,
    pub external_review_id: Option<String>,
    pub repo_full_name: Option<String>,
    pub pr_number: Option<i32>,
    pub author: Option<String>,
    pub synced_at: Option<String>,
    pub metadata: Option<String>,
    pub created_at: String,
    pub reviewed_at: Option<String>,
}

impl ReviewItemRecord {
    pub fn explainability_metadata(&self) -> Option<ReviewExplainabilityMetadataRecord> {
        let metadata = self.metadata.as_deref()?;
        serde_json::from_str(metadata).ok()
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewCommentRecord {
    pub id: String,
    pub review_item_id: String,
    pub external_comment_id: Option<String>,
    pub line_number: Option<i32>,
    pub content: String,
    pub author: Option<String>,
    pub comment_url: Option<String>,
    pub thread_id: Option<String>,
    pub metadata: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewItemWithComments {
    #[serde(flatten)]
    pub item: ReviewItemRecord,
    pub comments: Vec<ReviewCommentRecord>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ReviewIssueSnippetRecord {
    pub severity: String,
    pub rule: String,
    pub rule_id: Option<String>,
    pub message: String,
    pub file: Option<String>,
    pub line: Option<i32>,
    pub suggestion: Option<String>,
    pub confidence: f32,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewExplainabilityMetadataRecord {
    #[serde(default = "default_explainability_schema_version")]
    pub schema_version: u8,
    #[serde(default)]
    pub matched_rule_ids: Vec<String>,
    #[serde(default)]
    pub matched_rule_titles: Vec<String>,
    pub prompt_tokens_estimate: i32,
    pub trace_id: String,
    pub issue_count: usize,
    #[serde(default)]
    pub summary: Option<crate::models::ReviewSummary>,
    #[serde(default)]
    pub top_issues: Vec<ReviewIssueSnippetRecord>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ReviewCommentMetadataRecord {
    pub severity: String,
    pub rule: String,
    pub rule_id: Option<String>,
    pub confidence: f32,
    pub suggestion: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewProjectInput {
    pub project_id: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewSourceInput {
    pub source: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddCommentInput {
    pub review_item_id: String,
    pub external_comment_id: Option<String>,
    pub line_number: Option<i32>,
    pub content: String,
    pub author: Option<String>,
    pub comment_url: Option<String>,
    pub thread_id: Option<String>,
    pub metadata: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnsureItemInput {
    pub id: Option<String>,
    pub session_id: Option<String>,
    pub project_id: String,
    pub file_path: String,
    pub diff_content: String,
    pub status: String,
    pub source: String,
    pub source_kind: String,
    pub external_review_id: Option<String>,
    pub repo_full_name: Option<String>,
    pub pr_number: Option<i32>,
    pub author: Option<String>,
    pub synced_at: Option<String>,
    pub metadata: Option<String>,
    pub reviewed_at: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateItemStatusInput {
    pub id: String,
    pub status: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewItemIdInput {
    pub id: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewCommentIdInput {
    pub id: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListWithCommentsInput {
    pub project_id: String,
}
