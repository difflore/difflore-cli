use super::*;

/// Maximum conflicts sent to the judge per batch (keeps the prompt small and
/// bounds DB writes).
pub(super) const MAX_JUDGE_CONFLICTS: usize = 25;
/// A `compatible` verdict only DISMISSES a detected conflict when the model is
/// at least this confident; weaker `compatible` verdicts leave it `detected` so
/// a borderline call still reaches human review.
pub(super) const JUDGE_DISMISS_MIN_CONFIDENCE: f32 = 0.80;

/// Verdict the local-AI judge can return for a single detected conflict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum JudgeVerdict {
    /// The candidate genuinely contradicts the active rule -> `confirmed`.
    Contradicts,
    /// The two rules are compatible -> `dismissed` when confident enough.
    Compatible,
}

/// One normalized judge decision keyed by `conflictId` (the evidence hash).
#[derive(Debug, Clone, PartialEq)]
pub(super) struct JudgeDecision {
    pub(super) conflict_id: String,
    pub(super) verdict: JudgeVerdict,
    pub(super) confidence: f32,
    pub(super) rationale: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AiJudgeDecision {
    conflict_id: String,
    verdict: String,
    #[serde(default)]
    confidence: f32,
    #[serde(default)]
    rationale: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AiJudgeEnvelope {
    decisions: Vec<AiJudgeDecision>,
}

impl AiJudgeDecision {
    /// Normalize a raw model decision into a `JudgeDecision`, or drop it when the
    /// verdict is not one we understand (defensive: never guess an unknown
    /// verdict into a status change).
    fn normalize(self) -> Option<JudgeDecision> {
        let verdict = match self.verdict.trim().to_ascii_lowercase().as_str() {
            "contradicts" => JudgeVerdict::Contradicts,
            "compatible" => JudgeVerdict::Compatible,
            _ => return None,
        };
        let conflict_id = self.conflict_id.trim().to_owned();
        if conflict_id.is_empty() {
            return None;
        }
        // Reject out-of-range / non-finite confidence by mapping to 0.0 (same
        // posture as the curator): a poisoned `1e9` must not clear the dismiss
        // gate and silently retire a true conflict.
        let confidence = if self.confidence.is_finite() && (0.0..=1.0).contains(&self.confidence) {
            self.confidence
        } else {
            0.0
        };
        let rationale = self
            .rationale
            .map(|value| truncate_chars(value.trim(), 600))
            .filter(|value| !value.is_empty());
        Some(JudgeDecision {
            conflict_id,
            verdict,
            confidence,
            rationale,
        })
    }
}

/// Confirm or dismiss the detected conflicts for this batch with the local AI.
/// Purely additive precision: it persists the deterministic conflicts first
/// (so the judge annotates the same rows the reviewer will see), then upgrades
/// each row's status based on a SEPARATE small judge prompt. Any failure path
/// (no detected rows, LLM unavailable, parse failure) leaves statuses untouched.
pub(super) async fn judge_detected_conflicts_with_local_ai(
    pool: &SqlitePool,
    groups: &[PlannedGroup],
) -> Result<()> {
    // Ensure the just-detected conflicts exist as rows before we judge them, so
    // the judge annotates exactly what the reviewer command surfaces.
    persist_detected_conflicts(pool, groups).await?;

    let detected = load_memory_conflicts(
        pool,
        MemoryConflictFilter {
            limit: Some(MAX_JUDGE_CONFLICTS),
            status: Some("detected".to_owned()),
        },
    )
    .await?
    .conflicts;
    if detected.is_empty() {
        return Ok(());
    }

    let raw = match build_judge_prompt(&detected) {
        Some((system_prompt, user_prompt)) => {
            match crate::review_engine::complete_with_local_agent_cli(&system_prompt, &user_prompt)
                .await
            {
                Ok(raw) => raw,
                // LLM unavailable -> leave every conflict at `detected`.
                Err(_) => return Ok(()),
            }
        }
        None => return Ok(()),
    };

    // Parse failure -> leave every conflict at `detected` (deterministic
    // behavior preserved); never let a bad parse silently retire a conflict.
    let Ok(decisions) = parse_judge_decisions(&raw) else {
        return Ok(());
    };
    let known = detected
        .iter()
        .map(|conflict| conflict.evidence_hash.as_str())
        .collect::<HashSet<_>>();
    for decision in decisions {
        if !known.contains(decision.conflict_id.as_str()) {
            continue;
        }
        apply_judge_decision(pool, &decision).await?;
    }

    Ok(())
}

/// Build the separate, minimal judge prompt over the detected conflicts. Returns
/// `None` when there is nothing to judge.
pub(super) fn build_judge_prompt(conflicts: &[MemoryConflictRecord]) -> Option<(String, String)> {
    if conflicts.is_empty() {
        return None;
    }
    let payload = conflicts
        .iter()
        .map(|conflict| {
            json!({
                "conflictId": conflict.evidence_hash,
                "repo": conflict.source_repo,
                "overlapBasis": conflict.overlap_basis,
                "candidateRule": {
                    "title": conflict.candidate_title,
                    "rule": conflict.candidate_body,
                },
                "activeRule": {
                    "title": conflict.active_title,
                    "rule": conflict.active_body,
                },
            })
        })
        .collect::<Vec<_>>();
    let conflicts_json = serde_json::to_string_pretty(&Value::Array(payload)).ok()?;
    let system_prompt = "You are DiffLore's conflict judge. A deterministic check already flagged \
        each candidate rule as possibly contradicting an active rule in the same repository. \
        Decide, for each, whether the two rules truly give OPPOSING guidance on the same subject. \
        Return JSON only. Be conservative: if the rules can both hold at once, they are \
        compatible."
        .to_owned();
    let user_prompt = format!(
        "For each flagged conflict, return one decision:\n\
         - verdict: \"contradicts\" when the candidate rule and the active rule give opposing, \
           mutually exclusive guidance on the same subject within the same repo.\n\
         - verdict: \"compatible\" when both rules can hold at once (different subject, different \
           scope, or simply not opposed).\n\
         - confidence: 0.0 to 1.0.\n\
         - rationale: one concise sentence.\n\n\
         JSON schema:\n\
         {{\"decisions\":[{{\"conflictId\":\"...\",\"verdict\":\"contradicts|compatible\",\
         \"confidence\":0.0,\"rationale\":\"...\"}}]}}\n\n\
         Conflicts:\n{conflicts_json}"
    );
    Some((system_prompt, user_prompt))
}

/// Defensive judge JSON parser, mirroring `parse_curator_decisions`: tolerate a
/// bare array or a `{ "decisions": [...] }` envelope, possibly wrapped in code
/// fences or prose, and drop any decision with an unknown verdict.
pub(super) fn parse_judge_decisions(raw: &str) -> Result<Vec<JudgeDecision>> {
    let json_text = extract_json_object(raw)
        .ok_or_else(|| CoreError::Internal("local AI judge did not return JSON".to_owned()))?;
    let value: Value = serde_json::from_str(json_text).map_err(|err| {
        CoreError::Internal(format!("local AI judge returned invalid JSON: {err}"))
    })?;
    let raw_decisions = if value.is_array() {
        serde_json::from_value::<Vec<AiJudgeDecision>>(value)
            .map_err(|err| CoreError::Internal(format!("local AI judge parse failed: {err}")))?
    } else {
        serde_json::from_value::<AiJudgeEnvelope>(value)
            .map_err(|err| CoreError::Internal(format!("local AI judge parse failed: {err}")))?
            .decisions
    };
    Ok(raw_decisions
        .into_iter()
        .filter_map(AiJudgeDecision::normalize)
        .collect())
}

/// Apply one judge decision to its `detected` conflict row. `contradicts`
/// confirms; a confident-enough `compatible` dismisses; everything else leaves
/// the row `detected`. The `status = 'detected'` guard makes this safe against a
/// human verdict that landed between detection and judging.
pub(super) async fn apply_judge_decision(
    pool: &SqlitePool,
    decision: &JudgeDecision,
) -> Result<()> {
    let Some(new_status) = judge_status_for(decision.verdict, decision.confidence) else {
        // Low-confidence `compatible` is not authoritative enough to retire a
        // true conflict; leave it `detected` for human review.
        return Ok(());
    };
    update_conflict_judge_verdict(
        pool,
        &decision.conflict_id,
        new_status,
        decision.rationale.as_deref(),
        decision.confidence,
    )
    .await
}

/// Map a judge verdict + confidence to the target conflict status, or `None`
/// when the verdict is not authoritative enough to move the row off `detected`.
/// Pure so the confirm/dismiss/leave-detected policy is unit-testable without a
/// live model.
pub(super) fn judge_status_for(verdict: JudgeVerdict, confidence: f32) -> Option<&'static str> {
    match verdict {
        JudgeVerdict::Contradicts => Some("confirmed"),
        JudgeVerdict::Compatible if confidence >= JUDGE_DISMISS_MIN_CONFIDENCE => Some("dismissed"),
        JudgeVerdict::Compatible => None,
    }
}

/// Update a single conflict row from `detected` to the judged status, recording
/// the LLM rationale + confidence. The `status = 'detected'` predicate keeps
/// this non-authoritative: it never overwrites a human confirm/dismiss.
pub(super) async fn update_conflict_judge_verdict(
    pool: &SqlitePool,
    evidence_hash: &str,
    new_status: &str,
    rationale: Option<&str>,
    confidence: f32,
) -> Result<()> {
    ensure_memory_conflicts_table(pool).await?;
    sqlx::query(
        "UPDATE memory_conflicts \
         SET status = ?1, llm_rationale = ?2, llm_confidence = ?3, updated_at = datetime('now') \
         WHERE evidence_hash = ?4 AND status = 'detected'",
    )
    .bind(new_status)
    .bind(rationale)
    .bind(f64::from(confidence))
    .bind(evidence_hash)
    .execute(pool)
    .await?;
    Ok(())
}

/// Persist every deterministic candidate-vs-active conflict surfaced on the
/// planned groups. Called from the autopilot side-effect path; idempotent and
/// status-preserving (see `upsert_memory_conflict`).
pub(super) async fn persist_detected_conflicts(
    pool: &SqlitePool,
    groups: &[PlannedGroup],
) -> Result<()> {
    let with_conflicts = groups
        .iter()
        .filter(|group| group.conflict.is_some())
        .collect::<Vec<_>>();
    if with_conflicts.is_empty() {
        return Ok(());
    }
    ensure_memory_conflicts_table(pool).await?;
    for group in with_conflicts {
        let Some(conflict) = group.conflict.as_ref() else {
            continue;
        };
        upsert_memory_conflict(pool, group, conflict).await?;
    }
    Ok(())
}

/// Stable evidence hash over NORMALIZED CONTENT (repo, both rule ids, basis, and
/// normalized pattern sets) — deliberately NOT the ephemeral group id, so the
/// same logical conflict maps to a single row across runs.
pub(super) fn conflict_evidence_hash(
    source_repo: Option<&str>,
    candidate_rule_id: Option<&str>,
    conflict: &ActiveConflict,
    candidate_patterns: &[String],
) -> String {
    let mut input = String::new();
    input.push_str(source_repo.unwrap_or_default().trim());
    input.push('\0');
    input.push_str(candidate_rule_id.unwrap_or_default().trim());
    input.push('\0');
    input.push_str(conflict.rule_id.trim());
    input.push('\0');
    input.push_str(conflict.basis.trim());
    input.push('\0');
    input.push_str(&pattern_key(candidate_patterns));
    input.push('\0');
    input.push_str(&pattern_key(&conflict.active_patterns));
    crate::infra::crypto::sha256_block_hex(input.as_bytes())
}

/// Primary candidate rule id (the underlying skill/draft id, not the `draft:`
/// prefixed item id) used to attribute a conflict record.
pub(super) fn conflict_candidate_rule_id(candidates: &[PendingMemory]) -> Option<String> {
    primary_candidate(candidates).map(|candidate| match &candidate.kind {
        PendingMemoryKind::Draft { id } => id.clone(),
        PendingMemoryKind::Session { content_hash } => content_hash.clone(),
    })
}

/// Upsert a conflict record keyed by `evidence_hash`. Refreshes the snapshots
/// and `updated_at` on every run, but PRESERVES a non-`detected` status: once a
/// human confirms or dismisses a conflict, re-running the autopilot must not
/// reset it back to `detected`.
pub(super) async fn upsert_memory_conflict(
    pool: &SqlitePool,
    group: &PlannedGroup,
    conflict: &ActiveConflict,
) -> Result<()> {
    let candidate = primary_candidate(&group.candidates);
    let candidate_title = candidate.map_or(group.digest.title.as_str(), |candidate| {
        candidate.title.as_str()
    });
    let candidate_body = candidate.map_or(group.digest.sample.as_str(), |candidate| {
        candidate.body.as_str()
    });
    let candidate_rule_id = conflict_candidate_rule_id(&group.candidates);
    let evidence_hash = conflict_evidence_hash(
        group.digest.source_repo.as_deref(),
        candidate_rule_id.as_deref(),
        conflict,
        &group.digest.file_patterns,
    );
    let candidate_patterns_json = serde_json::to_string(&group.digest.file_patterns)?;
    let active_patterns_json = serde_json::to_string(&conflict.active_patterns)?;

    sqlx::query(
        "INSERT INTO memory_conflicts \
            (evidence_hash, candidate_group_id, candidate_rule_id, active_rule_id, source_repo, \
             overlap_basis, candidate_title, candidate_body, active_title, active_body, \
             candidate_patterns_json, active_patterns_json) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12) \
         ON CONFLICT(evidence_hash) DO UPDATE SET \
            candidate_group_id = excluded.candidate_group_id, \
            candidate_rule_id = excluded.candidate_rule_id, \
            active_rule_id = excluded.active_rule_id, \
            source_repo = excluded.source_repo, \
            overlap_basis = excluded.overlap_basis, \
            candidate_title = excluded.candidate_title, \
            candidate_body = excluded.candidate_body, \
            active_title = excluded.active_title, \
            active_body = excluded.active_body, \
            candidate_patterns_json = excluded.candidate_patterns_json, \
            active_patterns_json = excluded.active_patterns_json, \
            updated_at = datetime('now')",
    )
    .bind(&evidence_hash)
    .bind(&group.digest.group_id)
    .bind(candidate_rule_id.as_deref())
    .bind(&conflict.rule_id)
    .bind(group.digest.source_repo.as_deref())
    .bind(&conflict.basis)
    .bind(candidate_title)
    .bind(candidate_body)
    .bind(&conflict.title)
    .bind(&conflict.active_body)
    .bind(candidate_patterns_json)
    .bind(active_patterns_json)
    .execute(pool)
    .await?;
    Ok(())
}

/// Read-only view over persisted conflict records, mirroring
/// `load_autopilot_log`. Never writes (status mutations belong to a future
/// reviewer command).
pub async fn load_memory_conflicts(
    pool: &SqlitePool,
    filter: MemoryConflictFilter,
) -> Result<MemoryConflictReport> {
    ensure_memory_conflicts_table(pool).await?;
    let limit = i64::try_from(normalize_limit(filter.limit.unwrap_or(50))).unwrap_or(50);
    let status = filter
        .status
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let rows = if let Some(status) = status {
        sqlx::query(
            "SELECT evidence_hash, candidate_group_id, candidate_rule_id, active_rule_id, \
                    source_repo, overlap_basis, candidate_title, candidate_body, active_title, \
                    active_body, candidate_patterns_json, active_patterns_json, llm_rationale, \
                    llm_confidence, status, created_at, updated_at \
             FROM memory_conflicts \
             WHERE status = ?1 \
             ORDER BY updated_at DESC, evidence_hash ASC \
             LIMIT ?2",
        )
        .bind(status)
        .bind(limit)
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query(
            "SELECT evidence_hash, candidate_group_id, candidate_rule_id, active_rule_id, \
                    source_repo, overlap_basis, candidate_title, candidate_body, active_title, \
                    active_body, candidate_patterns_json, active_patterns_json, llm_rationale, \
                    llm_confidence, status, created_at, updated_at \
             FROM memory_conflicts \
             ORDER BY updated_at DESC, evidence_hash ASC \
             LIMIT ?1",
        )
        .bind(limit)
        .fetch_all(pool)
        .await?
    };

    let conflicts = rows
        .into_iter()
        .map(|row| {
            let candidate_patterns_json: String = row
                .try_get("candidate_patterns_json")
                .unwrap_or_else(|_| "[]".to_owned());
            let active_patterns_json: String = row
                .try_get("active_patterns_json")
                .unwrap_or_else(|_| "[]".to_owned());
            MemoryConflictRecord {
                evidence_hash: row.try_get("evidence_hash").unwrap_or_default(),
                candidate_group_id: row.try_get("candidate_group_id").unwrap_or_default(),
                candidate_rule_id: row.try_get("candidate_rule_id").ok().flatten(),
                active_rule_id: row.try_get("active_rule_id").unwrap_or_default(),
                source_repo: row.try_get("source_repo").ok().flatten(),
                overlap_basis: row.try_get("overlap_basis").unwrap_or_default(),
                candidate_title: row.try_get("candidate_title").unwrap_or_default(),
                candidate_body: row.try_get("candidate_body").unwrap_or_default(),
                active_title: row.try_get("active_title").unwrap_or_default(),
                active_body: row.try_get("active_body").unwrap_or_default(),
                candidate_patterns: parse_string_list(Some(&candidate_patterns_json)),
                active_patterns: parse_string_list(Some(&active_patterns_json)),
                llm_rationale: row.try_get("llm_rationale").ok().flatten(),
                llm_confidence: row.try_get("llm_confidence").ok().flatten(),
                status: row.try_get("status").unwrap_or_default(),
                created_at: row.try_get("created_at").unwrap_or_default(),
                updated_at: row.try_get("updated_at").unwrap_or_default(),
            }
        })
        .collect();

    Ok(MemoryConflictReport {
        schema_version: MEMORY_AUTOPILOT_SCHEMA_VERSION.to_owned(),
        conflicts,
    })
}

/// Runtime guard mirroring the embedded migration; keep both in sync.
pub(crate) async fn ensure_memory_conflicts_table(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS memory_conflicts (
            evidence_hash TEXT PRIMARY KEY,
            candidate_group_id TEXT NOT NULL DEFAULT '',
            candidate_rule_id TEXT,
            active_rule_id TEXT NOT NULL DEFAULT '',
            source_repo TEXT,
            overlap_basis TEXT NOT NULL DEFAULT '',
            candidate_title TEXT NOT NULL DEFAULT '',
            candidate_body TEXT NOT NULL DEFAULT '',
            active_title TEXT NOT NULL DEFAULT '',
            active_body TEXT NOT NULL DEFAULT '',
            candidate_patterns_json TEXT NOT NULL DEFAULT '[]',
            active_patterns_json TEXT NOT NULL DEFAULT '[]',
            llm_rationale TEXT,
            llm_confidence REAL,
            status TEXT NOT NULL DEFAULT 'detected',
            created_at TEXT DEFAULT (datetime('now')) NOT NULL,
            updated_at TEXT DEFAULT (datetime('now')) NOT NULL
        )",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_memory_conflicts_status_updated \
         ON memory_conflicts (status, updated_at)",
    )
    .execute(pool)
    .await?;
    Ok(())
}
