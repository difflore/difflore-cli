//! `CrossMachine` modal · Free → Team.
//!
//! Renders a centered, double-bordered, info-blue accented box with device
//! ASCII art and the `[s] sync · 14d trial   [l] keep local   [esc] dismiss`
//! footer.

use crossterm::event::KeyCode;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

use crate::theme::Theme;
use crate::widgets::center::centered_rect_abs;
use crate::widgets::truncate;

use super::dispatch::ModalAction;

/// View-model for the cross-machine modal. Built by the modal dispatcher
/// from the `CrossMachine { other_host }` event; only the source host is
/// shown.
#[derive(Clone, Debug)]
pub struct CrossMachineState {
    /// Hostname of the source machine where rules already live.
    pub source_host: String,
}

/// Keymap matching the footer: `[s] sync · 14d trial`, `[l] keep local`.
pub(crate) const fn action_for_key(code: KeyCode) -> Option<ModalAction> {
    match code {
        KeyCode::Char('s') => Some(ModalAction::Exit(crate::TuiExit::RunCloudLogin)),
        KeyCode::Char('l') => Some(ModalAction::Notice("Kept this machine local for now.")),
        _ => None,
    }
}

/// Render the modal centered inside `area`, using `theme.info` as the accent.
pub fn render(frame: &mut Frame<'_>, area: Rect, state: &CrossMachineState, theme: &Theme) {
    let area = centered_rect_abs(64, 18, area);
    frame.render_widget(Clear, area);

    let accent = Style::default().fg(theme.info);
    let muted = Style::default().fg(theme.muted);
    let pewter = Style::default().fg(theme.diff);
    let strong = Style::default()
        .fg(theme.foreground)
        .add_modifier(Modifier::BOLD);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(accent)
        .title(Span::styled(
            " ⚡ NEW DEVICE DETECTED ",
            accent.add_modifier(Modifier::BOLD),
        ))
        .title_alignment(Alignment::Center)
        .style(Style::default().bg(theme.bg));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Body layout: art (8) · spacer · pitch lines · footer
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(8), // ASCII art
            Constraint::Length(1), // spacer
            Constraint::Length(2), // pitch
            Constraint::Min(0),    // filler
            Constraint::Length(1), // footer
        ])
        .split(inner);

    // Source-host device glyph. Only the source host is illustrated.
    let mut device: Vec<Line<'_>> = Vec::with_capacity(8);
    device.push(Line::styled("┌──────────────┐", pewter));
    let host_label = format!("│ {:<12} │", truncate(&state.source_host, 12));
    device.push(Line::styled(host_label, pewter));
    device.push(Line::styled("├──────────────┤", pewter));
    for _ in 0..3 {
        // No rule list to show; each slot renders the `·` placeholder.
        device.push(Line::styled(format!("│ • {:<10} │", "·"), pewter));
    }
    device.push(Line::styled("└──────────────┘", pewter));
    device.push(Line::raw(""));
    frame.render_widget(
        Paragraph::new(device).alignment(Alignment::Center),
        chunks[0],
    );

    let pitch = vec![
        Line::from(vec![Span::styled(
            "Sync your rules across machines.",
            strong,
        )]),
        Line::from(vec![Span::styled(
            "Team trial · 14 days · no card required.",
            muted,
        )]),
    ];
    frame.render_widget(
        Paragraph::new(pitch).alignment(Alignment::Center),
        chunks[2],
    );

    let footer = Line::from(vec![
        Span::styled("[s]", accent.add_modifier(Modifier::BOLD)),
        Span::styled(" sync · 14d trial   ", muted),
        Span::styled("[l]", strong),
        Span::styled(" keep local   ", muted),
        Span::styled("[esc]", strong),
        Span::styled(" dismiss", muted),
    ]);
    frame.render_widget(
        Paragraph::new(footer).alignment(Alignment::Center),
        chunks[4],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_keeps_short_strings_intact() {
        assert_eq!(truncate("abc", 5), "abc");
    }

    #[test]
    fn truncate_adds_ellipsis_when_long() {
        assert_eq!(truncate("abcdefghij", 5), "abcd…");
    }
}
