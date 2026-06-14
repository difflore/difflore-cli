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

    // `status = 'active'` excludes unverified pending rules. The `id DESC`
    // tiebreak relies on ULID ordering tracking insertion time, so it
    // surfaces the truly-latest insert when two rules share an
    // `installed_at` second.
    let rows = sqlx::query(
        r"SELECT name, origin, source_repo
          FROM skills
          WHERE status = 'active'
            AND source_repo IS NOT NULL
            AND TRIM(source_repo) <> ''
            AND LOWER(source_repo) IN (SELECT value FROM json_each(?1))
            AND datetime(installed_at) > datetime(?2)
          ORDER BY datetime(installed_at) DESC, id DESC
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
