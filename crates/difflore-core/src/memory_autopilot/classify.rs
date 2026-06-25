use super::*;

pub(super) fn digest_group(
    group_id: String,
    candidates: &[PendingMemory],
    active_keys: &HashSet<String>,
    active_content_hashes: &HashSet<String>,
    active_rules: &[ActiveMemory],
) -> MemoryCandidateGroup {
    let title = strongest_title(candidates);
    let file_patterns = merged_patterns(candidates);
    let source_repo = single_source_repo(candidates);
    let origins = unique_strings(candidates.iter().map(|candidate| candidate.origin.as_str()));
    let verdicts = unique_strings(
        candidates
            .iter()
            .filter_map(|candidate| candidate.verdict.as_deref()),
    );
    let item_ids = candidates
        .iter()
        .map(|candidate| candidate.item_id.clone())
        .collect::<Vec<_>>();
    let sample = candidates
        .iter()
        .map(|candidate| candidate.body.trim())
        .find(|body| !body.is_empty())
        .unwrap_or("")
        .chars()
        .take(320)
        .collect::<String>();

    let (state, reason, confidence) = classify_group(
        &group_id,
        candidates,
        source_repo.as_deref(),
        &file_patterns,
        active_keys,
        active_content_hashes,
        active_rules,
    );

    MemoryCandidateGroup {
        group_id,
        title,
        state,
        reason,
        confidence,
        item_ids,
        source_repo,
        file_patterns,
        origins,
        verdicts,
        sample,
    }
}

pub(super) fn classify_group(
    group_id: &str,
    candidates: &[PendingMemory],
    source_repo: Option<&str>,
    file_patterns: &[String],
    active_keys: &HashSet<String>,
    active_content_hashes: &HashSet<String>,
    active_rules: &[ActiveMemory],
) -> (MemoryCandidateGroupState, String, Option<String>) {
    if active_keys.contains(group_id) {
        return (
            MemoryCandidateGroupState::AlreadyActive,
            "a matching active rule already exists".to_owned(),
            None,
        );
    }
    if candidates.iter().any(|candidate| {
        candidate
            .content_hash
            .as_deref()
            .is_some_and(|hash| active_content_hashes.contains(hash))
    }) {
        return (
            MemoryCandidateGroupState::AlreadyActive,
            "a matching active rule already exists".to_owned(),
            None,
        );
    }
    if candidates
        .iter()
        .any(|candidate| candidate.autopilot_disabled)
    {
        return (
            MemoryCandidateGroupState::NeedsReview,
            "disabled by user; re-enable manually if it becomes useful again".to_owned(),
            None,
        );
    }
    if file_patterns_are_broad(file_patterns) {
        return (
            MemoryCandidateGroupState::NeedsReview,
            "scope is too broad for automatic enablement".to_owned(),
            None,
        );
    }
    if has_merge_verdict(candidates) {
        return (
            MemoryCandidateGroupState::NeedsReview,
            "candidate asks to merge with an existing rule".to_owned(),
            None,
        );
    }
    if has_conflicting_language(candidates) {
        return (
            MemoryCandidateGroupState::NeedsReview,
            "candidate group contains conflicting guidance".to_owned(),
            None,
        );
    }
    if let Some(conflict) =
        detect_active_conflict(candidates, source_repo, file_patterns, active_rules)
    {
        return (
            MemoryCandidateGroupState::NeedsReview,
            format!(
                "conflicts with active rule \u{201c}{}\u{201d} ({}): opposing guidance on \u{201c}{}\u{201d} — review before enabling",
                conflict.title, conflict.rule_id, conflict.basis,
            ),
            None,
        );
    }
    if candidates
        .iter()
        .any(|candidate| candidate.origin == "pr_review")
    {
        return (
            MemoryCandidateGroupState::NeedsReview,
            "imported PR review needs human rule cleanup before autopilot can enable it".to_owned(),
            None,
        );
    }
    if candidates
        .iter()
        .all(|candidate| matches!(candidate.kind, PendingMemoryKind::Session { .. }))
        && candidates.len() >= 3
        && candidates.iter().all(session_keep_verdict)
    {
        return (
            MemoryCandidateGroupState::AutoEnable,
            format!(
                "{} matching session-mined discoveries agree on a narrow rule",
                candidates.len()
            ),
            Some(AUTOPILOT_CONFIDENCE.to_owned()),
        );
    }
    if candidates
        .iter()
        .all(|candidate| matches!(candidate.kind, PendingMemoryKind::Session { .. }))
        && candidates.iter().all(session_keep_verdict)
    {
        return (
            MemoryCandidateGroupState::Recommended,
            format!(
                "{} session-mined {} looks safe; review once before enabling",
                candidates.len(),
                if candidates.len() == 1 {
                    "discovery"
                } else {
                    "discoveries"
                }
            ),
            Some(RECOMMENDED_CONFIDENCE.to_owned()),
        );
    }
    (
        MemoryCandidateGroupState::NeedsReview,
        "needs human review before becoming active memory".to_owned(),
        None,
    )
}
