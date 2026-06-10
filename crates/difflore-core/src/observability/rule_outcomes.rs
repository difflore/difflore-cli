//! Local-only rule outcome telemetry.
//!
//! Records when a rule is surfaced by recall (`kind = 'recalled'`) so
//! `difflore memory` and `rules show` can report which rules pull weight.
//! Fix-acceptance telemetry lives separately in `fix_outcomes`; both are read
//! together by the surfaces.
//!
//! Data never leaves the device.

use sqlx::SqlitePool;

pub const KIND_RECALLED: &str = "recalled";

#[derive(Debug, Clone)]
pub struct RuleRecallInput<'a> {
    pub rule_id: &'a str,
    pub session_id: Option<&'a str>,
    pub repo_full_name: Option<&'a str>,
    pub file_path: Option<&'a str>,
    pub query_text: &'a str,
    pub rank: i64,
    pub top_k: i64,
    pub strict_file_match: bool,
}

/// Insert one row per recalled rule. No-op when `rule_ids` is empty.
pub async fn record_recalled(pool: &SqlitePool, rule_ids: &[String]) -> crate::Result<()> {
    if rule_ids.is_empty() {
        return Ok(());
    }
    let mut tx = pool.begin().await?;
    for id in rule_ids {
        sqlx::query!(
            "INSERT INTO rule_outcomes (rule_id, kind) VALUES (?1, ?2)",
            id,
            KIND_RECALLED
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Insert recall rows with low-sensitivity context.
///
/// Stores hashes and scope, never prompt text or source code. `rank` matters:
/// a `rank <= 3` recall means the rule was one an agent would actually see, not
/// merely present somewhere in the corpus.
pub async fn record_recalled_with_context(
    pool: &SqlitePool,
    recalls: &[RuleRecallInput<'_>],
) -> crate::Result<()> {
    if recalls.is_empty() {
        return Ok(());
    }
    let mut tx = pool.begin().await?;
    for recall in recalls {
        let query_hash = crate::mcp_rule_serves::query_hash(recall.query_text);
        let rank = recall.rank.max(1);
        let top_k = recall.top_k.max(1);
        let strict = i64::from(recall.strict_file_match);
        sqlx::query!(
            "INSERT INTO rule_outcomes
             (rule_id, kind, session_id, repo_full_name, file_path, query_hash,
              rank, top_k, strict_file_match)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            recall.rule_id,
            KIND_RECALLED,
            recall.session_id,
            recall.repo_full_name,
            recall.file_path,
            query_hash,
            rank,
            top_k,
            strict,
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct RecallCount {
    pub rule_id: String,
    pub count: i64,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct RecallSummary {
    pub recall_events: i64,
    pub recalled_rules: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct AcceptedFixEvidence {
    pub file_path: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct TopRecallEvidence {
    pub rule_id: String,
    pub repo_full_name: Option<String>,
    pub file_path: Option<String>,
    pub rank: i64,
    pub top_k: i64,
    pub strict_file_match: bool,
    pub recalled_at: String,
}

/// Total local recall proof over the last `days` days.
pub async fn summary(pool: &SqlitePool, days: i64) -> crate::Result<RecallSummary> {
    let days = days.max(1);
    let window = format!("-{days} days");
    let row = sqlx::query_as!(
        RecallSummary,
        r#"SELECT
             COUNT(*) AS "recall_events!: i64",
             COUNT(DISTINCT rule_id) AS "recalled_rules!: i64"
         FROM rule_outcomes
         WHERE kind = 'recalled'
           AND datetime(created_at) >= datetime('now', ?1)"#,
        window,
    )
    .fetch_one(pool)
    .await?;
    Ok(row)
}

/// Top-N rules by recall events within the last `days` days.
pub async fn top_recalled(
    pool: &SqlitePool,
    days: i64,
    limit: i64,
) -> crate::Result<Vec<RecallCount>> {
    let days = days.max(1);
    let limit = limit.max(1);
    let window = format!("-{days} days");
    // INNER JOIN `skills` so deleted rules don't surface as zombie rows
    // rendered as a bare rule_id.
    let rows = sqlx::query_as!(
        RecallCount,
        r#"SELECT o.rule_id AS "rule_id!: String", COUNT(*) AS "count!: i64"
         FROM rule_outcomes o
         INNER JOIN skills s ON s.id = o.rule_id
         WHERE o.kind = 'recalled'
           AND datetime(o.created_at) >= datetime('now', ?1)
         GROUP BY o.rule_id
         ORDER BY COUNT(*) DESC, o.rule_id ASC
         LIMIT ?2"#,
        window,
        limit
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Total recall count for a single rule over the last `days` days.
pub async fn recall_count_for(pool: &SqlitePool, rule_id: &str, days: i64) -> crate::Result<i64> {
    let days = days.max(1);
    let window = format!("-{days} days");
    let n: i64 = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "n!: i64" FROM rule_outcomes
         WHERE kind = 'recalled' AND rule_id = ?1
           AND datetime(created_at) >= datetime('now', ?2)"#,
        rule_id,
        window
    )
    .fetch_one(pool)
    .await?;
    Ok(n)
}

pub async fn latest_top3_recall_for(
    pool: &SqlitePool,
    rule_id: &str,
    days: i64,
) -> crate::Result<Option<TopRecallEvidence>> {
    let days = days.max(1);
    let window = format!("-{days} days");
    let row = sqlx::query_as!(
        TopRecallEvidence,
        r#"SELECT rule_id AS "rule_id!: String",
                repo_full_name,
                file_path,
                COALESCE(rank, 999) AS "rank!: i64",
                COALESCE(top_k, 0) AS "top_k!: i64",
                strict_file_match != 0 AS "strict_file_match!: bool",
                created_at AS "recalled_at!: String"
         FROM rule_outcomes
         WHERE kind = 'recalled'
           AND rule_id = ?1
           AND rank BETWEEN 1 AND 3
           AND datetime(created_at) >= datetime('now', ?2)
         ORDER BY datetime(created_at) DESC, id DESC
         LIMIT 1"#,
        rule_id,
        window,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// How many `fix_outcomes` rows for this rule were `accepted = 1 AND
/// applied_ok = 1` within the window. Read here so the memory/show
/// surfaces have a single import path for "rule outcome" reads.
pub async fn fix_accepted_count_for(
    pool: &SqlitePool,
    rule_id: &str,
    days: i64,
) -> crate::Result<i64> {
    let days = days.max(1);
    let window = format!("-{days} days");
    let n: i64 = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "n!: i64" FROM fix_outcomes
         WHERE rule_id = ?1 AND accepted = 1 AND applied_ok = 1
           AND datetime(created_at) >= datetime('now', ?2)"#,
        rule_id,
        window
    )
    .fetch_one(pool)
    .await?;
    Ok(n)
}

pub async fn latest_accepted_fix_for(
    pool: &SqlitePool,
    rule_id: &str,
    days: i64,
) -> crate::Result<Option<AcceptedFixEvidence>> {
    let days = days.max(1);
    let window = format!("-{days} days");
    let row = sqlx::query_as!(
        AcceptedFixEvidence,
        r#"SELECT file_path, created_at AS "created_at!: String"
         FROM fix_outcomes
         WHERE rule_id = ?1
           AND accepted = 1
           AND applied_ok = 1
           AND datetime(created_at) >= datetime('now', ?2)
         ORDER BY datetime(created_at) DESC, id DESC
         LIMIT 1"#,
        rule_id,
        window,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

#[cfg(test)]
#[allow(clippy::str_to_string)] // reason: test code — failure should panic with context.
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn setup() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("open pool");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("apply migrations");
        pool
    }

    async fn insert_skill(pool: &SqlitePool, id: &str, name: &str) {
        sqlx::query!(
            "INSERT INTO skills (id, name, source, directory, version)
             VALUES (?1, ?2, 'manual', '/tmp', '1.0.0')",
            id,
            name,
        )
        .execute(pool)
        .await
        .expect("insert skill");
    }

    /// Locks in the fix for the `difflore memory` zombie-rule bug:
    /// recall events whose owning rule has been deleted from `skills`
    /// must not surface in the Most-recalled list.
    #[tokio::test]
    async fn top_recalled_excludes_deleted_rules() {
        let pool = setup().await;
        insert_skill(&pool, "r1", "Real rule").await;
        insert_skill(&pool, "r2", "Soon-deleted rule").await;

        record_recalled(&pool, &["r1".to_owned()])
            .await
            .expect("record r1");
        record_recalled(&pool, &["r2".to_owned(), "r2".to_owned()])
            .await
            .expect("record r2");

        // Drop the rule but keep its outcome rows (idempotent design).
        sqlx::query!("DELETE FROM skills WHERE id = 'r2'")
            .execute(&pool)
            .await
            .expect("delete r2");

        let rows = top_recalled(&pool, 7, 10).await.expect("top_recalled");
        let ids: Vec<&str> = rows.iter().map(|r| r.rule_id.as_str()).collect();
        assert!(ids.contains(&"r1"), "real rule should appear: {ids:?}");
        assert!(
            !ids.contains(&"r2"),
            "deleted rule must not appear: {ids:?}"
        );
    }

    #[tokio::test]
    async fn summary_counts_local_recall_events_and_distinct_rules() {
        let pool = setup().await;
        record_recalled(&pool, &["r1".to_owned(), "r2".to_owned()])
            .await
            .expect("record first recall");
        record_recalled(&pool, &["r2".to_owned()])
            .await
            .expect("record second recall");

        let row = summary(&pool, 30).await.expect("summary");
        assert_eq!(row.recall_events, 3);
        assert_eq!(row.recalled_rules, 2);
    }

    #[tokio::test]
    async fn latest_top3_recall_for_requires_ranked_recall_context() {
        let pool = setup().await;
        record_recalled_with_context(
            &pool,
            &[
                RuleRecallInput {
                    rule_id: "r1",
                    session_id: Some("session-1"),
                    repo_full_name: Some("acme/widgets"),
                    file_path: Some("src/auth.rs"),
                    query_text: "src/auth.rs validate auth token",
                    rank: 4,
                    top_k: 5,
                    strict_file_match: true,
                },
                RuleRecallInput {
                    rule_id: "r1",
                    session_id: Some("session-1"),
                    repo_full_name: Some("acme/widgets"),
                    file_path: Some("src/auth.rs"),
                    query_text: "src/auth.rs validate auth token",
                    rank: 2,
                    top_k: 5,
                    strict_file_match: true,
                },
            ],
        )
        .await
        .expect("record ranked recall");

        let recall = latest_top3_recall_for(&pool, "r1", 30)
            .await
            .expect("latest recall")
            .expect("top3 recall");

        assert_eq!(recall.rank, 2);
        assert_eq!(recall.top_k, 5);
        assert_eq!(recall.repo_full_name.as_deref(), Some("acme/widgets"));
        assert_eq!(recall.file_path.as_deref(), Some("src/auth.rs"));
        assert!(recall.strict_file_match);
    }

    #[tokio::test]
    async fn latest_accepted_fix_for_returns_newest_applied_fix() {
        let pool = setup().await;
        insert_skill(&pool, "r1", "Real rule").await;
        sqlx::query!(
            "INSERT INTO fix_outcomes
             (id, rule_id, rule_name, file_path, accepted, applied_ok, created_at)
             VALUES
             ('f-old', 'r1', 'Real rule', 'src/old.rs', 1, 1, '2026-05-01 00:00:00'),
             ('f-new', 'r1', 'Real rule', 'src/new.rs', 1, 1, datetime('now')),
             ('f-rejected', 'r1', 'Real rule', 'src/rejected.rs', 0, 0, datetime('now'))",
        )
        .execute(&pool)
        .await
        .expect("insert fix outcomes");

        let latest = latest_accepted_fix_for(&pool, "r1", 30)
            .await
            .expect("latest accepted fix")
            .expect("some accepted fix");
        assert_eq!(latest.file_path.as_deref(), Some("src/new.rs"));
    }
}
