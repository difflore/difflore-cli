//! Local review-gate finding ledger.
//!
//! Records low-sensitivity proof that a source-backed rule caught a review
//! finding in `difflore review --diff` or the CI gate. The raw finding text and
//! source diff are not stored; only a stable hash is persisted.

use sha2::{Digest, Sha256};
use sqlx::{Row, SqlitePool};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewGateSource {
    Review,
    Ci,
}

impl ReviewGateSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Review => "review",
            Self::Ci => "ci",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewGateEventInput {
    pub rule_id: String,
    pub file_path: String,
    pub finding_hash: String,
    pub source: ReviewGateSource,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReviewCommentsAvoidedSummary {
    pub last30: i64,
    pub total: i64,
}

/// Hash the rule id, normalized file path, optional line, and normalized
/// finding text. The input deliberately excludes run-specific metadata such as
/// trace ids, timestamps, confidence, and provider names, so re-running the
/// same unfixed diff produces the same hash while distinct emitted findings in
/// the same file can still separate by line/text.
pub fn finding_hash(
    rule_id: &str,
    file_path: &str,
    line: Option<i32>,
    finding_text: &str,
) -> String {
    use std::fmt::Write as _;

    let mut hasher = Sha256::new();
    hasher.update(b"review-gate-finding-v1\0");
    hasher.update(normalize_hash_part(rule_id).as_bytes());
    hasher.update(b"\0");
    hasher.update(normalize_path(file_path).as_bytes());
    hasher.update(b"\0");
    if let Some(line) = line {
        hasher.update(line.to_string().as_bytes());
    }
    hasher.update(b"\0");
    hasher.update(normalize_hash_part(finding_text).as_bytes());

    hasher
        .finalize()
        .iter()
        .fold(String::with_capacity(64), |mut acc, byte| {
            let _ = write!(acc, "{byte:02x}");
            acc
        })
}

fn normalize_hash_part(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn normalize_path(value: &str) -> String {
    normalize_hash_part(&value.replace('\\', "/"))
}

pub async fn record_many(
    pool: &SqlitePool,
    inputs: &[ReviewGateEventInput],
) -> crate::Result<usize> {
    if inputs.is_empty() {
        return Ok(0);
    }

    let mut tx = pool.begin().await?;
    let mut inserted = 0usize;
    for input in inputs {
        let rule_id = input.rule_id.trim();
        let file_path = input.file_path.trim();
        let finding_hash = input.finding_hash.trim();
        if rule_id.is_empty() || file_path.is_empty() || finding_hash.is_empty() {
            continue;
        }
        sqlx::query(
            "INSERT INTO review_gate_events
             (rule_id, file_path, finding_hash, source)
             VALUES (?1, ?2, ?3, ?4)",
        )
        .bind(rule_id)
        .bind(file_path)
        .bind(finding_hash)
        .bind(input.source.as_str())
        .execute(&mut *tx)
        .await?;
        inserted += 1;
    }
    tx.commit().await?;
    Ok(inserted)
}

pub async fn review_comments_avoided_summary(
    pool: &SqlitePool,
    days: i64,
) -> crate::Result<ReviewCommentsAvoidedSummary> {
    let last30 = distinct_count(pool, Some(days.max(1))).await?;
    let total = distinct_count(pool, None).await?;
    Ok(ReviewCommentsAvoidedSummary { last30, total })
}

async fn distinct_count(pool: &SqlitePool, days: Option<i64>) -> crate::Result<i64> {
    let mut query = String::from(
        "SELECT COUNT(*) AS n FROM (
             SELECT DISTINCT rule_id, file_path, finding_hash
             FROM review_gate_events",
    );
    if days.is_some() {
        query.push_str(" WHERE datetime(created_at) >= datetime('now', ?1)");
    }
    query.push(')');

    let mut q = sqlx::query(&query);
    let window;
    if let Some(days) = days {
        window = format!("-{days} days");
        q = q.bind(window.as_str());
    }
    let row = q.fetch_one(pool).await?;
    Ok(row.try_get::<i64, _>("n").unwrap_or(0))
}

#[cfg(test)]
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

    fn event(rule_id: &str, file_path: &str, finding_text: &str) -> ReviewGateEventInput {
        ReviewGateEventInput {
            rule_id: rule_id.to_owned(),
            file_path: file_path.to_owned(),
            finding_hash: finding_hash(rule_id, file_path, Some(12), finding_text),
            source: ReviewGateSource::Review,
        }
    }

    #[test]
    fn finding_hash_is_stable_and_hides_finding_text() {
        let a = finding_hash(
            "rule-1",
            "src/auth.ts",
            Some(12),
            "Use a constant-time token check.\n\nAvoid timing leaks.",
        );
        let b = finding_hash(
            " rule-1 ",
            "src\\auth.ts",
            Some(12),
            "use a constant-time token check. Avoid timing leaks.",
        );

        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(!a.contains("token"));
    }

    #[tokio::test]
    async fn duplicate_finding_counts_once_inside_window() {
        let pool = setup().await;
        let finding = event("rule-1", "src/auth.ts", "Use a constant-time check.");

        record_many(&pool, &[finding.clone(), finding])
            .await
            .expect("record duplicate findings");

        let summary = review_comments_avoided_summary(&pool, 30)
            .await
            .expect("summary");
        assert_eq!(summary.last30, 1);
        assert_eq!(summary.total, 1);
    }

    #[tokio::test]
    async fn old_events_are_excluded_from_window_but_counted_total() {
        let pool = setup().await;
        let old = event("rule-1", "src/auth.ts", "Use a constant-time check.");
        let recent = event("rule-1", "src/auth.ts", "Validate missing token.");
        let old_hash = old.finding_hash.clone();

        record_many(&pool, &[old, recent])
            .await
            .expect("record findings");
        sqlx::query(
            "UPDATE review_gate_events
             SET created_at = datetime('now', '-45 days')
             WHERE finding_hash = ?1",
        )
        .bind(old_hash)
        .execute(&pool)
        .await
        .expect("age one finding");

        let summary = review_comments_avoided_summary(&pool, 30)
            .await
            .expect("summary");
        assert_eq!(summary.last30, 1);
        assert_eq!(summary.total, 2);
    }
}
