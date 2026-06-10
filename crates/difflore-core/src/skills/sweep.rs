//! Maintenance sweeps for stale or weakly grounded skills.
//!
//! 1. [`sweep_stale_skills`] multiplicatively decays `confidence_score` on
//!    skills installed long enough ago to have a track record, NOT served in
//!    the recent window, and that have NEVER earned an accepted fix. Confidence
//!    is a multiplier in the recall ranker (`context::retrieval::rules`), so
//!    halving it drops the chunk out of the top 5 over two sweeps without
//!    deleting anything.
//!
//! 2. [`quarantine_unguided_conv_reviews`] flips `conv-review-*` skills with
//!    neither `file_patterns` nor a `trigger` to `status = 'pending'`. Those
//!    are imported from PR comments with no grounding, so the embedding does
//!    all the work and usually loses to a real rule. Pending keeps the row (it
//!    can be promoted back by a human or an accept event) but removes it from
//!    active recall.
//!
//! Hard constraints: no schema changes, no DELETEs, all writes in a single
//! sqlx transaction so a partial sweep can't half-decay the corpus.

use serde::Serialize;
use sqlx::SqlitePool;

use crate::Result;

/// Tuning knobs for [`sweep_stale_skills`].
/// Defaults: 14-day install/serve windows, x0.5 decay, 0.05 floor.
#[derive(Debug, Clone, Copy)]
pub struct SweepOpts {
    /// Skip skills installed within the last `stale_install_days` —
    /// they haven't had a fair chance to be served yet.
    pub stale_install_days: u32,
    /// A skill counts as "active" if any `mcp_rule_serves` row inside
    /// the last `stale_serve_days` references it via `rule_ids_json`.
    pub stale_serve_days: u32,
    /// Multiplier applied to `confidence_score` per sweep. Defaults to 0.5 so
    /// two sweeps drop a 0.6 base below the 0.15 cutoff.
    pub decay_factor: f32,
    /// When true, compute the decay plan without committing.
    pub dry_run: bool,
    /// Confidence floor; we never drive below this.
    pub min_floor: f32,
}

impl Default for SweepOpts {
    fn default() -> Self {
        Self {
            stale_install_days: 14,
            stale_serve_days: 14,
            decay_factor: 0.5,
            dry_run: false,
            min_floor: 0.05,
        }
    }
}

/// Outcome of one [`sweep_stale_skills`] pass. Counts are snapshots taken in
/// the same transaction as the writes (or the dry-run preview).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SweepReport {
    /// Total skills considered by the sweep (`installed_at < cutoff`).
    pub examined: u64,
    /// Skills whose confidence was actually multiplied by `decay_factor`.
    pub decayed: u64,
    /// Skills skipped because they were served inside the recent window
    /// or had an accepted `fix_outcomes` row.
    pub skipped_because_active: u64,
    /// Skills already at or below `min_floor` — left untouched so the
    /// floor stays meaningful.
    pub skipped_because_already_at_floor: u64,
    /// Echo of [`SweepOpts::dry_run`]: whether this report is a real or
    /// simulated pass.
    pub dry_run: bool,
}

/// Decay `confidence_score` on truly-stale skills. See module docs.
pub async fn sweep_stale_skills(pool: &SqlitePool, opts: SweepOpts) -> Result<SweepReport> {
    let install_window = format!("-{} days", opts.stale_install_days);
    let serve_window = format!("-{} days", opts.stale_serve_days);

    // Single SQL pass identifies the decay candidates and computes bucket
    // counts on the same snapshot so report numbers reconcile with what we'd
    // UPDATE. Buckets, per stale-installed skill:
    //   - "active"   → served in the last `stale_serve_days` OR has an accepted
    //                  fix_outcomes row
    //   - "at_floor" → confidence_score <= min_floor
    //   - "decay"    → everything else: the actual targets
    // The cross-join expands rule_ids_json (a JSON array of skill ids) into one
    // row per id via json_each, so SQLite's json1 builds the right text-affinity
    // comparison against skills.id.
    let stale_serve_subquery = "id IN (\
        SELECT value FROM mcp_rule_serves, json_each(rule_ids_json) \
        WHERE served_at > datetime('now', ?2)\
    )";
    let accepted_subquery =
        "id IN (SELECT rule_id FROM fix_outcomes WHERE rule_id IS NOT NULL AND accepted = 1)";

    // Materialise the candidate ids so the dry-run preview and the real UPDATE
    // see the same snapshot. Stale candidates number in the low thousands, so a
    // fetch_all is fine.
    let candidates_sql = format!(
        "SELECT id, confidence_score FROM skills \
         WHERE installed_at < datetime('now', ?1) \
           AND NOT ({stale_serve_subquery}) \
           AND NOT ({accepted_subquery})"
    );

    let examined: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM skills WHERE installed_at < datetime('now', ?1)")
            .bind(&install_window)
            .fetch_one(pool)
            .await?;

    let rows: Vec<(String, f64)> = sqlx::query_as(&candidates_sql)
        .bind(&install_window)
        .bind(&serve_window)
        .fetch_all(pool)
        .await?;

    // Anything above floor is a decay target; floor-or-below is skipped.
    let floor = f64::from(opts.min_floor);
    let to_decay: Vec<&(String, f64)> = rows.iter().filter(|(_, c)| *c > floor).collect();
    let at_floor_len = rows.len() - to_decay.len();

    let decayed_count = u64::try_from(to_decay.len()).unwrap_or(u64::MAX);
    let at_floor_count = u64::try_from(at_floor_len).unwrap_or(u64::MAX);
    let examined_u64 = u64::try_from(examined).unwrap_or(0);
    // "active" = stale-installed minus the candidates we pulled.
    let skipped_active = examined_u64.saturating_sub(decayed_count + at_floor_count);

    let report = SweepReport {
        examined: examined_u64,
        decayed: decayed_count,
        skipped_because_active: skipped_active,
        skipped_because_already_at_floor: at_floor_count,
        dry_run: opts.dry_run,
    };

    if opts.dry_run || to_decay.is_empty() {
        return Ok(report);
    }

    // Single transaction so a mid-batch failure leaves the corpus consistent.
    let factor = f64::from(opts.decay_factor);
    let mut tx = pool.begin().await?;
    for (id, conf) in &to_decay {
        let new_conf = (conf * factor).max(floor);
        sqlx::query(
            "UPDATE skills SET confidence_score = ?1, updated_at = datetime('now') WHERE id = ?2",
        )
        .bind(new_conf)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;

    Ok(report)
}

/// Outcome of [`quarantine_unguided_conv_reviews`]. `flipped_ids` is the
/// exact list of skill ids that moved from `active` to `pending` —
/// useful for both audit logs and undo scripts.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuarantineReport {
    pub examined: u64,
    pub flipped: u64,
    pub flipped_ids: Vec<String>,
    pub dry_run: bool,
}

/// Move `conv-review-*` skills with neither `file_patterns` nor a
/// `trigger` to `status='pending'`. See module docs for the rationale.
pub async fn quarantine_unguided_conv_reviews(
    pool: &SqlitePool,
    dry_run: bool,
) -> Result<QuarantineReport> {
    // file_patterns is TEXT NULLable — treat NULL, "", and "[]" as
    // "unguided". trigger is TEXT NULLable — treat NULL and "" the same.
    let candidates: Vec<String> = sqlx::query_scalar(
        "SELECT id FROM skills \
         WHERE id LIKE 'conv-review-%' \
           AND status = 'active' \
           AND (file_patterns IS NULL OR file_patterns = '' OR file_patterns = '[]') \
           AND (trigger IS NULL OR trigger = '')",
    )
    .fetch_all(pool)
    .await?;

    let examined_total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM skills WHERE id LIKE 'conv-review-%' AND status = 'active'",
    )
    .fetch_one(pool)
    .await?;

    let report = QuarantineReport {
        examined: u64::try_from(examined_total).unwrap_or(0),
        flipped: u64::try_from(candidates.len()).unwrap_or(u64::MAX),
        flipped_ids: candidates.clone(),
        dry_run,
    };

    if dry_run || candidates.is_empty() {
        return Ok(report);
    }

    let mut tx = pool.begin().await?;
    for id in &candidates {
        sqlx::query(
            "UPDATE skills SET status = 'pending', updated_at = datetime('now') WHERE id = ?1",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn fresh_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        // Minimal schema covering only what the sweep touches.
        sqlx::query(
            "CREATE TABLE skills (
                id TEXT PRIMARY KEY NOT NULL,
                name TEXT NOT NULL DEFAULT '',
                source TEXT NOT NULL DEFAULT '',
                directory TEXT NOT NULL DEFAULT '',
                version TEXT NOT NULL DEFAULT '',
                description TEXT NOT NULL DEFAULT '',
                type TEXT NOT NULL DEFAULT 'skill',
                engines TEXT NOT NULL DEFAULT '[]',
                tags TEXT NOT NULL DEFAULT '[]',
                trigger TEXT,
                check_prompt TEXT,
                repo_owner TEXT,
                repo_name TEXT,
                repo_branch TEXT,
                readme_url TEXT,
                source_repo TEXT,
                enabled_for_codex INTEGER NOT NULL DEFAULT 0,
                enabled_for_claude INTEGER NOT NULL DEFAULT 0,
                enabled_for_gemini INTEGER NOT NULL DEFAULT 0,
                enabled_for_cursor INTEGER NOT NULL DEFAULT 0,
                confidence_score REAL NOT NULL DEFAULT 0.7,
                file_patterns TEXT,
                origin TEXT NOT NULL DEFAULT 'manual',
                content_hash TEXT,
                hash_created_at INTEGER,
                status TEXT NOT NULL DEFAULT 'active',
                installed_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            )",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TABLE mcp_rule_serves (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                rule_ids_json TEXT NOT NULL DEFAULT '[]',
                served_at TEXT NOT NULL DEFAULT (datetime('now'))
            )",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TABLE fix_outcomes (
                id TEXT PRIMARY KEY NOT NULL,
                rule_id TEXT,
                rule_name TEXT NOT NULL DEFAULT '',
                accepted INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            )",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    /// Insert a skill with `installed_at = datetime('now', age_modifier)`.
    /// `age_modifier` is a SQLite modifier like "-20 days" or "-2 days".
    async fn insert_skill(
        pool: &SqlitePool,
        id: &str,
        confidence: f64,
        age_modifier: &str,
        file_patterns: Option<&str>,
        trigger: Option<&str>,
    ) {
        sqlx::query(
            "INSERT INTO skills (id, name, confidence_score, installed_at, file_patterns, trigger) \
             VALUES (?1, ?1, ?2, datetime('now', ?3), ?4, ?5)",
        )
        .bind(id)
        .bind(confidence)
        .bind(age_modifier)
        .bind(file_patterns)
        .bind(trigger)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn confidence(pool: &SqlitePool, id: &str) -> f64 {
        sqlx::query_scalar("SELECT confidence_score FROM skills WHERE id = ?1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// Tolerant float comparison to satisfy clippy's `float_cmp` lint.
    fn approx_eq(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    async fn status(pool: &SqlitePool, id: &str) -> String {
        sqlx::query_scalar("SELECT status FROM skills WHERE id = ?1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    fn opts() -> SweepOpts {
        SweepOpts::default()
    }

    #[tokio::test]
    async fn sweep_only_decays_stale_never_served_with_no_accept() {
        let pool = fresh_pool().await;
        // 1. Fresh (<14d): should be skipped (not even examined).
        insert_skill(&pool, "fresh", 0.7, "-2 days", None, None).await;
        // 2. Stale & never served: the only legitimate decay target.
        insert_skill(&pool, "stale-quiet", 0.7, "-20 days", None, None).await;
        // 3. Stale but served 5 days ago: counts as "active".
        insert_skill(&pool, "stale-served", 0.7, "-20 days", None, None).await;
        sqlx::query(
            "INSERT INTO mcp_rule_serves (rule_ids_json, served_at) \
             VALUES (?1, datetime('now', '-5 days'))",
        )
        .bind(r#"["stale-served"]"#)
        .execute(&pool)
        .await
        .unwrap();
        // 4. Stale with accepted fix: counts as "active".
        insert_skill(&pool, "stale-accepted", 0.7, "-20 days", None, None).await;
        sqlx::query(
            "INSERT INTO fix_outcomes (id, rule_id, rule_name, accepted) \
             VALUES ('fo-1', 'stale-accepted', 'stale-accepted', 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        // 5. Stale already at floor: skipped because at floor.
        insert_skill(&pool, "stale-floor", 0.05, "-20 days", None, None).await;

        let report = sweep_stale_skills(&pool, opts()).await.unwrap();

        assert!(approx_eq(confidence(&pool, "fresh").await, 0.7));
        assert!(approx_eq(confidence(&pool, "stale-quiet").await, 0.35));
        assert!(approx_eq(confidence(&pool, "stale-served").await, 0.7));
        assert!(approx_eq(confidence(&pool, "stale-accepted").await, 0.7));
        assert!(approx_eq(confidence(&pool, "stale-floor").await, 0.05));

        assert_eq!(report.decayed, 1);
        assert_eq!(report.skipped_because_already_at_floor, 1);
        // 4 stale skills examined (fresh excluded by install window).
        assert_eq!(report.examined, 4);
        // 4 examined - 1 decayed - 1 at_floor = 2 active.
        assert_eq!(report.skipped_because_active, 2);
        assert!(!report.dry_run);
    }

    #[tokio::test]
    async fn sweep_dry_run_does_not_commit() {
        let pool = fresh_pool().await;
        insert_skill(&pool, "fresh", 0.7, "-2 days", None, None).await;
        insert_skill(&pool, "stale-quiet", 0.7, "-20 days", None, None).await;
        insert_skill(&pool, "stale-served", 0.7, "-20 days", None, None).await;
        sqlx::query(
            "INSERT INTO mcp_rule_serves (rule_ids_json, served_at) \
             VALUES (?1, datetime('now', '-5 days'))",
        )
        .bind(r#"["stale-served"]"#)
        .execute(&pool)
        .await
        .unwrap();
        insert_skill(&pool, "stale-accepted", 0.7, "-20 days", None, None).await;
        sqlx::query(
            "INSERT INTO fix_outcomes (id, rule_id, rule_name, accepted) \
             VALUES ('fo-1', 'stale-accepted', 'stale-accepted', 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        insert_skill(&pool, "stale-floor", 0.05, "-20 days", None, None).await;

        let dry = SweepOpts {
            dry_run: true,
            ..SweepOpts::default()
        };
        let report = sweep_stale_skills(&pool, dry).await.unwrap();

        // The report still reflects what *would* happen…
        assert_eq!(report.decayed, 1);
        assert!(report.dry_run);
        // …but NO row was actually updated.
        assert!(approx_eq(confidence(&pool, "fresh").await, 0.7));
        assert!(approx_eq(confidence(&pool, "stale-quiet").await, 0.7));
        assert!(approx_eq(confidence(&pool, "stale-served").await, 0.7));
        assert!(approx_eq(confidence(&pool, "stale-accepted").await, 0.7));
        assert!(approx_eq(confidence(&pool, "stale-floor").await, 0.05));
    }

    #[tokio::test]
    async fn quarantine_flips_only_unguided_conv_reviews() {
        let pool = fresh_pool().await;
        // 1. conv-review with file_patterns: keep active.
        insert_skill(
            &pool,
            "conv-review-1",
            0.6,
            "-1 days",
            Some(r#"["**/*.rs"]"#),
            None,
        )
        .await;
        // 2. conv-review with neither: quarantine.
        insert_skill(&pool, "conv-review-2", 0.6, "-1 days", None, None).await;
        // 3. conv-review with trigger only: keep active.
        insert_skill(
            &pool,
            "conv-review-3",
            0.6,
            "-1 days",
            None,
            Some("when editing"),
        )
        .await;

        let report = quarantine_unguided_conv_reviews(&pool, false)
            .await
            .unwrap();

        assert_eq!(report.flipped, 1);
        assert_eq!(report.flipped_ids, vec!["conv-review-2".to_owned()]);
        assert_eq!(status(&pool, "conv-review-1").await, "active");
        assert_eq!(status(&pool, "conv-review-2").await, "pending");
        assert_eq!(status(&pool, "conv-review-3").await, "active");
    }

    #[tokio::test]
    async fn decay_is_bounded_by_min_floor() {
        let pool = fresh_pool().await;
        // Confidence just above floor — one decay step would push it
        // below if we didn't clamp.
        insert_skill(&pool, "just-above-floor", 0.06, "-20 days", None, None).await;

        let report = sweep_stale_skills(&pool, opts()).await.unwrap();
        assert_eq!(report.decayed, 1);

        let new_conf = confidence(&pool, "just-above-floor").await;
        // 0.06 * 0.5 = 0.03, clamped to 0.05 floor.
        assert!(
            (new_conf - 0.05).abs() < 1e-6,
            "expected floor clamp at 0.05, got {new_conf}"
        );
        // And running again should hit the at_floor short-circuit.
        let report2 = sweep_stale_skills(&pool, opts()).await.unwrap();
        assert_eq!(report2.decayed, 0);
        assert_eq!(report2.skipped_because_already_at_floor, 1);
    }
}
