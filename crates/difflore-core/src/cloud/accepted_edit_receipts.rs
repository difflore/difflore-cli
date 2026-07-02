use sqlx::{Row, SqlitePool};

use crate::contract::{
    RecordAcceptedEditRequest, RecordAcceptedEditResponse, accepted_edit_diff_signature,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptedEditReceiptInsert {
    pub cloud_acceptance_id: Option<String>,
    pub local_receipt_key: String,
    pub repo_full_name: Option<String>,
    pub target_pr_number: Option<i64>,
    pub file_path: Option<String>,
    pub diff_signature: String,
    pub rule_ids: Vec<String>,
    pub acceptance_source: String,
    pub client: Option<String>,
    pub team_id: Option<String>,
    pub observations_inserted: i64,
    pub launch_grade: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AcceptedEditReceiptSummary {
    pub rows_last30: i64,
    pub rows_for_current_repo: i64,
    pub rows_without_repo: i64,
    pub rows_missing_rule_ids: i64,
    pub rows_missing_rule_ids_for_current_repo: i64,
    pub rows_with_cloud_rule_ids: i64,
    pub rows_with_cloud_rule_ids_for_current_repo: i64,
    pub rows_with_local_rule_ids: i64,
    pub rows_with_local_rule_ids_for_current_repo: i64,
    pub launch_grade_rows: i64,
    pub launch_grade_rows_for_current_repo: i64,
}

pub async fn record_confirmed(
    db: &SqlitePool,
    receipt: AcceptedEditReceiptInsert,
) -> Result<(), sqlx::Error> {
    let rule_ids_json = serde_json::to_string(&receipt.rule_ids).unwrap_or_else(|_| "[]".into());
    let now_ms = chrono::Utc::now().timestamp_millis();
    let created_at = now_ms;
    let uploaded_at = now_ms;
    sqlx::query(
        r"INSERT OR IGNORE INTO accepted_edit_receipts (
            cloud_acceptance_id,
            local_receipt_key,
            repo_full_name,
            target_pr_number,
            file_path,
            diff_signature,
            rule_ids_json,
            acceptance_source,
            client,
            team_id,
            observations_inserted,
            launch_grade,
            created_at,
            uploaded_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
    )
    .bind(receipt.cloud_acceptance_id)
    .bind(receipt.local_receipt_key)
    .bind(normalize_optional_repo(receipt.repo_full_name))
    .bind(receipt.target_pr_number)
    .bind(receipt.file_path)
    .bind(receipt.diff_signature)
    .bind(rule_ids_json)
    .bind(receipt.acceptance_source)
    .bind(receipt.client)
    .bind(receipt.team_id)
    .bind(receipt.observations_inserted)
    .bind(i64::from(receipt.launch_grade))
    .bind(created_at)
    .bind(uploaded_at)
    .execute(db)
    .await?;
    Ok(())
}

pub fn receipt_from_accepted_edit_response(
    request: &RecordAcceptedEditRequest,
    response: &RecordAcceptedEditResponse,
) -> Option<AcceptedEditReceiptInsert> {
    if !response.acceptance_recorded {
        return None;
    }
    let diff_signature = response
        .diff_signature
        .as_deref()
        .or(request.diff_signature.as_deref())
        .map(str::to_owned)
        .unwrap_or_else(|| accepted_edit_diff_signature(&request.before_code, &request.after_code));
    let rule_ids = if response.attributed_rule_ids.is_empty() {
        request.rule_ids.clone()
    } else {
        response.attributed_rule_ids.clone()
    };
    Some(AcceptedEditReceiptInsert {
        cloud_acceptance_id: response.acceptance_id.clone(),
        local_receipt_key: local_receipt_key(request, &diff_signature),
        repo_full_name: request.repo_full_name.clone(),
        target_pr_number: request.target_pr_number,
        file_path: request.file_path.clone(),
        diff_signature,
        rule_ids,
        acceptance_source: request
            .acceptance_source
            .clone()
            .unwrap_or_else(|| "unknown".to_owned()),
        client: request.client.clone(),
        team_id: response.team_id.clone(),
        observations_inserted: i64::from(response.observations_inserted),
        launch_grade: response.launch_grade_paid_value_ready,
    })
}

pub async fn summary_for_repos(
    db: &SqlitePool,
    normalized_repos: &[String],
    window_days: i64,
) -> Result<AcceptedEditReceiptSummary, sqlx::Error> {
    let cutoff_ms = chrono::Utc::now()
        .timestamp_millis()
        .saturating_sub(window_days.max(1) * 24 * 60 * 60 * 1_000);
    let repo_set = normalized_repo_set(normalized_repos);
    let rows = sqlx::query(
        r"SELECT repo_full_name, rule_ids_json, launch_grade
          FROM accepted_edit_receipts
          WHERE uploaded_at >= ?1",
    )
    .bind(cutoff_ms)
    .fetch_all(db)
    .await?;

    let mut summary = AcceptedEditReceiptSummary::default();
    for row in rows {
        summary.rows_last30 += 1;
        let repo = row
            .try_get::<Option<String>, _>("repo_full_name")
            .ok()
            .flatten()
            .and_then(|repo| normalize_repo(&repo));
        let repo_matches = match repo {
            Some(repo) if repo_set.iter().any(|candidate| candidate == &repo) => {
                summary.rows_for_current_repo += 1;
                true
            }
            Some(_) => false,
            None => {
                summary.rows_without_repo += 1;
                false
            }
        };

        let rule_ids = row
            .try_get::<String, _>("rule_ids_json")
            .ok()
            .and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
            .unwrap_or_default()
            .into_iter()
            .map(|rule_id| rule_id.trim().to_owned())
            .filter(|rule_id| !rule_id.is_empty())
            .collect::<Vec<_>>();
        if rule_ids.is_empty() {
            summary.rows_missing_rule_ids += 1;
            if repo_matches {
                summary.rows_missing_rule_ids_for_current_repo += 1;
            }
        } else if rule_ids.iter().all(|rule_id| looks_like_uuid(rule_id)) {
            summary.rows_with_cloud_rule_ids += 1;
            if repo_matches {
                summary.rows_with_cloud_rule_ids_for_current_repo += 1;
            }
        } else {
            summary.rows_with_local_rule_ids += 1;
            if repo_matches {
                summary.rows_with_local_rule_ids_for_current_repo += 1;
            }
        }

        if row.try_get::<i64, _>("launch_grade").unwrap_or(0) != 0 {
            summary.launch_grade_rows += 1;
            if repo_matches {
                summary.launch_grade_rows_for_current_repo += 1;
            }
        }
    }
    Ok(summary)
}

fn normalize_optional_repo(repo: Option<String>) -> Option<String> {
    repo.and_then(|repo| normalize_repo(&repo))
}

fn normalized_repo_set(repos: &[String]) -> Vec<String> {
    repos
        .iter()
        .filter_map(|repo| normalize_repo(repo))
        .collect()
}

fn normalize_repo(repo: &str) -> Option<String> {
    let repo = repo.trim().trim_end_matches(".git").to_ascii_lowercase();
    (!repo.is_empty()).then_some(repo)
}

fn local_receipt_key(request: &RecordAcceptedEditRequest, diff_signature: &str) -> String {
    use sha2::{Digest as _, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(b"accepted-edit-receipt-v1\n");
    hasher.update(diff_signature.as_bytes());
    hasher.update(b"\n");
    hasher.update(request.repo_full_name.as_deref().unwrap_or("").as_bytes());
    hasher.update(b"\n");
    hasher.update(
        request
            .target_pr_number
            .map(|value| value.to_string())
            .unwrap_or_default()
            .as_bytes(),
    );
    hasher.update(b"\n");
    hasher.update(request.file_path.as_deref().unwrap_or("").as_bytes());
    hasher.update(b"\n");
    hasher.update(
        request
            .acceptance_source
            .as_deref()
            .unwrap_or("")
            .as_bytes(),
    );
    hasher.update(b"\n");
    hasher.update(request.client.as_deref().unwrap_or("").as_bytes());
    for rule_id in &request.rule_ids {
        hasher.update(b"\nrule:");
        hasher.update(rule_id.trim().as_bytes());
    }
    let digest = hasher.finalize();
    let mut out = String::with_capacity("accepted-edit-".len() + digest.len() * 2);
    out.push_str("accepted-edit-");
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

pub(crate) fn looks_like_uuid(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    for (idx, byte) in bytes.iter().enumerate() {
        if matches!(idx, 8 | 13 | 18 | 23) {
            if *byte != b'-' {
                return false;
            }
        } else if !byte.is_ascii_hexdigit() {
            return false;
        }
    }
    true
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
        crate::infra::db::run_migrations(&pool)
            .await
            .expect("migrate");
        pool
    }

    fn receipt(local_key: &str) -> AcceptedEditReceiptInsert {
        AcceptedEditReceiptInsert {
            cloud_acceptance_id: Some(format!("cloud-{local_key}")),
            local_receipt_key: local_key.to_owned(),
            repo_full_name: Some("Acme/App".to_owned()),
            target_pr_number: Some(42),
            file_path: Some("src/lib.rs".to_owned()),
            diff_signature: format!("diff-{local_key}"),
            rule_ids: vec!["550e8400-e29b-41d4-a716-446655440000".to_owned()],
            acceptance_source: "agent_retained_edit".to_owned(),
            client: Some("difflore_hook".to_owned()),
            team_id: Some("team-1".to_owned()),
            observations_inserted: 2,
            launch_grade: true,
        }
    }

    fn accepted_edit_request() -> RecordAcceptedEditRequest {
        RecordAcceptedEditRequest {
            before_code: "old".to_owned(),
            after_code: "new".to_owned(),
            file_path: Some("src/lib.rs".to_owned()),
            repo_full_name: Some("Acme/App".to_owned()),
            target_pr_number: Some(42),
            language: Some("rust".to_owned()),
            acceptance_source: Some("agent_retained_edit".to_owned()),
            client: Some("difflore_hook".to_owned()),
            diff_signature: Some("local-diff".to_owned()),
            rule_ids: vec!["local-rule".to_owned()],
        }
    }

    fn accepted_edit_response() -> RecordAcceptedEditResponse {
        RecordAcceptedEditResponse {
            ok: true,
            acceptance_recorded: true,
            acceptance_id: Some("cloud-acceptance-1".to_owned()),
            diff_signature: Some("cloud-diff".to_owned()),
            team_id: Some("team-1".to_owned()),
            attributed_rule_ids: vec!["550e8400-e29b-41d4-a716-446655440000".to_owned()],
            observations_inserted: 2,
            launch_grade_provenance_trusted: true,
            launch_grade_paid_value_ready: true,
            memory_reinforcement_recorded: false,
            memory_reinforcement_deduped: false,
            error: None,
        }
    }

    #[test]
    fn receipt_mapper_uses_cloud_response_for_confirmed_acceptance() {
        let request = accepted_edit_request();
        let response = accepted_edit_response();

        let receipt = receipt_from_accepted_edit_response(&request, &response)
            .expect("confirmed response should create receipt");

        assert_eq!(
            receipt.cloud_acceptance_id.as_deref(),
            Some("cloud-acceptance-1")
        );
        assert!(receipt.local_receipt_key.starts_with("accepted-edit-"));
        assert_eq!(receipt.repo_full_name.as_deref(), Some("Acme/App"));
        assert_eq!(receipt.diff_signature, "cloud-diff");
        assert_eq!(receipt.rule_ids, response.attributed_rule_ids);
        assert_eq!(receipt.observations_inserted, 2);
        assert!(receipt.launch_grade);
    }

    #[test]
    fn receipt_mapper_skips_unrecorded_acceptance() {
        let request = accepted_edit_request();
        let mut response = accepted_edit_response();
        response.acceptance_recorded = false;
        response.launch_grade_paid_value_ready = false;

        assert!(receipt_from_accepted_edit_response(&request, &response).is_none());
    }

    #[tokio::test]
    async fn receipt_store_records_confirmed_accepted_edit() {
        let pool = setup().await;
        record_confirmed(&pool, receipt("one"))
            .await
            .expect("record receipt");

        let summary = summary_for_repos(&pool, &["acme/app".to_owned()], 30)
            .await
            .expect("summary");

        assert_eq!(summary.rows_last30, 1);
        assert_eq!(summary.rows_for_current_repo, 1);
        assert_eq!(summary.rows_without_repo, 0);
        assert_eq!(summary.rows_missing_rule_ids, 0);
        assert_eq!(summary.rows_with_cloud_rule_ids, 1);
        assert_eq!(summary.rows_with_local_rule_ids, 0);
        assert_eq!(summary.launch_grade_rows, 1);
    }

    #[tokio::test]
    async fn receipt_store_counts_empty_outbox_as_confirmed_not_missing() {
        let pool = setup().await;
        record_confirmed(&pool, receipt("one"))
            .await
            .expect("record receipt");
        record_confirmed(&pool, receipt("one"))
            .await
            .expect("duplicate receipt ignored");

        let outbox_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cloud_outbox")
            .fetch_one(&pool)
            .await
            .expect("count outbox");
        let receipt_summary = summary_for_repos(&pool, &["acme/app".to_owned()], 30)
            .await
            .expect("receipt summary");

        assert_eq!(outbox_rows, 0);
        assert_eq!(receipt_summary.rows_last30, 1);
        assert_eq!(receipt_summary.rows_for_current_repo, 1);
    }
}
