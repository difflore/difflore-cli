use std::collections::HashMap;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextEngineRecord {
    #[serde(default = "default_context_enabled")]
    pub enabled: bool,
    #[serde(default = "default_context_auto_retrieve")]
    pub auto_retrieve: bool,
    #[serde(default = "default_max_rule_results")]
    pub max_rule_results: i32,
    #[serde(default = "default_rule_token_budget")]
    pub rule_token_budget: i32,
    #[serde(default = "default_allow_hosted_embeddings")]
    pub allow_hosted_embeddings: bool,
    /// Master switch for the real (non-SHA1) embedding provider.
    #[serde(default = "default_semantic_embedding")]
    pub semantic_embedding: bool,
    /// OpenAI-compatible base URL, e.g. `https://api.openai.com/v1`.
    #[serde(default)]
    pub embedding_provider_url: Option<String>,
    /// Keyring storage key for the provider's API key.
    ///
    /// This value is NOT the plaintext API key. It is an opaque identifier
    /// (an AES-GCM ciphertext hex blob produced by `crypto::encrypt_secret`)
    /// that is decrypted via `context::embedding::load_embedding_key` at
    /// use time. The actual API key is protected by a master key stored in
    /// the OS keyring.
    #[serde(default)]
    pub embedding_provider_key: Option<String>,
    /// Model name, e.g. `text-embedding-3-small`.
    #[serde(default)]
    pub embedding_model: Option<String>,
    /// Embedding dimension, e.g. 1536 for `text-embedding-3-small`.
    #[serde(default)]
    pub embedding_dim: Option<usize>,
}

const fn default_context_enabled() -> bool {
    true
}
const fn default_context_auto_retrieve() -> bool {
    true
}
const fn default_max_rule_results() -> i32 {
    4
}
const fn default_rule_token_budget() -> i32 {
    1500
}
const fn default_allow_hosted_embeddings() -> bool {
    true
}
const fn default_semantic_embedding() -> bool {
    true
}

impl Default for ContextEngineRecord {
    fn default() -> Self {
        Self {
            enabled: true,
            auto_retrieve: true,
            max_rule_results: 4,
            rule_token_budget: 1500,
            allow_hosted_embeddings: default_allow_hosted_embeddings(),
            semantic_embedding: default_semantic_embedding(),
            embedding_provider_url: None,
            embedding_provider_key: None,
            embedding_model: None,
            embedding_dim: None,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewEngineRecord {
    #[serde(default)]
    pub multi_perspective: bool,
    /// Recall past verdicts for similar code and inject them into the review
    /// prompt. Defaults to `true`; opt out via settings.
    #[serde(default = "default_past_verdict_recall")]
    pub past_verdict_recall: bool,
    /// Run a second cheap-model "self-check" pass over the merged issues to
    /// score confidence and drop obvious false positives. Defaults to `true`.
    #[serde(default = "default_true")]
    pub self_check_enabled: bool,
    /// Emit a one-line PR summary plus a per-file walkthrough on each review.
    /// Defaults to `true`.
    #[serde(default = "default_true")]
    pub review_summary_enabled: bool,
    /// Snap each issue's reported line to the exact new-file line range by
    /// matching against parsed diff hunks, instead of trusting the model's
    /// claimed line. Only ever sharpens a line number: when no hunk matches
    /// the claimed line is kept, so it never regresses attribution. Defaults
    /// to `true`; set `false` to use claimed lines only.
    #[serde(default = "default_true")]
    pub hunk_line_resolution: bool,
    /// After rule recall, ask the review LLM in one extra batched call
    /// whether each recalled rule applies to this diff, dropping the ones it
    /// judges non-applicable before they enter the prompt. Fired only at
    /// review time, never on the commit hook.
    ///
    /// Defaults to `false` (opt-in) because it adds a whole extra LLM call,
    /// the existing rerank + path-hint boost already prune most
    /// off-topic rules, and a flaky judge must never starve a review of its
    /// rules. Enabling it also widens the candidate pool — see
    /// `judge_candidate_pool_top_k`.
    #[serde(default)]
    pub rule_applicability_judge: bool,
}

const fn default_past_verdict_recall() -> bool {
    true
}
const fn default_true() -> bool {
    true
}

impl Default for ReviewEngineRecord {
    fn default() -> Self {
        Self {
            multi_perspective: false,
            past_verdict_recall: default_past_verdict_recall(),
            self_check_enabled: default_true(),
            review_summary_enabled: default_true(),
            hunk_line_resolution: default_true(),
            rule_applicability_judge: false,
        }
    }
}

/// Per-file walkthrough entry. `intent` is a one-sentence description of
/// what the file's diff is trying to accomplish.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileIntent {
    pub file: String,
    pub intent: String,
}

/// Review summary: one-line PR description, per-file walkthrough, and
/// blocking / non-blocking issue counts. Attached to `ReviewCheckResult`
/// as an `Option`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ReviewSummary {
    pub one_line_summary: String,
    pub walkthrough_by_file: Vec<FileIntent>,
    pub blocking_count: u32,
    pub non_blocking_count: u32,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSettingsRecord {
    #[serde(default)]
    pub proxy_enabled: bool,
    #[serde(default = "default_proxy_port")]
    pub proxy_port: i32,
    #[serde(default = "default_language")]
    pub language: String,
    #[serde(default = "default_theme")]
    pub theme: String,
    #[serde(default)]
    pub sound_notifications: bool,
    #[serde(default)]
    pub default_shell: Option<String>,
    #[serde(default = "default_workspace")]
    pub default_workspace: String,
    #[serde(default)]
    pub shortcuts: HashMap<String, String>,
    #[serde(default)]
    pub context_engine: ContextEngineRecord,
    #[serde(default)]
    pub review_engine: ReviewEngineRecord,
    /// Show the "install `DiffLore` into your agent" hint after local
    /// commands when an agent is detected but the MCP server isn't wired
    /// up. `true` = show (default), `false` = user has dismissed it.
    #[serde(default = "default_true")]
    pub hints_mcp: bool,

    /// Default mode for `difflore fix` when no flag is given. Stored on disk
    /// as `fixDefaultMode`.
    #[serde(default = "default_fix_default_mode", rename = "fixDefaultMode")]
    pub fix_default_mode: String,

    /// Whether `difflore cloud sync` should run automatically in the background
    /// after login. Stored on disk as `syncAuto`.
    #[serde(default, rename = "syncAuto")]
    pub sync_auto: bool,

    /// Whether commands that need cloud may auto-trigger a browser login
    /// flow. `false` (default) keeps the CLI quiet on shared / headless
    /// machines. Stored on disk as `cloudAutoLogin`.
    #[serde(default, rename = "cloudAutoLogin")]
    pub cloud_auto_login: bool,
}

const fn default_proxy_port() -> i32 {
    4000
}
fn default_language() -> String {
    "en".into()
}
fn default_theme() -> String {
    "dark".into()
}
fn default_workspace() -> String {
    "~/projects".into()
}
fn default_fix_default_mode() -> String {
    "apply".into()
}
impl Default for AppSettingsRecord {
    fn default() -> Self {
        Self {
            proxy_enabled: false,
            proxy_port: default_proxy_port(),
            language: default_language(),
            theme: default_theme(),
            sound_notifications: false,
            default_shell: None,
            default_workspace: default_workspace(),
            shortcuts: HashMap::new(),
            context_engine: ContextEngineRecord::default(),
            review_engine: ReviewEngineRecord::default(),
            hints_mcp: true,
            fix_default_mode: default_fix_default_mode(),
            sync_auto: false,
            cloud_auto_login: false,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RuntimeReadyEvent {
    pub runtime: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectRecord {
    pub id: String,
    pub name: String,
    pub path: String,
    pub git_branch: Option<String>,
    pub active_sessions: i32,
    pub total_sessions: Option<i32>,
    pub created_at: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddProjectInput {
    pub path: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoveProjectInput {
    pub id: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderRecord {
    pub id: String,
    pub name: String,
    pub base_url: String,
    pub api_key: Option<String>,
    pub model_mapping: HashMap<String, String>,
    pub is_active: bool,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProviderAddInput {
    pub name: String,
    pub base_url: String,
    pub model_mapping: HashMap<String, String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProviderUpdateInput {
    pub id: String,
    pub name: Option<String>,
    pub base_url: Option<String>,
    pub model_mapping: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProviderRemoveInput {
    pub id: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSetActiveInput {
    pub id: String,
    pub is_active: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillRecord {
    pub id: String,
    pub name: String,
    pub source: String,
    pub directory: String,
    pub version: String,
    pub description: String,
    pub r#type: String,
    pub engines: Vec<String>,
    pub tags: Vec<String>,
    pub trigger: Option<String>,
    pub check_prompt: Option<String>,
    pub repo_owner: Option<String>,
    pub repo_name: Option<String>,
    pub repo_branch: Option<String>,
    pub readme_url: Option<String>,
    pub enabled_for_codex: bool,
    pub enabled_for_claude: bool,
    pub enabled_for_gemini: bool,
    pub enabled_for_cursor: bool,
    pub installed_at: String,
    pub updated_at: String,
    pub enforcement: Option<String>,
    /// Input channel: `manual` | `conversation` | `pr_review` | `extracted`.
    /// Conversation-channel rules get a lower base confidence (0.6 vs 0.7).
    #[serde(default = "default_origin")]
    pub origin: String,
}

fn default_origin() -> String {
    "manual".to_owned()
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct InstallSkillInput {
    pub owner: String,
    pub repo: String,
    pub branch: String,
    pub directory: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RemoveSkillInput {
    pub id: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToggleSkillEngineInput {
    pub id: String,
    pub engine: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CreateLocalSkillInput {
    pub name: String,
    pub engines: Option<Vec<String>>,
    pub tags: Option<Vec<String>>,
    pub description: Option<String>,
    pub r#type: Option<String>,
    pub trigger: Option<String>,
    pub check_prompt: Option<String>,
    pub content: Option<String>,
}

/// Input for `skills::remember()`. Records a rule the user told an agent
/// (or themselves via CLI) to remember during a conversation. Stored
/// locally with `origin = 'conversation'`, base confidence 0.6, and
/// `published = false` until the user explicitly publishes it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RememberRuleInput {
    /// Short rule title (becomes the local rule name).
    pub title: String,
    /// What the rule is and why — full natural-language body. The agent
    /// should transcribe the user's own words; not summarise them away.
    pub body: String,
    /// Optional glob patterns the rule applies to (e.g. `["**/*.ts"]`).
    /// Empty = repo-wide rule. Non-empty values are path hints/evidence globs,
    /// used for ranking boosts rather than hard recall filtering.
    #[serde(default)]
    pub file_patterns: Option<Vec<String>>,
    /// Optional bad-code snippet the user pointed at (the offending pattern).
    pub bad_code: Option<String>,
    /// Optional good-code snippet the user proposed (the corrected version).
    pub good_code: Option<String>,
    /// Optional severity hint surfaced in the rule body. `low|medium|high`.
    pub severity: Option<String>,
    /// Optional memory kind. Defaults to `review_rule`; use `soft_preference`
    /// for lightweight context/preferences that should be always injected with
    /// a small budget rather than ranked as precision review rules.
    #[serde(default)]
    pub kind: Option<String>,
    /// Optional category. Soft preferences currently use the existing
    /// `workflow_preference|user_preference|project_context` vocabulary.
    #[serde(default)]
    pub category: Option<String>,
    /// Channel that recorded this — defaults to `conversation`. Tests and
    /// the CLI override to `manual` so the discount + audit-tag behaviour
    /// can be exercised explicitly.
    #[serde(default)]
    pub origin: Option<String>,
    /// Agent/client that captured the rule, when known (for example
    /// `mcp-server`, `claude-code`, or `cursor`). Kept separate from origin so
    /// provenance does not fragment origin-based ranking and stats.
    #[serde(default)]
    pub captured_by_client: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillRepoRecord {
    pub id: String,
    pub owner: String,
    pub name: String,
    pub branch: String,
    pub enabled: bool,
    pub created_at: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SkillRepoAddInput {
    pub owner: String,
    pub name: String,
    pub branch: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SkillRepoRemoveInput {
    pub id: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateConfidenceInput {
    pub skill_id: String,
    /// "accept" (+0.05) or "reject" (-0.1)
    pub signal: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddExampleInput {
    pub skill_id: String,
    pub bad_code: String,
    pub good_code: String,
    pub description: Option<String>,
    pub source: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListExamplesInput {
    pub skill_id: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RemoveExampleInput {
    pub id: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleExampleRecord {
    pub id: String,
    pub skill_id: String,
    pub bad_code: String,
    pub good_code: String,
    pub description: Option<String>,
    pub source: String,
    pub created_at: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitStatusInput {
    pub project_path: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitStatusRecord {
    pub branch: Option<String>,
    pub ahead: i32,
    pub behind: i32,
    pub files: Vec<GitFileStatusRecord>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitFileStatusRecord {
    pub path: String,
    pub status: String,
    pub additions: i32,
    pub deletions: i32,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitBranchesInput {
    pub project_path: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitBranchRecord {
    pub name: String,
    pub current: bool,
    pub remote: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitDiffInput {
    pub project_path: String,
    pub staged: Option<bool>,
    pub ref1: Option<String>,
    pub ref2: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffHunkRecord {
    pub header: String,
    pub body: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffContentRecord {
    pub file_path: String,
    pub hunks: Vec<DiffHunkRecord>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitCommitInput {
    pub project_path: String,
    pub message: String,
    /// Specific files to stage. If empty/None, stages all changes (`git add -A`).
    pub files: Option<Vec<String>>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditorOpenInput {
    pub project_path: String,
    pub editor: Option<String>,
    pub file_path: Option<String>,
    pub line: Option<u32>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FilesSearchInput {
    pub project_path: String,
    pub query: String,
    pub limit: Option<i32>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FilesReadInput {
    pub project_path: String,
    pub relative_path: String,
    pub start_line: Option<i32>,
    pub end_line: Option<i32>,
    pub max_bytes: Option<i32>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSearchResult {
    pub path: String,
    pub relative_path: String,
    pub is_directory: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileReadRecord {
    pub absolute_path: String,
    pub relative_path: String,
    pub content: String,
    pub language: Option<String>,
    pub line_count: i32,
    pub truncated: bool,
    pub sha256: Option<String>,
}

impl crate::domain::rule_view::RuleView for SkillRecord {
    fn id(&self) -> &str {
        &self.id
    }
    fn content(&self) -> &str {
        &self.description
    }
    fn origin(&self) -> &str {
        &self.origin
    }
    fn confidence(&self) -> Option<f64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::rule_view::RuleView;

    #[test]
    fn skill_record_implements_rule_view() {
        let s = SkillRecord {
            id: "id1".into(),
            name: "n".into(),
            source: "s".into(),
            directory: "d".into(),
            version: "0".into(),
            description: "body".into(),
            r#type: "review_standard".into(),
            engines: vec![],
            tags: vec![],
            trigger: None,
            check_prompt: None,
            repo_owner: None,
            repo_name: None,
            repo_branch: None,
            readme_url: None,
            enabled_for_codex: false,
            enabled_for_claude: false,
            enabled_for_gemini: false,
            enabled_for_cursor: false,
            installed_at: String::new(),
            updated_at: String::new(),
            enforcement: None,
            origin: "pr_review".into(),
        };
        assert_eq!(s.id(), "id1");
        assert_eq!(s.content(), "body");
        assert_eq!(s.origin(), "pr_review");
        assert_eq!(s.confidence(), None);
    }

    #[test]
    fn context_engine_defaults_enable_semantic_embeddings() {
        let context = ContextEngineRecord::default();

        assert!(context.allow_hosted_embeddings);
        assert!(context.semantic_embedding);
    }

    #[test]
    fn missing_embedding_flags_deserialize_to_enabled() {
        let settings: AppSettingsRecord = serde_json::from_value(serde_json::json!({
            "contextEngine": {
                "enabled": true,
                "autoRetrieve": true,
                "maxRuleResults": 4,
                "ruleTokenBudget": 1500
            }
        }))
        .expect("settings should deserialize with defaults");

        assert!(settings.context_engine.allow_hosted_embeddings);
        assert!(settings.context_engine.semantic_embedding);
    }

    #[test]
    fn hunk_line_resolution_defaults_on() {
        // Hunk-aware attribution must be ON by default so `difflore fix`
        // patches anchor on the exact changed line, not a token-overlap guess.
        assert!(ReviewEngineRecord::default().hunk_line_resolution);
    }

    #[test]
    fn hunk_line_resolution_defaults_on_for_pre_phase4_configs() {
        // Upgrade path: a persisted config that predates the flag (omits the
        // key entirely) must deserialize with the feature ON via the serde
        // default, not fall back to Rust's `bool::default()` (false).
        let rec: ReviewEngineRecord = serde_json::from_str("{}").unwrap();
        assert!(
            rec.hunk_line_resolution,
            "missing key must default to true (sharpens fix patches on upgrade)"
        );
        // And an explicit opt-out is still honoured.
        let off: ReviewEngineRecord =
            serde_json::from_str(r#"{"hunkLineResolution": false}"#).unwrap();
        assert!(!off.hunk_line_resolution);
    }

    #[test]
    fn rule_applicability_judge_defaults_off() {
        // Opt-in feature: adds an extra LLM round-trip, so the default review
        // path must stay byte-identical. Default + missing-key both = off.
        assert!(!ReviewEngineRecord::default().rule_applicability_judge);
        let rec: ReviewEngineRecord = serde_json::from_str("{}").unwrap();
        assert!(
            !rec.rule_applicability_judge,
            "missing key must default to false (no extra LLM call unless opted in)"
        );
        // Explicit opt-in is honoured.
        let on: ReviewEngineRecord =
            serde_json::from_str(r#"{"ruleApplicabilityJudge": true}"#).unwrap();
        assert!(on.rule_applicability_judge);
    }
}
