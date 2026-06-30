use openapi_contract::api;
use serde::{Deserialize, Serialize};

use super::client::CloudClient;
use crate::contract::Success;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CandidateStatus {
    Pending,
    Approved,
    Rejected,
}

impl CandidateStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CandidateSeverity {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleCandidate {
    pub id: String,
    pub team_id: String,
    pub diff_signature: String,
    pub acceptance_count: f64,
    pub distinct_users: f64,
    pub generated_name: String,
    pub generated_description: String,
    pub generated_severity: String,
    pub example_before: String,
    pub example_after: String,
    pub language: Option<String>,
    pub status: String,
    pub reviewed_by: Option<String>,
    pub reviewed_at: Option<String>,
    pub rejection_reason: Option<String>,
    pub published_rule_id: Option<String>,
    pub origin: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleCandidateEvent {
    pub id: String,
    pub candidate_id: String,
    pub team_id: String,
    pub event_type: String,
    pub actor_id: Option<String>,
    pub status_from: Option<String>,
    pub status_to: Option<String>,
    pub reason: Option<String>,
    pub confidence_before: Option<f64>,
    pub confidence_after: Option<f64>,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleCandidateDetail {
    #[serde(flatten)]
    pub candidate: RuleCandidate,
    pub events: Vec<RuleCandidateEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListCandidatesRequest {
    pub team_id: String,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub status: Option<CandidateStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CountCandidatesRequest {
    pub team_id: String,
    pub status: Option<CandidateStatus>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CandidateCount {
    pub total: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CandidateApprovalEdits {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub severity: Option<CandidateSeverity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ApproveCandidateRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edits: Option<CandidateApprovalEdits>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApproveCandidateResponse {
    pub candidate_id: String,
    pub rule_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RejectCandidateRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DismissSignatureRequest {
    pub team_id: String,
    pub signature: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateCandidateSettingsRequest {
    pub team_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_distinct_users: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lookback_days: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CandidateSettings {
    pub team_id: String,
    pub min_count: f64,
    pub min_distinct_users: f64,
    pub lookback_days: f64,
    pub enabled: bool,
}

pub async fn list_candidates(
    client: &CloudClient,
    request: ListCandidatesRequest,
) -> crate::Result<Vec<RuleCandidate>> {
    let limit = request.limit.unwrap_or(20);
    let offset = request.offset.unwrap_or(0);
    if let Some(status) = request.status {
        client
            .fetch_api_json(
                api!(GET "/rules/candidates", query = {
                    teamId: &request.team_id,
                    limit: limit,
                    offset: offset,
                    status: status.as_str(),
                }),
                "list_candidates",
            )
            .await
    } else {
        client
            .fetch_api_json(
                api!(GET "/rules/candidates", query = {
                    teamId: &request.team_id,
                    limit: limit,
                    offset: offset,
                }),
                "list_candidates",
            )
            .await
    }
}

pub async fn count_candidates(
    client: &CloudClient,
    request: CountCandidatesRequest,
) -> crate::Result<CandidateCount> {
    if let Some(status) = request.status {
        client
            .fetch_api_json(
                api!(GET "/rules/candidates/count", query = {
                    teamId: &request.team_id,
                    status: status.as_str(),
                }),
                "count_candidates",
            )
            .await
    } else {
        client
            .fetch_api_json(
                api!(GET "/rules/candidates/count", query = {
                    teamId: &request.team_id,
                }),
                "count_candidates",
            )
            .await
    }
}

pub async fn get_candidate(
    client: &CloudClient,
    candidate_id: &str,
) -> crate::Result<RuleCandidateDetail> {
    client
        .fetch_api_json(
            api!(GET "/rules/candidates/{candidateId}", candidateId = candidate_id),
            "get_candidate",
        )
        .await
}

pub async fn approve_candidate(
    client: &CloudClient,
    candidate_id: &str,
    edits: Option<CandidateApprovalEdits>,
) -> crate::Result<ApproveCandidateResponse> {
    if let Some(edits) = edits {
        let request = ApproveCandidateRequest { edits: Some(edits) };
        client
            .fetch_api_json(
                api!(
                    POST "/rules/candidates/{candidateId}/approve",
                    candidateId = candidate_id,
                    body = &request
                ),
                "approve_candidate",
            )
            .await
    } else {
        client
            .fetch_api_json(
                api!(
                    POST "/rules/candidates/{candidateId}/approve",
                    candidateId = candidate_id
                ),
                "approve_candidate",
            )
            .await
    }
}

pub async fn reject_candidate(
    client: &CloudClient,
    candidate_id: &str,
    reason: Option<String>,
) -> crate::Result<()> {
    let _: Success = if let Some(reason) = reason {
        let request = RejectCandidateRequest {
            reason: Some(reason),
        };
        client
            .fetch_api_json(
                api!(
                    POST "/rules/candidates/{candidateId}/reject",
                    candidateId = candidate_id,
                    body = &request
                ),
                "reject_candidate",
            )
            .await?
    } else {
        client
            .fetch_api_json(
                api!(
                    POST "/rules/candidates/{candidateId}/reject",
                    candidateId = candidate_id
                ),
                "reject_candidate",
            )
            .await?
    };
    Ok(())
}

pub async fn dismiss_signature(
    client: &CloudClient,
    request: &DismissSignatureRequest,
) -> crate::Result<()> {
    let _: Success = client
        .fetch_api_json(
            api!(POST "/rules/candidates/dismiss-signature", body = request),
            "dismiss_signature",
        )
        .await?;
    Ok(())
}

pub async fn update_settings(
    client: &CloudClient,
    request: &UpdateCandidateSettingsRequest,
) -> crate::Result<CandidateSettings> {
    client
        .fetch_api_json(
            api!(POST "/rules/candidates/settings", body = request),
            "update_candidate_settings",
        )
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn assert_float_eq(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < f64::EPSILON,
            "expected {expected}, got {actual}"
        );
    }

    fn candidate_json() -> serde_json::Value {
        json!({
            "id": "cand-1",
            "teamId": "team-1",
            "diffSignature": "sig-1",
            "acceptanceCount": 4,
            "distinctUsers": 2,
            "generatedName": "Use typed errors",
            "generatedDescription": "Prefer typed errors over stringly status checks.",
            "generatedSeverity": "warning",
            "exampleBefore": "if err == \"missing\" {}",
            "exampleAfter": "if matches!(err, Error::Missing) {}",
            "language": "rust",
            "status": "pending",
            "reviewedBy": null,
            "reviewedAt": null,
            "rejectionReason": null,
            "publishedRuleId": null,
            "origin": "observation_cluster",
            "meta": {"source": "test"},
            "createdAt": "2026-06-30T00:00:00.000Z"
        })
    }

    #[test]
    fn parses_list_candidate_shape_from_contract() {
        let candidates: Vec<RuleCandidate> =
            serde_json::from_value(json!([candidate_json()])).unwrap();

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].id, "cand-1");
        assert_eq!(candidates[0].team_id, "team-1");
        assert_float_eq(candidates[0].acceptance_count, 4.0);
        assert_float_eq(candidates[0].distinct_users, 2.0);
        assert_eq!(candidates[0].language.as_deref(), Some("rust"));
        assert_eq!(candidates[0].status, "pending");
    }

    #[test]
    fn parses_candidate_detail_events_shape_from_contract() {
        let mut value = candidate_json();
        value.as_object_mut().unwrap().insert(
            "events".to_owned(),
            json!([{
                "id": "event-1",
                "candidateId": "cand-1",
                "teamId": "team-1",
                "eventType": "status_changed",
                "actorId": "user-1",
                "statusFrom": null,
                "statusTo": "pending",
                "reason": null,
                "confidenceBefore": null,
                "confidenceAfter": 0.92,
                "metadata": {"ignored": true},
                "createdAt": "2026-06-30T00:01:00.000Z"
            }]),
        );

        let detail: RuleCandidateDetail = serde_json::from_value(value).unwrap();

        assert_eq!(detail.candidate.id, "cand-1");
        assert_eq!(detail.events.len(), 1);
        assert_eq!(detail.events[0].candidate_id, "cand-1");
        assert_eq!(detail.events[0].confidence_after, Some(0.92));
    }

    #[test]
    fn serializes_approve_edits_without_absent_fields() {
        let request = ApproveCandidateRequest {
            edits: Some(CandidateApprovalEdits {
                name: Some("Use typed errors".to_owned()),
                severity: Some(CandidateSeverity::Warning),
                ..CandidateApprovalEdits::default()
            }),
        };

        assert_eq!(
            serde_json::to_value(request).unwrap(),
            json!({
                "edits": {
                    "name": "Use typed errors",
                    "severity": "warning"
                }
            })
        );
    }

    #[test]
    fn serializes_reject_request_without_null_reason() {
        assert_eq!(
            serde_json::to_value(RejectCandidateRequest::default()).unwrap(),
            json!({})
        );
        assert_eq!(
            serde_json::to_value(RejectCandidateRequest {
                reason: Some("duplicate".to_owned()),
            })
            .unwrap(),
            json!({"reason": "duplicate"})
        );
    }

    #[test]
    fn serializes_settings_request_without_absent_fields() {
        let request = UpdateCandidateSettingsRequest {
            team_id: "team-1".to_owned(),
            min_count: Some(3),
            min_distinct_users: None,
            lookback_days: Some(30),
            enabled: Some(true),
        };

        assert_eq!(
            serde_json::to_value(request).unwrap(),
            json!({
                "teamId": "team-1",
                "minCount": 3,
                "lookbackDays": 30,
                "enabled": true
            })
        );
    }

    #[test]
    fn parses_count_and_settings_responses() {
        let count: CandidateCount = serde_json::from_value(json!({"total": 7})).unwrap();
        assert_float_eq(count.total, 7.0);

        let settings: CandidateSettings = serde_json::from_value(json!({
            "teamId": "team-1",
            "minCount": 3,
            "minDistinctUsers": 2,
            "lookbackDays": 30,
            "enabled": true
        }))
        .unwrap();

        assert_eq!(settings.team_id, "team-1");
        assert_float_eq(settings.min_count, 3.0);
        assert_float_eq(settings.min_distinct_users, 2.0);
        assert_float_eq(settings.lookback_days, 30.0);
        assert!(settings.enabled);
    }
}
