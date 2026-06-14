//! Dashboard application state and main loop.
//!
//! `mod.rs` keeps `AppState` + the frame/event loop; the terminal lifecycle
//! lives in [`terminal`], plan assembly in [`plan_state`], key handling in
//! [`input`], view assembly in [`render`], and derived queries in
//! [`selectors`].

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossterm::event::KeyEventKind;
use difflore_core::domain::models::SkillRecord;
use difflore_core::observability::fix_outcomes::{FixOutcomeDaily, FixOutcomeSummary};
use ratatui::Terminal;
use ratatui::widgets::ListState;

use crate::modals::{Modal, ModalStack};
use crate::plan::{EventStrip, PlanState};
use crate::tabs::Tab;
use crate::tabs::memory::{RulesFocus, RulesOriginFilter, RulesRepoFilter, RulesSearch};

mod input;
mod plan_state;
mod render;
mod selectors;
mod terminal;

use plan_state::{derive_event_strip_from_events, derive_event_strip_from_plan, load_plan_state};
use terminal::{InputReader, SignalTask, TerminalPanicHook, TerminalSession, is_ctrl_c_key};

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
    /// Vertical scroll offset for the Fixes tab pipeline list.
    pub fixes_offset: usize,
    /// Last known visible-row budget for the pipeline panel, written by
    /// `tabs::fixes::render` each frame so the key handler can clamp
    /// `fixes_offset` without knowing the terminal geometry itself.
    pub fixes_visible_rows: usize,
    /// Last known number of rendered pipeline rows. Updated by render
    /// alongside `fixes_visible_rows`; input never re-reads the JSONL
    /// stream just to clamp scrolling.
    pub fixes_rows_len: usize,
    /// Detected `owner/repo` of the directory the TUI was launched from.
    pub current_repo: Option<String>,
    pub rules_repo_filter: RulesRepoFilter,

    /// Which pane in the Memory tab currently receives keystrokes.
    pub rules_focus: RulesFocus,

    /// Active substring search in the Memory tab.
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
            match difflore_core::infra::db::init_db().await {
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
            .and_then(difflore_core::infra::git::detect_github_repo_full_name);
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
        derive_event_strip_from_events(
            &mut plan_state,
            &difflore_core::observability::activity_stream::tail(20),
        );
        let modal_stack = modal_stack_for_launch(&plan_state, rules_empty, &wiring);
        let rules_origin_filter = crate::tabs::memory::filter::default_origin_filter(&rules);

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
                fixes_offset: 0,
                fixes_visible_rows: 0,
                fixes_rows_len: 0,
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

        terminal::finish_run(result, cleanup_result)?;

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

const FRAME_TICK: Duration = Duration::from_millis(250);
const INPUT_WAKE_INTERVAL: Duration = Duration::from_millis(16);

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
    let summary = difflore_core::observability::fix_outcomes::summary(pool, 30)
        .await
        .map_err(|e| format!("failed to load fix activity: {e}"))?;
    let daily = difflore_core::observability::fix_outcomes::daily(pool, 30)
        .await
        .map_err(|e| format!("failed to load fix activity: {e}"))?;
    Ok((summary, daily))
}

pub(super) fn open_url(url: &str) -> io::Result<()> {
    webbrowser::open(url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modals::ModalKind;

    #[test]
    fn from_digit_is_used_for_tab_jumps() {
        for d in 1u8..=4 {
            assert!(Tab::from_digit(d).is_some());
        }
        assert!(Tab::from_digit(0).is_none());
        assert!(Tab::from_digit(5).is_none());
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
