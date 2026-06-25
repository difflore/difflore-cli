mod diagnostics;
mod pool;
mod queries;
mod schema;

pub(crate) use diagnostics::cloud_embed_outage_active;
pub use diagnostics::{
    EmbeddingDiagnostics, effective_embedding_profile_for_freshness,
    embedding_provider_recently_down, gather_embedding_diagnostics,
    gather_embedding_diagnostics_with_activity,
};
pub use pool::{
    get_pool_for_cwd, get_pool_for_project, mark_rule_index_current, open_index_pool_at,
    rule_index_is_current,
};
pub use queries::{
    RuleChunksUpsertOutcome, fts_search, query_rule_chunks, query_rule_chunks_no_embeddings,
    upsert_rule_chunks, upsert_rule_chunks_isolated, upsert_rule_chunks_with_local_profile,
    upsert_rule_chunks_with_profile, upsert_rule_chunks_with_profile_and_timeout,
};
pub use schema::{IndexedRuleChunk, QueryFilter, index_db_path_for_project};

#[cfg(test)]
pub(crate) use schema::open_pool_at;
pub(crate) use schema::retired_global_index_db_path;

#[cfg(test)]
mod tests {
    use super::schema::INDEX_DB_NAME;
    use super::*;
    use crate::context::rule_source::RuleDocument;
    use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};
    use tempfile::TempDir;

    async fn fresh_pool(tmp: &TempDir) -> SqlitePool {
        let path = tmp.path().join("idx.db");
        open_pool_at(&path).await.expect("open_pool_at")
    }

    fn rd(skill_id: &str, content: &str) -> RuleDocument {
        RuleDocument {
            skill_id: skill_id.to_owned(),
            title: skill_id.to_owned(),
            content: content.to_owned(),
            confidence: 0.7,
            file_patterns: None,
            language: None,
            repo_scope: None,
        }
    }

    fn rd_with(
        skill_id: &str,
        content: &str,
        language: Option<&str>,
        repo_scope: Option<&str>,
    ) -> RuleDocument {
        RuleDocument {
            skill_id: skill_id.to_owned(),
            title: skill_id.to_owned(),
            content: content.to_owned(),
            confidence: 0.7,
            file_patterns: None,
            language: language.map(String::from),
            repo_scope: repo_scope.map(String::from),
        }
    }

    /// Per-test project hash so tests can share the crate-wide
    /// `shared_test_home()` without colliding in `projects/<hash>/…`, avoiding
    /// the `set_var` / `remove_var` race with concurrent test modules.
    fn unique_hash(tag: &str) -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        format!("{tag}-{nanos}")
    }

    #[tokio::test]
    async fn get_pool_for_project_creates_dir_and_db_on_first_call() {
        let home = crate::infra::db::shared_test_home();
        let hash = unique_hash("testhashcreate");
        let pool = get_pool_for_project(&hash).await.unwrap();
        // A trivial query succeeding proves the table was created.
        let _ = sqlx::query_scalar!(r#"SELECT COUNT(*) as "n!: i64" FROM rule_chunks"#)
            .fetch_one(&pool)
            .await
            .expect("count rule_chunks");
        let db_path = home.join("projects").join(&hash).join(INDEX_DB_NAME);
        assert!(
            db_path.exists(),
            "index DB file should have been created at {db_path:?}"
        );
    }

    #[tokio::test]
    async fn get_pool_for_project_reuses_pool_on_second_call() {
        let _home = crate::infra::db::shared_test_home();
        let hash = unique_hash("testhashreuse");
        let p1 = get_pool_for_project(&hash).await.unwrap();
        let p2 = get_pool_for_project(&hash).await.unwrap();
        // Can't compare the inner Arcs publicly, so prove the cache hit by
        // writing via p1 and reading the same state via p2.
        sqlx::query!(
            "INSERT INTO rule_chunks (id, skill_id, content, embedding, file_patterns) \
             VALUES ('rule-x', 'skill-x', 'hello', NULL, NULL)"
        )
        .execute(&p1)
        .await
        .expect("insert rule_chunks");
        let n = sqlx::query_scalar!(r#"SELECT COUNT(*) as "n!: i64" FROM rule_chunks"#)
            .fetch_one(&p2)
            .await
            .expect("count rule_chunks");
        assert_eq!(n, 1, "second pool must see writes from first pool");
    }

    #[tokio::test]
    async fn upsert_populates_language_and_repo_scope_columns() {
        // New rows carry the denormalised metadata so the SQL pre-filter can
        // act on them without a join back to the skills table.
        let tmp = TempDir::new().unwrap();
        let pool = fresh_pool(&tmp).await;
        let rules = vec![rd_with(
            "rust-style-1",
            "prefer `?` over `.unwrap()` in fallible code",
            Some("rust"),
            Some("tokio-rs/tokio"),
        )];
        upsert_rule_chunks(&pool, &rules).await.unwrap();

        let chunks = query_rule_chunks(&pool, &QueryFilter::default())
            .await
            .unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].language.as_deref(), Some("rust"));
        assert_eq!(chunks[0].repo_scope.as_deref(), Some("tokio-rs/tokio"));
    }

    #[tokio::test]
    async fn upsert_with_local_profile_uses_sha1_vectors() {
        let tmp = TempDir::new().unwrap();
        let pool = fresh_pool(&tmp).await;
        let rules = vec![rd("local-only", "keep MCP retrieval local")];

        let outcome = upsert_rule_chunks_with_local_profile(&pool, &rules)
            .await
            .unwrap();

        assert_eq!(
            outcome.embedding_profile,
            crate::context::embedding::local_embedding_profile()
        );
        let chunks = query_rule_chunks(&pool, &QueryFilter::default())
            .await
            .unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(
            chunks[0].embedding.len(),
            crate::context::embedding::EMBEDDING_DIM
        );
    }

    #[tokio::test]
    async fn upsert_empty_rule_set_prunes_all_orphan_chunks() {
        let tmp = TempDir::new().unwrap();
        let pool = fresh_pool(&tmp).await;
        upsert_rule_chunks(&pool, &[rd("orphan-a", "old a"), rd("orphan-b", "old b")])
            .await
            .unwrap();

        upsert_rule_chunks(&pool, &[]).await.unwrap();

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM rule_chunks")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
        let fts_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM rule_chunks_fts")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(fts_count, 0);
    }

    #[tokio::test]
    async fn query_rule_chunks_pre_filter_language_drops_off_language_chunks() {
        let tmp = TempDir::new().unwrap();
        let pool = fresh_pool(&tmp).await;
        let rules = vec![
            rd_with("rust-1", "rust content", Some("rust"), None),
            rd_with("py-1", "python content", Some("python"), None),
            rd_with("unscoped", "scope-free content", None, None),
        ];
        upsert_rule_chunks(&pool, &rules).await.unwrap();

        let filter = QueryFilter {
            language: Some("rust".into()),
            repo_scope: None,
        };
        let chunks = query_rule_chunks(&pool, &filter).await.unwrap();
        let ids: std::collections::HashSet<_> =
            chunks.iter().map(|c| c.skill_id.as_str()).collect();
        assert!(ids.contains("rust-1"));
        assert!(!ids.contains("py-1"), "python chunk must be filtered out");
        // NULL language means "global rule — applies to every language"
        // (mirrors repo_scope NULL semantics). Untagged rules must come
        // through, otherwise filter_from_file would silently drop the cluster
        // pipeline's most common case (almost no cloud rules carry a language
        // tag).
        assert!(
            ids.contains("unscoped"),
            "NULL language column must match a strict language filter as global"
        );
    }

    #[tokio::test]
    async fn query_rule_chunks_repo_scope_requires_exact_project_match() {
        // `repo_scope` NULL is unattributed metadata, not a runtime global
        // rule. When a caller filters by a specific repo, only that repo's
        // rows are eligible.
        let tmp = TempDir::new().unwrap();
        let pool = fresh_pool(&tmp).await;
        let rules = vec![
            rd_with("global-1", "applies everywhere", None, None),
            rd_with("repoA-1", "repo A only", None, Some("orgA/repoA")),
            rd_with("repoB-1", "repo B only", None, Some("orgB/repoB")),
        ];
        upsert_rule_chunks(&pool, &rules).await.unwrap();

        let filter = QueryFilter {
            language: None,
            repo_scope: Some("orgA/repoA".into()),
        };
        let chunks = query_rule_chunks(&pool, &filter).await.unwrap();
        let ids: std::collections::HashSet<_> =
            chunks.iter().map(|c| c.skill_id.as_str()).collect();
        assert!(
            !ids.contains("global-1"),
            "NULL scope must not widen into the current project"
        );
        assert!(ids.contains("repoA-1"));
        assert!(!ids.contains("repoB-1"), "other-repo row must be filtered");
    }

    #[tokio::test]
    async fn fts_triggers_update_on_chunk_insert_update_delete() {
        // Stress the three trigger paths: AFTER INSERT populates, AFTER
        // UPDATE refreshes content, AFTER DELETE removes.
        let tmp = TempDir::new().unwrap();
        let pool = fresh_pool(&tmp).await;

        // INSERT
        upsert_rule_chunks(&pool, &[rd("t1", "alpha bravo charlie")])
            .await
            .unwrap();
        let hits = fts_search(&pool, "bravo", &QueryFilter::default(), 5)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1, "insert trigger must mirror into FTS");

        // UPDATE (same skill_id → same chunk_id; content changes)
        upsert_rule_chunks(&pool, &[rd("t1", "delta echo foxtrot")])
            .await
            .unwrap();
        let stale = fts_search(&pool, "bravo", &QueryFilter::default(), 5)
            .await
            .unwrap();
        assert!(stale.is_empty(), "update trigger must drop old content");
        let fresh = fts_search(&pool, "foxtrot", &QueryFilter::default(), 5)
            .await
            .unwrap();
        assert_eq!(fresh.len(), 1, "update trigger must index new content");

        // DELETE
        sqlx::query!("DELETE FROM rule_chunks WHERE skill_id = 't1'")
            .execute(&pool)
            .await
            .expect("delete rule_chunks");
        let gone = fts_search(&pool, "foxtrot", &QueryFilter::default(), 5)
            .await
            .unwrap();
        assert!(gone.is_empty(), "delete trigger must remove from FTS");
    }

    #[tokio::test]
    async fn upsert_repairs_existing_fts_row_mismatch() {
        let tmp = TempDir::new().unwrap();
        let pool = fresh_pool(&tmp).await;
        let rules = vec![rd("t1", "alpha bravo")];
        upsert_rule_chunks(&pool, &rules).await.unwrap();
        sqlx::query("DELETE FROM rule_chunks_fts")
            .execute(&pool)
            .await
            .expect("delete fts mirror rows");

        upsert_rule_chunks(&pool, &rules).await.unwrap();

        let fts_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM rule_chunks_fts")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(fts_count, 1);
        let hits = fts_search(&pool, "bravo", &QueryFilter::default(), 5)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[tokio::test]
    async fn fts_search_finds_token_match_when_embedding_misses() {
        // The embedding vector for the SHA1 fallback is bag-of-words with
        // random-signed hashes → for short distinctive tokens like a
        // symbol name, FTS reliably wins where cosine noise dominates.
        let tmp = TempDir::new().unwrap();
        let pool = fresh_pool(&tmp).await;
        let rules = vec![
            rd(
                "r-zebrafish",
                "avoid using deprecated_xyzzy_handler in request paths",
            ),
            rd("r-unrelated", "totally unrelated rule about logging"),
        ];
        upsert_rule_chunks(&pool, &rules).await.unwrap();

        // Query on the rare token — FTS should return the zebrafish rule first.
        let hits = fts_search(
            &pool,
            "deprecated_xyzzy_handler",
            &QueryFilter::default(),
            5,
        )
        .await
        .unwrap();
        assert!(!hits.is_empty(), "FTS must find the rare token");
        let ids: Vec<_> = hits.iter().map(|(id, _)| id.as_str()).collect();
        assert!(
            ids.iter().any(|id| id.contains("zebrafish")),
            "FTS hit set must include the matching rule"
        );
    }

    #[tokio::test]
    async fn upsert_surfaces_preload_failure_instead_of_reindexing_blindly() {
        let tmp = TempDir::new().unwrap();
        let pool = fresh_pool(&tmp).await;
        sqlx::query("DROP TABLE rule_chunks")
            .execute(&pool)
            .await
            .expect("drop rule_chunks");

        let err = upsert_rule_chunks(&pool, &[rd("t1", "alpha bravo")])
            .await
            .expect_err("missing rule_chunks table should surface as an error");
        assert!(
            err.to_string().contains("rule_chunks"),
            "error should name the failing index table, got: {err}"
        );
    }

    #[tokio::test]
    async fn rule_index_is_current_surfaces_chunk_count_failure() {
        let tmp = TempDir::new().unwrap();
        let pool = fresh_pool(&tmp).await;
        sqlx::query("DROP TABLE rule_chunks")
            .execute(&pool)
            .await
            .expect("drop rule_chunks");

        let state = crate::context::rule_source::RuleIndexState {
            rule_count: 1,
            max_updated_at: None,
            embedding_profile: format!("sha1:local:{}", crate::context::embedding::EMBEDDING_DIM),
            scope_signature: None,
        };
        let err = rule_index_is_current(&pool, &state)
            .await
            .expect_err("chunk count failure should be observable");
        assert!(
            err.to_string().contains("rule_chunks"),
            "error should mention the missing chunk table, got: {err}"
        );
    }

    #[tokio::test]
    async fn rule_index_is_current_surfaces_missing_fts_table() {
        let tmp = TempDir::new().unwrap();
        let pool = fresh_pool(&tmp).await;
        sqlx::query("DROP TABLE rule_chunks_fts")
            .execute(&pool)
            .await
            .expect("drop fts table");

        let state = crate::context::rule_source::RuleIndexState {
            rule_count: 1,
            max_updated_at: None,
            embedding_profile: format!("sha1:local:{}", crate::context::embedding::EMBEDDING_DIM),
            scope_signature: None,
        };
        let err = rule_index_is_current(&pool, &state)
            .await
            .expect_err("missing FTS table should be observable");
        assert!(
            err.to_string().contains("rule_chunks_fts"),
            "error should mention the missing FTS table, got: {err}"
        );
    }

    #[tokio::test]
    async fn rule_index_scope_change_with_same_count_invalidates_freshness() {
        // A git remote change can swap one repo's in-scope rule set for a
        // different but equally-sized set with the same max timestamp. The
        // count/timestamp signature cannot tell these apart, so the scope
        // signature must, otherwise the freshness check would skip a re-index
        // and serve the wrong scope's chunks.
        let tmp = TempDir::new().unwrap();
        let pool = fresh_pool(&tmp).await;

        // Index two chunks for scope A and persist its signature.
        let scope_a = vec![rd("a-1", "alpha"), rd("a-2", "bravo")];
        upsert_rule_chunks(&pool, &scope_a).await.unwrap();
        let profile = crate::context::embedding::active_embedding_profile().await;
        let sig_a = crate::context::rule_source::scope_signature_from_skill_ids(
            scope_a.iter().map(|rule| rule.skill_id.as_str()),
        );
        let state_a = crate::context::rule_source::RuleIndexState {
            rule_count: 2,
            max_updated_at: Some("2026-05-01T00:00:00Z".to_owned()),
            embedding_profile: profile.clone(),
            scope_signature: sig_a.clone(),
        };
        mark_rule_index_current(&pool, &state_a).await.unwrap();

        // Same scope signature: still current.
        assert!(
            rule_index_is_current(&pool, &state_a).await.unwrap(),
            "unchanged scope must stay fresh"
        );

        // Different scope (same count, same max_updated_at) must invalidate.
        let sig_b = crate::context::rule_source::scope_signature_from_skill_ids(["b-1", "b-2"]);
        assert_ne!(sig_a, sig_b, "different rule sets must hash differently");
        let state_b = crate::context::rule_source::RuleIndexState {
            rule_count: 2,
            max_updated_at: Some("2026-05-01T00:00:00Z".to_owned()),
            embedding_profile: profile,
            scope_signature: sig_b,
        };
        assert!(
            !rule_index_is_current(&pool, &state_b).await.unwrap(),
            "a scope swap with unchanged count/timestamp must trigger a re-index"
        );
    }

    #[tokio::test]
    async fn migration_backfills_existing_fts_and_language_columns() {
        // Simulate a DB that pre-dated the FTS5 / language columns: create
        // the minimal pre-metadata schema, seed a row, then re-open via
        // `open_pool_at` and confirm the migration back-filled.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("pre-metadata.db");

        // Bootstrap a rule_chunks table WITHOUT FTS / language / repo_scope.
        // Using sqlx directly so we don't run the full migration path the
        // first time.
        {
            let opts = SqliteConnectOptions::new()
                .filename(&path)
                .create_if_missing(true)
                .journal_mode(SqliteJournalMode::Wal)
                .busy_timeout(std::time::Duration::from_secs(30));
            let pool = SqlitePoolOptions::new()
                .max_connections(1)
                .connect_with(opts)
                .await
                .unwrap();
            // non-macro: bootstraps a DB lacking the language/repo_scope
            // columns to exercise the migration path; cannot use query! because
            // the prepare DB already has rule_chunks at full shape.
            sqlx::query(
                "CREATE TABLE rule_chunks (
                    id TEXT PRIMARY KEY,
                    skill_id TEXT NOT NULL,
                    content TEXT NOT NULL,
                    embedding BLOB,
                    file_patterns TEXT
                )",
            )
            .execute(&pool)
            .await
            .expect("pre-metadata CREATE TABLE");
            sqlx::query(
                "INSERT INTO rule_chunks (id, skill_id, content, embedding, file_patterns) \
                 VALUES ('rule-pre-metadata', 'pre-metadata', 'pre metadata rule body', NULL, NULL)"
            )
            .execute(&pool)
            .await
            .unwrap();
            pool.close().await;
        }

        // Re-open through the migration path. It should add the missing
        // columns, create the FTS table, and back-fill existing content.
        let pool = open_pool_at(&path).await.unwrap();

        // language/repo_scope columns now exist (NULL-valued for rows that
        // predate those columns). The vector query skips this legacy row
        // because its embedding is NULL; FTS backfill below still indexes it.
        let (language, repo_scope): (Option<String>, Option<String>) = sqlx::query_as(
            "SELECT language, repo_scope FROM rule_chunks WHERE id = 'rule-pre-metadata'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(language.is_none());
        assert!(repo_scope.is_none());
        let chunks = query_rule_chunks(&pool, &QueryFilter::default())
            .await
            .unwrap();
        assert!(
            chunks.is_empty(),
            "legacy rows with NULL embeddings must not be exposed as empty vectors"
        );

        // FTS back-fill populated the existing content; searching for a
        // token that's only in that rule should return it.
        let hits = fts_search(&pool, "metadata", &QueryFilter::default(), 5)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1, "FTS back-fill must index pre-existing rows");
    }
}
