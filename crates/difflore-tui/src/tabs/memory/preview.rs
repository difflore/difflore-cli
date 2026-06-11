use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use difflore_core::domain::models::SkillRecord;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use super::{RulesFocus, RulesOriginFilter, RulesRepoFilter};
use crate::plan::{PlanState, Tier};
use crate::theme::origin_color;

use crate::widgets::truncate;

use super::RepoScope;
use super::filter::origin_distribution;
use super::focus_border_color;

pub(super) fn draw_origin_summary(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    rules: &[&SkillRecord],
    origin_filter: &RulesOriginFilter,
    scope: &RepoScope<'_>,
) {
    let title = format!(" origins ({}) ", origin_filter.label());

    let total = rules.len();

    // Scope line keeps the user oriented across repos and surfaces the `r`
    // shortcut without a help screen.
    let scope_target = match scope.filter {
        RulesRepoFilter::ThisRepo => scope
            .current_repo
            .map_or_else(|| "(no remote)".to_owned(), ToOwned::to_owned),
        RulesRepoFilter::All => "every repo".to_owned(),
        RulesRepoFilter::Global => "globally-scoped only".to_owned(),
    };
    let origins = origin_distribution(rules)
        .into_iter()
        .map(|(origin, count)| format!("{origin}:{count}"))
        .collect::<Vec<_>>()
        .join(" · ");
    let lines = vec![
        Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "view ",
                Style::default().fg(crate::theme::Theme::current().muted),
            ),
            Span::styled(
                origin_filter.label(),
                Style::default()
                    .fg(crate::theme::Theme::current().accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "  [f]",
                Style::default().fg(crate::theme::Theme::current().muted),
            ),
            Span::styled(
                " · ",
                Style::default().fg(crate::theme::Theme::current().border),
            ),
            Span::styled(
                "scope ",
                Style::default().fg(crate::theme::Theme::current().muted),
            ),
            Span::styled(
                scope.filter_label,
                Style::default()
                    .fg(crate::theme::Theme::current().accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " [r]",
                Style::default().fg(crate::theme::Theme::current().muted),
            ),
        ]),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(
                format!("shown {total}"),
                Style::default()
                    .fg(crate::theme::Theme::current().foreground)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" · {scope_target}"),
                Style::default().fg(crate::theme::Theme::current().muted),
            ),
        ]),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(
                truncate(&origins, 72),
                Style::default().fg(crate::theme::Theme::current().muted),
            ),
        ]),
    ];

    let summary = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(crate::theme::Theme::current().border))
            .title(Span::styled(
                title,
                Style::default().fg(crate::theme::Theme::current().accent),
            )),
    );
    frame.render_widget(summary, area);
}

pub(super) fn draw_detail(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    selected: Option<&SkillRecord>,
    focus: RulesFocus,
    source_repos: &HashMap<String, Option<String>>,
) {
    let border_color = focus_border_color(focus, RulesFocus::Detail);
    let theme = crate::theme::Theme::current();
    if let Some(rule) = selected {
        let repo = source_repos
            .get(&rule.id)
            .and_then(Option::as_deref)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let mut lines: Vec<Line<'_>> = Vec::new();
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::raw(" "),
            Span::styled("● ", Style::default().fg(origin_color(&rule.origin))),
            Span::styled(
                truncate(&rule.name, 90),
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        lines.push(Line::from(vec![
            Span::raw("   "),
            Span::styled(
                match repo {
                    Some(repo) => format!("{} · {} · {}", rule.origin, rule.r#type, repo),
                    None => format!("{} · {}", rule.origin, rule.r#type),
                },
                Style::default().fg(theme.muted),
            ),
        ]));
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::raw("   "),
            Span::styled(
                truncate(&rule.description, 220),
                Style::default().fg(theme.foreground),
            ),
        ]));
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::raw("   "),
            Span::styled(
                "[e] edit in cloud  ·  [p] publish  ·  [s] sources",
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        lines.push(Line::from(vec![
            Span::raw("   "),
            Span::styled(
                format!("rule {}", truncate(&rule.id, 72)),
                Style::default().fg(theme.muted),
            ),
        ]));

        let detail = Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color))
                .title(Span::styled(
                    " cloud actions ",
                    Style::default().fg(theme.accent),
                )),
        );
        frame.render_widget(detail, area);
    } else {
        let hint = Paragraph::new(vec![
            Line::from(""),
            Line::from(Span::styled(
                "  Select memory, then use e / p / s to continue in cloud.",
                Style::default().fg(theme.muted),
            )),
        ])
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color))
                .title(Span::styled(
                    " cloud actions ",
                    Style::default().fg(theme.accent),
                )),
        );
        frame.render_widget(hint, area);
    }
}

pub(super) fn render_empty(frame: &mut ratatui::Frame<'_>, area: Rect) {
    let body = Paragraph::new(vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "No team review memory yet.",
                Style::default()
                    .fg(crate::theme::Theme::current().foreground)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "  Start by turning past PR comments into team memory, then sync it into local agents:",
            Style::default().fg(crate::theme::Theme::current().muted),
        )),
        Line::from(Span::styled(
            "    difflore import-reviews --max-prs 50 --upload && difflore cloud sync",
            Style::default().fg(crate::theme::Theme::current().foreground),
        )),
        Line::from(Span::styled(
            "  After that, use `difflore recall --diff` to see exactly what agents remember.",
            Style::default().fg(crate::theme::Theme::current().muted),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "  No cloud login yet? Capture a memory locally:",
            Style::default().fg(crate::theme::Theme::current().muted),
        )),
        Line::from(Span::styled(
            "    difflore rules remember --title \"\u{2026}\" --body \"\u{2026}\"",
            Style::default().fg(crate::theme::Theme::current().foreground),
        )),
    ])
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(crate::theme::Theme::current().border))
            .title(Span::styled(
                " memory ",
                Style::default().fg(crate::theme::Theme::current().accent),
            )),
    );
    frame.render_widget(body, area);
}

pub(super) fn render_error(frame: &mut ratatui::Frame<'_>, area: Rect, err: &str) {
    let body = Paragraph::new(vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "Failed to load rules.",
                Style::default()
                    .fg(crate::theme::Theme::current().danger)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(
                err.to_owned(),
                Style::default().fg(crate::theme::Theme::current().foreground),
            ),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "  Try: `difflore doctor` to inspect the database, or re-run `difflore init`.",
            Style::default().fg(crate::theme::Theme::current().muted),
        )),
    ])
    .wrap(Wrap { trim: false })
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(crate::theme::Theme::current().danger))
            .title(Span::styled(
                " rules ",
                Style::default().fg(crate::theme::Theme::current().danger),
            )),
    );
    frame.render_widget(body, area);
}

/// Single muted line pinned to the bottom of the Memory tab. Surfaces the
/// active embedding mode + quota: Cloud-managed Free (capped), Cloud-managed
/// Team (unlimited), BYOK, or the SHA1 fallback.
pub(super) fn draw_embedder_status_bar(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    plan: &PlanState,
) {
    let theme = crate::theme::Theme::current();
    let mut snapshot = read_embedder_mode_snapshot();
    apply_plan_to_embedder_snapshot(&mut snapshot, plan);
    // Budget is `area.width - 1` to account for the single leading space
    // rendered below; `truncate` keeps the result (including the ellipsis)
    // within that budget.
    let text = truncate(
        &format_embedder_status_bar(&snapshot),
        usize::from(area.width.saturating_sub(1)),
    );
    let bar = Paragraph::new(Line::from(vec![
        Span::raw(" "),
        Span::styled(text, Style::default().fg(theme.muted)),
    ]));
    frame.render_widget(bar, area);
}

fn apply_plan_to_embedder_snapshot(snapshot: &mut EmbedderModeSnapshot, plan: &PlanState) {
    if !matches!(snapshot.mode, EmbedderMode::CloudManaged) {
        return;
    }
    snapshot.plan = Some(match plan.tier {
        Tier::Free => "Free".to_owned(),
        Tier::Team => "Team".to_owned(),
        Tier::TeamPlus => "Team Plus".to_owned(),
    });
}

/// Snapshot of the current embedder configuration. Mirrors the JSON shape
/// we expect to land at `~/.difflore/cache/embedder-mode.json`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct EmbedderModeSnapshot {
    pub(super) mode: EmbedderMode,
    /// Cap and used count for cloud-managed Free. `None` for paid /
    /// BYOK / SHA1 — those modes are uncapped.
    pub(super) cloud_cap: Option<(u32, u32)>,
    /// Plan label for cloud-managed mode.
    pub(super) plan: Option<String>,
    /// Provider host for BYOK. Never the key.
    pub(super) byok_host: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum EmbedderMode {
    CloudManaged,
    Byok,
    Sha1,
}

/// Best-effort sync read of the embedder mode. Delegates to
/// `difflore_core::context::embedding::probe_active_embedder_sync` so the
/// TUI status bar always agrees with the runtime resolver (same priority
/// chain: BYOK → cloud → SHA1). Stays sync so the render path never polls
/// a tokio runtime each frame.
fn read_embedder_mode_snapshot() -> EmbedderModeSnapshot {
    let now = Instant::now();
    let cache =
        EMBEDDER_SNAPSHOT_CACHE.get_or_init(|| Mutex::new(EmbedderSnapshotCache::fresh(now)));
    let Ok(mut cache) = cache.lock() else {
        return read_embedder_mode_snapshot_uncached();
    };

    if cache.is_fresh(now) {
        return cache.snapshot.clone();
    }

    *cache = EmbedderSnapshotCache::fresh(now);
    cache.snapshot.clone()
}

const EMBEDDER_SNAPSHOT_CACHE_TTL: Duration = Duration::from_millis(500);

static EMBEDDER_SNAPSHOT_CACHE: OnceLock<Mutex<EmbedderSnapshotCache>> = OnceLock::new();

#[derive(Clone, Debug)]
struct EmbedderSnapshotCache {
    checked_at: Instant,
    snapshot: EmbedderModeSnapshot,
}

impl EmbedderSnapshotCache {
    fn fresh(now: Instant) -> Self {
        Self {
            checked_at: now,
            snapshot: read_embedder_mode_snapshot_uncached(),
        }
    }

    fn is_fresh(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.checked_at) < EMBEDDER_SNAPSHOT_CACHE_TTL
    }
}

fn read_embedder_mode_snapshot_uncached() -> EmbedderModeSnapshot {
    use difflore_core::context::embedding::{ActiveEmbedderKind, probe_active_embedder_sync};

    match probe_active_embedder_sync() {
        ActiveEmbedderKind::Cloud { .. } => {
            let cloud_cap = latest_embed_cap_from_activity();
            EmbedderModeSnapshot {
                mode: EmbedderMode::CloudManaged,
                cloud_cap,
                plan: None,
                byok_host: None,
            }
        }
        ActiveEmbedderKind::Byok { provider_host, .. } => EmbedderModeSnapshot {
            mode: EmbedderMode::Byok,
            cloud_cap: None,
            plan: None,
            byok_host: Some(provider_host),
        },
        ActiveEmbedderKind::Sha1 => EmbedderModeSnapshot {
            mode: EmbedderMode::Sha1,
            cloud_cap: None,
            plan: None,
            byok_host: None,
        },
    }
}

fn latest_embed_cap_from_activity() -> Option<(u32, u32)> {
    difflore_core::observability::activity_stream::tail(20)
        .into_iter()
        .find_map(|event| match event.payload {
            difflore_core::observability::activity_stream::ActivityPayload::EmbedCapReached {
                cap,
                used,
            } => Some((used, cap)),
            _ => None,
        })
}

const DEFAULT_FREE_EMBED_CAP: u32 = 200;

/// Render the four canonical bar variants. Pure function so the format is
/// unit-testable independent of ratatui / TUI state.
pub(super) fn format_embedder_status_bar(snap: &EmbedderModeSnapshot) -> String {
    match snap.mode {
        EmbedderMode::CloudManaged => {
            let plan = snap.plan.as_deref().unwrap_or("free");
            // Free tier shows the cap + upgrade exits; paid tiers collapse to
            // the unlimited line so we never nag a paying user.
            if plan.eq_ignore_ascii_case("free") {
                let (used, cap) = snap.cloud_cap.unwrap_or((0, DEFAULT_FREE_EMBED_CAP));
                format!("Cloud embeddings · Free · {used}/{cap} embedded · Team/BYOK for unlimited")
            } else {
                format!("Cloud embeddings · {plan} · unlimited")
            }
        }
        EmbedderMode::Byok => {
            let host = snap.byok_host.as_deref().unwrap_or("api.openai.com");
            format!("BYOK embeddings · {host} · unlimited")
        }
        EmbedderMode::Sha1 => {
            // "Local lexical", not "SHA1": the hash feeds a hash + FTS5 hybrid,
            // not a pretend-semantic embedder, and the copy keeps the upgrade
            // path visible.
            "Local lexical · cloud login or BYOK for semantic recall".to_owned()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedder_snapshot_cache_is_time_bounded() {
        let now = Instant::now();
        let cache = EmbedderSnapshotCache {
            checked_at: now,
            snapshot: EmbedderModeSnapshot {
                mode: EmbedderMode::Sha1,
                cloud_cap: None,
                plan: None,
                byok_host: None,
            },
        };

        assert!(cache.is_fresh(now + Duration::from_millis(1)));
        assert!(!cache.is_fresh(now + EMBEDDER_SNAPSHOT_CACHE_TTL + Duration::from_millis(1)));
    }

    #[test]
    fn byok_status_bar_uses_host_without_key_material() {
        let text = format_embedder_status_bar(&EmbedderModeSnapshot {
            mode: EmbedderMode::Byok,
            cloud_cap: None,
            plan: None,
            byok_host: Some("proxy.local".to_owned()),
        });

        assert_eq!(text, "BYOK embeddings · proxy.local · unlimited");
    }

    #[test]
    fn cloud_status_bar_uses_reported_cap_when_available() {
        let text = format_embedder_status_bar(&EmbedderModeSnapshot {
            mode: EmbedderMode::CloudManaged,
            cloud_cap: Some((198, 200)),
            plan: Some("free".to_owned()),
            byok_host: None,
        });

        assert!(text.contains("198/200"));
        assert!(text.contains("Team/BYOK"));
        assert!(text.contains("BYOK"));
    }

    #[test]
    fn paid_plan_marks_cloud_embeddings_unlimited() {
        let mut snap = EmbedderModeSnapshot {
            mode: EmbedderMode::CloudManaged,
            cloud_cap: Some((198, 200)),
            plan: None,
            byok_host: None,
        };
        let plan = PlanState {
            tier: Tier::Team,
            ..Default::default()
        };

        apply_plan_to_embedder_snapshot(&mut snap, &plan);

        assert_eq!(
            format_embedder_status_bar(&snap),
            "Cloud embeddings · Team · unlimited"
        );
    }
}
