use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};

use crate::Result;
use crate::memory_autopilot::load_memory_digest;
use crate::memory_inbox::{
    MemoryActivityFilter, MemoryInboxWarning, MemoryListFilter, MemoryRuleItem,
    load_memory_activity, load_memory_inbox, load_memory_items,
};

const MEMORY_OVERVIEW_SCHEMA_VERSION: &str = "memory-overview.v1";
const DEFAULT_LATEST_LIMIT: usize = 5;
const MAX_LATEST_LIMIT: usize = 1_000;
const DEFAULT_ACTIVITY_DAYS: i64 = 30;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MemoryOverview {
    pub schema_version: String,
    pub remembered: RememberedOverview,
    pub needs_review: NeedsReviewOverview,
    pub paused: PausedOverview,
    pub sync: SyncOverview,
    pub activity: ActivityOverview,
    pub next: MemoryOverviewNextAction,
    pub debug: MemoryOverviewDebug,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RememberedOverview {
    pub available: i64,
    pub active_total: i64,
    pub active_for_repo: Option<i64>,
    pub latest: Vec<OverviewRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NeedsReviewOverview {
    pub local_drafts: i64,
    pub local_discoveries: i64,
    pub autopilot_needs_review_groups: i64,
    pub latest: Vec<OverviewReviewItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PausedOverview {
    pub count: i64,
    pub latest: Vec<OverviewRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SyncOverview {
    pub logged_in: bool,
    pub approved_session_candidates_pending_upload: i64,
    pub activity_records_pending_upload: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ActivityOverview {
    pub window_days: i64,
    pub recall_calls: i64,
    pub empty_recalls: i64,
    pub rules_surfaced: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OverviewRule {
    pub item_id: String,
    pub rule_id: String,
    pub title: String,
    pub origin: String,
    pub source_repo: Option<String>,
    pub file_patterns: Vec<String>,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OverviewReviewItem {
    pub item_id: String,
    pub kind: String,
    pub title: String,
    pub origin: Option<String>,
    pub source_repo: Option<String>,
    pub file_patterns: Vec<String>,
    pub updated_at: Option<String>,
    pub review_hint: Option<String>,
    pub commands: OverviewReviewCommands,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OverviewReviewCommands {
    pub show: String,
    pub approve: Option<String>,
    pub reject: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MemoryOverviewNextAction {
    pub kind: String,
    pub label: String,
    pub command: Option<String>,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MemoryOverviewDebug {
    pub repo_full_name: Option<String>,
    pub latest_limit: usize,
    pub inbox_warnings: Vec<MemoryInboxWarning>,
    pub digest_next_actions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryOverviewOptions {
    pub repo_full_name: Option<String>,
    pub latest_limit: usize,
    pub activity_days: i64,
}

impl Default for MemoryOverviewOptions {
    fn default() -> Self {
        Self {
            repo_full_name: None,
            latest_limit: DEFAULT_LATEST_LIMIT,
            activity_days: DEFAULT_ACTIVITY_DAYS,
        }
    }
}

pub async fn load_memory_overview(
    pool: &SqlitePool,
    options: MemoryOverviewOptions,
) -> Result<MemoryOverview> {
    let latest_limit = normalize_limit(options.latest_limit);
    let activity_days = options.activity_days.max(1);
    let repo_full_name = normalized_repo(options.repo_full_name);

    let inbox = load_memory_inbox(pool, latest_limit).await?;
    let pending_items = load_memory_items(
        pool,
        MemoryListFilter {
            state: Some("pending".to_owned()),
            kind: None,
            repo_full_name: None,
            query: None,
            limit: latest_limit,
        },
    )
    .await?;
    let activity = load_memory_activity(
        pool,
        MemoryActivityFilter {
            rule_id: None,
            repo_full_name: repo_full_name.clone(),
            days: activity_days,
            limit: latest_limit,
        },
    )
    .await?;
    let digest = load_memory_digest(pool, latest_limit).await?;
    let active_for_repo = active_rule_count_for_repo(pool, repo_full_name.as_deref()).await?;
    let paused = load_paused_overview(pool, latest_limit).await?;

    let remembered = RememberedOverview {
        available: inbox.active_rule_count(),
        active_total: inbox.active_rule_count(),
        active_for_repo,
        latest: inbox
            .active_rules
            .latest
            .iter()
            .map(OverviewRule::from_memory_rule)
            .collect(),
    };
    let needs_review = NeedsReviewOverview {
        local_drafts: inbox.local_draft_count(),
        local_discoveries: inbox.session_mined_count(),
        autopilot_needs_review_groups: i64::try_from(digest.counts.needs_review_groups)
            .unwrap_or(i64::MAX),
        latest: pending_items
            .items
            .into_iter()
            .map(OverviewReviewItem::from_memory_item)
            .collect(),
    };
    let sync = SyncOverview {
        logged_in: false,
        approved_session_candidates_pending_upload: inbox.memory_candidates_pending(),
        activity_records_pending_upload: inbox.cloud_observations_pending()
            + inbox.observation_events_pending(),
    };
    let activity = ActivityOverview {
        window_days: activity.days,
        recall_calls: activity.summary.calls,
        empty_recalls: activity.summary.empty_calls,
        rules_surfaced: activity.summary.rules_served,
    };
    let next = next_action(&remembered, &needs_review, &paused, &sync);
    let debug = MemoryOverviewDebug {
        repo_full_name,
        latest_limit,
        inbox_warnings: inbox.warnings,
        digest_next_actions: digest.next_actions,
    };

    Ok(MemoryOverview {
        schema_version: MEMORY_OVERVIEW_SCHEMA_VERSION.to_owned(),
        remembered,
        needs_review,
        paused,
        sync,
        activity,
        next,
        debug,
    })
}

impl OverviewRule {
    fn from_memory_rule(rule: &MemoryRuleItem) -> Self {
        Self {
            item_id: format!("rule:{}", rule.id),
            rule_id: rule.id.clone(),
            title: rule.name.clone(),
            origin: rule.origin.clone(),
            source_repo: rule.source_repo.clone(),
            file_patterns: rule.file_patterns.clone(),
            updated_at: rule.updated_at.clone(),
        }
    }
}

impl OverviewReviewItem {
    fn from_memory_item(item: crate::memory_inbox::MemoryListItem) -> Self {
        Self {
            item_id: item.item_id,
            kind: item.kind,
            title: item.title,
            origin: item.origin,
            source_repo: item.source_repo,
            file_patterns: item.file_patterns,
            updated_at: item.updated_at,
            review_hint: item.review_hint,
            commands: OverviewReviewCommands {
                show: item.commands.show,
                approve: item.commands.approve,
                reject: item.commands.reject,
            },
        }
    }
}

async fn active_rule_count_for_repo(pool: &SqlitePool, repo: Option<&str>) -> Result<Option<i64>> {
    let Some(repo) = repo else {
        return Ok(None);
    };
    let count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM skills \
         WHERE status = 'active' AND lower(source_repo) = lower(?1)",
    )
    .bind(repo)
    .fetch_one(pool)
    .await?;
    Ok(Some(count))
}

async fn load_paused_overview(pool: &SqlitePool, latest_limit: usize) -> Result<PausedOverview> {
    let count =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM skills WHERE status = 'disabled'")
            .fetch_one(pool)
            .await?;
    let rows = sqlx::query(
        "SELECT id, name, origin, source_repo, file_patterns, \
                COALESCE(updated_at, installed_at) AS updated_at \
         FROM skills \
         WHERE status = 'disabled' \
         ORDER BY datetime(COALESCE(updated_at, installed_at)) DESC, id ASC \
         LIMIT ?1",
    )
    .bind(limit_i64(latest_limit))
    .fetch_all(pool)
    .await?;

    let latest = rows
        .into_iter()
        .map(|row| {
            let id: String = row.try_get("id").unwrap_or_default();
            let file_patterns: Option<String> = row.try_get("file_patterns").ok().flatten();
            OverviewRule {
                item_id: format!("rule:{id}"),
                rule_id: id,
                title: row.try_get("name").unwrap_or_default(),
                origin: row.try_get("origin").unwrap_or_default(),
                source_repo: row.try_get("source_repo").ok().flatten(),
                file_patterns: parse_string_list(file_patterns.as_deref()),
                updated_at: row.try_get("updated_at").unwrap_or_default(),
            }
        })
        .collect();

    Ok(PausedOverview { count, latest })
}

fn next_action(
    remembered: &RememberedOverview,
    needs_review: &NeedsReviewOverview,
    paused: &PausedOverview,
    sync: &SyncOverview,
) -> MemoryOverviewNextAction {
    if needs_review.local_drafts > 0
        || needs_review.local_discoveries > 0
        || needs_review.autopilot_needs_review_groups > 0
    {
        return MemoryOverviewNextAction {
            kind: "review".to_owned(),
            label: "Review memory suggestions".to_owned(),
            command: Some("difflore memory review".to_owned()),
            reason: "Some local memory is waiting for approval before agents can use it."
                .to_owned(),
        };
    }

    if sync.approved_session_candidates_pending_upload > 0
        || sync.activity_records_pending_upload > 0
    {
        return MemoryOverviewNextAction {
            kind: "sync".to_owned(),
            label: "Sync memory activity".to_owned(),
            command: Some("difflore memory sync".to_owned()),
            reason: "Approved memory activity is queued for upload.".to_owned(),
        };
    }

    if remembered.available == 0 {
        return MemoryOverviewNextAction {
            kind: "import_or_review".to_owned(),
            label: "Import or review memory".to_owned(),
            command: Some("difflore memory import-agent-files".to_owned()),
            reason: "No active memory is available yet; import agent files or review discoveries."
                .to_owned(),
        };
    }

    if remembered.active_for_repo == Some(0) {
        return MemoryOverviewNextAction {
            kind: "add_repo_memory".to_owned(),
            label: "Add memory for this repo".to_owned(),
            command: Some("difflore memory remember --title <title> --body <body>".to_owned()),
            reason: "Memory exists on this machine, but none is scoped to the current repo."
                .to_owned(),
        };
    }

    if paused.count > 0 {
        return MemoryOverviewNextAction {
            kind: "ready_with_paused".to_owned(),
            label: "Memory is ready".to_owned(),
            command: None,
            reason: "Active memory is available; paused rules stay out of agent recall.".to_owned(),
        };
    }

    MemoryOverviewNextAction {
        kind: "ready".to_owned(),
        label: "Memory is ready".to_owned(),
        command: None,
        reason: "Active memory is available for agent recall.".to_owned(),
    }
}

fn normalized_repo(repo: Option<String>) -> Option<String> {
    repo.map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn parse_string_list(raw: Option<&str>) -> Vec<String> {
    raw.map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(|value| serde_json::from_str::<Vec<String>>(value).ok())
        .unwrap_or_default()
}

fn normalize_limit(limit: usize) -> usize {
    if limit == 0 {
        DEFAULT_LATEST_LIMIT
    } else {
        limit.min(MAX_LATEST_LIMIT)
    }
}

fn limit_i64(limit: usize) -> i64 {
    i64::try_from(normalize_limit(limit)).unwrap_or(MAX_LATEST_LIMIT as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud::outbox::kind;
    use crate::cloud::session_mined::{SessionMinedCandidate, SessionMinedCandidateArgs};
    use crate::infra::git::RepoScope;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;

    async fn migrated_pool() -> SqlitePool {
        let _home = crate::infra::db::shared_test_home();
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")
            .expect("sqlite opts")
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .expect("connect migrated sqlite");
        crate::infra::db::run_migrations(&pool)
            .await
            .expect("migrate");
        pool
    }

    fn options_for_repo(repo: Option<&str>) -> MemoryOverviewOptions {
        MemoryOverviewOptions {
            repo_full_name: repo.map(str::to_owned),
            latest_limit: 5,
            activity_days: 30,
        }
    }

    async fn insert_skill(
        pool: &SqlitePool,
        id: &str,
        name: &str,
        status: &str,
        source_repo: Option<&str>,
    ) {
        sqlx::query(
            "INSERT INTO skills \
                (id, name, source, directory, version, description, origin, source_repo, file_patterns, status) \
             VALUES (?1, ?2, 'local', ?1, '1.0.0', ?3, 'manual', ?4, '[\"src/**/*.rs\"]', ?5)",
        )
        .bind(id)
        .bind(name)
        .bind(format!("{name} body"))
        .bind(source_repo)
        .bind(status)
        .execute(pool)
        .await
        .expect("insert skill");
    }

    fn candidate(title: &str) -> SessionMinedCandidate {
        SessionMinedCandidate::try_new(SessionMinedCandidateArgs {
            session_id: "sess-1".to_owned(),
            ts_ms: 1_714_000_000_000,
            source_repo: RepoScope::canonical("owner/repo").expect("scope"),
            title: title.to_owned(),
            body: "Prefer typed parsing for queue payloads.".to_owned(),
            file_patterns: vec!["src/**/*.rs".to_owned()],
            gate_model: "claude:haiku".to_owned(),
            gate_verdict: "KEEP".to_owned(),
        })
        .expect("candidate")
    }

    #[tokio::test]
    async fn memory_overview_empty_db_points_to_import_or_review() {
        let pool = migrated_pool().await;

        let overview = load_memory_overview(&pool, options_for_repo(None))
            .await
            .expect("overview");

        assert_eq!(overview.remembered.available, 0);
        assert_eq!(overview.remembered.active_total, 0);
        assert_eq!(overview.remembered.active_for_repo, None);
        assert_eq!(overview.needs_review.local_drafts, 0);
        assert_eq!(overview.needs_review.local_discoveries, 0);
        assert_eq!(overview.paused.count, 0);
        assert_eq!(overview.next.kind, "import_or_review");
        assert!(
            overview.next.reason.contains("import") || overview.next.reason.contains("review"),
            "next action should mention import/review: {:?}",
            overview.next
        );
    }

    #[tokio::test]
    async fn memory_overview_separates_machine_active_from_repo_scoped_active() {
        let pool = migrated_pool().await;
        insert_skill(&pool, "rule-global", "Global reminder", "active", None).await;
        insert_skill(
            &pool,
            "rule-other",
            "Other repo reminder",
            "active",
            Some("other/repo"),
        )
        .await;

        let overview = load_memory_overview(&pool, options_for_repo(Some("owner/repo")))
            .await
            .expect("overview");

        assert_eq!(overview.remembered.available, 2);
        assert_eq!(overview.remembered.active_total, 2);
        assert_eq!(overview.remembered.active_for_repo, Some(0));
        assert_eq!(overview.remembered.latest.len(), 2);
        assert_eq!(overview.next.kind, "add_repo_memory");
    }

    #[tokio::test]
    async fn memory_overview_keeps_drafts_and_discoveries_in_needs_review_not_sync() {
        let pool = migrated_pool().await;
        insert_skill(
            &pool,
            "draft-typed-parsing",
            "Draft typed parsing",
            "pending",
            Some("owner/repo"),
        )
        .await;
        let discovery = candidate("Session discovery");
        let discovery_hash = discovery.content_hash.clone();
        sqlx::query(
            "INSERT INTO cloud_outbox (kind, payload_json, status, retry_count, created_at) \
             VALUES (?1, ?2, 'pending', 0, 42)",
        )
        .bind(kind::SESSION_MINED_CANDIDATE)
        .bind(serde_json::to_string(&discovery).expect("payload"))
        .execute(&pool)
        .await
        .expect("insert discovery");

        let overview = load_memory_overview(&pool, options_for_repo(Some("owner/repo")))
            .await
            .expect("overview");

        assert_eq!(overview.needs_review.local_drafts, 1);
        assert_eq!(overview.needs_review.local_discoveries, 1);
        assert_eq!(overview.sync.approved_session_candidates_pending_upload, 0);
        assert_eq!(overview.next.kind, "review");
        assert!(
            overview.needs_review.latest.iter().any(|item| {
                item.item_id == "draft:draft-typed-parsing" && item.kind == "draft"
            })
        );
        assert!(overview.needs_review.latest.iter().any(|item| {
            item.item_id == format!("session:{discovery_hash}") && item.kind == "candidate"
        }));
    }

    #[tokio::test]
    async fn memory_overview_places_disabled_rules_in_paused_not_needs_review() {
        let pool = migrated_pool().await;
        insert_skill(
            &pool,
            "rule-paused",
            "Paused typed parsing",
            "disabled",
            Some("owner/repo"),
        )
        .await;

        let overview = load_memory_overview(&pool, options_for_repo(Some("owner/repo")))
            .await
            .expect("overview");

        assert_eq!(overview.paused.count, 1);
        assert_eq!(overview.paused.latest[0].rule_id, "rule-paused");
        assert_eq!(overview.needs_review.local_drafts, 0);
        assert!(overview.needs_review.latest.is_empty());
    }
}
