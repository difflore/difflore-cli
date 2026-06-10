use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Tabs, Wrap};

use crate::layout::centered_rect_abs;
use crate::tabs::{self, Tab};
use crate::theme::{self, Theme};
use crate::widgets::SmartStatusBar;

use super::build_status_bar_view;
use super::{App, cloud_memory_rule_count, raw_local_rule_count};

impl App {
    pub(super) fn draw(&mut self, frame: &mut ratatui::Frame<'_>) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2), // header
                Constraint::Length(2), // tabs
                Constraint::Min(1),    // body
                Constraint::Length(1), // context hint strip
                Constraint::Length(1), // status bar (plan / quotas)
            ])
            .split(frame.area());

        let theme = Theme::current();
        self.draw_header(frame, chunks[0], &theme);
        self.draw_tabs(frame, chunks[1], &theme);
        self.draw_body(frame, chunks[2]);
        self.draw_hint_strip(frame, chunks[3], &theme);
        self.draw_status_bar(frame, chunks[4], &theme);

        if let Some(modal) = self.modal_stack.current().cloned() {
            self.draw_modal(frame, frame.area(), &modal);
        }

        if self.show_help {
            draw_help_overlay(frame, frame.area(), &theme);
        }
    }

    fn draw_header(&self, frame: &mut ratatui::Frame<'_>, area: Rect, t: &Theme) {
        // Value strip; segments collapse from the right when the terminal is
        // narrow.
        let memory_count = cloud_memory_rule_count(&self.state.rules);
        let raw_count = raw_local_rule_count(&self.state.rules);
        let primary_count = if memory_count > 0 {
            memory_count
        } else {
            self.state.rules.len()
        };
        let wiring = &self.state.wiring;
        let provider = wiring
            .provider_name
            .clone()
            .unwrap_or_else(|| "no provider".to_owned());
        let cloud_status = if wiring.cloud_logged_in {
            "Cloud sync on".to_owned()
        } else {
            "Cloud sync off".to_owned()
        };
        // `applied` = patch accepted and applied; shown as "accepted fixes".
        let accepted_fixes = self
            .state
            .fix_outcome_summary
            .as_ref()
            .map_or(0, |s| s.applied);

        let mut segments: Vec<String> = vec![
            format!("{primary_count} memories"),
            format!(
                "{}/{} agents wired for memory",
                wiring.agents_installed, wiring.agents_detected
            ),
            provider,
            cloud_status,
            format!("{accepted_fixes} accepted fixes"),
        ];
        if raw_count > 0 {
            segments.insert(1, format!("{raw_count} local drafts"));
        }

        let width = usize::from(area.width);
        // Always keep at least the first segment ("memories").
        let keep = header_segments_to_keep(width, segments.len());
        let strip = segments[..keep.min(segments.len())].join(" · ");

        let wordmark = Paragraph::new(Line::from(vec![
            Span::styled(" \u{2503} ", Style::default().fg(t.accent)),
            Span::styled(
                "difflore",
                Style::default()
                    .fg(t.foreground)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(strip, Style::default().fg(t.muted)),
        ]))
        .block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(Style::default().fg(t.border)),
        );
        frame.render_widget(wordmark, area);
    }

    fn draw_tabs(&self, frame: &mut ratatui::Frame<'_>, area: Rect, t: &Theme) {
        let titles: Vec<Line<'_>> = Tab::ALL
            .iter()
            .enumerate()
            .map(|(i, tab)| {
                Line::from(vec![
                    Span::styled(format!(" {} ", i + 1), Style::default().fg(t.muted)),
                    Span::raw(tab.title()),
                ])
            })
            .collect();

        let tabs = Tabs::new(titles)
            .select(self.active_tab.index())
            .style(Style::default().fg(t.foreground))
            .highlight_style(
                Style::default()
                    .fg(t.accent)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            )
            .divider(Span::styled(" · ", Style::default().fg(t.border)));

        frame.render_widget(tabs, area);
    }

    fn draw_body(&mut self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        match self.active_tab {
            Tab::Rules => {
                let plan = self.state.plan_state.clone();
                tabs::rules::render(
                    frame,
                    area,
                    tabs::rules::RenderProps {
                        rules: &self.state.rules,
                        list_state: &mut self.state.rules_list_state,
                        origin_filter: &self.state.rules_origin_filter,
                        search: &self.state.rules_search,
                        load_error: self.state.rules_load_error.as_deref(),
                        scope: tabs::rules::RepoScope {
                            source_repos: &self.state.rules_source_repos,
                            current_repo: self.state.current_repo.as_deref(),
                            filter: self.state.rules_repo_filter,
                            filter_label: self.state.rules_repo_filter.label(),
                        },
                        focus: self.state.rules_focus,
                        plan: &plan,
                    },
                );
            }
            Tab::Activity => {
                let stats = tabs::activity::render(
                    frame,
                    area,
                    self.state.fix_outcome_summary.as_ref(),
                    &self.state.fix_outcome_daily,
                    self.state.fix_outcomes_load_error.as_deref(),
                    self.state.activity_offset,
                    &self.state.rules_source_repos,
                );
                self.state.activity_visible_rows = stats.visible_rows;
                self.state.activity_rows_len = stats.rows_len;
            }
            Tab::Team => tabs::team::render(frame, area, &self.state.project_root),
            Tab::Settings => tabs::settings::render(
                frame,
                area,
                &self.state.project_root,
                self.state.rules.len(),
                self.state.plan_state(),
                &self.state.wiring,
                self.state.rules_load_error.as_deref(),
            ),
        }
    }

    /// Single-line key hint strip just above the status bar. Surfaces the
    /// keys that work in the current state so users don't have to press `?`
    /// to remember them.
    fn draw_hint_strip(&self, frame: &mut ratatui::Frame<'_>, area: Rect, t: &Theme) {
        let notice = self.state.status_notice.as_deref();
        let hint = notice.map_or_else(|| self.context_hint(), ToOwned::to_owned);
        let style = if notice.is_some() {
            Style::default().fg(t.warn)
        } else {
            Style::default().fg(t.muted)
        };
        let hint = fit_text(&hint, usize::from(area.width.saturating_sub(1)));
        let line = Line::from(vec![Span::raw(" "), Span::styled(hint, style)]);
        frame.render_widget(Paragraph::new(line), area);
    }

    fn context_hint(&self) -> String {
        let always = "? help · q quit · Tab next";
        match self.active_tab {
            Tab::Rules => {
                if self.state.rules_search.is_editing() {
                    return "type to filter · Enter/Tab commit · Esc cancel · Backspace delete"
                        .to_owned();
                }
                let body = "j/k row · h/l pane · / search · f view · r scope · e/p/s cloud";
                format!("{body} · {always}")
            }
            Tab::Activity => always.to_owned(),
            Tab::Team => {
                format!("c extracted rules \u{2197} · d memory dashboard \u{2197} · {always}")
            }
            Tab::Settings => {
                format!(
                    "i install once · l cloud login · a provider · w memory dashboard \u{2197} · u upgrade \u{2197} · {always}"
                )
            }
        }
    }

    fn draw_status_bar(&self, frame: &mut ratatui::Frame<'_>, area: Rect, t: &Theme) {
        let view = build_status_bar_view(self.state.plan_state());
        SmartStatusBar::render(frame, area, t, &view);
    }
}

const HELP_TEXT: &str = "\
TABS\n\
  1   Memory       2   Fixes           3   Cloud           4   Setup\n\
  DiffLore feeds team review memory to Claude, Codex, Cursor, and local agents.\n\
\n\
NAVIGATION\n\
  Tab Shift-Tab   prev / next tab\n\
  h l \u{2190} \u{2192}       focus pane within tab (Rules)\n\
  j k             row down / up\n\
  g G             top / bottom\n\
  Esc             dismiss search / filter / selection\n\
  q               quit\n\
  ?               this overlay\n\
\n\
MEMORY TAB\n\
  /               substring search (Esc clears, Enter commits)\n\
  f               cycle origin filter\n\
  r               cycle repo scope (this repo / all / global)\n\
  e               edit selected memory in cloud\n\
  p               publish selected memory in cloud\n\
  s               view memory sources in cloud\n\
\n\
FIXES TAB\n\
  Read-only event log of recalls, injections, reinforcements, and fix outcomes.\n\
  j k             scroll pipeline rows down / up (g / G jump to top / bottom)\n\
\n\
CLOUD TAB\n\
  c               review extracted rules in cloud\n\
  d / o           open memory dashboard in browser\n\
\n\
SETUP TAB\n\
  i               install once / re-sync agents with `difflore init`\n\
  l               run `difflore cloud login` (then `difflore cloud sync`)\n\
  a               run `difflore providers setup` (interactive)\n\
  w               open memory dashboard\n\
  u               review pricing / upgrade\n\
\n\
Press ? or Esc to close.";

/// Paint a solid scrim across the full frame so a centred panel reads as a
/// modal. ratatui has no real alpha, so `crust` (the darkest palette tone)
/// visually mutes whatever was rendered underneath.
pub(super) fn draw_backdrop(frame: &mut ratatui::Frame<'_>, area: Rect, t: &Theme) {
    let scrim = Block::default().style(Style::default().bg(t.crust));
    frame.render_widget(scrim, area);
}

fn draw_help_overlay(frame: &mut ratatui::Frame<'_>, area: Rect, t: &Theme) {
    // Without the backdrop scrim the underlying tab text bleeds out around
    // the panel border (visible as fragments of "team candidat",
    // "full dashboar" etc on the sides).
    draw_backdrop(frame, area, t);
    let panel = centered_rect_abs(77, 30, area);
    frame.render_widget(Clear, panel);

    let mut lines: Vec<Line<'_>> = Vec::new();
    for raw in HELP_TEXT.lines() {
        let trimmed = raw.trim_end();
        let style = section_style_for(trimmed, t);
        lines.push(Line::from(Span::styled(trimmed.to_owned(), style)));
    }

    let body = Paragraph::new(lines).wrap(Wrap { trim: false }).block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(ratatui::widgets::BorderType::Double)
            .border_style(Style::default().fg(t.border))
            .title(Span::styled(" Help ", theme::box_title(t)))
            .title_alignment(Alignment::Left),
    );
    frame.render_widget(body, panel);
}

fn section_style_for(line: &str, t: &Theme) -> Style {
    let is_section = !line.is_empty()
        && !line.starts_with(' ')
        && line
            .chars()
            .all(|c| c.is_ascii_uppercase() || c == '-' || c == ' ');
    if is_section {
        return Style::default().fg(t.muted).add_modifier(Modifier::BOLD);
    }
    Style::default().fg(t.foreground)
}

const HEADER_WIDTH_THRESHOLDS: [(usize, usize); 4] = [(40, 1), (56, 2), (68, 3), (80, 4)];

fn header_segments_to_keep(width: usize, segment_count: usize) -> usize {
    for (threshold, keep) in HEADER_WIDTH_THRESHOLDS {
        if width < threshold {
            return keep.min(segment_count);
        }
    }
    segment_count
}

fn fit_text(text: &str, max_chars: usize) -> String {
    let char_count = text.chars().count();
    if char_count <= max_chars {
        return text.to_owned();
    }
    if max_chars <= 3 {
        return ".".repeat(max_chars);
    }
    let prefix: String = text.chars().take(max_chars - 3).collect();
    format!("{prefix}...")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_segments_collapse_at_documented_widths() {
        assert_eq!(header_segments_to_keep(39, 5), 1);
        assert_eq!(header_segments_to_keep(40, 5), 2);
        assert_eq!(header_segments_to_keep(55, 5), 2);
        assert_eq!(header_segments_to_keep(56, 5), 3);
        assert_eq!(header_segments_to_keep(67, 5), 3);
        assert_eq!(header_segments_to_keep(68, 5), 4);
        assert_eq!(header_segments_to_keep(79, 5), 4);
        assert_eq!(header_segments_to_keep(80, 5), 5);
    }

    #[test]
    fn header_segments_never_exceed_available_segments() {
        assert_eq!(header_segments_to_keep(80, 2), 2);
        assert_eq!(header_segments_to_keep(39, 0), 0);
    }

    #[test]
    fn fit_text_truncates_with_ascii_ellipsis() {
        assert_eq!(fit_text("abcdef", 4), "a...");
        assert_eq!(fit_text("abc", 4), "abc");
    }
}
