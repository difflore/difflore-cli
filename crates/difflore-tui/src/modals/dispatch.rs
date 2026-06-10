use crossterm::event::KeyCode;
use ratatui::layout::Rect;
use ratatui::widgets::Clear;

use crate::layout::centered_rect_pct;
use crate::modals::Modal;
use crate::theme::Theme;

use super::App;
use super::render::draw_backdrop;

impl App {
    pub(super) fn handle_modal_key(&mut self, modal: &Modal, code: KeyCode) -> bool {
        let Some(action) = modal_action_for_key(modal, code) else {
            return false;
        };

        match action {
            ModalAction::Dismiss => {}
            ModalAction::Exit(exit) => {
                self.pending_exit = exit;
                self.should_quit = true;
            }
            ModalAction::OpenCloud(path) => self.open_cloud_path(path),
            ModalAction::Notice(message) => self.set_status_notice(message),
        }
        true
    }

    #[allow(clippy::unused_self)] // reason: kept as method for symmetry with sibling draw_* methods
    pub(super) fn draw_modal(&self, frame: &mut ratatui::Frame<'_>, area: Rect, modal: &Modal) {
        let theme = Theme::current();
        draw_backdrop(frame, area, &theme);
        let panel = centered_rect_pct(60, 12, area);
        frame.render_widget(Clear, panel);
        match modal {
            Modal::CrossMachine { other_host } => {
                let state = crate::modals::cross_machine::CrossMachineState {
                    source_host: other_host.clone(),
                };
                crate::modals::cross_machine::render(frame, panel, &state, &theme);
            }
            Modal::TeammateCaught {
                rule,
                teammate,
                fired_at,
            } => {
                let state = crate::modals::teammate_caught::TeammateCaughtState {
                    rule: rule.clone(),
                    teammate: teammate.clone(),
                    fired_at: fired_at.clone(),
                };
                crate::modals::teammate_caught::render(frame, panel, &state, &theme);
            }
            Modal::FixRunsLow { used, quota } => {
                let state = crate::modals::fix_runs_low::FixRunsLowState::new(*used, *quota);
                crate::modals::fix_runs_low::render(frame, panel, &state, &theme);
            }
            Modal::Onboarding { step } => {
                let state = crate::modals::onboarding::OnboardingState::new(*step);
                crate::modals::onboarding::render(frame, panel, &state, &theme);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ModalAction {
    Dismiss,
    Exit(crate::TuiExit),
    OpenCloud(&'static str),
    Notice(&'static str),
}

const fn modal_action_for_key(modal: &Modal, code: KeyCode) -> Option<ModalAction> {
    match (modal, code) {
        (_, KeyCode::Esc) => Some(ModalAction::Dismiss),
        (Modal::CrossMachine { .. }, KeyCode::Char('s')) => {
            Some(ModalAction::Exit(crate::TuiExit::RunCloudLogin))
        }
        (Modal::CrossMachine { .. }, KeyCode::Char('l')) => {
            Some(ModalAction::Notice("Kept this machine local for now."))
        }
        (Modal::TeammateCaught { .. }, KeyCode::Char('t')) => {
            Some(ModalAction::OpenCloud("pricing?from=tui&intent=team_trial"))
        }
        (Modal::TeammateCaught { .. }, KeyCode::Char('c')) => Some(ModalAction::Notice(
            "Kept local comment flow. Open Cloud when you want team auto-comments.",
        )),
        (Modal::FixRunsLow { .. }, KeyCode::Char('u')) => {
            Some(ModalAction::OpenCloud("pricing?from=tui&intent=upgrade"))
        }
        (Modal::FixRunsLow { .. }, KeyCode::Char('b')) => {
            Some(ModalAction::Exit(crate::TuiExit::RunProvidersAdd))
        }
        (Modal::Onboarding { step }, KeyCode::Enter) => Some(onboarding_enter_action(*step)),
        (Modal::Onboarding { .. }, KeyCode::Char('s')) => {
            Some(ModalAction::Notice("Onboarding skipped for this launch."))
        }
        _ => None,
    }
}

const fn onboarding_enter_action(step: u8) -> ModalAction {
    match step {
        1 => ModalAction::Exit(crate::TuiExit::RunInit),
        2 => ModalAction::Exit(crate::TuiExit::RunProvidersAdd),
        3 => ModalAction::Exit(crate::TuiExit::RunCloudLogin),
        4 => ModalAction::Notice("Run `difflore recall --diff` after closing the TUI."),
        5 => ModalAction::Notice("Run `difflore fix --preview` after closing the TUI."),
        _ => ModalAction::Dismiss,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modal_ctas_map_to_real_launch_actions() {
        assert_eq!(
            modal_action_for_key(
                &Modal::FixRunsLow {
                    used: 190,
                    quota: 200
                },
                KeyCode::Char('b')
            ),
            Some(ModalAction::Exit(crate::TuiExit::RunProvidersAdd))
        );
        assert_eq!(
            modal_action_for_key(&Modal::Onboarding { step: 1 }, KeyCode::Enter),
            Some(ModalAction::Exit(crate::TuiExit::RunInit))
        );
        assert_eq!(
            modal_action_for_key(
                &Modal::CrossMachine {
                    other_host: "work".to_owned()
                },
                KeyCode::Char('s')
            ),
            Some(ModalAction::Exit(crate::TuiExit::RunCloudLogin))
        );
    }

    #[test]
    fn unknown_modal_key_is_ignored() {
        assert_eq!(
            modal_action_for_key(&Modal::Onboarding { step: 1 }, KeyCode::Char('x')),
            None
        );
    }
}
