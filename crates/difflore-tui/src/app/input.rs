use crossterm::event::KeyCode;

use crate::tabs::{self, Tab};

use super::{App, RulesFocus, RulesSearch};

impl App {
    pub(super) fn handle_key(&mut self, code: KeyCode) {
        if self.show_help {
            match code {
                KeyCode::Char('?' | 'q') | KeyCode::Esc => {
                    self.show_help = false;
                }
                _ => {}
            }
            return;
        }

        if let Some(current) = self.modal_stack.current().cloned() {
            if self.handle_modal_key(&current, code) {
                self.modal_stack.dismiss();
            }
            return;
        }

        // While the user is typing into the Rules-tab search bar, every
        // keystroke (including `q`, digits, `Tab`) must go to the input.
        // Critically: only `Editing` blocks globals; once `Filtering` the
        // user is back in nav mode and `q` quits.
        if self.active_tab == Tab::Rules && self.state.rules_search.is_editing() {
            self.handle_rules_key(code);
            return;
        }

        match code {
            KeyCode::Char('?') => {
                self.show_help = true;
                return;
            }
            KeyCode::Char('q') => {
                self.should_quit = true;
                return;
            }
            KeyCode::Char('u')
                if matches!(
                    self.state.plan_state.event_strip,
                    crate::state::EventStrip::FixRunsLow { .. }
                ) =>
            {
                self.open_cloud_path("pricing?from=tui&intent=upgrade");
                return;
            }
            KeyCode::Char('b')
                if matches!(
                    self.state.plan_state.event_strip,
                    crate::state::EventStrip::FixRunsLow { .. }
                ) =>
            {
                self.pending_exit = crate::TuiExit::RunProvidersAdd;
                self.should_quit = true;
                return;
            }
            KeyCode::Char(':') => {
                self.set_status_notice("No command palette yet. Press ? for shortcuts.");
                return;
            }
            KeyCode::Esc => {
                // Esc precedence: search → origin filter → row selection.
                // Esc never quits — `q` is the dedicated exit.
                if self.active_tab == Tab::Rules
                    && !matches!(self.state.rules_search, RulesSearch::Off)
                {
                    self.state.rules_search = RulesSearch::Off;
                    self.reset_selection_after_filter_change();
                } else {
                    let default_filter = super::default_origin_filter(&self.state.rules);
                    if self.active_tab == Tab::Rules
                        && self.state.rules_origin_filter != default_filter
                    {
                        self.state.rules_origin_filter = default_filter;
                        self.reset_selection_after_filter_change();
                    } else if self.active_tab == Tab::Rules
                        && self.state.rules_list_state.selected().is_some()
                    {
                        self.state.rules_list_state.select(None);
                    }
                }
                return;
            }
            KeyCode::Tab => {
                self.active_tab = self.active_tab.next();
                return;
            }
            KeyCode::BackTab => {
                self.active_tab = self.active_tab.prev();
                return;
            }
            KeyCode::Char(c) if c.is_ascii_digit() => {
                if let Some(d) = c.to_digit(10)
                    && let Ok(d) = u8::try_from(d)
                    && let Some(tab) = Tab::from_digit(d)
                {
                    self.active_tab = tab;
                    return;
                }
            }
            _ => {}
        }

        match self.active_tab {
            Tab::Rules => self.handle_rules_key(code),
            Tab::Team => self.handle_team_key(code),
            Tab::Settings => self.handle_settings_key(code),
            Tab::Activity => self.handle_activity_key(code),
        }
    }

    pub(super) fn handle_activity_key(&mut self, code: KeyCode) {
        let rows_len = self.state.activity_rows_len;
        let visible = self.state.activity_visible_rows;
        let max_offset = rows_len.saturating_sub(visible);
        let candidate = match code {
            KeyCode::Char('j') | KeyCode::Down => self.state.activity_offset.saturating_add(1),
            KeyCode::Char('k') | KeyCode::Up => self.state.activity_offset.saturating_sub(1),
            KeyCode::Char('g') => 0,
            KeyCode::Char('G') => max_offset,
            _ => self.state.activity_offset,
        };
        self.state.activity_offset = tabs::activity::clamp_offset(candidate, rows_len, visible);
    }

    pub(super) fn handle_team_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('c') => {
                self.open_cloud_path("team/candidates?from=tui&intent=candidates");
            }
            KeyCode::Char('d' | 'o') => {
                self.open_cloud_path("?from=tui");
            }
            _ => {}
        }
    }

    pub(super) fn handle_settings_key(&mut self, code: KeyCode) {
        // `i` and `l` need to spawn an interactive child process. We can't
        // run those cleanly from inside the alt-screen — they print to stdout
        // and may prompt. Record the intent, quit the loop, and let the CLI
        // host dispatch the subprocess after the terminal is restored.
        match code {
            KeyCode::Char('i') => {
                self.pending_exit = crate::TuiExit::RunInit;
                self.should_quit = true;
            }
            KeyCode::Char('l') => {
                self.pending_exit = crate::TuiExit::RunCloudLogin;
                self.should_quit = true;
            }
            KeyCode::Char('a') => {
                self.pending_exit = crate::TuiExit::RunProvidersAdd;
                self.should_quit = true;
            }
            KeyCode::Char('w') => {
                self.open_cloud_path("?from=tui");
            }
            KeyCode::Char('u') => {
                self.open_cloud_path("pricing?from=tui&intent=upgrade");
            }
            _ => {}
        }
    }

    /// Open the cloud rule page for the given rule ID with attribution
    /// query-string so cloud-side analytics can measure how much of the
    /// rule-edit funnel is fed by the TUI.
    fn open_rule_in_cloud(&mut self, rule_id: &str, intent: &str) {
        self.open_cloud_path(&format!("rules/{rule_id}?from=tui&intent={intent}"));
    }

    pub(super) fn handle_rules_key(&mut self, code: KeyCode) {
        // Editing-mode swallows everything except Esc / Enter / Backspace —
        // typing into the search bar must not also navigate the list.
        if let RulesSearch::Editing(mut q) = std::mem::take(&mut self.state.rules_search) {
            let mut reset_selection = false;
            match code {
                KeyCode::Esc => {
                    self.state.rules_search = RulesSearch::Off;
                    reset_selection = true;
                }
                KeyCode::Enter => {
                    self.state.rules_search = committed_search(q);
                }
                KeyCode::Tab => {
                    self.state.rules_search = committed_search(q);
                    self.active_tab = self.active_tab.next();
                    reset_selection = true;
                }
                KeyCode::BackTab => {
                    self.state.rules_search = committed_search(q);
                    self.active_tab = self.active_tab.prev();
                    reset_selection = true;
                }
                KeyCode::Backspace => {
                    q.pop();
                    self.state.rules_search = RulesSearch::Editing(q);
                    self.reset_selection_after_filter_change();
                }
                KeyCode::Char(c) => {
                    q.push(c);
                    self.state.rules_search = RulesSearch::Editing(q);
                    self.reset_selection_after_filter_change();
                }
                _ => {
                    self.state.rules_search = RulesSearch::Editing(q);
                }
            }
            if reset_selection {
                self.reset_selection_after_filter_change();
            }
            return;
        }

        // Pane focus switching always works, even when the list is empty.
        match code {
            KeyCode::Char('h') | KeyCode::Left => {
                self.state.rules_focus = RulesFocus::List;
                return;
            }
            KeyCode::Char('l') | KeyCode::Right => {
                self.state.rules_focus = RulesFocus::Detail;
                return;
            }
            KeyCode::Char('/') => {
                let seed = match &self.state.rules_search {
                    RulesSearch::Filtering(q) => q.clone(),
                    _ => String::new(),
                };
                self.state.rules_search = RulesSearch::Editing(seed);
                self.state.rules_focus = RulesFocus::List;
                return;
            }
            // Filter-cycle keys must work in the empty state too — that's
            // when users most need to escape a zero-match filter.
            KeyCode::Char('f') => {
                self.cycle_origin_filter();
                self.reset_selection_after_filter_change();
                return;
            }
            KeyCode::Char('r') => {
                self.cycle_repo_filter();
                self.reset_selection_after_filter_change();
                return;
            }
            _ => {}
        }

        let len = self.filtered_rules_count();
        if len == 0 {
            return;
        }

        // Cloud CTAs work from either pane — they target the currently
        // selected rule, not the focused pane.
        if matches!(code, KeyCode::Char('e' | 'p' | 's')) {
            if let Some(rule_id) = self.selected_rule().map(|rule| rule.id.clone()) {
                let intent = match code {
                    KeyCode::Char('e') => "edit",
                    KeyCode::Char('p') => "publish",
                    KeyCode::Char('s') => "sources",
                    _ => unreachable!(),
                };
                self.open_rule_in_cloud(&rule_id, intent);
            }
            return;
        }

        match code {
            KeyCode::Char('j') | KeyCode::Down => {
                let current = self.state.rules_list_state.selected().unwrap_or(0);
                let next = (current + 1).min(len - 1);
                self.state.rules_list_state.select(Some(next));
            }
            KeyCode::Char('k') | KeyCode::Up => {
                let current = self.state.rules_list_state.selected().unwrap_or(0);
                self.state
                    .rules_list_state
                    .select(Some(current.saturating_sub(1)));
            }
            KeyCode::Char('g') => {
                self.state.rules_list_state.select(Some(0));
            }
            KeyCode::Char('G') => {
                self.state
                    .rules_list_state
                    .select(Some(len.saturating_sub(1)));
            }
            _ => {}
        }
    }
}

fn committed_search(query: String) -> RulesSearch {
    if query.is_empty() {
        RulesSearch::Off
    } else {
        RulesSearch::Filtering(query)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use ratatui::widgets::ListState;

    use super::*;
    use crate::app::{AppState, RulesOriginFilter, RulesRepoFilter};
    use crate::modals::ModalStack;

    fn test_app() -> App {
        App {
            state: AppState {
                project_root: PathBuf::from("."),
                plan_state: crate::state::PlanState::default(),
                rules: Vec::new(),
                rules_list_state: ListState::default(),
                rules_origin_filter: RulesOriginFilter::All,
                rules_load_error: None,
                rules_source_repos: HashMap::new(),
                fix_outcome_summary: None,
                fix_outcome_daily: Vec::new(),
                fix_outcomes_load_error: None,
                activity_offset: 0,
                activity_visible_rows: 0,
                activity_rows_len: 0,
                current_repo: None,
                rules_repo_filter: RulesRepoFilter::All,
                rules_focus: RulesFocus::List,
                rules_search: RulesSearch::Off,
                wiring: crate::WiringSnapshot::default(),
                status_notice: None,
            },
            active_tab: Tab::Rules,
            should_quit: false,
            pending_exit: crate::TuiExit::Quit,
            modal_stack: ModalStack::new(),
            show_help: false,
        }
    }

    #[test]
    fn tab_commits_rules_search_and_cycles_forward() {
        let mut app = test_app();
        app.state.rules_search = RulesSearch::Editing("auth".to_owned());

        app.handle_key(KeyCode::Tab);

        assert_eq!(app.active_tab, Tab::Activity);
        assert_eq!(
            app.state.rules_search,
            RulesSearch::Filtering("auth".to_owned())
        );
    }

    #[test]
    fn backtab_commits_rules_search_and_cycles_backward() {
        let mut app = test_app();
        app.state.rules_search = RulesSearch::Editing("auth".to_owned());

        app.handle_key(KeyCode::BackTab);

        assert_eq!(app.active_tab, Tab::Settings);
        assert_eq!(
            app.state.rules_search,
            RulesSearch::Filtering("auth".to_owned())
        );
    }

    #[test]
    fn activity_scroll_clamps_against_rendered_row_count() {
        let mut app = test_app();
        app.active_tab = Tab::Activity;
        app.state.activity_rows_len = 20;
        app.state.activity_visible_rows = 8;

        app.handle_activity_key(KeyCode::Char('G'));
        assert_eq!(app.state.activity_offset, 12);

        app.handle_activity_key(KeyCode::Char('j'));
        assert_eq!(app.state.activity_offset, 12);
    }

    #[test]
    fn global_byok_key_handles_low_capacity_event_strip() {
        let mut app = test_app();
        app.state.plan_state.event_strip = crate::state::EventStrip::FixRunsLow {
            used: 198,
            quota: 200,
        };

        app.handle_key(KeyCode::Char('b'));

        assert_eq!(app.pending_exit, crate::TuiExit::RunProvidersAdd);
        assert!(app.should_quit);
    }

    #[test]
    fn colon_shows_command_hint_instead_of_being_swallowed() {
        let mut app = test_app();

        app.handle_key(KeyCode::Char(':'));

        assert!(!app.should_quit);
        assert_eq!(app.pending_exit, crate::TuiExit::Quit);
        assert_eq!(app.active_tab, Tab::Rules);
        assert_eq!(
            app.state.status_notice.as_deref(),
            Some("No command palette yet. Press ? for shortcuts.")
        );
    }

    #[test]
    fn colon_is_search_text_while_rules_search_is_editing() {
        let mut app = test_app();
        app.state.rules_search = RulesSearch::Editing("origin".to_owned());

        app.handle_key(KeyCode::Char(':'));

        assert_eq!(
            app.state.rules_search,
            RulesSearch::Editing("origin:".to_owned())
        );
        assert!(app.state.status_notice.is_none());
    }

    #[test]
    fn committed_search_drops_empty_query() {
        assert_eq!(committed_search(String::new()), RulesSearch::Off);
        assert_eq!(
            committed_search("cache".to_owned()),
            RulesSearch::Filtering("cache".to_owned())
        );
    }
}
