use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::domain::models::{AddExampleInput, RememberRuleInput, SkillRecord};
use crate::error::CoreError;
use crate::infra::git::RepoScope;
use crate::observability::privacy::{redact_secrets, strip_private_tagged_regions};

use super::semantic_dedup::{SemanticRuleKey, semantic_rules_match};
use super::{add_example, count_captures_today, fetch_skill_row_by_id};

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
    /// `deduped` collapsed a fresh PENDING import into an already-`active`
    /// rule of identical content (re-import of a promoted rule). Unlike the
    /// other dedup paths this does NOT strengthen confidence — the approved
    /// rule is left untouched — so callers must not report it as a soft
    /// accept. Always false when `deduped` is false.
    pub matched_existing_active: bool,
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
pub const REMEMBER_KIND_REVIEW_RULE: &str = "review_rule";
pub const REMEMBER_KIND_SOFT_PREFERENCE: &str = "soft_preference";
pub const SOFT_PREFERENCE_RULE_TYPE: &str = "soft_preference";
const REVIEW_RULE_TYPE: &str = "review_standard";
const SOFT_PREFERENCE_CATEGORIES: &[&str] =
    &["workflow_preference", "user_preference", "project_context"];

fn sanitize_remember_text(input: &str) -> String {
    redact_secrets(&strip_private_tagged_regions(input))
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

fn parse_existing_file_patterns(raw: Option<&str>) -> Vec<String> {
    let Some(raw) = raw.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<String>>(raw).unwrap_or_default()
}

fn normalize_remember_kind(kind: Option<&str>, category: Option<&str>) -> Option<&'static str> {
    let kind = kind.map(str::trim).filter(|s| !s.is_empty());
    match kind {
        None => Some(
            normalize_soft_preference_category(category)
                .map_or(REMEMBER_KIND_REVIEW_RULE, |_| REMEMBER_KIND_SOFT_PREFERENCE),
        ),
        Some("review_rule" | "review" | "review_standard") => Some(REMEMBER_KIND_REVIEW_RULE),
        Some("soft_preference" | "soft-preference" | "preference" | "context") => {
            Some(REMEMBER_KIND_SOFT_PREFERENCE)
        }
        Some(_) => None,
    }
}

fn normalize_soft_preference_category(category: Option<&str>) -> Option<String> {
    let category = category?.trim();
    if category.is_empty() {
        return None;
    }
    let normalized = category.replace('-', "_").to_ascii_lowercase();
    SOFT_PREFERENCE_CATEGORIES
        .iter()
        .any(|known| *known == normalized)
        .then_some(normalized)
}

fn remember_rule_type(kind: &str) -> &'static str {
    if kind == REMEMBER_KIND_SOFT_PREFERENCE {
        SOFT_PREFERENCE_RULE_TYPE
    } else {
        REVIEW_RULE_TYPE
    }
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

/// Public helper for non-creation paths that update a rule's persisted body.
/// Keep this routed through the same normalization as `remember_inner` so
/// exact-content dedup remains coherent after package import/edit flows.
pub fn remember_rule_content_hash(
    title: &str,
    body: &str,
    file_patterns: Option<&[String]>,
) -> String {
    let title = title.trim();
    let body_sanitized = sanitize_remember_text(body.trim());
    let body = body_sanitized.trim();
    let patterns_csv = canonical_file_patterns_csv(file_patterns);
    remember_content_hash(&patterns_csv, title, body)
}

/// The single source of truth for the dedup/tombstone content hash of a
/// `RememberRuleInput`. `remember_inner` (the creation path) and
/// `is_rejected_signature` (the import-suppression lookup) both route through
/// this so the hash they compare can never drift apart. Mirrors exactly the
/// normalisation `remember_inner` applies before its content-hash dedup:
/// trimmed title, sanitised+trimmed body, canonical (trimmed/sorted/deduped)
/// file-pattern CSV.
fn remember_signature_hash(input: &RememberRuleInput) -> String {
    remember_rule_content_hash(&input.title, &input.body, input.file_patterns.as_deref())
}

/// True if this exact rule content was previously rejected and tombstoned,
/// so the import pipeline must not re-create it.
pub async fn is_rejected_signature(
    db: &sqlx::SqlitePool,
    input: &RememberRuleInput,
) -> crate::Result<bool> {
    let content_hash = remember_signature_hash(input);
    // Runtime-checked (non-macro) query: the `rejected_signatures` table has
    // no entry in the committed `.sqlx/` offline cache, so a `query!` macro
    // would fail to compile under SQLX_OFFLINE.
    let hit: Option<i64> =
        sqlx::query_scalar("SELECT 1 FROM rejected_signatures WHERE content_hash = ?1 LIMIT 1")
            .bind(content_hash.as_str())
            .fetch_optional(db)
            .await?;
    Ok(hit.is_some())
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
    status: RuleStatus,
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
        "trustState": status.as_str(),
        "dedupScope": status.as_str(),
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

/// Strengthen an existing rule (soft +0.05 accept) and assemble the
/// `RememberOutcome` for a dedup hit. Shared by the three strengthen-based
/// dedup branches (cross-run content hash, 30s window, title/body) so the
/// strengthen → re-fetch → outcome assembly lives in one place. `window_hit`
/// distinguishes the 30s-window branch; the other two pass `false`.
async fn strengthen_dedup_outcome(
    db: &sqlx::SqlitePool,
    existing_id: &str,
    now: &str,
    reason: &str,
    status: RuleStatus,
    origin: &str,
    window_hit: bool,
) -> crate::Result<RememberOutcome> {
    let confidence_after =
        strengthen_existing_remember_rule(db, existing_id, now, reason, status).await?;
    let skill = fetch_skill_row_by_id(db, existing_id).await?;
    let captures_today = count_captures_today(db, origin).await?;
    Ok(RememberOutcome {
        skill,
        deduped: true,
        dedup_window_hit: window_hit,
        matched_existing_active: false,
        confidence_after,
        captures_today,
    })
}

async fn record_remember_provenance_event(
    db: &sqlx::SqlitePool,
    skill_id: &str,
    origin: &str,
    captured_by_client: Option<&str>,
    content_hash: &str,
    status: RuleStatus,
) -> crate::Result<()> {
    let event_id = format!("rule-event-{}", Uuid::new_v4());
    let metadata = serde_json::json!({
        "source": "remember_rule",
        "origin": origin,
        "capturedByClient": captured_by_client,
        "contentHash": content_hash,
        "trustState": status.as_str(),
        "servedToAgents": status == RuleStatus::Active,
        "requiresUserApproval": status == RuleStatus::Pending,
    })
    .to_string();
    let reason = match status {
        RuleStatus::Active => "Captured active memory through remember_rule",
        RuleStatus::Pending => "Proposed untrusted memory draft through remember_rule",
    };
    sqlx::query(
        "INSERT INTO rule_events
         (id, skill_id, kind, source, reason, metadata)
         VALUES (?1, ?2, 'capture_provenance', 'remember_rule_provenance', ?3, ?4)",
    )
    .bind(event_id)
    .bind(skill_id)
    .bind(reason)
    .bind(metadata)
    .execute(db)
    .await?;
    Ok(())
}

async fn find_semantic_pr_review_match(
    db: &sqlx::SqlitePool,
    input: &RememberRuleInput,
    body_trimmed: &str,
    source_repo: &str,
    status: RuleStatus,
) -> crate::Result<Option<(String, f64)>> {
    let incoming_patterns = input.file_patterns.clone().unwrap_or_default();
    let incoming = SemanticRuleKey::new(&input.title, body_trimmed, &incoming_patterns);
    let rows = sqlx::query_as::<_, (String, String, String, Option<String>, f64)>(
        "SELECT id, name, description, file_patterns, confidence_score FROM skills
         WHERE origin = 'pr_review'
           AND status = ?1
           AND lower(source_repo) = lower(?2)
         ORDER BY installed_at ASC, id ASC",
    )
    .bind(status.as_str())
    .bind(source_repo)
    .fetch_all(db)
    .await?;

    Ok(rows
        .into_iter()
        .find_map(|(id, title, description, file_patterns, confidence)| {
            let existing_patterns = parse_existing_file_patterns(file_patterns.as_deref());
            let candidate = SemanticRuleKey::new(&title, &description, &existing_patterns);
            semantic_rules_match(&incoming, &candidate).then_some((id, confidence))
        }))
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

/// Insert a `status='pending'` draft so MCP doesn't serve it. Used by
/// import/extraction flows to land drafts pending review. Dedup is scoped to
/// pending rows only, so an untrusted draft can never strengthen or inherit
/// trust from an already-approved active rule.
pub async fn remember_as_candidate(
    db: &sqlx::SqlitePool,
    input: RememberRuleInput,
) -> crate::Result<RememberOutcome> {
    remember_inner(db, input, None, RuleStatus::Pending, None).await
}

/// Insert a `status='pending'` draft, seeding `confidence_score` from a
/// caller-computed value (e.g. the import gate's durability score) instead
/// of the flat conversation default. This only sets the seed confidence and
/// pending bit; routing stays the caller's responsibility. Idempotent on
/// the dedup path like `remember_as_candidate`: within the pending trust tier
/// only.
pub async fn remember_as_candidate_with_confidence(
    db: &sqlx::SqlitePool,
    input: RememberRuleInput,
    confidence: f32,
) -> crate::Result<RememberOutcome> {
    remember_inner(
        db,
        input,
        Some(f64::from(confidence)),
        RuleStatus::Pending,
        None,
    )
    .await
}

/// Like [`remember_as_candidate_with_confidence`], but writes the canonical
/// repository scope atomically with the candidate and uses it for same-repo
/// semantic dedupe before insert. Review-import callers should prefer this
/// over post-insert `source_repo` patching so GitHub/GitLab candidates can be
/// compared without crossing repository boundaries.
pub async fn remember_as_candidate_with_confidence_for_repo(
    db: &sqlx::SqlitePool,
    input: RememberRuleInput,
    confidence: f32,
    source_repo: &RepoScope,
) -> crate::Result<RememberOutcome> {
    remember_inner(
        db,
        input,
        Some(f64::from(confidence)),
        RuleStatus::Pending,
        Some(source_repo),
    )
    .await
}

pub async fn remember(
    db: &sqlx::SqlitePool,
    input: RememberRuleInput,
) -> crate::Result<RememberOutcome> {
    remember_inner(db, input, None, RuleStatus::Active, None).await
}

/// Insert or dedupe an approved active rule while atomically recording the
/// repository scope. Used by trusted local onboarding sources that should be
/// served immediately, but only inside the repo that supplied the memory.
pub async fn remember_for_repo(
    db: &sqlx::SqlitePool,
    input: RememberRuleInput,
    source_repo: &RepoScope,
) -> crate::Result<RememberOutcome> {
    remember_inner(db, input, None, RuleStatus::Active, Some(source_repo)).await
}

/// Guard the size/shape invariants of a `RememberRuleInput` before any DB work.
/// Mirrors the user-facing limits the MCP surface advertises; on success the
/// caller may assume non-empty title/body and bounded examples/patterns.
fn validate_remember_input(input: &RememberRuleInput) -> crate::Result<()> {
    if input.title.trim().is_empty() {
        return Err(CoreError::Validation(
            "remember_rule: title must not be empty".into(),
        ));
    }
    if input.body.trim().is_empty() {
        return Err(CoreError::Validation(
            "remember_rule: body must not be empty".into(),
        ));
    }
    let Some(kind) = normalize_remember_kind(input.kind.as_deref(), input.category.as_deref())
    else {
        return Err(CoreError::Validation(
            "remember_rule: kind must be review_rule or soft_preference".into(),
        ));
    };
    if kind == REMEMBER_KIND_SOFT_PREFERENCE
        && input.category.as_deref().is_some_and(|category| {
            !category.trim().is_empty()
                && normalize_soft_preference_category(Some(category)).is_none()
        })
    {
        return Err(CoreError::Validation(
            "remember_rule: soft preference category must be workflow_preference, user_preference, or project_context".into(),
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
    Ok(())
}

/// Enforce the per-day conversation-channel capture cap. Conversation is the
/// only origin gated here (the `manual` CLI path is exempt — a human typing
/// rules isn't the failure mode; a looping agent is). Counts fresh and
/// dedup-bump captures alike, since the signal is how many times the agent
/// invoked remember_rule today.
async fn enforce_remember_rate_limit(db: &sqlx::SqlitePool, origin: &str) -> crate::Result<()> {
    if origin == "conversation" {
        let captures_today = count_captures_today(db, origin).await?;
        if captures_today >= REMEMBER_DAILY_LIMIT {
            return Err(CoreError::Validation(format!(
                "remember_rule daily cap reached ({captures_today}/{REMEMBER_DAILY_LIMIT}). \
                 If this is intentional, import review history with `difflore import-reviews`. \
                 If an agent is looping, run `difflore status --json` to audit local memory and archive noisy entries in DiffLore Cloud."
            )));
        }
    }
    Ok(())
}

/// Shared body for `remember` and `remember_as_candidate_with_confidence`.
/// `confidence_override` seeds the fresh-insert `confidence_score`; `None`
/// starts conversation rules at 0.6. Fresh inserts are clamped to the
/// conversation ceiling; dedup bumps are unchanged. Dedup is always scoped to
/// the same lifecycle status as the caller's fresh insert, keeping pending
/// drafts and approved active rules in separate trust tiers.
///
/// Reads as a short pipeline: validate → slug → rate-limit → the dedup ladder
/// (cross-run content hash, 30s window, title/body) → fresh insert. Each dedup
/// rung returns early via [`strengthen_dedup_outcome`]; only a clean miss
/// reaches the insert at the end.
async fn remember_inner(
    db: &sqlx::SqlitePool,
    input: RememberRuleInput,
    confidence_override: Option<f64>,
    status: RuleStatus,
    source_repo_scope: Option<&RepoScope>,
) -> crate::Result<RememberOutcome> {
    validate_remember_input(&input)?;
    let title_trimmed = input.title.trim();
    let body_sanitized = sanitize_remember_text(input.body.trim());
    let body_trimmed = body_sanitized.trim();

    // Path-traversal-safe slug (shared with create_local) so the generated
    // directory name can't escape the skills root.
    let Some(slug) = crate::skills::fs::safe_slug(title_trimmed) else {
        return Err(CoreError::Validation(
            "remember_rule: title produces an empty slug after sanitization".into(),
        ));
    };

    let now_utc = chrono::Utc::now();
    let now = now_utc.format("%Y-%m-%d %H:%M:%S").to_string();
    let origin = input
        .origin
        .clone()
        .unwrap_or_else(|| "conversation".into());
    let normalized_kind = normalize_remember_kind(input.kind.as_deref(), input.category.as_deref())
        .unwrap_or(REMEMBER_KIND_REVIEW_RULE);
    let normalized_category = normalize_soft_preference_category(input.category.as_deref());
    let skill_type = remember_rule_type(normalized_kind);
    let captured_by_client = normalize_capture_client(input.captured_by_client.as_deref());
    let source_repo = source_repo_scope.map(RepoScope::as_str);

    enforce_remember_rate_limit(db, &origin).await?;

    // Content-hash input: canonical (trimmed/sorted/deduped) patterns +
    // title + body, so semantically identical glob sets don't fork
    // duplicate rules. No session id — rules are cross-session by nature.
    let file_patterns_csv = canonical_file_patterns_csv(input.file_patterns.as_deref());
    // Share the exact hash computation with `is_rejected_signature` so the
    // creation path and the import-suppression lookup can never drift. The
    // helper re-derives title/body/patterns from `input` identically to the
    // locals computed above, so the resulting hash is unchanged.
    let content_hash = remember_signature_hash(&input);
    let now_ms: i64 = now_utc.timestamp_millis();
    let window_start_ms = now_ms - REMEMBER_DEDUP_WINDOW_MS;

    // Cross-run exact-content dedup for non-conversation channels: imports
    // and extraction jobs re-run often, so an identical (patterns, title,
    // body) hash collapses regardless of age.
    let status_filter = status.as_str();
    if origin != "conversation" {
        // Review re-import must not fork a duplicate PENDING draft of content
        // the user already approved. The same-status dedup below only matches
        // the pending tier, so an identical rule promoted to `active` on a
        // prior import would otherwise be re-created as a fresh draft every run
        // (then trip promote_candidate's "duplicates active rule" guard and
        // abort the import). Dedup into the approved rule instead and — unlike
        // the pending soft-strengthen path below — leave it untouched:
        // re-seeing the same review comment is not new evidence, so confidence
        // and status stay put.
        //
        // Gated to the review-import origin ("pr_review"; see
        // import_reviews::local_candidates) ON PURPOSE. Other non-conversation
        // origins (notably `session_mined`) carry their own repo scope and
        // approval flow; silently collapsing one into a same-hash active rule
        // of a DIFFERENT repo would consume the candidate without ever serving
        // it where it came from. Those callers keep the original pending-tier
        // behavior.
        if origin == "pr_review" && status == RuleStatus::Pending {
            let active_existing: Option<(String, f64)> = if let Some(repo) = source_repo {
                sqlx::query_as(
                    "SELECT id, confidence_score FROM skills \
                     WHERE content_hash = ?1 AND status = 'active' \
                       AND lower(source_repo) = lower(?2) \
                     ORDER BY hash_created_at ASC, id ASC LIMIT 1",
                )
                .bind(content_hash.as_str())
                .bind(repo)
                .fetch_optional(db)
                .await?
            } else {
                sqlx::query_as(
                    "SELECT id, confidence_score FROM skills \
                     WHERE content_hash = ?1 AND status = 'active' \
                     ORDER BY hash_created_at ASC, id ASC LIMIT 1",
                )
                .bind(content_hash.as_str())
                .fetch_optional(db)
                .await?
            };
            if let Some((existing, confidence_after)) = active_existing {
                // Unlike the strengthen-based rungs below, the approved rule is
                // left untouched here — re-seeing the same review comment is not
                // new evidence — so this branch assembles its own outcome.
                let skill = fetch_skill_row_by_id(db, existing.as_str()).await?;
                let captures_today = count_captures_today(db, &origin).await?;
                return Ok(RememberOutcome {
                    skill,
                    deduped: true,
                    dedup_window_hit: false,
                    matched_existing_active: true,
                    confidence_after,
                    captures_today,
                });
            }
        }
        let existing_id: Option<String> = if let Some(repo) = source_repo {
            sqlx::query_scalar(
                "SELECT id FROM skills WHERE content_hash = ?1 \
                 AND status = ?2 \
                 AND lower(source_repo) = lower(?3) \
                 ORDER BY hash_created_at ASC, id ASC LIMIT 1",
            )
            .bind(content_hash.as_str())
            .bind(status_filter)
            .bind(repo)
            .fetch_optional(db)
            .await?
        } else {
            sqlx::query_scalar(
                "SELECT id FROM skills WHERE content_hash = ?1 \
                 AND status = ?2 \
                 ORDER BY hash_created_at ASC, id ASC LIMIT 1",
            )
            .bind(content_hash.as_str())
            .bind(status_filter)
            .fetch_optional(db)
            .await?
        };
        if let Some(existing) = existing_id {
            return strengthen_dedup_outcome(
                db,
                existing.as_str(),
                now.as_str(),
                "import content-hash dedup",
                status,
                &origin,
                false,
            )
            .await;
        }

        if origin == "pr_review" && status == RuleStatus::Pending {
            if let Some(repo) = source_repo {
                if let Some((existing, confidence_after)) = find_semantic_pr_review_match(
                    db,
                    &input,
                    body_trimmed,
                    repo,
                    RuleStatus::Active,
                )
                .await?
                {
                    let skill = fetch_skill_row_by_id(db, existing.as_str()).await?;
                    let captures_today = count_captures_today(db, &origin).await?;
                    return Ok(RememberOutcome {
                        skill,
                        deduped: true,
                        dedup_window_hit: false,
                        matched_existing_active: true,
                        confidence_after,
                        captures_today,
                    });
                }
                if let Some((existing, _)) = find_semantic_pr_review_match(
                    db,
                    &input,
                    body_trimmed,
                    repo,
                    RuleStatus::Pending,
                )
                .await?
                {
                    return strengthen_dedup_outcome(
                        db,
                        existing.as_str(),
                        now.as_str(),
                        "import semantic dedup",
                        status,
                        &origin,
                        false,
                    )
                    .await;
                }
            }
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
         AND status = ?2 \
         AND hash_created_at IS NOT NULL \
         AND hash_created_at >= ?3 \
         ORDER BY hash_created_at DESC, id ASC LIMIT 1",
    )
    .bind(window_content_hash)
    .bind(status_filter)
    .bind(window_start_ms)
    .fetch_optional(db)
    .await?;

    if let Some(existing) = window_hit_id {
        return strengthen_dedup_outcome(
            db,
            existing.as_str(),
            now.as_str(),
            "dedup window hit",
            status,
            &origin,
            true,
        )
        .await;
    }

    // Title/body dedup guard: outside the 30s window, a matching normalised
    // title and similar body becomes a soft confidence signal, not a new row.
    let id_prefix = format!("conv-{slug}-");
    let legacy_rows = sqlx::query_as::<_, (String, String, Option<String>)>(
        "SELECT id, description, file_patterns FROM skills \
         WHERE id LIKE ?1 || '%' AND origin = 'conversation' \
         AND status = ?2 \
         ORDER BY installed_at ASC, id ASC LIMIT 10",
    )
    .bind(id_prefix)
    .bind(status_filter)
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
        return strengthen_dedup_outcome(
            db,
            existing.as_str(),
            now.as_str(),
            "title dedup",
            status,
            &origin,
            false,
        )
        .await;
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

    let engines_json = serde_json::to_string(&Vec::<String>::new())?;
    // Tags always include the origin marker; "conversation" is added only
    // when the origin differs (e.g. the `manual` path) so a
    // `tags=conversation` search finds agent-mediated captures specifically.
    let mut tags_vec: Vec<String> = if origin == "conversation" {
        vec!["conversation".into()]
    } else {
        vec![origin.clone(), "conversation".into()]
    };
    if normalized_kind == REMEMBER_KIND_SOFT_PREFERENCE {
        tags_vec.push(REMEMBER_KIND_SOFT_PREFERENCE.to_owned());
        if let Some(category) = normalized_category.as_ref() {
            tags_vec.push(category.clone());
            tags_vec.push(format!("category:{category}"));
        }
    }
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
    let insert_source_repo = source_repo;
    let insert_content_hash = content_hash.as_str();
    let insert_status = status.as_str();
    let insert_result = sqlx::query(
        "INSERT INTO skills
         (id, name, source, directory, version, description, type, engines, tags,
          trigger, check_prompt, file_patterns, source_repo, enabled_for_claude, confidence_score,
          installed_at, updated_at, origin, captured_by_client, content_hash, hash_created_at,
          status)
         VALUES (?1, ?2, 'local', ?3, '1.0.0', ?4, ?5, ?6, ?7,
                 NULL, NULL, ?8, ?9, 1, ?10, ?11, ?11, ?12, ?13, ?14, ?15, ?16)",
    )
    .bind(insert_id)
    .bind(title_trimmed)
    .bind(insert_directory)
    .bind(insert_description)
    .bind(skill_type)
    .bind(insert_engines)
    .bind(insert_tags)
    .bind(insert_file_patterns)
    .bind(insert_source_repo)
    .bind(confidence)
    .bind(insert_now)
    .bind(insert_origin)
    .bind(insert_captured_by_client)
    .bind(insert_content_hash)
    .bind(now_ms)
    .bind(insert_status)
    .execute(db)
    .await;
    if let Err(e) = insert_result {
        return Err(e.into());
    }
    record_remember_provenance_event(
        db,
        &id,
        insert_origin,
        insert_captured_by_client,
        insert_content_hash,
        status,
    )
    .await?;

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

    let skill = fetch_skill_row_by_id(db, &id).await?;
    let captures_today = count_captures_today(db, &origin).await?;
    Ok(RememberOutcome {
        skill,
        deduped: false,
        dedup_window_hit: false,
        matched_existing_active: false,
        confidence_after: confidence,
        captures_today,
    })
}
