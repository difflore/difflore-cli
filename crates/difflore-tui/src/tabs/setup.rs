//! Settings tab: a passive surface showing the current plan, what's wired
//! up, and where to go next. Each row with a next step surfaces a key badge
//! that opens the right CLI command or cloud URL.

use std::path::Path;

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::WiringSnapshot;
use crate::plan::{PlanState, Tier};

pub fn render(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    project_root: &Path,
    rules_total: usize,
    plan: &PlanState,
    wiring: &WiringSnapshot,
    load_error: Option<&str>,
) {
    let theme = crate::theme::Theme::current();

    let outer = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(Span::styled(" setup ", Style::default().fg(theme.accent)));
    let inner = outer.inner(area);
    frame.render_widget(outer, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(8),  // plan + corpus snapshot
            Constraint::Length(16), // grouped readiness rows (Blockers/Ready/Optional)
            Constraint::Min(1),     // shortcuts hint + error
        ])
        .split(inner);

    draw_plan_snapshot(frame, chunks[0], project_root, rules_total, plan);
    draw_wiring(frame, chunks[1], plan, wiring);
    draw_footer(frame, chunks[2], load_error);
}

fn draw_plan_snapshot(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    project_root: &Path,
    rules_total: usize,
    plan: &PlanState,
) {
    let theme = crate::theme::Theme::current();
    let tier_label = match plan.tier {
        Tier::Free => "Free",
        Tier::Team => "Team",
        Tier::TeamPlus => "Team Plus",
    };
    let tier_color = match plan.tier {
        Tier::Free => theme.muted,
        Tier::Team => theme.origin_cloud,
        Tier::TeamPlus => theme.origin_team,
    };

    let lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled("plan       ", Style::default().fg(theme.muted)),
            Span::styled(
                tier_label,
                Style::default().fg(tier_color).add_modifier(Modifier::BOLD),
            ),
            Span::raw("   "),
            Span::styled(
                "·  CLI + MCP + local team rules",
                Style::default().fg(theme.muted),
            ),
        ]),
        Line::from(vec![
            Span::raw("  "),
            Span::styled("corpus     ", Style::default().fg(theme.muted)),
            Span::styled(
                format!("{rules_total} rules"),
                Style::default()
                    .fg(theme.foreground)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("   "),
            Span::styled(
                format!("·  {} published", plan.published_count),
                Style::default().fg(theme.muted),
            ),
        ]),
        Line::from(vec![
            Span::raw("  "),
            Span::styled("project    ", Style::default().fg(theme.muted)),
            Span::styled(
                project_root.display().to_string(),
                Style::default().fg(theme.foreground),
            ),
        ]),
    ];

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

/// One readiness row in the Setup tab. Carries enough metadata for the
/// renderer to drop it under the right group header (Blockers / Ready /
/// Optional). Built from `WiringSnapshot` + `PlanState` at draw time.
struct SetupRow {
    group: SetupGroup,
    key: &'static str,
    label: &'static str,
    state: String,
    state_color: ratatui::style::Color,
    hint: String,
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum SetupGroup {
    Blockers,
    Ready,
    Optional,
}

fn draw_wiring(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    plan: &PlanState,
    wiring: &WiringSnapshot,
) {
    let theme = crate::theme::Theme::current();

    // Each row carries: key badge · label · live state · CTA hint. The
    // state column reflects what the CLI snapshot actually saw on disk
    // at startup — green means wired, amber means a known gap, muted
    // means "we'd recommend this but the user is fine without it".
    let agents_state = if wiring.agents_detected == 0 {
        ("no agents detected on this machine", theme.muted)
    } else if wiring.agents_installed == 0 {
        (
            // Static slice that lives for the duration of render.
            "0 / detected: install once so agents can request rules",
            theme.warn,
        )
    } else if wiring.agents_installed < wiring.agents_detected {
        ("drift: re-run init so new IDEs see rules", theme.warn)
    } else {
        ("all detected agents can request team rules", theme.success)
    };
    let agents_state_string = format!(
        "{} / {} · {}",
        wiring.agents_installed, wiring.agents_detected, agents_state.0
    );

    let cloud_state = if wiring.cloud_logged_in {
        ("logged in · rule sync on", theme.success)
    } else {
        ("not logged in · rules stay local-only", theme.muted)
    };

    let provider_state = match &wiring.provider_name {
        Some(name) => (format!("{name} configured"), theme.success),
        None => (
            "no provider · review uses bare CLI defaults".to_owned(),
            theme.warn,
        ),
    };

    let upgrade_state = if plan.tier == Tier::Free {
        ("trial Team to publish + share rules", theme.muted)
    } else {
        ("review plan + quotas", theme.muted)
    };

    let host = difflore_core::cloud::endpoints::web_host_display();

    // Classify rows: agents/provider go under Blockers when missing
    // (they gate core value), Ready when wired. Cloud login is Ready
    // when in, Optional when out (memory still works locally). Daemon
    // and git hooks live under Optional per launch brief.
    let agents_group = if wiring.agents_installed == 0 {
        SetupGroup::Blockers
    } else {
        SetupGroup::Ready
    };
    let provider_group = if wiring.provider_name.is_some() {
        SetupGroup::Ready
    } else {
        SetupGroup::Blockers
    };
    let cloud_group = if wiring.cloud_logged_in {
        SetupGroup::Ready
    } else {
        SetupGroup::Optional
    };

    let (daemon_state, daemon_color) = if wiring.daemon_running {
        ("running".to_owned(), theme.success)
    } else {
        ("optional \u{00b7} not running".to_owned(), theme.muted)
    };
    let (hooks_state, hooks_color) = if wiring.pre_commit_installed {
        ("pre-commit installed".to_owned(), theme.success)
    } else {
        ("optional \u{00b7} pre-commit off".to_owned(), theme.muted)
    };

    let rows: Vec<SetupRow> = vec![
        SetupRow {
            group: agents_group,
            key: "i",
            label: "agents",
            state: agents_state_string,
            state_color: agents_state.1,
            hint: "\u{2192} install once for Claude/Codex/Cursor".to_owned(),
        },
        SetupRow {
            group: provider_group,
            key: "a",
            label: "provider",
            state: provider_state.0,
            state_color: provider_state.1,
            hint: "\u{2192} runs `difflore providers setup`".to_owned(),
        },
        SetupRow {
            group: cloud_group,
            key: "l",
            label: "cloud",
            state: cloud_state.0.to_owned(),
            state_color: cloud_state.1,
            hint: "\u{2192} login, then `difflore cloud sync`".to_owned(),
        },
        SetupRow {
            group: SetupGroup::Optional,
            key: "w",
            label: "dashboard",
            state: "open team rules dashboard".to_owned(),
            state_color: theme.muted,
            hint: format!("\u{2192} {host}"),
        },
        SetupRow {
            group: SetupGroup::Optional,
            key: "u",
            label: "upgrade",
            state: upgrade_state.0.to_owned(),
            state_color: upgrade_state.1,
            hint: format!("\u{2192} {host}/pricing"),
        },
        SetupRow {
            group: SetupGroup::Optional,
            key: "-",
            label: "daemon",
            state: daemon_state,
            state_color: daemon_color,
            hint: "\u{2192} `difflore daemon start` (optional)".to_owned(),
        },
        SetupRow {
            group: SetupGroup::Optional,
            key: "-",
            label: "hooks",
            state: hooks_state,
            state_color: hooks_color,
            hint: "\u{2192} `difflore hook install` (optional)".to_owned(),
        },
    ];

    let mut lines: Vec<Line<'_>> = Vec::new();
    for group in [
        SetupGroup::Blockers,
        SetupGroup::Ready,
        SetupGroup::Optional,
    ] {
        let group_rows: Vec<&SetupRow> = rows.iter().filter(|r| r.group == group).collect();
        if group_rows.is_empty() {
            continue;
        }
        let header = match group {
            SetupGroup::Blockers => "Blockers",
            SetupGroup::Ready => "Ready",
            SetupGroup::Optional => "Optional",
        };
        let header_color = match group {
            SetupGroup::Blockers => theme.warn,
            SetupGroup::Ready => theme.success,
            SetupGroup::Optional => theme.muted,
        };
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                header.to_owned(),
                Style::default()
                    .fg(header_color)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        for row in group_rows {
            lines.push(Line::from(vec![
                Span::raw("  "),
                key_badge(row.key, &theme),
                Span::raw("  "),
                Span::styled(
                    format!("{:<10}", row.label),
                    Style::default()
                        .fg(theme.foreground)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(row.state.clone(), Style::default().fg(row.state_color)),
                Span::raw("   "),
                Span::styled(row.hint.clone(), Style::default().fg(theme.muted)),
            ]));
        }
        lines.push(Line::from(""));
    }

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn draw_footer(frame: &mut ratatui::Frame<'_>, area: Rect, load_error: Option<&str>) {
    let theme = crate::theme::Theme::current();
    let mut lines: Vec<Line<'_>> = Vec::new();

    if let Some(err) = load_error {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "warning  ",
                Style::default()
                    .fg(theme.danger)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(err.to_owned(), Style::default().fg(theme.danger)),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            "Import, sync, recall, fix: the CLI delivers first-day value locally; team editing lives at",
            Style::default().fg(theme.muted),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            difflore_core::cloud::endpoints::web_host_display(),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            ". The TUI stays read-only on purpose so it can fit on one screen.",
            Style::default().fg(theme.muted),
        ),
    ]));

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn key_badge(key: &str, theme: &crate::theme::Theme) -> Span<'static> {
    Span::styled(
        format!(" {key} "),
        Style::default()
            .fg(theme.foreground)
            .bg(theme.highlight_bg)
            .add_modifier(Modifier::BOLD),
    )
}
