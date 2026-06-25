use sqlx::Row;

const LOCAL_REVIEW_STATUS_PATH: &str = "$.localReview.status";
const LOCAL_REVIEW_STATUS_APPROVED: &str = "approved";
const LOCAL_TRIAGE_STATUS_PATH: &str = "$.localTriage.status";
const LOCAL_TRIAGE_SUPERSEDED_BY: &str = "superseded_by";
const LOCAL_TRIAGE_CLUSTERED_INTO: &str = "clustered_into";
const LOCAL_TRIAGE_DROPPED_LOW_SIGNAL: &str = "dropped_low_signal";

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::commands::status) struct MemoryInboxSummary {
    pub(in crate::commands::status) active_rules: i64,
    pub(in crate::commands::status) local_drafts: i64,
    pub(in crate::commands::status) local_discoveries: LocalDiscoverySummary,
    pub(in crate::commands::status) queues: MemoryQueueSummary,
    pub(in crate::commands::status) cloud: LocalCloudSummary,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(in crate::commands::status) warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::commands::status) struct LocalDiscoverySummary {
    pub(in crate::commands::status) session_mined_candidates: i64,
    pub(in crate::commands::status) latest: Vec<SessionMinedPreview>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::commands::status) struct SessionMinedPreview {
    pub(in crate::commands::status) id: String,
    pub(in crate::commands::status) row_id: i64,
    pub(in crate::commands::status) status: String,
    pub(in crate::commands::status) created_at_ms: i64,
    pub(in crate::commands::status) title: String,
    pub(in crate::commands::status) source_repo: String,
    pub(in crate::commands::status) file_patterns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::commands::status) struct MemoryQueueSummary {
    pub(in crate::commands::status) cloud_outbox: Vec<QueueCount>,
    pub(in crate::commands::status) session_mined_pending: i64,
    pub(in crate::commands::status) session_mined_blocked: i64,
    pub(in crate::commands::status) observations_outbox: Vec<QueueCount>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::commands::status) struct QueueCount {
    pub(in crate::commands::status) kind: String,
    pub(in crate::commands::status) status: String,
    pub(in crate::commands::status) count: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::commands::status) struct LocalCloudSummary {
    pub(in crate::commands::status) logged_in: bool,
    pub(in crate::commands::status) team_ready: Option<bool>,
    pub(in crate::commands::status) blocker: Option<String>,
    pub(in crate::commands::status) team_status_source: String,
}

impl MemoryInboxSummary {
    pub(in crate::commands::status) fn empty(
        active_rules: i64,
        local_drafts: i64,
        cloud_logged_in: bool,
    ) -> Self {
        Self {
            active_rules,
            local_drafts,
            local_discoveries: LocalDiscoverySummary {
                session_mined_candidates: 0,
                latest: Vec::new(),
            },
            queues: MemoryQueueSummary {
                cloud_outbox: Vec::new(),
                session_mined_pending: 0,
                session_mined_blocked: 0,
                observations_outbox: Vec::new(),
            },
            cloud: local_cloud_summary(cloud_logged_in, None),
            warnings: Vec::new(),
        }
    }
}

pub(in crate::commands::status) async fn memory_inbox_summary(
    db: &difflore_core::SqlitePool,
    active_rules: i64,
    local_drafts: i64,
    cloud_logged_in: bool,
) -> MemoryInboxSummary {
    let Ok(queue_counts) = cloud_outbox_counts(db).await else {
        let mut summary = MemoryInboxSummary::empty(active_rules, local_drafts, cloud_logged_in);
        summary
            .warnings
            .push("cloud_outbox summary unavailable".to_owned());
        return summary;
    };

    let mut warnings = Vec::new();
    let latest = latest_session_mined_previews(db, &mut warnings)
        .await
        .unwrap_or_else(|err| {
            warnings.push(format!("session-mined preview unavailable: {err}"));
            Vec::new()
        });
    let queue_session_mined_pending = count_kind_statuses(
        &queue_counts,
        difflore_core::cloud::outbox::kind::SESSION_MINED_CANDIDATE,
        &["pending", "processing"],
    );
    let session_mined_blocked = count_kind_statuses(
        &queue_counts,
        difflore_core::cloud::outbox::kind::SESSION_MINED_CANDIDATE,
        &["abandoned"],
    );
    let observations_outbox = queue_counts
        .iter()
        .filter(|count| count.kind == difflore_core::cloud::outbox::kind::OBSERVATION)
        .cloned()
        .collect();
    let inferred_team_ready = infer_team_ready_from_warnings(&warnings);

    MemoryInboxSummary {
        active_rules,
        local_drafts,
        local_discoveries: LocalDiscoverySummary {
            session_mined_candidates: visible_session_mined_count(db, &mut warnings)
                .await
                .unwrap_or(queue_session_mined_pending),
            latest,
        },
        queues: MemoryQueueSummary {
            cloud_outbox: queue_counts,
            session_mined_pending: queue_session_mined_pending,
            session_mined_blocked,
            observations_outbox,
        },
        cloud: local_cloud_summary(cloud_logged_in, inferred_team_ready),
        warnings,
    }
}

async fn cloud_outbox_counts(
    db: &difflore_core::SqlitePool,
) -> Result<Vec<QueueCount>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT kind, status, COUNT(*) AS count \
         FROM cloud_outbox \
         GROUP BY kind, status \
         ORDER BY kind, status",
    )
    .fetch_all(db)
    .await?;

    rows.into_iter()
        .map(|row| {
            Ok(QueueCount {
                kind: row.try_get("kind")?,
                status: row.try_get("status")?,
                count: row.try_get("count")?,
            })
        })
        .collect()
}

async fn latest_session_mined_previews(
    db: &difflore_core::SqlitePool,
    warnings: &mut Vec<String>,
) -> Result<Vec<SessionMinedPreview>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, payload_json, status, created_at, last_error \
         FROM cloud_outbox \
         WHERE kind = ?1 \
           AND NOT (
                json_valid(payload_json)
                AND LOWER(COALESCE(json_extract(payload_json, ?2), '')) = ?3
           ) \
           AND NOT (
                json_valid(payload_json)
                AND LOWER(COALESCE(json_extract(payload_json, ?4), '')) IN (?5, ?6, ?7)
           ) \
         ORDER BY created_at DESC, id DESC \
         LIMIT 3",
    )
    .bind(difflore_core::cloud::outbox::kind::SESSION_MINED_CANDIDATE)
    .bind(LOCAL_REVIEW_STATUS_PATH)
    .bind(LOCAL_REVIEW_STATUS_APPROVED)
    .bind(LOCAL_TRIAGE_STATUS_PATH)
    .bind(LOCAL_TRIAGE_SUPERSEDED_BY)
    .bind(LOCAL_TRIAGE_CLUSTERED_INTO)
    .bind(LOCAL_TRIAGE_DROPPED_LOW_SIGNAL)
    .fetch_all(db)
    .await?;

    let mut previews = Vec::new();
    for row in rows {
        let row_id: i64 = row.try_get("id")?;
        let payload_json: String = row.try_get("payload_json")?;
        let status: String = row.try_get("status")?;
        let created_at_ms: i64 = row.try_get("created_at")?;
        let last_error: Option<String> = row.try_get("last_error")?;
        if let Some(error) = last_error
            .as_deref()
            .map(str::trim)
            .filter(|error| !error.is_empty())
        {
            warnings.push(format!(
                "session-mined outbox row {row_id} upload issue: {error}"
            ));
        }
        match serde_json::from_str::<difflore_core::cloud::session_mined::SessionMinedCandidate>(
            &payload_json,
        ) {
            Ok(candidate) => previews.push(SessionMinedPreview {
                id: format!("session:{}", candidate.content_hash),
                row_id,
                status,
                created_at_ms,
                title: candidate.title,
                source_repo: candidate.source_repo,
                file_patterns: candidate.file_patterns,
            }),
            Err(err) => warnings.push(format!(
                "session-mined outbox row {row_id} could not be parsed: {err}"
            )),
        }
    }
    Ok(previews)
}

async fn visible_session_mined_count(
    db: &difflore_core::SqlitePool,
    warnings: &mut Vec<String>,
) -> Option<i64> {
    match sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) \
         FROM cloud_outbox \
         WHERE kind = ?1 \
           AND json_valid(payload_json) \
           AND NOT (
                json_valid(payload_json)
                AND LOWER(COALESCE(json_extract(payload_json, ?2), '')) = ?3
           ) \
           AND NOT (
                json_valid(payload_json)
                AND LOWER(COALESCE(json_extract(payload_json, ?4), '')) IN (?5, ?6, ?7)
           )",
    )
    .bind(difflore_core::cloud::outbox::kind::SESSION_MINED_CANDIDATE)
    .bind(LOCAL_REVIEW_STATUS_PATH)
    .bind(LOCAL_REVIEW_STATUS_APPROVED)
    .bind(LOCAL_TRIAGE_STATUS_PATH)
    .bind(LOCAL_TRIAGE_SUPERSEDED_BY)
    .bind(LOCAL_TRIAGE_CLUSTERED_INTO)
    .bind(LOCAL_TRIAGE_DROPPED_LOW_SIGNAL)
    .fetch_one(db)
    .await
    {
        Ok(count) => Some(count),
        Err(err) => {
            warnings.push(format!("session-mined visible count unavailable: {err}"));
            None
        }
    }
}

fn count_kind_statuses(counts: &[QueueCount], kind: &str, statuses: &[&str]) -> i64 {
    counts
        .iter()
        .filter(|count| count.kind == kind && statuses.contains(&count.status.as_str()))
        .map(|count| count.count)
        .sum()
}

fn local_cloud_summary(logged_in: bool, inferred_team_ready: Option<bool>) -> LocalCloudSummary {
    let team_status_source = match inferred_team_ready {
        Some(_) => "local_outbox_error",
        None => "not_cached_offline",
    }
    .to_owned();

    LocalCloudSummary {
        logged_in,
        team_ready: inferred_team_ready,
        blocker: None,
        team_status_source,
    }
}

fn infer_team_ready_from_warnings(warnings: &[String]) -> Option<bool> {
    warnings.iter().find_map(|warning| {
        let lower = warning.to_ascii_lowercase();
        let mentions_team = lower.contains("team") || lower.contains("workspace");
        let looks_missing = lower.contains("missing")
            || lower.contains("no team")
            || lower.contains("needs_team_workspace")
            || lower.contains("403");
        (mentions_team && looks_missing).then_some(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool_with_cloud_outbox() -> difflore_core::SqlitePool {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("open sqlite");
        sqlx::query(
            "CREATE TABLE cloud_outbox (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                kind TEXT NOT NULL,
                payload_json TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending',
                retry_count INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL,
                claimed_at INTEGER,
                last_error TEXT,
                enriched_at INTEGER
            )",
        )
        .execute(&pool)
        .await
        .expect("create cloud_outbox");
        pool
    }

    fn session_candidate_json(title: &str, hash: &str) -> String {
        serde_json::json!({
            "session_id": "s1",
            "ts_ms": 1,
            "source_repo": "acme/app",
            "title": title,
            "body": "Prefer stable modal wrappers.",
            "file_patterns": ["src/**/*.tsx"],
            "gate_model": "claude:haiku",
            "gate_verdict": "KEEP",
            "content_hash": hash,
            "origin": "session_mined",
            "requires_human_approval": true
        })
        .to_string()
    }

    async fn insert_outbox(
        pool: &difflore_core::SqlitePool,
        kind: &str,
        payload_json: &str,
        status: &str,
        created_at: i64,
        last_error: Option<&str>,
    ) {
        sqlx::query(
            "INSERT INTO cloud_outbox
                (kind, payload_json, status, retry_count, created_at, last_error)
             VALUES (?1, ?2, ?3, 0, ?4, ?5)",
        )
        .bind(kind)
        .bind(payload_json)
        .bind(status)
        .bind(created_at)
        .bind(last_error)
        .execute(pool)
        .await
        .expect("insert outbox row");
    }

    #[tokio::test]
    async fn memory_inbox_counts_and_previews_session_mined_candidates() {
        let pool = pool_with_cloud_outbox().await;
        insert_outbox(
            &pool,
            difflore_core::cloud::outbox::kind::SESSION_MINED_CANDIDATE,
            &session_candidate_json("Wrap modals with content blocks", "abc123abc123abcd"),
            "pending",
            20,
            None,
        )
        .await;
        insert_outbox(
            &pool,
            difflore_core::cloud::outbox::kind::OBSERVATION,
            "{}",
            "pending",
            10,
            None,
        )
        .await;

        let summary = memory_inbox_summary(&pool, 2, 1, true).await;

        assert_eq!(summary.active_rules, 2);
        assert_eq!(summary.local_drafts, 1);
        assert_eq!(summary.local_discoveries.session_mined_candidates, 1);
        assert_eq!(summary.queues.session_mined_pending, 1);
        assert_eq!(summary.queues.cloud_outbox.len(), 2);
        assert_eq!(
            summary.local_discoveries.latest[0].id,
            "session:abc123abc123abcd"
        );
        assert_eq!(
            summary.local_discoveries.latest[0].title,
            "Wrap modals with content blocks"
        );
        assert!(summary.warnings.is_empty());
    }

    #[tokio::test]
    async fn memory_inbox_keeps_corrupt_session_rows_as_warnings() {
        let pool = pool_with_cloud_outbox().await;
        insert_outbox(
            &pool,
            difflore_core::cloud::outbox::kind::SESSION_MINED_CANDIDATE,
            "{not-json",
            "abandoned",
            20,
            Some("403 Forbidden: missing team workspace"),
        )
        .await;

        let summary = memory_inbox_summary(&pool, 0, 0, true).await;

        assert_eq!(summary.local_discoveries.session_mined_candidates, 0);
        assert_eq!(summary.queues.session_mined_blocked, 1);
        assert!(summary.local_discoveries.latest.is_empty());
        assert_eq!(summary.cloud.team_ready, Some(false));
        assert_eq!(summary.cloud.blocker, None);
        assert!(
            summary
                .warnings
                .iter()
                .any(|w| w.contains("could not be parsed"))
        );
    }
}
