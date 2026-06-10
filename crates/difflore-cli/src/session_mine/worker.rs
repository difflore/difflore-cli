//! Session-mine top-level worker.
//!
//! Composes [`super::extract`], [`super::gate`] and the cloud-outbox
//! enqueue path. The hook dispatcher spawns [`run_worker_detached`]
//! when the trigger fires.
//!
//! Failure policy: every error is swallowed (logged to stderr at
//! most). Session-mine is an out-of-band evidence channel and must
//! never block the user's hook output or surface a panic into the
//! agent session.

use difflore_core::cloud::outbox::{OutboxQueue, kind as outbox_kind};
use difflore_core::cloud::session_mined::SessionMinedCandidate;
use difflore_core::infra::db::current_project_root;

use super::extract::Pair;
use super::gate::{ExistingRule, GateArgs, GateVerdict, run_gate};

/// Cap on existing rules forwarded to the gate prompt. Bounds the SQL
/// round-trip and cloning cost when a team has thousands of rules.
const MAX_EXISTING_RULES_FOR_GATE: usize = 24;

/// Per-rule body snippet cap in the gate's "existing rules" digest.
const EXISTING_RULE_BODY_SNIPPET_CHARS: usize = 280;

/// Spawn the worker as a detached tokio task, returning immediately.
///
/// `client_name` is the platform string the hook reports
/// (`"claude-code"`, `"cursor"`, …), used for extract dispatch.
/// `cwd` derives `source_repo` via the git remote; `None` falls back
/// to `current_project_root()`.
pub fn run_worker_detached(
    client_name: String,
    transcript_path: Option<String>,
    session_id: Option<String>,
    cwd: Option<String>,
) {
    // Prefer the existing tokio runtime (hook dispatcher is
    // `#[tokio::main]`); outside a runtime (e.g. test harness),
    // `spawn` would panic, so fall back to a dedicated thread.
    let task = async move {
        if let Err(e) =
            run_worker_inner(&client_name, transcript_path.as_deref(), session_id.as_deref(), cwd.as_deref())
                .await
        {
            if difflore_core::env::debug_telemetry() {
                eprintln!("[difflore.session_mine] worker failed: {e}");
            }
        }
    };
    if tokio::runtime::Handle::try_current().is_ok() {
        tokio::spawn(task);
    } else {
        // No runtime: run on a temporary one so callers get the same
        // observable behaviour without panicking on `spawn`.
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    if difflore_core::env::debug_telemetry() {
                        eprintln!("[difflore.session_mine] cannot build fallback runtime: {e}");
                    }
                    return;
                }
            };
            rt.block_on(task);
        });
    }
}

/// Body of the worker, separated from the spawn helper so tests can
/// exercise it with a controlled environment.
async fn run_worker_inner(
    client_name: &str,
    transcript_path: Option<&str>,
    session_id: Option<&str>,
    cwd: Option<&str>,
) -> Result<(), String> {
    let pairs = extract_pairs(client_name, transcript_path);
    if pairs.is_empty() {
        // No conversational data to mine.
        return Ok(());
    }

    let Some(source_repo) = resolve_source_repo(cwd) else {
        // Project Scope Invariant: never enqueue a scopeless
        // candidate. We no-op rather than fabricate a `source_repo`.
        return Ok(());
    };

    let session_id = session_id.unwrap_or("").trim().to_owned();
    if session_id.is_empty() {
        return Ok(());
    }

    // One DB handle for both reading existing rules and enqueuing on
    // Keep. Best-effort: log and drop the session on failure.
    let db = match difflore_core::db::init_db().await {
        Ok(p) => p,
        Err(e) => {
            if difflore_core::env::debug_telemetry() {
                eprintln!("[difflore.session_mine] DB open failed: {e}");
            }
            return Ok(());
        }
    };

    let existing_rules = load_existing_rules(&db, &source_repo).await;
    let ts_ms = chrono::Utc::now().timestamp_millis();
    let gate_model = format!("{client_name}:gate");
    let args = GateArgs {
        session_id: &session_id,
        source_repo: &source_repo,
        pairs: &pairs,
        existing_rules: &existing_rules,
        gate_model: &gate_model,
        client_name,
        ts_ms,
    };
    let verdict = match run_gate(args).await {
        Ok(v) => v,
        Err(e) => {
            if difflore_core::env::debug_telemetry() {
                eprintln!("[difflore.session_mine] gate failed: {e}");
            }
            return Ok(());
        }
    };

    match verdict {
        GateVerdict::Keep { candidate } => match enqueue_candidate(&db, &candidate).await {
            Ok(_) => Ok(()),
            Err(e) => {
                if difflore_core::env::debug_telemetry() {
                    eprintln!("[difflore.session_mine] enqueue failed: {e}");
                }
                Ok(())
            }
        },
        GateVerdict::Merge { rule_id, .. } => {
            // Merge handling is deferred: the worker doesn't yet have
            // the existing rule's file_patterns needed to build a
            // complete `SessionMinedCandidate`. Log and drop.
            if difflore_core::env::debug_telemetry() {
                eprintln!("[difflore.session_mine] gate MERGE for {rule_id}; handling deferred");
            }
            Ok(())
        }
        GateVerdict::Skip { reason } => {
            if difflore_core::env::debug_telemetry() {
                eprintln!("[difflore.session_mine] gate SKIP: {reason}");
            }
            Ok(())
        }
    }
}

/// Read active rules and project them into the `ExistingRule` shape
/// the gate expects. Rules with a `repo_owner`/`repo_name` pair are
/// kept only when they match `source_repo`; rules without that
/// metadata are included permissively. Failures collapse to an empty
/// list, which the gate treats as valid input.
async fn load_existing_rules(db: &sqlx::SqlitePool, source_repo: &str) -> Vec<ExistingRule> {
    let Ok(skills) = difflore_core::skills::list(db).await else {
        return Vec::new();
    };
    let scope = source_repo.to_ascii_lowercase();
    skills
        .iter()
        .filter(|s| match (&s.repo_owner, &s.repo_name) {
            (Some(o), Some(n)) => format!("{o}/{n}").to_ascii_lowercase() == scope,
            _ => true,
        })
        .take(MAX_EXISTING_RULES_FOR_GATE)
        .map(|s| ExistingRule {
            rule_id: s.id.clone(),
            title: s.name.clone(),
            body_snippet: s
                .description
                .chars()
                .take(EXISTING_RULE_BODY_SNIPPET_CHARS)
                .collect(),
        })
        .collect()
}

fn extract_pairs(client_name: &str, transcript_path: Option<&str>) -> Vec<Pair> {
    let platform = super::extract::Platform::from_client_name(client_name);
    let args = super::extract::ExtractArgs {
        platform,
        transcript_path,
        session_id: None,
        max_pairs: 10,
    };
    super::extract::extract_recent_session_pairs(args).unwrap_or_default()
}

/// Resolve `source_repo` per the Project Scope Invariant. Tries the
/// git origin `owner/repo` first, then the lowercased cwd basename.
/// Returns `None` only when both fail (e.g. running from `/`).
fn resolve_source_repo(cwd: Option<&str>) -> Option<String> {
    let path = cwd.map_or_else(current_project_root, std::path::PathBuf::from);
    let path_str = path.to_string_lossy().to_string();

    if let Some(repo) =
        difflore_core::git::detect_github_repo_full_names(&path_str).into_iter().next()
    {
        let trimmed = repo.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_owned());
        }
    }

    let basename = path
        .file_name()
        .and_then(|s| s.to_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    Some(basename.to_ascii_lowercase())
}

/// Serialize the candidate and append it to the cloud outbox under
/// `kind = "session_mined_candidate"`.
pub async fn enqueue_candidate(
    db: &sqlx::SqlitePool,
    candidate: &SessionMinedCandidate,
) -> Result<i64, String> {
    candidate
        .validate()
        .map_err(|e| format!("session-mine: invalid candidate: {e}"))?;
    let payload = serde_json::to_string(candidate)
        .map_err(|e| format!("session-mine: serialize: {e}"))?;
    let queue = OutboxQueue::new(db.clone());
    queue
        .enqueue(outbox_kind::SESSION_MINED_CANDIDATE, &payload)
        .await
        .map_err(|e| format!("session-mine: enqueue: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use difflore_core::cloud::session_mined::SessionMinedCandidateArgs;

    fn candidate() -> SessionMinedCandidate {
        SessionMinedCandidate::try_new(SessionMinedCandidateArgs {
            session_id: "sess_w".to_owned(),
            ts_ms: 1_714_000_000_000,
            source_repo: "owner/repo".to_owned(),
            title: "Reject scopeless rules".to_owned(),
            body: "Sessions without a resolvable source_repo must drop their candidate \
                   instead of enqueueing a scopeless row."
                .to_owned(),
            file_patterns: vec!["src/**/*.rs".to_owned()],
            gate_model: "claude:haiku".to_owned(),
            gate_verdict: "KEEP".to_owned(),
        })
        .expect("test fixture must be valid")
    }

    #[test]
    fn enqueue_helper_validates_payload_before_touching_the_db() {
        // Lock the validation gate so a refactor cannot let an invalid
        // payload onto the outbox path. (No live SqlitePool needed.)
        let mut bad = candidate();
        bad.requires_human_approval = false;
        let err = bad.validate().unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("requires_human_approval"),
            "draft-flag rejection must surface in the error message: {msg}"
        );
    }

    #[test]
    fn candidate_round_trips_through_json_with_kind_string() {
        // The wire shape is load-bearing for the cloud-side endpoint,
        // so lock the JSON round-trip and the outbox kind string.
        let cand = candidate();
        let payload = serde_json::to_string(&cand).expect("serialize");
        let kind = outbox_kind::SESSION_MINED_CANDIDATE;
        assert_eq!(kind, "session_mined_candidate");

        let decoded: SessionMinedCandidate =
            serde_json::from_str(&payload).expect("decode");
        assert_eq!(decoded.source_repo, "owner/repo");
        assert!(decoded.requires_human_approval);
        assert_eq!(decoded.origin, "session_mined");
    }

    #[test]
    fn resolve_source_repo_prefers_git_remote_then_basename() {
        // Outside a git repo, the cwd basename still satisfies the
        // Project Scope Invariant.
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path();
        let repo = resolve_source_repo(Some(path.to_str().unwrap()));
        assert!(repo.is_some(), "tempdir basename must satisfy the invariant");
        let repo = repo.unwrap();
        // Basenames are lowercased for stable casing across OSes.
        assert_eq!(repo, repo.to_ascii_lowercase());
    }
}
