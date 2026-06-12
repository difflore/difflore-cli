#[cfg(test)]
mod tests {
    use super::super::*;
    use crate::domain::models::*;

    #[test]
    fn decode_base64_table() {
        // GitHub contents API returns base64 with embedded newlines, so the
        // whitespace-tolerant case is the most load-bearing entry here.
        let cases: &[(&str, &str)] = &[
            ("aGVsbG8=", "hello"),
            ("Zm9v", "foo"),
            ("aGVs\nbG8g\nd29y\nbGQ=", "hello world"),
            ("", ""),
        ];
        for (input, expected) in cases {
            assert_eq!(decode_base64_lossy(input), *expected, "input: {input:?}");
        }
    }

    #[test]
    fn parse_list_value_table() {
        let cases: &[(&str, Vec<&str>)] = &[
            (
                "[rust, typescript, python]",
                vec!["rust", "typescript", "python"],
            ),
            ("['foo', \"bar\", , 'baz']", vec!["foo", "bar", "baz"]),
            ("a, b, c", vec!["a", "b", "c"]),
        ];
        for (input, expected) in cases {
            assert_eq!(parse_list_value(input), *expected, "input: {input}");
        }
    }

    #[test]
    fn parse_skill_frontmatter_extracts_fields_and_body() {
        let md = "---\n\
type: review_standard\n\
version: 2.0.0\n\
tags: [security, api]\n\
engines: [claude, codex]\n\
trigger: on-review\n\
---\n\
\n\
# My Rule\n\
\n\
body text";
        let fm = parse_skill_frontmatter(md);
        assert_eq!(fm.r#type.as_deref(), Some("review_standard"));
        assert_eq!(fm.version.as_deref(), Some("2.0.0"));
        assert_eq!(fm.trigger.as_deref(), Some("on-review"));
        assert_eq!(
            fm.tags.as_ref().unwrap(),
            &vec!["security".to_owned(), "api".to_owned()]
        );
        assert_eq!(
            fm.engines.as_ref().unwrap(),
            &vec!["claude".to_owned(), "codex".to_owned()]
        );
        assert!(fm.body.contains("# My Rule"));
        assert!(fm.body.contains("body text"));
        // Frontmatter itself should not leak into body.
        assert!(!fm.body.contains("type: review_standard"));
    }

    #[test]
    fn parse_skill_frontmatter_without_leading_fence_returns_whole_content() {
        let md = "# Plain markdown\n\nno frontmatter here";
        let fm = parse_skill_frontmatter(md);
        assert!(fm.r#type.is_none());
        assert!(fm.tags.is_none());
        assert_eq!(fm.body, md);
    }

    #[test]
    fn skill_row_to_record_parses_json_and_coerces_bool_flags() {
        let row = SkillRow {
            id: "skill-1".into(),
            name: "My Skill".into(),
            source: "local".into(),
            directory: "my-skill".into(),
            version: "1.0.0".into(),
            description: "desc".into(),
            r#type: "skill".into(),
            engines: r#"["claude","codex"]"#.into(),
            tags: r#"["x","y"]"#.into(),
            trigger: Some("t".into()),
            check_prompt: None,
            repo_owner: None,
            repo_name: None,
            repo_branch: None,
            readme_url: None,
            enabled_for_codex: 1,
            enabled_for_claude: 0,
            enabled_for_gemini: 1,
            enabled_for_cursor: 0,
            installed_at: "t".into(),
            updated_at: "t".into(),
            origin: "manual".into(),
        };
        let rec: SkillRecord = row.into();
        assert_eq!(rec.engines, vec!["claude".to_owned(), "codex".to_owned()]);
        assert_eq!(rec.tags, vec!["x".to_owned(), "y".to_owned()]);
        assert!(rec.enabled_for_codex);
        assert!(!rec.enabled_for_claude);
        assert!(rec.enabled_for_gemini);
        assert!(!rec.enabled_for_cursor);
        assert!(rec.enforcement.is_none());
    }

    #[test]
    fn skill_row_to_record_recovers_malformed_engines_column() {
        let row = SkillRow {
            id: "skill-bad".into(),
            name: "bad".into(),
            source: "local".into(),
            directory: "bad".into(),
            version: "1.0.0".into(),
            description: String::new(),
            r#type: "skill".into(),
            engines: "not-json".into(),
            tags: "{not json}".into(),
            trigger: None,
            check_prompt: None,
            repo_owner: None,
            repo_name: None,
            repo_branch: None,
            readme_url: None,
            enabled_for_codex: 0,
            enabled_for_claude: 0,
            enabled_for_gemini: 0,
            enabled_for_cursor: 0,
            installed_at: "t".into(),
            updated_at: "t".into(),
            origin: "manual".into(),
        };
        let rec: SkillRecord = row.into();
        assert_eq!(rec.engines, vec!["claude".to_owned()]);
        assert!(rec.tags.is_empty());
    }

    #[test]
    fn skill_repo_row_to_record_flips_enabled_int_to_bool() {
        let enabled_row = SkillRepoRow {
            id: "repo-1".into(),
            owner: "anthropic".into(),
            name: "skills".into(),
            branch: "main".into(),
            enabled: 1,
            created_at: "t".into(),
        };
        let disabled_row = SkillRepoRow {
            id: "repo-2".into(),
            owner: "x".into(),
            name: "y".into(),
            branch: "main".into(),
            enabled: 0,
            created_at: "t".into(),
        };
        let r1: SkillRepoRecord = enabled_row.into();
        let r2: SkillRepoRecord = disabled_row.into();
        assert!(r1.enabled);
        assert!(!r2.enabled);
    }

    // remember_rule content-hash + 30s window dedup: a content-hash storm
    // inside the window collapses to a single soft-accept bump; outside the
    // window, title/body dedup or a fresh insert still applies.

    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;

    struct DedupTestEnv;

    impl DedupTestEnv {
        async fn db() -> sqlx::SqlitePool {
            let _home = crate::infra::db::shared_test_home();
            let opts = SqliteConnectOptions::from_str("sqlite::memory:")
                .unwrap()
                .foreign_keys(true);
            let pool = SqlitePoolOptions::new()
                .max_connections(1)
                .connect_with(opts)
                .await
                .unwrap();
            crate::infra::db::run_migrations(&pool).await.unwrap();
            pool
        }
    }

    fn synced_rule(id: &str) -> crate::cloud::sync::SyncedRule {
        crate::cloud::sync::SyncedRule {
            id: id.to_owned(),
            name: "Cloud review rule".to_owned(),
            r#type: "review_standard".to_owned(),
            description: "Keep the cross-agent review memory available.".to_owned(),
            version: "1.0.0".to_owned(),
            engines: vec!["claude".to_owned()],
            tags: vec!["origin:review-extraction".to_owned()],
            trigger: None,
            check_prompt: None,
            content: "Cloud review memory should protect every local agent.".to_owned(),
            updated_at: "2026-05-06T00:00:00Z".to_owned(),
            created_at: "2026-05-06T00:00:00Z".to_owned(),
            file_patterns: vec!["**/*.rs".to_owned()],
            origin: Some("extracted".to_owned()),
            source_repo: Some("acme/widgets".to_owned()),
        }
    }

    #[tokio::test]
    async fn apply_sync_result_enables_cloud_rules_for_all_agents() {
        let db = DedupTestEnv::db().await;
        apply_sync_result(
            &db,
            &crate::cloud::sync::SyncResult {
                created: vec![synced_rule("cloud-all-agents")],
                updated: vec![],
                deleted: vec![],
            },
        )
        .await
        .unwrap();

        let row = sqlx::query!(
            "SELECT enabled_for_codex, enabled_for_claude, enabled_for_gemini, enabled_for_cursor \
             FROM skills WHERE id = ?1",
            "cloud-all-agents",
        )
        .fetch_one(&db)
        .await
        .unwrap();

        assert_eq!(row.enabled_for_codex, 1);
        assert_eq!(row.enabled_for_claude, 1);
        assert_eq!(row.enabled_for_gemini, 1);
        assert_eq!(row.enabled_for_cursor, 1);
    }

    #[tokio::test]
    async fn repo_scope_alias_expansion_is_unique_and_conservative() {
        let db = DedupTestEnv::db().await;
        for (id, repo) in [
            ("fastapi-rule", "fastapi/fastapi"),
            ("router-upstream", "tanstack/router"),
            ("router-direct", "difflore-fixtures/router"),
            ("ambiguous-one", "one/widgets"),
            ("ambiguous-two", "two/widgets"),
        ] {
            sqlx::query(
                "INSERT INTO skills
                 (id, name, source, directory, version, description, source_repo, status)
                 VALUES (?1, ?1, 'cloud', '/tmp', '1.0.0', 'body', ?2, 'active')",
            )
            .bind(id)
            .bind(repo)
            .execute(&db)
            .await
            .unwrap();
        }

        let fastapi =
            expand_repo_scopes_with_source_aliases(&db, &["difflore-fixtures/fastapi".to_owned()])
                .await
                .unwrap();
        assert_eq!(
            fastapi,
            vec![
                "difflore-fixtures/fastapi".to_owned(),
                "fastapi/fastapi".to_owned()
            ]
        );

        let router =
            expand_repo_scopes_with_source_aliases(&db, &["difflore-fixtures/router".to_owned()])
                .await
                .unwrap();
        assert_eq!(
            router,
            vec![
                "difflore-fixtures/router".to_owned(),
                "tanstack/router".to_owned()
            ]
        );

        let ambiguous = expand_repo_scopes_with_source_aliases(&db, &["acme/widgets".to_owned()])
            .await
            .unwrap();
        assert_eq!(ambiguous, vec!["acme/widgets".to_owned()]);

        sqlx::query(
            "INSERT INTO skills
             (id, name, source, directory, version, description, source_repo, status)
             VALUES (?1, ?1, 'cloud', '/tmp', '1.0.0', 'body', ?2, 'active')",
        )
        .bind("github-app-rule")
        .bind("acme/app")
        .execute(&db)
        .await
        .unwrap();
        let gitlab =
            expand_repo_scopes_with_source_aliases(&db, &["gitlab.com/acme/app".to_owned()])
                .await
                .unwrap();
        assert_eq!(gitlab, vec!["gitlab.com/acme/app".to_owned()]);

        let self_managed = expand_repo_scopes_with_source_aliases(
            &db,
            &["gitlab.corp.example/acme/app".to_owned()],
        )
        .await
        .unwrap();
        assert_eq!(
            self_managed,
            vec!["gitlab.corp.example/acme/app".to_owned()]
        );
    }

    #[tokio::test]
    async fn apply_sync_result_preserves_existing_agent_toggles_on_noop_sync() {
        let db = DedupTestEnv::db().await;
        sqlx::query!(
            "INSERT INTO skills \
             (id, name, source, directory, version, description, type, engines, tags, \
              enabled_for_codex, enabled_for_claude, enabled_for_gemini, enabled_for_cursor, \
              installed_at, updated_at, status) \
             VALUES \
             ('old-cloud-rule', 'Old cloud rule', 'cloud', 'old-cloud-rule', '1.0.0', \
              'legacy cloud sync row', 'review_standard', '[]', '[]', \
              0, 1, 0, 0, '2026-05-05 00:00:00', '2026-05-05 00:00:00', 'active')",
        )
        .execute(&db)
        .await
        .unwrap();

        apply_sync_result(
            &db,
            &crate::cloud::sync::SyncResult {
                created: vec![],
                updated: vec![],
                deleted: vec![],
            },
        )
        .await
        .unwrap();

        let row = sqlx::query!(
            "SELECT enabled_for_codex, enabled_for_claude, enabled_for_gemini, enabled_for_cursor \
             FROM skills WHERE id = ?1",
            "old-cloud-rule",
        )
        .fetch_one(&db)
        .await
        .unwrap();

        assert_eq!(row.enabled_for_codex, 0);
        assert_eq!(row.enabled_for_claude, 1);
        assert_eq!(row.enabled_for_gemini, 0);
        assert_eq!(row.enabled_for_cursor, 0);
    }

    #[tokio::test]
    async fn apply_sync_result_sanitizes_cloud_rule_directory() {
        let db = DedupTestEnv::db().await;
        let raw_id = r"..\..\Windows\System32";
        apply_sync_result(
            &db,
            &crate::cloud::sync::SyncResult {
                created: vec![synced_rule(raw_id)],
                updated: vec![],
                deleted: vec![],
            },
        )
        .await
        .unwrap();

        let directory: String = sqlx::query_scalar("SELECT directory FROM skills WHERE id = ?1")
            .bind(raw_id)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_ne!(directory, raw_id);
        assert!(!directory.contains('\\'));
        assert!(!directory.contains('/'));
        assert!(!directory.contains(".."));
    }

    #[tokio::test]
    async fn apply_sync_result_created_updates_stale_cloud_row_without_clobbering_agent_toggles() {
        let db = DedupTestEnv::db().await;
        sqlx::query(
            "INSERT INTO skills \
             (id, name, source, directory, version, description, type, engines, tags, \
              enabled_for_codex, enabled_for_claude, enabled_for_gemini, enabled_for_cursor, \
              installed_at, updated_at, origin, status) \
             VALUES \
             ('cloud-stale-row', 'Old cloud rule', 'cloud', 'cloud-stale-row', '0.1.0', \
              'stale local description', 'review_standard', '[]', '[]', \
              0, 1, 0, 0, '2026-05-05 00:00:00', '2026-05-05 00:00:00', 'cloud', 'active')",
        )
        .execute(&db)
        .await
        .unwrap();

        let mut rule = synced_rule("cloud-stale-row");
        rule.name = "Fresh cloud rule".to_owned();
        rule.content = "fresh cloud content for retrieval".to_owned();
        rule.version = "2.0.0".to_owned();

        apply_sync_result(
            &db,
            &crate::cloud::sync::SyncResult {
                created: vec![rule],
                updated: vec![],
                deleted: vec![],
            },
        )
        .await
        .unwrap();

        let row: (String, String, String, i64, i64, i64) = sqlx::query_as(
            "SELECT name, version, description, enabled_for_codex, enabled_for_gemini, enabled_for_cursor \
             FROM skills WHERE id = 'cloud-stale-row'",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(row.0, "Fresh cloud rule");
        assert_eq!(row.1, "2.0.0");
        assert_eq!(row.2, "fresh cloud content for retrieval");
        assert_eq!(row.3, 0);
        assert_eq!(row.4, 0);
        assert_eq!(row.5, 0);
    }

    #[tokio::test]
    async fn apply_sync_result_updated_preserves_user_disabled_agent_toggles() {
        let db = DedupTestEnv::db().await;
        apply_sync_result(
            &db,
            &crate::cloud::sync::SyncResult {
                created: vec![synced_rule("cloud-user-toggles")],
                updated: vec![],
                deleted: vec![],
            },
        )
        .await
        .unwrap();
        sqlx::query(
            "UPDATE skills
             SET enabled_for_codex = 0, enabled_for_claude = 1,
                 enabled_for_gemini = 0, enabled_for_cursor = 0
             WHERE id = 'cloud-user-toggles'",
        )
        .execute(&db)
        .await
        .unwrap();

        let mut rule = synced_rule("cloud-user-toggles");
        rule.name = "Cloud rule with refreshed content".to_owned();
        rule.content = "cloud refreshed the text but must not reset user agent toggles".to_owned();
        apply_sync_result(
            &db,
            &crate::cloud::sync::SyncResult {
                created: vec![],
                updated: vec![rule],
                deleted: vec![],
            },
        )
        .await
        .unwrap();

        let row: (String, i64, i64, i64, i64) = sqlx::query_as(
            "SELECT name, enabled_for_codex, enabled_for_claude, enabled_for_gemini, enabled_for_cursor \
             FROM skills WHERE id = ?1",
        )
        .bind("cloud-user-toggles")
        .fetch_one(&db)
        .await
        .unwrap();

        assert_eq!(row.0, "Cloud rule with refreshed content");
        assert_eq!(row.1, 0);
        assert_eq!(row.2, 1);
        assert_eq!(row.3, 0);
        assert_eq!(row.4, 0);
    }

    #[tokio::test]
    async fn apply_sync_result_keeps_existing_source_repo_and_audits_conflict() {
        let db = DedupTestEnv::db().await;
        apply_sync_result(
            &db,
            &crate::cloud::sync::SyncResult {
                created: vec![synced_rule("cloud-source-repo-conflict")],
                updated: vec![],
                deleted: vec![],
            },
        )
        .await
        .unwrap();
        sqlx::query(
            "UPDATE skills SET source_repo = 'acme/old-widgets' WHERE id = 'cloud-source-repo-conflict'",
        )
        .execute(&db)
        .await
        .unwrap();

        let mut rule = synced_rule("cloud-source-repo-conflict");
        rule.source_repo = Some("acme/new-widgets".to_owned());
        apply_sync_result(
            &db,
            &crate::cloud::sync::SyncResult {
                created: vec![],
                updated: vec![rule],
                deleted: vec![],
            },
        )
        .await
        .unwrap();

        let source_repo: String =
            sqlx::query_scalar("SELECT source_repo FROM skills WHERE id = ?1")
                .bind("cloud-source-repo-conflict")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(source_repo, "acme/old-widgets");

        let row: (String, String, Option<String>) =
            sqlx::query_as("SELECT kind, source, metadata FROM rule_events WHERE skill_id = ?1")
                .bind("cloud-source-repo-conflict")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(row.0, "source_repo_conflict");
        assert_eq!(row.1, "cloud_sync");
        let metadata: serde_json::Value = serde_json::from_str(row.2.as_deref().unwrap())
            .expect("source repo conflict metadata must be JSON");
        assert_eq!(metadata["existingSourceRepo"], "acme/old-widgets");
        assert_eq!(metadata["incomingSourceRepo"], "acme/new-widgets");
    }

    #[tokio::test]
    async fn apply_sync_result_canonicalizes_cloud_source_repo_before_writing() {
        let db = DedupTestEnv::db().await;
        let mut rule = synced_rule("cloud-source-repo-canonical");
        rule.source_repo = Some("GitLab.Corp.Example:8443/Group/Project".to_owned());

        apply_sync_result(
            &db,
            &crate::cloud::sync::SyncResult {
                created: vec![rule],
                updated: vec![],
                deleted: vec![],
            },
        )
        .await
        .unwrap();

        let source_repo: String =
            sqlx::query_scalar("SELECT source_repo FROM skills WHERE id = ?1")
                .bind("cloud-source-repo-canonical")
                .fetch_one(&db)
                .await
                .unwrap();

        assert_eq!(source_repo, "gitlab.corp.example:8443/group/project");
    }

    #[tokio::test]
    async fn apply_sync_result_rejects_noncanonical_cloud_source_repo() {
        let db = DedupTestEnv::db().await;
        let mut rule = synced_rule("cloud-source-repo-invalid");
        rule.source_repo = Some("project".to_owned());

        apply_sync_result(
            &db,
            &crate::cloud::sync::SyncResult {
                created: vec![rule],
                updated: vec![],
                deleted: vec![],
            },
        )
        .await
        .unwrap();

        let source_repo: Option<String> =
            sqlx::query_scalar("SELECT source_repo FROM skills WHERE id = ?1")
                .bind("cloud-source-repo-invalid")
                .fetch_one(&db)
                .await
                .unwrap();

        assert_eq!(source_repo, None);
    }

    #[tokio::test]
    async fn search_meta_uses_canonical_source_repo_only() {
        let db = DedupTestEnv::db().await;
        sqlx::query(
            "INSERT INTO skills \
             (id, name, source, directory, version, description, type, engines, tags, \
              file_patterns, repo_owner, repo_name, source_repo, installed_at, updated_at, status) \
             VALUES \
             ('canonical-source', 'Canonical source', 'local', 'canonical-source', '1.0.0', \
              'Rule from canonical source_repo.', 'review_standard', '[]', '[]', \
              '[\"src/**/*.rs\"]', NULL, NULL, 'acme/widgets', '2026-05-19 00:00:00', '2026-05-19 00:00:00', 'active'), \
             ('retired-parts-only', 'Retired parts only', 'local', 'retired-parts-only', '1.0.0', \
              'Old repo parts must not be reinterpreted.', 'review_standard', '[]', '[]', \
              NULL, 'acme', 'widgets', NULL, '2026-05-19 00:00:00', '2026-05-19 00:00:00', 'active')",
        )
        .execute(&db)
        .await
        .unwrap();

        let ids = vec![
            "canonical-source".to_owned(),
            "retired-parts-only".to_owned(),
        ];
        let meta = fetch_search_meta(&db, &ids).await;

        assert_eq!(
            meta["canonical-source"].source_repo.as_deref(),
            Some("acme/widgets")
        );
        assert_eq!(
            meta["canonical-source"].file_patterns,
            vec!["src/**/*.rs".to_owned()]
        );
        assert!(
            meta["retired-parts-only"].source_repo.is_none(),
            "repo_owner/repo_name must not be reconstructed as source_repo"
        );

        let health = crate::infra::db::corpus_health(&db).await.unwrap();
        assert!(
            health
                .by_source_repo
                .contains(&("acme/widgets".to_owned(), 1))
        );
        assert!(
            health.by_source_repo.contains(&("<unset>".to_owned(), 1)),
            "retired repo parts must stay unattributed in corpus health"
        );
        assert!(
            !health
                .by_source_repo
                .contains(&("acme/widgets".to_owned(), 2)),
            "source_repo stats must not combine canonical source_repo with retired repo parts"
        );
    }

    #[tokio::test]
    async fn apply_sync_result_delete_only_removes_cloud_rows() {
        let db = DedupTestEnv::db().await;
        create_local(
            &db,
            CreateLocalSkillInput {
                name: "Keep Local Delete Guard".to_owned(),
                engines: Some(vec![]),
                tags: None,
                description: Some("local row must survive cloud delete".to_owned()),
                r#type: Some("review_standard".to_owned()),
                trigger: None,
                check_prompt: None,
                content: None,
            },
        )
        .await
        .unwrap();
        apply_sync_result(
            &db,
            &crate::cloud::sync::SyncResult {
                created: vec![synced_rule("cloud-delete-me")],
                updated: vec![],
                deleted: vec![],
            },
        )
        .await
        .unwrap();

        apply_sync_result(
            &db,
            &crate::cloud::sync::SyncResult {
                created: vec![],
                updated: vec![],
                deleted: vec![
                    "local-keep-local-delete-guard".to_owned(),
                    "cloud-delete-me".to_owned(),
                ],
            },
        )
        .await
        .unwrap();

        let local_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM skills WHERE id = ?1")
            .bind("local-keep-local-delete-guard")
            .fetch_one(&db)
            .await
            .unwrap();
        let cloud_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM skills WHERE id = ?1")
            .bind("cloud-delete-me")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(local_count, 1);
        assert_eq!(cloud_count, 0);
    }

    fn remember_input(title: &str, body: &str, patterns: Option<Vec<&str>>) -> RememberRuleInput {
        RememberRuleInput {
            title: title.into(),
            body: body.into(),
            file_patterns: patterns.map(|v| v.into_iter().map(String::from).collect()),
            bad_code: None,
            good_code: None,
            severity: None,
            origin: None,
            captured_by_client: None,
        }
    }

    #[test]
    fn content_hash_is_stable_and_input_sensitive() {
        let base = remember_content_hash("**/*.rs", "Title", "Body");
        assert_eq!(base, remember_content_hash("**/*.rs", "Title", "Body"));
        assert_eq!(base.len(), 64);
        assert!(base.chars().all(|c| c.is_ascii_hexdigit()));
        // Any input change must perturb the hash.
        assert_ne!(base, remember_content_hash("**/*.ts", "Title", "Body"));
        assert_ne!(base, remember_content_hash("**/*.rs", "Other", "Body"));
        assert_ne!(base, remember_content_hash("**/*.rs", "Title", "Other"));
    }

    #[tokio::test]
    async fn remember_persists_capture_client() {
        let db = DedupTestEnv::db().await;
        let mut input = remember_input(
            "Caller provenance rule",
            "Remember which client captured this rule.",
            Some(vec!["**/*.rs"]),
        );
        input.captured_by_client = Some(" claude-code ".to_owned());

        let remembered = remember(&db, input).await.unwrap();
        let captured: Option<String> =
            sqlx::query_scalar("SELECT captured_by_client FROM skills WHERE id = ?1")
                .bind(&remembered.skill.id)
                .fetch_one(&db)
                .await
                .unwrap();

        assert_eq!(captured.as_deref(), Some("claude-code"));
    }

    #[tokio::test]
    async fn remember_dedup_window_ignores_cloud_hash_collision() {
        let db = DedupTestEnv::db().await;
        let input = remember_input("Cloud collision", "Same body", Some(vec!["**/*.rs"]));
        let file_patterns_csv = input.file_patterns.as_ref().unwrap().join(",");
        let hash = remember_content_hash(&file_patterns_csv, "Cloud collision", "Same body");
        let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
        let now_ms = chrono::Utc::now().timestamp_millis();
        sqlx::query(
            "INSERT INTO skills \
             (id, name, source, directory, version, description, type, engines, tags, \
              enabled_for_claude, installed_at, updated_at, origin, content_hash, hash_created_at, status) \
             VALUES (?1, ?2, 'cloud', ?3, '1.0.0', ?4, 'review_standard', '[]', '[]', \
                     1, ?5, ?5, 'cloud', ?6, ?7, 'active')",
        )
        .bind("cloud-hash-collision")
        .bind("Cloud collision")
        .bind("safe-cloud-dir")
        .bind("Same body")
        .bind(&now)
        .bind(&hash)
        .bind(now_ms)
        .execute(&db)
        .await
        .unwrap();

        let remembered = remember(&db, input).await.unwrap();
        assert!(!remembered.deduped);
        assert_ne!(remembered.skill.id, "cloud-hash-collision");
    }

    #[tokio::test]
    async fn remember_candidate_dedups_identical_pr_review_across_runs() {
        // Regression: re-running `difflore import-reviews` must not fork a
        // duplicate row for the same comment. Import candidates carry
        // origin = "pr_review", which both conversation-gated dedup guards
        // skip — so before the cross-run content-hash guard a second import
        // inserted an identical row instead of strengthening the first.
        let db = DedupTestEnv::db().await;
        let body = "When touching `src/x.tsx`, use the generic `showError(err)`.";

        let mut first = remember_input(
            "Use the generic showError",
            body,
            Some(vec!["src/**/*.tsx"]),
        );
        first.origin = Some("pr_review".to_owned());
        let mut second = remember_input(
            "Use the generic showError",
            body,
            Some(vec!["src/**/*.tsx"]),
        );
        second.origin = Some("pr_review".to_owned());

        let first_outcome = remember_as_candidate_with_confidence(&db, first, 0.55_f32)
            .await
            .unwrap();
        assert!(
            !first_outcome.deduped,
            "first import must insert a fresh row"
        );

        let second_outcome = remember_as_candidate_with_confidence(&db, second, 0.55_f32)
            .await
            .unwrap();
        assert!(
            second_outcome.deduped,
            "re-importing identical content must dedup, not duplicate"
        );
        assert_eq!(
            second_outcome.skill.id, first_outcome.skill.id,
            "dedup must strengthen the original row, not create a new one"
        );

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM skills")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(count, 1, "no duplicate skills row after re-import");
    }

    #[tokio::test]
    async fn remember_redacts_private_tagged_regions_before_persisting() {
        let db = DedupTestEnv::db().await;
        let mut input = remember_input(
            "Redact private memory",
            "Keep the rule. <private>token=abc</private>",
            Some(vec!["**/*.rs"]),
        );
        input.bad_code = Some("let token = \"<secret>sk-123</secret>\";".to_owned());
        input.good_code = Some("let token = env_token();".to_owned());

        let remembered = remember(&db, input).await.unwrap();

        assert!(
            remembered
                .skill
                .description
                .contains("[redacted private content]")
        );
        assert!(!remembered.skill.description.contains("token=abc"));

        let skill_id = &remembered.skill.id;
        let example = sqlx::query!(
            "SELECT bad_code, good_code FROM rule_examples WHERE skill_id = ?1",
            skill_id,
        )
        .fetch_one(&db)
        .await
        .unwrap();
        let bad_code = example.bad_code;
        let good_code = example.good_code;
        assert!(bad_code.contains("[redacted private content]"));
        assert!(!bad_code.contains("sk-123"));
        assert_eq!(good_code, "let token = env_token();");
    }

    #[tokio::test]
    async fn remember_redacts_raw_secretish_tokens_before_persisting() {
        let db = DedupTestEnv::db().await;
        let input = remember_input(
            "Redact raw secret",
            "Never store token sk-proj-abcdefghijklmnopqrstuvwxyz in review memory.",
            Some(vec!["**/*.rs"]),
        );

        let remembered = remember(&db, input).await.unwrap();

        assert!(
            remembered
                .skill
                .description
                .contains("[redacted private content]")
        );
        assert!(!remembered.skill.description.contains("sk-proj-"));
    }

    #[tokio::test]
    async fn remember_rejects_oversized_body() {
        let db = DedupTestEnv::db().await;
        let err = remember(
            &db,
            remember_input(
                "Huge body",
                &"x".repeat(REMEMBER_BODY_CHAR_LIMIT + 1),
                Some(vec!["**/*.rs"]),
            ),
        )
        .await
        .expect_err("oversized body must be rejected");

        assert!(err.to_string().contains("body"));
    }

    /// Force the previous insert's `hash_created_at` 31s into the past so the
    /// next `remember()` call routes through title/body dedup instead of the
    /// 30s window path.
    async fn age_out_window(db: &sqlx::SqlitePool, skill_id: &str) {
        let stale_ms = chrono::Utc::now().timestamp_millis() - 31_000;
        sqlx::query!(
            "UPDATE skills SET hash_created_at = ?1 WHERE id = ?2",
            stale_ms,
            skill_id
        )
        .execute(db)
        .await
        .unwrap();
    }

    /// Expected post-conditions for the second `remember()` in a dedup test.
    struct DedupExpect {
        deduped: bool,
        window_hit: bool,
        same_id: bool,
        rows: i64,
    }

    struct DedupCase {
        name: &'static str,
        first: (&'static str, &'static str, Vec<&'static str>),
        second: (&'static str, &'static str, Vec<&'static str>),
        age_out_between: bool,
        expect: DedupExpect,
    }

    /// Run a two-step `remember()` scenario and assert the post-conditions
    /// of the second call. Used by every dedup-window test below so the
    /// arrange/act/assert shape stays uniform.
    async fn run_dedup_case(case: DedupCase) {
        let db = DedupTestEnv::db().await;

        let first = remember(
            &db,
            remember_input(case.first.0, case.first.1, Some(case.first.2)),
        )
        .await
        .unwrap();
        assert!(!first.deduped, "[{}] first call must insert", case.name);
        assert!(
            !first.dedup_window_hit,
            "[{}] first call cannot be a window hit",
            case.name
        );
        let first_id = first.skill.id.clone();

        if case.age_out_between {
            age_out_window(&db, &first_id).await;
        }

        let second = remember(
            &db,
            remember_input(case.second.0, case.second.1, Some(case.second.2)),
        )
        .await
        .unwrap();
        assert_eq!(
            second.deduped, case.expect.deduped,
            "[{}] deduped mismatch",
            case.name
        );
        assert_eq!(
            second.dedup_window_hit, case.expect.window_hit,
            "[{}] dedup_window_hit mismatch",
            case.name
        );
        if case.expect.same_id {
            assert_eq!(
                second.skill.id, first_id,
                "[{}] expected same id",
                case.name
            );
        } else {
            assert_ne!(
                second.skill.id, first_id,
                "[{}] expected different id",
                case.name
            );
        }
        if case.expect.window_hit {
            assert!(
                (second.confidence_after - 0.65).abs() < 1e-9,
                "[{}] window hit should bump 0.60 -> 0.65, got {}",
                case.name,
                second.confidence_after
            );
        }
        let row_count =
            sqlx::query_scalar!("SELECT COUNT(*) FROM skills WHERE origin = 'conversation'")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(
            row_count, case.expect.rows,
            "[{}] row count mismatch",
            case.name
        );
    }

    #[tokio::test]
    async fn remember_dedup_window_hit_within_30s_returns_same_id() {
        run_dedup_case(DedupCase {
            name: "window_hit_within_30s_returns_same_id",
            first: ("Window hit rule", "Body text", vec!["**/*.rs"]),
            second: ("Window hit rule", "Body text", vec!["**/*.rs"]),
            age_out_between: false,
            expect: DedupExpect {
                deduped: true,
                window_hit: true,
                same_id: true,
                rows: 1,
            },
        })
        .await;
    }

    #[tokio::test]
    async fn remember_dedup_window_miss_after_31s_inserts_new_row() {
        run_dedup_case(DedupCase {
            name: "window_miss_after_31s_uses_legacy_path",
            first: (
                "Stale window rule",
                "Body text that will age out",
                vec!["**/*.rs"],
            ),
            second: (
                "Stale window rule",
                "Body text that will age out",
                vec!["**/*.rs"],
            ),
            age_out_between: true,
            expect: DedupExpect {
                deduped: true,
                window_hit: false,
                same_id: true,
                rows: 1,
            },
        })
        .await;
    }

    #[tokio::test]
    async fn remember_different_title_same_body_inserts_new_row() {
        run_dedup_case(DedupCase {
            name: "different_title_same_body_inserts_new_row",
            first: ("First title", "Identical body", vec!["**/*.rs"]),
            second: ("Different title", "Identical body", vec!["**/*.rs"]),
            age_out_between: false,
            expect: DedupExpect {
                deduped: false,
                window_hit: false,
                same_id: false,
                rows: 2,
            },
        })
        .await;
    }

    #[tokio::test]
    async fn remember_different_patterns_same_title_and_body_inserts_new_row() {
        run_dedup_case(DedupCase {
            name: "different_patterns_same_title_inserts_new_row",
            first: ("Same title", "Same body", vec!["**/*.rs"]),
            second: ("Same title", "Same body", vec!["**/*.ts"]),
            age_out_between: false,
            expect: DedupExpect {
                deduped: false,
                window_hit: false,
                same_id: false,
                rows: 2,
            },
        })
        .await;
    }

    #[tokio::test]
    async fn remember_same_title_different_body_inserts_new_row_after_window() {
        run_dedup_case(DedupCase {
            name: "same_title_different_body_inserts_new_row_after_window",
            first: (
                "Ambiguous title",
                "Use structured errors in request handlers.",
                vec!["**/*.rs"],
            ),
            second: (
                "Ambiguous title",
                "Avoid allocating large buffers in parser loops.",
                vec!["**/*.rs"],
            ),
            age_out_between: true,
            expect: DedupExpect {
                deduped: false,
                window_hit: false,
                same_id: false,
                rows: 2,
            },
        })
        .await;
    }

    #[tokio::test]
    async fn update_confidence_records_rule_event() {
        let db = DedupTestEnv::db().await;

        let remembered = remember(
            &db,
            remember_input(
                "Feedback event rule",
                "Body text for a durable feedback event",
                Some(vec!["**/*.rs"]),
            ),
        )
        .await
        .unwrap();

        let change = update_confidence(
            &db,
            UpdateConfidenceInput {
                skill_id: remembered.skill.id.clone(),
                signal: "accept".into(),
            },
        )
        .await
        .unwrap();
        assert!((change.before - 0.6).abs() < 1e-9);
        assert!((change.after - 0.65).abs() < 1e-9);

        let row = sqlx::query!(
            "SELECT kind, confidence_before, confidence_after \
             FROM rule_events WHERE skill_id = ?1",
            remembered.skill.id
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(row.kind, "feedback_accept");
        assert!((row.confidence_before.unwrap() - 0.6).abs() < 1e-9);
        assert!((row.confidence_after.unwrap() - 0.65).abs() < 1e-9);
    }

    #[tokio::test]
    async fn update_confidence_rejects_unknown_signal_with_allowed_values() {
        let db = DedupTestEnv::db().await;
        let remembered = remember(
            &db,
            remember_input(
                "Invalid feedback signal rule",
                "Body text for invalid signal coverage",
                Some(vec!["**/*.rs"]),
            ),
        )
        .await
        .unwrap();

        let err = update_confidence(
            &db,
            UpdateConfidenceInput {
                skill_id: remembered.skill.id,
                signal: "maybe".into(),
            },
        )
        .await
        .expect_err("unknown signal must be rejected");

        let msg = err.to_string();
        assert!(msg.contains("accept"), "allowed values missing: {msg}");
        assert!(msg.contains("reject"), "allowed values missing: {msg}");
    }

    #[tokio::test]
    async fn create_local_audits_engine_link_failure() {
        let db = DedupTestEnv::db().await;
        let engine_dir = fs::get_engine_skills_dir("codex")
            .expect("codex skill dir should resolve under shared test home");
        std::fs::create_dir_all(&engine_dir).unwrap();
        let blocking_entry = engine_dir.join("engine-link-audit-rule");
        std::fs::write(&blocking_entry, "not a managed skill link").unwrap();

        let skill = create_local(
            &db,
            CreateLocalSkillInput {
                name: "Engine Link Audit Rule".to_owned(),
                engines: Some(vec!["codex".to_owned()]),
                tags: None,
                description: Some("exercise engine-link audit path".to_owned()),
                r#type: Some("review_standard".to_owned()),
                trigger: None,
                check_prompt: None,
                content: None,
            },
        )
        .await
        .unwrap();

        let row: (String, String, Option<String>) =
            sqlx::query_as("SELECT kind, source, metadata FROM rule_events WHERE skill_id = ?1")
                .bind(skill.id)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(row.0, "engine_link_failed");
        assert_eq!(row.1, "local_rule_create");
        let metadata: serde_json::Value = serde_json::from_str(row.2.as_deref().unwrap()).unwrap();
        assert_eq!(metadata["engine"], "codex");
        assert_eq!(metadata["enabled"], true);
    }

    #[tokio::test]
    async fn create_local_duplicate_does_not_create_skill_directory() {
        let db = DedupTestEnv::db().await;
        let slug = format!("db-only-duplicate-{}", uuid::Uuid::new_v4());
        let id = format!("local-{slug}");
        let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
        sqlx::query(
            "INSERT INTO skills
             (id, name, source, directory, version, description, type, engines, tags,
              enabled_for_claude, installed_at, updated_at, status)
             VALUES (?1, ?2, 'local', ?3, '1.0.0', '', 'review_standard', '[]', '[]',
                     1, ?4, ?4, 'active')",
        )
        .bind(&id)
        .bind(&slug)
        .bind(&slug)
        .bind(&now)
        .execute(&db)
        .await
        .unwrap();

        let skill_dir = fs::skills_base_dir().unwrap().join("local").join(&slug);
        let _ = std::fs::remove_dir_all(&skill_dir);

        let err = create_local(
            &db,
            CreateLocalSkillInput {
                name: slug.clone(),
                engines: Some(vec![]),
                tags: None,
                description: None,
                r#type: Some("review_standard".to_owned()),
                trigger: None,
                check_prompt: None,
                content: None,
            },
        )
        .await
        .expect_err("duplicate DB row should fail before filesystem write");

        assert!(err.to_string().contains("already exists"));
        assert!(
            !skill_dir.exists(),
            "duplicate create_local must not leave an orphan skill directory"
        );
    }

    #[tokio::test]
    async fn remember_confidence_caps_at_conversation_ceiling() {
        // Conversation-channel rules cap at 0.70. A looping agent must not push
        // a rule past that ceiling through either dedup path.
        let db = DedupTestEnv::db().await;

        let first = remember(
            &db,
            remember_input("Cap rule", "Cap body", Some(vec!["**/*.rs"])),
        )
        .await
        .unwrap();
        let first_id = first.skill.id.clone();
        // Fresh insert lands at 0.60 per the conversation default.
        let initial = sqlx::query_scalar!(
            "SELECT confidence_score FROM skills WHERE id = ?1",
            first_id
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert!(
            (initial - 0.60).abs() < 1e-9,
            "fresh capture should start at 0.60, got {initial}"
        );

        assert_confidence_bumps_respect_cap(&db, &first_id, "Cap rule", "Cap body", 20).await;

        // After 20 bumps the rule must be saturated AT the cap, not 1.0.
        let saturated = sqlx::query_scalar!(
            "SELECT confidence_score FROM skills WHERE id = ?1",
            first_id
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert!(
            (saturated - REMEMBER_CONVERSATION_CONFIDENCE_CAP).abs() < 1e-9,
            "after 20 bumps, confidence should saturate at {REMEMBER_CONVERSATION_CONFIDENCE_CAP}, got {saturated}"
        );

        // One more capture after a fresh age-out should still be capped.
        age_out_window(&db, &first_id).await;
        let after_age_out = remember(
            &db,
            remember_input("Cap rule", "Cap body", Some(vec!["**/*.rs"])),
        )
        .await
        .unwrap();
        assert!(
            (after_age_out.confidence_after - REMEMBER_CONVERSATION_CONFIDENCE_CAP).abs() < 1e-9,
            "post-age-out re-capture must stay at the cap, got {}",
            after_age_out.confidence_after
        );
    }

    #[tokio::test]
    async fn remember_captures_today_counts_dedup_bumps() {
        let db = DedupTestEnv::db().await;
        let first = remember(
            &db,
            remember_input(
                "Daily count rule",
                "Daily count body",
                Some(vec!["**/*.rs"]),
            ),
        )
        .await
        .unwrap();
        assert_eq!(first.captures_today, 1);

        let second = remember(
            &db,
            remember_input(
                "Daily count rule",
                "Daily count body",
                Some(vec!["**/*.rs"]),
            ),
        )
        .await
        .unwrap();
        assert!(second.deduped);
        assert_eq!(second.captures_today, 2);

        let counted = count_captures_today(&db, "conversation").await.unwrap();
        assert_eq!(counted, 2);
    }

    /// Re-capture the same rule `iterations` times, alternating between the
    /// window-hit path and title/body dedup so both SQL bump paths are exercised.
    /// Asserts each bump deduped and stayed at or below the cap.
    async fn assert_confidence_bumps_respect_cap(
        db: &sqlx::SqlitePool,
        skill_id: &str,
        title: &str,
        body: &str,
        iterations: usize,
    ) {
        for i in 0..iterations {
            if i % 2 == 1 {
                age_out_window(db, skill_id).await;
            }
            let again = remember(db, remember_input(title, body, Some(vec!["**/*.rs"])))
                .await
                .unwrap();
            assert!(again.deduped, "iteration {i} must dedup, not insert");
            assert!(
                again.confidence_after <= REMEMBER_CONVERSATION_CONFIDENCE_CAP + 1e-9,
                "iteration {i} confidence {} exceeded cap {}",
                again.confidence_after,
                REMEMBER_CONVERSATION_CONFIDENCE_CAP
            );
        }
    }

    #[tokio::test]
    async fn candidate_status_is_excluded_from_mcp_serve_query() {
        let db = DedupTestEnv::db().await;

        // Active rule via plain remember.
        let active = remember(
            &db,
            remember_input("Active rule", "Body A", Some(vec!["**/*.rs"])),
        )
        .await
        .unwrap();

        // Pending rule via remember_as_candidate. Use a different title so
        // the slug-based dedup doesn't merge it into `active`.
        let pending = remember_as_candidate(
            &db,
            remember_input("Pending candidate", "Body B", Some(vec!["**/*.rs"])),
        )
        .await
        .unwrap();
        assert_ne!(active.skill.id, pending.skill.id);

        let pending_status =
            sqlx::query_scalar!("SELECT status FROM skills WHERE id = ?1", pending.skill.id)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(pending_status, "pending");

        let served = crate::context::rule_source::load_rules_from_db(&db)
            .await
            .unwrap();
        let served_ids: std::collections::HashSet<&str> =
            served.iter().map(|r| r.skill_id.as_str()).collect();
        assert!(
            served_ids.contains(active.skill.id.as_str()),
            "active rule must be served by MCP load path"
        );
        assert!(
            !served_ids.contains(pending.skill.id.as_str()),
            "pending candidate MUST NOT be served by MCP load path"
        );

        // list_candidates surfaces the pending row, list() surfaces both.
        let candidates = list_candidates(&db, None, None).await.unwrap();
        assert!(
            candidates.iter().any(|c| c.id == pending.skill.id),
            "pending rule must show up in list_candidates"
        );
        assert!(
            !candidates.iter().any(|c| c.id == active.skill.id),
            "active rule must not show up in list_candidates"
        );

        // Promotion flips the bit.
        let promoted = promote_candidate(&db, &pending.skill.id).await.unwrap();
        assert_eq!(promoted.id, pending.skill.id);
        let post_promote = crate::context::rule_source::load_rules_from_db(&db)
            .await
            .unwrap();
        let post_ids: std::collections::HashSet<&str> =
            post_promote.iter().map(|r| r.skill_id.as_str()).collect();
        assert!(
            post_ids.contains(pending.skill.id.as_str()),
            "after promote, the rule must be served by MCP"
        );

        // Reject removes the row entirely.
        let extra = remember_as_candidate(&db, remember_input("Reject me", "Body C", None))
            .await
            .unwrap();
        reject_candidate(&db, &extra.skill.id).await.unwrap();
        let exists =
            sqlx::query_scalar!("SELECT COUNT(*) FROM skills WHERE id = ?1", extra.skill.id)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(exists, 0, "reject_candidate must delete the row");
    }

    #[tokio::test]
    async fn pending_candidate_repo_filter_uses_source_repo_only() {
        let db = DedupTestEnv::db().await;
        let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
        sqlx::query(
            "INSERT INTO skills
             (id, name, source, directory, version, description, type, engines, tags,
              repo_owner, repo_name, source_repo, enabled_for_claude, installed_at, updated_at, status)
             VALUES
             ('pending-canonical-repo', 'Canonical Repo Candidate', 'local', 'pending-canonical-repo',
              '1.0.0', 'Rule:\nUse canonical source_repo.', 'review_standard', '[]', '[]',
              NULL, NULL, 'acme/widgets', 1, ?1, ?1, 'pending'),
             ('pending-retired-repo-parts', 'Retired Repo Parts Candidate', 'local', 'pending-retired-repo-parts',
              '1.0.0', 'Rule:\nDo not match repo parts.', 'review_standard', '[]', '[]',
              'acme', 'widgets', NULL, 1, ?1, ?1, 'pending')",
        )
        .bind(&now)
        .execute(&db)
        .await
        .unwrap();

        let candidates = list_candidates(&db, Some("ACME/Widgets"), None)
            .await
            .unwrap();
        let ids: std::collections::HashSet<&str> = candidates
            .iter()
            .map(|candidate| candidate.id.as_str())
            .collect();
        assert!(ids.contains("pending-canonical-repo"));
        assert!(
            !ids.contains("pending-retired-repo-parts"),
            "repo_owner/repo_name must not satisfy the canonical source_repo filter"
        );
        assert_eq!(
            count_pending_candidates(&db, Some("ACME/Widgets"))
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            candidates
                .iter()
                .find(|candidate| candidate.id == "pending-canonical-repo")
                .and_then(|candidate| candidate.source_repo.as_deref()),
            Some("acme/widgets")
        );
    }

    #[tokio::test]
    async fn remember_as_candidate_with_confidence_seeds_and_clamps_score() {
        let db = DedupTestEnv::db().await;

        // A 0.65 capture confidence is seeded verbatim onto the pending row,
        // replacing the flat 0.6 import default.
        let mid = remember_as_candidate_with_confidence(
            &db,
            remember_input("Seeded mid rule", "Body mid", Some(vec!["**/*.rs"])),
            0.65,
        )
        .await
        .unwrap();
        let mid_id = mid.skill.id.clone();
        let mid_conf: f64 =
            sqlx::query_scalar!("SELECT confidence_score FROM skills WHERE id = ?1", mid_id)
                .fetch_one(&db)
                .await
                .unwrap();
        assert!(
            (mid_conf - 0.65).abs() < 1e-6,
            "seed confidence should be applied verbatim, got {mid_conf}"
        );
        let mid_status = sqlx::query_scalar!("SELECT status FROM skills WHERE id = ?1", mid_id)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(mid_status, "pending");

        // An over-cap seed (0.95) is clamped to the conversation ceiling so an
        // import can't seed past manually-curated parity.
        let high = remember_as_candidate_with_confidence(
            &db,
            remember_input("Seeded high rule", "Body high", Some(vec!["**/*.ts"])),
            0.95,
        )
        .await
        .unwrap();
        let high_id = high.skill.id.clone();
        let high_conf: f64 =
            sqlx::query_scalar!("SELECT confidence_score FROM skills WHERE id = ?1", high_id)
                .fetch_one(&db)
                .await
                .unwrap();
        assert!(
            (high_conf - REMEMBER_CONVERSATION_CONFIDENCE_CAP).abs() < 1e-6,
            "over-cap seed should clamp to {REMEMBER_CONVERSATION_CONFIDENCE_CAP}, got {high_conf}"
        );
    }

    #[tokio::test]
    async fn promoting_review_candidate_records_structured_source_proof() {
        let db = DedupTestEnv::db().await;
        let pending = remember_as_candidate(
            &db,
            remember_input(
                "Prefer structured API parsing",
                "Imported from a GitHub PR review comment. Keep as a pending candidate until a human confirms this is a repeatable review rule.\n\n\
                 Source: tanstack/router#42\n\
                 Comment: https://github.com/tanstack/router/pull/42#discussion_r1\n\
                 File: packages/router/src/parser.ts\n\n\
                 Reviewer said:\n\
                 Please avoid regex parsing here; use the structured parser so nested routes keep working.",
                Some(vec!["packages/router/src/parser.ts"]),
            ),
        )
        .await
        .unwrap();

        promote_candidate(&db, &pending.skill.id).await.unwrap();

        let pending_id = &pending.skill.id;
        let row = sqlx::query!(
            "SELECT kind, source, reason, metadata FROM rule_events \
             WHERE skill_id = ?1 AND kind = 'source_proof'",
            pending_id,
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(row.kind, "source_proof");
        assert_eq!(row.source.as_str(), "candidate_promotion");
        assert!(
            row.reason
                .as_deref()
                .unwrap_or("")
                .contains("tanstack/router#42")
        );
        let metadata: serde_json::Value =
            serde_json::from_str(row.metadata.as_deref().unwrap_or("")).unwrap();
        let proof = &metadata["sourceProof"];
        assert_eq!(proof["source"], "tanstack/router#42");
        assert_eq!(
            proof["commentUrl"],
            "https://github.com/tanstack/router/pull/42#discussion_r1"
        );
        assert_eq!(proof["file"], "packages/router/src/parser.ts");
        assert!(
            proof["excerpt"]
                .as_str()
                .unwrap()
                .contains("structured parser")
        );
    }

    #[tokio::test]
    async fn pending_candidates_are_excluded_from_skills_list() {
        let db = DedupTestEnv::db().await;

        let active = remember(
            &db,
            remember_input("Active list rule", "Body A", Some(vec!["**/*.rs"])),
        )
        .await
        .unwrap();
        let pending = remember_as_candidate(
            &db,
            remember_input("Pending list rule", "Body B", Some(vec!["**/*.rs"])),
        )
        .await
        .unwrap();
        assert_ne!(active.skill.id, pending.skill.id);

        let listed = list(&db).await.unwrap();
        let listed_ids: std::collections::HashSet<&str> =
            listed.iter().map(|s| s.id.as_str()).collect();
        assert!(
            listed_ids.contains(active.skill.id.as_str()),
            "active rule must show up in skills::list()"
        );
        assert!(
            !listed_ids.contains(pending.skill.id.as_str()),
            "pending candidate MUST NOT show up in skills::list()"
        );

        // list_all returns both — that's the candidates-aware variant.
        let all = list_all(&db).await.unwrap();
        let all_ids: std::collections::HashSet<&str> = all.iter().map(|s| s.id.as_str()).collect();
        assert!(all_ids.contains(active.skill.id.as_str()));
        assert!(
            all_ids.contains(pending.skill.id.as_str()),
            "pending candidate MUST show up in skills::list_all()"
        );
    }

    #[tokio::test]
    async fn pending_candidates_are_excluded_from_origin_count() {
        let db = DedupTestEnv::db().await;

        let _active = remember(
            &db,
            remember_input("Active origin rule", "Body A", Some(vec!["**/*.rs"])),
        )
        .await
        .unwrap();
        let _pending = remember_as_candidate(
            &db,
            remember_input("Pending origin rule", "Body B", Some(vec!["**/*.rs"])),
        )
        .await
        .unwrap();

        let s = stats(&db).await.unwrap();
        assert_eq!(s.total, 1, "stats.total must exclude pending candidates");
        let conv_count: i64 = s
            .by_origin
            .iter()
            .find(|o| o.origin == "conversation")
            .map_or(0, |o| o.count);
        assert_eq!(
            conv_count, 1,
            "by_origin conversation count must exclude pending candidates"
        );

        // corpus_health (the doctor / rules-explain stats helper) must
        // also exclude pending — the dashboard's "growth" view leaked
        // candidates before this fix.
        let h = crate::infra::db::corpus_health(&db).await.unwrap();
        assert_eq!(h.total, 1, "corpus_health.total must exclude pending");
        let conv_corpus = h
            .by_origin
            .iter()
            .find(|(origin, _)| origin == "conversation")
            .map_or(0, |(_, n)| *n);
        assert_eq!(
            conv_corpus, 1,
            "corpus_health by_origin conversation must exclude pending"
        );
    }
}
