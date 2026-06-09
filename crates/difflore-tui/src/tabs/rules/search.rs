use std::collections::HashMap;

use difflore_core::models::SkillRecord;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};

use crate::app::{RulesFocus, RulesOriginFilter, RulesRepoFilter, RulesSearch, origin_color};

use crate::widgets::truncate;

use super::RepoScope;
use super::focus_border_color;

pub(super) fn draw_list(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    rules: &[&SkillRecord],
    state: &mut ListState,
    focus: RulesFocus,
    search: &RulesSearch,
    source_repos: &HashMap<String, Option<String>>,
) {
    let theme = crate::theme::Theme::current();
    let items: Vec<ListItem<'_>> = rules
        .iter()
        .map(|r| {
            let repo = source_repos
                .get(&r.id)
                .and_then(|v| v.as_deref())
                .filter(|s| !s.trim().is_empty());
            let title_max = if repo.is_some() { 42 } else { 58 };
            let mut spans: Vec<Span<'_>> = vec![
                Span::styled("● ", Style::default().fg(origin_color(&r.origin))),
                Span::styled(
                    truncate(&r.name, title_max),
                    Style::default()
                        .fg(theme.foreground)
                        .add_modifier(Modifier::BOLD),
                ),
            ];
            if let Some(repo) = repo {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(
                    truncate(repo, 22),
                    Style::default().fg(theme.accent),
                ));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();

    let border_color = focus_border_color(focus, RulesFocus::List);
    // Title differentiates the two search states so users can tell "I'm
    // typing" from "filter is locked, I'm navigating":
    //   Editing(q)   → ` rules (12) · /q_ `   ← `_` is the cursor hint
    //   Filtering(q) → ` rules (12) · ⌕ q `   ← lens icon, no cursor
    //   Off          → ` rules (12) `
    let title = match search {
        RulesSearch::Editing(q) => format!(" rules ({}) · /{q}_ ", rules.len()),
        RulesSearch::Filtering(q) => format!(" rules ({}) · \u{2315} {q} ", rules.len()),
        RulesSearch::Off => format!(" rules ({}) ", rules.len()),
    };
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color))
                .title(Span::styled(
                    title,
                    Style::default().fg(crate::theme::Theme::current().accent),
                )),
        )
        .highlight_style(
            Style::default()
                .bg(crate::theme::Theme::current().highlight_bg)
                .fg(crate::theme::Theme::current().foreground)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");

    frame.render_stateful_widget(list, area, state);
}

/// Empty-state body when the active (origin × repo × search) filter combo
/// eliminated every rule. Tells the user which knob is hiding everything
/// and how to undo, instead of leaving them with a blank pane.
pub(super) fn draw_filter_no_results(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    origin_filter: &RulesOriginFilter,
    search: &RulesSearch,
    scope: &RepoScope<'_>,
) {
    let theme = crate::theme::Theme::current();
    let mut active: Vec<(&str, String, &str)> = Vec::new();
    if let Some(q) = search.query()
        && !q.is_empty()
    {
        active.push(("search", format!("/{q}"), "Esc clears"));
    }
    if !matches!(origin_filter, RulesOriginFilter::All) {
        active.push(("view", origin_filter.label(), "f cycles"));
    }
    if !matches!(scope.filter, RulesRepoFilter::All) {
        active.push(("scope", scope.filter_label.to_owned(), "r cycles"));
    }

    // ThisRepo-only state gets the launch brief's "No Current Repo
    // Matches" framing instead of the generic "no rules match".
    let scoped_only = matches!(origin_filter, RulesOriginFilter::All)
        && search.query().filter(|q| !q.is_empty()).is_none()
        && matches!(scope.filter, RulesRepoFilter::ThisRepo);
    let headline = if scoped_only {
        "No review memory scoped to this repo."
    } else {
        "No review memory matches the active filter."
    };
    let mut lines: Vec<Line<'_>> = vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(
                headline,
                Style::default()
                    .fg(theme.foreground)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(""),
    ];
    if scoped_only {
        lines.push(Line::from(Span::styled(
            "  Try all memory (r), run `difflore import-reviews --max-prs 50 --upload`, then `difflore cloud sync`.",
            Style::default().fg(theme.muted),
        )));
        lines.push(Line::from(""));
    }
    for (label, value, undo) in &active {
        lines.push(Line::from(vec![
            Span::raw("    "),
            Span::styled(format!("{label:>7}  "), Style::default().fg(theme.muted)),
            Span::styled(value.clone(), Style::default().fg(theme.accent)),
            Span::raw("    "),
            Span::styled(format!("({undo})"), Style::default().fg(theme.muted)),
        ]));
    }
    if active.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no filter active — corpus is genuinely empty?)",
            Style::default().fg(theme.muted),
        )));
    }
    let body = Paragraph::new(lines).wrap(Wrap { trim: false }).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.border))
            .title(Span::styled(
                " no matches ",
                Style::default().fg(theme.accent),
            )),
    );
    frame.render_widget(body, area);
}
