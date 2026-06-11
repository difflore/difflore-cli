use openapi_contract::api;

use crate::cloud::client::CloudClient;
use crate::contract::{Extraction, InviteResult, Success, Team, TeamMember, TeamRuleSummary};
use crate::domain::models::SkillRecord;
use crate::error::CoreError;

use super::cloud_id::{
    ensure_cloud_rule_id, resolve_cloud_rule_id_for_unpublish, resolve_existing_cloud_rule_id,
};
use super::types::{
    ReviewInboxItem, TeamContextInput, TeamInviteInput, TeamInviteResult, TeamMemberIdInput,
    TeamMemberRecord, TeamMembersResult, TeamRulePublishInput, TeamRuleUnpublishInput,
    TeamSkillsResult, TeamUpdateRoleInput,
};

async fn resolve_team_id(
    client: &CloudClient,
    explicit: Option<String>,
) -> crate::Result<(String, bool)> {
    if let Some(id) = explicit {
        return Ok((id, false));
    }
    let team: Option<Team> = api!(GET "/teams/my").fetch(client).await.ok().flatten();
    match team {
        Some(t) => Ok((t.id, true)),
        // No team on the current cloud account; name the situation and
        // point at the surface that creates one.
        None => Err(CoreError::NotFound(
            "no team for the current cloud account. Create or join one at \
             difflore.dev/team, then retry."
                .into(),
        )),
    }
}

pub async fn members(input: TeamContextInput) -> crate::Result<TeamMembersResult> {
    let client = CloudClient::create().await;
    if !client.is_logged_in() {
        return Ok(TeamMembersResult {
            members: vec![],
            default_team_used: false,
        });
    }
    let (team_id, default_team_used) = resolve_team_id(&client, input.team_id).await?;
    let members: Vec<TeamMember> = api!(GET "/teams/{id}/members", id = &team_id)
        .fetch(&client)
        .await?;
    Ok(TeamMembersResult {
        members: members.into_iter().map(TeamMemberRecord::from).collect(),
        default_team_used,
    })
}

pub async fn invite(input: TeamInviteInput) -> crate::Result<TeamInviteResult> {
    let client = CloudClient::create().await;
    if !client.is_logged_in() {
        return Err(CoreError::Internal(
            "not logged in to cloud. Run `difflore cloud login` first.".into(),
        ));
    }
    let (team_id, default_team_used) = resolve_team_id(&client, input.team_id).await?;
    let body = serde_json::json!({
        "email": input.email,
        "role": input.role.unwrap_or_else(|| "member".into()),
    });
    let result: InviteResult = api!(POST "/teams/{id}/invite", id = &team_id, body = &body)
        .fetch(&client)
        .await?;
    Ok(TeamInviteResult {
        id: result.id,
        default_team_used,
    })
}

pub async fn remove_member(input: TeamMemberIdInput) -> crate::Result<()> {
    let client = CloudClient::create().await;
    if !client.is_logged_in() {
        return Err(CoreError::Internal(
            "not logged in to cloud. Run `difflore cloud login` first.".into(),
        ));
    }
    let (team_id, _) = resolve_team_id(&client, input.team_id).await?;
    let _: Success =
        api!(DELETE "/teams/{id}/members/{userId}", id = &team_id, userId = &input.user_id)
            .fetch(&client)
            .await?;
    Ok(())
}

pub async fn update_role(input: TeamUpdateRoleInput) -> crate::Result<()> {
    let client = CloudClient::create().await;
    if !client.is_logged_in() {
        return Err(CoreError::Internal(
            "not logged in to cloud. Run `difflore cloud login` first.".into(),
        ));
    }
    let (team_id, _) = resolve_team_id(&client, input.team_id).await?;
    let body = serde_json::json!({ "role": input.role });
    let _: Success = api!(PUT "/teams/{id}/members/{userId}/role", id = &team_id, userId = &input.user_id, body = &body)
        .fetch(&client)
        .await?;
    Ok(())
}

pub async fn skills(input: TeamContextInput) -> crate::Result<TeamSkillsResult> {
    let client = CloudClient::create().await;
    if !client.is_logged_in() {
        return Ok(TeamSkillsResult {
            skills: vec![],
            default_team_used: false,
        });
    }
    let (_team_id, default_team_used) = resolve_team_id(&client, input.team_id).await?;
    // /rules/team returns team rules for the current user (no team_id param needed)
    let rules_json: Vec<serde_json::Value> = api!(GET "/rules/team").fetch(&client).await?;
    let rules: Vec<TeamRuleSummary> = rules_json
        .into_iter()
        .map(serde_json::from_value)
        .collect::<Result<_, _>>()?;
    let skills = rules
        .into_iter()
        .map(|r| SkillRecord {
            id: r.id,
            name: r.name,
            description: r.description,
            r#type: r.r#type,
            version: r.version,
            engines: r.engines,
            tags: r.tags,
            trigger: r.trigger,
            check_prompt: r.check_prompt,
            directory: String::new(),
            source: "team".into(),
            repo_owner: None,
            repo_name: None,
            repo_branch: None,
            readme_url: None,
            enabled_for_codex: false,
            enabled_for_claude: false,
            enabled_for_gemini: false,
            enabled_for_cursor: false,
            installed_at: r.created_at.clone(),
            updated_at: r.updated_at,
            enforcement: r
                .published_in_teams
                .first()
                .and_then(|entry| entry.get("enforcement"))
                .and_then(|v| v.as_str())
                .map(ToOwned::to_owned),
            origin: "team".into(),
        })
        .collect();
    Ok(TeamSkillsResult {
        skills,
        default_team_used,
    })
}

/// Resolve a local/conversation rule id to an already-known cloud UUID.
///
/// This is intentionally read-only: unlike team publishing, it never creates a
/// cloud rule row. Callers should treat `Ok(None)` as "do not attribute".
pub async fn resolve_known_cloud_rule_id(
    pool: &sqlx::SqlitePool,
    rule_id: &str,
) -> crate::Result<Option<String>> {
    resolve_existing_cloud_rule_id(pool, rule_id).await
}

pub async fn publish_rule(input: TeamRulePublishInput) -> crate::Result<String> {
    let client = CloudClient::create().await;
    if !client.is_logged_in() {
        return Err(CoreError::Internal(
            "not logged in to cloud. Run `difflore cloud login` first.".into(),
        ));
    }

    let (team_id, _) = resolve_team_id(&client, input.team_id).await?;
    let pool = crate::infra::db::init_db()
        .await
        .map_err(CoreError::Internal)?;

    if let Some(s) = crate::skills::rule_status(&pool, &input.rule_id).await?
        && s == "pending"
    {
        return Err(CoreError::Validation(format!(
            "rule '{}' is a pending memory draft. Run `difflore status` before publishing it to a team.",
            input.rule_id,
        )));
    }

    let cloud_rule_id = ensure_cloud_rule_id(&pool, &client, &input.rule_id).await?;

    // Look up local origin when the caller didn't pass one. Keyed by the
    // cloud uuid, which is the row's current id after `ensure_cloud_rule_id`.
    let origin = match input.origin {
        Some(o) => Some(o),
        None => sqlx::query_scalar!("SELECT origin FROM skills WHERE id = ?1", cloud_rule_id)
            .fetch_optional(&pool)
            .await
            .ok()
            .flatten(),
    };
    let body = serde_json::json!({
        "ruleId": cloud_rule_id,
        "teamId": team_id,
        "enforcement": input.enforcement.unwrap_or_else(|| "recommended".into()),
        "origin": origin,
    });
    let _: Success = api!(POST "/rules/team/publish", body = &body)
        .fetch(&client)
        .await?;
    Ok(cloud_rule_id)
}

pub async fn unpublish_rule(input: TeamRuleUnpublishInput) -> crate::Result<()> {
    let client = CloudClient::create().await;
    if !client.is_logged_in() {
        return Err(CoreError::Internal(
            "not logged in to cloud. Run `difflore cloud login` first.".into(),
        ));
    }

    let (team_id, _) = resolve_team_id(&client, input.team_id).await?;
    let pool = crate::infra::db::init_db()
        .await
        .map_err(CoreError::Internal)?;
    let cloud_rule_id = resolve_cloud_rule_id_for_unpublish(&pool, &input.rule_id).await?;
    let body = serde_json::json!({
        "ruleId": cloud_rule_id,
        "teamId": team_id,
    });
    let _: Success = api!(POST "/rules/team/unpublish", body = &body)
        .fetch(&client)
        .await?;
    Ok(())
}

pub async fn review_inbox(limit: usize) -> crate::Result<Vec<ReviewInboxItem>> {
    let client = CloudClient::create().await;
    if !client.is_logged_in() {
        return Ok(vec![]);
    }

    let rows: Vec<Extraction> = api!(GET "/reviews/extractions/recent")
        .fetch(&client)
        .await?;

    let items = rows
        .into_iter()
        .take(limit)
        .map(|r| ReviewInboxItem {
            id: r.id,
            knowledge_type: r.knowledge_type,
            title: r.title,
            content: r.content,
            confidence: r.confidence.unwrap_or(0.0),
            status: r.status,
            file_patterns: r.file_patterns.unwrap_or_default(),
            created_at: r.created_at,
        })
        .collect();

    Ok(items)
}
