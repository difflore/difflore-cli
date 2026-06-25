use super::*;

pub(super) async fn build_plan(
    pool: &SqlitePool,
    limit: usize,
    options: BuildPlanOptions,
) -> Result<MemoryPlan> {
    let active_rules = load_active_rules(pool, MAX_PENDING_SCAN).await?;
    let pending = load_pending_memories(pool, MAX_PENDING_SCAN).await?;
    let active_keys = active_rules
        .iter()
        .map(active_memory_key)
        .collect::<HashSet<_>>();
    let active_content_hashes = active_rules
        .iter()
        .filter_map(|rule| rule.content_hash.as_deref())
        .map(str::to_owned)
        .collect::<HashSet<_>>();

    let mut groups = group_pending_memories(pending)
        .into_iter()
        .map(|(key, mut candidates)| {
            candidates.sort_by(|a, b| a.item_id.cmp(&b.item_id));
            let digest = digest_group(
                key,
                &candidates,
                &active_keys,
                &active_content_hashes,
                &active_rules,
            );
            // Re-run the deterministic conflict check against the same inputs
            // `digest_group` used, so we can persist a reviewable record from
            // the autopilot side-effect path. Only meaningful when the group is
            // actually routed to review (AlreadyActive groups already collide on
            // identity, not on opposing guidance).
            let conflict = (digest.state == MemoryCandidateGroupState::NeedsReview)
                .then(|| {
                    detect_active_conflict(
                        &candidates,
                        digest.source_repo.as_deref(),
                        &digest.file_patterns,
                        &active_rules,
                    )
                })
                .flatten();
            PlannedGroup {
                digest,
                candidates,
                conflict,
            }
        })
        .collect::<Vec<_>>();
    apply_cached_curator_recommendations(pool, &mut groups).await?;
    if options.local_ai_curator {
        refine_pr_review_groups_with_local_ai(pool, &mut groups, options.curator_max_candidates)
            .await?;
    }
    groups.sort_by(|a, b| {
        group_rank(&a.digest)
            .cmp(&group_rank(&b.digest))
            .then_with(|| b.candidates.len().cmp(&a.candidates.len()))
            .then_with(|| a.digest.title.cmp(&b.digest.title))
    });

    let visible_groups = groups
        .iter()
        .take(limit)
        .map(|group| group.digest.clone())
        .collect::<Vec<_>>();
    let counts = MemoryDigestCounts {
        active_rules: i64::try_from(active_rules.len()).unwrap_or(i64::MAX),
        pending_drafts: count_pending_kind(&groups, "draft"),
        pending_session_candidates: count_pending_kind(&groups, "session"),
        auto_enable_groups: groups
            .iter()
            .filter(|group| group.digest.state == MemoryCandidateGroupState::AutoEnable)
            .count(),
        recommended_groups: groups
            .iter()
            .filter(|group| group.digest.state == MemoryCandidateGroupState::Recommended)
            .count(),
        needs_review_groups: groups
            .iter()
            .filter(|group| group.digest.state == MemoryCandidateGroupState::NeedsReview)
            .count(),
    };
    let next_actions = next_actions(&counts);
    let autopilot = load_autopilot_schedule_status(pool).await?;
    let active_rules_for_cleanup = active_rules.clone();
    let digest = MemoryDigest {
        schema_version: MEMORY_AUTOPILOT_SCHEMA_VERSION.to_owned(),
        counts,
        autopilot,
        active_rules: active_rules
            .into_iter()
            .take(50)
            .map(|rule| MemoryDigestRule {
                item_id: rule.item_id,
                rule_id: rule.rule_id,
                title: rule.title,
                origin: rule.origin,
                source_repo: rule.source_repo,
                file_patterns: rule.file_patterns,
                updated_at: rule.updated_at,
            })
            .collect(),
        candidate_groups: visible_groups,
        next_actions,
        note: "Autopilot only enables high-confidence local rules. Team sharing stays explicit."
            .to_owned(),
    };

    Ok(MemoryPlan {
        digest,
        groups,
        active_rules: active_rules_for_cleanup,
    })
}

pub(super) struct MemoryPlan {
    pub(super) digest: MemoryDigest,
    pub(super) groups: Vec<PlannedGroup>,
    pub(super) active_rules: Vec<ActiveMemory>,
}

pub(super) async fn load_pending_memories(
    pool: &SqlitePool,
    limit: usize,
) -> Result<Vec<PendingMemory>> {
    let mut pending = Vec::new();
    let disabled_rule_ids = load_autopilot_disabled_rule_ids(pool).await?;
    for draft in list_candidates(pool, None, Some(limit)).await? {
        pending.push(pending_from_draft(draft, &disabled_rule_ids));
    }

    let inbox = load_memory_inbox(pool, limit).await?;
    for discovery in inbox.local_discoveries.latest {
        pending.push(PendingMemory {
            item_id: discovery.item_id.clone(),
            kind: PendingMemoryKind::Session {
                content_hash: discovery.content_hash,
            },
            title: discovery.title,
            body: discovery.body,
            raw_description: None,
            content_hash: None,
            origin: "session_mined".to_owned(),
            source_repo: Some(discovery.source_repo),
            file_patterns: normalize_patterns(discovery.file_patterns),
            verdict: Some(discovery.gate_verdict),
            session_id: Some(discovery.session_id),
            session_created_at_ms: Some(discovery.created_at_ms),
            distinct_evidence_count: discovery.distinct_evidence_count,
            autopilot_disabled: false,
        });
    }
    Ok(pending)
}

pub(super) async fn load_autopilot_disabled_rule_ids(pool: &SqlitePool) -> Result<HashSet<String>> {
    ensure_autopilot_events_table(pool).await?;
    let rows = sqlx::query(
        "SELECT rule_id FROM memory_autopilot_events WHERE event_type = 'disabled' AND rule_id IS NOT NULL",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|row| row.try_get::<Option<String>, _>("rule_id").ok().flatten())
        .collect())
}

pub(super) fn pending_from_draft(
    draft: CandidateRule,
    disabled_rule_ids: &HashSet<String>,
) -> PendingMemory {
    let draft_id = draft.id;
    PendingMemory {
        item_id: format!("draft:{draft_id}"),
        kind: PendingMemoryKind::Draft {
            id: draft_id.clone(),
        },
        title: draft.name,
        body: draft
            .drafted_rule
            .clone()
            .unwrap_or_else(|| draft.description.clone()),
        raw_description: Some(draft.description),
        content_hash: draft
            .content_hash
            .map(|hash| hash.trim().to_owned())
            .filter(|hash| !hash.is_empty()),
        origin: draft.origin,
        source_repo: draft.source_repo,
        file_patterns: normalize_patterns(draft.file_patterns),
        verdict: None,
        session_id: None,
        session_created_at_ms: None,
        distinct_evidence_count: None,
        autopilot_disabled: disabled_rule_ids.contains(&draft_id),
    }
}

pub(super) async fn load_active_rules(
    pool: &SqlitePool,
    limit: usize,
) -> Result<Vec<ActiveMemory>> {
    let rows = sqlx::query(
        "SELECT id, name, description, content_hash, origin, source_repo, file_patterns, \
                COALESCE(updated_at, installed_at) AS updated_at \
         FROM skills \
         WHERE status = 'active' \
         ORDER BY datetime(COALESCE(updated_at, installed_at)) DESC, id ASC \
         LIMIT ?1",
    )
    .bind(i64::try_from(limit).unwrap_or(50))
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            let rule_id: String = row.try_get("id").unwrap_or_default();
            let file_patterns_raw: Option<String> = row.try_get("file_patterns").ok().flatten();
            ActiveMemory {
                item_id: format!("rule:{rule_id}"),
                rule_id,
                title: row.try_get("name").unwrap_or_default(),
                body: row.try_get("description").unwrap_or_default(),
                content_hash: row.try_get("content_hash").ok().flatten(),
                origin: row.try_get("origin").unwrap_or_default(),
                source_repo: row.try_get("source_repo").ok(),
                file_patterns: parse_string_list(file_patterns_raw.as_deref()),
                updated_at: row.try_get("updated_at").unwrap_or_default(),
            }
        })
        .collect())
}

pub(super) async fn apply_cached_curator_recommendations(
    pool: &SqlitePool,
    groups: &mut [PlannedGroup],
) -> Result<()> {
    let recommendations = load_curator_recommendations(pool).await?;
    if recommendations.is_empty() {
        return Ok(());
    }

    for group in groups.iter_mut() {
        if !pr_review_candidate_group(group)
            || group.digest.state != MemoryCandidateGroupState::NeedsReview
        {
            continue;
        }
        let Some(cached) = recommendations.get(&group.digest.group_id) else {
            continue;
        };
        if cached.prompt_version != MEMORY_AUTOPILOT_SCHEMA_VERSION {
            continue;
        }
        if cached.input_hash != group_input_hash(group) {
            continue;
        }
        apply_cached_curator_recommendation(group, cached);
    }

    Ok(())
}

pub(super) async fn load_curator_recommendations(
    pool: &SqlitePool,
) -> Result<BTreeMap<String, CachedCuratorRecommendation>> {
    ensure_curator_recommendations_table(pool).await?;
    let rows = sqlx::query(
        "SELECT group_id, input_hash, state, confidence, title, rule, file_patterns_json, \
                reason, prompt_version \
         FROM memory_curator_recommendations",
    )
    .fetch_all(pool)
    .await?;
    let mut recommendations = BTreeMap::new();
    for row in rows {
        let group_id: String = row.try_get("group_id").unwrap_or_default();
        let state_raw: String = row.try_get("state").unwrap_or_default();
        let Some(state) = candidate_group_state_from_cache(&state_raw) else {
            continue;
        };
        let file_patterns_json: String = row
            .try_get("file_patterns_json")
            .unwrap_or_else(|_| "[]".to_owned());
        recommendations.insert(
            group_id,
            CachedCuratorRecommendation {
                input_hash: row.try_get("input_hash").unwrap_or_default(),
                state,
                confidence: row.try_get("confidence").ok().flatten(),
                title: row.try_get("title").unwrap_or_default(),
                rule: row.try_get("rule").unwrap_or_default(),
                file_patterns: parse_string_list(Some(&file_patterns_json)),
                reason: row.try_get("reason").unwrap_or_default(),
                prompt_version: row.try_get("prompt_version").unwrap_or_default(),
            },
        );
    }
    Ok(recommendations)
}

pub(super) async fn upsert_curator_recommendation(
    pool: &SqlitePool,
    group: &PlannedGroup,
    input_hash: &str,
) -> Result<()> {
    if group.digest.state == MemoryCandidateGroupState::AlreadyActive {
        return Ok(());
    }
    ensure_curator_recommendations_table(pool).await?;
    let state = candidate_group_state_cache_key(&group.digest.state);
    let confidence = group.digest.confidence.clone();
    let file_patterns_json = serde_json::to_string(&group.digest.file_patterns)?;
    let item_ids_json = serde_json::to_string(&group.digest.item_ids)?;
    let rule = primary_candidate(&group.candidates)
        .map_or(group.digest.sample.as_str(), |candidate| {
            candidate.body.as_str()
        });
    sqlx::query(
        "INSERT INTO memory_curator_recommendations \
            (group_id, input_hash, state, confidence, title, rule, file_patterns_json, \
             reason, item_ids_json, prompt_version) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) \
         ON CONFLICT(group_id) DO UPDATE SET \
            input_hash = excluded.input_hash, \
            state = excluded.state, \
            confidence = excluded.confidence, \
            title = excluded.title, \
            rule = excluded.rule, \
            file_patterns_json = excluded.file_patterns_json, \
            reason = excluded.reason, \
            item_ids_json = excluded.item_ids_json, \
            prompt_version = excluded.prompt_version, \
            updated_at = datetime('now')",
    )
    .bind(&group.digest.group_id)
    .bind(input_hash)
    .bind(state)
    .bind(confidence.as_deref())
    .bind(&group.digest.title)
    .bind(rule)
    .bind(file_patterns_json)
    .bind(&group.digest.reason)
    .bind(item_ids_json)
    .bind(MEMORY_AUTOPILOT_SCHEMA_VERSION)
    .execute(pool)
    .await?;
    Ok(())
}

pub(crate) async fn ensure_curator_recommendations_table(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS memory_curator_recommendations (
            group_id TEXT PRIMARY KEY,
            input_hash TEXT NOT NULL,
            state TEXT NOT NULL,
            confidence TEXT,
            title TEXT NOT NULL DEFAULT '',
            rule TEXT NOT NULL DEFAULT '',
            file_patterns_json TEXT NOT NULL DEFAULT '[]',
            reason TEXT NOT NULL DEFAULT '',
            item_ids_json TEXT NOT NULL DEFAULT '[]',
            prompt_version TEXT NOT NULL,
            created_at TEXT DEFAULT (datetime('now')) NOT NULL,
            updated_at TEXT DEFAULT (datetime('now')) NOT NULL
        )",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_memory_curator_recommendations_state_updated \
         ON memory_curator_recommendations (state, updated_at)",
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub(super) fn apply_cached_curator_recommendation(
    group: &mut PlannedGroup,
    cached: &CachedCuratorRecommendation,
) {
    if cached.state == MemoryCandidateGroupState::NeedsReview {
        if !cached.reason.is_empty() {
            cached.reason.clone_into(&mut group.digest.reason);
        }
        group.digest.confidence.clone_from(&cached.confidence);
        return;
    }
    if !matches!(
        cached.state,
        MemoryCandidateGroupState::AutoEnable | MemoryCandidateGroupState::Recommended
    ) || cached.title.trim().is_empty()
        || cached.rule.trim().is_empty()
    {
        return;
    }

    if let Some(candidate) = group.candidates.first_mut() {
        cached.title.clone_into(&mut candidate.title);
        cached.rule.clone_into(&mut candidate.body);
        candidate.file_patterns.clone_from(&cached.file_patterns);
    }
    cached.title.clone_into(&mut group.digest.title);
    group.digest.sample = truncate_chars(&cached.rule, 320);
    group.digest.file_patterns.clone_from(&cached.file_patterns);
    group.digest.state = cached.state.clone();
    cached.reason.clone_into(&mut group.digest.reason);
    group.digest.confidence.clone_from(&cached.confidence);
}

pub(super) fn group_input_hash(group: &PlannedGroup) -> String {
    let mut input = String::new();
    input.push_str(MEMORY_AUTOPILOT_SCHEMA_VERSION);
    input.push('\n');
    input.push_str(&group.digest.group_id);
    input.push('\n');
    for candidate in &group.candidates {
        input.push_str(&candidate.item_id);
        input.push('\0');
        input.push_str(&candidate.title);
        input.push('\0');
        input.push_str(&candidate.body);
        input.push('\0');
        input.push_str(candidate.raw_description.as_deref().unwrap_or_default());
        input.push('\0');
        input.push_str(candidate.content_hash.as_deref().unwrap_or_default());
        input.push('\0');
        input.push_str(&candidate.origin);
        input.push('\0');
        input.push_str(candidate.source_repo.as_deref().unwrap_or_default());
        input.push('\0');
        input.push_str(&candidate.file_patterns.join("\0"));
        input.push('\n');
    }
    crate::infra::crypto::sha256_block_hex(input.as_bytes())
}

pub(super) fn pr_review_candidate_group(group: &PlannedGroup) -> bool {
    group.candidates.len() == 1
        && group
            .candidates
            .first()
            .is_some_and(|candidate| candidate.origin == "pr_review")
}

pub(super) const fn candidate_group_state_cache_key(
    state: &MemoryCandidateGroupState,
) -> &'static str {
    match state {
        MemoryCandidateGroupState::AutoEnable => "auto_enable",
        MemoryCandidateGroupState::Recommended => "recommended",
        MemoryCandidateGroupState::NeedsReview => "needs_review",
        MemoryCandidateGroupState::AlreadyActive => "already_active",
    }
}

pub(super) fn candidate_group_state_from_cache(raw: &str) -> Option<MemoryCandidateGroupState> {
    match raw.trim() {
        "auto_enable" => Some(MemoryCandidateGroupState::AutoEnable),
        "recommended" => Some(MemoryCandidateGroupState::Recommended),
        "needs_review" => Some(MemoryCandidateGroupState::NeedsReview),
        "already_active" => Some(MemoryCandidateGroupState::AlreadyActive),
        _ => None,
    }
}
