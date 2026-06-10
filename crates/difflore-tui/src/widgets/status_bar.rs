//! Plan-aware status bar.
//!
//! Format:
//!
//! ```text
//! ● {plan}  │  {plan_label}  │  {ruleCount} memories          {right_side}
//! ```
//!
//! `PlanStateView` carries the plan, rule counts, capacity, and event-strip
//! state needed to render the left summary and right-side hint.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::theme::Theme;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PlanTier {
    Free,
    Team,
    TeamPlus,
}

impl PlanTier {
    /// Plan-accent dot color.
    pub const fn dot(self, t: &Theme) -> Color {
        match self {
            Self::Free => t.diff,
            Self::Team => t.origin_cloud,
            Self::TeamPlus => t.origin_team,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Free => "Free",
            Self::Team => "Team",
            Self::TeamPlus => "Team Plus",
        }
    }
}

/// Drives the right-side hint. Carries the CTA-relevant payload so
/// the status bar can format the variant without re-reading the full
/// `PlanState`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EventStripState {
    None,
    CrossMachine,
    TeammateCaught {
        teammate: String,
        when_label: String,
    },
    FixRunsLow {
        used: u32,
        quota: u32,
    },
}

/// View-model consumed by `SmartStatusBar`. Held in `App`; produced
/// from the full `PlanState` Rust mirror by `build_status_bar_view`.
#[derive(Clone, Debug)]
pub struct PlanStateView {
    pub tier: PlanTier,
    pub plan_label: String,
    pub rule_count: u32,
    pub published_count: u32,
    pub event_strip: EventStripState,
    pub fix_runs_used: u32,
    pub fix_runs_quota: u32,
}

impl PlanStateView {
    /// Render the right-side hint string per the §5.4 table. Returns
    /// `None` when the steady-state cell is empty (e.g. Free · steady,
    /// or any Team/Team Plus state without an active event strip).
    ///
    /// There used to be `members_syncing > 0` arms emitting a
    /// "✓ syncing N members" line, but `build_status_bar_view` always
    /// produced a zero member count (no data source feeds it), so those
    /// arms were unreachable in production and were removed along with
    /// the field.
    pub fn right_side(&self) -> Option<String> {
        // FixRunsLow on Free is nonsensical (Free has no hosted capacity); fall
        // back to the steady-state empty cell rather than panic.
        match (self.tier, &self.event_strip) {
            (PlanTier::Free, EventStripState::FixRunsLow { used, quota }) => Some(format!(
                "⚠ {used}/{quota} cloud embeds used  │  [u] upgrade · [b] BYOK"
            )),
            (PlanTier::Free, EventStripState::CrossMachine) => {
                Some("⚡ rules not synced  │  [s] sync · 14d trial".into())
            }
            (PlanTier::Free, EventStripState::TeammateCaught { when_label, .. }) => Some(format!(
                "⚡ teammate PR caught ({when_label})  │  [t] try Team · multiply impact"
            )),
            (PlanTier::Team, EventStripState::FixRunsLow { used, quota }) => Some(format!(
                "⚠ {used}/{quota} review-memory capacity used  │  [u] Team Plus · expand"
            )),
            (PlanTier::Free, EventStripState::None) | (PlanTier::Team | PlanTier::TeamPlus, _) => {
                None
            }
        }
    }
}

pub struct SmartStatusBar;

impl SmartStatusBar {
    /// Render the status bar into `area`. The caller supplies the
    /// full theme so we never have to re-resolve at draw time.
    pub fn render(frame: &mut Frame<'_>, area: Rect, theme: &Theme, view: &PlanStateView) {
        let dot = Span::styled("● ", Style::default().fg(view.tier.dot(theme)));
        let plan = Span::styled(
            view.tier.label(),
            Style::default()
                .fg(theme.foreground)
                .add_modifier(Modifier::BOLD),
        );
        let sep = || Span::styled("  │  ", Style::default().fg(theme.subtle));
        let label = Span::styled(view.plan_label.clone(), Style::default().fg(theme.muted));
        let count_label = if view.published_count > 0 {
            format!(
                "{} memories · {} published",
                view.rule_count, view.published_count
            )
        } else {
            format!("{} memories", view.rule_count)
        };
        let counts = Span::styled(count_label, Style::default().fg(theme.muted));

        let mut spans: Vec<Span<'static>> = vec![dot, plan, sep(), label, sep(), counts];

        if let Some(right) = view.right_side() {
            // Warning hue for the "low quota" / "not synced" variants,
            // lore hue for the teammate-caught nudge. The trailing arm
            // is only a defensive fallback — `right_side` returns `None`
            // for every other `(tier, event_strip)` pair, so no muted
            // right-side string is ever actually painted.
            let style = match (&view.event_strip, view.tier) {
                (EventStripState::FixRunsLow { .. } | EventStripState::CrossMachine, _) => {
                    Style::default().fg(theme.warn)
                }
                (EventStripState::TeammateCaught { .. }, _) => Style::default().fg(theme.lore),
                _ => Style::default().fg(theme.muted),
            };
            spans.push(Span::styled(format!("    {right}"), style));
        }

        let line = Line::from(spans);
        let para = Paragraph::new(line).style(Style::default().bg(theme.bg));
        frame.render_widget(para, area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Steady-state Free view; tests tweak `tier` / `event_strip` from
    /// here. Replaces the former production `PlanStateView::free()`,
    /// which was only ever used by these tests.
    fn free_view() -> PlanStateView {
        PlanStateView {
            tier: PlanTier::Free,
            plan_label: "Free".into(),
            rule_count: 0,
            published_count: 0,
            event_strip: EventStripState::None,
            fix_runs_used: 0,
            fix_runs_quota: 0,
        }
    }

    #[test]
    fn free_steady_has_no_right_side() {
        let v = free_view();
        assert_eq!(v.right_side(), None);
    }

    #[test]
    fn team_steady_is_quiet() {
        let mut v = free_view();
        v.tier = PlanTier::Team;
        assert_eq!(v.right_side(), None);
    }

    #[test]
    fn team_plus_steady_is_quiet() {
        let mut v = free_view();
        v.tier = PlanTier::TeamPlus;
        assert_eq!(v.right_side(), None);
    }

    #[test]
    fn team_fix_runs_low_renders_capacity() {
        let mut v = free_view();
        v.tier = PlanTier::Team;
        v.event_strip = EventStripState::FixRunsLow {
            used: 240,
            quota: 300,
        };
        let right = v.right_side();
        assert!(
            right
                .as_deref()
                .is_some_and(|text| text.contains("240/300"))
        );
        assert!(
            right
                .as_deref()
                .is_some_and(|text| text.contains("review-memory capacity"))
        );
        assert!(
            right
                .as_deref()
                .is_some_and(|text| text.contains("Team Plus"))
        );
    }

    #[test]
    fn free_embed_cap_renders_upgrade_or_byok_exit() {
        let mut v = free_view();
        v.event_strip = EventStripState::FixRunsLow {
            used: 198,
            quota: 200,
        };
        let right = v.right_side();
        assert!(
            right
                .as_deref()
                .is_some_and(|text| text.contains("198/200"))
        );
        assert!(right.as_deref().is_some_and(|text| text.contains("BYOK")));
    }
}
