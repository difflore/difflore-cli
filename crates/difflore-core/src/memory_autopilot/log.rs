use super::*;

pub async fn load_autopilot_log(
    pool: &SqlitePool,
    filter: MemoryAutopilotLogFilter,
) -> Result<MemoryAutopilotLog> {
    ensure_autopilot_events_table(pool).await?;
    let rows = sqlx::query(
        "SELECT id, event_type, rule_id, item_ids_json, group_id, title, reason, payload_json, created_at \
         FROM memory_autopilot_events \
         ORDER BY id DESC \
         LIMIT ?1",
    )
    .bind(i64::try_from(normalize_limit(filter.limit)).unwrap_or(20))
    .fetch_all(pool)
    .await?;

    let events = rows
        .into_iter()
        .map(|row| {
            let item_ids_json: String = row.try_get("item_ids_json").unwrap_or_default();
            let payload_json: String = row.try_get("payload_json").unwrap_or_default();
            MemoryAutopilotEvent {
                id: row.try_get("id").unwrap_or_default(),
                event_type: row.try_get("event_type").unwrap_or_default(),
                rule_id: row.try_get("rule_id").ok(),
                item_ids: serde_json::from_str(&item_ids_json).unwrap_or_default(),
                group_id: row.try_get("group_id").ok(),
                title: row.try_get("title").unwrap_or_default(),
                reason: row.try_get("reason").unwrap_or_default(),
                payload: serde_json::from_str(&payload_json).unwrap_or_else(|_| json!({})),
                created_at: row.try_get("created_at").unwrap_or_default(),
            }
        })
        .collect();

    Ok(MemoryAutopilotLog {
        schema_version: MEMORY_AUTOPILOT_SCHEMA_VERSION.to_owned(),
        events,
    })
}

pub async fn disable_memory_rule(
    pool: &SqlitePool,
    rule_id: &str,
    reason: Option<&str>,
) -> Result<MemoryDisableOutcome> {
    let id = normalize_rule_id(rule_id);
    if id.is_empty() {
        return Err(CoreError::Validation(
            "memory rule id is required; use rule:<id> or <id>".to_owned(),
        ));
    }
    let row = sqlx::query("SELECT id, name, status FROM skills WHERE id = ?1")
        .bind(&id)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| CoreError::NotFound(format!("memory rule `{id}` not found")))?;
    let status: String = row.try_get("status").unwrap_or_default();
    if status != "active" {
        return Err(CoreError::Validation(format!(
            "memory rule `{id}` is not active; current state is `{status}`"
        )));
    }
    let title: String = row.try_get("name").unwrap_or_else(|_| id.clone());
    sqlx::query(
        "UPDATE skills SET status = 'pending', updated_at = datetime('now') WHERE id = ?1 AND status = 'active'",
    )
    .bind(&id)
    .execute(pool)
    .await?;

    let reason = reason
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("disabled by user")
        .to_owned();
    let item_ids = vec![format!("rule:{id}")];
    record_autopilot_event(
        pool,
        AutopilotEventInput {
            event_type: "disabled",
            rule_id: Some(&id),
            item_ids: &item_ids,
            group_id: None,
            title: &title,
            reason: &reason,
            payload: json!({ "previousState": "active", "currentState": "pending" }),
        },
    )
    .await?;

    Ok(MemoryDisableOutcome {
        item_id: format!("rule:{id}"),
        rule_id: id,
        title,
        previous_state: "active".to_owned(),
        current_state: "pending".to_owned(),
        reason,
    })
}

pub(crate) struct AutopilotEventInput<'a> {
    pub(crate) event_type: &'a str,
    pub(crate) rule_id: Option<&'a str>,
    pub(crate) item_ids: &'a [String],
    pub(crate) group_id: Option<&'a str>,
    pub(crate) title: &'a str,
    pub(crate) reason: &'a str,
    pub(crate) payload: Value,
}

pub(crate) async fn record_autopilot_event(
    pool: &SqlitePool,
    event: AutopilotEventInput<'_>,
) -> Result<()> {
    ensure_autopilot_events_table(pool).await?;
    let item_ids_json = serde_json::to_string(event.item_ids)?;
    let payload_json = serde_json::to_string(&event.payload)?;
    sqlx::query(
        "INSERT INTO memory_autopilot_events \
            (event_type, rule_id, item_ids_json, group_id, title, reason, payload_json) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )
    .bind(event.event_type)
    .bind(event.rule_id)
    .bind(item_ids_json)
    .bind(event.group_id)
    .bind(event.title)
    .bind(event.reason)
    .bind(payload_json)
    .execute(pool)
    .await?;
    Ok(())
}

pub(crate) async fn ensure_autopilot_events_table(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS memory_autopilot_events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            event_type TEXT NOT NULL,
            rule_id TEXT,
            item_ids_json TEXT NOT NULL DEFAULT '[]',
            group_id TEXT,
            title TEXT NOT NULL DEFAULT '',
            reason TEXT NOT NULL DEFAULT '',
            payload_json TEXT NOT NULL DEFAULT '{}',
            created_at TEXT DEFAULT (datetime('now')) NOT NULL
        )",
    )
    .execute(pool)
    .await?;
    Ok(())
}
