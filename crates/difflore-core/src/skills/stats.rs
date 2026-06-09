use super::REMEMBER_DAILY_LIMIT;

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RulesStats {
    pub total: i64,
    pub by_origin: Vec<OriginCount>,
    pub conversation_captures_today: i64,
    pub conversation_daily_limit: i64,
    pub top_strengthened: Vec<StrengthenedRule>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OriginCount {
    pub origin: String,
    pub count: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StrengthenedRule {
    pub id: String,
    pub name: String,
    pub origin: String,
    pub confidence: f64,
}

pub async fn stats(db: &sqlx::SqlitePool) -> crate::Result<RulesStats> {
    let total = sqlx::query_scalar!("SELECT COUNT(*) FROM skills WHERE status = 'active'")
        .fetch_one(db)
        .await?;

    // Per-origin breakdown, sorted by count desc so the dominant
    // channel surfaces first. Pending candidates are excluded — the
    // stats dashboard reflects the live rule corpus.
    let by_origin_rows = sqlx::query!(
        "SELECT origin, COUNT(*) AS c FROM skills WHERE status = 'active' \
         GROUP BY origin ORDER BY c DESC, origin ASC",
    )
    .fetch_all(db)
    .await?;
    let by_origin: Vec<OriginCount> = by_origin_rows
        .into_iter()
        .map(|r| OriginCount {
            origin: r.origin,
            count: r.c,
        })
        .collect();

    let conversation_captures_today = count_captures_today(db, "conversation").await?;

    // Top 5 rules by confidence, restricted to conversation-origin rules
    // that have been bumped above the 0.6 base. These are the ones the
    // user (or agent) re-captured — a strong signal of "this matters".
    // Limit 5 to keep terminal output digestible.
    let top_rows = sqlx::query!(
        "SELECT id, name, origin, confidence_score FROM skills \
         WHERE origin = 'conversation' AND confidence_score > 0.6 \
         AND status = 'active' \
         ORDER BY confidence_score DESC, updated_at DESC LIMIT 5",
    )
    .fetch_all(db)
    .await?;
    let top_strengthened: Vec<StrengthenedRule> = top_rows
        .into_iter()
        .map(|r| StrengthenedRule {
            id: r.id,
            name: r.name,
            origin: r.origin,
            confidence: r.confidence_score,
        })
        .collect();

    Ok(RulesStats {
        total,
        by_origin,
        conversation_captures_today,
        conversation_daily_limit: REMEMBER_DAILY_LIMIT,
        top_strengthened,
    })
}

/// How many conversation-channel captures landed today. Used both for
/// the rate-limit warn threshold and for surfacing `captures_today` on
/// the outcome so callers can render guidance like "12/50 today, getting
/// close to the cap". Returns 0 for non-conversation origins (the rate
/// limit only protects against agent runaway).
pub async fn count_captures_today(db: &sqlx::SqlitePool, origin: &str) -> crate::Result<i64> {
    if origin != "conversation" {
        return Ok(0);
    }
    let local_day = chrono::Local::now().date_naive().to_string();
    let n = sqlx::query_scalar::<_, i64>(
        "SELECT
            (SELECT COUNT(*) FROM skills
             WHERE origin = 'conversation'
             AND date(installed_at, 'localtime') = ?1)
            +
            (SELECT COUNT(*) FROM rule_events
             WHERE source = 'remember_rule'
             AND date(created_at, 'localtime') = ?1)",
    )
    .bind(local_day)
    .fetch_one(db)
    .await?;
    Ok(n)
}
