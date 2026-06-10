mod api;
mod cloud_id;
mod types;

pub use api::{
    invite, members, publish_rule, remove_member, resolve_known_cloud_rule_id, review_inbox,
    skills, unpublish_rule, update_role,
};
pub use types::{
    ReviewInboxItem, TeamContextInput, TeamInviteInput, TeamInviteResult, TeamMemberIdInput,
    TeamMemberRecord, TeamMembersResult, TeamRulePublishInput, TeamRuleUnpublishInput,
    TeamSkillsResult, TeamUpdateRoleInput,
};

#[cfg(test)]
mod tests {
    use super::cloud_id::{
        build_rule_create_body, resolve_cloud_rule_id_for_unpublish, rule_cloud_mapping_key,
    };
    use super::types::{LocalRuleUploadRow, TeamRulePublishInput};
    use sqlx::SqlitePool;
    use uuid::Uuid;

    async fn setup_migrated_pool() -> SqlitePool {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("open in-memory pool");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("apply migrations");
        pool
    }

    #[test]
    fn rule_create_body_preserves_local_origin() {
        let row = LocalRuleUploadRow {
            name: "Prefer structured logs".into(),
            rule_type: "review_standard".into(),
            description: "Use logger.info instead of println.".into(),
            version: "1.0.0".into(),
            engines_json: r#"["claude"]"#.into(),
            tags_json: r#"["conversation"]"#.into(),
            trigger: None,
            check_prompt: Some("Check logging calls".into()),
            file_patterns_json: Some(r#"["**/*.rs"]"#.into()),
            origin: "conversation".into(),
            source_repo: Some("acme/widgets".into()),
        };

        let body = build_rule_create_body(&row);
        assert_eq!(body["origin"].as_str(), Some("conversation"));
        assert_eq!(body["content"].as_str(), Some(row.description.as_str()));
        assert_eq!(body["visibility"].as_str(), Some("team"));
        assert_eq!(body["filePatterns"][0].as_str(), Some("**/*.rs"));
        assert_eq!(body["sourceRepo"].as_str(), Some("acme/widgets"));
    }

    #[test]
    fn rule_create_body_falls_back_to_name_for_empty_content() {
        let row = LocalRuleUploadRow {
            name: "Name only".into(),
            rule_type: "skill".into(),
            description: "  ".into(),
            version: "1.0.0".into(),
            engines_json: "[]".into(),
            tags_json: "[]".into(),
            trigger: None,
            check_prompt: None,
            file_patterns_json: None,
            origin: "manual".into(),
            source_repo: None,
        };

        let body = build_rule_create_body(&row);
        assert_eq!(body["content"].as_str(), Some("Name only"));
        assert_eq!(body["origin"].as_str(), Some("manual"));
    }

    #[tokio::test]
    async fn unpublish_resolves_slug_from_auth_mapping() {
        let pool = setup_migrated_pool().await;
        let cloud_id = Uuid::new_v4().to_string();
        let key = rule_cloud_mapping_key("conv-example-12345678");
        sqlx::query!(
            "INSERT INTO auth (key, value) VALUES (?1, ?2)",
            key,
            cloud_id
        )
        .execute(&pool)
        .await
        .expect("seed auth mapping");

        let resolved = resolve_cloud_rule_id_for_unpublish(&pool, "conv-example-12345678")
            .await
            .expect("resolve cloud rule id");
        assert_eq!(resolved, cloud_id);
    }

    #[tokio::test]
    async fn unpublish_resolves_slug_from_cloud_id_column_when_present() {
        let pool = setup_migrated_pool().await;
        let cloud_id = Uuid::new_v4().to_string();
        sqlx::query!(
            "INSERT INTO skills (id, name, source, directory, version, cloud_id) \
             VALUES (?1, 'n', 's', 'd', '1.0.0', ?2)",
            "local-example",
            cloud_id
        )
        .execute(&pool)
        .await
        .expect("seed skill row");

        let resolved = resolve_cloud_rule_id_for_unpublish(&pool, "local-example")
            .await
            .expect("resolve via cloud_id column");
        assert_eq!(resolved, cloud_id);
    }

    #[tokio::test]
    async fn unpublish_missing_slug_mapping_is_not_found() {
        let pool = setup_migrated_pool().await;

        let err = resolve_cloud_rule_id_for_unpublish(&pool, "conv-missing-12345678")
            .await
            .expect_err("expected NotFound");
        assert!(
            err.to_string().contains("publish"),
            "unexpected error: {err}"
        );
    }

    // `origin` must travel up on the wire so the cloud Dashboard sees the
    // input-channel provenance of published rules.
    #[test]
    fn team_rule_publish_input_includes_origin_on_wire() {
        let input = TeamRulePublishInput {
            rule_id: "rule-1".into(),
            enforcement: Some("required".into()),
            team_id: Some("t1".into()),
            origin: Some("conversation".into()),
        };
        let json = serde_json::to_value(&input).unwrap();
        assert_eq!(json.get("ruleId").and_then(|v| v.as_str()), Some("rule-1"));
        assert_eq!(
            json.get("origin").and_then(|v| v.as_str()),
            Some("conversation")
        );
    }
}
