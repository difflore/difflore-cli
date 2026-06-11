use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::domain::models::{AddExampleInput, RememberRuleInput, SkillRecord};
use crate::error::CoreError;
use crate::observability::privacy::{redact_secretish_tokens, strip_private_tagged_regions};

use super::{SkillRow, add_example, count_captures_today};

#[derive(Debug, Clone)]
pub struct RememberOutcome {
    pub skill: SkillRecord,
    /// The rule already existed and this call was a soft accept (+0.05
    /// confidence) rather than a new row. Set for both the content-hash
    /// window and title/body dedup paths.
    pub deduped: bool,
    /// `deduped` was driven by the content-hash + 30s window check
    /// (rapid-fire re-captures of identical content), as opposed to a
    /// deliberate re-capture later the same day. Always false when
    /// `deduped` is false.
    pub dedup_window_hit: bool,
    pub confidence_after: f64,
    /// Conversation-channel captures today *after* this call (counts fresh
    /// inserts and dedup bumps; manual captures don't count). Past
    /// `REMEMBER_WARN_THRESHOLD` the agent should warn about a runaway
    /// rate; past `REMEMBER_DAILY_LIMIT` the call is rejected before this
    /// struct is built.
    pub captures_today: i64,
}

/// Dedup window size in milliseconds. Identical content-hash captures
/// within this window collapse into a single soft-accept bump so an
/// agent in a tight loop cannot stack many +0.05 increments on one
/// rule.
pub const REMEMBER_DEDUP_WINDOW_MS: i64 = 30_000;

/// Confidence ceiling for conversation-channel rules. Caps agent-captured
/// rules below manually curated memory so a looping agent can't push one
/// past manual rules in ranking.
pub const REMEMBER_CONVERSATION_CONFIDENCE_CAP: f64 = 0.70;
pub const REMEMBER_BODY_CHAR_LIMIT: usize = 16 * 1024;
pub const REMEMBER_EXAMPLE_CHAR_LIMIT: usize = 16 * 1024;
pub const REMEMBER_FILE_PATTERN_LIMIT: usize = 32;
pub const REMEMBER_FILE_PATTERN_CHAR_LIMIT: usize = 256;

fn sanitize_remember_text(input: &str) -> String {
    redact_secretish_tokens(&strip_private_tagged_regions(input))
}

fn normalize_capture_client(input: Option<&str>) -> Option<String> {
    let value = input?.trim();
    if value.is_empty() {
        return None;
    }
    let normalized: String = value
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .take(64)
        .collect();
    (!normalized.is_empty()).then_some(normalized)
}

fn canonical_file_patterns_csv(patterns: Option<&[String]>) -> String {
    let Some(patterns) = patterns else {
        return String::new();
    };
    let mut patterns: Vec<String> = patterns
        .iter()
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .map(ToOwned::to_owned)
        .collect();
    patterns.sort();
    patterns.dedup();
    patterns.join(",")
}

fn parse_existing_file_patterns_csv(raw: Option<&str>) -> String {
    let Some(raw) = raw.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return String::new();
    };
    serde_json::from_str::<Vec<String>>(raw)
        .map(|patterns| canonical_file_patterns_csv(Some(&patterns)))
        .unwrap_or_default()
}

fn normalise_dedup_text(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn remember_bodies_semantically_match(incoming: &str, existing: &str) -> bool {
    let incoming = normalise_dedup_text(incoming);
    let existing = normalise_dedup_text(existing);
    if incoming.is_empty() || existing.is_empty() {
        return false;
    }
    if incoming == existing {
        return true;
    }

    let incoming_terms: std::collections::HashSet<&str> = incoming
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|term| term.len() >= 4)
        .collect();
    let existing_terms: std::collections::HashSet<&str> = existing
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|term| term.len() >= 4)
        .collect();
    if incoming_terms.len().min(existing_terms.len()) < 4 {
        return false;
    }
    let overlap = incoming_terms.intersection(&existing_terms).count();
    let union = incoming_terms.union(&existing_terms).count();
    union > 0 && (overlap as f64 / union as f64) >= 0.72
}

/// SHA-256 content hash for the dedup window:
/// `hex(sha256(patterns + "\n" + title + "\n" + body))`. The full digest
/// avoids a 64-bit collision strengthening an unrelated rule.
pub(crate) fn remember_content_hash(file_patterns_csv: &str, title: &str, body: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(file_patterns_csv.as_bytes());
    hasher.update(b"\n");
    hasher.update(title.as_bytes());
    hasher.update(b"\n");
    hasher.update(body.as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// Soft warning threshold: above this count surfaces warn without blocking.
/// Ten is a strong signal of an agent runaway or a deliberate batch import.
pub const REMEMBER_WARN_THRESHOLD: i64 = 10;

/// Hard daily limit (per-calendar-day, local time). 50 captures in a day
/// most likely means an agent is looping; blocking protects the corpus
/// from being polluted faster than the user can audit it.
pub const REMEMBER_DAILY_LIMIT: i64 = 50;

async fn strengthen_existing_remember_rule(
    db: &sqlx::SqlitePool,
    skill_id: &str,
    now: &str,
    reason: &str,
) -> crate::Result<f64> {
    let before: f64 = sqlx::query_scalar!(
        "SELECT confidence_score FROM skills WHERE id = ?1",
        skill_id
    )
    .fetch_one(db)
    .await?;
    let after = (before + 0.05).min(REMEMBER_CONVERSATION_CONFIDENCE_CAP);

    sqlx::query!(
        "UPDATE skills
         SET confidence_score = ?1,
             updated_at = ?2
         WHERE id = ?3",
        after,
        now,
        skill_id,
    )
    .execute(db)
    .await?;

    let event_id = format!("rule-event-{}", Uuid::new_v4());
    let metadata = serde_json::json!({
        "signal": "remember_rule_dedup",
        "delta": 0.05,
    })
    .to_string();
    sqlx::query!(
        "INSERT INTO rule_events
         (id, skill_id, kind, source, confidence_before, confidence_after, reason, metadata)
         VALUES (?1, ?2, 'feedback_accept', 'remember_rule', ?3, ?4, ?5, ?6)",
        event_id,
        skill_id,
        before,
        after,
        reason,
        metadata,
    )
    .execute(db)
    .await?;

    Ok(after)
}

async fn record_engine_link_failure(
    db: &sqlx::SqlitePool,
    skill_id: &str,
    engine: &str,
    error: &std::io::Error,
) {
    let event_id = format!("rule-event-{}", Uuid::new_v4());
    let reason = format!("sync_engine_link failed for engine {engine}: {error}");
    let metadata = serde_json::json!({
        "engine": engine,
        "enabled": true,
        "error": error.to_string(),
    })
    .to_string();
    if let Err(insert_err) = sqlx::query(
        "INSERT INTO rule_events
         (id, skill_id, kind, source, reason, metadata)
         VALUES (?1, ?2, 'engine_link_failed', 'remember_rule', ?3, ?4)",
    )
    .bind(event_id)
    .bind(skill_id)
    .bind(reason)
    .bind(metadata)
    .execute(db)
    .await
    {
        eprintln!("warning: failed to audit sync_engine_link failure: {insert_err}");
    }
}

/// Lifecycle status for a row in the local `skills` table. `Active` rows
/// are served by MCP; `Pending` rows are unreviewed drafts not served until
/// promoted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleStatus {
    Active,
    Pending,
}

impl RuleStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Pending => "pending",
        }
    }
}

/// Insert a rule via `remember()` and downgrade it to `status='pending'`
/// so MCP doesn't serve it. Used by import/extraction flows to land drafts
/// pending review. Idempotent: a dedup hit on an already-`active` row is
/// left alone (the user already reviewed it).
pub async fn remember_as_candidate(
    db: &sqlx::SqlitePool,
    input: RememberRuleInput,
) -> crate::Result<RememberOutcome> {
    let outcome = remember(db, input).await?;
    if !outcome.deduped {
        let skill_id = outcome.skill.id.as_str();
        sqlx::query!(
            "UPDATE skills SET status = 'pending' WHERE id = ?1",
            skill_id
        )
        .execute(db)
        .await?;
    }
    Ok(outcome)
}

/// Insert a `status='pending'` draft, seeding `confidence_score` from a
/// caller-computed value (e.g. the import gate's durability score) instead
/// of the flat conversation default. This only sets the seed confidence and
/// pending bit; routing stays the caller's responsibility. Idempotent on
/// the dedup path like `remember_as_candidate`.
pub async fn remember_as_candidate_with_confidence(
    db: &sqlx::SqlitePool,
    input: RememberRuleInput,
    confidence: f32,
) -> crate::Result<RememberOutcome> {
    let outcome = remember_inner(db, input, Some(f64::from(confidence))).await?;
    if !outcome.deduped {
        let skill_id = outcome.skill.id.as_str();
        sqlx::query!(
            "UPDATE skills SET status = 'pending' WHERE id = ?1",
            skill_id
        )
        .execute(db)
        .await?;
    }
    Ok(outcome)
}

pub async fn remember(
    db: &sqlx::SqlitePool,
    input: RememberRuleInput,
) -> crate::Result<RememberOutcome> {
    remember_inner(db, input, None).await
}

/// Shared body for `remember` and `remember_as_candidate_with_confidence`.
/// `confidence_override` seeds the fresh-insert `confidence_score`; `None`
/// starts conversation rules at 0.6. Fresh inserts are clamped to the
/// conversation ceiling; dedup bumps are unchanged.
async fn remember_inner(
    db: &sqlx::SqlitePool,
    input: RememberRuleInput,
    confidence_override: Option<f64>,
) -> crate::Result<RememberOutcome> {
    let title_trimmed = input.title.trim();
    if title_trimmed.is_empty() {
        return Err(CoreError::Validation(
            "remember_rule: title must not be empty".into(),
        ));
    }
    if input.body.trim().is_empty() {
        return Err(CoreError::Validation(
            "remember_rule: body must not be empty".into(),
        ));
    }
    if input.body.chars().count() > REMEMBER_BODY_CHAR_LIMIT {
        return Err(CoreError::Validation(format!(
            "remember_rule: body must be {REMEMBER_BODY_CHAR_LIMIT} chars or fewer"
        )));
    }
    for (label, value) in [
        ("bad_code", input.bad_code.as_deref()),
        ("good_code", input.good_code.as_deref()),
    ] {
        if value.is_some_and(|v| v.chars().count() > REMEMBER_EXAMPLE_CHAR_LIMIT) {
            return Err(CoreError::Validation(format!(
                "remember_rule: {label} must be {REMEMBER_EXAMPLE_CHAR_LIMIT} chars or fewer"
            )));
        }
    }
    if let Some(patterns) = input.file_patterns.as_ref() {
        if patterns.len() > REMEMBER_FILE_PATTERN_LIMIT {
            return Err(CoreError::Validation(format!(
                "remember_rule: file_patterns accepts at most {REMEMBER_FILE_PATTERN_LIMIT} entries"
            )));
        }
        if patterns
            .iter()
            .any(|p| p.chars().count() > REMEMBER_FILE_PATTERN_CHAR_LIMIT)
        {
            return Err(CoreError::Validation(format!(
                "remember_rule: file_patterns entries must be {REMEMBER_FILE_PATTERN_CHAR_LIMIT} chars or fewer"
            )));
        }
    }
    let body_sanitized = sanitize_remember_text(input.body.trim());
    let body_trimmed = body_sanitized.trim();

    // Path-traversal-safe slug (same algorithm as create_local) so the
    // generated directory name can't escape the skills root.
    let slug: String = title_trimmed
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if slug.is_empty() {
        return Err(CoreError::Validation(
            "remember_rule: title produces an empty slug after sanitization".into(),
        ));
    }

    let now_utc = chrono::Utc::now();
    let now = now_utc.format("%Y-%m-%d %H:%M:%S").to_string();
    let origin = input
        .origin
        .clone()
        .unwrap_or_else(|| "conversation".into());
    let captured_by_client = normalize_capture_client(input.captured_by_client.as_deref());

    // Hard rate limit, conversation-channel only (the `manual` CLI path is
    // exempt — a human typing rules isn't the failure mode; a looping agent
    // is). Counts fresh and dedup-bump captures alike, since the signal is
    // how many times the agent invoked remember_rule today.
    if origin == "conversation" {
        let captures_today = count_captures_today(db, &origin).await?;
        if captures_today >= REMEMBER_DAILY_LIMIT {
            return Err(CoreError::Validation(format!(
                "remember_rule daily cap reached ({captures_today}/{REMEMBER_DAILY_LIMIT}). \
                 If this is intentional, import review history with `difflore import-reviews`. \
                 If an agent is looping, run `difflore status --json` to audit local memory and archive noisy entries in DiffLore Cloud."
            )));
        }
    }

    // Content-hash input: canonical (trimmed/sorted/deduped) patterns +
    // title + body, so semantically identical glob sets don't fork
    // duplicate rules. No session id — rules are cross-session by nature.
    let file_patterns_csv = canonical_file_patterns_csv(input.file_patterns.as_deref());
    let content_hash = remember_content_hash(&file_patterns_csv, title_trimmed, body_trimmed);
    let now_ms: i64 = now_utc.timestamp_millis();
    let window_start_ms = now_ms - REMEMBER_DEDUP_WINDOW_MS;

    // Cross-run exact-content dedup for non-conversation channels: imports
    // and extraction jobs re-run often, so an identical (patterns, title,
    // body) hash collapses regardless of age.
    if origin != "conversation" {
        let existing_id: Option<String> = sqlx::query_scalar(
            "SELECT id FROM skills WHERE content_hash = ?1 \
             ORDER BY hash_created_at ASC, id ASC LIMIT 1",
        )
        .bind(content_hash.as_str())
        .fetch_optional(db)
        .await?;
        if let Some(existing) = existing_id {
            let update_now = now.as_str();
            let confidence_after = strengthen_existing_remember_rule(
                db,
                existing.as_str(),
                update_now,
                "import content-hash dedup",
            )
            .await?;
            let row = sqlx::query_as!(
                SkillRow,
                "SELECT id, name, source, directory, version, description, type, \
                 engines, tags, trigger, check_prompt, repo_owner, repo_name, repo_branch, readme_url, \
                 enabled_for_codex, enabled_for_claude, enabled_for_gemini, enabled_for_cursor, \
                 installed_at, updated_at, origin FROM skills WHERE id = ?1",
                existing
            )
            .fetch_one(db)
            .await?;
            let captures_today = count_captures_today(db, &origin).await?;
            return Ok(RememberOutcome {
                skill: SkillRecord::from(row),
                deduped: true,
                dedup_window_hit: false,
                confidence_after,
                captures_today,
            });
        }
    }

    // Window-dedup guard: identical content within the last 30s collapses
    // into one soft-accept bump. Older duplicates fall through to title/body
    // dedup so deliberate re-captures can still strengthen the rule.
    let window_content_hash = content_hash.as_str();
    let window_hit_id: Option<String> = sqlx::query_scalar(
        "SELECT id FROM skills \
         WHERE content_hash = ?1 \
         AND origin = 'conversation' \
         AND hash_created_at IS NOT NULL \
         AND hash_created_at >= ?2 \
         ORDER BY hash_created_at DESC, id ASC LIMIT 1",
    )
    .bind(window_content_hash)
    .bind(window_start_ms)
    .fetch_optional(db)
    .await?;

    if let Some(existing) = window_hit_id {
        let update_now = now.as_str();
        let confidence_after = strengthen_existing_remember_rule(
            db,
            existing.as_str(),
            update_now,
            "dedup window hit",
        )
        .await?;
        let row = sqlx::query_as!(
            SkillRow,
            "SELECT id, name, source, directory, version, description, type, \
             engines, tags, trigger, check_prompt, repo_owner, repo_name, repo_branch, readme_url, \
             enabled_for_codex, enabled_for_claude, enabled_for_gemini, enabled_for_cursor, \
             installed_at, updated_at, origin FROM skills WHERE id = ?1",
            existing
        )
        .fetch_one(db)
        .await?;
        let captures_today = count_captures_today(db, &origin).await?;
        return Ok(RememberOutcome {
            skill: SkillRecord::from(row),
            deduped: true,
            dedup_window_hit: true,
            confidence_after,
            captures_today,
        });
    }

    // Title/body dedup guard: outside the 30s window, a matching normalised
    // title and similar body becomes a soft confidence signal, not a new row.
    let id_prefix = format!("conv-{slug}-");
    let legacy_rows = sqlx::query_as::<_, (String, String, Option<String>)>(
        "SELECT id, description, file_patterns FROM skills \
         WHERE id LIKE ?1 || '%' AND origin = 'conversation' \
         ORDER BY installed_at ASC, id ASC LIMIT 10",
    )
    .bind(id_prefix)
    .fetch_all(db)
    .await?;
    let existing_id = legacy_rows.into_iter().find_map(|row| {
        let (id, description, file_patterns) = row;
        let existing_patterns = parse_existing_file_patterns_csv(file_patterns.as_deref());
        (existing_patterns == file_patterns_csv
            && remember_bodies_semantically_match(body_trimmed, &description))
        .then_some(id)
    });

    if let Some(existing) = existing_id {
        let update_now = now.as_str();
        let confidence_after =
            strengthen_existing_remember_rule(db, existing.as_str(), update_now, "title dedup")
                .await?;
        let row = sqlx::query_as!(
            SkillRow,
            "SELECT id, name, source, directory, version, description, type, \
             engines, tags, trigger, check_prompt, repo_owner, repo_name, repo_branch, readme_url, \
             enabled_for_codex, enabled_for_claude, enabled_for_gemini, enabled_for_cursor, \
             installed_at, updated_at, origin FROM skills WHERE id = ?1",
            existing
        )
        .fetch_one(db)
        .await?;
        let captures_today = count_captures_today(db, &origin).await?;
        return Ok(RememberOutcome {
            skill: SkillRecord::from(row),
            deduped: true,
            dedup_window_hit: false,
            confidence_after,
            captures_today,
        });
    }

    // No collision — fresh insert. The suffix keeps disk paths unique even
    // if two unrelated captures slug to the same root.
    let id_suffix = Uuid::new_v4()
        .to_string()
        .chars()
        .take(8)
        .collect::<String>();
    let id = format!("conv-{slug}-{id_suffix}");
    let file_patterns_json = input
        .file_patterns
        .as_ref()
        .filter(|v| !v.is_empty())
        .map(serde_json::to_string)
        .transpose()?;

    // Assemble SKILL.md so it reads naturally on disk. Severity (if given)
    // becomes a metadata line; bad/good examples land in rule_examples below.
    let mut skill_md = String::new();
    skill_md.push_str("---\n");
    skill_md.push_str("type: review_standard\n");
    skill_md.push_str("engines: [claude]\n");
    skill_md.push_str(&format!("tags: [{origin}, conversation]\n"));
    skill_md.push_str("---\n\n");
    skill_md.push_str(&format!("# {title_trimmed}\n\n"));
    if let Some(sev) = input.severity.as_deref().filter(|s| !s.is_empty()) {
        skill_md.push_str(&format!("**Severity:** {sev}\n\n"));
    }
    skill_md.push_str(body_trimmed);
    skill_md.push('\n');

    // Persist to disk so hand-edits round-trip, path-confined to the
    // skills/local/ root via the same canonicalisation guard as create_local.
    let base_dir = crate::skills::fs::skills_base_dir()
        .map_err(CoreError::Internal)?
        .join("local");
    std::fs::create_dir_all(&base_dir)
        .map_err(|e| CoreError::Internal(format!("failed to create skills dir: {e}")))?;
    let canonical_base = base_dir
        .canonicalize()
        .map_err(|e| CoreError::Internal(format!("failed to resolve skills dir: {e}")))?;
    let skill_dir = base_dir.join(&id);
    let skill_dir_for_check = canonical_base.join(&id);
    if !skill_dir_for_check.starts_with(&canonical_base) {
        return Err(CoreError::Validation(
            "remember_rule: invalid slug after sanitization".into(),
        ));
    }
    std::fs::create_dir_all(&skill_dir)
        .map_err(|e| CoreError::Internal(format!("failed to create skill directory: {e}")))?;
    let canonical_skill = skill_dir
        .canonicalize()
        .map_err(|e| CoreError::Internal(format!("failed to resolve skill directory: {e}")))?;
    if !canonical_skill.starts_with(&canonical_base) {
        return Err(CoreError::Validation("remember_rule: path escape".into()));
    }
    std::fs::write(skill_dir.join("SKILL.md"), &skill_md)
        .map_err(|e| CoreError::Internal(format!("failed to write SKILL.md: {e}")))?;

    let engines_json = serde_json::to_string(&["claude"])?;
    // Tags always include the origin marker; "conversation" is added only
    // when the origin differs (e.g. the `manual` path) so a
    // `tags=conversation` search finds agent-mediated captures specifically.
    let tags_vec: Vec<String> = if origin == "conversation" {
        vec!["conversation".into()]
    } else {
        vec![origin.clone(), "conversation".into()]
    };
    let tags_json = serde_json::to_string(&tags_vec)?;
    let description = body_trimmed.to_owned();
    // Conversation rules start at 0.6 (below manual). A caller seed replaces
    // that base but is still clamped to [0.0, conversation ceiling].
    let confidence: f64 =
        confidence_override.map_or(0.6, |c| c.clamp(0.0, REMEMBER_CONVERSATION_CONFIDENCE_CAP));

    let insert_id = id.as_str();
    let insert_directory = id.as_str();
    let insert_description = description.as_str();
    let insert_engines = engines_json.as_str();
    let insert_tags = tags_json.as_str();
    let insert_file_patterns = file_patterns_json.as_deref();
    let insert_now = now.as_str();
    let insert_origin = origin.as_str();
    let insert_captured_by_client = captured_by_client.as_deref();
    let insert_content_hash = content_hash.as_str();
    let insert_result = sqlx::query!(
        "INSERT INTO skills
         (id, name, source, directory, version, description, type, engines, tags,
          trigger, check_prompt, file_patterns, enabled_for_claude, confidence_score,
          installed_at, updated_at, origin, captured_by_client, content_hash, hash_created_at)
         VALUES (?1, ?2, 'local', ?3, '1.0.0', ?4, 'review_standard', ?5, ?6,
                 NULL, NULL, ?7, 1, ?8, ?9, ?9, ?10, ?11, ?12, ?13)",
        insert_id,
        title_trimmed,
        insert_directory,
        insert_description,
        insert_engines,
        insert_tags,
        insert_file_patterns,
        confidence,
        insert_now,
        insert_origin,
        insert_captured_by_client,
        insert_content_hash,
        now_ms
    )
    .execute(db)
    .await;
    if let Err(e) = insert_result {
        let _ = std::fs::remove_dir_all(&skill_dir);
        return Err(e.into());
    }

    // Keep the claude engine link consistent with `enabled_for_claude=1`.
    if let Err(e) = crate::skills::fs::sync_engine_link("local", &id, "claude", true) {
        eprintln!("warning: sync_engine_link failed for engine claude: {e}");
        record_engine_link_failure(db, &id, "claude", &e).await;
    }

    // Insert the bad/good example only when both sides are present — a
    // one-sided example tends to hurt few-shot quality.
    if let (Some(bad), Some(good)) = (input.bad_code.as_deref(), input.good_code.as_deref()) {
        let bad = sanitize_remember_text(bad);
        let good = sanitize_remember_text(good);
        if !bad.trim().is_empty() && !good.trim().is_empty() {
            let example_input = AddExampleInput {
                skill_id: id.clone(),
                bad_code: bad,
                good_code: good,
                description: None,
                source: Some(origin.clone()),
            };
            if let Err(e) = add_example(db, example_input).await {
                eprintln!("warning: failed to attach example to remembered rule: {e}");
            }
        }
    }

    let row = sqlx::query_as!(
        SkillRow,
        "SELECT id, name, source, directory, version, description, type, \
         engines, tags, trigger, check_prompt, repo_owner, repo_name, repo_branch, readme_url, \
         enabled_for_codex, enabled_for_claude, enabled_for_gemini, enabled_for_cursor, \
         installed_at, updated_at, origin FROM skills WHERE id = ?1",
        id
    )
    .fetch_one(db)
    .await?;
    let captures_today = count_captures_today(db, &origin).await?;
    Ok(RememberOutcome {
        skill: SkillRecord::from(row),
        deduped: false,
        dedup_window_hit: false,
        confidence_after: confidence,
        captures_today,
    })
}
