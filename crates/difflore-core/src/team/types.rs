use crate::cloud::api_types;
use crate::models::SkillRecord;

/// Local row fields needed to seed a fresh cloud rule when uploading a
/// local-only capture (conversation/manual) for the first time. Mirrors
/// the cloud `POST /rules` input schema (`orpc/rules.ts:create`).
#[derive(sqlx::FromRow)]
pub(super) struct LocalRuleUploadRow {
    pub(super) name: String,
    pub(super) rule_type: String,
    pub(super) description: String,
    pub(super) version: String,
    pub(super) engines_json: String,
    pub(super) tags_json: String,
    pub(super) trigger: Option<String>,
    pub(super) check_prompt: Option<String>,
    pub(super) file_patterns_json: Option<String>,
    pub(super) origin: String,
    pub(super) source_repo: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamMemberRecord {
    pub user_id: String,
    pub name: Option<String>,
    pub email: String,
    pub role: String,
    pub joined_at: String,
}

impl From<api_types::TeamMember> for TeamMemberRecord {
    fn from(m: api_types::TeamMember) -> Self {
        Self {
            user_id: m.user_id,
            name: Some(m.name),
            email: m.email,
            role: m.role,
            joined_at: m.joined_at,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamInviteInput {
    pub email: String,
    pub role: Option<String>,
    pub team_id: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamInviteResult {
    pub id: String,
    #[serde(default)]
    pub default_team_used: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamMemberIdInput {
    pub user_id: String,
    pub team_id: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamUpdateRoleInput {
    pub user_id: String,
    pub role: String,
    pub team_id: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamContextInput {
    pub team_id: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamMembersResult {
    pub members: Vec<TeamMemberRecord>,
    pub default_team_used: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamSkillsResult {
    pub skills: Vec<SkillRecord>,
    pub default_team_used: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamRulePublishInput {
    pub rule_id: String,
    pub enforcement: Option<String>,
    pub team_id: Option<String>,
    /// 2026-04-20: input-channel provenance forwarded to the cloud so
    /// the team Dashboard can show "this rule started life as a
    /// conversation capture" instead of just "manual". Optional —
    /// `publish_rule` will look it up from the local DB when omitted.
    #[serde(default)]
    pub origin: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamRuleUnpublishInput {
    pub rule_id: String,
    pub team_id: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewInboxItem {
    pub id: String,
    pub knowledge_type: String,
    pub title: String,
    pub content: String,
    pub confidence: f64,
    pub status: String,
    pub file_patterns: Vec<String>,
    pub created_at: String,
}
