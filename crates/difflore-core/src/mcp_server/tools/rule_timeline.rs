use serde_json::{Value, json};

use crate::context::types::{EvidenceKind, EvidenceRecord};
use crate::review_trajectory::TrajectoryStep;

use super::super::{
    AVG_FULL_RULE_TOKENS, McpState, build_cost_meta, emit_trajectory_step, estimate_tokens,
};
use super::util::{build_timeline_evidence, origin_to_kind, rule_preview, truncate_chars};

/// Max `depth_before` / `depth_after` a caller can request.
pub(crate) const RULE_TIMELINE_MAX_DEPTH: u32 = 20;

/// Max preview length; control characters are stripped upstream.
pub(crate) const TIMELINE_PREVIEW_MAX_CHARS: usize = 120;

/// One timeline row. `ts` is the ISO8601 local-timestamp string SQLite writes
/// to `skills.installed_at` / `rule_examples.created_at`, kept opaque so it
/// lex-sorts chronologically without a tz-aware parse step.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct TimelineRow {
    id: String,
    ts: String,
    kind: &'static str,
    source: String,
    preview: String,
    evidence: Vec<EvidenceRecord>,
}

/// Row shape for skill lookup. `installed_at` is the focal timestamp; follow-up
/// feedback is read from `rule_events`.
#[derive(sqlx::FromRow)]
pub(crate) struct TimelineSkillRow {
    id: String,
    name: String,
    description: String,
    origin: String,
    installed_at: String,
    /// "learned from <repo>" provenance. Optional — manual / global rules have
    /// no upstream.
    source_repo: Option<String>,
}

/// Row shape for example lookup. `source` records how the example landed
/// (`extracted`, `pr_review`, `manual`, ...).
#[derive(sqlx::FromRow)]
pub(crate) struct TimelineExampleRow {
    id: String,
    bad_code: String,
    good_code: String,
    description: Option<String>,
    source: String,
    created_at: String,
}

#[derive(sqlx::FromRow)]
pub(crate) struct TimelineEventRow {
    id: String,
    kind: String,
    source: String,
    confidence_before: Option<f64>,
    confidence_after: Option<f64>,
    reason: Option<String>,
    created_at: String,
}

pub(crate) async fn tool_rule_timeline(
    state: &McpState,
    args: &Value,
) -> Result<Value, (i32, String)> {
    let rule_id = args
        .get("rule_id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or((-32602, "Missing required parameter: rule_id".to_owned()))?
        .to_owned();

    // Clamp depths to [0, RULE_TIMELINE_MAX_DEPTH]. Defaults match the tool
    // schema so omitting the field and sending null behave the same.
    let depth_before = args
        .get("depth_before")
        .and_then(Value::as_u64)
        .map_or(5, |n| n.min(u64::from(RULE_TIMELINE_MAX_DEPTH)) as usize);
    let depth_after = args
        .get("depth_after")
        .and_then(Value::as_u64)
        .map_or(5, |n| n.min(u64::from(RULE_TIMELINE_MAX_DEPTH)) as usize);

    // Fetch the focal rule first so a typo'd id 404s cleanly rather than
    // returning a silent empty array. Runtime query avoids `.sqlx` cache
    // updates for this diagnostic surface.
    let skill: Option<TimelineSkillRow> = sqlx::query_as(
        "SELECT id, name, description, origin, installed_at, source_repo \
         FROM skills WHERE id = ?1 AND status = 'active'",
    )
    .bind(&rule_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| (-32603, format!("Failed to look up rule: {e}")))?;

    let Some(skill) = skill else {
        return Err((
            -32602,
            format!(
                "rule '{rule_id}' not found; run `difflore status --json` to inspect local memory."
            ),
        ));
    };

    // Build the event list from every local source:
    //   • the skill row → one creation event at `installed_at`.
    //   • rule_examples → one event per example at `created_at`.
    //   • rule_events → durable feedback signals (accept / dismiss).
    let mut rows: Vec<TimelineRow> = Vec::new();
    let created_preview = rule_preview(&skill.description, TIMELINE_PREVIEW_MAX_CHARS);
    let created_kind = origin_to_kind(&skill.origin);
    rows.push(TimelineRow {
        id: skill.id.clone(),
        ts: skill.installed_at.clone(),
        kind: created_kind,
        source: skill.origin.clone(),
        preview: format!("Rule created: {}", truncate_chars(&skill.name, 80))
            .chars()
            .take(TIMELINE_PREVIEW_MAX_CHARS)
            .collect::<String>(),
        evidence: vec![build_timeline_evidence(
            EvidenceKind::RuleCreated,
            &skill.origin,
            &skill.installed_at,
            &created_preview,
        )],
    });
    // Examples — one row each, keyed separately from the rule so agents can
    // cite a specific example as evidence.
    let examples: Vec<TimelineExampleRow> = sqlx::query_as!(
        TimelineExampleRow,
        "SELECT id, bad_code, good_code, description, source, created_at \
         FROM rule_examples WHERE skill_id = ?1 ORDER BY created_at ASC",
        rule_id
    )
    .fetch_all(&state.db)
    .await
    .map_err(|e| (-32603, format!("Failed to load rule examples: {e}")))?;

    let examples_count = examples.len();
    for ex in examples {
        // Prefer the example description; fall back to a bad→good one-liner so
        // the row still carries signal when no description was captured.
        let raw = ex
            .description
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| {
                format!(
                    "bad: {} -> good: {}",
                    truncate_chars(&ex.bad_code, 40),
                    truncate_chars(&ex.good_code, 40),
                )
            });
        let preview = rule_preview(&raw, TIMELINE_PREVIEW_MAX_CHARS);
        let kind = {
            #[allow(clippy::match_same_arms)]
            // reason: explicit "extracted" branch documents the source while wildcard handles unknown sources
            match ex.source.as_str() {
                "pr_review" => "pr_review",
                "extracted" => "extracted",
                "conversation" => "remember",
                "manual" => "manual",
                _ => "extracted",
            }
        };
        let evidence = vec![build_timeline_evidence(
            EvidenceKind::RuleExample,
            &ex.source,
            &ex.created_at,
            &preview,
        )];
        rows.push(TimelineRow {
            id: ex.id,
            ts: ex.created_at,
            kind,
            source: ex.source,
            preview,
            evidence,
        });
    }

    let feedback_events: Vec<TimelineEventRow> = sqlx::query_as!(
        TimelineEventRow,
        "SELECT id, kind, source, confidence_before, confidence_after, reason, created_at \
         FROM rule_events WHERE skill_id = ?1 ORDER BY created_at ASC",
        rule_id
    )
    .fetch_all(&state.db)
    .await
    .map_err(|e| (-32603, format!("Failed to load rule events: {e}")))?;

    for ev in feedback_events {
        let preview_raw = match (ev.confidence_before, ev.confidence_after) {
            (Some(before), Some(after)) => {
                format!(
                    "{}: confidence {:.2} -> {:.2}",
                    ev.kind.replace('_', " "),
                    before,
                    after
                )
            }
            _ => ev.reason.unwrap_or_else(|| ev.kind.replace('_', " ")),
        };
        let kind = match ev.kind.as_str() {
            "feedback_accept" => "feedback_accept",
            "feedback_dismiss" => "feedback_dismiss",
            _ => "updated",
        };
        let evidence = vec![build_timeline_evidence(
            EvidenceKind::RuleUpdated,
            &ev.source,
            &ev.created_at,
            &preview_raw,
        )];
        rows.push(TimelineRow {
            id: ev.id,
            ts: ev.created_at,
            kind,
            source: ev.source,
            preview: rule_preview(&preview_raw, TIMELINE_PREVIEW_MAX_CHARS),
            evidence,
        });
    }

    // Chronological asc; tie-break on id so rows sharing a timestamp (SQLite's
    // 1-sec resolution) stay deterministic across calls.
    rows.sort_by(|a, b| a.ts.cmp(&b.ts).then(a.id.cmp(&b.id)));

    // Split around the focal row (skill.installed_at): `depth_before` rows
    // before it and `depth_after` after, focal always included.
    let focal_ts = skill.installed_at.clone();
    let focal_idx = rows
        .iter()
        .position(|r| r.ts == focal_ts && r.id == skill.id)
        .unwrap_or(0);
    let before_start = focal_idx.saturating_sub(depth_before);
    let after_end = (focal_idx + 1 + depth_after).min(rows.len());
    // TimelineRow serializes to the same JSON shape as RuleTimelineEventRecord
    // (snake_case field names match), so drain the sub-slice straight to serde.
    let events: Vec<TimelineRow> = rows.drain(before_start..after_end).collect();

    // Surface source_repo at the top level so an agent can narrate "learned
    // from <repo>". Only set when present (empty/blank is elided).
    let source_repo = skill
        .source_repo
        .clone()
        .filter(|r: &String| !r.trim().is_empty());
    let body = match source_repo {
        Some(repo) => json!({
            "rule_id": skill.id,
            "rule_name": skill.name,
            "source_repo": repo,
            "focal_ts": focal_ts,
            "events": events,
        }),
        None => json!({
            "rule_id": skill.id,
            "rule_name": skill.name,
            "focal_ts": focal_ts,
            "events": events,
        }),
    };
    let text = serde_json::to_string(&body).map_err(|e| {
        (
            -32603,
            format!("Failed to serialise rule_timeline response: {e}"),
        )
    })?;

    let tokens_used = estimate_tokens(&text);
    emit_trajectory_step(&TrajectoryStep::McpResponseSize {
        tool: "rule_timeline".to_owned(),
        total_tokens: tokens_used,
        rules_injected: 0,
    });

    // Estimate savings versus fetching every referenced full rule.
    let referenced_rules = 1 + examples_count;
    let tokens_if_full = Some(AVG_FULL_RULE_TOKENS * referenced_rules);

    Ok(json!({
        "content": [{ "type": "text", "text": text }],
        "_meta": {
            "cost": build_cost_meta(tokens_used, tokens_if_full),
            "impact": {
                "kind": "rule_timeline",
                "events": events.len(),
            }
        }
    }))
}
