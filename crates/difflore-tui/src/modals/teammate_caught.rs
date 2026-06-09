//! `TeammateCaught` modal · Free → Team.
//!
//! Renders an emerald-accented double-bordered box with a hero line,
//! a diff hunk illustration, and a 2-column NOW vs WITH TEAM
//! comparison. Footer keys: `[t]` enable Team / `[c]` just comment /
//! `[esc]` dismiss.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

use crate::layout::centered_rect_abs;
use crate::theme::Theme;

/// View-model for the teammate-caught modal. `draw_modal` builds this
/// via field literals; there is no preview-hunk input — the modal
/// always renders the synthetic `- bad / + good` illustration, so the
/// former `hunk: Vec<HunkLine>` field (always empty) and its enum were
/// removed.
#[derive(Clone, Debug)]
pub struct TeammateCaughtState {
    pub rule: String,
    pub teammate: String,
    pub fired_at: String,
}

pub fn render(frame: &mut Frame<'_>, area: Rect, state: &TeammateCaughtState, theme: &Theme) {
    let area = centered_rect_abs(70, 22, area);
    frame.render_widget(Clear, area);

    let accent = Style::default().fg(theme.success);
    let muted = Style::default().fg(theme.muted);
    let danger = Style::default().fg(theme.danger);
    let strong = Style::default()
        .fg(theme.foreground)
        .add_modifier(Modifier::BOLD);
    let pewter = Style::default().fg(theme.diff);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(accent)
        .title(Span::styled(
            " ⚡ YOUR RULE FIRED ",
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
            Constraint::Length(2), // hero
            Constraint::Length(6), // hunk
            Constraint::Length(1), // spacer
            Constraint::Length(6), // 2-col compare
            Constraint::Min(0),    // filler
            Constraint::Length(1), // footer
        ])
        .split(inner);

    // Hero.
    let hero = vec![
        Line::from(vec![
            Span::styled("Your local rule just caught ", strong),
            Span::styled(
                format!("@{}", state.teammate),
                accent.add_modifier(Modifier::BOLD),
            ),
            Span::styled("'s PR.", strong),
        ]),
        Line::from(vec![Span::styled(
            format!("· fired {}", state.fired_at),
            muted,
        )]),
    ];
    frame.render_widget(Paragraph::new(hero).alignment(Alignment::Center), chunks[0]);

    // Synthetic diff hunk illustration. There is no per-event hunk
    // input — this placeholder is always what renders.
    let mut hunk_lines: Vec<Line<'_>> = vec![
        Line::styled("  - if (user.role === 'admin') {", danger),
        Line::styled("  + if (canAdmin(user)) {", accent),
    ];
    hunk_lines.push(Line::raw(""));
    hunk_lines.push(Line::from(vec![
        Span::styled("  ▶ rule fired: ", accent.add_modifier(Modifier::BOLD)),
        Span::styled(state.rule.clone(), strong),
    ]));
    frame.render_widget(Paragraph::new(hunk_lines), chunks[1]);

    // 2-column NOW vs WITH TEAM comparison.
    let cmp = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(chunks[3]);

    let now_col = vec![
        Line::styled("NOW", muted.add_modifier(Modifier::BOLD)),
        Line::styled("· local-only", pewter),
        Line::styled("· teammates miss it", pewter),
        Line::styled("· comments by hand", pewter),
    ];
    let team_col = vec![
        Line::styled("WITH TEAM", accent.add_modifier(Modifier::BOLD)),
        Line::styled("· auto-comment on PRs", strong),
        Line::styled("· shared rule library", strong),
        Line::styled("· 14-day trial", accent),
    ];
    frame.render_widget(Paragraph::new(now_col), cmp[0]);
    frame.render_widget(Paragraph::new(team_col), cmp[1]);

    // Footer.
    let footer = Line::from(vec![
        Span::styled("[t]", accent.add_modifier(Modifier::BOLD)),
        Span::styled(" enable Team · 14d   ", muted),
        Span::styled("[c]", strong),
        Span::styled(" just comment   ", muted),
        Span::styled("[esc]", strong),
        Span::styled(" dismiss", muted),
    ]);
    frame.render_widget(
        Paragraph::new(footer).alignment(Alignment::Center),
        chunks[5],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_holds_event_fields() {
        let s = TeammateCaughtState {
            rule: "no-admin-checks".to_owned(),
            teammate: "alice".to_owned(),
            fired_at: "2m ago".to_owned(),
        };
        assert_eq!(s.rule, "no-admin-checks");
        assert_eq!(s.teammate, "alice");
        assert_eq!(s.fired_at, "2m ago");
    }
}
