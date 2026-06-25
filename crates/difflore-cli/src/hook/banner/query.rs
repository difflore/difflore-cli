//! SQL for "new rules since `prev_ts`, scoped to this repo".
//!
//! Reads canonical rules from `~/.difflore/data.db`, filtering
//! `source_repo` against the current repo's aliases so a user in
//! `acme/billing` isn't spammed with rules learned in `acme/notifier`.
//!
//! Uses `skills.installed_at` (rule creation) rather than the
//! `rule_events` stream, whose `created_at` captures every state change —
//! using it would surface confidence-bumps as "new rules" and make the
//! banner noisy.

use sqlx::Row;

/// One banner row. Fields are passed through verbatim from the DB and may
/// be long; the render module is free to truncate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewRule {
    /// Rule name from `skills.name`.
    pub title: String,
    /// `origin` enum value (`manual`, `conversation`, `pr_review`,
    /// `extracted`); drives the provenance phrase in the banner.
    pub origin: String,
    /// `source_repo`, used for the optional "← from {repo}" suffix.
    pub source_repo: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemoryPulseSummary {
    pub folded_away: usize,
    pub to_confirm: usize,
}

impl MemoryPulseSummary {
    pub const fn should_render(&self, ready_count: usize) -> bool {
        ready_count > 0 || self.to_confirm > 0
    }

    pub const fn is_empty(&self) -> bool {
        self.folded_away == 0 && self.to_confirm == 0
    }
}

/// Up to `limit` rules whose `installed_at` is later than `prev_ts_ms`
/// (millis since epoch) AND whose `source_repo` matches one of
/// `repo_aliases` (case-insensitive), ordered newest-first. When
/// `prev_ts_ms` is `None` the time filter is dropped, returning the most
/// recent rules — the "first session ever" case where everything is new.
/// SQL errors propagate as `Err(String)`.
pub async fn new_rules_since(
    db: &difflore_core::SqlitePool,
    prev_ts_ms: Option<i64>,
    repo_aliases: &[String],
    limit: usize,
) -> Result<Vec<NewRule>, String> {
    if repo_aliases.is_empty() {
        // No repo identity would select rules from every repo on the
        // machine — the exact noise this filter prevents.
        return Ok(Vec::new());
    }

    // Bind the alias set as one JSON-array parameter, unfolded via
    // `json_each` in the SQL. Matches `commands::status::queries` so
    // sqlite's plan cache reuses the same shape.
    let repos_json = serde_json::to_string(repo_aliases).map_err(|e| format!("aliases: {e}"))?;

    // `skills.installed_at` is stored via `datetime('now')` (second
    // precision, no millis), so compare via `datetime(…)` on both sides.
    // `None` maps to the unix epoch, keeping the SQL single-shape and
    // matching every row.
    let watermark_iso = prev_ts_ms
        .and_then(chrono::DateTime::<chrono::Utc>::from_timestamp_millis)
        .map_or_else(|| "1970-01-01T00:00:00Z".to_owned(), |dt| dt.to_rfc3339());

    let limit_i64 = i64::try_from(limit).unwrap_or(5);

    ensure_autopilot_events_table(db).await?;

    // `status = 'active'` excludes unverified pending rules. The
    // auto-enabled branch keys off `memory_autopilot_events.created_at`, not
    // `skills.installed_at`, so old drafts promoted in the background still
    // appear in the next SessionStart banner. The regular installed_at branch
    // excludes those same rule ids to avoid duplicate bullets.
    let rows = sqlx::query(
        r"WITH auto_enabled AS (
              SELECT rule_id, MAX(created_at) AS surfaced_at
              FROM memory_autopilot_events
              WHERE event_type = 'auto_enabled'
                AND rule_id IS NOT NULL
                AND datetime(created_at) > datetime(?2)
              GROUP BY rule_id
          ),
          banner_rows AS (
              SELECT id, name, origin, source_repo, installed_at AS surfaced_at
              FROM skills
              WHERE status = 'active'
                AND source_repo IS NOT NULL
                AND TRIM(source_repo) <> ''
                AND LOWER(source_repo) IN (SELECT value FROM json_each(?1))
                AND datetime(installed_at) > datetime(?2)
                AND id NOT IN (SELECT rule_id FROM auto_enabled)
              UNION ALL
              SELECT s.id, s.name, 'autopilot' AS origin, s.source_repo, a.surfaced_at
              FROM auto_enabled a
              JOIN skills s ON s.id = a.rule_id
              WHERE s.status = 'active'
                AND s.source_repo IS NOT NULL
                AND TRIM(s.source_repo) <> ''
                AND LOWER(s.source_repo) IN (SELECT value FROM json_each(?1))
          )
          SELECT name, origin, source_repo
          FROM banner_rows
          ORDER BY datetime(surfaced_at) DESC, id DESC
          LIMIT ?3",
    )
    .bind(repos_json)
    .bind(watermark_iso)
    .bind(limit_i64)
    .fetch_all(db)
    .await
    .map_err(|e| format!("query skills: {e}"))?;

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let title: String = row.try_get("name").unwrap_or_default();
        let origin: String = row
            .try_get("origin")
            .unwrap_or_else(|_| "manual".to_owned());
        let source_repo: Option<String> = row.try_get("source_repo").ok();
        if title.trim().is_empty() {
            // Skip corrupted rows rather than render an empty bullet.
            continue;
        }
        out.push(NewRule {
            title,
            origin,
            source_repo,
        });
    }
    Ok(out)
}

pub async fn memory_pulse_since(
    db: &difflore_core::SqlitePool,
    prev_ts_ms: Option<i64>,
    repo_aliases: &[String],
) -> Result<MemoryPulseSummary, String> {
    if repo_aliases.is_empty() {
        return Ok(MemoryPulseSummary::default());
    }

    let aliases = repo_aliases
        .iter()
        .map(|alias| alias.to_ascii_lowercase())
        .collect::<Vec<_>>();
    let repos_json = serde_json::to_string(&aliases).map_err(|e| format!("aliases: {e}"))?;
    let watermark_iso = prev_ts_ms
        .and_then(chrono::DateTime::<chrono::Utc>::from_timestamp_millis)
        .map_or_else(|| "1970-01-01T00:00:00Z".to_owned(), |dt| dt.to_rfc3339());

    ensure_autopilot_events_table(db).await?;
    let rows = sqlx::query(
        r"SELECT event_type, payload_json
          FROM memory_autopilot_events
          WHERE datetime(created_at) > datetime(?2)
            AND event_type IN (
                'session_candidate_superseded',
                'session_candidate_dropped_low_signal',
                'session_candidate_active_covered',
                'agent_file_review_rule_pending',
                'candidate_confirm_pending'
            )
            AND group_id IS NOT NULL
            AND EXISTS (
                SELECT 1
                FROM json_each(?1)
                WHERE LOWER(group_id) LIKE value || ':%'
            )",
    )
    .bind(repos_json)
    .bind(watermark_iso)
    .fetch_all(db)
    .await
    .map_err(|e| format!("query memory pulse: {e}"))?;

    let mut pulse = MemoryPulseSummary::default();
    for row in rows {
        let event_type: String = row.try_get("event_type").unwrap_or_default();
        let payload_json: String = row.try_get("payload_json").unwrap_or_default();
        let payload = serde_json::from_str::<serde_json::Value>(&payload_json)
            .unwrap_or_else(|_| serde_json::json!({}));
        match event_type.as_str() {
            "session_candidate_superseded" => {
                pulse.folded_away += payload
                    .get("supersededCount")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|count| usize::try_from(count).ok())
                    .unwrap_or(1);
            }
            "session_candidate_dropped_low_signal" | "session_candidate_active_covered" => {
                pulse.folded_away += payload
                    .get("deletedCount")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|count| usize::try_from(count).ok())
                    .unwrap_or(1);
            }
            "agent_file_review_rule_pending" | "candidate_confirm_pending" => {
                pulse.to_confirm += 1;
            }
            _ => {}
        }
    }

    Ok(pulse)
}

async fn ensure_autopilot_events_table(db: &difflore_core::SqlitePool) -> Result<(), String> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS memory_autopilot_events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            event_type TEXT NOT NULL,
            rule_id TEXT,
            item_ids_json TEXT NOT NULL DEFAULT '[]',
            group_id TEXT,
            title TEXT NOT NULL DEFAULT '',
            reason TEXT NOT NULL DEFAULT '',
            payload_json TEXT NOT NULL DEFAULT '{}',
            created_at TEXT DEFAULT (datetime('now')) NOT NULL
        )",
    )
    .execute(db)
    .await
    .map_err(|e| format!("ensure autopilot events: {e}"))?;
    Ok(())
}
