use super::dedup::file_patterns_match;
#[cfg(test)]
use super::dedup::{RECENT_RULE_FIRE_WINDOW_MS, event_content_hash};
use super::events::{
    AcceptedFixOutcomeRuleSummary, AcceptedRecallLinkSummary, ActualCitationSummary, CitedEdit,
    ObservationEvent, ObservationUploadIssue, PriorRuleUseLinks, RuleFireSnapshot,
};
use crate::error::InternalResultExt as _;
#[cfg(test)]
use chrono::DateTime;
use chrono::Utc;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

const OUTBOX_DB_NAME: &str = "observations_outbox.db";
const MAX_QUEUE_ROWS: i64 = 10_000;
/// SQL `LIMIT` for one flush claim, as `i64` for sqlx binding. Derived from the
/// shared [`crate::cloud::outbox_core::MAX_OBSERVATION_BATCH`] so the emitter
/// and the outbox drainer never disagree on the cloud batch ceiling.
pub(super) const MAX_FLUSH_BATCH: i64 = crate::cloud::outbox_core::MAX_OBSERVATION_BATCH as i64;

// Re-exported under `pub(super)` so `sync.rs` and this module's tests import
// them via `super::storage::{...}`.
pub(super) use crate::cloud::outbox_core::{now_unix_ms, truncate};

/// One decoded *accepted* `fix_outcome` row, the unit the three
/// accepted-outcome aggregators fold over.
struct AcceptedOutcome {
    rule_id: String,
    session_id: String,
    file_path: Option<String>,
    occurred_at_ms: i64,
    mcp_serve_event_ids: Vec<i64>,
}

/// Per-caller SQL/IO error-message strings for
/// [`ObservationEmitter::fold_accepted_outcomes`], so each aggregator keeps its
/// own distinct error text.
struct AcceptedOutcomeCtx {
    select_err: &'static str,
    read_err: &'static str,
}

#[derive(Debug, Clone)]
pub struct ObservationEmitter {
    pool: SqlitePool,
}

impl ObservationEmitter {
    pub(super) const fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub async fn open_default() -> crate::Result<Self> {
        let path = crate::infra::paths::data_home()?.join(OUTBOX_DB_NAME);
        Self::open_at(&path).await
    }

    pub async fn open_at(path: &Path) -> crate::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).internal()?;
        }

        let opts = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(3)
            .connect_with(opts)
            .await
            .map_err(|e| crate::CoreError::internal(format!("open observations outbox: {e}")))?;
        migrate(&pool).await?;
        Ok(Self { pool })
    }

    pub async fn enqueue(&self, event: &ObservationEvent) -> crate::Result<i64> {
        if !crate::cloud::capture::capture_enabled() {
            return Ok(0);
        }
        let payload_json = serde_json::to_string(event)
            .map_err(|e| crate::CoreError::internal(format!("serialize observation: {e}")))?;
        let rule_ids = event.rule_ids();
        let rule_ids_json = serde_json::to_string(&rule_ids)
            .map_err(|e| crate::CoreError::internal(format!("serialize rule ids: {e}")))?;
        let now = now_unix_ms();
        let result = sqlx::query(
            "INSERT INTO observation_events \
             (event_type, rule_id, rule_ids_json, session_id, file_path, occurred_at_ms, \
              payload_json, status, retry_count, next_attempt_at_ms, created_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'pending', 0, ?8, ?9)",
        )
        .bind(event.event_type())
        .bind(event.rule_id())
        .bind(rule_ids_json)
        .bind(event.session_id())
        .bind(event.file_path())
        .bind(event.occurred_at_ms())
        .bind(payload_json)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(|e| crate::CoreError::internal(format!("enqueue observation: {e}")))?;
        let _ = self.cap_queue().await;
        Ok(result.last_insert_rowid())
    }

    pub async fn matching_recent_rule_ids(
        &self,
        app_db: &SqlitePool,
        session_id: &str,
        file_path: &str,
        within_ms: i64,
    ) -> crate::Result<Vec<String>> {
        let cutoff = now_unix_ms() - within_ms.max(1);
        let rows = sqlx::query(
            "SELECT payload_json FROM observation_events \
             WHERE event_type IN ('rule_fired', 'mcp_rule_served') AND occurred_at_ms >= ?1 \
               AND (session_id = ?2 OR session_id = '' OR session_id = 'mcp-server' OR ?2 = '') \
             ORDER BY occurred_at_ms DESC LIMIT 50",
        )
        .bind(cutoff)
        .bind(session_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| crate::CoreError::internal(format!("select recent rule fires: {e}")))?;

        let mut ordered_ids = Vec::<String>::new();
        for row in rows {
            let payload: String = row.try_get("payload_json").unwrap_or_default();
            let Ok(
                ObservationEvent::RuleFired { rule_ids, .. }
                | ObservationEvent::McpRuleServed { rule_ids, .. },
            ) = serde_json::from_str::<ObservationEvent>(&payload)
            else {
                continue;
            };
            for id in rule_ids {
                if !ordered_ids.iter().any(|existing| existing == &id) {
                    ordered_ids.push(id);
                }
            }
        }

        if ordered_ids.is_empty() {
            return Ok(Vec::new());
        }

        let rules_json = serde_json::to_string(&ordered_ids)
            .map_err(|e| crate::CoreError::internal(format!("serialize ids: {e}")))?;
        let rows = sqlx::query(
            "SELECT id, file_patterns FROM skills \
             WHERE id IN (SELECT value FROM json_each(?1))",
        )
        .bind(rules_json)
        .fetch_all(app_db)
        .await
        .map_err(|e| crate::CoreError::internal(format!("load rule patterns: {e}")))?;

        let mut matches = Vec::new();
        for row in rows {
            let id: String = row.try_get("id").unwrap_or_default();
            let file_patterns: Option<String> = row
                .try_get::<Option<String>, _>("file_patterns")
                .unwrap_or(None);
            if id.is_empty() {
                continue;
            }
            if file_patterns_match(file_patterns.as_deref(), file_path)
                && ordered_ids.iter().any(|existing| existing == &id)
            {
                matches.push(id);
            }
        }

        matches.sort_by_key(|id| {
            ordered_ids
                .iter()
                .position(|candidate| candidate == id)
                .unwrap_or(usize::MAX)
        });
        Ok(matches)
    }

    pub async fn strongest_recent_rule_id(
        &self,
        app_db: &SqlitePool,
        session_id: &str,
        file_path: &str,
        within_ms: i64,
    ) -> crate::Result<Option<String>> {
        Ok(self
            .matching_recent_rule_ids(app_db, session_id, file_path, within_ms)
            .await?
            .into_iter()
            .next())
    }

    /// Outbox row ids of `mcp_rule_served` events that include `rule_id`
    /// and either match the same `repo_full_name` or `file_path`, within
    /// `window_ms` before `accepted_at_ms`. Used to populate
    /// [`ObservationEvent::FixOutcome::mcp_serve_event_ids`] at enqueue
    /// time so the audited cross-link
    /// `acceptedOutcomesLinkedToMcpRuleServe` survives the session-id
    /// asymmetry between hook-emitted serve events (`session_id="hook"`)
    /// and agent-emitted accepted edits.
    pub async fn recent_mcp_serve_event_ids(
        &self,
        rule_id: &str,
        repo_full_name: Option<&str>,
        file_path: Option<&str>,
        accepted_at_ms: i64,
        window_ms: i64,
    ) -> crate::Result<Vec<i64>> {
        let rule_id = rule_id.trim();
        if rule_id.is_empty() {
            return Ok(Vec::new());
        }
        let start_ms = accepted_at_ms.saturating_sub(window_ms.max(1));
        let repo = repo_full_name.unwrap_or("").trim();
        let file = file_path.unwrap_or("").trim();
        if repo.is_empty() && file.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(
            "SELECT id, payload_json FROM observation_events \
             WHERE event_type = 'mcp_rule_served' \
               AND occurred_at_ms BETWEEN ?1 AND ?2 \
               AND ((?3 <> '' AND file_path = ?3) OR (?4 <> ''))",
        )
        .bind(start_ms)
        .bind(accepted_at_ms)
        .bind(file)
        .bind(repo)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| crate::CoreError::internal(format!("select recent mcp serve events: {e}")))?;

        let mut ids = Vec::new();
        for row in rows {
            let id: i64 = row.try_get("id").unwrap_or_default();
            let payload: String = row.try_get("payload_json").unwrap_or_default();
            let Ok(ObservationEvent::McpRuleServed {
                rule_ids,
                repo_full_name: serve_repo,
                file_path: serve_file,
                ..
            }) = serde_json::from_str::<ObservationEvent>(&payload)
            else {
                continue;
            };
            if !rule_ids.iter().any(|id| id == rule_id) {
                continue;
            }
            // Repo-scope match is the strongest signal — the hook serves
            // rules in the context of a specific working directory so the
            // detected `owner/repo` is a reliable bridge across session
            // boundaries. File-path match is the secondary bridge for
            // serves that did not carry repo metadata.
            let repo_matches =
                !repo.is_empty() && serve_repo.as_deref().is_some_and(|name| name == repo);
            let file_matches =
                !file.is_empty() && serve_file.as_deref().is_some_and(|name| name == file);
            if repo_matches || file_matches {
                ids.push(id);
            }
        }
        Ok(ids)
    }

    pub async fn latest_rule_fire_for_session(
        &self,
        session_id: &str,
    ) -> crate::Result<Option<RuleFireSnapshot>> {
        let row = sqlx::query(
            "SELECT payload_json FROM observation_events \
             WHERE event_type = 'rule_fired' \
               AND (session_id = ?1 OR session_id = '' OR session_id = 'mcp-server' OR ?1 = '') \
             ORDER BY occurred_at_ms DESC, id DESC LIMIT 1",
        )
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| crate::CoreError::internal(format!("select latest rule fire: {e}")))?;

        let Some(row) = row else {
            return Ok(None);
        };
        let payload: String = row.try_get("payload_json").unwrap_or_default();
        let Ok(ObservationEvent::RuleFired {
            rule_ids,
            file_path,
            ..
        }) = serde_json::from_str::<ObservationEvent>(&payload)
        else {
            return Ok(None);
        };
        Ok(Some(RuleFireSnapshot {
            rule_ids,
            file_path,
        }))
    }

    pub async fn cited_edits_for_session(&self, session_id: &str) -> crate::Result<Vec<CitedEdit>> {
        let rows = sqlx::query(
            "SELECT DISTINCT rule_id, file_path FROM observation_events \
             WHERE event_type = 'rule_cited_in_edit' AND session_id = ?1 \
               AND rule_id IS NOT NULL AND file_path IS NOT NULL",
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| crate::CoreError::internal(format!("select cited edits: {e}")))?;

        Ok(rows
            .into_iter()
            .filter_map(|row| {
                let rule_id: String = row.try_get("rule_id").ok()?;
                let file_path: String = row.try_get("file_path").ok()?;
                if rule_id.is_empty() || file_path.is_empty() {
                    None
                } else {
                    Some(CitedEdit { rule_id, file_path })
                }
            })
            .collect())
    }

    pub async fn has_fix_outcome(&self, session_id: &str, rule_id: &str) -> crate::Result<bool> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM observation_events \
             WHERE event_type = 'fix_outcome' AND session_id = ?1 AND rule_id = ?2",
        )
        .bind(session_id)
        .bind(rule_id)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| crate::CoreError::internal(format!("count fix outcomes: {e}")))?;
        Ok(count > 0)
    }

    /// Window start for an `N`-day accepted-outcome lookback, in unix ms.
    fn accepted_outcomes_since_ms(days: i64) -> i64 {
        Utc::now()
            .checked_sub_signed(chrono::Duration::days(days.max(1)))
            .unwrap_or_else(Utc::now)
            .timestamp_millis()
    }

    /// Load every *accepted* `fix_outcome` row within the last `days` days,
    /// decoded into its component fields.
    ///
    /// Does the shared SELECT + `payload_json` decode + `accepted: true`
    /// filter, skipping non-`FixOutcome` / rejected rows. It deliberately does
    /// NOT apply the `rule_id.trim()` / empty-skip or the `prior_rule_use_links`
    /// cross-link — those differ per caller, so each caller loops over the
    /// decoded outcomes itself. `ctx` supplies the per-caller SQL/IO error
    /// strings.
    async fn fold_accepted_outcomes(
        &self,
        days: i64,
        ctx: &AcceptedOutcomeCtx,
    ) -> crate::Result<Vec<AcceptedOutcome>> {
        let since_ms = Self::accepted_outcomes_since_ms(days);
        let rows = sqlx::query(
            "SELECT payload_json FROM observation_events \
             WHERE event_type = 'fix_outcome' AND occurred_at_ms >= ?1",
        )
        .bind(since_ms)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| crate::CoreError::internal(format!("{}: {e}", ctx.select_err)))?;

        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let payload_json: String = row
                .try_get("payload_json")
                .map_err(|e| crate::CoreError::internal(format!("{}: {e}", ctx.read_err)))?;
            let Ok(ObservationEvent::FixOutcome {
                rule_id,
                session_id,
                file_path,
                accepted: true,
                occurred_at,
                mcp_serve_event_ids,
            }) = serde_json::from_str::<ObservationEvent>(&payload_json)
            else {
                continue;
            };
            out.push(AcceptedOutcome {
                rule_id,
                session_id,
                file_path,
                occurred_at_ms: occurred_at.timestamp_millis(),
                mcp_serve_event_ids,
            });
        }
        Ok(out)
    }

    pub async fn accepted_fix_outcome_count(&self, days: i64) -> crate::Result<i64> {
        // The count path never computes prior-recall links, keeping a single
        // SELECT.
        let outcomes = self
            .fold_accepted_outcomes(
                days,
                &AcceptedOutcomeCtx {
                    select_err: "select accepted fix outcome count",
                    read_err: "read accepted fix outcome payload",
                },
            )
            .await?;
        Ok(i64::try_from(outcomes.len()).unwrap_or(i64::MAX))
    }

    pub async fn accepted_recall_link_summary(
        &self,
        days: i64,
        lookback_days: i64,
    ) -> crate::Result<AcceptedRecallLinkSummary> {
        let lookback_ms = chrono::Duration::days(lookback_days.max(1))
            .num_milliseconds()
            .max(1);
        let outcomes = self
            .fold_accepted_outcomes(
                days,
                &AcceptedOutcomeCtx {
                    select_err: "select accepted recall link candidates",
                    read_err: "read accepted recall link payload",
                },
            )
            .await?;

        let mut summary = AcceptedRecallLinkSummary::default();
        for outcome in outcomes {
            summary.accepted_outcomes += 1;
            let mut links = self
                .prior_rule_use_links(
                    &outcome.rule_id,
                    &outcome.session_id,
                    outcome.file_path.as_deref(),
                    outcome.occurred_at_ms,
                    lookback_ms,
                )
                .await?;
            // Inline serve-event ids close the cross-link when the session_id
            // and file_path heuristics miss (e.g. serve recorded from the hook
            // with `session_id="hook"`, accepted edit with the agent session
            // id).
            if !outcome.mcp_serve_event_ids.is_empty() {
                links.mcp_rule_serve = true;
            }
            if links.any() {
                summary.linked_to_prior_recall += 1;
            }
            if links.rule_recall {
                summary.linked_to_rule_recall += 1;
            }
            if links.mcp_rule_serve {
                summary.linked_to_mcp_rule_serve += 1;
            }
            if links.edit_attribution {
                summary.linked_to_edit_attribution += 1;
            }
        }

        Ok(summary)
    }

    pub async fn accepted_fix_outcome_rule_summaries(
        &self,
        days: i64,
        lookback_days: i64,
    ) -> crate::Result<Vec<AcceptedFixOutcomeRuleSummary>> {
        let lookback_ms = chrono::Duration::days(lookback_days.max(1))
            .num_milliseconds()
            .max(1);
        let outcomes = self
            .fold_accepted_outcomes(
                days,
                &AcceptedOutcomeCtx {
                    select_err: "select accepted fix outcome rule summaries",
                    read_err: "read accepted fix outcome summary payload",
                },
            )
            .await?;

        let mut summaries: HashMap<String, AcceptedFixOutcomeRuleSummary> = HashMap::new();
        for outcome in outcomes {
            let rule_id = outcome.rule_id.trim();
            if rule_id.is_empty() {
                continue;
            }

            let occurred_at_ms = outcome.occurred_at_ms;
            let mut links = self
                .prior_rule_use_links(
                    rule_id,
                    &outcome.session_id,
                    outcome.file_path.as_deref(),
                    occurred_at_ms,
                    lookback_ms,
                )
                .await?;
            // Mirror the per-rule audit so the inline serve-event ids
            // recorded at hook fire time still register the cross-link.
            if !outcome.mcp_serve_event_ids.is_empty() {
                links.mcp_rule_serve = true;
            }
            let summary = summaries.entry(rule_id.to_owned()).or_insert_with(|| {
                AcceptedFixOutcomeRuleSummary {
                    rule_id: rule_id.to_owned(),
                    ..AcceptedFixOutcomeRuleSummary::default()
                }
            });
            summary.accepted_outcomes += 1;
            if links.any() {
                summary.linked_to_prior_recall += 1;
            }
            if links.rule_recall {
                summary.linked_to_rule_recall += 1;
            }
            if links.mcp_rule_serve {
                summary.linked_to_mcp_rule_serve += 1;
            }
            if links.edit_attribution {
                summary.linked_to_edit_attribution += 1;
            }
            if occurred_at_ms >= summary.latest_occurred_at_ms {
                summary.latest_occurred_at_ms = occurred_at_ms;
                if let Some(file) = outcome
                    .file_path
                    .as_deref()
                    .map(str::trim)
                    .filter(|file| !file.is_empty())
                {
                    summary.sample_file = Some(file.to_owned());
                }
            }
        }

        let mut out: Vec<_> = summaries.into_values().collect();
        out.sort_by(|a, b| {
            b.accepted_outcomes
                .cmp(&a.accepted_outcomes)
                .then(b.linked_to_prior_recall.cmp(&a.linked_to_prior_recall))
                .then(b.latest_occurred_at_ms.cmp(&a.latest_occurred_at_ms))
                .then(a.rule_id.cmp(&b.rule_id))
        });
        Ok(out)
    }

    async fn prior_rule_use_links(
        &self,
        rule_id: &str,
        session_id: &str,
        file_path: Option<&str>,
        accepted_at_ms: i64,
        lookback_ms: i64,
    ) -> crate::Result<PriorRuleUseLinks> {
        if rule_id.trim().is_empty() {
            return Ok(PriorRuleUseLinks::default());
        }
        let start_ms = accepted_at_ms.saturating_sub(lookback_ms);
        let file_path = file_path.unwrap_or("");
        // `rule_cited_in_edit` is the conservative bridge for clients
        // that do not pass a stable MCP session id: the hook emits it
        // only after a recent rule recall was matched to the edited file.
        let rows = sqlx::query(
            "SELECT event_type, rule_ids_json FROM observation_events \
             WHERE event_type IN ('rule_fired', 'mcp_rule_served', 'rule_cited_in_edit') \
               AND occurred_at_ms BETWEEN ?1 AND ?2 \
               AND (session_id = ?3 OR (?4 <> '' AND file_path = ?4))",
        )
        .bind(start_ms)
        .bind(accepted_at_ms)
        .bind(session_id)
        .bind(file_path)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| {
            crate::CoreError::internal(format!("select prior recall link candidates: {e}"))
        })?;

        let mut links = PriorRuleUseLinks::default();
        for row in rows {
            let event_type: String = row.try_get("event_type").map_err(|e| {
                crate::CoreError::internal(format!("read prior recall event type: {e}"))
            })?;
            let raw: String = row.try_get("rule_ids_json").map_err(|e| {
                crate::CoreError::internal(format!("read prior recall rule ids: {e}"))
            })?;
            let Ok(ids) = serde_json::from_str::<Vec<String>>(&raw) else {
                continue;
            };
            if ids.iter().any(|id| id == rule_id) {
                match event_type.as_str() {
                    "rule_fired" => links.rule_recall = true,
                    "mcp_rule_served" => links.mcp_rule_serve = true,
                    "rule_cited_in_edit" => links.edit_attribution = true,
                    _ => {}
                }
            }
        }
        Ok(links)
    }

    pub async fn pending_upload_count(&self) -> crate::Result<i64> {
        // Both `pending` and the in-flight `sending` state are unsent work; a
        // row briefly leased by a flusher still counts as a pending upload
        // (and a crashed sender's stranded `sending` row stays visible until
        // its lease expires and it is re-claimed).
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM observation_events WHERE status IN ('pending', 'sending')",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|e| crate::CoreError::internal(format!("count pending observation uploads: {e}")))
    }

    pub async fn has_rule_actual_citation(
        &self,
        session_id: &str,
        rule_id: &str,
    ) -> crate::Result<bool> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM observation_events \
             WHERE event_type = 'rule_actually_cited' AND session_id = ?1 AND rule_id = ?2",
        )
        .bind(session_id)
        .bind(rule_id)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| crate::CoreError::internal(format!("count actual rule citations: {e}")))?;
        Ok(count > 0)
    }

    pub async fn actual_citation_summary_since(
        &self,
        since_ms: i64,
    ) -> crate::Result<ActualCitationSummary> {
        let actual_citations: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM observation_events \
             WHERE event_type = 'rule_actually_cited' AND occurred_at_ms >= ?1",
        )
        .bind(since_ms)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| crate::CoreError::internal(format!("count actual rule citations: {e}")))?;

        let rule_fires: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM observation_events \
             WHERE event_type = 'rule_fired' AND occurred_at_ms >= ?1",
        )
        .bind(since_ms)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| crate::CoreError::internal(format!("count rule fires: {e}")))?;

        let pending_uploads: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM observation_events \
             WHERE event_type = 'rule_actually_cited' \
               AND status IN ('pending', 'sending') \
               AND occurred_at_ms >= ?1",
        )
        .bind(since_ms)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| {
            crate::CoreError::internal(format!("count pending actual rule citations: {e}"))
        })?;

        let pending_upload_issue = if pending_uploads > 0 {
            let last_error: Option<String> = sqlx::query_scalar(
                "SELECT last_error FROM observation_events \
                 WHERE event_type = 'rule_actually_cited' \
                   AND status IN ('pending', 'sending') \
                   AND occurred_at_ms >= ?1 \
                   AND last_error IS NOT NULL \
                 ORDER BY retry_count DESC, id DESC \
                 LIMIT 1",
            )
            .bind(since_ms)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| {
                crate::CoreError::internal(format!("load pending upload diagnosis: {e}"))
            })?;
            last_error.as_deref().map(classify_upload_issue)
        } else {
            None
        };

        Ok(ActualCitationSummary {
            actual_citations,
            rule_fires,
            pending_uploads,
            pending_upload_issue,
        })
    }

    pub async fn accepted_fix_proof_sources(
        &self,
        rule_ids: &[String],
    ) -> crate::Result<HashMap<String, String>> {
        let mut out = HashMap::new();
        if rule_ids.is_empty() {
            return Ok(out);
        }

        let placeholders = std::iter::repeat_n("?", rule_ids.len())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT rule_id, session_id, payload_json FROM observation_events \
             WHERE event_type = 'fix_outcome' \
               AND rule_id IN ({placeholders}) \
               AND rule_id IS NOT NULL"
        );
        let mut q = sqlx::query(&sql);
        for id in rule_ids {
            q = q.bind(id);
        }
        let rows = q.fetch_all(&self.pool).await.map_err(|e| {
            crate::CoreError::internal(format!("select accepted fix proof sources: {e}"))
        })?;

        for row in rows {
            let rule_id: String = row
                .try_get("rule_id")
                .map_err(|e| crate::CoreError::internal(format!("read proof rule_id: {e}")))?;
            let session_id: String = row
                .try_get("session_id")
                .map_err(|e| crate::CoreError::internal(format!("read proof session_id: {e}")))?;
            let payload_json: String = row
                .try_get("payload_json")
                .map_err(|e| crate::CoreError::internal(format!("read proof payload_json: {e}")))?;
            let Ok(ObservationEvent::FixOutcome { accepted: true, .. }) =
                serde_json::from_str::<ObservationEvent>(&payload_json)
            else {
                continue;
            };
            let source = accepted_proof_source_from_session_id(&session_id).to_owned();
            match out.get(&rule_id) {
                Some(existing) if proof_source_rank(existing) >= proof_source_rank(&source) => {}
                _ => {
                    out.insert(rule_id, source);
                }
            }
        }

        Ok(out)
    }

    pub(super) async fn cap_queue(&self) -> crate::Result<()> {
        // Trim in increasing order of value so the hard 10k cap never sheds
        // unsent signal while deletable rows remain: sent → abandoned →
        // (oldest-overall only as a last resort, when nothing but
        // pending/parked rows are left).
        let mut overflow = self.queue_overflow().await?;
        if overflow <= 0 {
            return Ok(());
        }

        sqlx::query(
            "DELETE FROM observation_events WHERE id IN ( \
               SELECT id FROM observation_events \
               WHERE status = 'sent' ORDER BY sent_at_ms ASC, id ASC LIMIT ?1 \
             )",
        )
        .bind(overflow)
        .execute(&self.pool)
        .await
        .map_err(|e| crate::CoreError::internal(format!("trim sent observations: {e}")))?;

        overflow = self.queue_overflow().await?;
        if overflow > 0 {
            sqlx::query(
                "DELETE FROM observation_events WHERE id IN ( \
                   SELECT id FROM observation_events \
                   WHERE status = 'abandoned' ORDER BY created_at_ms ASC, id ASC LIMIT ?1 \
                 )",
            )
            .bind(overflow)
            .execute(&self.pool)
            .await
            .map_err(|e| crate::CoreError::internal(format!("trim abandoned observations: {e}")))?;
        }

        overflow = self.queue_overflow().await?;
        if overflow > 0 {
            sqlx::query(
                "DELETE FROM observation_events WHERE id IN ( \
                   SELECT id FROM observation_events \
                   ORDER BY created_at_ms ASC, id ASC LIMIT ?1 \
                 )",
            )
            .bind(overflow)
            .execute(&self.pool)
            .await
            .map_err(|e| crate::CoreError::internal(format!("trim observations: {e}")))?;
        }
        Ok(())
    }

    /// Rows currently over the [`MAX_QUEUE_ROWS`] cap (negative when under).
    async fn queue_overflow(&self) -> crate::Result<i64> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM observation_events")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| crate::CoreError::internal(format!("count observation queue: {e}")))?;
        Ok(count - MAX_QUEUE_ROWS)
    }
}

pub async fn accepted_fix_proof_sources_default(
    rule_ids: &[String],
) -> crate::Result<HashMap<String, String>> {
    ObservationEmitter::open_default()
        .await?
        .accepted_fix_proof_sources(rule_ids)
        .await
}

pub async fn actual_citation_summary_default(days: i64) -> crate::Result<ActualCitationSummary> {
    let since_ms = now_unix_ms().saturating_sub(days.max(1).saturating_mul(86_400_000));
    ObservationEmitter::open_default()
        .await?
        .actual_citation_summary_since(since_ms)
        .await
}

fn classify_upload_issue(error: &str) -> ObservationUploadIssue {
    let lower = error.to_ascii_lowercase();
    if lower.contains("scope_missing") || lower.contains("missing required scope") {
        ObservationUploadIssue::MissingCloudScope
    } else if lower.contains("rate_limited")
        || lower.contains("rate limited")
        || lower.contains("429")
    {
        ObservationUploadIssue::RateLimited
    } else if lower.contains("invalid_batch") || lower.contains("invalid batch") {
        ObservationUploadIssue::InvalidBatch
    } else if lower.contains("returned 4") || lower.contains("forbidden") {
        ObservationUploadIssue::ServerRejected
    } else {
        ObservationUploadIssue::Unknown
    }
}

fn accepted_proof_source_from_session_id(session_id: &str) -> &'static str {
    if session_id.starts_with("cloud-fix-acceptance:") {
        "cloud_fix"
    } else if session_id.starts_with("historical-fix-acceptance:") {
        "historical_backfill"
    } else {
        "local_fix"
    }
}

fn proof_source_rank(source: &str) -> u8 {
    match source {
        "local_fix" => 4,
        "cloud_fix" => 3,
        "mixed" => 2,
        "historical_backfill" => 1,
        _ => 0,
    }
}

pub async fn enqueue_and_flush_default(
    event: ObservationEvent,
    client: &crate::cloud::client::CloudClient,
) -> crate::Result<(usize, usize)> {
    if !crate::cloud::capture::capture_enabled() {
        return Ok((0, 0));
    }
    let emitter = ObservationEmitter::open_default().await?;
    emitter.enqueue(&event).await?;
    emitter.flush_to_cloud(client).await
}

pub async fn enqueue_default(event: ObservationEvent) -> crate::Result<i64> {
    if !crate::cloud::capture::capture_enabled() {
        return Ok(0);
    }
    let emitter = ObservationEmitter::open_default().await?;
    emitter.enqueue(&event).await
}

async fn migrate(pool: &SqlitePool) -> crate::Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS observation_events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            event_type TEXT NOT NULL,
            rule_id TEXT,
            rule_ids_json TEXT NOT NULL DEFAULT '[]',
            session_id TEXT NOT NULL DEFAULT '',
            file_path TEXT,
            occurred_at_ms INTEGER NOT NULL,
            payload_json TEXT NOT NULL,
            status TEXT NOT NULL DEFAULT 'pending',
            retry_count INTEGER NOT NULL DEFAULT 0,
            next_attempt_at_ms INTEGER NOT NULL DEFAULT 0,
            last_error TEXT,
            created_at_ms INTEGER NOT NULL,
            sent_at_ms INTEGER
        )",
    )
    .execute(pool)
    .await
    .map_err(|e| crate::CoreError::internal(format!("create observation_events: {e}")))?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS observation_events_pending_idx \
         ON observation_events (status, next_attempt_at_ms, created_at_ms)",
    )
    .execute(pool)
    .await
    .map_err(|e| crate::CoreError::internal(format!("create pending index: {e}")))?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS observation_events_recent_idx \
         ON observation_events (event_type, session_id, occurred_at_ms)",
    )
    .execute(pool)
    .await
    .map_err(|e| crate::CoreError::internal(format!("create recent index: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_serializes_with_snake_case_tag() {
        let event = ObservationEvent::RuleFired {
            rule_ids: vec!["r1".to_owned()],
            file_path: Some("src/lib.rs".to_owned()),
            intent: Some("fix bug".to_owned()),
            session_id: "s".to_owned(),
            fired_at: DateTime::parse_from_rfc3339("2026-05-05T12:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        };
        let value = serde_json::to_value(event).unwrap();
        assert_eq!(value["event_type"], "rule_fired");
        assert_eq!(value["rule_ids"][0], "r1");
    }

    #[test]
    fn mcp_rule_served_event_serializes_low_sensitive_fields_only() {
        let event = ObservationEvent::McpRuleServed {
            tool: "search_rules".to_owned(),
            session_id: "s".to_owned(),
            repo_full_name: Some("acme/widgets".to_owned()),
            file_path: Some("src/lib.rs".to_owned()),
            query_hash: "fc2b18493e42be726bd550a895ec1cae48c9ca833f004b427077f1270432ff3b"
                .to_owned(),
            rule_ids: vec!["r1".to_owned(), "r2".to_owned()],
            top_k: 5,
            was_empty: false,
            strict_match_count: 2,
            estimated_tokens: 123,
            served_at: DateTime::parse_from_rfc3339("2026-05-05T12:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        };
        let value = serde_json::to_value(event).unwrap();

        assert_eq!(value["event_type"], "mcp_rule_served");
        assert_eq!(value["tool"], "search_rules");
        assert_eq!(value["session_id"], "s");
        assert_eq!(value["repo_full_name"], "acme/widgets");
        assert_eq!(value["file_path"], "src/lib.rs");
        assert_eq!(value["rule_ids"][0], "r1");
        assert_eq!(value["top_k"], 5);
        assert_eq!(value["was_empty"], false);
        assert_eq!(value["strict_match_count"], 2);
        assert_eq!(value["estimated_tokens"], 123);
        assert!(value.get("query").is_none());
        assert!(value.get("intent").is_none());
    }

    #[test]
    fn actual_citation_event_serializes_with_source_excerpt() {
        let event = ObservationEvent::RuleActuallyCited {
            rule_id: "r1".to_owned(),
            session_id: "s".to_owned(),
            file_path: Some("src/lib.rs".to_owned()),
            citation_excerpt: "applying Rule 1 (learned from acme/widgets)".to_owned(),
            cited_at: DateTime::parse_from_rfc3339("2026-05-05T12:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        };
        let value = serde_json::to_value(event).unwrap();

        assert_eq!(value["event_type"], "rule_actually_cited");
        assert_eq!(value["rule_id"], "r1");
        assert_eq!(value["file_path"], "src/lib.rs");
        assert!(
            value["citation_excerpt"]
                .as_str()
                .unwrap()
                .contains("learned from acme/widgets")
        );
    }

    #[test]
    fn fix_outcome_event_serializes_file_path_for_impact_proof() {
        let event = ObservationEvent::FixOutcome {
            rule_id: "r1".to_owned(),
            session_id: "s".to_owned(),
            file_path: Some("src/lib.rs".to_owned()),
            accepted: true,
            occurred_at: DateTime::parse_from_rfc3339("2026-05-05T12:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            mcp_serve_event_ids: Vec::new(),
        };
        let value = serde_json::to_value(event).unwrap();

        assert_eq!(value["event_type"], "fix_outcome");
        assert_eq!(value["rule_id"], "r1");
        assert_eq!(value["file_path"], "src/lib.rs");
        assert_eq!(value["accepted"], true);
        // Empty vec is skipped to keep cloud payloads identical to the
        // pre-link schema for outcomes that do not carry a serve link.
        assert!(value.get("mcp_serve_event_ids").is_none());
    }

    #[test]
    fn fix_outcome_event_serializes_mcp_serve_event_ids_when_linked() {
        let event = ObservationEvent::FixOutcome {
            rule_id: "r1".to_owned(),
            session_id: "s".to_owned(),
            file_path: Some("src/lib.rs".to_owned()),
            accepted: true,
            occurred_at: DateTime::parse_from_rfc3339("2026-05-05T12:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            mcp_serve_event_ids: vec![42, 43],
        };
        let value = serde_json::to_value(event).unwrap();
        assert_eq!(value["mcp_serve_event_ids"][0], 42);
        assert_eq!(value["mcp_serve_event_ids"][1], 43);
    }

    #[test]
    fn fix_outcome_event_deserialises_legacy_payload_without_mcp_serve_event_ids() {
        // Pre-cross-link rows in the local outbox do not carry the field.
        // Make sure we still parse them as `Vec::new()` rather than rejecting
        // the row (which would silently drop accepted-edit history).
        let raw = r#"{
            "event_type": "fix_outcome",
            "rule_id": "r1",
            "session_id": "s",
            "file_path": "src/lib.rs",
            "accepted": true,
            "occurred_at": "2026-05-05T12:00:00Z"
        }"#;
        let event: ObservationEvent = serde_json::from_str(raw).unwrap();
        match event {
            ObservationEvent::FixOutcome {
                mcp_serve_event_ids,
                ..
            } => assert!(mcp_serve_event_ids.is_empty()),
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn file_patterns_match_empty_as_universal_and_globs_specific_paths() {
        assert!(file_patterns_match(None, "src/lib.rs"));
        assert!(file_patterns_match(Some("[]"), "src/lib.rs"));
        assert!(file_patterns_match(
            Some(r#"["src/**/*.rs"]"#),
            "src/cloud/observations.rs"
        ));
        assert!(!file_patterns_match(
            Some(r#"["src/**/*.ts"]"#),
            "src/cloud/observations.rs"
        ));
    }

    #[tokio::test]
    async fn enqueue_persists_rule_fired_event() {
        let temp = tempfile::tempdir().unwrap();
        let emitter = ObservationEmitter::open_at(&temp.path().join("obs.db"))
            .await
            .unwrap();
        let id = emitter
            .enqueue(&ObservationEvent::RuleFired {
                rule_ids: vec!["r1".to_owned(), "r2".to_owned()],
                file_path: Some("src/lib.rs".to_owned()),
                intent: Some("edit".to_owned()),
                session_id: "s".to_owned(),
                fired_at: Utc::now(),
            })
            .await
            .unwrap();
        assert!(id > 0);
    }

    #[tokio::test]
    async fn retry_pending_uploads_now_makes_backed_off_rows_due() {
        let temp = tempfile::tempdir().unwrap();
        let emitter = ObservationEmitter::open_at(&temp.path().join("obs.db"))
            .await
            .unwrap();
        let id = emitter
            .enqueue(&ObservationEvent::RuleFired {
                rule_ids: vec!["r1".to_owned()],
                file_path: Some("src/lib.rs".to_owned()),
                intent: Some("edit".to_owned()),
                session_id: "s".to_owned(),
                fired_at: Utc::now(),
            })
            .await
            .unwrap();
        let future_ms = now_unix_ms() + 3_600_000;
        sqlx::query("UPDATE observation_events SET next_attempt_at_ms = ?1 WHERE id = ?2")
            .bind(future_ms)
            .bind(id)
            .execute(&emitter.pool)
            .await
            .unwrap();

        let reset = emitter.retry_pending_uploads_now().await.unwrap();
        let next_attempt: i64 =
            sqlx::query_scalar("SELECT next_attempt_at_ms FROM observation_events WHERE id = ?1")
                .bind(id)
                .fetch_one(&emitter.pool)
                .await
                .unwrap();

        assert_eq!(reset, 1);
        assert!(next_attempt <= now_unix_ms());
    }

    #[tokio::test]
    async fn pending_upload_count_tracks_unsent_events() {
        let temp = tempfile::tempdir().unwrap();
        let emitter = ObservationEmitter::open_at(&temp.path().join("obs.db"))
            .await
            .unwrap();
        let first = emitter
            .enqueue(&ObservationEvent::RuleFired {
                rule_ids: vec!["r1".to_owned()],
                file_path: Some("src/lib.rs".to_owned()),
                intent: Some("edit".to_owned()),
                session_id: "s".to_owned(),
                fired_at: Utc::now(),
            })
            .await
            .unwrap();
        let _second = emitter
            .enqueue(&ObservationEvent::McpRuleServed {
                tool: "search_rules".to_owned(),
                session_id: "s".to_owned(),
                repo_full_name: Some("acme/widgets".to_owned()),
                file_path: Some("src/lib.rs".to_owned()),
                query_hash: "fc2b18493e42be726bd550a895ec1cae48c9ca833f004b427077f1270432ff3b"
                    .to_owned(),
                rule_ids: vec!["r1".to_owned()],
                top_k: 5,
                was_empty: false,
                strict_match_count: 1,
                estimated_tokens: 123,
                served_at: Utc::now(),
            })
            .await
            .unwrap();

        assert_eq!(emitter.pending_upload_count().await.unwrap(), 2);

        emitter.mark_sent(first, now_unix_ms()).await.unwrap();

        assert_eq!(emitter.pending_upload_count().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn strongest_recent_rule_id_keeps_outcome_attribution_conservative() {
        let temp = tempfile::tempdir().unwrap();
        let emitter = ObservationEmitter::open_at(&temp.path().join("obs.db"))
            .await
            .unwrap();
        let app_db = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query("CREATE TABLE skills (id TEXT PRIMARY KEY, file_patterns TEXT)")
            .execute(&app_db)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO skills (id, file_patterns) VALUES \
             ('r1', '[\"context.go\"]'), \
             ('r2', '[\"**/*.go\"]'), \
             ('r3', '[\"**/*.rs\"]')",
        )
        .execute(&app_db)
        .await
        .unwrap();

        emitter
            .enqueue(&ObservationEvent::RuleFired {
                rule_ids: vec!["r1".to_owned(), "r2".to_owned(), "r3".to_owned()],
                file_path: Some("context.go".to_owned()),
                intent: Some("fix body size status".to_owned()),
                session_id: "s".to_owned(),
                fired_at: Utc::now(),
            })
            .await
            .unwrap();

        let rule_id = emitter
            .strongest_recent_rule_id(&app_db, "s", "context.go", RECENT_RULE_FIRE_WINDOW_MS)
            .await
            .unwrap();

        assert_eq!(rule_id.as_deref(), Some("r1"));
    }

    #[tokio::test]
    async fn strongest_recent_rule_id_accepts_mcp_rule_served_as_recall_proof() {
        let temp = tempfile::tempdir().unwrap();
        let emitter = ObservationEmitter::open_at(&temp.path().join("obs.db"))
            .await
            .unwrap();
        let app_db = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query("CREATE TABLE skills (id TEXT PRIMARY KEY, file_patterns TEXT)")
            .execute(&app_db)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO skills (id, file_patterns) VALUES \
             ('r-served', '[\"src/**/*.rs\"]'), \
             ('r-other', '[\"**/*.go\"]')",
        )
        .execute(&app_db)
        .await
        .unwrap();

        emitter
            .enqueue(&ObservationEvent::McpRuleServed {
                tool: "get_rules".to_owned(),
                session_id: "mcp-server".to_owned(),
                repo_full_name: Some("acme/widgets".to_owned()),
                file_path: None,
                query_hash: "fc2b18493e42be726bd550a895ec1cae48c9ca833f004b427077f1270432ff3b"
                    .to_owned(),
                rule_ids: vec!["r-served".to_owned(), "r-other".to_owned()],
                top_k: 2,
                was_empty: false,
                strict_match_count: 0,
                estimated_tokens: 200,
                served_at: Utc::now(),
            })
            .await
            .unwrap();

        let rule_id = emitter
            .strongest_recent_rule_id(
                &app_db,
                "agent-session",
                "src/lib.rs",
                RECENT_RULE_FIRE_WINDOW_MS,
            )
            .await
            .unwrap();

        assert_eq!(rule_id.as_deref(), Some("r-served"));
    }

    #[tokio::test]
    async fn latest_rule_fire_for_session_returns_ordered_rule_ids_and_file() {
        let temp = tempfile::tempdir().unwrap();
        let emitter = ObservationEmitter::open_at(&temp.path().join("obs.db"))
            .await
            .unwrap();

        emitter
            .enqueue(&ObservationEvent::RuleFired {
                rule_ids: vec!["old".to_owned()],
                file_path: Some("old.rs".to_owned()),
                intent: Some("old".to_owned()),
                session_id: "s".to_owned(),
                fired_at: Utc::now(),
            })
            .await
            .unwrap();
        emitter
            .enqueue(&ObservationEvent::RuleFired {
                rule_ids: vec!["r1".to_owned(), "r2".to_owned()],
                file_path: Some("src/lib.rs".to_owned()),
                intent: Some("edit".to_owned()),
                session_id: "s".to_owned(),
                fired_at: Utc::now(),
            })
            .await
            .unwrap();

        let latest = emitter
            .latest_rule_fire_for_session("s")
            .await
            .unwrap()
            .unwrap();

        assert_eq!(latest.rule_ids, vec!["r1", "r2"]);
        assert_eq!(latest.file_path.as_deref(), Some("src/lib.rs"));
    }

    #[tokio::test]
    async fn actual_citation_summary_counts_agent_citations_and_pending_uploads() {
        let temp = tempfile::tempdir().unwrap();
        let emitter = ObservationEmitter::open_at(&temp.path().join("obs.db"))
            .await
            .unwrap();
        emitter
            .enqueue(&ObservationEvent::RuleFired {
                rule_ids: vec!["r1".to_owned()],
                file_path: Some("src/lib.rs".to_owned()),
                intent: Some("edit".to_owned()),
                session_id: "s".to_owned(),
                fired_at: Utc::now(),
            })
            .await
            .unwrap();
        emitter
            .enqueue(&ObservationEvent::RuleActuallyCited {
                rule_id: "r1".to_owned(),
                session_id: "s".to_owned(),
                file_path: Some("src/lib.rs".to_owned()),
                citation_excerpt: "applying Rule 1 (learned from acme/widgets)".to_owned(),
                cited_at: Utc::now(),
            })
            .await
            .unwrap();

        let summary = emitter.actual_citation_summary_since(0).await.unwrap();

        assert_eq!(summary.actual_citations, 1);
        assert_eq!(summary.rule_fires, 1);
        assert_eq!(summary.pending_uploads, 1);
        assert_eq!(summary.pending_upload_issue, None);
    }

    #[test]
    fn classify_upload_issue_recognizes_missing_cloud_scope() {
        let err = "post_observation_events returned 403 Forbidden: {\"code\":\"SCOPE_MISSING\",\"message\":\"Forbidden: missing required scope\"}";

        assert_eq!(
            classify_upload_issue(err),
            ObservationUploadIssue::MissingCloudScope
        );
    }

    #[tokio::test]
    async fn accepted_fix_proof_sources_reads_only_accepted_fix_outcomes() {
        let temp = tempfile::tempdir().unwrap();
        let emitter = ObservationEmitter::open_at(&temp.path().join("obs.db"))
            .await
            .unwrap();
        emitter
            .enqueue(&ObservationEvent::FixOutcome {
                rule_id: "r-local".to_owned(),
                session_id: "fix-acceptance:legacy".to_owned(),
                file_path: Some("src/local.rs".to_owned()),
                accepted: true,
                occurred_at: Utc::now(),
                mcp_serve_event_ids: Vec::new(),
            })
            .await
            .unwrap();
        emitter
            .enqueue(&ObservationEvent::FixOutcome {
                rule_id: "r-cloud".to_owned(),
                session_id: "cloud-fix-acceptance:123".to_owned(),
                file_path: Some("src/cloud.rs".to_owned()),
                accepted: true,
                occurred_at: Utc::now(),
                mcp_serve_event_ids: Vec::new(),
            })
            .await
            .unwrap();
        emitter
            .enqueue(&ObservationEvent::FixOutcome {
                rule_id: "r-rejected".to_owned(),
                session_id: "fix-acceptance:legacy".to_owned(),
                file_path: Some("src/rejected.rs".to_owned()),
                accepted: false,
                occurred_at: Utc::now(),
                mcp_serve_event_ids: Vec::new(),
            })
            .await
            .unwrap();

        let ids = vec![
            "r-local".to_owned(),
            "r-cloud".to_owned(),
            "r-rejected".to_owned(),
        ];
        let sources = emitter.accepted_fix_proof_sources(&ids).await.unwrap();

        assert_eq!(
            sources.get("r-local").map(String::as_str),
            Some("local_fix")
        );
        assert_eq!(
            sources.get("r-cloud").map(String::as_str),
            Some("cloud_fix")
        );
        assert!(!sources.contains_key("r-rejected"));
    }

    #[tokio::test]
    async fn accepted_fix_outcome_count_reads_local_hook_proofs() {
        let temp = tempfile::tempdir().unwrap();
        let emitter = ObservationEmitter::open_at(&temp.path().join("obs.db"))
            .await
            .unwrap();
        emitter
            .enqueue(&ObservationEvent::FixOutcome {
                rule_id: "r-accepted".to_owned(),
                session_id: "agent-session-1".to_owned(),
                file_path: Some("src/lib.rs".to_owned()),
                accepted: true,
                occurred_at: Utc::now(),
                mcp_serve_event_ids: Vec::new(),
            })
            .await
            .unwrap();
        emitter
            .enqueue(&ObservationEvent::FixOutcome {
                rule_id: "r-rejected".to_owned(),
                session_id: "agent-session-1".to_owned(),
                file_path: Some("src/other.rs".to_owned()),
                accepted: false,
                occurred_at: Utc::now(),
                mcp_serve_event_ids: Vec::new(),
            })
            .await
            .unwrap();

        assert_eq!(emitter.accepted_fix_outcome_count(30).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn accepted_recall_link_summary_counts_prior_rule_serves() {
        let temp = tempfile::tempdir().unwrap();
        let emitter = ObservationEmitter::open_at(&temp.path().join("obs.db"))
            .await
            .unwrap();
        let served_at = Utc::now()
            .checked_sub_signed(chrono::Duration::minutes(5))
            .unwrap();
        emitter
            .enqueue(&ObservationEvent::McpRuleServed {
                tool: "search_rules".to_owned(),
                session_id: "agent-session-1".to_owned(),
                repo_full_name: Some("acme/widgets".to_owned()),
                file_path: Some("src/lib.rs".to_owned()),
                query_hash: "fc2b18493e42be726bd550a895ec1cae48c9ca833f004b427077f1270432ff3b"
                    .to_owned(),
                rule_ids: vec!["r-accepted".to_owned()],
                top_k: 5,
                was_empty: false,
                strict_match_count: 1,
                estimated_tokens: 123,
                served_at,
            })
            .await
            .unwrap();
        emitter
            .enqueue(&ObservationEvent::FixOutcome {
                rule_id: "r-accepted".to_owned(),
                session_id: "agent-session-1".to_owned(),
                file_path: Some("src/lib.rs".to_owned()),
                accepted: true,
                occurred_at: Utc::now(),
                mcp_serve_event_ids: Vec::new(),
            })
            .await
            .unwrap();
        emitter
            .enqueue(&ObservationEvent::FixOutcome {
                rule_id: "r-unlinked".to_owned(),
                session_id: "other-session".to_owned(),
                file_path: Some("src/other.rs".to_owned()),
                accepted: true,
                occurred_at: Utc::now(),
                mcp_serve_event_ids: Vec::new(),
            })
            .await
            .unwrap();
        emitter
            .enqueue(&ObservationEvent::FixOutcome {
                rule_id: "r-rejected".to_owned(),
                session_id: "agent-session-1".to_owned(),
                file_path: Some("src/lib.rs".to_owned()),
                accepted: false,
                occurred_at: Utc::now(),
                mcp_serve_event_ids: Vec::new(),
            })
            .await
            .unwrap();

        let summary = emitter.accepted_recall_link_summary(30, 7).await.unwrap();

        assert_eq!(summary.accepted_outcomes, 2);
        assert_eq!(summary.linked_to_prior_recall, 1);
        assert_eq!(summary.linked_to_rule_recall, 0);
        assert_eq!(summary.linked_to_mcp_rule_serve, 1);
        assert_eq!(summary.linked_to_edit_attribution, 0);
    }

    #[tokio::test]
    async fn accepted_recall_link_summary_counts_prior_edit_attribution() {
        let temp = tempfile::tempdir().unwrap();
        let emitter = ObservationEmitter::open_at(&temp.path().join("obs.db"))
            .await
            .unwrap();
        emitter
            .enqueue(&ObservationEvent::RuleCitedInEdit {
                rule_id: "r-attributed".to_owned(),
                session_id: String::new(),
                file_path: "src/lib.rs".to_owned(),
                diff_excerpt: "-old\n+new".to_owned(),
                cited_at: Utc::now()
                    .checked_sub_signed(chrono::Duration::minutes(2))
                    .unwrap(),
            })
            .await
            .unwrap();
        emitter
            .enqueue(&ObservationEvent::FixOutcome {
                rule_id: "r-attributed".to_owned(),
                session_id: String::new(),
                file_path: Some("src/lib.rs".to_owned()),
                accepted: true,
                occurred_at: Utc::now(),
                mcp_serve_event_ids: Vec::new(),
            })
            .await
            .unwrap();

        let summary = emitter.accepted_recall_link_summary(30, 7).await.unwrap();

        assert_eq!(summary.accepted_outcomes, 1);
        assert_eq!(summary.linked_to_prior_recall, 1);
        assert_eq!(summary.linked_to_rule_recall, 0);
        assert_eq!(summary.linked_to_mcp_rule_serve, 0);
        assert_eq!(summary.linked_to_edit_attribution, 1);
    }

    #[tokio::test]
    async fn accepted_recall_link_summary_counts_prior_rule_recall() {
        let temp = tempfile::tempdir().unwrap();
        let emitter = ObservationEmitter::open_at(&temp.path().join("obs.db"))
            .await
            .unwrap();
        emitter
            .enqueue(&ObservationEvent::RuleFired {
                rule_ids: vec!["r-recalled".to_owned()],
                file_path: Some("src/lib.rs".to_owned()),
                intent: Some("edit".to_owned()),
                session_id: "agent-session-1".to_owned(),
                fired_at: Utc::now()
                    .checked_sub_signed(chrono::Duration::minutes(2))
                    .unwrap(),
            })
            .await
            .unwrap();
        emitter
            .enqueue(&ObservationEvent::FixOutcome {
                rule_id: "r-recalled".to_owned(),
                session_id: "agent-session-1".to_owned(),
                file_path: Some("src/lib.rs".to_owned()),
                accepted: true,
                occurred_at: Utc::now(),
                mcp_serve_event_ids: Vec::new(),
            })
            .await
            .unwrap();

        let summary = emitter.accepted_recall_link_summary(30, 7).await.unwrap();

        assert_eq!(summary.accepted_outcomes, 1);
        assert_eq!(summary.linked_to_prior_recall, 1);
        assert_eq!(summary.linked_to_rule_recall, 1);
        assert_eq!(summary.linked_to_mcp_rule_serve, 0);
        assert_eq!(summary.linked_to_edit_attribution, 0);
    }

    #[tokio::test]
    async fn accepted_recall_link_summary_picks_up_inline_mcp_serve_event_ids() {
        // Reproduces the production gap behind Caveat 2: the MCP serve was
        // recorded with `session_id="hook"` (no client session id passed
        // through), while the accepted edit was recorded with the agent
        // session id. The session_id+file_path heuristic in
        // `prior_rule_use_links` fails here because the file_path on the
        // hook-issued serve points at the file the user was *reading*, not
        // the file the agent later edited. The inline `mcp_serve_event_ids`
        // bridge populated by `recent_mcp_serve_event_ids` on the same
        // repo carries the cross-link instead.
        let temp = tempfile::tempdir().unwrap();
        let emitter = ObservationEmitter::open_at(&temp.path().join("obs.db"))
            .await
            .unwrap();
        let served_at = Utc::now()
            .checked_sub_signed(chrono::Duration::minutes(5))
            .unwrap();
        let serve_id = emitter
            .enqueue(&ObservationEvent::McpRuleServed {
                tool: "search_rules".to_owned(),
                session_id: "hook".to_owned(),
                repo_full_name: Some("acme/widgets".to_owned()),
                file_path: Some("src/reader.rs".to_owned()),
                query_hash: "fc2b18493e42be726bd550a895ec1cae48c9ca833f004b427077f1270432ff3b"
                    .to_owned(),
                rule_ids: vec!["r-accepted".to_owned()],
                top_k: 5,
                was_empty: false,
                strict_match_count: 0,
                estimated_tokens: 100,
                served_at,
            })
            .await
            .unwrap();
        let occurred_at = Utc::now();
        let inline_ids = emitter
            .recent_mcp_serve_event_ids(
                "r-accepted",
                Some("acme/widgets"),
                Some("src/edited.rs"),
                occurred_at.timestamp_millis(),
                30 * 60 * 1000,
            )
            .await
            .unwrap();
        assert_eq!(inline_ids, vec![serve_id]);
        emitter
            .enqueue(&ObservationEvent::FixOutcome {
                rule_id: "r-accepted".to_owned(),
                session_id: "agent-session-1".to_owned(),
                file_path: Some("src/edited.rs".to_owned()),
                accepted: true,
                occurred_at,
                mcp_serve_event_ids: inline_ids,
            })
            .await
            .unwrap();

        let summary = emitter.accepted_recall_link_summary(30, 7).await.unwrap();

        assert_eq!(summary.accepted_outcomes, 1);
        assert_eq!(summary.linked_to_prior_recall, 1);
        assert_eq!(summary.linked_to_mcp_rule_serve, 1);
    }

    #[tokio::test]
    async fn accepted_fix_outcome_rule_summaries_group_by_rule_and_prior_recall() {
        let temp = tempfile::tempdir().unwrap();
        let emitter = ObservationEmitter::open_at(&temp.path().join("obs.db"))
            .await
            .unwrap();
        let served_at = Utc::now()
            .checked_sub_signed(chrono::Duration::minutes(5))
            .unwrap();
        emitter
            .enqueue(&ObservationEvent::McpRuleServed {
                tool: "search_rules".to_owned(),
                session_id: "agent-session-1".to_owned(),
                repo_full_name: Some("acme/widgets".to_owned()),
                file_path: Some("src/new.rs".to_owned()),
                query_hash: "fc2b18493e42be726bd550a895ec1cae48c9ca833f004b427077f1270432ff3b"
                    .to_owned(),
                rule_ids: vec!["r-accepted".to_owned()],
                top_k: 5,
                was_empty: false,
                strict_match_count: 1,
                estimated_tokens: 123,
                served_at,
            })
            .await
            .unwrap();
        emitter
            .enqueue(&ObservationEvent::FixOutcome {
                rule_id: "r-accepted".to_owned(),
                session_id: "agent-session-1".to_owned(),
                file_path: Some("src/old.rs".to_owned()),
                accepted: true,
                occurred_at: served_at,
                mcp_serve_event_ids: Vec::new(),
            })
            .await
            .unwrap();
        emitter
            .enqueue(&ObservationEvent::FixOutcome {
                rule_id: "r-accepted".to_owned(),
                session_id: "agent-session-1".to_owned(),
                file_path: Some("src/new.rs".to_owned()),
                accepted: true,
                occurred_at: Utc::now(),
                mcp_serve_event_ids: Vec::new(),
            })
            .await
            .unwrap();
        emitter
            .enqueue(&ObservationEvent::FixOutcome {
                rule_id: "r-other".to_owned(),
                session_id: "other-session".to_owned(),
                file_path: Some("src/other.rs".to_owned()),
                accepted: true,
                occurred_at: Utc::now(),
                mcp_serve_event_ids: Vec::new(),
            })
            .await
            .unwrap();
        emitter
            .enqueue(&ObservationEvent::FixOutcome {
                rule_id: "r-rejected".to_owned(),
                session_id: "agent-session-1".to_owned(),
                file_path: Some("src/rejected.rs".to_owned()),
                accepted: false,
                occurred_at: Utc::now(),
                mcp_serve_event_ids: Vec::new(),
            })
            .await
            .unwrap();

        let summaries = emitter
            .accepted_fix_outcome_rule_summaries(30, 7)
            .await
            .unwrap();

        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].rule_id, "r-accepted");
        assert_eq!(summaries[0].accepted_outcomes, 2);
        assert_eq!(summaries[0].linked_to_prior_recall, 2);
        assert_eq!(summaries[0].linked_to_mcp_rule_serve, 2);
        assert_eq!(summaries[0].linked_to_edit_attribution, 0);
        assert_eq!(summaries[0].sample_file.as_deref(), Some("src/new.rs"));
        assert_eq!(summaries[1].rule_id, "r-other");
        assert_eq!(summaries[1].accepted_outcomes, 1);
        assert_eq!(summaries[1].linked_to_prior_recall, 0);
        assert_eq!(summaries[1].linked_to_mcp_rule_serve, 0);
    }

    #[test]
    fn event_content_hash_is_stable_for_equal_payload() {
        let when = DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap();
        let a = ObservationEvent::RuleFired {
            rule_ids: vec!["r1".to_owned(), "r2".to_owned()],
            file_path: Some("src/lib.rs".to_owned()),
            intent: Some("edit".to_owned()),
            session_id: "s-1".to_owned(),
            fired_at: when,
        };
        let b = ObservationEvent::RuleFired {
            rule_ids: vec!["r1".to_owned(), "r2".to_owned()],
            file_path: Some("src/lib.rs".to_owned()),
            intent: Some("edit".to_owned()),
            session_id: "s-1".to_owned(),
            fired_at: when,
        };
        assert_eq!(event_content_hash(&a), event_content_hash(&b));
        assert_eq!(event_content_hash(&a).len(), 16);

        let c = ObservationEvent::RuleFired {
            rule_ids: vec!["r1".to_owned()],
            file_path: Some("src/lib.rs".to_owned()),
            intent: Some("edit".to_owned()),
            session_id: "s-1".to_owned(),
            fired_at: when,
        };
        assert_ne!(event_content_hash(&a), event_content_hash(&c));
    }

    #[tokio::test]
    async fn migrate_creates_expected_columns() {
        let temp = tempfile::tempdir().unwrap();
        let emitter = ObservationEmitter::open_at(&temp.path().join("obs.db"))
            .await
            .unwrap();
        let rows = sqlx::query("PRAGMA table_info(observation_events)")
            .fetch_all(&emitter.pool)
            .await
            .unwrap();
        let mut columns: Vec<String> = rows
            .into_iter()
            .map(|r| r.try_get::<String, _>("name").unwrap_or_default())
            .collect();
        columns.sort();
        let expected = vec![
            "created_at_ms",
            "event_type",
            "file_path",
            "id",
            "last_error",
            "next_attempt_at_ms",
            "occurred_at_ms",
            "payload_json",
            "retry_count",
            "rule_id",
            "rule_ids_json",
            "sent_at_ms",
            "session_id",
            "status",
        ];
        assert_eq!(columns, expected);
    }
}
