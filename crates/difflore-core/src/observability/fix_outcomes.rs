use sqlx::SqlitePool;
use uuid::Uuid;

#[derive(Debug, Clone, Copy)]
pub struct FixOutcomeInput<'a> {
    pub rule_id: Option<&'a str>,
    pub rule_name: &'a str,
    pub file_path: Option<&'a str>,
    pub repo_full_name: Option<&'a str>,
    pub pr_number: Option<i64>,
    pub diff_signature: Option<&'a str>,
    pub accepted: bool,
    pub applied_ok: bool,
    pub failed_reason: Option<&'a str>,
}

/// Best-effort `rule_name → skills.id` lookup. Used by both the
/// `fix_outcomes` write path (to back-fill `rule_id` at insert time when
/// the caller only knew the human name) and by the one-shot
/// `difflore skills backfill-attribution` CLI command.
///
/// Returns `None` if `name` is empty/whitespace, or if neither the exact
/// match nor the case-insensitive prefix match resolves a single id. All
/// SQL errors are swallowed and surface as `None` because this is a
/// best-effort enrichment — callers must never fail the hot path on a
/// missed attribution lookup.
///
/// **Collision disambiguation**: duplicate skill names exist, so a naive
/// `LIMIT 1` picks an arbitrary row. Exact and prefix queries filter by
/// `status = 'active'` and rank candidates by:
///   1. skill installed at or before the outcome timestamp (so accepts
///      attribute to a skill that actually existed when the user took
///      the action),
///   2. higher `confidence_score`,
///   3. most-recent `installed_at`.
///
/// `as_of` is typically `fix_outcomes.created_at`. Callers that don't
/// have one (e.g. the write path before insert) may pass `None`, which
/// disables the "installed-before" preference and falls back to the
/// confidence/recency ordering.
pub async fn resolve_rule_id_by_name(
    pool: &SqlitePool,
    name: &str,
    as_of: Option<&str>,
) -> Option<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return None;
    }

    // Empty string makes every `installed_at <= ''` comparison false in
    // SQLite, which collapses the first ORDER BY arm to "all
    // candidates rank as 1" — i.e. confidence/recency wins. That's the
    // intent when the caller has no timestamp.
    let as_of_bind = as_of.unwrap_or("");

    // 1) Exact match (case-sensitive), ranked. Covers the common case
    //    where the rule_name was lifted verbatim from a `skills.name`
    //    row. Active-only + installed-before-as_of + higher confidence
    //    + most-recent install collapses N collided rows to one
    //    deterministic pick.
    let exact: Result<Option<String>, _> = sqlx::query_scalar(
        "SELECT id FROM skills \
         WHERE name = ?1 AND status = 'active' \
         ORDER BY \
           CASE WHEN installed_at <= ?2 THEN 0 ELSE 1 END, \
           confidence_score DESC, \
           installed_at DESC \
         LIMIT 1",
    )
    .bind(trimmed)
    .bind(as_of_bind)
    .fetch_optional(pool)
    .await;
    if let Ok(Some(id)) = exact
        && !id.is_empty()
    {
        return Some(id);
    }

    // 2) Normalized prefix match, same ranking. Handles names like
    //    "headChar returns wrong byte" matching
    //    "headChar returns wrong byte (off-by-one)" — see the
    //    polish-surface scan finding #1 for the real-world sample.
    let prefix: Result<Option<String>, _> = sqlx::query_scalar(
        "SELECT id FROM skills \
         WHERE LOWER(name) LIKE LOWER(?1) || '%' AND status = 'active' \
         ORDER BY \
           CASE WHEN installed_at <= ?2 THEN 0 ELSE 1 END, \
           confidence_score DESC, \
           installed_at DESC \
         LIMIT 1",
    )
    .bind(trimmed)
    .bind(as_of_bind)
    .fetch_optional(pool)
    .await;
    if let Ok(Some(id)) = prefix
        && !id.is_empty()
    {
        return Some(id);
    }

    None
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct FixOutcomeSummary {
    pub applied: i64,
    pub failed: i64,
    pub rejected: i64,
}

/// Honest split of `accepted=1` rows into real wins vs accepted patches that
/// did not apply. Reporting raw `accepted=1` conflates these cases; user-facing
/// acceptance metrics should use `accepted_and_applied`.
///
/// The `COALESCE(applied_ok, 1) = 1` predicate treats NULL `applied_ok`
/// (older rows from before the column landed on the schema) as success.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::FromRow)]
pub struct FixOutcomeSplitSummary {
    pub accepted_and_applied: i64,
    pub accepted_but_failed: i64,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct FixOutcomeDaily {
    pub day: String,
    pub applied: i64,
    pub failed: i64,
    pub rejected: i64,
}

pub async fn record_many(pool: &SqlitePool, inputs: &[FixOutcomeInput<'_>]) -> crate::Result<()> {
    if inputs.is_empty() {
        return Ok(());
    }

    // Resolve missing `rule_id` values from `rule_name` so dashboard rollups
    // can join cleanly. If no skill matches, write the caller's value and never
    // fail the hot path.
    //
    // We resolve BEFORE opening the transaction so the resolver doesn't
    // contend with the tx for the single connection on pools sized at
    // `max_connections(1)` (the in-memory test pool, and a degenerate
    // production pool under heavy load). One query per input row is
    // fine: callers batch small lists, and the lookup is
    // index-friendly.
    let mut resolved_ids: Vec<Option<String>> = Vec::with_capacity(inputs.len());
    for input in inputs {
        let needs_resolve =
            input.rule_id.is_none_or(|s| s.trim().is_empty()) && !input.rule_name.trim().is_empty();
        if needs_resolve {
            // Write path: the row's `created_at` will be `datetime('now')`
            // at insert, so every currently-active skill satisfies
            // `installed_at <= now`. Passing `None` lets the resolver
            // fall through to confidence/recency ranking; there is no outcome
            // timestamp yet to disambiguate by install time.
            resolved_ids.push(resolve_rule_id_by_name(pool, input.rule_name, None).await);
        } else {
            resolved_ids.push(None);
        }
    }

    let mut tx = pool.begin().await?;
    for (input, resolved) in inputs.iter().zip(resolved_ids.iter()) {
        let id = Uuid::new_v4().to_string();
        let accepted = i64::from(input.accepted);
        let applied_ok = i64::from(input.applied_ok);

        let rule_id_to_bind: Option<&str> = match resolved.as_deref() {
            Some(id) => Some(id),
            None => input.rule_id,
        };

        sqlx::query(
            "INSERT INTO fix_outcomes
             (id, rule_id, rule_name, file_path, repo_full_name, pr_number, diff_signature,
              accepted, applied_ok, failed_reason)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        )
        .bind(id)
        .bind(rule_id_to_bind)
        .bind(input.rule_name)
        .bind(input.file_path)
        .bind(input.repo_full_name)
        .bind(input.pr_number)
        .bind(input.diff_signature)
        .bind(accepted)
        .bind(applied_ok)
        .bind(input.failed_reason)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn summary(pool: &SqlitePool, days: i64) -> crate::Result<FixOutcomeSummary> {
    let days = days.max(1);
    let window = format!("-{days} days");
    let row = sqlx::query_as!(
        FixOutcomeSummary,
        r#"SELECT
            COALESCE(SUM(CASE WHEN accepted = 1 AND applied_ok = 1 THEN 1 ELSE 0 END), 0) AS "applied!: i64",
            COALESCE(SUM(CASE WHEN accepted = 1 AND applied_ok = 0 THEN 1 ELSE 0 END), 0) AS "failed!: i64",
            COALESCE(SUM(CASE WHEN accepted = 0 THEN 1 ELSE 0 END), 0) AS "rejected!: i64"
         FROM fix_outcomes
         WHERE datetime(created_at) >= datetime('now', ?1)"#,
        window
    )
    .fetch_one(pool)
    .await?;
    Ok(row)
}

/// Honest split of `accepted=1` rows into real wins vs phantom accepts.
/// See [`FixOutcomeSplitSummary`] for why this exists. `days <= 0` is
/// clamped to 1 to match the other window-scoped readers in this file.
///
/// Implementation note: this is computed in one round trip alongside an
/// optional window filter so callers can pass the same `days` value
/// they already use for `summary` and get directly-comparable numbers.
pub async fn split_summary(pool: &SqlitePool, days: i64) -> crate::Result<FixOutcomeSplitSummary> {
    let days = days.max(1);
    let window = format!("-{days} days");
    // Runtime `query_as` keeps this optional rollup independent of the offline
    // SQLx cache.
    let row = sqlx::query_as::<_, FixOutcomeSplitSummary>(
        r"SELECT
            COALESCE(SUM(CASE WHEN accepted = 1 AND COALESCE(applied_ok, 1) = 1 THEN 1 ELSE 0 END), 0) AS accepted_and_applied,
            COALESCE(SUM(CASE WHEN accepted = 1 AND applied_ok = 0 THEN 1 ELSE 0 END), 0) AS accepted_but_failed
         FROM fix_outcomes
         WHERE datetime(created_at) >= datetime('now', ?1)",
    )
    .bind(window)
    .fetch_one(pool)
    .await?;
    Ok(row)
}

/// Top N most-common `failed_reason` values within the window. The
/// reason text is used verbatim — fix.rs already strips the `CoreError`
/// `Internal error: ` prefix before inserting, so what comes back is
/// the user-facing sentence.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct FixOutcomeReason {
    pub reason: String,
    pub count: i64,
}

pub async fn top_failure_reasons(
    pool: &SqlitePool,
    days: i64,
    limit: i64,
) -> crate::Result<Vec<FixOutcomeReason>> {
    let days = days.max(1);
    let limit = limit.max(1);
    let window = format!("-{days} days");
    let rows = sqlx::query_as!(
        FixOutcomeReason,
        r#"SELECT failed_reason AS "reason!: String", COUNT(*) AS "count!: i64"
         FROM fix_outcomes
         WHERE accepted = 1
           AND applied_ok = 0
           AND failed_reason IS NOT NULL
           AND TRIM(failed_reason) <> ''
           AND datetime(created_at) >= datetime('now', ?1)
         GROUP BY failed_reason
         ORDER BY COUNT(*) DESC, failed_reason ASC
         LIMIT ?2"#,
        window,
        limit
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn accepted_signature_count(pool: &SqlitePool, days: i64) -> crate::Result<i64> {
    let days = days.max(1);
    let window = format!("-{days} days");
    let count = sqlx::query_scalar!(
        r#"SELECT COUNT(DISTINCT diff_signature) AS "count!: i64"
         FROM fix_outcomes
         WHERE accepted = 1
           AND applied_ok = 1
           AND diff_signature IS NOT NULL
           AND TRIM(diff_signature) <> ''
           AND datetime(created_at) >= datetime('now', ?1)"#,
        window,
    )
    .fetch_one(pool)
    .await?;
    Ok(count)
}

pub async fn daily(pool: &SqlitePool, days: i64) -> crate::Result<Vec<FixOutcomeDaily>> {
    let days = days.max(1);
    let window = format!("-{days} days");
    let rows = sqlx::query_as!(
        FixOutcomeDaily,
        r#"SELECT
            date(created_at) AS "day!: String",
            COALESCE(SUM(CASE WHEN accepted = 1 AND applied_ok = 1 THEN 1 ELSE 0 END), 0) AS "applied!: i64",
            COALESCE(SUM(CASE WHEN accepted = 1 AND applied_ok = 0 THEN 1 ELSE 0 END), 0) AS "failed!: i64",
            COALESCE(SUM(CASE WHEN accepted = 0 THEN 1 ELSE 0 END), 0) AS "rejected!: i64"
         FROM fix_outcomes
         WHERE datetime(created_at) >= datetime('now', ?1)
         GROUP BY date(created_at)
         ORDER BY date(created_at) ASC"#,
        window
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn pool_with_fix_outcomes() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE fix_outcomes (
                id TEXT PRIMARY KEY NOT NULL,
                rule_id TEXT,
                rule_name TEXT NOT NULL,
                file_path TEXT,
                repo_full_name TEXT,
                pr_number INTEGER,
                diff_signature TEXT,
                accepted INTEGER NOT NULL,
                applied_ok INTEGER NOT NULL DEFAULT 0,
                failed_reason TEXT,
                created_at TEXT DEFAULT (datetime('now')) NOT NULL
            )",
        )
        .execute(&pool)
        .await
        .unwrap();
        // Minimal `skills` table for the rule_id resolver tests. The
        // resolver reads `id`, `name`, `status`, `confidence_score`,
        // and `installed_at`; `updated_at` is kept for recency test seeds.
        sqlx::query(
            "CREATE TABLE skills (
                id TEXT PRIMARY KEY NOT NULL,
                name TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'active',
                confidence_score REAL NOT NULL DEFAULT 0.7,
                installed_at TEXT DEFAULT (datetime('now')) NOT NULL,
                updated_at TEXT DEFAULT (datetime('now')) NOT NULL
            )",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    async fn seed_skill(pool: &SqlitePool, id: &str, name: &str, updated_at: &str) {
        // Mirror `installed_at` from `updated_at` so recency tests use one
        // ranking signal.
        sqlx::query(
            "INSERT INTO skills (id, name, installed_at, updated_at) \
             VALUES (?1, ?2, ?3, ?3)",
        )
        .bind(id)
        .bind(name)
        .bind(updated_at)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn seed_skill_full(
        pool: &SqlitePool,
        id: &str,
        name: &str,
        status: &str,
        confidence: f64,
        installed_at: &str,
    ) {
        sqlx::query(
            "INSERT INTO skills (id, name, status, confidence_score, installed_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
        )
        .bind(id)
        .bind(name)
        .bind(status)
        .bind(confidence)
        .bind(installed_at)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn record_many_persists_local_diff_signature() {
        let pool = pool_with_fix_outcomes().await;
        record_many(
            &pool,
            &[FixOutcomeInput {
                rule_id: Some("rule-1"),
                rule_name: "Rule 1",
                file_path: Some("src/lib.rs"),
                repo_full_name: Some("acme/widgets"),
                pr_number: Some(42),
                diff_signature: Some("abc123"),
                accepted: true,
                applied_ok: true,
                failed_reason: None,
            }],
        )
        .await
        .unwrap();

        let stored: Option<String> =
            sqlx::query_scalar!("SELECT diff_signature FROM fix_outcomes WHERE rule_id = 'rule-1'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(stored.as_deref(), Some("abc123"));

        let target: (Option<String>, Option<i64>) =
            sqlx::query_as("SELECT repo_full_name, pr_number FROM fix_outcomes WHERE rule_id = ?1")
                .bind("rule-1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(target.0.as_deref(), Some("acme/widgets"));
        assert_eq!(target.1, Some(42));
    }

    #[tokio::test]
    async fn accepted_signature_count_counts_unique_applied_proof_hashes() {
        let pool = pool_with_fix_outcomes().await;
        record_many(
            &pool,
            &[
                FixOutcomeInput {
                    rule_id: Some("rule-1"),
                    rule_name: "Rule 1",
                    file_path: Some("src/lib.rs"),
                    repo_full_name: None,
                    pr_number: None,
                    diff_signature: Some("same"),
                    accepted: true,
                    applied_ok: true,
                    failed_reason: None,
                },
                FixOutcomeInput {
                    rule_id: Some("rule-2"),
                    rule_name: "Rule 2",
                    file_path: Some("src/lib.rs"),
                    repo_full_name: None,
                    pr_number: None,
                    diff_signature: Some("same"),
                    accepted: true,
                    applied_ok: true,
                    failed_reason: None,
                },
                FixOutcomeInput {
                    rule_id: Some("rule-3"),
                    rule_name: "Rule 3",
                    file_path: Some("src/other.rs"),
                    repo_full_name: None,
                    pr_number: None,
                    diff_signature: Some("failed"),
                    accepted: true,
                    applied_ok: false,
                    failed_reason: Some("no patch"),
                },
            ],
        )
        .await
        .unwrap();

        assert_eq!(accepted_signature_count(&pool, 30).await.unwrap(), 1);
    }

    // The insert path must resolve `rule_id` from `rule_name` when the caller
    // did not have one. Exact match wins over the prefix-match fallback.
    #[tokio::test]
    async fn record_many_back_fills_rule_id_via_exact_name() {
        let pool = pool_with_fix_outcomes().await;
        seed_skill(&pool, "skill-foo", "foo", "2026-05-20").await;
        seed_skill(&pool, "skill-foo-variant", "foo (variant)", "2026-05-26").await;

        record_many(
            &pool,
            &[FixOutcomeInput {
                rule_id: Some(""),
                rule_name: "foo",
                file_path: None,
                repo_full_name: None,
                pr_number: None,
                diff_signature: None,
                accepted: true,
                applied_ok: true,
                failed_reason: None,
            }],
        )
        .await
        .unwrap();

        let stored: Option<String> =
            sqlx::query_scalar("SELECT rule_id FROM fix_outcomes WHERE rule_name = 'foo'")
                .fetch_one(&pool)
                .await
                .unwrap();
        // Exact-name match must beat the more-recently-updated "foo (variant)".
        assert_eq!(stored.as_deref(), Some("skill-foo"));
    }

    // Graceful-degrade: an unmatched rule_name must NOT fail the insert
    // and must leave `rule_id` empty (the caller-supplied value).
    #[tokio::test]
    async fn record_many_leaves_rule_id_empty_when_no_match() {
        let pool = pool_with_fix_outcomes().await;
        seed_skill(&pool, "skill-foo", "foo", "2026-05-20").await;

        record_many(
            &pool,
            &[FixOutcomeInput {
                rule_id: None,
                rule_name: "bar baz",
                file_path: None,
                repo_full_name: None,
                pr_number: None,
                diff_signature: None,
                accepted: true,
                applied_ok: true,
                failed_reason: None,
            }],
        )
        .await
        .unwrap();

        let stored: Option<String> =
            sqlx::query_scalar("SELECT rule_id FROM fix_outcomes WHERE rule_name = 'bar baz'")
                .fetch_one(&pool)
                .await
                .unwrap();
        // No skill matched → caller's value (None) stored as-is.
        assert!(stored.is_none(), "expected NULL rule_id, got {stored:?}");
    }

    // Prefix fallback: when no exact match exists, use the case-insensitive
    // prefix lookup and tiebreak on recency.
    #[tokio::test]
    async fn resolve_rule_id_by_name_prefix_match_picks_latest() {
        let pool = pool_with_fix_outcomes().await;
        seed_skill(
            &pool,
            "skill-old",
            "headChar returns wrong byte",
            "2026-04-01",
        )
        .await;
        seed_skill(
            &pool,
            "skill-new",
            "headChar returns wrong byte (off-by-one)",
            "2026-05-26",
        )
        .await;

        // Exact match still wins when present.
        let exact = resolve_rule_id_by_name(&pool, "headChar returns wrong byte", None).await;
        assert_eq!(exact.as_deref(), Some("skill-old"));

        // With only the verbose name in the rules table, the prefix
        // path picks the variant up.
        sqlx::query("DELETE FROM skills WHERE id = 'skill-old'")
            .execute(&pool)
            .await
            .unwrap();
        let prefix = resolve_rule_id_by_name(&pool, "headChar returns wrong", None).await;
        assert_eq!(prefix.as_deref(), Some("skill-new"));
    }

    #[tokio::test]
    async fn resolve_rule_id_by_name_empty_input_returns_none() {
        let pool = pool_with_fix_outcomes().await;
        assert!(resolve_rule_id_by_name(&pool, "", None).await.is_none());
        assert!(resolve_rule_id_by_name(&pool, "   ", None).await.is_none());
    }

    // The resolver must deterministically prefer (1) active skills installed at
    // or before the outcome timestamp, (2) higher confidence, (3) most recent
    // install. Seed three colliding "duplicate-rule" skills:
    //   - skill-after: installed AFTER the outcome (should be skipped)
    //   - skill-before-lo: installed BEFORE, low confidence
    //   - skill-before-hi: installed BEFORE, high confidence (winner)
    #[tokio::test]
    async fn resolve_rule_id_by_name_disambiguates_collisions_via_ranking() {
        let pool = pool_with_fix_outcomes().await;

        seed_skill_full(
            &pool,
            "skill-after",
            "duplicate-rule",
            "active",
            0.95,
            "2026-05-15",
        )
        .await;
        seed_skill_full(
            &pool,
            "skill-before-lo",
            "duplicate-rule",
            "active",
            0.40,
            "2026-04-01",
        )
        .await;
        seed_skill_full(
            &pool,
            "skill-before-hi",
            "duplicate-rule",
            "active",
            0.80,
            "2026-04-15",
        )
        .await;

        // Outcome happened 2026-05-01 — skill-after didn't exist yet.
        let resolved = resolve_rule_id_by_name(&pool, "duplicate-rule", Some("2026-05-01")).await;
        assert_eq!(
            resolved.as_deref(),
            Some("skill-before-hi"),
            "ranking must pick the highest-confidence skill installed before the outcome",
        );
    }

    // Pending/inactive skills must not be returned, even if they're the
    // only exact-name match — the resolver filters by `status='active'`.
    #[tokio::test]
    async fn resolve_rule_id_by_name_skips_non_active_skills() {
        let pool = pool_with_fix_outcomes().await;
        seed_skill_full(
            &pool,
            "skill-pending",
            "pending-only",
            "pending",
            0.99,
            "2026-04-01",
        )
        .await;
        assert!(
            resolve_rule_id_by_name(&pool, "pending-only", Some("2026-05-01"))
                .await
                .is_none()
        );

        // Once an active sibling exists, it wins.
        seed_skill_full(
            &pool,
            "skill-active",
            "pending-only",
            "active",
            0.20,
            "2026-04-02",
        )
        .await;
        let resolved = resolve_rule_id_by_name(&pool, "pending-only", Some("2026-05-01")).await;
        assert_eq!(resolved.as_deref(), Some("skill-active"));
    }

    // Same ranking applies to the prefix-fallback path: among multiple
    // active prefix-matches, the resolver must prefer
    // installed-before-as_of + higher confidence.
    #[tokio::test]
    async fn resolve_rule_id_by_name_prefix_match_disambiguates_collisions() {
        let pool = pool_with_fix_outcomes().await;

        // Three skills with the same prefix; the lookup will be
        // "prefix-rule".
        seed_skill_full(
            &pool,
            "skill-after",
            "prefix-rule (recent variant)",
            "active",
            0.99,
            "2026-05-15",
        )
        .await;
        seed_skill_full(
            &pool,
            "skill-before-lo",
            "prefix-rule (low conf)",
            "active",
            0.30,
            "2026-04-01",
        )
        .await;
        seed_skill_full(
            &pool,
            "skill-before-hi",
            "prefix-rule (high conf)",
            "active",
            0.85,
            "2026-04-10",
        )
        .await;

        let resolved = resolve_rule_id_by_name(&pool, "prefix-rule", Some("2026-05-01")).await;
        assert_eq!(
            resolved.as_deref(),
            Some("skill-before-hi"),
            "prefix fallback must apply the same ranking as the exact match",
        );
    }

    // split_summary must distinguish real applies from phantom accepts. Seed 3
    // real wins (acc=1, ok=1), 2 phantom failures (acc=1, ok=0), and 1
    // rejected (acc=0). Expected:
    //   accepted_and_applied = 3
    //   accepted_but_failed  = 2
    #[tokio::test]
    async fn split_summary_distinguishes_real_applies_from_phantom_accepts() {
        let pool = pool_with_fix_outcomes().await;
        sqlx::query(
            "INSERT INTO fix_outcomes (id, rule_name, accepted, applied_ok) VALUES
             ('win-1', 'r', 1, 1),
             ('win-2', 'r', 1, 1),
             ('win-3', 'r', 1, 1),
             ('phantom-1', 'r', 1, 0),
             ('phantom-2', 'r', 1, 0),
             ('rejected', 'r', 0, 0)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let split = split_summary(&pool, 30).await.expect("split summary");
        assert_eq!(
            split.accepted_and_applied, 3,
            "only acc=1 AND applied_ok=1 should count as a real win"
        );
        assert_eq!(
            split.accepted_but_failed, 2,
            "only acc=1 AND applied_ok=0 (explicit failure) should count as phantom"
        );
    }

    // The COALESCE(applied_ok, 1) = 1 branch is purely defensive: the
    // production schema has NOT NULL on `applied_ok`, so this can only fire on
    // a hand-rolled local DB. Cover it with a separate pool that omits the NOT
    // NULL constraint.
    #[tokio::test]
    async fn split_summary_treats_null_applied_ok_as_applied_for_legacy_rows() {
        use sqlx::sqlite::SqlitePoolOptions;
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        // Permissive schema: `applied_ok` nullable.
        sqlx::query(
            "CREATE TABLE fix_outcomes (
                id TEXT PRIMARY KEY NOT NULL,
                rule_id TEXT,
                rule_name TEXT NOT NULL,
                file_path TEXT,
                repo_full_name TEXT,
                pr_number INTEGER,
                diff_signature TEXT,
                accepted INTEGER NOT NULL,
                applied_ok INTEGER,
                failed_reason TEXT,
                created_at TEXT DEFAULT (datetime('now')) NOT NULL
            )",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO fix_outcomes (id, rule_name, accepted, applied_ok) VALUES
             ('legacy-1', 'r', 1, NULL),
             ('legacy-2', 'r', 1, NULL),
             ('phantom', 'r', 1, 0)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let split = split_summary(&pool, 30).await.expect("split summary");
        assert_eq!(
            split.accepted_and_applied, 2,
            "legacy rows with NULL applied_ok must be charitably counted as applied",
        );
        assert_eq!(split.accepted_but_failed, 1);
    }
}
