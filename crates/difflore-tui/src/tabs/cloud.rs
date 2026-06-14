use std::path::Path;

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

pub fn render(frame: &mut ratatui::Frame<'_>, area: Rect, _project_root: &Path) {
    let outer = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(crate::theme::Theme::current().border))
        .title(Span::styled(
            " team ↗ ",
            Style::default().fg(crate::theme::Theme::current().accent),
        ));
    let inner = outer.inner(area);
    frame.render_widget(outer, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5), // banner
            Constraint::Length(4), // ASCII art
            Constraint::Length(2), // description
            Constraint::Length(6), // hotkey list
            Constraint::Min(1),    // footer
        ])
        .split(inner);

    draw_banner(frame, chunks[0]);
    draw_art(frame, chunks[1]);
    draw_description(frame, chunks[2]);
    draw_hotkeys(frame, chunks[3]);
    draw_footer(frame, chunks[4]);
}

fn draw_banner(frame: &mut ratatui::Frame<'_>, area: Rect) {
    let text = Paragraph::new(vec![
        Line::from(""),
        Line::from(Span::styled(
            "Team review judgment becomes shared agent memory through cloud.",
            Style::default()
                .fg(crate::theme::Theme::current().foreground)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Your TUI is the local cockpit. Team features (GitHub App ingest,",
            Style::default().fg(crate::theme::Theme::current().muted),
        )),
        Line::from(Span::styled(
            format!(
                "rule extraction, dashboard links) live at {}.",
                difflore_core::cloud::endpoints::web_host_display()
            ),
            Style::default().fg(crate::theme::Theme::current().muted),
        )),
    ])
    .alignment(Alignment::Center);
    frame.render_widget(text, area);
}

fn draw_art(frame: &mut ratatui::Frame<'_>, area: Rect) {
    // A compact ASCII cloud glyph using ratatui-safe box-drawing chars.
    let art = Paragraph::new(vec![
        Line::from(Span::styled(
            "       ╭─────────────╮       ",
            Style::default().fg(crate::theme::Theme::current().accent),
        )),
        Line::from(Span::styled(
            "     ╭─┤  difflore   ├─╮     ",
            Style::default().fg(crate::theme::Theme::current().accent),
        )),
        Line::from(Span::styled(
            "     │      cloud      │     ",
            Style::default().fg(crate::theme::Theme::current().accent),
        )),
        Line::from(Span::styled(
            "     ╰─────────────────╯     ",
            Style::default().fg(crate::theme::Theme::current().accent),
        )),
    ])
    .alignment(Alignment::Center);
    frame.render_widget(art, area);
}

fn draw_description(frame: &mut ratatui::Frame<'_>, area: Rect) {
    let text = Paragraph::new(vec![Line::from(Span::styled(
        "Cloud login lets `difflore cloud sync` refresh the rules Claude, Codex, Cursor, and friends recall.",
        Style::default().fg(crate::theme::Theme::current().muted),
    ))])
    .alignment(Alignment::Center);
    frame.render_widget(text, area);
}

fn draw_hotkeys(frame: &mut ratatui::Frame<'_>, area: Rect) {
    let text = Paragraph::new(vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("    "),
            key_badge("c"),
            Span::raw("  "),
            Span::styled(
                "Review extracted memories",
                Style::default().fg(crate::theme::Theme::current().foreground),
            ),
            Span::raw("   "),
            Span::styled(
                "→ /team/candidates",
                Style::default().fg(crate::theme::Theme::current().muted),
            ),
        ]),
        Line::from(vec![
            Span::raw("    "),
            key_badge("d"),
            Span::raw("  "),
            Span::styled(
                "Open team rules dashboard",
                Style::default().fg(crate::theme::Theme::current().foreground),
            ),
            Span::raw("   "),
            Span::styled(
                "→ /  (full cloud)",
                Style::default().fg(crate::theme::Theme::current().muted),
            ),
        ]),
        Line::from(vec![
            Span::raw("    "),
            key_badge("o"),
            Span::raw("  "),
            Span::styled(
                "Open dashboard (alias of d)",
                Style::default().fg(crate::theme::Theme::current().foreground),
            ),
        ]),
    ])
    .wrap(Wrap { trim: false });
    frame.render_widget(text, area);
}

fn draw_footer(frame: &mut ratatui::Frame<'_>, area: Rect) {
    let text = Paragraph::new(vec![
        Line::from(""),
        Line::from(Span::styled(
            "First-day loop: `difflore init` → `difflore import-reviews --upload` → `difflore cloud sync` → `difflore recall --diff`.",
            Style::default().fg(crate::theme::Theme::current().muted),
        )),
    ])
    .alignment(Alignment::Center);
    frame.render_widget(text, area);
}

fn key_badge(key: &str) -> Span<'static> {
    Span::styled(
        format!(" {key} "),
        Style::default()
            .fg(crate::theme::Theme::current().foreground)
            .bg(crate::theme::Theme::current().highlight_bg)
            .add_modifier(Modifier::BOLD),
    )
}
