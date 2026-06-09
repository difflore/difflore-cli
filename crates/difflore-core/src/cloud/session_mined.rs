//! Session-mined candidate rules.
//!
//! The local worker turns session transcripts into candidate rules:
//!
//! 1. A SessionEnd / Stop / N-turn watermark fires the local worker
//!    (see `difflore-cli/src/session_mine/`).
//! 2. The worker pulls the last few user-prompt / assistant-text pairs
//!    from the platform transcript, strips tool calls + thinking blocks,
//!    and hands them to a small LLM gate (Haiku-class).
//! 3. The gate verdict is either KEEP (a brand-new reusable rule) or
//!    MERGE:<id> (an extension of an existing rule). In either case the
//!    worker enqueues a [`SessionMinedCandidate`] on `cloud_outbox` with
//!    `kind = "session_mined_candidate"`.
//! 4. The cloud clusters those rows into draft `candidate_rules` with
//!    `origin = 'session_mined'` and `requires_human_approval = true`.

use sha2::{Digest, Sha256};

/// Cap on `title` (chars, not bytes). Matches the existing
/// `Observation::title` convention so cloud-side renderers can share
/// truncation logic.
pub const TITLE_MAX_CHARS: usize = 120;

/// Cap on `body` (chars, not bytes). 2 KB is enough for a 3-5 sentence
/// rule body with a snippet; anything longer is almost certainly the
/// raw transcript text the gate failed to compress.
pub const BODY_MAX_CHARS: usize = 2000;

/// Maximum number of file glob patterns we will accept from the gate.
/// 1-3 is the sweet spot: a single broad glob is too noisy, more than
/// three usually means the gate failed to localise the rule.
pub const MAX_FILE_PATTERNS: usize = 3;

/// Stable origin tag for the candidate-rule pipeline.
pub const ORIGIN: &str = "session_mined";

/// Wire format for one session-mined candidate. Serialised verbatim
/// into `cloud_outbox.payload_json` under `kind =
/// "session_mined_candidate"`.
///
/// Every field except `gate_verdict` carries a hard local invariant —
/// see the `validate` method and the constructor builders below.
/// `source_repo` is the load-bearing one: it MUST be derived from the
/// current git remote / cwd and never empty, otherwise the candidate
/// has no Project Scope and the cloud has no way to attribute it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionMinedCandidate {
    /// Platform session id from the hook stdin payload (Claude Code
    /// `session_id`, Cursor session uuid, …). Empty string is *not*
    /// accepted — cloud-side dedup keys on this.
    pub session_id: String,
    /// Unix-ms at the moment the gate produced its verdict.
    pub ts_ms: i64,
    /// `owner/repo` for the GitHub remote, or the cwd basename as a
    /// fallback for non-GitHub repos. Never empty; rejected if the
    /// builder can't determine a repo identity for the current
    /// workspace.
    pub source_repo: String,
    /// Single-line title, ≤ [`TITLE_MAX_CHARS`] chars. The gate is
    /// instructed to write this as a behavioural rule (imperative or
    /// declarative); rendering on the cloud side adds prefixes like
    /// "Remember:" so we don't double them.
    pub title: String,
    /// Multi-sentence rule body, ≤ [`BODY_MAX_CHARS`] chars. Usually
    /// 2-5 sentences plus an optional code snippet.
    pub body: String,
    /// 1-3 file globs, never empty. Cloud-side cascade ordering keys
    /// on these (rules whose patterns match the target file surface
    /// first), so a candidate with zero patterns can never be served.
    pub file_patterns: Vec<String>,
    /// Provider:model identifier for the gate call, e.g.
    /// `"claude:haiku"`. Used for audits + future per-model recall.
    pub gate_model: String,
    /// `"KEEP"` for a brand-new rule, or `"MERGE:<id>"` where `<id>` is
    /// the cloud rule id the gate decided to extend. Cloud applies
    /// the MERGE shape against the named rule's body; KEEP becomes a
    /// fresh `candidate_rules` row.
    pub gate_verdict: String,
    /// 16-char hex sha256 of `source_repo|title|body`. Stable across
    /// retries so the cloud can dedup duplicate outbox uploads of the
    /// same candidate.
    pub content_hash: String,
    /// Origin discriminator on the wire, always [`ORIGIN`].
    pub origin: String,
    /// Draft gate; session-mined candidates require human approval.
    pub requires_human_approval: bool,
}

/// Errors a [`SessionMinedCandidate`] can fail validation with.
/// Returned by the constructor / `validate` so the worker can swallow
/// invalid candidates without retrying through the outbox.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CandidateError {
    /// `source_repo` is empty or whitespace-only. The Project Scope
    /// Invariant says: drop the candidate, don't enqueue a scopeless row.
    #[error("session-mined candidate is missing source_repo — drop")]
    MissingSourceRepo,
    /// `session_id` is empty. Without a session id the cloud cannot
    /// dedup or attribute the candidate.
    #[error("session-mined candidate is missing session_id — drop")]
    MissingSessionId,
    /// `title` empty or longer than [`TITLE_MAX_CHARS`].
    #[error("session-mined candidate title invalid (empty or > {TITLE_MAX_CHARS} chars)")]
    InvalidTitle,
    /// `body` empty or longer than [`BODY_MAX_CHARS`].
    #[error("session-mined candidate body invalid (empty or > {BODY_MAX_CHARS} chars)")]
    InvalidBody,
    /// `file_patterns` empty or > [`MAX_FILE_PATTERNS`].
    #[error("session-mined candidate must carry 1-{MAX_FILE_PATTERNS} file patterns")]
    InvalidFilePatterns,
    /// `gate_model` empty.
    #[error("session-mined candidate is missing gate_model")]
    MissingGateModel,
    /// `gate_verdict` empty / not `"KEEP"` / not `"MERGE:<id>"`.
    #[error("session-mined candidate gate_verdict must be 'KEEP' or 'MERGE:<id>'")]
    InvalidGateVerdict,
    /// `requires_human_approval` was set to `false`.
    #[error("session-mined candidates must keep requires_human_approval = true")]
    NotDraft,
    /// `origin` is anything other than [`ORIGIN`].
    #[error("session-mined candidate has wrong origin (expected {ORIGIN})")]
    WrongOrigin,
}

impl SessionMinedCandidate {
    /// Build a new candidate from gate output. Truncates title/body to
    /// the documented caps, derives the content hash, and pins the
    /// origin and draft flag. Callers must drop invalid candidates.
    pub fn try_new(args: SessionMinedCandidateArgs) -> Result<Self, CandidateError> {
        let SessionMinedCandidateArgs {
            session_id,
            ts_ms,
            source_repo,
            title,
            body,
            file_patterns,
            gate_model,
            gate_verdict,
        } = args;

        let session_id = session_id.trim().to_owned();
        if session_id.is_empty() {
            return Err(CandidateError::MissingSessionId);
        }
        let source_repo = source_repo.trim().to_owned();
        if source_repo.is_empty() {
            return Err(CandidateError::MissingSourceRepo);
        }
        let title = truncate_chars(title.trim(), TITLE_MAX_CHARS);
        if title.is_empty() {
            return Err(CandidateError::InvalidTitle);
        }
        let body = truncate_chars(body.trim(), BODY_MAX_CHARS);
        if body.is_empty() {
            return Err(CandidateError::InvalidBody);
        }
        let file_patterns: Vec<String> = file_patterns
            .into_iter()
            .map(|p| p.trim().to_owned())
            .filter(|p| !p.is_empty())
            .take(MAX_FILE_PATTERNS)
            .collect();
        if file_patterns.is_empty() {
            return Err(CandidateError::InvalidFilePatterns);
        }
        let gate_model = gate_model.trim().to_owned();
        if gate_model.is_empty() {
            return Err(CandidateError::MissingGateModel);
        }
        let gate_verdict = gate_verdict.trim().to_owned();
        if !is_valid_verdict(&gate_verdict) {
            return Err(CandidateError::InvalidGateVerdict);
        }

        let content_hash = compute_content_hash(&source_repo, &title, &body);

        Ok(Self {
            session_id,
            ts_ms,
            source_repo,
            title,
            body,
            file_patterns,
            gate_model,
            gate_verdict,
            content_hash,
            origin: ORIGIN.to_owned(),
            requires_human_approval: true,
        })
    }

    /// Re-validate an existing candidate. Used by the outbox
    /// dispatcher before posting so a corrupted row (e.g. tampered
    /// payload, wrong-origin row migrated in) never reaches the
    /// cloud endpoint.
    pub fn validate(&self) -> Result<(), CandidateError> {
        if self.session_id.trim().is_empty() {
            return Err(CandidateError::MissingSessionId);
        }
        if self.source_repo.trim().is_empty() {
            return Err(CandidateError::MissingSourceRepo);
        }
        if self.title.is_empty() || self.title.chars().count() > TITLE_MAX_CHARS {
            return Err(CandidateError::InvalidTitle);
        }
        if self.body.is_empty() || self.body.chars().count() > BODY_MAX_CHARS {
            return Err(CandidateError::InvalidBody);
        }
        if self.file_patterns.is_empty() || self.file_patterns.len() > MAX_FILE_PATTERNS {
            return Err(CandidateError::InvalidFilePatterns);
        }
        if self.gate_model.trim().is_empty() {
            return Err(CandidateError::MissingGateModel);
        }
        if !is_valid_verdict(&self.gate_verdict) {
            return Err(CandidateError::InvalidGateVerdict);
        }
        if self.origin != ORIGIN {
            return Err(CandidateError::WrongOrigin);
        }
        if !self.requires_human_approval {
            return Err(CandidateError::NotDraft);
        }
        Ok(())
    }
}

/// Builder bundle accepted by [`SessionMinedCandidate::try_new`]. Kept
/// as a struct (rather than a long argument list) so future fields
/// can be added without breaking call sites.
#[derive(Debug, Clone)]
pub struct SessionMinedCandidateArgs {
    pub session_id: String,
    pub ts_ms: i64,
    pub source_repo: String,
    pub title: String,
    pub body: String,
    pub file_patterns: Vec<String>,
    pub gate_model: String,
    pub gate_verdict: String,
}

fn is_valid_verdict(verdict: &str) -> bool {
    if verdict == "KEEP" {
        return true;
    }
    if let Some(rest) = verdict.strip_prefix("MERGE:") {
        return !rest.trim().is_empty();
    }
    false
}

/// `sha256(source_repo|title|body)[:16]` as lowercase hex. Mirrors
/// the 16-char convention used by `Observation::content_hash` and
/// `remember_rule` so cloud-side dedup logic doesn't need a separate
/// hash family.
pub fn compute_content_hash(source_repo: &str, title: &str, body: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(source_repo.as_bytes());
    hasher.update(b"|");
    hasher.update(title.as_bytes());
    hasher.update(b"|");
    hasher.update(body.as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> SessionMinedCandidateArgs {
        SessionMinedCandidateArgs {
            session_id: "sess_test".to_owned(),
            ts_ms: 1_714_000_000_000,
            source_repo: "owner/repo".to_owned(),
            title: "Prefer typed deserialization over Value::as_str".to_owned(),
            body: "When parsing oRPC payloads, deserialize into a concrete struct \
                   instead of walking serde_json::Value with as_str()."
                .to_owned(),
            file_patterns: vec!["src/**/*.rs".to_owned()],
            gate_model: "claude:haiku".to_owned(),
            gate_verdict: "KEEP".to_owned(),
        }
    }

    #[test]
    fn try_new_sets_origin_and_draft_flag_unconditionally() {
        let cand = SessionMinedCandidate::try_new(args()).expect("valid");
        assert_eq!(cand.origin, ORIGIN);
        assert!(
            cand.requires_human_approval,
            "session-mined candidates must default to draft"
        );
    }

    #[test]
    fn try_new_rejects_missing_source_repo() {
        // Scopeless candidates are dropped, never enqueued.
        let mut a = args();
        a.source_repo = String::new();
        let err = SessionMinedCandidate::try_new(a).unwrap_err();
        assert_eq!(err, CandidateError::MissingSourceRepo);
    }

    #[test]
    fn try_new_rejects_missing_session_id() {
        let mut a = args();
        a.session_id = "   ".to_owned();
        let err = SessionMinedCandidate::try_new(a).unwrap_err();
        assert_eq!(err, CandidateError::MissingSessionId);
    }

    #[test]
    fn try_new_rejects_empty_file_patterns() {
        let mut a = args();
        a.file_patterns = vec![];
        let err = SessionMinedCandidate::try_new(a).unwrap_err();
        assert_eq!(err, CandidateError::InvalidFilePatterns);

        let mut a = args();
        a.file_patterns = vec!["   ".to_owned(), String::new()];
        let err = SessionMinedCandidate::try_new(a).unwrap_err();
        assert_eq!(err, CandidateError::InvalidFilePatterns);
    }

    #[test]
    fn try_new_caps_file_patterns_at_three() {
        let mut a = args();
        a.file_patterns = vec![
            "a.rs".to_owned(),
            "b.rs".to_owned(),
            "c.rs".to_owned(),
            "d.rs".to_owned(),
        ];
        let cand = SessionMinedCandidate::try_new(a).expect("valid");
        assert_eq!(cand.file_patterns.len(), MAX_FILE_PATTERNS);
    }

    #[test]
    fn try_new_validates_verdict_shape() {
        for bad in ["", "MERGE:", "merge:abc", "REJECT", "merge"] {
            let mut a = args();
            a.gate_verdict = bad.to_owned();
            let err = SessionMinedCandidate::try_new(a).unwrap_err();
            assert_eq!(err, CandidateError::InvalidGateVerdict, "verdict='{bad}'");
        }
        for ok in ["KEEP", "MERGE:rule-123"] {
            let mut a = args();
            a.gate_verdict = ok.to_owned();
            SessionMinedCandidate::try_new(a).expect("verdict must be accepted");
        }
    }

    #[test]
    fn try_new_truncates_oversize_title_and_body() {
        let long: String = "x".repeat(TITLE_MAX_CHARS + 50);
        let big: String = "y".repeat(BODY_MAX_CHARS + 100);
        let mut a = args();
        a.title.clone_from(&long);
        a.body.clone_from(&big);
        let cand = SessionMinedCandidate::try_new(a).expect("valid");
        assert!(cand.title.chars().count() <= TITLE_MAX_CHARS);
        assert!(cand.body.chars().count() <= BODY_MAX_CHARS);
    }

    #[test]
    fn content_hash_is_stable_and_input_sensitive() {
        let a = SessionMinedCandidate::try_new(args()).unwrap();
        let b = SessionMinedCandidate::try_new(args()).unwrap();
        assert_eq!(a.content_hash, b.content_hash);
        assert_eq!(a.content_hash.len(), 16);

        let mut other = args();
        other.title = "Different rule".to_owned();
        let c = SessionMinedCandidate::try_new(other).unwrap();
        assert_ne!(a.content_hash, c.content_hash);
    }

    #[test]
    fn validate_rejects_tampered_origin_or_unpublished_off() {
        let cand = SessionMinedCandidate::try_new(args()).unwrap();
        cand.validate().unwrap();

        let mut tampered = cand.clone();
        tampered.origin = "remember_rule".to_owned();
        assert_eq!(
            tampered.validate().unwrap_err(),
            CandidateError::WrongOrigin
        );

        let mut leaked = cand;
        leaked.requires_human_approval = false;
        assert_eq!(leaked.validate().unwrap_err(), CandidateError::NotDraft);
    }

    #[test]
    fn wire_shape_serializes_with_snake_case_keys() {
        // Lock the snake_case wire contract used by the cloud endpoint.
        let cand = SessionMinedCandidate::try_new(args()).unwrap();
        let value = serde_json::to_value(&cand).expect("serialize");
        for required in [
            "session_id",
            "ts_ms",
            "source_repo",
            "title",
            "body",
            "file_patterns",
            "gate_model",
            "gate_verdict",
            "content_hash",
            "origin",
            "requires_human_approval",
        ] {
            assert!(value.get(required).is_some(), "missing field: {required}");
        }
        assert_eq!(value["requires_human_approval"], true);
        assert_eq!(value["origin"], ORIGIN);
    }
}
