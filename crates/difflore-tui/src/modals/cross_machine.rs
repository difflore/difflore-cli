//! `CrossMachine` modal · Free → Team.
//!
//! Renders a centered, double-bordered, info-blue accented box with
//! two-device ASCII art and the
//! `[s] sync · 14d trial   [l] keep local   [esc] dismiss` footer.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

use crate::layout::centered_rect_abs;
use crate::theme::Theme;
use crate::widgets::truncate;

/// View-model for the cross-machine modal. Built by `draw_modal` from
/// the `CrossMachine { other_host }` event.
///
/// The modal only ever has the source host to show: `draw_modal` never
/// supplied a `new_host` (always `""`) or `source_rules` (always
/// empty), so the empty second-device glyph and the example-rule slots
/// were removed along with those fields.
#[derive(Clone, Debug)]
pub struct CrossMachineState {
    /// Hostname of the source machine where rules already live.
    pub source_host: String,
}

/// Render the modal centered inside `area`. Uses `theme.info` as
/// the accent.
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

    // Source-host device glyph. There is no second (new-host) device or
    // example-rule list — those inputs were always empty, so only the
    // source host is illustrated.
    let mut device: Vec<Line<'_>> = Vec::with_capacity(8);
    device.push(Line::styled("┌──────────────┐", pewter));
    let host_label = format!("│ {:<12} │", truncate(&state.source_host, 12));
    device.push(Line::styled(host_label, pewter));
    device.push(Line::styled("├──────────────┤", pewter));
    for _ in 0..3 {
        // `source_rules` was always empty, so every slot rendered the
        // `·` placeholder; keep the exact same glyph row.
        device.push(Line::styled(format!("│ • {:<10} │", "·"), pewter));
    }
    device.push(Line::styled("└──────────────┘", pewter));
    device.push(Line::raw(""));
    frame.render_widget(
        Paragraph::new(device).alignment(Alignment::Center),
        chunks[0],
    );

    // Pitch lines.
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

    // Footer key hints.
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
