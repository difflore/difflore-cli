use std::collections::HashMap;

use difflore_core::domain::models::SkillRecord;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::widgets::ListState;

use crate::plan::PlanState;

pub(crate) mod filter;
mod preview;
mod search;

pub use filter::{RulesFocus, RulesOriginFilter, RulesRepoFilter, RulesSearch};

use preview::{
    draw_detail, draw_embedder_status_bar, draw_origin_summary, render_empty, render_error,
};
use search::{draw_filter_no_results, draw_list};

/// Project-scope context for the Memory tab. Owned by `AppState` and passed
/// by reference each frame so the tab can decide which rows to show
/// (current repo / all / global) and label the origins panel header.
pub struct RepoScope<'a> {
    pub source_repos: &'a HashMap<String, Option<String>>,
    pub current_repo: Option<&'a str>,
    pub filter: RulesRepoFilter,
    pub filter_label: &'static str,
}

pub struct RenderProps<'a> {
    pub rules: &'a [SkillRecord],
    pub list_state: &'a mut ListState,
    pub origin_filter: &'a RulesOriginFilter,
    pub search: &'a RulesSearch,
    pub load_error: Option<&'a str>,
    pub scope: RepoScope<'a>,
    pub focus: RulesFocus,
    pub plan: &'a PlanState,
}

/// Build the sorted + filtered rule slice that drives BOTH the rendered
/// list and selection-derived state (`selected_rule`, cursor clamp).
///
/// Single source of truth: the list and `App::selected_rule()` index into
/// the *same* `Vec`, so the highlighted row and the cloud CTAs can never
/// disagree.
///
/// Ordering: origin (`distribution_sort_key`) → source_repo → name. Repo
/// as the second key keeps a repo's rules contiguous within the
/// cloud-origin band.
pub(crate) fn ordered_filtered_rules<'a>(
    rules: &'a [SkillRecord],
    origin_filter: &RulesOriginFilter,
    search: &RulesSearch,
    scope: &RepoScope<'_>,
) -> Vec<&'a SkillRecord> {
    let needle = search.query().map(str::to_lowercase);
    let mut visible: Vec<&SkillRecord> = rules
        .iter()
        .filter(|r| origin_filter.includes_origin(&r.origin))
        .filter(|r| match &needle {
            Some(q) if !q.is_empty() => {
                let hay = format!("{} {}", r.name.to_lowercase(), r.description.to_lowercase());
                hay.contains(q)
            }
            _ => true,
        })
        .filter(|r| scope.includes(r))
        .collect();
    visible.sort_by(|a, b| {
        let a_repo = scope
            .source_repos
            .get(&a.id)
            .and_then(|v| v.as_deref())
            .unwrap_or("");
        let b_repo = scope
            .source_repos
            .get(&b.id)
            .and_then(|v| v.as_deref())
            .unwrap_or("");
        difflore_core::domain::origins::distribution_sort_key(&a.origin)
            .cmp(&difflore_core::domain::origins::distribution_sort_key(&b.origin))
            .then_with(|| a_repo.cmp(b_repo))
            .then_with(|| a.name.cmp(&b.name))
    });
    visible
}

pub fn render(frame: &mut ratatui::Frame<'_>, area: Rect, props: RenderProps<'_>) {
    let RenderProps {
        rules,
        list_state,
        origin_filter,
        search,
        load_error,
        scope,
        focus,
        plan,
    } = props;

    if let Some(err) = load_error {
        render_error(frame, area, err);
        return;
    }

    if rules.is_empty() {
        render_empty(frame, area);
        return;
    }

    // Reserve the bottom row for the embedding-mode + quota status bar.
    // It sticks to the last line and never pushes panel content.
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);
    let body_area = outer[0];
    let status_area = outer[1];

    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
        .split(body_area);

    // Single source of truth shared with `App::selected_rule` /
    // `filtered_rules_count` so the highlighted row and the cloud CTAs
    // always reference the same rule.
    let visible = ordered_filtered_rules(rules, origin_filter, search, &scope);

    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(5), Constraint::Min(1)])
        .split(chunks[0]);

    let selected = list_state.selected().and_then(|i| visible.get(i).copied());

    // No rule survived the active filter combo — keep the chrome but
    // replace the list body with a contextual "narrowed too far" message.
    if visible.is_empty() {
        draw_origin_summary(frame, left[0], &visible, origin_filter, &scope);
        draw_filter_no_results(frame, left[1], origin_filter, search, &scope);
        draw_detail(frame, chunks[1], None, focus, scope.source_repos);
        draw_embedder_status_bar(frame, status_area, plan);
        return;
    }

    draw_origin_summary(frame, left[0], &visible, origin_filter, &scope);
    draw_list(
        frame,
        left[1],
        &visible,
        list_state,
        focus,
        search,
        scope.source_repos,
    );
    draw_detail(frame, chunks[1], selected, focus, scope.source_repos);
    draw_embedder_status_bar(frame, status_area, plan);
}

/// Color-pick for a pane's border based on which pane currently has focus.
/// Focused pane gets the accent color; the inactive pane gets the standard
/// border color. Drives the visual cue users need for "where does my next
/// keypress go".
pub(super) fn focus_border_color(focus: RulesFocus, pane: RulesFocus) -> ratatui::style::Color {
    if focus == pane {
        crate::theme::Theme::current().accent
    } else {
        crate::theme::Theme::current().border
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::filter::origin_distribution;
    use super::preview::{EmbedderMode, EmbedderModeSnapshot, format_embedder_status_bar};
    use difflore_core::domain::models::SkillRecord;

    fn sample_rule(origin: &str) -> SkillRecord {
        SkillRecord {
            id: format!("{origin}-id"),
            name: format!("{origin}-rule"),
            source: "local".into(),
            directory: "/tmp/rule".into(),
            version: "1.0.0".into(),
            description: "sample".into(),
            r#type: "workflow".into(),
            engines: Vec::new(),
            tags: Vec::new(),
            trigger: None,
            check_prompt: None,
            repo_owner: None,
            repo_name: None,
            repo_branch: None,
            readme_url: None,
            enabled_for_codex: true,
            enabled_for_claude: false,
            enabled_for_gemini: false,
            enabled_for_cursor: false,
            installed_at: "2026-01-01".into(),
            updated_at: "2026-01-01".into(),
            enforcement: None,
            origin: origin.into(),
        }
    }

    #[test]
    fn origin_distribution_keeps_origin_ordering() {
        let conversation = sample_rule("conversation");
        let manual = sample_rule("manual");
        let extracted = sample_rule("extracted");
        let rules = vec![&manual, &extracted, &conversation];

        let distribution = origin_distribution(&rules);
        assert_eq!(
            distribution,
            vec![
                ("conversation".to_owned(), 1),
                ("manual".to_owned(), 1),
                ("extracted".to_owned(), 1),
            ]
        );
    }

    #[test]
    fn embedder_bar_cloud_free_shows_cap_and_both_exits() {
        let snap = EmbedderModeSnapshot {
            mode: EmbedderMode::CloudManaged,
            cloud_cap: Some((187, 200)),
            plan: Some("free".into()),
            byok_host: None,
        };
        let line = format_embedder_status_bar(&snap);
        assert!(line.contains("Cloud embeddings"));
        assert!(line.contains("Free"));
        assert!(line.contains("187/200 embedded"));
        assert!(line.contains("Team/BYOK"));
    }

    #[test]
    fn embedder_bar_cloud_team_drops_cap_mention() {
        let snap = EmbedderModeSnapshot {
            mode: EmbedderMode::CloudManaged,
            cloud_cap: None,
            plan: Some("team".into()),
            byok_host: None,
        };
        let line = format_embedder_status_bar(&snap);
        assert_eq!(line, "Cloud embeddings · team · unlimited");
        assert!(!line.contains("/200"));
    }

    #[test]
    fn embedder_bar_byok_shows_host_only() {
        let snap = EmbedderModeSnapshot {
            mode: EmbedderMode::Byok,
            cloud_cap: None,
            plan: None,
            byok_host: Some("api.openai.com".into()),
        };
        assert_eq!(
            format_embedder_status_bar(&snap),
            "BYOK embeddings · api.openai.com · unlimited"
        );
    }

    #[test]
    fn embedder_bar_sha1_points_at_two_recovery_paths() {
        let snap = EmbedderModeSnapshot {
            mode: EmbedderMode::Sha1,
            cloud_cap: None,
            plan: None,
            byok_host: None,
        };
        let line = format_embedder_status_bar(&snap);
        assert!(line.starts_with("Local lexical"));
        assert!(line.contains("cloud login"));
        assert!(line.contains("BYOK"));
    }
}
