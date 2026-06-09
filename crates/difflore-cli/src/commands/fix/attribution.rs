// Lightweight per-rule attribution lookup: given the rule ids that
// drove a `fix` recall, return their `source_repo` so the report can
// say "learned from gin-gonic/gin" next to each rule. Cheap on cold
// caches because matched recall sets are small (top-K), and best-effort
// — any DB error degrades gracefully to an empty map so the user-facing
// flow keeps working.
use std::collections::HashMap;

use difflore_core::SqlitePool;

/// Map of rule id → `source_repo` for rules that have one. Rules without
/// a source repo (manual / global) are simply absent from the map.
pub(super) async fn fetch_rule_source_repos(
    db: &SqlitePool,
    rule_ids: &[String],
) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if rule_ids.is_empty() {
        return out;
    }
    // Build a parameter list — sqlx doesn't expand Vec into IN(?, ?, …)
    // for SQLite, so we render placeholders ourselves. Rule ids are uuids
    // produced by our pipeline so they're not user input; still bind via
    // parameters for safety.
    let placeholders = std::iter::repeat_n("?", rule_ids.len())
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT id, source_repo FROM skills WHERE id IN ({placeholders}) AND source_repo IS NOT NULL AND source_repo != ''"
    );
    let mut q = sqlx::query_as::<_, (String, String)>(&sql);
    for id in rule_ids {
        q = q.bind(id);
    }
    match q.fetch_all(db).await {
        Ok(rows) => {
            for (id, repo) in rows {
                out.insert(id, repo);
            }
        }
        Err(e) => {
            // Best-effort: surface once via stderr (recall already prints to
            // stderr on debug paths) but never fail the fix flow.
            eprintln!("[attribution] source_repo lookup failed: {e}");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn empty_input_returns_empty() {
        // Use an in-memory SQLite to avoid touching the user's DB.
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let result = fetch_rule_source_repos(&pool, &[]).await;
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn returns_repo_for_known_rules_only() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE skills (id TEXT PRIMARY KEY, name TEXT NOT NULL, source TEXT NOT NULL, \
             directory TEXT NOT NULL, version TEXT NOT NULL, source_repo TEXT)",
        )
        .execute(&pool)
        .await
        .unwrap();
        for (id, repo) in [
            ("r1", Some("gin-gonic/gin")),
            ("r2", Some("vitejs/vite")),
            ("r3", None::<&str>),
        ] {
            sqlx::query!(
                "INSERT INTO skills (id, name, source, directory, version, source_repo) VALUES (?, 'n', 'manual', '/', '1.0.0', ?)",
                id,
                repo,
            )
            .execute(&pool)
            .await
            .unwrap();
        }
        let ids = [
            "r1".to_owned(),
            "r2".to_owned(),
            "r3".to_owned(),
            "missing".to_owned(),
        ];
        let result = fetch_rule_source_repos(&pool, &ids).await;
        assert_eq!(result.get("r1").map(String::as_str), Some("gin-gonic/gin"));
        assert_eq!(result.get("r2").map(String::as_str), Some("vitejs/vite"));
        assert!(!result.contains_key("r3"));
        assert!(!result.contains_key("missing"));
    }
}
