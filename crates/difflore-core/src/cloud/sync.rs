use openapi_contract::{ApiClient, Method, api};
use serde::{Deserialize, Serialize};
use sha2::Digest;

use super::api_types::{
    BillingCurrent, Success, SyncProviders, SyncSettings, Team, TeamRuleSummary, UserProfile,
};
use super::client::CloudClient;
use crate::models::SkillRecord;
use crate::skills::fs::skills_base_dir;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncResult {
    pub created: Vec<SyncedRule>,
    pub updated: Vec<SyncedRule>,
    pub deleted: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamSyncResult {
    pub visible_count: i32,
    pub synced: SyncResult,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncedRule {
    pub id: String,
    pub name: String,
    pub r#type: String,
    pub description: String,
    pub version: String,
    pub engines: Vec<String>,
    pub tags: Vec<String>,
    pub trigger: Option<String>,
    pub check_prompt: Option<String>,
    pub content: String,
    pub updated_at: String,
    pub created_at: String,
    /// Glob list (e.g. `["**/*.rs"]`) the CLI's cascade uses to drop rules
    /// whose patterns don't match the current file. Empty / missing = universal rule.
    #[serde(default)]
    pub file_patterns: Vec<String>,
    /// Input-channel provenance (manual | conversation | `pr_review` |
    /// extracted). Defaults to None when the cloud doesn't emit it, so
    /// `apply_sync_result` falls back to `cloud`.
    #[serde(default)]
    pub origin: Option<String>,
    /// GitHub-style `owner/repo` provenance for the extractions that drafted
    /// this rule. Mirrors cloud's `rules_cloud.source_repo`.
    #[serde(default, rename = "sourceRepo")]
    pub source_repo: Option<String>,
}

impl SyncResult {
    pub const fn created_count(&self) -> usize {
        self.created.len()
    }
    pub const fn updated_count(&self) -> usize {
        self.updated.len()
    }
    pub const fn deleted_count(&self) -> usize {
        self.deleted.len()
    }
}

pub async fn sync_skills(
    client: &CloudClient,
    skills: &[SkillRecord],
) -> Result<Option<SyncResult>, crate::CoreError> {
    sync_skills_filtered(client, skills, &[]).await
}

/// Like `sync_skills` but filters out local-only ids before hashing so
/// pending candidates are not recreated as active cloud rules.
pub async fn sync_skills_filtered(
    client: &CloudClient,
    skills: &[SkillRecord],
    exclude_ids: &[String],
) -> Result<Option<SyncResult>, crate::CoreError> {
    let exclude: std::collections::HashSet<&str> = exclude_ids.iter().map(String::as_str).collect();
    let local_hashes: std::collections::HashMap<String, String> = skills
        .iter()
        .filter(|s| !exclude.contains(s.id.as_str()))
        .map(|skill| (skill.id.clone(), skill_content_hash(skill)))
        .collect();
    let payload = serde_json::json!({ "localHashes": local_hashes });

    let resp = client
        .request(Method::POST, "/rules/sync", None, Some(payload.to_string()))
        .await?;
    let status = resp.status();
    if !status.is_success() {
        return Err(crate::CoreError::Internal(format!(
            "rules sync returned {status}; run `difflore doctor --report` for cloud diagnostics"
        )));
    }
    let result: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| crate::CoreError::Internal(format!("rules sync decode error: {e}")))?;

    let created = result
        .get("created")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let updated = result
        .get("updated")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let deleted: Vec<String> = result
        .get("deleted")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    // Fail the whole sync (before any local DB mutation) if the cloud sent a
    // malformed rule — never partially apply a junk / empty-id rule.
    let created = created
        .iter()
        .map(map_synced_rule_value)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| {
            crate::CoreError::Internal(format!("rules sync: malformed `created` rule: {e}"))
        })?;
    let updated = updated
        .iter()
        .map(map_synced_rule_value)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| {
            crate::CoreError::Internal(format!("rules sync: malformed `updated` rule: {e}"))
        })?;
    if deleted.iter().any(|id| id.trim().is_empty()) {
        return Err(crate::CoreError::Internal(
            "rules sync: `deleted` contained an empty rule id".to_owned(),
        ));
    }

    Ok(Some(SyncResult {
        created,
        updated,
        deleted,
    }))
}

fn map_synced_rule_value(val: &serde_json::Value) -> Result<SyncedRule, String> {
    // `id` and `content` are REQUIRED — a missing/empty value would create or
    // overwrite a local rule keyed on an empty id (corrupting the store). Cloud
    // schema drift must fail the whole sync, never silently apply a junk rule.
    let required = |key: &str| -> Result<String, String> {
        match val.get(key).and_then(|v| v.as_str()) {
            Some(s) if !s.trim().is_empty() => Ok(s.to_owned()),
            _ => Err(format!("missing or empty required field `{key}`")),
        }
    };
    Ok(SyncedRule {
        id: required("id")?,
        name: val
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_owned(),
        r#type: val
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("review_standard")
            .to_owned(),
        description: val
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_owned(),
        version: val
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("1.0.0")
            .to_owned(),
        engines: val
            .get("engines")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default(),
        tags: val
            .get("tags")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default(),
        trigger: val
            .get("trigger")
            .and_then(|v| v.as_str())
            .map(String::from),
        check_prompt: val
            .get("checkPrompt")
            .and_then(|v| v.as_str())
            .map(String::from),
        content: required("content")?,
        updated_at: val
            .get("updatedAt")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_owned(),
        created_at: val
            .get("createdAt")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_owned(),
        file_patterns: val
            .get("filePatterns")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default(),
        origin: val.get("origin").and_then(|v| v.as_str()).map(String::from),
        source_repo: val
            .get("sourceRepo")
            .and_then(|v| v.as_str())
            .map(String::from),
    })
}

pub async fn sync_team_skills(client: &CloudClient) -> Result<TeamSyncResult, crate::CoreError> {
    let skills_json: Vec<serde_json::Value> = api!(GET "/rules/team").fetch(client).await?;
    let skills: Vec<TeamRuleSummary> = skills_json
        .into_iter()
        .map(serde_json::from_value)
        .collect::<Result<_, _>>()?;
    let visible_count = i32::try_from(skills.len()).unwrap_or(i32::MAX);
    let created = skills
        .into_iter()
        .map(|rule| SyncedRule {
            id: rule.id,
            name: rule.name,
            r#type: rule.r#type,
            description: rule.description.clone(),
            version: rule.version,
            engines: rule.engines,
            tags: rule.tags,
            trigger: rule.trigger,
            check_prompt: rule.check_prompt,
            content: rule.description,
            updated_at: rule.updated_at,
            created_at: rule.created_at,
            file_patterns: rule.file_patterns,
            origin: Some("team".to_owned()),
            source_repo: rule.source_repo,
        })
        .collect();
    Ok(TeamSyncResult {
        visible_count,
        synced: SyncResult {
            created,
            updated: vec![],
            deleted: vec![],
        },
    })
}

pub async fn sync_settings(
    client: &CloudClient,
    settings: &serde_json::Value,
) -> Result<(), crate::CoreError> {
    let payload = serde_json::json!({ "settings": settings });
    let _: Success = api!(PUT "/sync/settings", body = &payload)
        .fetch(client)
        .await?;
    Ok(())
}

/// Mask an API key for cross-device sync. The cloud never stores the secret —
/// only an opaque hint (e.g. last 4 chars) so users can recognize which key
/// they previously used on another device.
pub fn mask_api_key(key: &str) -> String {
    let trimmed = key.trim();
    if trimmed.len() <= 4 {
        "•".repeat(trimmed.len())
    } else {
        let visible = &trimmed[trimmed.len().saturating_sub(4)..];
        format!("••••{visible}")
    }
}

/// Build a structured provider sync payload from local provider records.
/// Used by `sync_providers`; exposed for callers that want to inspect what
/// will be sent before pushing.
pub fn build_provider_sync_entries(
    providers: &[crate::models::ProviderRecord],
) -> Vec<serde_json::Value> {
    providers
        .iter()
        .map(|p| {
            let mut obj = serde_json::Map::new();
            obj.insert("name".into(), serde_json::Value::String(p.name.clone()));
            obj.insert(
                "baseUrl".into(),
                serde_json::Value::String(p.base_url.clone()),
            );
            if let Some(key) = p.api_key.as_deref() {
                obj.insert(
                    "maskedKey".into(),
                    serde_json::Value::String(mask_api_key(key)),
                );
            }
            if !p.model_mapping.is_empty() {
                obj.insert(
                    "modelMapping".into(),
                    serde_json::to_value(&p.model_mapping).unwrap_or(serde_json::Value::Null),
                );
            }
            obj.insert(
                "updatedAt".into(),
                serde_json::Value::String(p.updated_at.clone()),
            );
            serde_json::Value::Object(obj)
        })
        .collect()
}

pub async fn sync_providers(
    client: &CloudClient,
    providers: &[serde_json::Value],
) -> Result<(), crate::CoreError> {
    let payload = serde_json::json!({ "providers": providers });
    let _: Success = api!(PUT "/sync/providers", body = &payload)
        .fetch(client)
        .await?;
    Ok(())
}

/// Fetch cloud-side settings blob. Returns (`settings_value`, `updated_at`) or
/// None if the cloud has no settings stored yet for this user.
pub async fn pull_settings(
    client: &CloudClient,
) -> Result<Option<(serde_json::Value, Option<String>)>, crate::CoreError> {
    let result: SyncSettings = api!(GET "/sync/settings").fetch(client).await?;
    let val = serde_json::to_value(&result).unwrap_or_default();
    let settings = val
        .get("settings")
        .cloned()
        .unwrap_or(serde_json::json!({}));
    let updated_at = val
        .get("updatedAt")
        .and_then(|v| v.as_str())
        .map(String::from);
    if settings.is_null() || settings.as_object().is_none_or(serde_json::Map::is_empty) {
        Ok(None)
    } else {
        Ok(Some((settings, updated_at)))
    }
}

/// Fetch cloud-side providers as a structured array. Returns the parsed JSON
/// array and optional `updated_at`, or None if not set. Cloud holds masked keys
/// only — callers must NOT trust `maskedKey` as a usable secret.
pub async fn pull_providers(
    client: &CloudClient,
) -> Result<Option<(serde_json::Value, Option<String>)>, crate::CoreError> {
    let result: SyncProviders = api!(GET "/sync/providers").fetch(client).await?;
    let val = serde_json::to_value(&result).unwrap_or_default();
    let providers = val.get("providers").cloned();
    let updated_at = val
        .get("updatedAt")
        .and_then(|v| v.as_str())
        .map(String::from);
    Ok(normalize_provider_payload(providers).map(|providers| (providers, updated_at)))
}

fn normalize_provider_payload(providers: Option<serde_json::Value>) -> Option<serde_json::Value> {
    match providers {
        Some(arr @ serde_json::Value::Array(_)) => Some(arr),
        None | Some(_) => None,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudStatus {
    pub logged_in: bool,
    pub email: Option<String>,
    pub plan: Option<String>,
    pub team_id: Option<String>,
    pub team_name: Option<String>,
}

/// Fetch the user's cloud status (profile + billing plan + active team).
/// Safe to call when not logged in — returns a `logged_in: false` response.
pub async fn fetch_cloud_status(client: &CloudClient) -> CloudStatus {
    if !client.is_logged_in() {
        return CloudStatus {
            logged_in: false,
            email: None,
            plan: None,
            team_id: None,
            team_name: None,
        };
    }

    let mut status_client = client.clone();
    let mut profile_result: Result<UserProfile, _> = api!(GET "/auth/profile").fetch(client).await;
    if profile_result.is_err() && CloudClient::refresh_saved_token().await.is_some() {
        status_client = CloudClient::create().await;
        profile_result = api!(GET "/auth/profile").fetch(&status_client).await;
    }
    let Ok(profile) = profile_result else {
        return CloudStatus {
            logged_in: false,
            email: None,
            plan: None,
            team_id: None,
            team_name: None,
        };
    };

    let email = serde_json::to_value(&profile)
        .ok()
        .and_then(|v| v.get("email").and_then(|e| e.as_str()).map(String::from));

    let billing_result: Result<BillingCurrent, _> =
        api!(GET "/billing/current").fetch(&status_client).await;
    let plan = billing_result
        .ok()
        .and_then(|b| serde_json::to_value(&b).ok())
        .and_then(|v| v.get("planId").and_then(|p| p.as_str()).map(String::from));

    let team_result: Result<Option<Team>, _> = api!(GET "/teams/my").fetch(&status_client).await;
    let team_value = team_result
        .ok()
        .flatten()
        .and_then(|t| serde_json::to_value(&t).ok());
    let team_id = team_value.as_ref().and_then(|v| {
        v.get("id")
            .or_else(|| v.get("teamId"))
            .and_then(|id| id.as_str())
            .map(String::from)
    });
    let team_name = team_value
        .as_ref()
        .and_then(|v| v.get("name").and_then(|n| n.as_str()).map(String::from));

    CloudStatus {
        logged_in: true,
        email,
        plan,
        team_id,
        team_name,
    }
}

fn skill_content_hash(skill: &SkillRecord) -> String {
    let skill_md_path = match skills_base_dir() {
        Ok(base) => Some(
            base.join(&skill.source)
                .join(&skill.directory)
                .join("SKILL.md"),
        ),
        Err(e) => {
            warn_skill_hash_fallback(&format!(
                "failed to resolve skills dir for {}: {e}",
                skill.id
            ));
            None
        }
    };

    let content = match skill_md_path {
        Some(path) => match std::fs::read_to_string(&path) {
            Ok(markdown) => extract_skill_content_body(&markdown),
            Err(e) => {
                if e.kind() != std::io::ErrorKind::NotFound {
                    warn_skill_hash_fallback(&format!(
                        "failed to read {} for {}: {e}",
                        path.display(),
                        skill.id
                    ));
                }
                fallback_skill_content_for_hash(skill)
            }
        },
        None => fallback_skill_content_for_hash(skill),
    };

    let digest = sha2::Sha256::digest(content.as_bytes());
    use std::fmt::Write as _;
    digest
        .iter()
        .fold(String::with_capacity(digest.len() * 2), |mut acc, b| {
            let _ = write!(acc, "{b:02x}");
            acc
        })
}

fn warn_skill_hash_fallback(message: &str) {
    if std::env::var_os("DIFFLORE_DEBUG_SYNC_HASH").is_some() {
        eprintln!("[difflore] rules sync hash fallback: {message}");
    }
}

fn fallback_skill_content_for_hash(skill: &SkillRecord) -> String {
    skill
        .check_prompt
        .clone()
        .or_else(|| {
            if skill.description.trim().is_empty() {
                None
            } else {
                Some(skill.description.clone())
            }
        })
        .unwrap_or_default()
}

fn extract_skill_content_body(markdown: &str) -> String {
    let mut lines = markdown.lines();
    if lines.next().map(str::trim) != Some("---") {
        return markdown.trim().to_owned();
    }

    let mut in_frontmatter = true;
    let mut body_lines: Vec<&str> = Vec::new();
    for line in markdown.lines().skip(1) {
        if in_frontmatter {
            if line.trim() == "---" {
                in_frontmatter = false;
            }
            continue;
        }
        body_lines.push(line);
    }

    if in_frontmatter {
        markdown.trim().to_owned()
    } else {
        body_lines.join("\n").trim().to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ProviderRecord;
    use std::collections::HashMap;

    #[test]
    fn mask_short_keys_returns_only_bullets() {
        assert_eq!(mask_api_key(""), "");
        assert_eq!(mask_api_key("ab"), "••");
        assert_eq!(mask_api_key("abcd"), "••••");
    }

    #[test]
    fn mask_long_keys_keeps_last_four() {
        assert_eq!(mask_api_key("sk-abcdef1234"), "••••1234");
        assert_eq!(mask_api_key("  spaced-key-9876  "), "••••9876");
    }

    fn make_provider(name: &str, key: Option<&str>) -> ProviderRecord {
        let mut mapping = HashMap::new();
        mapping.insert("review".into(), "claude-3".into());
        ProviderRecord {
            id: format!("{name}-id"),
            name: name.into(),
            base_url: format!("https://{name}.example.com"),
            api_key: key.map(String::from),
            model_mapping: mapping,
            is_active: true,
            created_at: "2026-04-10T00:00:00Z".into(),
            updated_at: "2026-04-10T00:00:00Z".into(),
        }
    }

    #[test]
    fn build_entries_masks_keys_and_omits_when_absent() {
        let providers = vec![
            make_provider("anthropic", Some("sk-ant-1234567890abcd")),
            make_provider("local", None),
        ];
        let entries = build_provider_sync_entries(&providers);
        assert_eq!(entries.len(), 2);

        let first = entries[0].as_object().unwrap();
        assert_eq!(first.get("name").unwrap().as_str(), Some("anthropic"));
        assert_eq!(
            first.get("baseUrl").unwrap().as_str(),
            Some("https://anthropic.example.com"),
        );
        assert_eq!(first.get("maskedKey").unwrap().as_str(), Some("••••abcd"));
        assert!(first.get("modelMapping").is_some());
        assert_eq!(
            first.get("updatedAt").unwrap().as_str(),
            Some("2026-04-10T00:00:00Z"),
        );

        let second = entries[1].as_object().unwrap();
        assert!(
            second.get("maskedKey").is_none(),
            "absent key should not emit maskedKey"
        );
    }

    #[test]
    fn build_entries_skips_empty_model_mapping() {
        let provider = ProviderRecord {
            id: "x".into(),
            name: "x".into(),
            base_url: "https://x".into(),
            api_key: None,
            model_mapping: HashMap::new(),
            is_active: false,
            created_at: "t".into(),
            updated_at: "t".into(),
        };
        let entries = build_provider_sync_entries(&[provider]);
        assert!(entries[0].get("modelMapping").is_none());
    }

    #[test]
    fn provider_pull_payload_is_canonical_array_only() {
        let array = serde_json::json!([{ "name": "codex" }]);
        assert_eq!(
            normalize_provider_payload(Some(array.clone())).as_ref(),
            Some(&array)
        );

        let stringified = serde_json::json!(r#"[{"name":"old"}]"#);
        assert!(
            normalize_provider_payload(Some(stringified)).is_none(),
            "stringified provider JSON must fail closed"
        );
        assert!(normalize_provider_payload(Some(serde_json::json!({}))).is_none());
    }

    #[test]
    fn sync_rule_mapping_uses_canonical_source_repo_field_only() {
        let val = serde_json::json!({
            "id": "rule-1",
            "name": "Rule",
            "content": "Body",
            "source_repo": "acme/retired",
            "sourceRepo": "acme/canonical"
        });
        let mapped = map_synced_rule_value(&val).expect("valid rule");
        assert_eq!(mapped.source_repo.as_deref(), Some("acme/canonical"));

        let retired_only = serde_json::json!({
            "id": "rule-2",
            "name": "Rule",
            "content": "Body",
            "source_repo": "acme/retired"
        });
        let mapped = map_synced_rule_value(&retired_only).expect("valid rule");
        assert_eq!(mapped.source_repo, None);
    }

    #[test]
    fn sync_rule_mapping_rejects_missing_required_id_or_content() {
        // Missing id → rejected (would otherwise create an empty-id local rule).
        let no_id = serde_json::json!({ "name": "R", "content": "Body" });
        assert!(map_synced_rule_value(&no_id).is_err());
        // Empty/whitespace content → rejected.
        let empty_content = serde_json::json!({ "id": "r", "name": "R", "content": "  " });
        assert!(map_synced_rule_value(&empty_content).is_err());
        // Both present → accepted.
        let ok = serde_json::json!({ "id": "r", "name": "R", "content": "Body" });
        assert!(map_synced_rule_value(&ok).is_ok());
    }

    #[test]
    fn skill_content_hash_falls_back_silently_when_skill_file_is_missing() {
        let skill = SkillRecord {
            id: "missing-cloud-rule".into(),
            name: "Missing cloud rule".into(),
            source: "cloud".into(),
            directory: "missing-cloud-rule".into(),
            version: "1.0.0".into(),
            description: "description fallback".into(),
            r#type: "review_standard".into(),
            engines: vec![],
            tags: vec![],
            trigger: None,
            check_prompt: Some("prefer check prompt for hashing".into()),
            repo_owner: None,
            repo_name: None,
            repo_branch: None,
            readme_url: None,
            enabled_for_codex: true,
            enabled_for_claude: true,
            enabled_for_gemini: true,
            enabled_for_cursor: true,
            installed_at: "2026-05-11T00:00:00Z".into(),
            updated_at: "2026-05-11T00:00:00Z".into(),
            enforcement: None,
            origin: "pr_review".into(),
        };

        let expected = {
            let digest = sha2::Sha256::digest(b"prefer check prompt for hashing");
            use std::fmt::Write as _;
            digest
                .iter()
                .fold(String::with_capacity(digest.len() * 2), |mut acc, b| {
                    let _ = write!(acc, "{b:02x}");
                    acc
                })
        };

        assert_eq!(skill_content_hash(&skill), expected);
    }
}
