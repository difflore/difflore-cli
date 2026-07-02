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
        if candidates.iter().any(|candidate| {
            let source_kind = candidate.source_kind.as_str();
            source_kind.starts_with("bot:")
                || source_kind.starts_with("ai_reviewer:")
                || source_kind == "human_override_bot"
        }) {
            return (
                MemoryCandidateGroupState::NeedsReview,
                "bot-sourced PR review memory needs human validation before autopilot can enable it"
                    .to_owned(),
                None,
            );
        }
        if pr_review_group_is_auto_enable_safe(candidates, source_repo, file_patterns) {
            return (
                MemoryCandidateGroupState::AutoEnable,
                format!(
                    "{} cleaned PR-review {} has narrow scope and source evidence",
                    candidates.len(),
                    if candidates.len() == 1 {
                        "rule"
                    } else {
                        "rules"
                    }
                ),
                Some(AUTOPILOT_CONFIDENCE.to_owned()),
            );
        }
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

fn pr_review_group_is_auto_enable_safe(
    candidates: &[PendingMemory],
    source_repo: Option<&str>,
    file_patterns: &[String],
) -> bool {
    source_repo.is_some_and(|repo| !repo.trim().is_empty())
        && !file_patterns.is_empty()
        && candidates.iter().all(|candidate| {
            candidate.origin == "pr_review"
                && matches!(candidate.kind, PendingMemoryKind::Draft { .. })
                && pr_review_candidate_is_clean_rule(candidate)
        })
}

fn pr_review_candidate_is_clean_rule(candidate: &PendingMemory) -> bool {
    let title = candidate.title.trim();
    let body = candidate.body.trim();
    if title.is_empty() || body.is_empty() {
        return false;
    }
    let title_lower = title.to_ascii_lowercase();
    if title_lower.starts_with("review:")
        || title_lower.starts_with("review rule for ")
        || title_lower == "review rule from imported pr comment"
        || title_lower == "imported pr review rule"
    {
        return false;
    }
    let rule_body = split_rule_body_and_evidence(body).map_or(body, |(rule_body, _)| rule_body);
    let source_text = candidate.raw_description.as_deref().unwrap_or(body);
    if !has_source_evidence(source_text) {
        return false;
    }
    let text = format!("{title}\n{rule_body}").to_ascii_lowercase();
    if [
        "reviewer asked",
        "reviewer said",
        "reviewer requested",
        "do we really",
        "not sure",
        " imo ",
    ]
    .iter()
    .any(|needle| text.contains(needle))
    {
        return false;
    }
    curator_rule_is_safe(title, rule_body)
}

fn split_rule_body_and_evidence(body: &str) -> Option<(&str, &str)> {
    let marker = "\n\nsource evidence:";
    let body_lower = body.to_ascii_lowercase();
    let evidence_start = body_lower.find(marker)?;
    let (rule_body, evidence_body) = body.split_at(evidence_start);
    Some((rule_body.trim(), evidence_body[marker.len()..].trim()))
}

fn has_source_evidence(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("source evidence:")
        && (lower.contains("source:") || lower.contains("comment:") || lower.contains("file:"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean_pr_review_candidate(source_kind: &str) -> PendingMemory {
        PendingMemory {
            item_id: "draft-bot-pr-review".to_owned(),
            kind: PendingMemoryKind::Draft {
                id: "draft-bot-pr-review".to_owned(),
            },
            title: "Validate request headers before parsing".to_owned(),
            body: "Rule:\nValidate request headers before parsing them.\n\nSource evidence:\nSource: owner/repo#42\nComment: https://github.com/owner/repo/pull/42#discussion_r123\nFile: src/http/request.rs".to_owned(),
            raw_description: None,
            content_hash: None,
            origin: "pr_review".to_owned(),
            source_kind: source_kind.to_owned(),
            source_repo: Some("owner/repo".to_owned()),
            file_patterns: vec!["src/http/request.rs".to_owned()],
            verdict: None,
            session_id: None,
            session_created_at_ms: None,
            distinct_evidence_count: None,
            autopilot_disabled: false,
        }
    }

    #[test]
    fn bot_sourced_pr_review_candidates_must_stay_needs_review() {
        let candidates = vec![clean_pr_review_candidate("bot:review-assistant")];

        let (state, reason, confidence) = classify_group(
            "owner/repo:validate-request-headers:src/http/request.rs",
            &candidates,
            Some("owner/repo"),
            &candidates[0].file_patterns,
            &HashSet::new(),
            &HashSet::new(),
            &[],
        );

        assert_eq!(state, MemoryCandidateGroupState::NeedsReview);
        assert!(reason.contains("bot-sourced PR review memory"));
        assert_eq!(confidence, None);
    }

    /// Human twin of the bot-gate test: an auto-enable-worthy candidate with
    /// a human source_kind must auto-enable, pinning that the gate
    /// discriminates on source_kind rather than blanket-blocking pr_review.
    /// Content mirrors the proven fixture in mod.rs (team-specific, not
    /// obvious) — the bot-gate fixture's "validate headers" rule would be
    /// held by the static obviousness filter regardless of source.
    #[test]
    fn human_sourced_twin_of_bot_gate_fixture_auto_enables() {
        let mut candidate = clean_pr_review_candidate("human");
        candidate.title = "Use base media components".to_owned();
        candidate.body = "Rule:\nUse base media components for images and videos in UI code instead of raw media tags outside the base component implementations.\n\nSource evidence:\nSource: owner/repo#57\nComment: https://github.com/owner/repo/pull/57#discussion_r57\nFile: src/components/developers/Hero.tsx".to_owned();
        candidate.file_patterns = vec!["src/components/developers/hero/**/*.tsx".to_owned()];
        let candidates = vec![candidate];

        let (state, reason, confidence) = classify_group(
            "owner/repo:use-base-media-components:src/components",
            &candidates,
            Some("owner/repo"),
            &candidates[0].file_patterns,
            &HashSet::new(),
            &HashSet::new(),
            &[],
        );

        assert_eq!(state, MemoryCandidateGroupState::AutoEnable);
        assert!(reason.contains("cleaned PR-review"));
        assert!(confidence.is_some());

        // Same content, bot-sourced: the gate must flip it to NeedsReview.
        let mut bot_twin = clean_pr_review_candidate("bot:review-assistant");
        bot_twin.title = candidates[0].title.clone();
        bot_twin.body = candidates[0].body.clone();
        bot_twin.file_patterns = candidates[0].file_patterns.clone();
        let bot_candidates = vec![bot_twin];
        let (bot_state, bot_reason, _) = classify_group(
            "owner/repo:use-base-media-components:src/components",
            &bot_candidates,
            Some("owner/repo"),
            &bot_candidates[0].file_patterns,
            &HashSet::new(),
            &HashSet::new(),
            &[],
        );
        assert_eq!(bot_state, MemoryCandidateGroupState::NeedsReview);
        assert!(bot_reason.contains("bot-sourced PR review memory"));
    }
}
