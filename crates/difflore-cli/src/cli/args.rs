use std::path::PathBuf;

use clap::{Args, ValueEnum};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum StatusLane {
    /// Show both local beta and production GA boundaries.
    All,
    /// Emphasize the local/design-partner beta lane.
    LocalBeta,
    /// Emphasize the production GA lane.
    ProductionGa,
}

impl StatusLane {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::LocalBeta => "local-beta",
            Self::ProductionGa => "production-ga",
        }
    }
}

#[derive(Args)]
pub(crate) struct FixCliArgs {
    /// Apply all high-confidence suggestions without prompting.
    #[arg(long)]
    pub(crate) yes: bool,

    /// Diff scope: `staged`, `worktree`, or `all` (auto-detect; default).
    #[arg(long, value_name = "SCOPE")]
    pub(crate) diff: Option<String>,

    /// Print which recalled memories produced each finding.
    #[arg(long, hide = true)]
    pub(crate) explain_rules: bool,

    /// Emit a Markdown report. Pass a file path, or `-` for stdout.
    #[arg(long, value_name = "PATH", hide = true)]
    pub(crate) report: Option<String>,

    /// Output applied-fix result as JSON. Requires `--yes`.
    #[arg(long, requires = "yes")]
    pub(crate) json: bool,

    /// Fix a GitHub PR locally. Accepts number, owner/repo#number, or PR URL.
    #[arg(long, value_name = "PR", conflicts_with = "diff")]
    pub(crate) pr: Option<String>,

    /// Checkout PR mode into this local branch name.
    #[arg(
        long,
        value_name = "NAME",
        requires = "pr",
        conflicts_with = "no_checkout",
        hide = true
    )]
    pub(crate) work_branch: Option<String>,

    /// In PR mode, analyze the current checkout instead of switching branches.
    #[arg(long, requires = "pr", hide = true)]
    pub(crate) no_checkout: bool,

    /// In PR mode, allow running with local uncommitted changes.
    #[arg(long, requires = "pr", hide = true)]
    pub(crate) allow_dirty: bool,

    /// In PR mode, skip uploading accepted-fix proof for local rehearsals.
    #[arg(long, requires = "pr", hide = true)]
    pub(crate) no_upload_acceptance: bool,

    /// Path to fix. Defaults to staged changes, then working tree.
    pub(crate) path: Option<PathBuf>,
}

#[derive(Args)]
pub(crate) struct ReviewCliArgs {
    /// Machine gate: exits 1 on high-confidence actionable findings. Never modifies files.
    #[arg(long)]
    pub(crate) ci: bool,

    /// With `--ci`, also fail on low-confidence suggestions. Requires `--ci`.
    #[arg(long, requires = "ci")]
    pub(crate) strict: bool,

    /// Diff scope: `staged`, `worktree`, or `all` (auto-detect; default).
    #[arg(long, value_name = "SCOPE")]
    pub(crate) diff: Option<String>,

    /// Print which recalled memories produced each finding.
    #[arg(long, hide = true)]
    pub(crate) explain_rules: bool,

    /// Emit a Markdown report. Pass a file path, or `-` for stdout.
    #[arg(long, value_name = "PATH", hide = true)]
    pub(crate) report: Option<String>,

    /// Output as JSON.
    #[arg(long)]
    pub(crate) json: bool,

    /// Review a GitHub PR. Accepts number, owner/repo#number, or PR URL.
    #[arg(long, value_name = "PR", conflicts_with = "diff")]
    pub(crate) pr: Option<String>,

    /// In PR mode, analyze the current checkout instead of switching branches.
    #[arg(long, requires = "pr", hide = true)]
    pub(crate) no_checkout: bool,

    /// In PR mode, allow running with local uncommitted changes.
    #[arg(long, requires = "pr", hide = true)]
    pub(crate) allow_dirty: bool,

    /// Path to review. Defaults to staged changes, then working tree.
    pub(crate) path: Option<PathBuf>,
}

#[derive(Args)]
pub(crate) struct SyncCliArgs {
    /// Pull-only: download cloud changes without pushing local edits.
    #[arg(long, conflicts_with = "push")]
    pub(crate) pull: bool,

    /// Push-only: upload local changes without pulling cloud updates.
    #[arg(long, conflicts_with = "pull")]
    pub(crate) push: bool,

    /// Show what would change without writing anywhere.
    #[arg(long)]
    pub(crate) dry_run: bool,

    /// Also upload raw observation_events activity. Skipped by default.
    #[arg(long)]
    pub(crate) include_observations: bool,

    /// Also upload raw session-mined memory candidates. Skipped by default.
    #[arg(long)]
    pub(crate) include_candidates: bool,

    /// Also upload raw imported-review, review-metric, and trajectory telemetry. Skipped by default.
    #[arg(long)]
    pub(crate) include_telemetry: bool,

    /// Output as JSON.
    #[arg(long)]
    pub(crate) json: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum ImportProviderArg {
    /// Import PR review history via the GitHub CLI (`gh`).
    Github,
    /// Import MR discussions via the GitLab REST API (PAT from `difflore auth gitlab`).
    Gitlab,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum ImportDistillArg {
    /// Prefer an installed local agent CLI, falling back to deterministic heuristics.
    Auto,
    /// Use deterministic local heuristics.
    Heuristic,
    /// Ask an installed local agent CLI to distill pending candidates.
    LocalAgent,
}

#[derive(Args)]
pub(crate) struct ImportReviewsCliArgs {
    /// Repo to import. Uses git remote when omitted.
    #[arg(long)]
    pub(crate) repo: Option<String>,

    /// Import from an upstream GitHub repo and attach memory to this repo.
    #[arg(long, value_name = "OWNER/REPO")]
    pub(crate) from_upstream: Option<String>,

    /// Review provider. Uses git remote detection when omitted.
    #[arg(long, value_enum, value_name = "PROVIDER")]
    pub(crate) provider: Option<ImportProviderArg>,

    /// Self-managed GitLab host (e.g. gitlab.corp.example). Implies `--provider gitlab`.
    #[arg(long, value_name = "HOST")]
    pub(crate) gitlab_host: Option<String>,

    /// Maximum number of PRs (GitLab: MRs) to import.
    #[arg(long, default_value_t = 50)]
    pub(crate) max_prs: usize,

    /// Import one PR number (GitLab: MR IID). Repeat for multiple.
    #[arg(long = "pr", value_name = "NUMBER")]
    pub(crate) pr_numbers: Vec<i32>,

    /// Comma-separated PR numbers to exclude from import.
    #[arg(long = "exclude-prs", value_name = "CSV", value_delimiter = ',')]
    pub(crate) exclude_prs: Vec<i32>,

    /// Only import PRs merged after this date (YYYY-MM-DD).
    #[arg(long)]
    pub(crate) since: Option<String>,

    /// Also include open PRs (default imports merged PRs only).
    #[arg(long)]
    pub(crate) include_open: bool,

    /// Upload imported reviews for cloud extraction instead of local drafting.
    #[arg(long)]
    pub(crate) upload: bool,

    /// Local distillation strategy when not uploading.
    #[arg(
        long,
        value_enum,
        default_value_t = ImportDistillArg::Auto,
        conflicts_with = "upload"
    )]
    pub(crate) distill: ImportDistillArg,

    /// Preview what would be imported without writing or uploading.
    #[arg(long)]
    pub(crate) dry_run: bool,

    /// Output as JSON.
    #[arg(long)]
    pub(crate) json: bool,

    /// Internal wall-clock cap for bounded background imports.
    #[arg(long, hide = true, value_name = "SECONDS")]
    pub(crate) wall_timeout_secs: Option<u64>,
}

#[derive(Args)]
pub(crate) struct LearnCliArgs {
    /// Optional note to capture alongside the recent session.
    #[arg(long)]
    pub(crate) note: Option<String>,

    /// Explicit transcript JSONL path. Defaults to the newest Claude Code transcript.
    #[arg(long, value_name = "PATH")]
    pub(crate) transcript: Option<PathBuf>,

    /// Explicit session id. Defaults to the transcript file stem.
    #[arg(long, value_name = "ID")]
    pub(crate) session: Option<String>,

    /// Agent transcript format/client name.
    #[arg(long, default_value = "claude-code", hide = true)]
    pub(crate) client: String,

    /// Output as JSON.
    #[arg(long)]
    pub(crate) json: bool,
}

#[derive(Args)]
pub(crate) struct InitCliArgs {
    /// Readiness preview only — never wires agents or runs provider setup.
    #[arg(long)]
    pub(crate) check: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum ExportFormatArg {
    /// `AGENTS.md` at the repo root (cross-agent convention; no engine filter).
    AgentsMd,
    /// `CLAUDE.md` at the repo root (only rules enabled for the claude engine).
    ClaudeMd,
    /// Both emitters.
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum MemoryPackageFormatArg {
    /// Infer from --output: .json writes one JSON file; otherwise writes a Markdown directory.
    Auto,
    /// Write one versioned JSON package file.
    Json,
    /// Write a directory package with manifest.json plus editable Markdown rule files.
    Markdown,
}

#[derive(Args)]
pub(crate) struct ExportCliArgs {
    /// Target format(s): `agents-md`, `claude-md`, or `all`. Repeatable.
    #[arg(long, value_enum, value_name = "FORMAT", default_values_t = [ExportFormatArg::All])]
    pub(crate) format: Vec<ExportFormatArg>,

    /// Print the export plan (create/update/unchanged/skipped) without writing.
    #[arg(long)]
    pub(crate) dry_run: bool,

    /// Output as JSON.
    #[arg(long)]
    pub(crate) json: bool,

    /// Skip Bad/Good example blocks to keep the export small.
    #[arg(long)]
    pub(crate) no_examples: bool,

    /// Export local rules only; exclude team/cloud-synced rules.
    #[arg(long)]
    pub(crate) local_only: bool,

    /// Cap export to the first N rules. Unlimited when omitted.
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u64).range(1..))]
    pub(crate) max_rules: Option<u64>,
}

#[derive(Args)]
pub(crate) struct RecallCliArgs {
    /// Recall intent text. Optional when `--diff` is set.
    pub(crate) intent: Option<String>,

    /// File path that would be edited (drives the `file_pattern` cascade).
    #[arg(long, value_name = "PATH")]
    pub(crate) file: Option<String>,

    /// Infer files (and rough intent) from the current `git diff --name-only`.
    #[arg(long)]
    pub(crate) diff: bool,

    /// Number of rules to return (1..=50).
    #[arg(long, default_value_t = 5)]
    pub(crate) top_k: usize,

    /// Output as JSON.
    #[arg(long)]
    pub(crate) json: bool,

    /// Show each rule's `file_patterns` and `source_repo`.
    #[arg(long)]
    pub(crate) verbose: bool,

    /// Print paste-ready Markdown; conflicts with `--json`.
    #[arg(long, conflicts_with = "json")]
    pub(crate) copy: bool,
}
