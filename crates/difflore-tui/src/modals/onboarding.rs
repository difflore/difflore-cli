//! `Onboarding` 5-step wizard · all tiers.
//!
//! Renders an emerald-accented double-bordered box with a horizontal
//! split: the left rail shows all 5 steps (`✓` done · `▶` current ·
//! `·` pending), and the right pane shows the current step's title,
//! copy, and CLI command in a highlighted box. Footer: `[Enter]` do
//! step / `[s]` skip / step N of 5.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

use crate::layout::centered_rect_abs;
use crate::theme::Theme;

/// 5-step onboarding wizard. Steps are 1-indexed; `current = 6`
/// means the wizard is complete and `App` should dismiss the modal.
#[derive(Clone, Debug)]
pub struct OnboardingState {
    pub current: u8,
}

impl OnboardingState {
    pub fn new(step: u8) -> Self {
        Self {
            current: step.clamp(1, 5),
        }
    }
}

struct StepCopy {
    title: &'static str,
    body: &'static str,
    command: &'static str,
}

const STEPS: [StepCopy; 5] = [
    StepCopy {
        title: "Wire agents",
        body: "Connect this repo so Claude, Codex, Cursor, and friends can recall team review memory before editing.",
        command: "difflore agents install",
    },
    StepCopy {
        title: "Pick provider",
        body: "Choose the model used when DiffLore turns remembered review judgment into patch suggestions.",
        command: "difflore providers setup",
    },
    StepCopy {
        title: "Import reviews",
        body: "Sign in to cloud, then teach DiffLore from past PR comments so repeat review feedback becomes reusable team memory.",
        command: "difflore cloud login && difflore import-reviews --max-prs 50 --upload",
    },
    StepCopy {
        title: "Preview recall",
        body: "Check the exact memories your local agents would receive for the current diff.",
        command: "difflore recall --diff",
    },
    StepCopy {
        title: "First fix",
        body: "Preview patches from team memory. Nothing changes until you choose to apply them.",
        command: "difflore fix --preview",
    },
];

const SHORT_LABELS: [&str; 5] = [
    "Wire agents",
    "Pick provider",
    "Import reviews",
    "Preview recall",
    "First fix",
];

pub fn render(frame: &mut Frame<'_>, area: Rect, state: &OnboardingState, theme: &Theme) {
    let area = centered_rect_abs(72, 18, area);
    frame.render_widget(Clear, area);

    let accent = Style::default().fg(theme.success);
    let muted = Style::default().fg(theme.muted);
    let pewter = Style::default().fg(theme.diff);
    let strong = Style::default()
        .fg(theme.foreground)
        .add_modifier(Modifier::BOLD);
    let cmd_box = Style::default()
        .fg(theme.foreground)
        .bg(theme.highlight_bg)
        .add_modifier(Modifier::BOLD);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(accent)
        .title(Span::styled(
            " ▶ DIFFLORE · FIRST RUN ",
            accent.add_modifier(Modifier::BOLD),
        ))
        .title_alignment(Alignment::Center)
        .style(Style::default().bg(theme.bg));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Body: split horizontal · left rail (24 cols) + right pane.
    let body = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(inner);

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(24), Constraint::Min(0)])
        .split(body[0]);

    // Left rail.
    let cur = state.current.clamp(1, 6);
    let mut rail: Vec<Line<'_>> = Vec::with_capacity(7);
    rail.push(Line::styled("STEPS", muted.add_modifier(Modifier::BOLD)));
    rail.push(Line::raw(""));
    for (i, label) in SHORT_LABELS.iter().enumerate() {
        let n = u8::try_from(i + 1).unwrap_or(5);
        match n.cmp(&cur) {
            std::cmp::Ordering::Less => {
                rail.push(Line::from(vec![
                    Span::styled(" ✓ ", accent),
                    Span::styled(
                        (*label).to_owned(),
                        pewter.add_modifier(Modifier::CROSSED_OUT),
                    ),
                ]));
            }
            std::cmp::Ordering::Equal => {
                rail.push(Line::from(vec![
                    Span::styled(" ▶ ", accent.add_modifier(Modifier::BOLD)),
                    Span::styled((*label).to_owned(), strong),
                ]));
            }
            std::cmp::Ordering::Greater => {
                rail.push(Line::from(vec![
                    Span::styled(" · ", muted),
                    Span::styled((*label).to_owned(), muted),
                ]));
            }
        }
    }
    frame.render_widget(Paragraph::new(rail), cols[0]);

    // Right pane.
    let step = &STEPS[usize::from(cur - 1)];
    let pane = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // step counter
            Constraint::Length(1), // spacer
            Constraint::Length(1), // title
            Constraint::Length(1), // spacer
            Constraint::Length(3), // body (wrapped)
            Constraint::Length(1), // spacer
            Constraint::Length(1), // command box
            Constraint::Min(0),    // filler
        ])
        .split(cols[1]);

    frame.render_widget(
        Paragraph::new(Line::styled(format!("Step {cur} of 5"), muted)),
        pane[0],
    );
    frame.render_widget(
        Paragraph::new(Line::styled(
            step.title.to_owned(),
            accent.add_modifier(Modifier::BOLD),
        )),
        pane[2],
    );
    frame.render_widget(
        Paragraph::new(Line::styled(step.body.to_owned(), strong))
            .wrap(ratatui::widgets::Wrap { trim: true }),
        pane[4],
    );
    frame.render_widget(
        Paragraph::new(Line::styled(format!("  $ {}  ", step.command), cmd_box)),
        pane[6],
    );

    // Footer.
    let footer = Line::from(vec![
        Span::styled("[Enter]", accent.add_modifier(Modifier::BOLD)),
        Span::styled(" do step   ", muted),
        Span::styled("[s]", strong),
        Span::styled(" skip   ", muted),
        Span::styled(format!("step {cur} of 5"), muted),
    ]);
    frame.render_widget(Paragraph::new(footer).alignment(Alignment::Center), body[1]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_clamps_into_range() {
        assert_eq!(OnboardingState::new(0).current, 1);
        assert_eq!(OnboardingState::new(99).current, 5);
        assert_eq!(OnboardingState::new(3).current, 3);
    }

    #[test]
    fn five_steps_present() {
        assert_eq!(STEPS.len(), 5);
        assert_eq!(SHORT_LABELS.len(), 5);
    }
}
