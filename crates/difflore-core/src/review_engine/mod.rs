mod diff_context;
mod parse;
mod pipeline;
mod prompts;
mod providers;

pub use diff_context::{
    DiffContextFile, DiffContextFileChange, DiffContextMode, DiffContextOptions,
    DiffContextSummary, DiffContextSummaryReason, PackedDiffContext, PackedDiffFile,
    pack_diff_context,
};
pub use pipeline::{
    ReviewEngine, merge_perspective_issues, run_review, run_review_multi,
    run_review_multi_with_trajectory, run_review_smart, run_review_with_trajectory,
    select_review_mode,
};
pub use prompts::{SegmentedPrompt, TeamRuleDigest, build_segmented_prompt};
pub use providers::{AGENT_CLI_SCHEME, agent_cli_sentinel};

use gate4agent::CliTool;
use providers::call_ai_provider;

/// One-shot completion against the user's active LLM provider.
///
/// Cheaper escape hatch for callers (like `difflore fix`'s patch
/// generator) that need a single round-trip without setting up the full
/// review pipeline. Looks up the active provider from the DB, decrypts
/// its API key, and dispatches through the same provider matrix the
/// review uses (Anthropic native / agent CLI / OpenAI-compatible).
///
/// Returns the raw text response — callers parse / validate further.
pub async fn complete_with_active_provider(
    db: &sqlx::SqlitePool,
    system_prompt: &str,
    user_prompt: &str,
) -> crate::Result<String> {
    // Mirror the review pipeline's engine resolution so this helper
    // honors the same agent-CLI fallback when no HTTP provider is
    // configured (or active). Without this, fix's patch generator
    // hard-fails on a clean install while review itself works fine.
    let engine = pipeline::resolve_review_engine(db).await?;
    let (provider_name, base_url, api_key, model) = match engine {
        ReviewEngine::HttpProvider {
            provider_name,
            base_url,
            api_key,
            model,
        } => (provider_name, base_url, api_key, model),
        ReviewEngine::AgentCli { tool, model } => {
            // call_ai_provider routes to gate4agent when base_url is an
            // agent-cli sentinel; `model` is forwarded as the CLI's
            // --model flag (or its per-tool equivalent).
            let provider_name = match tool {
                CliTool::ClaudeCode => "claude-cli",
                CliTool::Codex => "codex-cli",
                CliTool::Gemini => "gemini-cli",
                CliTool::OpenCode => "opencode-cli",
            };
            (
                provider_name.to_owned(),
                agent_cli_sentinel(tool).to_owned(),
                String::new(),
                model,
            )
        }
    };
    call_ai_provider(
        &provider_name,
        &base_url,
        &api_key,
        &model,
        system_prompt,
        user_prompt,
    )
    .await
}

// ── Perspectives ──

/// Review perspective used to specialize the system prompt for a pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReviewPerspective {
    Safety,
    Performance,
    Style,
    Docs,
    ApiDesign,
}

impl ReviewPerspective {
    /// Stable snake-case identifier used in logs / metadata.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Safety => "safety",
            Self::Performance => "performance",
            Self::Style => "style",
            Self::Docs => "docs",
            Self::ApiDesign => "api_design",
        }
    }

    /// Extra instructions appended to the base system prompt when this
    /// perspective is active. Each addendum stays narrowly focused so the
    /// LLM does not re-discover the same issues across passes.
    pub const fn system_prompt_addendum(self) -> &'static str {
        match self {
            Self::Safety => {
                "\n\n## Perspective: Safety\n\
                 Focus exclusively on safety, security and correctness concerns: \
                 unsafe code, injection, auth/authorization, input validation, \
                 memory safety, null/undefined dereferences, panics, data races, \
                 secrets exposure, and crash-causing error handling. \
                 Do NOT report performance or style nits."
            }
            Self::Performance => {
                "\n\n## Perspective: Performance\n\
                 Focus exclusively on performance and resource-usage concerns: \
                 algorithmic complexity, unnecessary allocations, N+1 queries, \
                 blocking calls on hot paths, excessive clones, cache-unfriendly \
                 access patterns, and memory footprint. \
                 Do NOT report safety bugs or style nits."
            }
            Self::Style => {
                "\n\n## Perspective: Style\n\
                 Focus exclusively on style, readability, idioms and maintainability: \
                 naming, dead code, duplication, API ergonomics, formatting, \
                 documentation gaps, and convention adherence. \
                 Do NOT report safety bugs or performance issues."
            }
            Self::Docs => {
                "\n\n## Perspective: Docs\n\
                 Focus exclusively on documentation completeness and accuracy: \
                 missing or outdated doc comments, absent public-API rustdoc / \
                 jsdoc / docstrings, unclear naming that needs explanatory \
                 commentary, README drift from actual behavior, and examples \
                 that no longer compile or match the current API. \
                 Do NOT report safety, performance, or style issues."
            }
            Self::ApiDesign => {
                "\n\n## Perspective: ApiDesign\n\
                 Focus exclusively on public-API design quality: \
                 surface-area bloat, leaky abstractions, inconsistent \
                 naming/casing across the API, footguns (easy-to-misuse \
                 signatures), breaking-change risk on stable interfaces, \
                 missing builder patterns where they would reduce \
                 argument-order mistakes, and return types that should be \
                 enums or `Result` instead of `bool` / `Option`. \
                 Do NOT report safety, performance, style, or docs issues."
            }
        }
    }

    /// All perspectives, in the order they should be executed.
    pub const fn all() -> [Self; 5] {
        [
            Self::Safety,
            Self::Performance,
            Self::Style,
            Self::Docs,
            Self::ApiDesign,
        ]
    }
}

// ── Types ──

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewCheckInput {
    pub project_id: String,
    pub diff_content: String,
    pub file_path: Option<String>,
    /// Every file path touched by `diff_content`, when the caller knows the
    /// authoritative list (e.g. `difflore fix` from its diff records). Rule
    /// retrieval scopes the strict file-pattern cascade to this changeset
    /// (ANY-path match) instead of collapsing onto `file_path`. When empty,
    /// the pipeline falls back to parsing `+++` headers from `diff_content`
    /// — important for PR diffs packed under a char budget, where parsing
    /// the packed text would miss summarised-away files.
    #[serde(default)]
    pub diff_files: Vec<String>,
    pub engine: Option<String>,
    /// Cloud-side `pr_reviews` row id, when the caller already created
    /// one (e.g. the VS Code extension host after hitting the cloud
    /// `createReview` endpoint). When `Some`, `run_review_smart` collects
    /// trajectory, wall-clock, and token-estimate metrics and posts them to the
    /// cloud after the review completes. When `None`, cloud telemetry is skipped.
    #[serde(default)]
    pub review_id: Option<String>,
    /// GitHub `owner/repo` for this project. Scopes past-verdict recall
    /// to THIS repo's rules so the slogan "make AI understand your repo better" holds:
    /// a diff from repo X should retrieve rules learned from repo X, not
    /// unrelated repos the user has also indexed. `None` means no
    /// repo-scoped runtime recall; callers should populate it from
    /// `git remote get-url origin` whenever possible.
    #[serde(default)]
    pub repo_full_name: Option<String>,
    /// Additional same-project repo scopes discovered from git remotes.
    /// This lets forked worktrees retrieve rules learned from their
    /// upstream repository while still avoiding unrelated projects.
    #[serde(default)]
    pub repo_full_name_aliases: Vec<String>,
    /// Latency-sensitive preview mode. Callers such as `difflore fix --preview`
    /// need the first useful findings or a diagnostic quickly, so the review
    /// pipeline skips secondary recall/verification/summary passes.
    #[serde(default)]
    pub fast_preview: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewIssueRecord {
    pub severity: String,
    pub rule: String,
    pub rule_id: Option<String>,
    pub message: String,
    pub file: Option<String>,
    pub line: Option<i32>,
    pub suggestion: Option<String>,
    pub source_badge: Option<String>,
    /// Perspectives whose pass flagged this issue. Empty for single-pass
    /// reviews; populated by `review_diff_multi` / merge.
    #[serde(default)]
    pub perspectives: Vec<String>,
    /// Self-check confidence score. Range `[0.0, 1.0]`; higher is more
    /// confident the issue is a true positive. Defaults to `1.0` so older JSON
    /// records deserialize unchanged.
    #[serde(default = "default_confidence")]
    pub confidence: f32,
}

pub(crate) const fn default_confidence() -> f32 {
    1.0
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewCheckResult {
    pub issues: Vec<ReviewIssueRecord>,
    pub matched_rules: i32,
    pub matched_rule_ids: Vec<String>,
    pub matched_rule_titles: Vec<String>,
    pub prompt_tokens_estimate: i32,
    pub trace_id: String,
    /// Optional one-line summary, per-file walkthrough, and blocking counts.
    #[serde(default)]
    pub summary: Option<crate::domain::models::ReviewSummary>,
    /// Per-review stats surfaced to the IDE.
    #[serde(default)]
    pub stats: Option<ReviewStats>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ReviewStats {
    pub input_tokens: u32,
    #[serde(default)]
    pub duration_ms: Option<u64>,
    pub perspective_count: u32,
    pub past_verdicts_used: u32,
    #[serde(default)]
    pub trajectory_step_count: Option<u32>,
}

// Self-check and review summary.

/// Thin seam around `call_ai_provider` used by `verify_pass` and
/// `run_review_summary` so tests can inject canned responses without
/// spinning up a real HTTP client.
#[async_trait::async_trait]
pub trait ReviewLlm: Send + Sync {
    /// Send a system + user prompt and return the raw model response.
    async fn chat(&self, system_prompt: &str, user_prompt: &str) -> crate::Result<String>;
}

/// Production `ReviewLlm` impl that dispatches to the Anthropic native
/// Messages API when the provider base URL points at `api.anthropic.com`,
/// and falls through to the OpenAI-compatible `chat/completions` path
/// otherwise.
pub struct HttpReviewLlm {
    pub provider_name: String,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
}

#[async_trait::async_trait]
impl ReviewLlm for HttpReviewLlm {
    async fn chat(&self, system_prompt: &str, user_prompt: &str) -> crate::Result<String> {
        call_ai_provider(
            &self.provider_name,
            &self.base_url,
            &self.api_key,
            &self.model,
            system_prompt,
            user_prompt,
        )
        .await
    }
}

/// Local agent-CLI `ReviewLlm` impl. Drives one of `claude` / `codex` /
/// `gemini` / `opencode` through `gate4agent` and collects the streamed
/// assistant text. Used when no HTTP provider is active.
pub struct AgentCliReviewLlm {
    pub tool: CliTool,
    pub model: String,
}

#[async_trait::async_trait]
impl ReviewLlm for AgentCliReviewLlm {
    async fn chat(&self, system_prompt: &str, user_prompt: &str) -> crate::Result<String> {
        providers::call_agent_cli_provider(self.tool, &self.model, system_prompt, user_prompt).await
    }
}

#[cfg(test)]
mod tests;
