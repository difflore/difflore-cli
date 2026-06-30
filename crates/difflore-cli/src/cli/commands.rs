use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

use super::args::{
    ExportCliArgs, FixCliArgs, ImportReviewsCliArgs, InitCliArgs, LearnCliArgs,
    MemoryPackageFormatArg, RecallCliArgs, ReviewCliArgs, StatusLane, SyncCliArgs,
};

#[derive(Parser)]
#[command(name = "difflore")]
#[command(bin_name = "difflore")]
#[command(about = "Source-backed team rules for local coding agents")]
#[command(next_line_help = true)]
#[command(
    long_about = "DiffLore turns your team's past PR review judgment into local memory \
your AI agents can recall before they code. The core loop is: \
`difflore init`, `difflore import-reviews`, `difflore agents install`, then \
`difflore recall --diff` or `difflore review --diff all`. Background memory \
autopilot handles high-confidence local memory automatically; use \
`difflore memory`, `difflore memory review`, and `difflore memory log` to \
inspect and decide. Cloud sync is optional."
)]
pub(crate) struct Cli {
    /// Bare `difflore` shows local memory status and the next command.
    #[command(subcommand)]
    pub(crate) command: Option<Commands>,

    /// Disable interactive prompts in commands that support them.
    #[arg(long, global = true)]
    pub(crate) no_interactive: bool,
}

#[derive(Subcommand)]
pub(crate) enum Commands {
    /// See DiffLore work on a bundled sample — no setup, no repo, nothing written.
    #[command(
        long_about = concat!(
            "Run a zero-config demo with bundled review memory and a sample edit.\n",
            "It shows which memories fire and the next command to run.\n",
            "Nothing leaves your laptop and nothing is written to disk."
        )
    )]
    Try,

    /// Run first-time setup for this repo.
    Init(InitCliArgs),

    /// Show local memory status and the next command.
    Status {
        /// Output as JSON.
        #[arg(long)]
        json: bool,

        /// Filter readiness output to a release stage.
        #[arg(long, value_enum, default_value_t = StatusLane::All, hide = true)]
        lane: StatusLane,
    },

    /// Print the stable AI-facing CLI/MCP capability contract.
    Capabilities {
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Import past PR/MR review comments (GitHub, GitLab) as source-backed rule evidence.
    #[command(
        next_line_help = false,
        long_about = concat!(
            "Import past GitHub PR or GitLab MR review comments into local rules.\n",
            "The provider is auto-detected from the git remote.\n",
            "For self-managed GitLab, pass `--provider gitlab --gitlab-host <HOST>`\n",
            "or store a PAT with `difflore auth gitlab --host <HOST>`.\n",
            "Use `--dry-run` to preview; then run `difflore recall --diff`."
        )
    )]
    ImportReviews(ImportReviewsCliArgs),

    /// Review and approve the rules DiffLore has learned.
    Memory {
        /// Output the compact memory summary as JSON.
        #[arg(long)]
        json: bool,

        #[command(subcommand)]
        command: Option<MemoryCommands>,
    },

    /// Force DiffLore to learn from the latest session now.
    #[command(
        long_about = concat!(
            "Run the session-mining gate immediately over the latest Claude Code transcript, ",
            "optionally with a note. Learned items are queued as draft candidates and still ",
            "require normal review/approval."
        )
    )]
    Learn(LearnCliArgs),

    /// Preview which team rules an agent would see for an intent or current diff.
    Recall(RecallCliArgs),

    /// Review the current diff using source-backed team review judgment.
    #[command(
        next_line_help = false,
        long_about = concat!(
            "Analyze staged, working-tree, or PR changes against team review memory.\n",
            "Review never modifies files.\n",
            "Use `difflore review --ci` for a machine gate that exits non-zero on actionable findings.\n",
            "Use `difflore fix` when you want to apply suggested patches."
        )
    )]
    Review(ReviewCliArgs),

    /// Apply local patches using source-backed team review judgment.
    #[command(
        next_line_help = false,
        long_about = concat!(
            "Apply safe local patches for the current diff or a GitHub PR.\n",
            "Start with `difflore review --diff all` when you only want analysis.\n",
            "Accepted changes only touch the working tree.\n",
            "DiffLore never commits, pushes, opens PRs, or posts GitHub comments."
        )
    )]
    Fix(FixCliArgs),

    /// Export this repo's team rules into static agent context files.
    #[command(
        next_line_help = false,
        long_about = concat!(
            "Write recalled rules into AGENTS.md, CLAUDE.md, or both.\n\n",
            "Export is a static snapshot: it goes stale and cannot match the file being edited.\n",
            "Prefer `difflore agents install` for live, diff-aware injection.\n\n",
            "Side effects: writes only selected files and only inside DiffLore markers.\n",
            "Only the BEGIN/END DIFFLORE RULES block is managed.\n",
            "DiffLore never commits, pushes, or edits .gitignore."
        )
    )]
    Export(ExportCliArgs),

    /// Ask the team's source-backed rules a natural-language question.
    Ask {
        /// The question to ask.
        query: String,

        /// File path for scoping (optional; drives file-pattern recall).
        #[arg(long, value_name = "PATH")]
        file: Option<String>,

        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Compatibility alias for local memory draft review.
    #[command(hide = true)]
    Drafts {
        #[command(subcommand)]
        command: DraftsCommands,
    },

    /// Optional: sync team state and selected cloud queues.
    Cloud {
        #[command(subcommand)]
        command: CloudCommands,
    },

    /// Store GitLab credentials used for review import.
    Auth {
        #[command(subcommand)]
        command: AuthCommands,
    },

    /// Connect DiffLore to your local coding agents.
    Agents {
        #[command(subcommand)]
        command: AgentsCommands,
    },

    /// Refresh installed agent blocks and run diagnostics.
    #[command(
        long_about = concat!(
            "Refresh DiffLore installs safely.\n",
            "Shows binary update guidance when available.\n",
            "Re-renders unchanged agent config and hook blocks, then runs `doctor`."
        )
    )]
    Update {
        /// Preview agent block changes without touching disk; skips doctor.
        #[arg(long)]
        dry_run: bool,

        /// Overwrite agent blocks that were locally edited since DiffLore wrote them.
        #[arg(long)]
        force: bool,
    },

    /// Choose the local AI backend DiffLore uses for fixes.
    Providers {
        #[command(subcommand)]
        command: ProviderCommands,
    },

    /// Configure optional semantic search for higher-quality recall.
    Embeddings {
        #[command(subcommand)]
        command: EmbeddingsCommands,
    },

    /// Run an internal retrieval sanity check.
    #[command(
        hide = true,
        long_about = "Run an internal retrieval sanity check. Deterministic and offline; \
nothing is written to your real indexes. Not a published benchmark or competitive comparison."
    )]
    Eval {
        /// Number of rules to sample (1..=200). Ignored with `--golden`.
        #[arg(long, value_name = "N")]
        samples: Option<usize>,

        /// Score the committed golden-case fixture (paraphrase recall,
        /// precision, forbidden-exclusion, abstention) instead of self-recall.
        /// Offline and deterministic; needs no local corpus.
        #[arg(long)]
        golden: bool,

        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Replay a recorded review's decision trail — every issue traced to its memory evidence.
    #[command(
        hide = true,
        long_about = "Replay one recorded review decision trail from DiffLore Cloud. \
Pass `--json` for the raw document."
    )]
    Trajectory {
        /// The review id (UUID) to replay. Shown after a review runs.
        #[arg(value_name = "REVIEW_ID")]
        review_id: String,

        /// Output the raw trajectory document as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Show readiness and blockers; pass `--report` for a full diagnostic file.
    Doctor {
        /// Write a redacted support report. With no value, writes under
        /// `~/.difflore/reports/`; pass `-` for stdout or a path for a file.
        #[arg(
            long,
            value_name = "PATH",
            num_args = 0..=1,
            default_missing_value = ""
        )]
        report: Option<String>,

        /// Auto-repair the safe subset of detected problems.
        #[arg(long)]
        fix: bool,

        /// Preview retrying uploads that stopped after a login/auth outage.
        #[arg(long, hide = true)]
        drain_abandoned: bool,

        /// Only include uploads whose last attempt is older than this duration.
        /// Accepts `30d`, `7d`, `24h`, `1h`, `30m`.
        #[arg(long, value_name = "DURATION", default_value = "30d", hide = true)]
        older_than: String,

        /// Apply the retry queue changes instead of previewing them.
        #[arg(long, default_value_t = false, hide = true)]
        no_dry_run: bool,

        /// JSON output for the retry summary. Has no effect outside
        /// `--drain-abandoned`.
        #[arg(long, hide = true)]
        json: bool,
    },

    /// Internal MCP stdio transport used by installed agents.
    #[command(name = "mcp-server", hide = true)]
    McpServer,

    /// Internal warm hook-forward daemon for one project (spawned by the shim).
    #[command(name = "__hook-daemon", hide = true)]
    HookDaemon {
        /// Stable per-project hash selecting the index pool this daemon serves.
        #[arg(long, value_name = "HASH")]
        project_hash: String,
    },

    /// Internal cloud-outbox drain daemon (spawned best-effort by hooks).
    #[command(name = "__outbox-daemon", hide = true)]
    OutboxDaemon {
        /// Seconds between background drain passes.
        #[arg(long, default_value_t = 5, hide = true)]
        tick_interval_secs: u64,

        /// Maximum cloud_outbox rows claimed per pass.
        #[arg(long, default_value_t = 64, hide = true)]
        batch_size: usize,
    },

    /// Local skill-store maintenance utilities.
    #[command(hide = true)]
    Skills {
        #[command(subcommand)]
        command: SkillsCommands,
    },

    /// Verify plugin distribution manifests (maintainer-only release guardrail).
    #[command(hide = true)]
    Dist {
        #[command(subcommand)]
        command: DistCommands,
    },
}

#[derive(Subcommand)]
pub(crate) enum SkillsCommands {
    /// Preview cleanup for stale local memory records.
    Sweep {
        /// Apply the cleanup. Without this flag, prints a preview only.
        #[arg(long, default_value_t = false)]
        no_dry_run: bool,
        /// Confidence multiplier applied during cleanup.
        #[arg(long, default_value_t = 0.5)]
        decay_factor: f32,
        /// Ignore records newer than this many days.
        #[arg(long, default_value_t = 14)]
        days: u32,
        /// Also quarantine unguided review records.
        #[arg(long, default_value_t = false)]
        quarantine_unguided: bool,
    },

    /// Preview repair for old accepted-fix attribution records.
    #[command(name = "backfill-attribution")]
    BackfillAttribution {
        /// Apply the repair. Without this flag, prints a preview only.
        #[arg(long, default_value_t = false)]
        no_dry_run: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum DistCommands {
    /// Check plugin/marketplace manifests against the CLI version and bundle.
    Verify {
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum DraftsCommands {
    /// List pending memory drafts.
    List {
        /// Filter drafts to a GitHub OWNER/REPO.
        #[arg(long, value_name = "OWNER/REPO")]
        repo: Option<String>,

        /// Maximum drafts to show.
        #[arg(long, value_name = "N")]
        limit: Option<usize>,

        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Show one draft with full rule text and source evidence.
    Show {
        /// Pending draft id.
        id: String,

        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Review pending drafts interactively.
    Review {
        /// Filter drafts to a GitHub OWNER/REPO.
        #[arg(long, value_name = "OWNER/REPO")]
        repo: Option<String>,

        /// Maximum drafts to review.
        #[arg(long, value_name = "N")]
        limit: Option<usize>,
    },

    /// Approve a draft and activate it as local memory.
    Approve {
        /// Pending draft id. Omit when using --all.
        id: Option<String>,

        /// Approve every matching draft.
        #[arg(long)]
        all: bool,

        /// Filter --all to a GitHub OWNER/REPO.
        #[arg(long, value_name = "OWNER/REPO")]
        repo: Option<String>,

        /// Skip the confirmation prompt for --all.
        #[arg(long)]
        yes: bool,

        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Reject a draft and remove it from the local queue.
    Reject {
        /// Pending draft id. Omit when using --all.
        id: Option<String>,

        /// Reject every matching draft.
        #[arg(long)]
        all: bool,

        /// Filter --all to a GitHub OWNER/REPO.
        #[arg(long, value_name = "OWNER/REPO")]
        repo: Option<String>,

        /// Skip the confirmation prompt for --all.
        #[arg(long)]
        yes: bool,

        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum MemoryCommands {
    /// Show everything waiting for review, plus local rules already active.
    Inbox {
        /// Show all available rows instead of the default short preview.
        #[arg(long)]
        all: bool,

        /// Maximum rows to show per section.
        #[arg(long)]
        limit: Option<usize>,

        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Show active memory rules currently available to agents.
    Active {
        /// Show active rules from every repo. By default only the current repo is shown.
        #[arg(long)]
        all: bool,

        /// Maximum rules to show.
        #[arg(long)]
        limit: Option<usize>,

        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Show local evidence that agents retrieved or were shown rules.
    Activity {
        /// Look back this many days.
        #[arg(long, default_value_t = 30)]
        days: i64,

        /// Maximum recent events to show.
        #[arg(long)]
        limit: Option<usize>,

        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Show one memory item with its rule text and source evidence.
    Show {
        /// Item id, such as rule:<skill-id>, draft:<skill-id>, or session:<content_hash>.
        item_id: String,

        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Save and immediately enable a user-requested coding rule.
    #[command(
        long_about = concat!(
            "Save and immediately enable a user-requested coding rule.\n",
            "Agents should prefer the `remember_rule` MCP tool when available; this\n",
            "command is the CLI fallback for user phrases like \"remember this rule\",\n",
            "\"from now on\", or \"don't do this again\".\n",
            "\n",
            "Because the user explicitly asked to remember the rule, this command treats\n",
            "that request as approval and activates the rule for local agents."
        )
    )]
    Remember {
        /// Short imperative title for the rule.
        #[arg(long, value_name = "TEXT")]
        title: String,

        /// Full rule body and context. If omitted, non-interactive stdin is read.
        #[arg(long, value_name = "TEXT")]
        body: Option<String>,

        /// File glob the rule applies to. Repeat for multiple globs.
        #[arg(long = "file-pattern", value_name = "GLOB")]
        file_patterns: Vec<String>,

        /// Optional snippet showing the pattern to avoid.
        #[arg(long, value_name = "TEXT")]
        bad_code: Option<String>,

        /// Optional snippet showing the preferred pattern.
        #[arg(long, value_name = "TEXT")]
        good_code: Option<String>,

        /// Optional severity hint: low, medium, or high.
        #[arg(long, value_name = "LEVEL")]
        severity: Option<String>,

        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Import project agent memory files into local DiffLore memory.
    ImportAgentFiles {
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Walk through everything pending approval, one item at a time.
    Review {
        /// Maximum pending items to review.
        #[arg(long)]
        limit: Option<usize>,
    },

    /// Let DiffLore locally enable high-confidence memories and leave noisy ones for review.
    Autopilot {
        /// Preview what would be enabled without changing local memory.
        #[arg(long)]
        dry_run: bool,

        /// Maximum groups to enable in one run.
        #[arg(long, value_name = "N")]
        max_auto_enable: Option<usize>,

        /// Output as JSON.
        #[arg(long)]
        json: bool,

        /// Internal detached worker entry point.
        #[arg(long, hide = true)]
        background: bool,

        /// Internal lease owner token for the detached worker.
        #[arg(long, hide = true)]
        lease_owner: Option<String>,
    },

    /// Clean up duplicate or already-active pending memory candidates.
    #[command(
        long_about = concat!(
            "Clean up local pending memory candidates that are safe to remove.\n",
            "By default this previews only. Pass --apply to reject reviewable session\n",
            "candidates that already match an active rule, plus duplicate rows inside\n",
            "a candidate group. Approved optional-sync rows are left untouched."
        )
    )]
    Cleanup {
        /// Apply the cleanup. Without this flag, only print a preview.
        #[arg(long)]
        apply: bool,

        /// Maximum candidate groups to scan.
        #[arg(long, value_name = "N")]
        limit: Option<usize>,

        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Summarize active memory and pending candidate groups.
    Digest {
        /// Maximum candidate groups to show.
        #[arg(long)]
        limit: Option<usize>,

        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Show memory groups DiffLore recommends for approval.
    Recommended {
        /// Show all recommended groups instead of the default short preview.
        #[arg(long)]
        all: bool,

        /// Maximum recommended groups to show.
        #[arg(long)]
        limit: Option<usize>,

        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Show recent local autopilot and disable events.
    Log {
        /// Maximum events to show.
        #[arg(long)]
        limit: Option<usize>,

        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Show persisted candidate-vs-active rule conflicts for review.
    Conflicts {
        /// Maximum conflict records to show.
        #[arg(long)]
        limit: Option<usize>,

        /// Only show conflicts in this status (e.g. detected).
        #[arg(long)]
        status: Option<String>,

        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Stop serving an active local rule to agents.
    Disable {
        /// Rule id, such as rule:<skill-id> or <skill-id>.
        rule_id: String,

        /// Reason to record in the local audit log.
        #[arg(long)]
        reason: Option<String>,

        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Approve one pending item into active local memory.
    Approve {
        /// Item id, such as session:<content_hash> or draft:<skill-id>.
        item_id: String,

        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Reject one pending item and keep it out of active memory.
    Reject {
        /// Item id, such as session:<content_hash> or draft:<skill-id>.
        item_id: String,

        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Review memory suggestions generated from team activity.
    TeamCandidates {
        /// Team id. Defaults to the current cloud team.
        #[arg(long)]
        team_id: Option<String>,

        /// Maximum suggestions to show.
        #[arg(long, default_value_t = 20)]
        limit: i64,

        /// Result offset for pagination.
        #[arg(long, default_value_t = 0)]
        offset: i64,

        /// Candidate status to show.
        #[arg(long, value_enum, default_value_t = TeamCandidateStatusArg::Pending)]
        status: TeamCandidateStatusArg,

        /// Output as JSON.
        #[arg(long)]
        json: bool,

        #[command(subcommand)]
        command: Option<TeamCandidateCommands>,
    },

    /// Pull published team rules; raw local queues require opt-in flags.
    Sync(SyncCliArgs),

    /// Export active local/team memory rules as an editable package.
    #[command(
        long_about = concat!(
            "Export active memory rules to a versioned package for review or hand editing.\n",
            "With --format json this writes one JSON file. With --format markdown this writes\n",
            "manifest.json plus one editable Markdown file per rule. The target must be\n",
            "missing, an empty directory, or an empty file; DiffLore refuses to overwrite\n",
            "non-empty package targets."
        )
    )]
    ExportPackage {
        /// Output path. `.json` infers JSON; any other path infers a Markdown directory.
        #[arg(long, value_name = "PATH")]
        output: PathBuf,

        /// Package format.
        #[arg(long, value_enum, default_value_t = MemoryPackageFormatArg::Auto)]
        format: MemoryPackageFormatArg,

        /// Preview the package plan without writing files.
        #[arg(long)]
        dry_run: bool,

        /// Output as JSON.
        #[arg(long)]
        json: bool,

        /// Export local rules only; exclude team/cloud-synced rules.
        #[arg(long)]
        local_only: bool,

        /// Cap export to the first N rules. Unlimited when omitted.
        #[arg(long, value_name = "N", value_parser = clap::value_parser!(u64).range(1..))]
        max_rules: Option<u64>,
    },

    /// Import an editable memory package and update matching existing rules.
    #[command(
        long_about = concat!(
            "Import a versioned memory package from a JSON file or Markdown directory.\n",
            "The minimal safe loop updates existing rules by id. Missing ids are reported\n",
            "and never created implicitly. Use --dry-run to validate and preview changes."
        )
    )]
    ImportPackage {
        /// Package file or directory.
        #[arg(long, value_name = "PATH")]
        source: PathBuf,

        /// Validate and preview without updating local memory.
        #[arg(long)]
        dry_run: bool,

        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Review drafts mined from imported reviews (a subset of the inbox).
    #[command(hide = true)]
    Drafts {
        #[command(subcommand)]
        command: DraftsCommands,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum TeamCandidateStatusArg {
    Pending,
    Approved,
    Rejected,
    All,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum TeamCandidateSeverityArg {
    Info,
    Warning,
    Error,
}

#[derive(Subcommand)]
pub(crate) enum TeamCandidateCommands {
    /// Count team memory suggestions.
    Count {
        /// Team id. Defaults to the current cloud team.
        #[arg(long)]
        team_id: Option<String>,

        /// Candidate status to count.
        #[arg(long, value_enum)]
        status: Option<TeamCandidateStatusArg>,

        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Show one team memory suggestion.
    Show {
        /// Team candidate id.
        candidate_id: String,

        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Approve one team memory suggestion.
    Approve {
        /// Team candidate id.
        candidate_id: String,

        /// Optional title edit before publishing.
        #[arg(long, value_name = "TEXT")]
        name: Option<String>,

        /// Optional description edit before publishing.
        #[arg(long, value_name = "TEXT")]
        description: Option<String>,

        /// Optional severity edit before publishing.
        #[arg(long, value_enum)]
        severity: Option<TeamCandidateSeverityArg>,

        /// Optional rule content edit before publishing.
        #[arg(long, value_name = "TEXT")]
        content: Option<String>,

        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Reject one team memory suggestion.
    Reject {
        /// Team candidate id.
        candidate_id: String,

        /// Reason to store with the rejection.
        #[arg(long)]
        reason: Option<String>,

        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum AuthCommands {
    /// Store or verify a GitLab personal access token (needs `read_api` scope).
    #[command(next_line_help = false, long_about = concat!(
        "Store a GitLab personal access token for review import. The token is\n",
        "encrypted at rest with the same mechanism as the cloud login token.\n",
        "\n",
        "Pipe the token via stdin so it never lands in shell history:\n",
        "\n",
        "  echo \"<TOKEN>\" | difflore auth gitlab\n",
        "  echo \"<TOKEN>\" | difflore auth gitlab --host gitlab.corp.example\n",
        "\n",
        "At import time the token is resolved in this order:\n",
        "DIFFLORE_GITLAB_TOKEN env, GITLAB_TOKEN env, then stored token.\n",
        "\n",
        "Use `--check` to verify the resolved token against the host's\n",
        "/api/v4/user endpoint, and `--remove` to delete the stored token.",
    ))]
    Gitlab {
        /// GitLab host (self-managed instances supported, e.g. gitlab.corp.example).
        #[arg(long, value_name = "HOST", default_value = difflore_core::ingest::gitlab::auth::DEFAULT_GITLAB_HOST)]
        host: String,

        /// Verify the resolved token against GET https://<HOST>/api/v4/user instead of storing.
        #[arg(long, conflicts_with = "remove")]
        check: bool,

        /// Remove the stored token for this host.
        #[arg(long)]
        remove: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum CloudCommands {
    /// Show current cloud login, plan, and team info.
    Status {
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Sign in for team sync, dashboards, and managed semantic recall.
    #[command(next_line_help = false, long_about = concat!(
        "Log in to DiffLore Cloud.\n",
        "Use this for team sync, dashboards, and managed semantic recall.\n",
        "\n",
        "Browser login is the default for interactive use:\n",
        "\n",
        "  difflore cloud login\n",
        "  difflore cloud login --browser\n",
        "  difflore cloud login --github\n",
        "\n",
        "For headless environments, pass a token with `--token`, stdin, or ",
        "`DIFFLORE_CLOUD_TOKEN`.",
    ))]
    Login {
        /// Bearer token. Prefer stdin or `DIFFLORE_CLOUD_TOKEN` to keep it out of shell history.
        #[arg(long)]
        token: Option<String>,

        /// Force the browser OAuth flow even from local non-TTY shells; prints the auth URL.
        #[arg(long, conflicts_with_all = ["token", "github"])]
        browser: bool,

        /// Exchange the local GitHub CLI auth token for a DiffLore Cloud token.
        #[arg(long, conflicts_with = "token")]
        github: bool,
    },

    /// Pull published team rules; raw observation/session queues upload only with opt-in flags.
    Sync(SyncCliArgs),

    /// Show your team workspace and what it still needs.
    Team {
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Publish a local rule to the current cloud team.
    Publish {
        /// Local or cloud rule id to publish.
        #[arg(long, value_name = "RULE_ID")]
        rule: String,

        /// Team id. Defaults to the current cloud team.
        #[arg(long, value_name = "TEAM_ID")]
        team_id: Option<String>,

        /// Team enforcement policy.
        #[arg(long, default_value = "recommended", value_parser = ["recommended", "required"])]
        enforcement: String,

        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Remove a published rule from the current cloud team.
    Unpublish {
        /// Local or cloud rule id to unpublish.
        #[arg(long, value_name = "RULE_ID")]
        rule: String,

        /// Team id. Defaults to the current cloud team.
        #[arg(long, value_name = "TEAM_ID")]
        team_id: Option<String>,

        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Show how your team's rules changed real reviews.
    Impact {
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Clear the stored cloud token on this device.
    Logout,
}

#[derive(Subcommand)]
pub(crate) enum AgentsCommands {
    /// Install DiffLore into every detected local agent (MCP + hooks).
    Install {
        /// Preview what would change without touching disk.
        #[arg(long)]
        dry_run: bool,
    },

    /// Remove DiffLore from every agent it was installed into.
    #[command(
        long_about = "Undo `difflore agents install`: remove the DiffLore MCP entry and \
DiffLore hook groups from each agent it was wired into (preserving every other entry), \
then delete the canonical install record. Pass `--dry-run` to preview first."
    )]
    Uninstall {
        /// Preview what would be removed without touching disk.
        #[arg(long)]
        dry_run: bool,
    },

    /// Show which agents are connected.
    Status {
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Re-render DiffLore blocks that are unchanged since DiffLore wrote them.
    #[command(
        long_about = "Safely upgrade DiffLore's config/hook blocks to the current shape. \
Blocks that are byte-identical to what DiffLore last wrote are re-rendered in place; \
blocks you (or another tool) hand-edited are left untouched unless you pass `--force`. \
Pass `--dry-run` to preview the plan first."
    )]
    Update {
        /// Preview what would change without touching disk.
        #[arg(long)]
        dry_run: bool,

        /// Overwrite blocks that were locally edited since DiffLore wrote them.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum ProviderCommands {
    /// List configured AI backends.
    List {
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Interactive AI backend picker (Claude, Codex, Gemini, or OpenCode CLI).
    Setup,

    /// Add a local AI CLI backend.
    Add {
        /// Agent CLI to use: `claude`, `codex`, `gemini`, or `opencode`.
        #[arg(long, value_name = "TOOL")]
        tool: String,

        /// Optional model override. Defaults to the agent CLI's own default.
        #[arg(long)]
        model: Option<String>,
    },

    /// Remove an AI backend by ID.
    Remove {
        /// Provider ID to remove.
        id: String,
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },

    /// Set active AI backend.
    SetActive {
        /// Provider ID.
        id: String,
    },
}

#[derive(Subcommand)]
pub(crate) enum EmbeddingsCommands {
    /// Show the active search mode.
    Status {
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Configure a BYOK semantic embedding provider (OpenAI-compatible).
    Setup {
        /// Base URL of the OpenAI-compatible embedding endpoint.
        #[arg(long, value_name = "URL")]
        provider_url: Option<String>,

        /// Embedding model name.
        #[arg(long, value_name = "MODEL")]
        model: Option<String>,

        /// Output vector dimensionality.
        #[arg(long, value_name = "N")]
        dim: Option<usize>,

        /// API key. Prefer the `DIFFLORE_EMBEDDING_KEY` env var or piped stdin.
        #[arg(long, value_name = "KEY")]
        key: Option<String>,

        /// Configure a keyless local provider (no API key required).
        #[arg(long, conflicts_with = "key")]
        no_key: bool,
    },

    /// Turn off semantic search and use fast local keyword matching.
    Disable,

    /// Rebuild the local semantic search index for this repo.
    Rebuild {
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
}
