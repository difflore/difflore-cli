use clap::{Parser, Subcommand};

use super::args::{
    FixCliArgs, ImportReviewsCliArgs, InitCliArgs, RecallCliArgs, StatusLane, SyncCliArgs,
};

#[derive(Parser)]
#[command(name = "difflore")]
#[command(bin_name = "difflore")]
#[command(about = "AI review memory for local coding agents")]
#[command(next_line_help = true)]
#[command(
    long_about = "DiffLore turns your team's past PR review judgment into memory \
your local AI agents can recall before they code. The core loop is: \
`difflore init`, `difflore import-reviews`, `difflore cloud sync`, \
then `difflore recall --diff` or `difflore fix --preview`."
)]
pub(crate) struct Cli {
    /// Bare `difflore` shows local memory status and the next command.
    #[command(subcommand)]
    pub(crate) command: Option<Commands>,

    /// Disable interactive prompts (skips the first-run wizard on a TTY).
    #[arg(long, global = true)]
    pub(crate) no_interactive: bool,
}

#[derive(Subcommand)]
pub(crate) enum Commands {
    /// See DiffLore work on a bundled sample — no setup, no repo, nothing written.
    #[command(
        long_about = "Run a zero-config demo with bundled PR-review rules and a sample edit. \
It shows the memories that would fire, where they came from, and the next command to run. \
Nothing leaves your laptop and nothing is written to disk."
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

    /// Import past GitHub PR review comments as review memory evidence.
    #[command(
        next_line_help = false,
        long_about = "Import past GitHub PR review comments into local review memory. \
Use `--dry-run` to preview first, then run `difflore recall --diff` to see what agents will remember."
    )]
    ImportReviews(ImportReviewsCliArgs),

    /// Preview which team memories an agent would see for an intent or current diff.
    Recall(RecallCliArgs),

    /// Suggest local patches using remembered team review judgment.
    #[command(
        next_line_help = false,
        long_about = "Suggest safe local patches for the current diff or a GitHub PR. \
Start with `difflore fix --preview`; accepted changes only touch the working tree. \
DiffLore never commits, pushes, opens PRs, or posts GitHub comments."
    )]
    Fix(FixCliArgs),

    /// Ask the team's review memory a natural-language question.
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

    /// Review, approve, or reject local memory drafts.
    Drafts {
        #[command(subcommand)]
        command: DraftsCommands,
    },

    /// Manage cloud login, sync, and team impact.
    Cloud {
        #[command(subcommand)]
        command: CloudCommands,
    },

    /// Browse and install shareable starter rule packs.
    Packs {
        #[command(subcommand)]
        command: PacksCommands,
    },

    /// Wire and inspect local AI agent integrations.
    Agents {
        #[command(subcommand)]
        command: AgentsCommands,
    },

    /// Refresh the binary guidance, agent blocks, hook shim config, and doctor checks.
    #[command(
        long_about = "One update pass for DiffLore ergonomics. Prints the binary update command for \
your install channel when it can detect one, safely re-renders unchanged agent config/hook blocks \
with `agents update`, then runs `doctor` so stale shims and runtime drift are visible."
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

    /// Measure recall quality (self-recall @1/@5/MRR) on your corpus, fast and repeatable.
    #[command(
        hide = true,
        long_about = "Measure recall quality on the local corpus. Deterministic and offline; \
nothing is written to your real indexes."
    )]
    Eval {
        /// Number of rules to sample (1..=200).
        #[arg(long, value_name = "N")]
        samples: Option<usize>,

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
        /// Write the report to a file instead of stdout.
        #[arg(long)]
        report: bool,

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
pub(crate) enum PacksCommands {
    /// List packs in the registry (or installed packs with --installed).
    List {
        /// Registry base URL or file:// path (defaults to the first-party registry).
        #[arg(long, value_name = "URL")]
        registry: Option<String>,
        /// List locally-installed packs instead of the registry catalog.
        #[arg(long)]
        installed: bool,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show a pack's manifest (rules, provenance, target globs, license).
    Show {
        /// Pack id, optionally `<id>@<version>`.
        #[arg(value_name = "PACK_ID")]
        pack_id: String,
        #[arg(long, value_name = "URL")]
        registry: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Install a pack's rules as suggestion-only starter memory.
    Install {
        /// Pack id, optionally `<id>@<version>`.
        #[arg(value_name = "PACK_ID")]
        pack_id: String,
        #[arg(long, value_name = "URL")]
        registry: Option<String>,
        /// Preview the rows that would be written without writing.
        #[arg(long)]
        dry_run: bool,
        /// Skip confirmation prompts.
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        json: bool,
    },
    /// List locally-installed packs (alias for `packs list --installed`).
    Installed {
        #[arg(long)]
        json: bool,
    },
    /// Remove all rules installed from a pack.
    Uninstall {
        /// Pack id to remove.
        #[arg(value_name = "PACK_ID")]
        pack_id: String,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        json: bool,
    },
    /// Validate a local pack directory's pack.json and print PR instructions.
    Publish {
        /// Path to the pack.json manifest to validate.
        #[arg(value_name = "PATH")]
        path: String,
        #[arg(long, value_name = "URL")]
        registry: Option<String>,
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
pub(crate) enum CloudCommands {
    /// Show current cloud login, plan, and team info.
    Status {
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Log in to cloud (browser OAuth, GitHub CLI, or pre-issued token via flag/env/stdin).
    #[command(next_line_help = false, long_about = concat!(
        "Log in to DiffLore Cloud.\n",
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

    /// Sync governed team review memory with cloud.
    Sync(SyncCliArgs),

    /// Show the current team workspace and its readiness checks.
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

    /// Show cloud team impact proof.
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
    /// Install DiffLore into every detected local agent.
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

    /// Show per-agent detection and install state.
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

    /// Interactive AI backend picker (Claude / Codex / Gemini / OpenCode CLI).
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
