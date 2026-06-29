use super::*;

pub async fn run_memory_autopilot(
    pool: &SqlitePool,
    options: MemoryAutopilotOptions,
) -> Result<MemoryAutopilotReport> {
    let max_auto_enable = normalize_autopilot_limit(options.max_auto_enable);
    let mut plan = build_plan(
        pool,
        MAX_PENDING_SCAN,
        BuildPlanOptions {
            local_ai_curator: true,
            curator_max_candidates: options.curator_max_candidates,
        },
    )
    .await?;
    if !options.dry_run {
        let triage = apply_candidate_triage(pool, &plan.groups, &plan.active_rules).await?;
        if triage.changed {
            plan = build_plan(
                pool,
                MAX_PENDING_SCAN,
                BuildPlanOptions {
                    local_ai_curator: false,
                    curator_max_candidates: None,
                },
            )
            .await?;
        }
    }
    // Persist deterministic conflict records from the autopilot side-effect
    // path (NOT load_memory_digest, which stays read-only). This is a non-AI
    // audit feature, so it must not be gated on the local-AI curator option.
    persist_detected_conflicts(pool, &plan.groups).await?;
    let mut auto_enabled = Vec::new();
    let mut skipped = Vec::new();
    let mut applied = 0usize;

    for group in &mut plan.groups {
        match group.digest.state {
            MemoryCandidateGroupState::AutoEnable if applied < max_auto_enable => {
                if options.dry_run {
                    auto_enabled.push(MemoryAutopilotAction {
                        group_id: group.digest.group_id.clone(),
                        title: group.digest.title.clone(),
                        rule_id: None,
                        item_ids: group.digest.item_ids.clone(),
                        reason: group.digest.reason.clone(),
                        dry_run: true,
                    });
                    applied += 1;
                    continue;
                }

                // A single group failing to enable — e.g. a draft that only
                // collides with an already-active rule *after* refinement
                // recomputes its content hash, so build_plan couldn't pre-flag
                // it `AlreadyActive` — must not abort the whole autopilot run.
                // Skip it and continue; one bad candidate never stalls the
                // queue (mirrors the outbox delivery policy).
                let rule = match enable_group(pool, &group.candidates).await {
                    Ok(rule) => rule,
                    // Candidate-local outcomes (a refined draft that collides
                    // with an already-active rule, or one promoted/removed by a
                    // concurrent run) are skipped so one bad candidate can't
                    // stall the batch. Genuine infrastructure failures
                    // (DB/IO/internal) still propagate and fail the run.
                    Err(err @ (CoreError::Validation(_) | CoreError::NotFound(_))) => {
                        skipped.push(MemoryAutopilotSkip {
                            group_id: group.digest.group_id.clone(),
                            title: group.digest.title.clone(),
                            item_ids: group.digest.item_ids.clone(),
                            reason: format!("skipped (enable failed): {err}"),
                        });
                        continue;
                    }
                    Err(err) => return Err(err),
                };
                let item_ids = group.digest.item_ids.clone();
                let action = MemoryAutopilotAction {
                    group_id: group.digest.group_id.clone(),
                    title: group.digest.title.clone(),
                    rule_id: Some(rule.id.clone()),
                    item_ids,
                    reason: group.digest.reason.clone(),
                    dry_run: false,
                };
                record_autopilot_event(
                    pool,
                    AutopilotEventInput {
                        event_type: "auto_enabled",
                        rule_id: Some(&rule.id),
                        item_ids: &action.item_ids,
                        group_id: Some(&action.group_id),
                        title: &action.title,
                        reason: &action.reason,
                        payload: json!({
                            "ruleId": rule.id,
                            "ruleTitle": rule.name,
                            "source": "memory_autopilot",
                        }),
                    },
                )
                .await?;
                auto_enabled.push(action);
                applied += 1;
            }
            MemoryCandidateGroupState::AutoEnable => {
                skipped.push(MemoryAutopilotSkip {
                    group_id: group.digest.group_id.clone(),
                    title: group.digest.title.clone(),
                    item_ids: group.digest.item_ids.clone(),
                    reason: format!("per-run limit reached ({max_auto_enable})"),
                });
            }
            _ => skipped.push(MemoryAutopilotSkip {
                group_id: group.digest.group_id.clone(),
                title: group.digest.title.clone(),
                item_ids: group.digest.item_ids.clone(),
                reason: group.digest.reason.clone(),
            }),
        }
    }

    if !options.dry_run && !auto_enabled.is_empty() {
        plan = build_plan(
            pool,
            MAX_PENDING_SCAN,
            BuildPlanOptions {
                local_ai_curator: false,
                curator_max_candidates: None,
            },
        )
        .await?;
    }

    Ok(MemoryAutopilotReport {
        dry_run: options.dry_run,
        max_auto_enable,
        auto_enabled,
        skipped,
        digest: plan.digest,
    })
}

pub async fn promote_candidate_with_curator_recommendation(
    pool: &SqlitePool,
    draft_id: &str,
) -> Result<SkillRecord> {
    apply_cached_curator_recommendation_to_draft(pool, draft_id).await?;
    promote_candidate(pool, draft_id).await
}

async fn apply_cached_curator_recommendation_to_draft(
    pool: &SqlitePool,
    draft_id: &str,
) -> Result<()> {
    let Some(mut group) = planned_single_draft_group(pool, draft_id).await? else {
        return Ok(());
    };
    if !pr_review_candidate_group(&group)
        || group.digest.state != MemoryCandidateGroupState::NeedsReview
    {
        return Ok(());
    }

    let input_hash = group_input_hash(&group);
    let recommendations = load_curator_recommendations(pool).await?;
    let Some(cached) = recommendations.get(&group.digest.group_id) else {
        return Ok(());
    };
    if cached.prompt_version != MEMORY_AUTOPILOT_SCHEMA_VERSION || cached.input_hash != input_hash {
        return Ok(());
    }

    let Some(original) = group.candidates.first().cloned() else {
        return Ok(());
    };
    apply_cached_curator_recommendation(&mut group, cached);
    if !matches!(
        group.digest.state,
        MemoryCandidateGroupState::AutoEnable | MemoryCandidateGroupState::Recommended
    ) {
        return Ok(());
    }
    let Some(refined) = group.candidates.first() else {
        return Ok(());
    };
    if draft_refinement_changed(&original, refined) {
        update_pending_draft_with_refined_rule(pool, draft_id, refined).await?;
    }
    Ok(())
}

async fn planned_single_draft_group(
    pool: &SqlitePool,
    draft_id: &str,
) -> Result<Option<PlannedGroup>> {
    let draft = list_candidates(pool, None, None)
        .await?
        .into_iter()
        .find(|draft| draft.id == draft_id);
    let Some(draft) = draft else {
        return Ok(None);
    };
    let disabled_rule_ids = load_autopilot_disabled_rule_ids(pool).await?;
    let candidate = pending_from_draft(draft, &disabled_rule_ids);
    let active_rules = load_active_rules(pool, MAX_PENDING_SCAN).await?;
    let active_keys = active_rules
        .iter()
        .map(active_memory_key)
        .collect::<HashSet<_>>();
    let active_content_hashes = active_rules
        .iter()
        .filter_map(|rule| rule.content_hash.as_deref())
        .map(str::to_owned)
        .collect::<HashSet<_>>();
    let group_id = candidate_group_key(&candidate);
    let digest = digest_group(
        group_id,
        std::slice::from_ref(&candidate),
        &active_keys,
        &active_content_hashes,
        &active_rules,
    );
    Ok(Some(PlannedGroup {
        digest,
        candidates: vec![candidate],
        conflict: None,
    }))
}

fn draft_refinement_changed(original: &PendingMemory, refined: &PendingMemory) -> bool {
    original.title != refined.title
        || original.body != refined.body
        || normalize_patterns(original.file_patterns.clone())
            != normalize_patterns(refined.file_patterns.clone())
}

pub(super) async fn refine_pr_review_groups_with_local_ai(
    pool: &SqlitePool,
    groups: &mut [PlannedGroup],
    max_candidates: Option<usize>,
) -> Result<()> {
    if cfg!(test) {
        return Ok(());
    }

    let mut options = MemoryCuratorOptions::default();
    if let Some(limit) = max_candidates {
        options.max_candidates = limit;
    }
    let candidates = groups
        .iter()
        .filter(|group| pr_review_group_needs_ai_refinement(group))
        .take(options.max_candidates)
        .filter_map(pr_review_curator_candidate)
        .collect::<Vec<_>>();
    if !candidates.is_empty() {
        // The exact set of groups actually submitted to the curator this run
        // (after the `take(max_candidates)` bound). Only these may be rewritten
        // to the "no decision" message when the curator omits them; groups
        // truncated by the bound keep their deterministic NeedsReview reason.
        let submitted_group_ids: HashSet<String> = candidates
            .iter()
            .map(|candidate| candidate.group_id.clone())
            .collect();
        let outcome = curate_memory_candidates_with_local_ai(&candidates, options).await?;
        if let Some(detail) = outcome.unavailable_reason {
            mark_ai_refinement_unavailable(groups, &detail);
        } else {
            let decisions_by_group = curator_decisions_by_group(outcome.decisions);
            for group in groups
                .iter_mut()
                .filter(|group| pr_review_group_needs_ai_refinement(group))
                .filter(|group| submitted_group_ids.contains(&group.digest.group_id))
            {
                let Some(decision) = decisions_by_group.get(&group.digest.group_id) else {
                    "local memory curator did not return a decision for this PR review"
                        .clone_into(&mut group.digest.reason);
                    continue;
                };
                let input_hash = group_input_hash(group);
                apply_curator_decision(group, decision, options);
                upsert_curator_recommendation(pool, group, &input_hash).await?;
            }
        }
    }

    // SECOND, non-authoritative layer: ask the local AI to confirm/dismiss the
    // deterministic conflicts surfaced on this batch. This only annotates the
    // persisted `memory_conflicts` rows; it never relaxes the deterministic
    // NeedsReview classification above. LLM-unavailable / parse-failure leaves
    // every conflict at `detected` (deterministic behavior preserved).
    judge_detected_conflicts_with_local_ai(pool, groups).await?;

    Ok(())
}

pub(super) fn pr_review_group_needs_ai_refinement(group: &PlannedGroup) -> bool {
    group.digest.state == MemoryCandidateGroupState::NeedsReview
        && group.digest.reason
            == "imported PR review needs human rule cleanup before autopilot can enable it"
        && group.candidates.len() == 1
        && group
            .candidates
            .first()
            .is_some_and(|candidate| candidate.origin == "pr_review")
}

pub(super) fn pr_review_curator_candidate(group: &PlannedGroup) -> Option<MemoryCuratorCandidate> {
    let candidate = group.candidates.first()?;
    Some(MemoryCuratorCandidate {
        group_id: group.digest.group_id.clone(),
        current_title: candidate.title.clone(),
        current_rule: candidate.body.clone(),
        source: MemoryCuratorSource::PrReview,
        source_repo: candidate.source_repo.clone(),
        file_patterns: candidate.file_patterns.clone(),
        source_evidence: candidate
            .raw_description
            .as_deref()
            .map(compact_source_evidence)
            .unwrap_or_default(),
        behavior_observations: Vec::new(),
    })
}

pub(super) fn compact_source_evidence(raw_description: &str) -> String {
    let lines = raw_description
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            (trimmed.starts_with("Source:")
                || trimmed.starts_with("Comment:")
                || trimmed.starts_with("File:")
                || trimmed.starts_with("Reviewer said:")
                || (!trimmed.is_empty() && trimmed.len() < 240))
                .then_some(trimmed)
        })
        .take(12)
        .collect::<Vec<_>>()
        .join("\n");
    truncate_chars(&lines, 1_200)
}

pub(super) fn apply_curator_decision(
    group: &mut PlannedGroup,
    decision: &MemoryCuratorDecision,
    options: MemoryCuratorOptions,
) {
    let reason = decision
        .reason
        .as_deref()
        .unwrap_or("local memory curator review");
    group.digest.confidence = Some(format_confidence(decision.confidence));
    if decision.action != MemoryCuratorAction::Enable {
        group.digest.reason =
            format!("local memory curator left this PR review for human cleanup: {reason}");
        return;
    }
    let title = decision.title.as_deref().map_or("", str::trim);
    let rule = decision.rule.as_deref().map_or("", str::trim);
    if !curator_rule_is_safe(title, rule) {
        group.digest.reason = format!(
            "local memory curator proposed rule text did not pass the safety gate: {reason}"
        );
        return;
    }

    if let Some(candidate) = group.candidates.first_mut() {
        title.clone_into(&mut candidate.title);
        rule.clone_into(&mut candidate.body);
        if let Some(scope) = decision.scope
            && let Some(patterns) = file_patterns_for_curator_scope(scope, &candidate.file_patterns)
        {
            candidate.file_patterns = normalize_patterns(patterns);
        }
    }
    title.clone_into(&mut group.digest.title);
    group.digest.sample = truncate_chars(rule, 320);
    group.digest.file_patterns = merged_patterns(&group.candidates);
    if decision.confidence >= options.min_confidence {
        group.digest.state = MemoryCandidateGroupState::AutoEnable;
        group.digest.reason = format!(
            "local memory curator refined this PR review into a high-confidence rule: {reason}"
        );
        group.digest.confidence = Some(AUTOPILOT_CONFIDENCE.to_owned());
    } else if decision.confidence >= DEFAULT_RECOMMENDED_MIN_CONFIDENCE {
        group.digest.state = MemoryCandidateGroupState::Recommended;
        group.digest.reason =
            format!("local memory curator recommends this rule after review: {reason}");
        group.digest.confidence = Some(format_confidence(decision.confidence));
    } else {
        group.digest.reason = format!(
            "local memory curator confidence {:.2} is below recommendation threshold {:.2}: {reason}",
            decision.confidence, DEFAULT_RECOMMENDED_MIN_CONFIDENCE
        );
    }
}

pub(super) fn mark_ai_refinement_unavailable(groups: &mut [PlannedGroup], detail: &str) {
    let detail = truncate_chars(detail, 240);
    for group in groups
        .iter_mut()
        .filter(|group| pr_review_group_needs_ai_refinement(group))
    {
        group.digest.reason =
            format!("local memory curator unavailable; review manually: {detail}");
    }
}

pub(super) async fn enable_group(
    pool: &SqlitePool,
    candidates: &[PendingMemory],
) -> Result<SkillRecord> {
    let Some(primary) = primary_candidate(candidates) else {
        return Err(CoreError::Validation(
            "cannot enable an empty memory candidate group".to_owned(),
        ));
    };

    let rule = match &primary.kind {
        PendingMemoryKind::Draft { id } => {
            update_pending_draft_with_refined_rule(pool, id, primary).await?;
            promote_candidate(pool, id).await?
        }
        PendingMemoryKind::Session { content_hash } => {
            approve_session_mined_candidate(pool, content_hash)
                .await?
                .rule
        }
    };

    for candidate in candidates {
        if let PendingMemoryKind::Session { content_hash } = &candidate.kind {
            let _ = mark_session_mined_candidate_approved_for_rule(pool, content_hash, &rule.id)
                .await?;
        }
    }

    Ok(rule)
}

pub(super) async fn update_pending_draft_with_refined_rule(
    pool: &SqlitePool,
    draft_id: &str,
    candidate: &PendingMemory,
) -> Result<()> {
    let Some(raw_description) = candidate.raw_description.as_deref() else {
        return Ok(());
    };
    let description = rewrite_draft_description(raw_description, &candidate.body);
    let file_patterns_json = if candidate.file_patterns.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&candidate.file_patterns)?)
    };
    sqlx::query(
        "UPDATE skills SET name = ?1, description = ?2, file_patterns = ?3, \
         updated_at = datetime('now') WHERE id = ?4 AND status = 'pending'",
    )
    .bind(&candidate.title)
    .bind(description)
    .bind(file_patterns_json.as_deref())
    .bind(draft_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub(super) fn rewrite_draft_description(raw_description: &str, refined_rule: &str) -> String {
    if let Some((_, evidence)) = raw_description.split_once("Source evidence:") {
        format!(
            "Rule:\n{}\n\nSource evidence:\n{}",
            refined_rule.trim(),
            evidence.trim()
        )
    } else {
        format!("Rule:\n{}", refined_rule.trim())
    }
}
