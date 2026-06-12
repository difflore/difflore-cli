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

use crate::infra::git::RepoScope;

/// Cap on `title`, in chars not bytes. Matches `Observation::title` so
/// cloud-side renderers share truncation logic.
pub const TITLE_MAX_CHARS: usize = 120;

/// Cap on `body`, in chars not bytes. 2 KB fits a 3-5 sentence body with a
/// snippet; longer almost always means the gate failed to compress the
/// transcript.
pub const BODY_MAX_CHARS: usize = 2000;

/// Maximum file globs accepted from the gate. More than 3 usually means
/// the gate failed to localise the rule.
pub const MAX_FILE_PATTERNS: usize = 3;

pub const ORIGIN: &str = "session_mined";

/// Wire format for one session-mined candidate, serialised into
/// `cloud_outbox.payload_json` under `kind = "session_mined_candidate"`.
/// Every field except `gate_verdict` carries a local invariant enforced by
/// `validate` and the constructor.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionMinedCandidate {
    /// Platform session id from the hook payload. Never empty — cloud-side
    /// dedup keys on this.
    pub session_id: String,
    /// Unix-ms when the gate produced its verdict.
    pub ts_ms: i64,
    /// Canonical repo scope (`owner/repo` for GitHub, `host/group/project` for
    /// GitLab). Never empty, otherwise the candidate has no Project Scope and
    /// the cloud cannot attribute it. The wire field stays a bare string for
    /// payload compatibility, but the only way to populate it is via
    /// [`SessionMinedCandidate::try_new`], which requires a [`RepoScope`] — so
    /// this column shares the single canonicalization gate every other
    /// `source_repo` write goes through (the structural fix for the recurring
    /// host-dimension scope bug).
    pub source_repo: String,
    /// Single-line title, ≤ [`TITLE_MAX_CHARS`] chars. Written as a bare
    /// behavioural rule; the cloud adds prefixes like "Remember:".
    pub title: String,
    /// Rule body, ≤ [`BODY_MAX_CHARS`] chars.
    pub body: String,
    /// 1-3 file globs, never empty. Cloud cascade ordering keys on these,
    /// so a patternless candidate can never be served.
    pub file_patterns: Vec<String>,
    /// Provider:model for the gate call, e.g. `"claude:haiku"`.
    pub gate_model: String,
    /// `"KEEP"` for a new rule, or `"MERGE:<id>"` to extend the named cloud
    /// rule's body.
    pub gate_verdict: String,
    /// 16-char hex sha256 of `source_repo|title|body`. Stable across
    /// retries so the cloud can dedup duplicate uploads.
    pub content_hash: String,
    /// Wire discriminator, always [`ORIGIN`].
    pub origin: String,
    pub requires_human_approval: bool,
}

/// Validation failures for a [`SessionMinedCandidate`]. Returned by the
/// constructor and `validate` so the worker can drop invalid candidates
/// without retrying through the outbox.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CandidateError {
    #[error("session-mined candidate is missing source_repo — drop")]
    MissingSourceRepo,
    #[error("session-mined candidate is missing session_id — drop")]
    MissingSessionId,
    #[error("session-mined candidate title invalid (empty or > {TITLE_MAX_CHARS} chars)")]
    InvalidTitle,
    #[error("session-mined candidate body invalid (empty or > {BODY_MAX_CHARS} chars)")]
    InvalidBody,
    #[error("session-mined candidate must carry 1-{MAX_FILE_PATTERNS} file patterns")]
    InvalidFilePatterns,
    #[error("session-mined candidate is missing gate_model")]
    MissingGateModel,
    #[error("session-mined candidate gate_verdict must be 'KEEP' or 'MERGE:<id>'")]
    InvalidGateVerdict,
    #[error("session-mined candidates must keep requires_human_approval = true")]
    NotDraft,
    #[error("session-mined candidate has wrong origin (expected {ORIGIN})")]
    WrongOrigin,
}

impl SessionMinedCandidate {
    /// Build a candidate from gate output: truncates title/body to the
    /// caps, derives the content hash, and pins origin + draft flag.
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
        // `source_repo` arrives as a `RepoScope`, so the canonical/non-empty
        // invariant is already enforced by the newtype's constructor — there is
        // no path to build a candidate from a raw, unnormalized String. We keep
        // a defensive empty check for symmetry with `validate`, which also runs
        // on candidates rehydrated from the outbox wire format.
        let source_repo = source_repo.into_string();
        if source_repo.trim().is_empty() {
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

    /// Re-validate before posting so a corrupted or wrong-origin row never
    /// reaches the cloud endpoint.
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

/// Builder bundle accepted by [`SessionMinedCandidate::try_new`].
///
/// `source_repo` is a [`RepoScope`], not a raw `String`: the session-mined
/// candidate is one of the five `skills.source_repo` write entry points, and
/// routing it through the newtype makes `RepoScope` the single normalization
/// gate for every one of them. A caller cannot construct a candidate from an
/// unnormalized remote string.
#[derive(Debug, Clone)]
pub struct SessionMinedCandidateArgs {
    pub session_id: String,
    pub ts_ms: i64,
    pub source_repo: RepoScope,
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

/// `sha256(source_repo|title|body)[:16]` as lowercase hex. Mirrors the
/// 16-char convention of `Observation::content_hash` and `remember_rule`
/// so cloud-side dedup shares one hash family.
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
            source_repo: RepoScope::canonical("owner/repo").expect("canonical scope"),
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
    fn source_repo_cannot_be_constructed_from_empty_or_noncanonical_string() {
        // The `MissingSourceRepo` gap is now structurally unreachable: the
        // candidate's `source_repo` is a `RepoScope`, and `RepoScope::canonical`
        // refuses empty / unnormalized input, so there is no way to even build
        // the args for a scopeless candidate.
        assert!(RepoScope::canonical("").is_none());
        assert!(RepoScope::canonical("   ").is_none());
        assert!(RepoScope::canonical("not a repo").is_none());
        // A valid canonical scope still round-trips into the candidate.
        let cand = SessionMinedCandidate::try_new(args()).expect("valid");
        assert_eq!(cand.source_repo, "owner/repo");
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
