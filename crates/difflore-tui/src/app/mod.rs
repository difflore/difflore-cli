use std::collections::HashMap;
use std::io;
use std::panic::{self, PanicHookInfo};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::cursor::Show;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use difflore_core::activity_stream::{ActivityEvent, ActivityPayload};
use difflore_core::cloud::sync::CloudStatus;
use difflore_core::models::SkillRecord;
use difflore_core::observability::fix_outcomes::{FixOutcomeDaily, FixOutcomeSummary};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::style::Color;
use ratatui::widgets::ListState;

use crate::modals::{Modal, ModalStack};
use crate::state::{Entitlements, EventStrip, PlanState, SupportSla, Tier};
use crate::tabs::Tab;
use crate::theme::{self, Theme};
use crate::widgets::{EventStripState, PlanStateView, PlanTier};

mod input;
mod modals;
mod render;
mod state;

type TuiTerminal = Terminal<CrosstermBackend<io::Stdout>>;
type PanicHook = Box<dyn Fn(&PanicHookInfo<'_>) + Sync + Send + 'static>;

pub struct AppState {
    pub(super) project_root: PathBuf,
    pub(super) plan_state: PlanState,

    pub rules: Vec<SkillRecord>,
    pub rules_list_state: ListState,
    pub rules_origin_filter: RulesOriginFilter,
    pub rules_load_error: Option<String>,
    /// Side-table `(rule_id → source_repo)` keyed off `skills.source_repo`.
    /// `SkillRecord` stays untouched (stable serde surface); the TUI joins
    /// against this map to filter/group by upstream repo.
    pub rules_source_repos: HashMap<String, Option<String>>,
    pub fix_outcome_summary: Option<FixOutcomeSummary>,
    pub fix_outcome_daily: Vec<FixOutcomeDaily>,
    pub fix_outcomes_load_error: Option<String>,
    /// Vertical scroll offset for the Activity tab pipeline list.
    pub activity_offset: usize,
    /// Last known visible-row budget for the pipeline panel, written by
    /// `tabs::activity::render` each frame so the key handler can clamp
    /// `activity_offset` without knowing the terminal geometry itself.
    pub activity_visible_rows: usize,
    /// Last known number of rendered pipeline rows. Updated by render
    /// alongside `activity_visible_rows`; input never re-reads the JSONL
    /// stream just to clamp scrolling.
    pub activity_rows_len: usize,
    /// Detected `owner/repo` of the directory the TUI was launched from.
    pub current_repo: Option<String>,
    pub rules_repo_filter: RulesRepoFilter,

    /// Which pane in the Rules tab currently receives keystrokes.
    pub rules_focus: RulesFocus,

    /// Active substring search in the Rules tab.
    pub rules_search: RulesSearch,

    /// Snapshot of agent install / provider / cloud login state, loaded
    /// once at startup by the CLI.
    pub wiring: crate::WiringSnapshot,

    /// Non-fatal runtime notice shown in the hint strip. Used for input
    /// reader/browser errors so the dashboard does not exit just because
    /// a side-channel hiccuped.
    pub status_notice: Option<String>,
}

impl AppState {
    pub(crate) const fn plan_state(&self) -> &PlanState {
        &self.plan_state
    }
}

/// Three-way scope toggle for the Rules tab. Default is `ThisRepo` when
/// the user launched the TUI inside a known GitHub repo.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RulesRepoFilter {
    ThisRepo,
    All,
    Global,
}

/// Memory view selector. `CloudMemory` is the product-facing set and matches
/// the cloud Memory page: accepted/synced cloud rules plus extracted review
/// memories. `All` keeps local raw imports visible without inflating the
/// default memory count.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum RulesOriginFilter {
    #[default]
    CloudMemory,
    All,
    Origin(String),
}

impl RulesOriginFilter {
    pub(crate) fn includes_origin(&self, origin: &str) -> bool {
        match self {
            Self::CloudMemory => is_cloud_memory_origin(origin),
            Self::All => true,
            Self::Origin(want) => origin == want,
        }
    }

    pub(crate) fn label(&self) -> String {
        match self {
            Self::CloudMemory => "cloud memory".to_owned(),
            Self::All => "all local".to_owned(),
            Self::Origin(origin) => origin.clone(),
        }
    }
}

impl RulesRepoFilter {
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::ThisRepo => "this repo",
            Self::All => "all repos",
            Self::Global => "global",
        }
    }
}

/// Three-state machine for the Rules tab `/` search. `Off` / `Editing(q)` /
/// `Filtering(q)` matches the model fzf / lf / less use; `Esc` always
/// returns to `Off`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum RulesSearch {
    #[default]
    Off,
    Editing(String),
    Filtering(String),
}

impl RulesSearch {
    pub(crate) const fn query(&self) -> Option<&str> {
        match self {
            Self::Off => None,
            Self::Editing(q) | Self::Filtering(q) => Some(q.as_str()),
        }
    }

    pub(crate) const fn is_editing(&self) -> bool {
        matches!(self, Self::Editing(_))
    }
}

/// Which pane in the Rules tab currently has keyboard focus. Switched with
/// `h` / `l`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RulesFocus {
    #[default]
    List,
    Detail,
}

pub struct App {
    pub(super) state: AppState,
    pub(super) active_tab: Tab,
    pub(super) should_quit: bool,
    /// Set when the user picks an action that requires a subprocess.
    /// `App::run` returns this so the CLI host can dispatch *after*
    /// `LeaveAlternateScreen` has restored the terminal.
    pub(super) pending_exit: crate::TuiExit,
    pub(super) modal_stack: ModalStack,
    pub(super) show_help: bool,
}

impl App {
    pub(crate) async fn new(project_root: PathBuf, wiring: crate::WiringSnapshot) -> Self {
        let (rules_result, rules_source_repos_result, fix_activity_result) =
            match difflore_core::db::init_db().await {
                Ok(pool) => {
                    let rules_pool = pool.clone();
                    let repos_pool = pool.clone();
                    tokio::join!(
                        load_rules(&rules_pool),
                        load_rule_source_repos(&repos_pool),
                        load_fix_activity(&pool)
                    )
                }
                Err(err) => {
                    let err = format!("failed to initialize database: {err}");
                    (Err(err.clone()), Err(err.clone()), Err(err))
                }
            };

        let (rules, rules_load_error) = match rules_result {
            Ok(rs) => (rs, None),
            Err(e) => (Vec::new(), Some(e)),
        };

        // Best-effort sidecar load. If it fails we still render rules — the
        // repo filter just collapses to "All" via the empty map.
        let rules_source_repos = rules_source_repos_result.unwrap_or_default();
        let (fix_outcome_summary, fix_outcome_daily, fix_outcomes_load_error) =
            match fix_activity_result {
                Ok((summary, daily)) => (Some(summary), daily, None),
                Err(e) => (None, Vec::new(), Some(e)),
            };

        // GitHub remote → "owner/repo". `None` outside a repo or for
        // non-github remotes; the filter then defaults to All.
        let current_repo = project_root
            .to_str()
            .and_then(difflore_core::git::detect_github_repo_full_name);
        // Default Memory tab to the current repo when one is detected so
        // empty-state copy guides the user to scope-cycle. Falls back to
        // All when not in a repo.
        let rules_repo_filter = if current_repo.is_some() {
            RulesRepoFilter::ThisRepo
        } else {
            RulesRepoFilter::All
        };

        let mut rules_list_state = ListState::default();
        if !rules.is_empty() {
            rules_list_state.select(Some(0));
        }

        let rules_empty = rules.is_empty();
        let mut plan_state = load_plan_state(&rules, &wiring).await;
        derive_event_strip_from_plan(&mut plan_state);
        derive_event_strip_from_events(&mut plan_state, &difflore_core::activity_stream::tail(20));
        let modal_stack = modal_stack_for_launch(&plan_state, rules_empty, &wiring);
        let rules_origin_filter = default_origin_filter(&rules);

        Self {
            state: AppState {
                project_root,
                plan_state,
                rules,
                rules_list_state,
                rules_origin_filter,
                rules_load_error,
                rules_source_repos,
                fix_outcome_summary,
                fix_outcome_daily,
                fix_outcomes_load_error,
                activity_offset: 0,
                activity_visible_rows: 0,
                activity_rows_len: 0,
                current_repo,
                rules_repo_filter,
                rules_focus: RulesFocus::default(),
                rules_search: RulesSearch::default(),
                wiring,
                status_notice: None,
            },
            active_tab: Tab::default(),
            should_quit: false,
            pending_exit: crate::TuiExit::Quit,
            modal_stack,
            show_help: false,
        }
    }

    pub(crate) async fn run(mut self) -> crate::Result<crate::TuiExit> {
        let shutdown_requested = Arc::new(AtomicBool::new(false));
        let _signal_task = SignalTask::spawn(Arc::clone(&shutdown_requested));
        let _panic_hook = TerminalPanicHook::install();
        let mut terminal = TerminalSession::start()?;

        let result = self
            .event_loop(terminal.terminal_mut(), &shutdown_requested)
            .await;
        let cleanup_result = terminal.cleanup();

        finish_run(result, cleanup_result)?;

        Ok(self.pending_exit)
    }

    async fn event_loop<B: ratatui::backend::Backend>(
        &mut self,
        terminal: &mut Terminal<B>,
        shutdown_requested: &AtomicBool,
    ) -> crate::Result<()> {
        let input_reader = InputReader::spawn();
        while !self.should_quit && !shutdown_requested.load(Ordering::Relaxed) {
            terminal.draw(|frame| self.draw(frame))?;

            let frame_started = Instant::now();
            loop {
                if self.should_quit || shutdown_requested.load(Ordering::Relaxed) {
                    break;
                }

                if self.drain_pending_input(&input_reader) {
                    break;
                }

                let elapsed = frame_started.elapsed();
                if elapsed >= FRAME_TICK {
                    break;
                }

                if let Some(remaining) = FRAME_TICK.checked_sub(elapsed) {
                    tokio::time::sleep(remaining.min(INPUT_WAKE_INTERVAL)).await;
                }
            }
        }
        Ok(())
    }

    fn drain_pending_input(&mut self, input_reader: &InputReader) -> bool {
        let mut handled = false;
        while let Some(result) = input_reader.next_key() {
            let key = match result {
                Ok(key) => key,
                Err(err) => {
                    self.set_status_notice(format!(
                        "Input reader error: {err}. Ctrl-C still exits."
                    ));
                    continue;
                }
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            handled = true;
            if is_ctrl_c_key(key) {
                self.should_quit = true;
                break;
            }
            self.handle_key(key.code);
            if self.should_quit {
                break;
            }
        }
        handled
    }
}

const FRAME_TICK: Duration = Duration::from_millis(250);
const INPUT_WAKE_INTERVAL: Duration = Duration::from_millis(16);
const INPUT_POLL_INTERVAL: Duration = Duration::from_millis(10);
const CLOUD_STATUS_TIMEOUT: Duration = Duration::from_secs(2);
const FREE_CLOUD_MEMORY_CAP: usize = 200;

struct InputReader {
    receiver: mpsc::Receiver<io::Result<KeyEvent>>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl InputReader {
    fn spawn() -> Self {
        let (sender, receiver) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let handle = thread::spawn(move || read_input_events(&thread_stop, &sender));
        Self {
            receiver,
            stop,
            handle: Some(handle),
        }
    }

    fn next_key(&self) -> Option<io::Result<KeyEvent>> {
        self.receiver.try_recv().ok()
    }
}

impl Drop for InputReader {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn read_input_events(stop: &Arc<AtomicBool>, sender: &mpsc::Sender<io::Result<KeyEvent>>) {
    while !stop.load(Ordering::Relaxed) {
        match event::poll(INPUT_POLL_INTERVAL) {
            Ok(true) => match event::read() {
                Ok(Event::Key(key)) => {
                    if sender.send(Ok(key)).is_err() {
                        break;
                    }
                }
                Ok(_) => {}
                Err(err) => {
                    let _ = sender.send(Err(err));
                    break;
                }
            },
            Ok(false) => {}
            Err(err) => {
                let _ = sender.send(Err(err));
                break;
            }
        }
    }
}

struct SignalTask {
    handle: tokio::task::JoinHandle<()>,
}

impl SignalTask {
    fn spawn(shutdown_requested: Arc<AtomicBool>) -> Self {
        let handle = tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                shutdown_requested.store(true, Ordering::Relaxed);
            }
        });
        Self { handle }
    }
}

impl Drop for SignalTask {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

struct TerminalSession {
    terminal: TuiTerminal,
    cleaned: bool,
}

impl TerminalSession {
    fn start() -> crate::Result<Self> {
        enable_raw_mode()?;

        let mut stdout = io::stdout();
        if let Err(err) = execute!(stdout, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(err.into());
        }

        let backend = CrosstermBackend::new(stdout);
        let terminal = match Terminal::new(backend) {
            Ok(terminal) => terminal,
            Err(err) => {
                restore_terminal_best_effort();
                return Err(err.into());
            }
        };

        Ok(Self {
            terminal,
            cleaned: false,
        })
    }

    const fn terminal_mut(&mut self) -> &mut TuiTerminal {
        &mut self.terminal
    }

    fn cleanup(&mut self) -> io::Result<()> {
        if self.cleaned {
            return Ok(());
        }
        self.cleaned = true;

        let mut errors = CleanupErrors::default();
        errors.record(disable_raw_mode());
        errors.record(execute!(self.terminal.backend_mut(), LeaveAlternateScreen));
        errors.record(self.terminal.show_cursor());
        errors.finish()
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

struct TerminalPanicHook {
    original_hook: Arc<Mutex<Option<PanicHook>>>,
}

impl TerminalPanicHook {
    fn install() -> Self {
        let original_hook = Arc::new(Mutex::new(Some(panic::take_hook())));
        let hook_for_panic = Arc::clone(&original_hook);
        panic::set_hook(Box::new(move |info| {
            restore_terminal_best_effort();
            if let Ok(original_hook) = hook_for_panic.lock()
                && let Some(original_hook) = original_hook.as_ref()
            {
                original_hook(info);
            }
        }));
        Self { original_hook }
    }
}

impl Drop for TerminalPanicHook {
    fn drop(&mut self) {
        if let Ok(mut original_hook) = self.original_hook.lock()
            && let Some(original_hook) = original_hook.take()
        {
            panic::set_hook(original_hook);
        }
    }
}

#[derive(Default)]
struct CleanupErrors {
    first: Option<io::Error>,
}

impl CleanupErrors {
    fn record(&mut self, result: io::Result<()>) {
        if let Err(err) = result
            && self.first.is_none()
        {
            self.first = Some(err);
        }
    }

    fn finish(self) -> io::Result<()> {
        self.first.map_or_else(|| Ok(()), Err)
    }
}

fn restore_terminal_best_effort() {
    let _ = disable_raw_mode();
    let mut stdout = io::stdout();
    let _ = execute!(stdout, LeaveAlternateScreen, Show);
}

fn finish_run(result: crate::Result<()>, cleanup_result: io::Result<()>) -> crate::Result<()> {
    match result {
        Ok(()) => cleanup_result.map_err(crate::TuiError::from),
        Err(err) => Err(err),
    }
}

fn is_ctrl_c_key(key: KeyEvent) -> bool {
    key.kind == KeyEventKind::Press
        && key.code == KeyCode::Char('c')
        && key.modifiers.contains(KeyModifiers::CONTROL)
}

pub(super) fn build_status_bar_view(plan: &PlanState) -> PlanStateView {
    let tier = match plan.tier {
        Tier::Free => PlanTier::Free,
        Tier::Team => PlanTier::Team,
        Tier::TeamPlus => PlanTier::TeamPlus,
    };

    let event_strip = match &plan.event_strip {
        EventStrip::None => EventStripState::None,
        EventStrip::CrossMachine { .. } => EventStripState::CrossMachine,
        EventStrip::TeammateCaught {
            teammate, fired_at, ..
        } => EventStripState::TeammateCaught {
            teammate: teammate.clone(),
            when_label: fired_at.clone(),
        },
        EventStrip::FixRunsLow { used, quota } => EventStripState::FixRunsLow {
            used: *used,
            quota: *quota,
        },
    };

    PlanStateView {
        tier,
        plan_label: plan.plan_label.clone(),
        rule_count: plan.rule_count,
        published_count: plan.published_count,
        event_strip,
        fix_runs_used: plan.entitlements.fix_runs_used,
        fix_runs_quota: plan.entitlements.fix_runs_quota,
    }
}

fn derive_event_strip_from_plan(plan: &mut PlanState) {
    if !matches!(plan.event_strip, EventStrip::None) {
        return;
    }

    let used = plan.entitlements.fix_runs_used;
    let quota = plan.entitlements.fix_runs_quota;
    if matches!(plan.tier, Tier::Team) && quota > 0 && u64::from(used) * 5 >= u64::from(quota) * 4 {
        plan.event_strip = EventStrip::FixRunsLow { used, quota };
    }
}

fn derive_event_strip_from_events(plan: &mut PlanState, events: &[ActivityEvent]) {
    if !matches!(plan.event_strip, EventStrip::None) {
        return;
    }

    if !matches!(plan.tier, Tier::Free) {
        return;
    }

    if let Some((used, quota)) = latest_embed_cap(events) {
        plan.entitlements.fix_runs_used = used;
        plan.entitlements.fix_runs_quota = quota;
        plan.event_strip = EventStrip::FixRunsLow { used, quota };
    }
}

fn latest_embed_cap(events: &[ActivityEvent]) -> Option<(u32, u32)> {
    events.iter().find_map(|event| {
        if let ActivityPayload::EmbedCapReached { cap, used } = &event.payload {
            Some((*used, *cap))
        } else {
            None
        }
    })
}

async fn load_plan_state(rules: &[SkillRecord], wiring: &crate::WiringSnapshot) -> PlanState {
    let mut plan = PlanState {
        rule_count: count_to_u32(primary_memory_rule_count(rules)),
        ..Default::default()
    };

    if wiring.cloud_logged_in {
        apply_cloud_login_baseline(&mut plan, rules);
        let client = difflore_core::cloud::client::CloudClient::create().await;
        if let Ok(status) = tokio::time::timeout(
            CLOUD_STATUS_TIMEOUT,
            difflore_core::cloud::sync::fetch_cloud_status(&client),
        )
        .await
        {
            apply_cloud_status_to_plan(&mut plan, &status);
        }
    }

    plan
}

fn apply_cloud_login_baseline(plan: &mut PlanState, rules: &[SkillRecord]) {
    if cloud_memory_rule_count(rules) > FREE_CLOUD_MEMORY_CAP {
        plan.tier = Tier::Team;
        "Cloud Team".clone_into(&mut plan.plan_label);
        "#5ee0c8".clone_into(&mut plan.plan_accent);
        plan.entitlements = entitlements_for_tier(Tier::Team);
    } else {
        "Cloud Free".clone_into(&mut plan.plan_label);
    }
}

fn apply_cloud_status_to_plan(plan: &mut PlanState, status: &CloudStatus) {
    if !status.logged_in {
        return;
    }

    let tier = tier_from_cloud_status(status);
    plan.tier = tier;
    plan.plan_label = plan_label_from_cloud_status(status, tier);
    match tier {
        Tier::Free => "#7d8588",
        Tier::Team => "#5ee0c8",
        Tier::TeamPlus => "#a78bfa",
    }
    .clone_into(&mut plan.plan_accent);
    plan.entitlements = entitlements_for_tier(tier);
}

fn tier_from_cloud_status(status: &CloudStatus) -> Tier {
    let plan = status
        .plan
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase()
        .replace('-', "_");
    match plan.as_str() {
        "team_plus" | "enterprise" => Tier::TeamPlus,
        "team" | "pro" | "business" => Tier::Team,
        _ if status.team_name.is_some() => Tier::Team,
        _ => Tier::Free,
    }
}

fn plan_label_from_cloud_status(status: &CloudStatus, tier: Tier) -> String {
    if let Some(team) = status.team_name.as_deref().map(str::trim)
        && !team.is_empty()
    {
        return team.to_owned();
    }

    match tier {
        Tier::Free => "Cloud Free".to_owned(),
        Tier::Team => "Cloud Team".to_owned(),
        Tier::TeamPlus => "Cloud Team Plus".to_owned(),
    }
}

fn entitlements_for_tier(tier: Tier) -> Entitlements {
    match tier {
        Tier::Free => Entitlements::default(),
        Tier::Team => Entitlements {
            cloud_hosted: true,
            cross_machine_sync: true,
            publish_to_team: true,
            knowledge_build: true,
            byok_allowed: true,
            support_sla: SupportSla::H48,
            ..Default::default()
        },
        Tier::TeamPlus => Entitlements {
            cloud_hosted: true,
            cross_machine_sync: true,
            publish_to_team: true,
            knowledge_build: true,
            byok_allowed: true,
            support_sla: SupportSla::H8,
            ..Default::default()
        },
    }
}

fn modal_stack_for_launch(
    plan: &PlanState,
    rules_empty: bool,
    wiring: &crate::WiringSnapshot,
) -> ModalStack {
    let mut stack = ModalStack::new();

    match &plan.event_strip {
        EventStrip::None => {}
        EventStrip::CrossMachine { other_host } => stack.try_show(Modal::CrossMachine {
            other_host: other_host.clone(),
        }),
        EventStrip::TeammateCaught {
            rule,
            teammate,
            fired_at,
        } => stack.try_show(Modal::TeammateCaught {
            rule: rule.clone(),
            teammate: teammate.clone(),
            fired_at: fired_at.clone(),
        }),
        EventStrip::FixRunsLow { used, quota } => stack.try_show(Modal::FixRunsLow {
            used: *used,
            quota: *quota,
        }),
    }

    if rules_empty {
        stack.try_show(Modal::Onboarding {
            step: onboarding_step(wiring),
        });
    }

    stack
}

const fn onboarding_step(wiring: &crate::WiringSnapshot) -> u8 {
    if wiring.agents_detected > wiring.agents_installed {
        1
    } else if wiring.provider_name.is_none() {
        2
    } else if !wiring.cloud_logged_in {
        3
    } else {
        4
    }
}

async fn load_rules(pool: &difflore_core::SqlitePool) -> Result<Vec<SkillRecord>, String> {
    // Rules live in the global DB; per-project DBs store embeddings.
    difflore_core::skills::list(pool)
        .await
        .map_err(|e| format!("failed to load rules: {e}"))
}

async fn load_rule_source_repos(
    pool: &difflore_core::SqlitePool,
) -> Result<HashMap<String, Option<String>>, String> {
    difflore_core::skills::list_source_repos(pool)
        .await
        .map_err(|e| format!("failed to load source_repos: {e}"))
}

async fn load_fix_activity(
    pool: &difflore_core::SqlitePool,
) -> Result<(FixOutcomeSummary, Vec<FixOutcomeDaily>), String> {
    let summary = difflore_core::fix_outcomes::summary(pool, 30)
        .await
        .map_err(|e| format!("failed to load fix activity: {e}"))?;
    let daily = difflore_core::fix_outcomes::daily(pool, 30)
        .await
        .map_err(|e| format!("failed to load fix activity: {e}"))?;
    Ok((summary, daily))
}

pub(super) fn open_url(url: &str) -> io::Result<()> {
    webbrowser::open(url)
}

impl App {
    pub(super) fn set_status_notice(&mut self, notice: impl Into<String>) {
        self.state.status_notice = Some(crate::widgets::truncate(&notice.into(), 120));
    }

    pub(super) fn open_url_with_notice(&mut self, url: &str) {
        match open_url(url) {
            Ok(()) => self.set_status_notice(format!(
                "Opened {} in your browser.",
                difflore_core::cloud::endpoints::web_host_display()
            )),
            Err(err) => self.set_status_notice(format!("Could not open browser: {err}")),
        }
    }

    pub(super) fn open_cloud_path(&mut self, path: &str) {
        let url = difflore_core::cloud::endpoints::web_link(path);
        self.open_url_with_notice(&url);
    }
}

fn count_to_u32(count: usize) -> u32 {
    u32::try_from(count).unwrap_or(u32::MAX)
}

pub(crate) fn cloud_memory_rule_count(rules: &[SkillRecord]) -> usize {
    rules
        .iter()
        .filter(|rule| is_cloud_memory_origin(&rule.origin))
        .count()
}

pub(crate) fn raw_local_rule_count(rules: &[SkillRecord]) -> usize {
    rules
        .iter()
        .filter(|rule| !is_cloud_memory_origin(&rule.origin))
        .count()
}

fn primary_memory_rule_count(rules: &[SkillRecord]) -> usize {
    let cloud_memory = cloud_memory_rule_count(rules);
    if cloud_memory > 0 {
        cloud_memory
    } else {
        rules.len()
    }
}

fn default_origin_filter(rules: &[SkillRecord]) -> RulesOriginFilter {
    if cloud_memory_rule_count(rules) > 0 {
        RulesOriginFilter::CloudMemory
    } else {
        RulesOriginFilter::All
    }
}

pub(crate) fn is_cloud_memory_origin(origin: &str) -> bool {
    matches!(origin, "cloud" | "extracted")
}

/// Shared origin-to-color mapping through the bundled origin taxonomy.
pub(crate) fn origin_color(origin: &str) -> Color {
    match difflore_core::origins::color_hex_for(origin) {
        Some(hex) => theme::hex_to_color(hex),
        None => Theme::current().muted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modals::ModalKind;

    fn rule_with_origin(origin: &str) -> SkillRecord {
        SkillRecord {
            id: format!("{origin}-id"),
            name: format!("{origin}-rule"),
            source: "local".into(),
            directory: "/tmp/rule".into(),
            version: "1.0.0".into(),
            description: "sample".into(),
            r#type: "workflow".into(),
            engines: Vec::new(),
            tags: Vec::new(),
            trigger: None,
            check_prompt: None,
            repo_owner: None,
            repo_name: None,
            repo_branch: None,
            readme_url: None,
            enabled_for_codex: true,
            enabled_for_claude: false,
            enabled_for_gemini: false,
            enabled_for_cursor: false,
            installed_at: "2026-01-01".into(),
            updated_at: "2026-01-01".into(),
            enforcement: None,
            origin: origin.into(),
        }
    }

    #[test]
    fn origin_color_round_trips_through_registry() {
        let muted = Theme::current().muted;
        for id in [
            "manual",
            "conversation",
            "pr_review",
            "extracted",
            "cloud",
            "team",
        ] {
            #[allow(clippy::panic)] // reason: test invariant — taxonomy must list every id
            let hex =
                difflore_core::origins::color_hex_for(id).unwrap_or_else(|| panic!("missing {id}"));
            let expected = theme::hex_to_color(hex);
            assert_eq!(origin_color(id), expected, "round-trip failed for {id}");
            assert_ne!(origin_color(id), muted, "{id} fell back to muted");
        }
    }

    #[test]
    fn unknown_origin_falls_back_to_muted() {
        assert_eq!(origin_color("not-a-real-origin"), Theme::current().muted);
    }

    #[test]
    fn from_digit_is_used_for_tab_jumps() {
        for d in 1u8..=4 {
            assert!(Tab::from_digit(d).is_some());
        }
        assert!(Tab::from_digit(0).is_none());
        assert!(Tab::from_digit(5).is_none());
    }

    #[test]
    fn build_status_bar_view_maps_event_strip_variants() {
        let mut plan = PlanState {
            tier: Tier::Team,
            event_strip: EventStrip::FixRunsLow {
                used: 240,
                quota: 300,
            },
            ..Default::default()
        };
        plan.entitlements.fix_runs_used = 240;
        plan.entitlements.fix_runs_quota = 300;
        let view = build_status_bar_view(&plan);
        assert_eq!(view.tier, PlanTier::Team);
        assert!(matches!(
            view.event_strip,
            EventStripState::FixRunsLow {
                used: 240,
                quota: 300
            }
        ));
        let right = view.right_side();
        assert!(
            right
                .as_deref()
                .is_some_and(|text| text.contains("240/300"))
        );
    }

    #[test]
    fn ctrl_c_key_is_detected_only_for_control_c_presses() {
        assert!(is_ctrl_c_key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        )));
        assert!(!is_ctrl_c_key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::NONE,
        )));
        assert!(!is_ctrl_c_key(KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::CONTROL,
        )));
    }

    #[test]
    fn cleanup_errors_keep_first_failure() {
        let mut errors = CleanupErrors::default();
        errors.record(Err(io::Error::other("first")));
        errors.record(Ok(()));
        errors.record(Err(io::Error::other("second")));

        let err = errors.finish().unwrap_err();
        assert_eq!(err.to_string(), "first");
    }

    #[test]
    fn event_loop_error_wins_over_cleanup_error() {
        let result = finish_run(
            Err(crate::TuiError::Io(io::Error::other("event loop"))),
            Err(io::Error::other("cleanup")),
        );

        let err = result.unwrap_err();
        assert!(err.to_string().contains("event loop"));
    }

    #[test]
    fn cleanup_error_is_returned_when_event_loop_succeeds() {
        let result = finish_run(Ok(()), Err(io::Error::other("cleanup")));

        let err = result.unwrap_err();
        assert!(err.to_string().contains("cleanup"));
    }

    #[test]
    fn derive_event_strip_flags_team_capacity_at_eighty_percent() {
        let mut plan = PlanState {
            tier: Tier::Team,
            ..Default::default()
        };
        plan.entitlements.fix_runs_used = 80;
        plan.entitlements.fix_runs_quota = 100;

        derive_event_strip_from_plan(&mut plan);

        assert_eq!(
            plan.event_strip,
            EventStrip::FixRunsLow {
                used: 80,
                quota: 100
            }
        );
    }

    #[test]
    fn derive_event_strip_uses_recent_embed_cap_activity() {
        let mut plan = PlanState::default();
        let events = vec![ActivityEvent {
            ts_ms: 1,
            payload: ActivityPayload::EmbedCapReached {
                used: 198,
                cap: 200,
            },
        }];

        derive_event_strip_from_events(&mut plan, &events);

        assert_eq!(
            plan.event_strip,
            EventStrip::FixRunsLow {
                used: 198,
                quota: 200
            }
        );
        assert_eq!(plan.entitlements.fix_runs_used, 198);
        assert_eq!(plan.entitlements.fix_runs_quota, 200);
    }

    #[test]
    fn count_to_u32_saturates() {
        assert_eq!(count_to_u32(42), 42);
        assert_eq!(count_to_u32(usize::MAX), u32::MAX);
    }

    #[test]
    fn cloud_memory_count_excludes_raw_local_imports() {
        let rules = vec![
            rule_with_origin("cloud"),
            rule_with_origin("extracted"),
            rule_with_origin("pr_review"),
            rule_with_origin("manual"),
            rule_with_origin("conversation"),
        ];

        assert_eq!(cloud_memory_rule_count(&rules), 2);
        assert_eq!(raw_local_rule_count(&rules), 3);
        assert_eq!(
            default_origin_filter(&rules),
            RulesOriginFilter::CloudMemory
        );
    }

    #[test]
    fn team_cloud_status_drives_plan_badge() {
        let mut plan = PlanState::default();
        let status = CloudStatus {
            logged_in: true,
            email: Some("hibrandonevans@outlook.com".to_owned()),
            plan: Some("team".to_owned()),
            team_name: Some("invite-smoke-60377e".to_owned()),
            team_id: None,
        };

        apply_cloud_status_to_plan(&mut plan, &status);

        assert_eq!(plan.tier, Tier::Team);
        assert_eq!(plan.plan_label, "invite-smoke-60377e");
        assert!(plan.entitlements.cross_machine_sync);
        assert!(plan.entitlements.publish_to_team);
    }

    #[test]
    fn logged_in_large_cloud_cache_starts_as_team_before_remote_status_returns() {
        let mut plan = PlanState::default();
        let rules: Vec<SkillRecord> = (0..=FREE_CLOUD_MEMORY_CAP)
            .map(|i| {
                let mut rule = rule_with_origin("cloud");
                rule.id = format!("cloud-{i}");
                rule
            })
            .collect();

        apply_cloud_login_baseline(&mut plan, &rules);

        assert_eq!(plan.tier, Tier::Team);
        assert_eq!(plan.plan_label, "Cloud Team");
        assert!(plan.entitlements.cross_machine_sync);
    }

    #[test]
    fn paid_plan_ignores_stale_free_embed_cap_events() {
        let mut plan = PlanState {
            tier: Tier::Team,
            ..Default::default()
        };
        let events = vec![ActivityEvent {
            ts_ms: 1,
            payload: ActivityPayload::EmbedCapReached {
                used: 198,
                cap: 200,
            },
        }];

        derive_event_strip_from_events(&mut plan, &events);

        assert_eq!(plan.event_strip, EventStrip::None);
    }

    #[test]
    fn modal_stack_for_launch_turns_plan_events_into_visible_modals() {
        let plan = PlanState {
            event_strip: EventStrip::TeammateCaught {
                rule: "Rule".to_owned(),
                teammate: "ana".to_owned(),
                fired_at: "today".to_owned(),
            },
            ..Default::default()
        };

        let stack = modal_stack_for_launch(&plan, false, &crate::WiringSnapshot::default());

        assert_eq!(
            stack.current().map(Modal::kind),
            Some(ModalKind::TeammateCaught)
        );
    }

    #[test]
    fn onboarding_modal_is_seeded_for_empty_rule_set() {
        let wiring = crate::WiringSnapshot {
            agents_detected: 2,
            agents_installed: 1,
            ..Default::default()
        };

        let stack = modal_stack_for_launch(&PlanState::default(), true, &wiring);

        assert_eq!(
            stack.current().map(Modal::kind),
            Some(ModalKind::Onboarding)
        );
        assert_eq!(onboarding_step(&wiring), 1);
    }
}
