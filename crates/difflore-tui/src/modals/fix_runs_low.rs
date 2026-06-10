//! `FixRunsLow` modal · Team → Team Plus.
//!
//! Renders an amber-accented double-bordered box with an
//! `ascii_bar_counts` progress bar, usage display, and pitch. Footer
//! keys: `[u]` upgrade Team Plus / `[b]` BYOK / `[esc]` dismiss.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

use crate::layout::centered_rect_abs;
use crate::theme::Theme;
use crate::widgets::ascii_bar::ascii_bar_counts;

#[derive(Clone, Debug)]
pub struct FixRunsLowState {
    pub used: u32,
    pub quota: u32,
}

impl FixRunsLowState {
    pub const fn new(used: u32, quota: u32) -> Self {
        Self { used, quota }
    }

    fn percent(&self) -> u32 {
        capacity_percent(self.used, self.quota)
    }
}

pub fn render(frame: &mut Frame<'_>, area: Rect, state: &FixRunsLowState, theme: &Theme) {
    let area = centered_rect_abs(60, 14, area);
    frame.render_widget(Clear, area);

    let accent = Style::default().fg(theme.warn);
    let muted = Style::default().fg(theme.muted);
    let strong = Style::default()
        .fg(theme.foreground)
        .add_modifier(Modifier::BOLD);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(accent)
        .title(Span::styled(
            " ⚠ REVIEW MEMORY CAPACITY LOW ",
            accent.add_modifier(Modifier::BOLD),
        ))
        .title_alignment(Alignment::Center)
        .style(Style::default().bg(theme.bg));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(1), // bar
            Constraint::Length(1), // pct line
            Constraint::Length(1), // spacer
            Constraint::Length(1), // usage
            Constraint::Length(1), // projection (optional)
            Constraint::Length(1), // spacer
            Constraint::Length(2), // pitch
            Constraint::Min(0),    // filler
            Constraint::Length(1), // footer
        ])
        .split(inner);

    let bar = ascii_bar_counts(state.used, state.quota, 30);
    frame.render_widget(
        Paragraph::new(Line::styled(bar, accent)).alignment(Alignment::Center),
        chunks[0],
    );

    let pct = state.percent();
    frame.render_widget(
        Paragraph::new(Line::styled(format!("{pct}% of monthly capacity"), muted))
            .alignment(Alignment::Center),
        chunks[1],
    );

    let usage_spans = vec![
        Span::styled(state.used.to_string(), strong),
        Span::styled(" / ", muted),
        Span::styled(state.quota.to_string(), strong),
        Span::styled(" review-memory capacity used", muted),
    ];
    frame.render_widget(
        Paragraph::new(Line::from(usage_spans)).alignment(Alignment::Center),
        chunks[3],
    );

    // chunks[4] is a blank spacer slot; kept so the bar/usage/pitch
    // spacing stays fixed.

    let pitch = vec![
        Line::styled(
            "Team Plus: larger shared memory + Reviewer Context.",
            strong,
        ),
        Line::styled("BYOK keeps local extraction on your own keys.", muted),
    ];
    frame.render_widget(
        Paragraph::new(pitch).alignment(Alignment::Center),
        chunks[6],
    );

    let footer = Line::from(vec![
        Span::styled("[u]", accent.add_modifier(Modifier::BOLD)),
        Span::styled(" upgrade Team Plus · 14d   ", muted),
        Span::styled("[b]", strong),
        Span::styled(" configure BYOK   ", muted),
        Span::styled("[esc]", strong),
        Span::styled(" dismiss", muted),
    ]);
    frame.render_widget(
        Paragraph::new(footer).alignment(Alignment::Center),
        chunks[8],
    );
}

fn capacity_percent(used: u32, quota: u32) -> u32 {
    if quota == 0 {
        return 0;
    }
    let capped = used.min(quota);
    let rounded = (u64::from(capped) * 200 + u64::from(quota)) / (u64::from(quota) * 2);
    u32::try_from(rounded).unwrap_or(100).min(100)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_clamps_above_one() {
        let s = FixRunsLowState::new(500, 100);
        assert_eq!(s.percent(), 100);
    }

    #[test]
    fn percent_zero_quota_is_zero() {
        let s = FixRunsLowState::new(10, 0);
        assert_eq!(s.percent(), 0);
    }

    #[test]
    fn percent_eighty_percent() {
        let s = FixRunsLowState::new(80, 100);
        assert_eq!(s.percent(), 80);
    }

    #[test]
    fn percent_rounds_and_clamps_with_integer_math() {
        assert_eq!(capacity_percent(1, 3), 33);
        assert_eq!(capacity_percent(500, 100), 100);
        assert_eq!(capacity_percent(1, 0), 0);
    }
}
