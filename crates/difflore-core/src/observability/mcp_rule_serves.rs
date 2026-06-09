//! Local MCP rule-serve ledger.
//!
//! This records the small, low-sensitive facts needed to prove the open
//! source runtime is doing useful work: which MCP rule tool served rules,
//! whether it came up empty, the repo/file scope, and a hash of the query.
//! It deliberately does not store the prompt, source code, or rule bodies.

use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

#[derive(Debug, Clone)]
pub struct McpRuleServeInput<'a> {
    pub tool: &'a str,
    pub session_id: Option<&'a str>,
    pub repo_full_name: Option<&'a str>,
    pub file_path: Option<&'a str>,
    pub query_text: &'a str,
    pub rule_ids: &'a [String],
    pub top_k: i64,
    pub strict_match_count: i64,
    pub estimated_tokens: i64,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct McpRuleServeSummary {
    pub calls: i64,
    pub empty_calls: i64,
    pub rules_served: i64,
    pub strict_matches: i64,
    pub estimated_tokens: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct McpRuleServeRuleSummary {
    pub calls: i64,
    pub strict_match_calls: i64,
    pub estimated_tokens: i64,
    pub latest: Option<McpRuleServeRuleEvidence>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpRuleServeRuleEvidence {
    pub tool: String,
    pub repo_full_name: Option<String>,
    pub file_path: Option<String>,
    pub served_at: String,
    pub strict_scoped: bool,
    pub estimated_tokens: i64,
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct McpRuleServeRuleRow {
    tool: String,
    repo_full_name: Option<String>,
    file_path: Option<String>,
    rule_ids_json: String,
    strict_match_count: i64,
    estimated_tokens: i64,
    served_at: String,
}

pub fn query_hash(query_text: &str) -> String {
    use std::fmt::Write as _;
    let mut hasher = Sha256::new();
    hasher.update(query_text.as_bytes());
    hasher
        .finalize()
        .iter()
        .fold(String::with_capacity(64), |mut acc, byte| {
            let _ = write!(acc, "{byte:02x}");
            acc
        })
}

pub async fn record(pool: &SqlitePool, input: &McpRuleServeInput<'_>) -> crate::Result<()> {
    let rule_ids_json = serde_json::to_string(input.rule_ids).unwrap_or_else(|_| "[]".to_owned());
    let rule_count = input.rule_ids.len() as i64;
    let was_empty = i64::from(rule_count == 0);
    let hash = query_hash(input.query_text);

    let top_k = input.top_k.max(0);
    let strict_match_count = input.strict_match_count.max(0);
    let estimated_tokens = input.estimated_tokens.max(0);
    sqlx::query!(
        "INSERT INTO mcp_rule_serves
         (tool, session_id, repo_full_name, file_path, query_hash, rule_ids_json,
          rule_count, top_k, was_empty, strict_match_count, estimated_tokens)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        input.tool,
        input.session_id,
        input.repo_full_name,
        input.file_path,
        hash,
        rule_ids_json,
        rule_count,
        top_k,
        was_empty,
        strict_match_count,
        estimated_tokens,
    )
    .execute(pool)
    .await?;

    Ok(())
}

pub async fn summary(pool: &SqlitePool, days: i64) -> crate::Result<McpRuleServeSummary> {
    let days = days.max(1);
    let window = format!("-{days} days");
    let row = sqlx::query_as!(
        McpRuleServeSummary,
        r#"SELECT
            COUNT(*) AS "calls!: i64",
            COALESCE(SUM(was_empty), 0) AS "empty_calls!: i64",
            COALESCE(SUM(rule_count), 0) AS "rules_served!: i64",
            COALESCE(SUM(strict_match_count), 0) AS "strict_matches!: i64",
            COALESCE(SUM(estimated_tokens), 0) AS "estimated_tokens!: i64"
         FROM mcp_rule_serves
         WHERE datetime(served_at) >= datetime('now', ?1)"#,
        window,
    )
    .fetch_one(pool)
    .await?;
    Ok(row)
}

pub async fn summary_for_repos(
    pool: &SqlitePool,
    repo_full_names: &[String],
    days: i64,
) -> crate::Result<McpRuleServeSummary> {
    let repos: Vec<String> = repo_full_names
        .iter()
        .map(|repo| repo.trim().to_ascii_lowercase())
        .filter(|repo| !repo.is_empty())
        .collect();
    if repos.is_empty() {
        return summary(pool, days).await;
    }

    let days = days.max(1);
    let window = format!("-{days} days");
    let repos_json = serde_json::to_string(&repos).unwrap_or_else(|_| "[]".to_owned());
    let row = sqlx::query_as::<_, McpRuleServeSummary>(
        r"SELECT
            COUNT(*) AS calls,
            COALESCE(SUM(was_empty), 0) AS empty_calls,
            COALESCE(SUM(rule_count), 0) AS rules_served,
            COALESCE(SUM(strict_match_count), 0) AS strict_matches,
            COALESCE(SUM(estimated_tokens), 0) AS estimated_tokens
         FROM mcp_rule_serves
         WHERE datetime(served_at) >= datetime('now', ?1)
           AND repo_full_name IS NOT NULL
           AND lower(repo_full_name) IN (SELECT value FROM json_each(?2))",
    )
    .bind(window)
    .bind(repos_json)
    .fetch_one(pool)
    .await?;
    Ok(row)
}

pub async fn summary_for_rule(
    pool: &SqlitePool,
    rule_id: &str,
    days: i64,
) -> crate::Result<McpRuleServeRuleSummary> {
    let days = days.max(1);
    let window = format!("-{days} days");
    let rows = sqlx::query_as!(
        McpRuleServeRuleRow,
        r#"SELECT tool AS "tool!: String", repo_full_name, file_path,
                rule_ids_json AS "rule_ids_json!: String",
                strict_match_count AS "strict_match_count!: i64",
                estimated_tokens AS "estimated_tokens!: i64",
                served_at AS "served_at!: String"
         FROM mcp_rule_serves
         WHERE was_empty = 0
           AND datetime(served_at) >= datetime('now', ?1)
         ORDER BY datetime(served_at) DESC, id DESC"#,
        window,
    )
    .fetch_all(pool)
    .await?;

    let mut summary = McpRuleServeRuleSummary::default();
    for row in rows {
        if !rule_ids_json_contains(&row.rule_ids_json, rule_id) {
            continue;
        }
        summary.calls += 1;
        summary.strict_match_calls += i64::from(row.strict_match_count > 0);
        summary.estimated_tokens += row.estimated_tokens.max(0);
        if summary.latest.is_none() {
            summary.latest = Some(McpRuleServeRuleEvidence {
                tool: row.tool,
                repo_full_name: row.repo_full_name,
                file_path: row.file_path,
                served_at: row.served_at,
                strict_scoped: row.strict_match_count > 0,
                estimated_tokens: row.estimated_tokens.max(0),
            });
        }
    }
    Ok(summary)
}

fn rule_ids_json_contains(raw: &str, rule_id: &str) -> bool {
    serde_json::from_str::<Vec<String>>(raw).is_ok_and(|ids| ids.iter().any(|id| id == rule_id))
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

    #[test]
    fn query_hash_is_stable_and_does_not_expose_query_text() {
        let hash = query_hash("src/auth.ts validate bearer token");
        assert_eq!(hash.len(), 64);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(hash, query_hash("src/auth.ts validate bearer token"));
        assert!(!hash.contains("auth"));
    }

    #[tokio::test]
    async fn record_and_summary_capture_served_and_empty_calls() {
        let pool = setup().await;
        record(
            &pool,
            &McpRuleServeInput {
                tool: "search_rules",
                session_id: Some("session-1"),
                repo_full_name: Some("acme/app"),
                file_path: Some("src/auth.ts"),
                query_text: "src/auth.ts validate bearer token",
                rule_ids: &["rule-1".to_owned(), "rule-2".to_owned()],
                top_k: 5,
                strict_match_count: 2,
                estimated_tokens: 123,
            },
        )
        .await
        .expect("record served");
        record(
            &pool,
            &McpRuleServeInput {
                tool: "search_rules",
                session_id: Some("session-2"),
                repo_full_name: Some("acme/app"),
                file_path: Some("src/other.ts"),
                query_text: "src/other.ts unknown pattern",
                rule_ids: &[],
                top_k: 10,
                strict_match_count: 0,
                estimated_tokens: 25,
            },
        )
        .await
        .expect("record empty");

        let summary = summary(&pool, 30).await.expect("summary");
        assert_eq!(summary.calls, 2);
        assert_eq!(summary.empty_calls, 1);
        assert_eq!(summary.rules_served, 2);
        assert_eq!(summary.strict_matches, 2);
        assert_eq!(summary.estimated_tokens, 148);
    }

    #[tokio::test]
    async fn summary_for_repos_counts_only_matching_repo_aliases() {
        let pool = setup().await;
        record(
            &pool,
            &McpRuleServeInput {
                tool: "search_rules",
                session_id: Some("session-1"),
                repo_full_name: Some("Acme/App"),
                file_path: Some("src/auth.ts"),
                query_text: "auth middleware",
                rule_ids: &["rule-1".to_owned(), "rule-2".to_owned()],
                top_k: 5,
                strict_match_count: 2,
                estimated_tokens: 120,
            },
        )
        .await
        .expect("record acme serve");
        record(
            &pool,
            &McpRuleServeInput {
                tool: "search_rules",
                session_id: Some("session-2"),
                repo_full_name: Some("other/repo"),
                file_path: Some("src/auth.ts"),
                query_text: "auth middleware",
                rule_ids: &["rule-3".to_owned()],
                top_k: 5,
                strict_match_count: 1,
                estimated_tokens: 90,
            },
        )
        .await
        .expect("record other serve");

        let scoped = summary_for_repos(&pool, &["acme/app".to_owned()], 30)
            .await
            .expect("scoped summary");

        assert_eq!(scoped.calls, 1);
        assert_eq!(scoped.rules_served, 2);
        assert_eq!(scoped.strict_matches, 2);
        assert_eq!(scoped.estimated_tokens, 120);
    }

    #[tokio::test]
    async fn summary_for_rule_counts_only_exact_json_rule_ids() {
        let pool = setup().await;
        record(
            &pool,
            &McpRuleServeInput {
                tool: "search_rules",
                session_id: Some("session-1"),
                repo_full_name: Some("acme/app"),
                file_path: Some("src/auth.ts"),
                query_text: "auth middleware",
                rule_ids: &["rule-1".to_owned(), "rule-10".to_owned()],
                top_k: 5,
                strict_match_count: 2,
                estimated_tokens: 120,
            },
        )
        .await
        .expect("record multi-rule serve");
        record(
            &pool,
            &McpRuleServeInput {
                tool: "search_rules",
                session_id: Some("session-2"),
                repo_full_name: Some("acme/app"),
                file_path: Some("src/auth.ts"),
                query_text: "auth middleware exact",
                rule_ids: &["rule-10".to_owned()],
                top_k: 5,
                strict_match_count: 0,
                estimated_tokens: 80,
            },
        )
        .await
        .expect("record single-rule serve");

        let rule_1 = summary_for_rule(&pool, "rule-1", 30)
            .await
            .expect("summary rule-1");
        assert_eq!(
            rule_1,
            McpRuleServeRuleSummary {
                calls: 1,
                strict_match_calls: 1,
                estimated_tokens: 120,
                latest: Some(McpRuleServeRuleEvidence {
                    tool: "search_rules".to_owned(),
                    repo_full_name: Some("acme/app".to_owned()),
                    file_path: Some("src/auth.ts".to_owned()),
                    served_at: rule_1.latest.as_ref().unwrap().served_at.clone(),
                    strict_scoped: true,
                    estimated_tokens: 120,
                }),
            }
        );

        let rule_10 = summary_for_rule(&pool, "rule-10", 30)
            .await
            .expect("summary rule-10");
        assert_eq!(
            rule_10,
            McpRuleServeRuleSummary {
                calls: 2,
                strict_match_calls: 1,
                estimated_tokens: 200,
                latest: Some(McpRuleServeRuleEvidence {
                    tool: "search_rules".to_owned(),
                    repo_full_name: Some("acme/app".to_owned()),
                    file_path: Some("src/auth.ts".to_owned()),
                    served_at: rule_10.latest.as_ref().unwrap().served_at.clone(),
                    strict_scoped: false,
                    estimated_tokens: 80,
                }),
            }
        );
    }
}
