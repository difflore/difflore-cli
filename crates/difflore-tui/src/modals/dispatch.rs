//! Modal dispatch: maps a `(modal, key)` pair to one [`ModalAction`] and
//! fans a [`Modal`] out to its renderer. Per-modal keymaps live next to
//! each modal's copy in `modals/<name>.rs`; this file only routes.

use crossterm::event::KeyCode;
use ratatui::layout::Rect;

use crate::theme::Theme;

use super::{Modal, cross_machine, fix_runs_low, onboarding, teammate_caught};

/// What a modal keypress asks the app to do. The app layer owns the side
/// effects (quit, browser, notices); modals stay pure copy + mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ModalAction {
    Dismiss,
    Exit(crate::TuiExit),
    OpenCloud(&'static str),
    Notice(&'static str),
}

/// `Esc` dismisses any modal; everything else is the modal's own keymap.
pub(crate) const fn action_for_key(modal: &Modal, code: KeyCode) -> Option<ModalAction> {
    if matches!(code, KeyCode::Esc) {
        return Some(ModalAction::Dismiss);
    }
    match modal {
        Modal::CrossMachine { .. } => cross_machine::action_for_key(code),
        Modal::TeammateCaught { .. } => teammate_caught::action_for_key(code),
        Modal::FixRunsLow { .. } => fix_runs_low::action_for_key(code),
        Modal::Onboarding { step } => onboarding::action_for_key(*step, code),
    }
}

/// Render `modal` into the already-cleared `panel` rect.
pub(crate) fn render(frame: &mut ratatui::Frame<'_>, panel: Rect, modal: &Modal, theme: &Theme) {
    match modal {
        Modal::CrossMachine { other_host } => {
            let state = cross_machine::CrossMachineState {
                source_host: other_host.clone(),
            };
            cross_machine::render(frame, panel, &state, theme);
        }
        Modal::TeammateCaught {
            rule,
            teammate,
            fired_at,
        } => {
            let state = teammate_caught::TeammateCaughtState {
                rule: rule.clone(),
                teammate: teammate.clone(),
                fired_at: fired_at.clone(),
            };
            teammate_caught::render(frame, panel, &state, theme);
        }
        Modal::FixRunsLow { used, quota } => {
            let state = fix_runs_low::FixRunsLowState::new(*used, *quota);
            fix_runs_low::render(frame, panel, &state, theme);
        }
        Modal::Onboarding { step } => {
            let state = onboarding::OnboardingState::new(*step);
            onboarding::render(frame, panel, &state, theme);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modal_ctas_map_to_real_launch_actions() {
        assert_eq!(
            action_for_key(
                &Modal::FixRunsLow {
                    used: 190,
                    quota: 200
                },
                KeyCode::Char('b')
            ),
            Some(ModalAction::Exit(crate::TuiExit::RunProvidersAdd))
        );
        assert_eq!(
            action_for_key(&Modal::Onboarding { step: 1 }, KeyCode::Enter),
            Some(ModalAction::Exit(crate::TuiExit::RunInit))
        );
        assert_eq!(
            action_for_key(
                &Modal::CrossMachine {
                    other_host: "work".to_owned()
                },
                KeyCode::Char('s')
            ),
            Some(ModalAction::Exit(crate::TuiExit::RunCloudLogin))
        );
    }

    #[test]
    fn esc_dismisses_every_modal() {
        assert_eq!(
            action_for_key(&Modal::Onboarding { step: 1 }, KeyCode::Esc),
            Some(ModalAction::Dismiss)
        );
    }

    #[test]
    fn unknown_modal_key_is_ignored() {
        assert_eq!(
            action_for_key(&Modal::Onboarding { step: 1 }, KeyCode::Char('x')),
            None
        );
    }
}
