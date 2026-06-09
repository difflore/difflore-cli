//! Activity tab — real local fix outcomes over time, plus a live
//! "memory pipeline" event stream so the user can SEE rules being
//! recalled, reinforced, and injected as their agent runs.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use difflore_core::activity_stream::{self, ActivityEvent, ActivityPayload};
use difflore_core::observability::fix_outcomes::{FixOutcomeDaily, FixOutcomeSummary};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{BarChart, Block, Borders, Paragraph, Sparkline, Wrap};

/// Render the Activity tab. Returns the visible-row budget of the
/// pipeline list so the app-level key handler can clamp the scroll
/// offset against the current terminal geometry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RenderStats {
    pub visible_rows: usize,
    pub rows_len: usize,
}

pub fn render(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    summary: Option<&FixOutcomeSummary>,
    daily: &[FixOutcomeDaily],
    load_error: Option<&str>,
    offset: usize,
    source_repos: &HashMap<String, Option<String>>,
) -> RenderStats {
    if let Some(error) = load_error {
        render_error(frame, area, error);
        return RenderStats::default();
    }

    let stream_events = cached_activity_events(40);
    let rows_len = displayed_rows_count(&stream_events);

    // No data anywhere → empty state. Otherwise we always render the
    // pipeline panel even if fix outcomes are missing — for fresh
    // installs the pipeline is the first thing to come alive.
    let has_outcomes = matches!(summary, Some(s) if s.applied + s.failed + s.rejected > 0);
    if !has_outcomes && stream_events.is_empty() {
        render_empty(frame, area);
        return RenderStats::default();
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(12), // outcome bars
            Constraint::Length(6),  // sparkline
            Constraint::Min(8),     // memory pipeline stream
        ])
        .split(area);

    if let Some(s) = summary {
        draw_outcome_bars(frame, chunks[0], s);
        draw_outcome_sparkline(frame, chunks[1], daily, s);
    } else {
        draw_outcomes_placeholder(frame, chunks[0]);
        draw_outcomes_placeholder(frame, chunks[1]);
    }
    let pipeline_area = chunks[2];
    draw_pipeline_stream(frame, pipeline_area, &stream_events, offset, source_repos);
    // Subtract 2 for the bordered block's top + bottom edges. Floor at
    // 0 on absurdly small terminals so the clamp arithmetic stays sane.
    RenderStats {
        visible_rows: usize::from(pipeline_area.height).saturating_sub(2),
        rows_len,
    }
}

/// Number of rows the pipeline panel will display for the given event
/// tail, after burst grouping and the 20-row cap. Used by the app key
/// handler to compute the scroll-offset clamp.
pub fn displayed_rows_count(events: &[ActivityEvent]) -> usize {
    group_consecutive(events, 20).len()
}

/// Clamp a candidate scroll offset against the row count and visible
/// row budget. Centralised so both the key handler and tests share the
/// same arithmetic; `[0, max(0, rows.saturating_sub(visible_rows))]`.
pub fn clamp_offset(offset: usize, rows: usize, visible_rows: usize) -> usize {
    let max_offset = rows.saturating_sub(visible_rows);
    offset.min(max_offset)
}

fn draw_outcomes_placeholder(frame: &mut ratatui::Frame<'_>, area: Rect) {
    let theme = crate::theme::Theme::current();
    let p = Paragraph::new(Line::from(Span::styled(
        "  Fix outcomes will appear after `difflore fix` accepts/skips a patch.",
        Style::default().fg(theme.muted),
    )))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.border))
            .title(Span::styled(
                " fix outcomes ",
                Style::default().fg(theme.accent),
            )),
    );
    frame.render_widget(p, area);
}

/// Render the live memory-pipeline stream — last N events newest-first.
/// Consecutive identical events within 1 second collapse to a single
/// row with `(×N)` so a burst of recalls doesn't drown the panel.
fn draw_pipeline_stream(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    events: &[ActivityEvent],
    offset: usize,
    source_repos: &HashMap<String, Option<String>>,
) {
    let theme = crate::theme::Theme::current();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        // Title says "lines" not "events" because group_consecutive
        // collapses bursts ("(×N)") into one row — a literal "20 events"
        // claim would be wrong any time a single line stands in for a
        // recall storm.
        .title(Span::styled(
            " memory pipeline · last 20 lines ",
            Style::default().fg(theme.accent),
        ));

    if events.is_empty() {
        let body = Paragraph::new(vec![
            Line::from(""),
            Line::from(Span::styled(
                "  Waiting for recall. Run `difflore recall --diff` or ask an agent to edit this repo.",
                Style::default().fg(theme.muted),
            )),
            Line::from(Span::styled(
                "  You will see rules injected before coding and reinforced after fixes.",
                Style::default().fg(theme.muted),
            )),
        ])
        .wrap(Wrap { trim: false })
        .block(block);
        frame.render_widget(body, area);
        return;
    }

    let grouped = group_consecutive(events, 20);
    let skip = offset.min(grouped.len());
    let lines: Vec<Line<'_>> = grouped
        .iter()
        .skip(skip)
        .map(|(ev, count)| event_line(ev, *count, source_repos))
        .collect();
    let body = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .block(block);
    frame.render_widget(body, area);
}

/// Collapse runs of identical events emitted within 1 second of each
/// other into one (event, count) entry. `events` is newest-first; we
/// preserve that order. Truncates to `limit` rows.
fn group_consecutive(events: &[ActivityEvent], limit: usize) -> Vec<(&ActivityEvent, usize)> {
    let mut out: Vec<(&ActivityEvent, usize)> = Vec::with_capacity(events.len().min(limit));
    for ev in events {
        if let Some((last, count)) = out.last_mut() {
            if last.payload == ev.payload && last.ts_ms.abs_diff(ev.ts_ms) <= 1000 {
                *count += 1;
                continue;
            }
        }
        if out.len() >= limit {
            break;
        }
        out.push((ev, 1));
    }
    out
}

fn event_line<'a>(
    event: &'a ActivityEvent,
    count: usize,
    source_repos: &HashMap<String, Option<String>>,
) -> Line<'a> {
    let theme = crate::theme::Theme::current();
    // Resolved provenance for the rule this event mentions, if any. We
    // only attach a "← learned from <repo>" suffix to event variants
    // that carry a `rule_id`; non-rule events (e.g. RetrievalEmbedding,
    // EmbedCapReached, EmbeddingFallback) and rule events whose
    // source_repo is unknown or empty get no suffix so the panel stays
    // uncluttered.
    let mut rule_repo: Option<String> = None;
    let (glyph, prefix, prefix_color, summary) = match &event.payload {
        ActivityPayload::RuleRecalled {
            rule_id,
            rule_title,
            score,
            ..
        } => {
            rule_repo = lookup_repo(source_repos, rule_id);
            (
                "•",
                "Recalled",
                theme.accent,
                format!("\"{}\" (score {:.2})", truncate(rule_title, 50), score),
            )
        }
        ActivityPayload::RuleInjected {
            rule_count,
            prompt_chars,
            intent_summary,
        } => (
            "→",
            "Injected",
            theme.success,
            format!(
                "{} rule{} · {} chars · {}",
                rule_count,
                if *rule_count == 1 { "" } else { "s" },
                prompt_chars,
                truncate(intent_summary, 60)
            ),
        ),
        ActivityPayload::RuleReinforced {
            rule_id,
            rule_title,
            prev_strength,
            new_strength,
            reason,
            ..
        } => {
            rule_repo = lookup_repo(source_repos, rule_id);
            (
                "↻",
                "Reinforced",
                theme.warn,
                format!(
                    "\"{}\" {:.1} → {:.1} ({})",
                    truncate(rule_title, 40),
                    prev_strength,
                    new_strength,
                    reason
                ),
            )
        }
        ActivityPayload::RetrievalEmbedding { hits, took_ms } => (
            "≈",
            "Embedding",
            theme.muted,
            format!("{hits} hits · {took_ms} ms"),
        ),
        ActivityPayload::EmbedCapReached { cap, used } => (
            "⚠",
            "Cap reached",
            theme.warn,
            format!(
                "{used}/{cap} cloud embeds used · falling back to SHA1 (Team for unlimited · or `providers setup` for BYOK)",
            ),
        ),
        ActivityPayload::EmbeddingFallback { reason } => (
            "⚠",
            "Embedding fallback",
            theme.warn,
            format!("local SHA1 active after {reason} · run `difflore doctor`"),
        ),
    };

    let count_suffix = if count > 1 {
        format!(" (\u{00d7}{count})")
    } else {
        String::new()
    };

    let mut spans = vec![
        Span::raw("  "),
        Span::styled(glyph.to_owned(), Style::default().fg(prefix_color)),
        Span::raw(" "),
        Span::styled(
            prefix.to_owned(),
            Style::default()
                .fg(prefix_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(summary, Style::default().fg(theme.foreground)),
        Span::styled(count_suffix, Style::default().fg(theme.muted)),
    ];
    // Buyer-grounding suffix: "← learned from <repo>". Same framing as
    // the Memory/Rules tab badge (iter #11) and the CLI fix preview
    // header (iter #10) so users see a consistent attribution thread
    // across surfaces. Only appended when the event references a known
    // rule and that rule has a non-empty source_repo.
    if let Some(repo) = rule_repo {
        spans.push(Span::styled(
            "  \u{2190} learned from ",
            Style::default().fg(theme.muted),
        ));
        spans.push(Span::styled(repo, Style::default().fg(theme.muted)));
    }
    Line::from(spans)
}

/// Resolve a rule's `source_repo` from the shared map, returning `None`
/// for unknown ids and for entries whose repo is `None` or whitespace.
fn lookup_repo(source_repos: &HashMap<String, Option<String>>, rule_id: &str) -> Option<String> {
    source_repos
        .get(rule_id)
        .and_then(|v| v.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

use crate::widgets::truncate;

fn draw_outcome_bars(frame: &mut ratatui::Frame<'_>, area: Rect, summary: &FixOutcomeSummary) {
    let data = [
        (
            "applied",
            u64::try_from(summary.applied.max(0)).unwrap_or(0),
            outcome_color("applied"),
        ),
        (
            "failed",
            u64::try_from(summary.failed.max(0)).unwrap_or(0),
            outcome_color("failed"),
        ),
        (
            "rejected",
            u64::try_from(summary.rejected.max(0)).unwrap_or(0),
            outcome_color("rejected"),
        ),
    ];

    let bar_areas = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Ratio(1, 3); 3])
        .margin(0)
        .split(Rect {
            x: area.x + 2,
            y: area.y + 1,
            width: area.width.saturating_sub(4),
            height: area.height.saturating_sub(2),
        });

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(crate::theme::Theme::current().border))
        .title(Span::styled(
            " fix outcomes · last 30 days ",
            Style::default().fg(crate::theme::Theme::current().accent),
        ));
    frame.render_widget(block, area);

    let max = data.iter().map(|(_, c, _)| *c).max().unwrap_or(1).max(1);

    for (i, (label, count, color)) in data.iter().enumerate() {
        let bar = BarChart::default()
            .data(&[(*label, *count)])
            .max(max)
            .bar_width(label_bar_width(label))
            .bar_style(Style::default().fg(*color))
            .value_style(
                Style::default()
                    .fg(crate::theme::Theme::current().foreground)
                    .bg(*color),
            );
        frame.render_widget(bar, bar_areas[i]);
    }
}

fn draw_outcome_sparkline(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    daily: &[FixOutcomeDaily],
    summary: &FixOutcomeSummary,
) {
    let data = daily_outcome_totals(daily, 30);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(crate::theme::Theme::current().border))
        .title(Span::styled(
            format!(
                " patch decisions · last 30 days ({} total, max/day {}) ",
                safe_outcome_total(summary.applied, summary.failed, summary.rejected),
                data.iter().max().copied().unwrap_or(0)
            ),
            Style::default().fg(crate::theme::Theme::current().accent),
        ));
    let sparkline = Sparkline::default()
        .block(block)
        .data(&data)
        .style(Style::default().fg(outcome_color("applied")));
    frame.render_widget(sparkline, area);
}

fn render_empty(frame: &mut ratatui::Frame<'_>, area: Rect) {
    let body = Paragraph::new(vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "No memory activity yet.",
                Style::default()
                    .fg(crate::theme::Theme::current().foreground)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "  Activity appears when agents recall rules or `difflore fix` accepts/skips patches.",
            Style::default().fg(crate::theme::Theme::current().muted),
        )),
        Line::from(Span::styled(
            "  Empty corpus first? Run `difflore cloud login`, then `difflore import-reviews --max-prs 50 --upload`,",
            Style::default().fg(crate::theme::Theme::current().muted),
        )),
        Line::from(Span::styled(
            "  then `difflore cloud sync`. Without `cloud login`, --upload skips uploading each batch and falls back to local drafting.",
            Style::default().fg(crate::theme::Theme::current().muted),
        )),
    ])
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(crate::theme::Theme::current().border))
            .title(Span::styled(
                " activity ",
                Style::default().fg(crate::theme::Theme::current().accent),
            )),
    );
    frame.render_widget(body, area);
}

fn render_error(frame: &mut ratatui::Frame<'_>, area: Rect, error: &str) {
    let body = Paragraph::new(vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "Fix activity could not be loaded.",
                Style::default()
                    .fg(crate::theme::Theme::current().foreground)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            format!("  {error}"),
            Style::default().fg(crate::theme::Theme::current().muted),
        )),
    ])
    .wrap(Wrap { trim: false })
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(crate::theme::Theme::current().border))
            .title(Span::styled(
                " activity ",
                Style::default().fg(crate::theme::Theme::current().accent),
            )),
    );
    frame.render_widget(body, area);
}

fn outcome_color(kind: &str) -> Color {
    let theme = crate::theme::Theme::current();
    match kind {
        "applied" => theme.success,
        "failed" => theme.warn,
        "rejected" => theme.muted,
        _ => theme.foreground,
    }
}

/// Bucket outcome rows by day for the last `days` days.
/// Returns oldest-first so the sparkline reads left-to-right chronologically.
fn daily_outcome_totals(rows: &[FixOutcomeDaily], days: usize) -> Vec<u64> {
    use chrono::Utc;

    let mut buckets: BTreeMap<chrono::NaiveDate, u64> = BTreeMap::new();
    let today = Utc::now().naive_utc().date();
    let cutoff = today - chrono::Duration::days(usize_to_i64_days(days.saturating_sub(1)));

    for row in rows {
        let date = parse_date_prefix(&row.day);
        if let Some(d) = date
            && d >= cutoff
            && d <= today
        {
            *buckets.entry(d).or_insert(0) +=
                safe_outcome_total(row.applied, row.failed, row.rejected);
        }
    }

    (0..days)
        .map(|offset| {
            let d = cutoff + chrono::Duration::days(usize_to_i64_days(offset));
            *buckets.get(&d).unwrap_or(&0)
        })
        .collect()
}

fn usize_to_i64_days(days: usize) -> i64 {
    i64::try_from(days).unwrap_or(i64::MAX)
}

fn safe_outcome_total(applied: i64, failed: i64, rejected: i64) -> u64 {
    let total = applied.saturating_add(failed).saturating_add(rejected);
    u64::try_from(total.max(0)).unwrap_or(0)
}

fn label_bar_width(label: &str) -> u16 {
    u16::try_from(label.len())
        .unwrap_or(u16::MAX.saturating_sub(2))
        .saturating_add(2)
}

fn parse_date_prefix(ts: &str) -> Option<chrono::NaiveDate> {
    if ts.len() < 10 {
        return None;
    }
    chrono::NaiveDate::parse_from_str(&ts[..10], "%Y-%m-%d").ok()
}

const ACTIVITY_CACHE_TTL: Duration = Duration::from_millis(250);

static ACTIVITY_CACHE: OnceLock<Mutex<ActivityEventsCache>> = OnceLock::new();

#[derive(Clone, Debug, PartialEq, Eq)]
struct ActivityLogSignature {
    path: Option<PathBuf>,
    modified: Option<SystemTime>,
    len: Option<u64>,
}

impl ActivityLogSignature {
    fn read() -> Self {
        let path = difflore_core::paths::data_home()
            .ok()
            .map(|dir| dir.join("activity.jsonl"));
        let metadata = path.as_ref().and_then(|p| std::fs::metadata(p).ok());
        Self {
            path,
            modified: metadata.as_ref().and_then(|m| m.modified().ok()),
            len: metadata.map(|m| m.len()),
        }
    }
}

#[derive(Clone, Debug)]
struct ActivityEventsCache {
    checked_at: Instant,
    signature: ActivityLogSignature,
    limit: usize,
    events: Vec<ActivityEvent>,
}

impl ActivityEventsCache {
    fn fresh(now: Instant, signature: ActivityLogSignature, limit: usize) -> Self {
        Self {
            checked_at: now,
            signature,
            limit,
            events: activity_stream::tail(limit),
        }
    }

    fn is_fresh(&self, now: Instant, limit: usize) -> bool {
        self.limit == limit && now.saturating_duration_since(self.checked_at) < ACTIVITY_CACHE_TTL
    }

    fn matches_signature(&self, limit: usize, signature: &ActivityLogSignature) -> bool {
        self.limit == limit && &self.signature == signature
    }
}

fn cached_activity_events(limit: usize) -> Vec<ActivityEvent> {
    let now = Instant::now();
    let cache = ACTIVITY_CACHE.get_or_init(|| {
        Mutex::new(ActivityEventsCache::fresh(
            now,
            ActivityLogSignature::read(),
            limit,
        ))
    });
    let Ok(mut cache) = cache.lock() else {
        return activity_stream::tail(limit);
    };

    if cache.is_fresh(now, limit) {
        return cache.events.clone();
    }

    let signature = ActivityLogSignature::read();
    if cache.matches_signature(limit, &signature) {
        cache.checked_at = now;
        return cache.events.clone();
    }

    *cache = ActivityEventsCache::fresh(now, signature, limit);
    cache.events.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evt(ts_ms: i64, payload: ActivityPayload) -> ActivityEvent {
        ActivityEvent { ts_ms, payload }
    }

    #[test]
    fn group_consecutive_collapses_identical_within_one_second() {
        let p = ActivityPayload::RuleRecalled {
            rule_id: "r1".into(),
            rule_title: "t".into(),
            score: 0.5,
            took_ms: 0,
        };
        // Newest-first ordering, three identical events within 200 ms.
        let events = vec![evt(2000, p.clone()), evt(1900, p.clone()), evt(1800, p)];
        let grouped = group_consecutive(&events, 20);
        assert_eq!(grouped.len(), 1);
        assert_eq!(grouped[0].1, 3);
    }

    #[test]
    fn group_consecutive_keeps_distinct_events_separate() {
        let a = ActivityPayload::RuleRecalled {
            rule_id: "r1".into(),
            rule_title: "a".into(),
            score: 0.1,
            took_ms: 0,
        };
        let b = ActivityPayload::RetrievalEmbedding {
            hits: 5,
            took_ms: 12,
        };
        let events = vec![evt(2000, a), evt(1900, b)];
        let grouped = group_consecutive(&events, 20);
        assert_eq!(grouped.len(), 2);
        assert_eq!(grouped[0].1, 1);
        assert_eq!(grouped[1].1, 1);
    }

    #[test]
    fn render_pipeline_with_events_does_not_panic() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let events = vec![
            evt(
                3000,
                ActivityPayload::RuleInjected {
                    rule_count: 2,
                    prompt_chars: 1234,
                    intent_summary: "src/foo.rs · review".into(),
                },
            ),
            evt(
                2500,
                ActivityPayload::EmbeddingFallback {
                    reason: "network".into(),
                },
            ),
            evt(
                2000,
                ActivityPayload::RuleReinforced {
                    rule_id: "r1".into(),
                    rule_title: "Avoid string concat in queries".into(),
                    prev_strength: 1.2,
                    new_strength: 2.2,
                    reason: "fix_accepted".to_owned(),
                },
            ),
        ];
        terminal
            .draw(|f| {
                let area = f.area();
                draw_pipeline_stream(f, area, &events, 0, &HashMap::new());
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let s = buf
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(s.contains("Injected"));
        assert!(s.contains("Embedding fallback"));
        assert!(s.contains("Reinforced"));
    }

    #[test]
    fn clamp_offset_floors_at_zero_when_rows_fit() {
        // Top end: visible budget covers (or exceeds) the row count, so
        // any positive offset gets pulled back to 0 — there is nothing
        // to scroll to.
        assert_eq!(clamp_offset(0, 5, 10), 0);
        assert_eq!(clamp_offset(7, 5, 10), 0);
        assert_eq!(clamp_offset(3, 10, 10), 0);
    }

    #[test]
    fn clamp_offset_caps_at_rows_minus_visible() {
        // Bottom end: offset can never exceed rows - visible. A `G`
        // press lands here; subsequent `j` presses must stick at max.
        assert_eq!(clamp_offset(0, 20, 8), 0);
        assert_eq!(clamp_offset(5, 20, 8), 5);
        assert_eq!(clamp_offset(12, 20, 8), 12);
        assert_eq!(clamp_offset(99, 20, 8), 12);
        // visible = 0 (e.g. unrendered) leaves the full range available.
        assert_eq!(clamp_offset(99, 20, 0), 20);
    }

    #[test]
    fn daily_outcome_totals_fills_missing_days() {
        let today = chrono::Utc::now().naive_utc().date();
        let rows = vec![FixOutcomeDaily {
            day: today.to_string(),
            applied: 2,
            failed: 1,
            rejected: 3,
        }];

        let data = daily_outcome_totals(&rows, 3);

        assert_eq!(data.len(), 3);
        assert_eq!(data[0], 0);
        assert_eq!(data[1], 0);
        assert_eq!(data[2], 6);
    }

    #[test]
    fn safe_outcome_total_clamps_negative_and_saturates() {
        assert_eq!(safe_outcome_total(-10, 1, 2), 0);
        assert_eq!(safe_outcome_total(i64::MAX, i64::MAX, 1), u64::MAX / 2);
        assert_eq!(label_bar_width("applied"), 9);
    }

    #[test]
    fn activity_cache_reuses_events_inside_ttl() {
        let now = Instant::now();
        let signature = ActivityLogSignature {
            path: None,
            modified: None,
            len: None,
        };
        let cache = ActivityEventsCache {
            checked_at: now,
            signature: signature.clone(),
            limit: 40,
            events: Vec::new(),
        };

        assert!(cache.is_fresh(now + Duration::from_millis(1), 40));
        assert!(!cache.is_fresh(now + ACTIVITY_CACHE_TTL + Duration::from_millis(1), 40));
        assert!(cache.matches_signature(40, &signature));
        assert!(!cache.matches_signature(20, &signature));
    }
}
