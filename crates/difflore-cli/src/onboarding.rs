use std::io::{self, IsTerminal, Write};
use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};
use std::time::{Duration, Instant};

use crate::style::{self, sym};

/// Filename of the sentinel under `~/.difflore/` that records a user has
/// already seen the first-run welcome. Bare `difflore` checks for it
/// before deciding whether to print the screen below.
const SENTINEL: &str = "welcomed";
const SENTINEL_VERSION: &str = "welcome-v2";

/// Resume marker written by Step 4 when Cloud processing outlasts the
/// user's patience. Its presence makes a re-run of `difflore` jump back
/// to Step 4 instead of restarting the flow. Removed once a wizard
/// completes (`mark_welcomed`) or the user resets.
const RESUME_SENTINEL: &str = "wizard_resume";
const RESUME_SENTINEL_VERSION: &str = "wizard-resume-v2";

/// Soft offer-out: at this elapsed mark we ask the user whether to keep
/// waiting or finish now and resume later. Tunable, not user-facing.
const SOFT_CAP: Duration = Duration::from_secs(60);

/// Hard cap: we never block the wizard longer than this even if the user
/// keeps saying "wait more". Past this, fall through to the resume hint
/// unconditionally.
const HARD_CAP: Duration = Duration::from_secs(180);

/// Heartbeat cadence — how often Step 4 reprints the elapsed line so the
/// user sees the wait isn't frozen. Each tick also re-polls cloud.
const POLL_INTERVAL: Duration = Duration::from_secs(5);

const DEFAULT_IMPORT_MAX_PRS: usize = 50;
const MIN_IMPORT_MAX_PRS: usize = 1;
const MAX_IMPORT_MAX_PRS: usize = 1000;

/// First-run state machine:
///
/// ```text
/// Brand-new user, zero rules, no cloud login, sentinel missing
///   -> launch interactive wizard (collapses 4-bounce onboarding),
///      then drop into the TUI dashboard
/// New machine, no welcome sentinel, but not fresh
///   -> show static welcome screen, then drop into the TUI dashboard
/// Returning user / opted out / non-TTY
///   -> stay silent; bare `difflore` falls through to the compact
///      status surface (no TUI)
/// ```
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum FirstRunPath {
    /// New machine + interactive — run the chained wizard, then the TUI
    /// dashboard (`tui_entry::run_dashboard`).
    LaunchWizard,
    /// New machine + interactive, but not fresh (existing rules or a cloud
    /// login) — print the static welcome, then the TUI dashboard.
    ShowWelcome,
    /// Either a returning user (sentinel exists), an opt-out, or a non-TTY
    /// context. The caller falls through to the compact status surface;
    /// the TUI is never launched here.
    Skip,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum WelcomeFlow {
    ContinueTui,
    Stop,
}

impl WelcomeFlow {
    pub(crate) const fn should_continue_tui(self) -> bool {
        matches!(self, Self::ContinueTui)
    }
}

/// Inputs for [`should_launch_wizard`]. Pulled out of the env-sniffing
/// branches so unit tests can supply deterministic fixtures.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WizardSignals {
    pub stdin_is_tty: bool,
    pub stdout_is_tty: bool,
    pub no_interactive_flag: bool,
    pub no_welcome_env: bool,
    pub sentinel_exists: bool,
    pub local_rules_count: usize,
    pub cloud_logged_in: bool,
}

/// Should the chained wizard run?
///
/// The four skip rails (CI flag, pipe, env opt-out, returning user)
/// short-circuit before the first-run criteria so an onboarded user is
/// never pulled back into the wizard.
pub(crate) const fn should_launch_wizard(s: WizardSignals) -> bool {
    if s.no_interactive_flag {
        return false;
    }
    if !s.stdin_is_tty || !s.stdout_is_tty {
        return false;
    }
    if s.no_welcome_env {
        return false;
    }
    if s.sentinel_exists {
        return false;
    }
    // First-run: no local rules and not logged in => the user hasn't
    // gotten value out of the product yet.
    s.local_rules_count == 0 && !s.cloud_logged_in
}

#[derive(Debug, Clone, Copy)]
struct FirstRunPreflight {
    stdout_is_tty: bool,
    no_interactive_flag: bool,
    no_welcome_env: bool,
    sentinel_exists: bool,
    resume_pending: bool,
}

/// Fast rails that need no DB / cloud probing, so returning, opted-out, or
/// resume-pending users skip the `CommandContext::new` latency.
const fn preflight_first_run_path(s: FirstRunPreflight) -> Option<FirstRunPath> {
    if !s.stdout_is_tty || s.no_interactive_flag || s.no_welcome_env {
        return Some(FirstRunPath::Skip);
    }
    if s.sentinel_exists && !s.resume_pending {
        return Some(FirstRunPath::Skip);
    }
    if s.resume_pending {
        return Some(FirstRunPath::LaunchWizard);
    }
    None
}

/// Decide which first-run path bare `difflore` should take.
///
/// Suppression rails run before the DB/cloud sniff, in order: non-TTY,
/// no-interactive, env opt-out, then sentinels.
pub(crate) async fn first_run_path(no_interactive_flag: bool) -> FirstRunPath {
    let stdout_tty = io::stdout().is_terminal();
    let stdin_tty = io::stdin().is_terminal();
    if !stdout_tty || no_interactive_flag {
        return FirstRunPath::Skip;
    }
    let no_welcome_env = difflore_core::infra::env::flag_set(difflore_core::infra::env::DIFFLORE_NO_WELCOME);
    if no_welcome_env {
        return FirstRunPath::Skip;
    }
    let Ok(dir) = difflore_core::infra::paths::data_home() else {
        return FirstRunPath::Skip;
    };
    let sentinel_exists = sentinel_version_current(&dir.join(SENTINEL), SENTINEL_VERSION);
    let resume_pending =
        sentinel_version_current(&dir.join(RESUME_SENTINEL), RESUME_SENTINEL_VERSION);
    if let Some(path) = preflight_first_run_path(FirstRunPreflight {
        stdout_is_tty: stdout_tty,
        no_interactive_flag,
        no_welcome_env,
        sentinel_exists,
        resume_pending,
    }) {
        return path;
    }

    // Probe the same signals init.rs uses so wizard and static welcome
    // agree on what "fresh install" means.
    let (rules_count, cloud_logged_in) = sniff_first_run_state().await;

    let signals = WizardSignals {
        stdin_is_tty: stdin_tty,
        stdout_is_tty: stdout_tty,
        no_interactive_flag,
        no_welcome_env: false,
        sentinel_exists: false,
        local_rules_count: rules_count,
        cloud_logged_in,
    };

    if should_launch_wizard(signals) {
        FirstRunPath::LaunchWizard
    } else {
        FirstRunPath::ShowWelcome
    }
}

/// Reuse init.rs's queries so the wizard's notion of "fresh user" matches
/// the dashboard's. Failures degrade to "fresh".
async fn sniff_first_run_state() -> (usize, bool) {
    // `first_run_path` runs once per process before dispatch, so build a
    // context locally rather than threading one through the pipeline.
    let ctx = crate::runtime::CommandContext::new(crate::runtime::OutputMode::Text).await;
    let rules = match difflore_core::skills::stats(&ctx.db).await {
        Ok(s) => s.total as usize,
        Err(_) => 0,
    };
    let logged_in = ctx.cloud().await.is_logged_in();
    (rules, logged_in)
}

/// Print the first-run welcome screen and mark the sentinel.
///
/// Non-blocking: the TUI launches afterwards regardless of what the user
/// types. The screen sets expectations the empty TUI dashboard can't.
pub(crate) async fn show_welcome_then_continue() -> WelcomeFlow {
    let cloud_logged_in = difflore_core::cloud::client::CloudClient::create()
        .await
        .is_logged_in();
    let import_cmd = static_welcome_import_command(cloud_logged_in);
    let wordmark = style::wordmark();
    let rule = style::pewter(style::DIVIDER);
    let tip = style::emerald(sym::TIP);

    println!();
    println!("  {wordmark}");
    println!("  {rule}");
    println!(
        "  DiffLore helps Claude, Codex, Cursor, and local agents remember team review judgment."
    );
    println!("  Fewer repeated review comments, fewer redo loops.");
    println!();
    println!("  {tip} See it work right now - no repo, no setup:");
    println!(
        "        {}   a live recall on a bundled sample edit (5 seconds)",
        style::cmd("difflore try"),
    );
    println!();
    println!("  {tip} Then make it real on your repo:");
    println!(
        "        1. {} install once: wire agents and pick a provider",
        style::cmd("difflore init"),
    );
    println!(
        "        2. {}  {}",
        style::cmd(import_cmd),
        if cloud_logged_in {
            "upload PR comments for team memory"
        } else {
            "create local memories from PR comments"
        },
    );
    println!(
        "        3. {}   show what your AI agents would recall on a real diff",
        style::cmd("difflore recall --diff"),
    );
    println!(
        "        4. {}        show recall, agent readiness, and accepted edits",
        style::cmd("difflore status"),
    );
    println!(
        "        {} Cloud adds managed team memory, sync, and accepted-edit dashboards.",
        style::pewter(sym::BULLET),
    );
    println!();
    println!(
        "  {} press {} to open the local memory dashboard ({} won't show again)",
        style::pewter(sym::BULLET),
        style::cmd("Enter"),
        style::pewter("this message"),
    );
    print!("  > ");
    let _ = io::stdout().flush();
    let mut buf = String::new();
    let _ = io::stdin().read_line(&mut buf);

    mark_welcomed();
    WelcomeFlow::ContinueTui
}

const fn static_welcome_import_command(cloud_logged_in: bool) -> &'static str {
    if cloud_logged_in {
        "difflore import-reviews --max-prs 50 --upload"
    } else {
        "difflore import-reviews --max-prs 50"
    }
}

/// Run the chained 5-step onboarding wizard.
///
/// Each step is skippable (`n` bails out cleanly with a `difflore init`
/// bridge) so a user is never trapped mid-flow.
pub(crate) async fn run_wizard() -> WelcomeFlow {
    let resume = difflore_core::infra::paths::data_home().ok().is_some_and(|dir| {
        sentinel_version_current(&dir.join(RESUME_SENTINEL), RESUME_SENTINEL_VERSION)
    });
    run_wizard_with_interrupt(resume).await
}

/// Step return signal. `Continue` advances; `BailWelcomed` means the step
/// already called `finish_with_bridge` (marks welcomed) and the wizard
/// exits silently; `BailResumeLater` means the resume marker is set and
/// the wizard exits without marking welcomed.
enum StepFlow {
    Continue,
    BailWelcomed,
    BailResumeLater,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum Step4Mode {
    CloudExtraction,
    LocalBridge,
}

#[derive(Default)]
struct WizardState {
    detected_repo: Option<String>,
    cloud_logged_in: bool,
    used_cloud_import: bool,
    cloud_rule_baseline: Option<i32>,
    local_import_ready: bool,
    import_recovery_needed: bool,
    import_max_prs: usize,
}

impl WizardState {
    fn new() -> Self {
        Self {
            import_max_prs: DEFAULT_IMPORT_MAX_PRS,
            ..Self::default()
        }
    }
}

const fn step4_mode(state: &WizardState) -> Step4Mode {
    if state.used_cloud_import {
        Step4Mode::CloudExtraction
    } else {
        Step4Mode::LocalBridge
    }
}

const INTERRUPT_MARK_WELCOMED: u8 = 0;
const INTERRUPT_WRITE_RESUME: u8 = 1;

#[derive(Clone, Default)]
struct WizardInterrupt {
    action: Arc<AtomicU8>,
}

impl WizardInterrupt {
    fn mark_welcomed_on_interrupt(&self) {
        self.action
            .store(INTERRUPT_MARK_WELCOMED, Ordering::Relaxed);
    }

    fn write_resume_on_interrupt(&self) {
        self.action.store(INTERRUPT_WRITE_RESUME, Ordering::Relaxed);
    }

    fn should_write_resume(&self) -> bool {
        self.action.load(Ordering::Relaxed) == INTERRUPT_WRITE_RESUME
    }
}

async fn run_wizard_with_interrupt(resume: bool) -> WelcomeFlow {
    let interrupt = WizardInterrupt::default();
    tokio::select! {
        flow = run_wizard_inner(resume, interrupt.clone()) => flow,
        _ = tokio::signal::ctrl_c() => {
            handle_wizard_interrupt(&interrupt);
            WelcomeFlow::Stop
        }
    }
}

fn handle_wizard_interrupt(interrupt: &WizardInterrupt) {
    println!();
    println!("  {} Setup interrupted.", style::amber(sym::WARN));
    if interrupt.should_write_resume() {
        write_resume_marker();
        println!(
            "  {} Saved your Cloud processing checkpoint; resume with {}.",
            style::pewter(sym::BULLET),
            style::cmd("difflore"),
        );
    } else {
        mark_welcomed();
        println!(
            "  {} Saved the first-run decision; continue later with {}.",
            style::pewter(sym::BULLET),
            style::cmd("difflore init"),
        );
    }
}

async fn run_wizard_inner(resume: bool, interrupt: WizardInterrupt) -> WelcomeFlow {
    print_wizard_header(resume);

    let mut state = WizardState::new();

    if resume {
        println!(
            "  {} Steps 1-3 already done in the previous session.",
            style::emerald(sym::OK),
        );
        state.used_cloud_import = true;
        interrupt.write_resume_on_interrupt();
    } else {
        interrupt.mark_welcomed_on_interrupt();
        match step1_repo_confirm(&mut state).await {
            StepFlow::Continue => {}
            StepFlow::BailWelcomed | StepFlow::BailResumeLater => return WelcomeFlow::Stop,
        }
        match step2_login(&mut state).await {
            StepFlow::Continue => {}
            StepFlow::BailWelcomed | StepFlow::BailResumeLater => return WelcomeFlow::Stop,
        }
        match step3_import(&mut state).await {
            StepFlow::Continue => {}
            StepFlow::BailWelcomed | StepFlow::BailResumeLater => return WelcomeFlow::Stop,
        }
    }

    match step4_mode(&state) {
        Step4Mode::CloudExtraction => {
            interrupt.write_resume_on_interrupt();
            match step4_wait(state.cloud_rule_baseline).await {
                StepFlow::Continue => {}
                StepFlow::BailWelcomed | StepFlow::BailResumeLater => return WelcomeFlow::Stop,
            }

            interrupt.mark_welcomed_on_interrupt();
            step5_recall().await;
        }
        Step4Mode::LocalBridge => {
            step4_local_candidate_bridge(
                state.local_import_ready,
                state.import_recovery_needed,
                state.import_max_prs,
            );
        }
    }

    println!();
    println!(
        "  {} DiffLore is wired. Run `difflore status` any time to see the next best memory step.",
        style::emerald(sym::OK),
    );

    mark_welcomed();
    WelcomeFlow::ContinueTui
}

fn print_wizard_header(resume: bool) {
    let wordmark = style::wordmark();
    let rule = style::pewter(style::DIVIDER);

    println!();
    if resume {
        println!("  {wordmark}  resuming setup");
    } else {
        println!("  {wordmark}  setup wizard");
    }
    println!("  {rule}");
    if resume {
        println!("  Picking up where we left off - checking on Cloud processing.");
    } else {
        println!("  Five quick questions to wire DiffLore into your repo.");
        println!(
            "  {} skip any step with {} - you can resume with {}",
            style::pewter(sym::BULLET),
            style::cmd("n"),
            style::cmd("difflore"),
        );
        if io::stdout().is_terminal() {
            println!(
                "  {}",
                style::pewter("Tip: skip this with `difflore --no-interactive` (CI / scripts)."),
            );
        }
    }
    println!();
}

async fn step1_repo_confirm(state: &mut WizardState) -> StepFlow {
    state.detected_repo = detect_repo_label();
    let repo_label = state.detected_repo.as_deref().unwrap_or("this repo");
    let q1 = format!(
        "Step 1/5  Detected: {}. Use this as your first source?",
        style::ident(repo_label),
    );
    if !prompt_yes(&q1).await {
        finish_with_bridge("OK - you can wire a different repo later with `difflore init`.");
        return StepFlow::BailWelcomed;
    }
    StepFlow::Continue
}

async fn step2_login(state: &mut WizardState) -> StepFlow {
    let cloud_client = difflore_core::cloud::client::CloudClient::create().await;
    if cloud_client.is_logged_in() {
        println!(
            "  {} Step 2/5  Already logged in to DiffLore Cloud.",
            style::emerald(sym::OK),
        );
        state.cloud_logged_in = true;
        return StepFlow::Continue;
    }
    let q2 = "Step 2/5  Connect to DiffLore Cloud for managed team memory?".to_owned();
    if !prompt_yes(&q2).await {
        println!(
            "  {} Staying local - the CLI can still draft candidates and recall accepted rules.",
            style::pewter(sym::BULLET),
        );
        state.cloud_logged_in = false;
        return StepFlow::Continue;
    }
    if let Err(e) = crate::commands::cloud::try_login_dispatch(None, true).await {
        println!();
        println!(
            "  {} Cloud login did not finish: {e}",
            style::amber(sym::WARN),
        );
        println!(
            "  {} Continuing locally so first-run setup still shows value.",
            style::pewter(sym::BULLET),
        );
        println!(
            "  {} Later: {} then {}",
            style::pewter(sym::BULLET),
            style::cmd("difflore cloud login"),
            style::cmd("difflore import-reviews --upload"),
        );
        state.cloud_logged_in = false;
        return StepFlow::Continue;
    }
    let refreshed = difflore_core::cloud::client::CloudClient::create().await;
    state.cloud_logged_in = refreshed.is_logged_in();
    StepFlow::Continue
}

async fn step3_import(state: &mut WizardState) -> StepFlow {
    let from_label = state.detected_repo.as_deref().unwrap_or("upstream");
    let import_label = if state.cloud_logged_in {
        "Upload PR review history for Cloud team memory from"
    } else {
        "Draft local candidates from PR review history in"
    };
    let q3 = format!("Step 3/5  {import_label} {}?", style::ident(from_label));
    println!(
        "  {} Preview first any time with {}.",
        style::pewter(sym::BULLET),
        style::cmd("difflore import-reviews --dry-run"),
    );
    if !prompt_yes(&q3).await {
        finish_with_bridge(
            "OK - you can import later with `difflore import-reviews --max-prs 50`.",
        );
        return StepFlow::BailWelcomed;
    }
    let Some(max_prs) = prompt_import_depth(DEFAULT_IMPORT_MAX_PRS).await else {
        finish_with_bridge(
            "OK - import was not started. Resume later with `difflore import-reviews --dry-run`.",
        );
        return StepFlow::BailWelcomed;
    };
    state.import_max_prs = max_prs;
    println!(
        "  {} Using {}; import progress prints fetched PR/comment counts as it runs.",
        style::pewter(sym::BULLET),
        style::cmd(&format!("--max-prs {max_prs}")),
    );
    state.used_cloud_import = state.cloud_logged_in;
    if state.used_cloud_import {
        state.cloud_rule_baseline = capture_cloud_rule_count().await;
    }
    let ctx = crate::runtime::CommandContext::new(crate::runtime::OutputMode::Text).await;
    let result = crate::commands::import_reviews::try_handle(
        &ctx,
        crate::commands::import_reviews::ImportArgs {
            repo: None,
            from_upstream: None,
            max_prs,
            pr_numbers: Vec::new(),
            exclude_prs: Vec::new(),
            since: None,
            include_open: false,
            upload: state.used_cloud_import,
            dry_run: false,
            json: false,
        },
    )
    .await;
    match result {
        Ok(outcome) => {
            if state.used_cloud_import && !outcome.cloud_upload_queued {
                println!(
                    "  {} No Cloud processing was queued, so setup will stay on the local path.",
                    style::pewter(sym::BULLET),
                );
                state.used_cloud_import = false;
            }
            state.local_import_ready = !state.used_cloud_import;
            StepFlow::Continue
        }
        Err(e) => {
            println!();
            println!(
                "  {} Step 3 import could not finish.",
                style::amber(sym::WARN)
            );
            println!("{e}");
            println!(
                "  {} Recovery: run {} after GitHub auth/rate limits are clear.",
                style::pewter(sym::BULLET),
                style::cmd("difflore import-reviews --dry-run"),
            );
            println!(
                "  {} Then run {} to draft local memory candidates.",
                style::pewter(sym::BULLET),
                style::cmd(&format!("difflore import-reviews --max-prs {max_prs}")),
            );
            state.used_cloud_import = false;
            state.local_import_ready = false;
            state.import_recovery_needed = true;
            StepFlow::Continue
        }
    }
}

fn step4_local_candidate_bridge(
    local_import_ready: bool,
    import_recovery_needed: bool,
    max_prs: usize,
) {
    println!();
    if local_import_ready {
        println!("  Step 4/5  Local memories are ready.");
        println!(
            "  {} Preview what agents can recall now:",
            style::pewter(sym::BULLET),
        );
        println!("    {}", style::cmd("difflore recall --diff"));
    } else if import_recovery_needed {
        println!("  Step 4/5  Import is paused; your setup progress is still saved.");
        println!(
            "  {} First recover GitHub access and preview the import:",
            style::pewter(sym::BULLET),
        );
        println!("    {}", style::cmd("gh auth login"));
        println!("    {}", style::cmd("difflore import-reviews --dry-run"));
        println!(
            "  {} Then create local memories:",
            style::pewter(sym::BULLET),
        );
        println!(
            "    {}",
            style::cmd(&format!("difflore import-reviews --max-prs {max_prs}")),
        );
    } else {
        println!("  Step 4/5  Staying local.");
        println!(
            "  {} Start with a preview, then create local memories:",
            style::pewter(sym::BULLET),
        );
        println!("    {}", style::cmd("difflore import-reviews --dry-run"));
        println!(
            "    {}",
            style::cmd(&format!("difflore import-reviews --max-prs {max_prs}")),
        );
    }
    println!(
        "  {} Then run {} to prove the agent recall path on this repo.",
        style::pewter(sym::BULLET),
        style::cmd("difflore recall --diff"),
    );
}

async fn step4_wait(baseline: Option<i32>) -> StepFlow {
    println!();
    println!("  Step 4/5  Waiting for Cloud processing (usually 10-60s, occasionally longer)...");
    match wait_for_extraction_tolerant(SOFT_CAP, HARD_CAP, 1, baseline).await {
        ExtractionOutcome::Done { pulled } => {
            println!(
                "  {} Pulled {pulled} rules from cloud.",
                style::emerald(sym::OK),
            );
            clear_resume_marker();
            StepFlow::Continue
        }
        ExtractionOutcome::ResumeLater { pulled, elapsed } => {
            println!();
            println!(
                "  {} Extraction is taking longer than usual ({}s elapsed, {pulled} rules so far).",
                style::pewter(sym::BULLET),
                elapsed.as_secs(),
            );
            println!(
                "  {} Your import is still running in Cloud - we've saved your progress.",
                style::pewter(sym::BULLET),
            );
            println!(
                "  {} Resume any time with {} (or just re-run {}).",
                style::pewter(sym::BULLET),
                style::cmd("difflore"),
                style::cmd("difflore"),
            );
            write_resume_marker();
            StepFlow::BailResumeLater
        }
    }
}

async fn step5_recall() {
    println!();
    println!("  Step 5/5  Showing what your AI agents will recall on the current diff:");
    let ctx = crate::runtime::CommandContext::new(crate::runtime::OutputMode::Text).await;
    crate::commands::recall::handle_recall(
        &ctx,
        crate::commands::recall::RecallArgs {
            intent: None,
            file: None,
            diff: true,
            top_k: 3,
            json: false,
            verbose: false,
            copy: false,
        },
    )
    .await;
}

/// Outcome of [`wait_for_extraction_tolerant`]. `Done` means the rule
/// threshold was hit; `ResumeLater` means the user opted out at the soft
/// cap or the hard cap fired — the caller writes the resume marker.
enum ExtractionOutcome {
    Done { pulled: i32 },
    ResumeLater { pulled: i32, elapsed: Duration },
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum HeartbeatMode {
    InlineRewrite,
    PlainLines,
}

const fn heartbeat_mode(stdout_is_tty: bool, term_is_dumb: bool) -> HeartbeatMode {
    if stdout_is_tty && !term_is_dumb {
        HeartbeatMode::InlineRewrite
    } else {
        HeartbeatMode::PlainLines
    }
}

fn current_heartbeat_mode() -> HeartbeatMode {
    let term_is_dumb = difflore_core::infra::env::var(difflore_core::infra::env::TERM)
        .is_some_and(|term| term.eq_ignore_ascii_case("dumb"));
    heartbeat_mode(io::stdout().is_terminal(), term_is_dumb)
}

fn print_extraction_heartbeat(mode: HeartbeatMode, elapsed: Duration, delta: i32) {
    let line = format!(
        "  ... {}s elapsed, {delta} memories pulled, still processing",
        elapsed.as_secs(),
    );
    match mode {
        HeartbeatMode::InlineRewrite => {
            print!("\r{line}     ");
            let _ = io::stdout().flush();
        }
        HeartbeatMode::PlainLines => {
            println!("{line}");
        }
    }
}

// TODO(wave5): wrap (soft_cap, hard_cap, min_rules) into a `WaitConfig`
// struct if a second caller appears. Today there's only one call site
// in `step4_wait` and the 3-arg signature is still under the clippy
// threshold, so the struct would be cosmetic churn.
/// Tolerant Step-4 polling loop:
/// - Heartbeat line every `POLL_INTERVAL` showing elapsed seconds and
///   running delta. Replaces the silent spinner; the user always knows
///   the wait isn't frozen.
/// - At `soft_cap` we stop and ask: keep waiting, or finish + resume.
/// - `hard_cap` is the absolute ceiling — past it the answer is forced
///   to "resume later" no matter what.
/// - Returns `Done` as soon as the rule delta meets `min_rules`.
async fn wait_for_extraction_tolerant(
    soft_cap: Duration,
    hard_cap: Duration,
    min_rules: i32,
    baseline: Option<i32>,
) -> ExtractionOutcome {
    let client = difflore_core::cloud::client::CloudClient::create().await;
    if !client.is_logged_in() {
        // Nothing to poll; resume-later gives the bridge text instead of
        // a confusing silent success.
        return ExtractionOutcome::ResumeLater {
            pulled: 0,
            elapsed: Duration::ZERO,
        };
    }
    let baseline = match baseline {
        Some(value) => Some(value),
        None => difflore_core::cloud::sync::sync_team_skills(&client)
            .await
            .ok()
            .map(|team| team.visible_count)
            .filter(|value| *value == 0),
    };
    let start = Instant::now();
    let mut soft_offered = false;
    let heartbeat = current_heartbeat_mode();
    loop {
        let elapsed = start.elapsed();
        let now = difflore_core::cloud::sync::sync_team_skills(&client)
            .await
            .map_or_else(|_| baseline.unwrap_or(0), |team| team.visible_count);
        let delta = extraction_delta(now, baseline);

        if delta >= min_rules {
            println!();
            return ExtractionOutcome::Done { pulled: delta };
        }

        if elapsed >= hard_cap {
            println!();
            return ExtractionOutcome::ResumeLater {
                pulled: delta,
                elapsed,
            };
        }

        if !soft_offered && elapsed >= soft_cap {
            soft_offered = true;
            println!();
            let q = format!(
                "  Still waiting after {}s ({delta} rules so far). Keep waiting up to {}s more?",
                elapsed.as_secs(),
                (hard_cap.saturating_sub(elapsed)).as_secs(),
            );
            if !prompt_yes(&q).await {
                return ExtractionOutcome::ResumeLater {
                    pulled: delta,
                    elapsed,
                };
            }
        } else {
            print_extraction_heartbeat(heartbeat, elapsed, delta);
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn capture_cloud_rule_count() -> Option<i32> {
    let client = difflore_core::cloud::client::CloudClient::create().await;
    if !client.is_logged_in() {
        return None;
    }
    difflore_core::cloud::sync::sync_team_skills(&client)
        .await
        .map(|team| team.visible_count)
        .ok()
}

fn extraction_delta(current_count: i32, baseline: Option<i32>) -> i32 {
    match baseline {
        Some(baseline) => (current_count - baseline).max(0),
        None => current_count.max(0),
    }
}

async fn prompt_import_depth(default: usize) -> Option<usize> {
    print!(
        "  {} How far back? {} ",
        style::emerald(sym::TIP),
        style::pewter(&format!(
            "PR limit [{default}] (20 quick, 50 recommended, 100 deeper)"
        )),
    );
    let _ = io::stdout().flush();
    let stdin_is_tty = io::stdin().is_terminal();
    let Some(buf) = read_prompt_line().await else {
        println!();
        return None;
    };

    let answer = clean_prompt_answer(&buf);
    if answer.is_empty() && !stdin_is_tty {
        println!();
        return None;
    }

    match parse_import_depth_answer(&answer, default) {
        Ok(max_prs) => Some(max_prs),
        Err(e) => {
            println!(
                "  {} {e}; using {}.",
                style::amber(sym::WARN),
                style::cmd(&format!("--max-prs {default}")),
            );
            Some(default)
        }
    }
}

fn parse_import_depth_answer(answer: &str, default: usize) -> Result<usize, String> {
    let cleaned = clean_prompt_answer(answer);
    if cleaned.is_empty() {
        return Ok(default);
    }
    let parsed = cleaned
        .parse::<usize>()
        .map_err(|_| format!("{answer:?} is not a number"))?;
    if !(MIN_IMPORT_MAX_PRS..=MAX_IMPORT_MAX_PRS).contains(&parsed) {
        return Err(format!(
            "--max-prs must be between {MIN_IMPORT_MAX_PRS} and {MAX_IMPORT_MAX_PRS}"
        ));
    }
    Ok(parsed)
}

fn detect_repo_label() -> Option<String> {
    if let Ok(cwd) = std::env::current_dir() {
        if let Some(repo) =
            difflore_core::infra::git::detect_github_repo_full_names(&cwd.to_string_lossy())
                .into_iter()
                .next()
        {
            return Some(repo);
        }
    }
    let url = crate::support::util::git_str(&["config", "--get", "remote.origin.url"])?;
    if url.is_empty() {
        return None;
    }
    crate::commands::init::parse_owner_repo_from_url(&url).or(Some(url))
}

/// `[Y/n]` prompt, default yes: empty or any non-`n` answer is yes.
/// EOF/read errors bail out so a non-interactive caller isn't opted into
/// browser/network work.
async fn prompt_yes(question: &str) -> bool {
    print!(
        "  {} {} {} ",
        style::emerald(sym::TIP),
        question,
        style::pewter("[Y/n]"),
    );
    let _ = io::stdout().flush();
    let stdin_is_tty = io::stdin().is_terminal();
    let Some(buf) = read_prompt_line().await else {
        println!();
        return false;
    };
    let answer = clean_prompt_answer(&buf);
    if answer.is_empty() && !stdin_is_tty {
        println!();
        return false;
    }
    !(answer == "n" || answer == "no")
}

async fn read_prompt_line() -> Option<String> {
    let mut buf = String::new();
    let mut reader = tokio::io::BufReader::new(tokio::io::stdin());
    match tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut buf).await {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(buf),
    }
}

fn clean_prompt_answer(line: &str) -> String {
    line.trim_matches(|c: char| c.is_whitespace() || c == '\0' || c == '\u{feff}')
        .to_ascii_lowercase()
}

fn finish_with_bridge(msg: &str) {
    println!();
    println!("  {} {msg}", style::pewter(sym::BULLET));
    println!(
        "  {} Resume any time with {}.",
        style::pewter(sym::BULLET),
        style::cmd("difflore init"),
    );
    mark_welcomed();
}

/// Touch `~/.difflore/welcomed` so the screen never shows again, and
/// clear any pending resume marker. Failures are silent (worst case the
/// welcome shows a second time).
fn mark_welcomed() {
    let Ok(dir) = difflore_core::infra::paths::data_home() else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(dir.join(SENTINEL), sentinel_body(SENTINEL_VERSION));
    let _ = std::fs::remove_file(dir.join(RESUME_SENTINEL));
}

/// Write the resume marker so a later bare `difflore` jumps back into
/// Step 4 instead of restarting at Step 1. Best-effort; failures are
/// silent (re-running the full wizard is idempotent for steps 1-3 since
/// login + import dedupe).
fn write_resume_marker() {
    let Ok(dir) = difflore_core::infra::paths::data_home() else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(
        dir.join(RESUME_SENTINEL),
        sentinel_body(RESUME_SENTINEL_VERSION),
    );
}

/// Remove the resume marker once Step 4 completed, so the next `difflore`
/// doesn't re-enter the wizard. `mark_welcomed` also clears it; doing it
/// here keeps the success path correct independently.
fn clear_resume_marker() {
    let Ok(dir) = difflore_core::infra::paths::data_home() else {
        return;
    };
    let _ = std::fs::remove_file(dir.join(RESUME_SENTINEL));
}

fn sentinel_body(version: &str) -> String {
    format!("{version}\n")
}

fn sentinel_content_current(raw: &str, expected_version: &str) -> bool {
    raw.lines()
        .next()
        .is_some_and(|line| line == expected_version)
}

fn sentinel_version_current(path: &std::path::Path, expected_version: &str) -> bool {
    std::fs::read_to_string(path).is_ok_and(|raw| sentinel_content_current(&raw, expected_version))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> WizardSignals {
        WizardSignals {
            stdin_is_tty: true,
            stdout_is_tty: true,
            no_interactive_flag: false,
            no_welcome_env: false,
            sentinel_exists: false,
            local_rules_count: 0,
            cloud_logged_in: false,
        }
    }

    #[test]
    fn should_launch_wizard_fresh_interactive() {
        assert!(should_launch_wizard(base()));
    }

    #[test]
    fn should_launch_wizard_false_when_piped_stdin() {
        let s = WizardSignals {
            stdin_is_tty: false,
            ..base()
        };
        assert!(!should_launch_wizard(s));
    }

    #[test]
    fn should_launch_wizard_false_when_no_interactive_flag() {
        let s = WizardSignals {
            no_interactive_flag: true,
            ..base()
        };
        assert!(!should_launch_wizard(s));
    }

    #[test]
    fn should_launch_wizard_false_for_returning_user() {
        let s = WizardSignals {
            local_rules_count: 12,
            ..base()
        };
        assert!(!should_launch_wizard(s));
    }

    #[test]
    fn should_launch_wizard_false_when_sentinel_exists() {
        let s = WizardSignals {
            sentinel_exists: true,
            ..base()
        };
        assert!(!should_launch_wizard(s));
    }

    #[test]
    fn should_launch_wizard_false_when_env_opt_out() {
        let s = WizardSignals {
            no_welcome_env: true,
            ..base()
        };
        assert!(!should_launch_wizard(s));
    }

    #[test]
    fn first_run_preflight_skips_returning_user_without_state_probe() {
        assert_eq!(
            preflight_first_run_path(FirstRunPreflight {
                stdout_is_tty: true,
                no_interactive_flag: false,
                no_welcome_env: false,
                sentinel_exists: true,
                resume_pending: false,
            }),
            Some(FirstRunPath::Skip)
        );
    }

    #[test]
    fn first_run_preflight_resume_wins_over_welcomed_sentinel() {
        assert_eq!(
            preflight_first_run_path(FirstRunPreflight {
                stdout_is_tty: true,
                no_interactive_flag: false,
                no_welcome_env: false,
                sentinel_exists: true,
                resume_pending: true,
            }),
            Some(FirstRunPath::LaunchWizard)
        );
    }

    #[test]
    fn first_run_preflight_respects_no_interactive_before_probe() {
        assert_eq!(
            preflight_first_run_path(FirstRunPreflight {
                stdout_is_tty: true,
                no_interactive_flag: true,
                no_welcome_env: false,
                sentinel_exists: false,
                resume_pending: false,
            }),
            Some(FirstRunPath::Skip)
        );
    }

    #[test]
    fn welcome_flow_explicitly_controls_tui_continuation() {
        assert!(WelcomeFlow::ContinueTui.should_continue_tui());
        assert!(!WelcomeFlow::Stop.should_continue_tui());
    }

    /// The constants form an ordered triple: poll < soft < hard. Anything
    /// else makes the loop misbehave (e.g. soft cap firing before the
    /// first poll, or never).
    #[test]
    fn cap_constants_are_ordered() {
        assert!(POLL_INTERVAL < SOFT_CAP);
        assert!(SOFT_CAP < HARD_CAP);
    }

    #[test]
    fn import_depth_parser_accepts_blank_and_range() {
        assert_eq!(parse_import_depth_answer("", 50).unwrap(), 50);
        assert_eq!(parse_import_depth_answer("\r\n", 50).unwrap(), 50);
        assert_eq!(parse_import_depth_answer("100\r\n", 50).unwrap(), 100);
    }

    #[test]
    fn import_depth_parser_rejects_invalid_scope() {
        assert!(parse_import_depth_answer("0", 50).is_err());
        assert!(parse_import_depth_answer("1001", 50).is_err());
        assert!(parse_import_depth_answer("many", 50).is_err());
    }

    #[test]
    fn prompt_answer_cleanup_handles_windows_control_bytes() {
        assert_eq!(clean_prompt_answer("NO\r\n"), "no");
        assert_eq!(clean_prompt_answer("\u{feff}y\0\r\n"), "y");
    }

    #[test]
    fn heartbeat_mode_uses_plain_lines_for_captures_and_dumb_terms() {
        assert_eq!(heartbeat_mode(false, false), HeartbeatMode::PlainLines);
        assert_eq!(heartbeat_mode(true, true), HeartbeatMode::PlainLines);
        assert_eq!(heartbeat_mode(true, false), HeartbeatMode::InlineRewrite);
    }

    #[test]
    fn interrupt_action_switches_to_resume_checkpoint() {
        let interrupt = WizardInterrupt::default();
        assert!(!interrupt.should_write_resume());
        interrupt.write_resume_on_interrupt();
        assert!(interrupt.should_write_resume());
        interrupt.mark_welcomed_on_interrupt();
        assert!(!interrupt.should_write_resume());
    }

    #[test]
    fn sentinel_content_requires_current_version() {
        assert!(sentinel_content_current("welcome-v2\n", SENTINEL_VERSION));
        assert!(sentinel_content_current(
            "wizard-resume-v2\n",
            RESUME_SENTINEL_VERSION
        ));
        assert!(!sentinel_content_current("", SENTINEL_VERSION));
        assert!(!sentinel_content_current("welcome-v1\n", SENTINEL_VERSION));
    }

    #[test]
    fn static_welcome_import_command_matches_cloud_state() {
        assert_eq!(
            static_welcome_import_command(false),
            "difflore import-reviews --max-prs 50"
        );
        assert_eq!(
            static_welcome_import_command(true),
            "difflore import-reviews --max-prs 50 --upload"
        );
    }

    #[test]
    fn step4_mode_stays_local_when_login_was_skipped() {
        let mut state = WizardState::new();
        state.cloud_logged_in = false;
        state.used_cloud_import = false;

        assert_eq!(step4_mode(&state), Step4Mode::LocalBridge);

        state.cloud_logged_in = true;
        state.used_cloud_import = true;
        assert_eq!(step4_mode(&state), Step4Mode::CloudExtraction);
    }

    #[test]
    fn extraction_delta_uses_pre_import_baseline() {
        assert_eq!(extraction_delta(5, Some(2)), 3);
        assert_eq!(extraction_delta(2, Some(5)), 0);
    }

    #[test]
    fn extraction_delta_without_resume_baseline_counts_existing_rules() {
        assert_eq!(extraction_delta(4, None), 4);
        assert_eq!(extraction_delta(-1, None), 0);
    }
}
