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
use difflore_core::cloud::session_mined::{SessionMinedCandidate, SessionMinedCandidateArgs};
use difflore_core::infra::db::current_project_root;
use sqlx::Row;

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
        if let Err(e) = run_worker_inner(
            &client_name,
            transcript_path.as_deref(),
            session_id.as_deref(),
            cwd.as_deref(),
        )
        .await
        {
            if difflore_core::infra::env::debug_telemetry() {
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
                    if difflore_core::infra::env::debug_telemetry() {
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
    let db = match difflore_core::infra::db::init_db().await {
        Ok(p) => p,
        Err(e) => {
            if difflore_core::infra::env::debug_telemetry() {
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
            if difflore_core::infra::env::debug_telemetry() {
                eprintln!("[difflore.session_mine] gate failed: {e}");
            }
            return Ok(());
        }
    };

    match verdict {
        GateVerdict::Keep { candidate } => match enqueue_candidate(&db, &candidate).await {
            Ok(_) => Ok(()),
            Err(e) => {
                if difflore_core::infra::env::debug_telemetry() {
                    eprintln!("[difflore.session_mine] enqueue failed: {e}");
                }
                Ok(())
            }
        },
        GateVerdict::Merge {
            rule_id,
            title,
            updated_body,
            file_patterns,
        } => {
            let candidate = match merge_candidate_from_verdict(
                &session_id,
                ts_ms,
                &source_repo,
                &gate_model,
                &existing_rules,
                &rule_id,
                title.as_deref(),
                &updated_body,
                &file_patterns,
            ) {
                Ok(candidate) => candidate,
                Err(e) => {
                    if difflore_core::infra::env::debug_telemetry() {
                        eprintln!("[difflore.session_mine] MERGE candidate build failed: {e}");
                    }
                    return Ok(());
                }
            };
            match enqueue_candidate(&db, &candidate).await {
                Ok(_) => Ok(()),
                Err(e) => {
                    if difflore_core::infra::env::debug_telemetry() {
                        eprintln!("[difflore.session_mine] enqueue failed: {e}");
                    }
                    Ok(())
                }
            }
        }
        GateVerdict::Skip { reason } => {
            if difflore_core::infra::env::debug_telemetry() {
                eprintln!("[difflore.session_mine] gate SKIP: {reason}");
            }
            Ok(())
        }
    }
}

/// Read active rules and project them into the `ExistingRule` shape
/// the gate expects. Rules with `source_repo` or a `repo_owner`/`repo_name`
/// pair are kept only when they match `source_repo`; rules without that
/// metadata are included permissively. Failures collapse to an empty list,
/// which the gate treats as valid input.
async fn load_existing_rules(db: &sqlx::SqlitePool, source_repo: &str) -> Vec<ExistingRule> {
    let Ok(rows) = sqlx::query(
        "SELECT COALESCE(NULLIF(cloud_id, ''), id) AS rule_id,
                id, name, description, repo_owner, repo_name, source_repo, file_patterns \
         FROM skills WHERE status = 'active' ORDER BY installed_at DESC",
    )
    .fetch_all(db)
    .await
    else {
        return Vec::new();
    };
    let scope = source_repo.to_ascii_lowercase();
    rows.into_iter()
        .filter_map(|row| {
            let rule_id: String = row.try_get("rule_id").ok()?;
            if !looks_like_cloud_rule_id(&rule_id) {
                // Session-mined candidates are approved in the cloud, where a
                // MERGE target must be a published cloud rule UUID. Local
                // rules published from this device keep their local `skills.id`
                // but carry the cloud UUID in `skills.cloud_id`; local-only
                // rows have neither and must not be exposed as merge targets.
                return None;
            }
            let title: String = row.try_get("name").ok()?;
            let description: String = row.try_get("description").unwrap_or_default();
            let repo_owner: Option<String> = row.try_get("repo_owner").ok().flatten();
            let repo_name: Option<String> = row.try_get("repo_name").ok().flatten();
            let source_repo_col: Option<String> = row.try_get("source_repo").ok().flatten();

            if !rule_matches_source_repo(
                repo_owner.as_deref(),
                repo_name.as_deref(),
                source_repo_col.as_deref(),
                &scope,
            ) {
                return None;
            }

            let file_patterns_raw: Option<String> = row.try_get("file_patterns").ok().flatten();
            Some(ExistingRule {
                rule_id,
                title,
                body_snippet: description
                    .chars()
                    .take(EXISTING_RULE_BODY_SNIPPET_CHARS)
                    .collect(),
                file_patterns: parse_file_patterns(file_patterns_raw.as_deref()),
                source_repo: clean_optional(source_repo_col),
            })
        })
        .take(MAX_EXISTING_RULES_FOR_GATE)
        .collect()
}

fn merge_candidate_from_verdict(
    session_id: &str,
    ts_ms: i64,
    source_repo: &str,
    gate_model: &str,
    existing_rules: &[ExistingRule],
    rule_id: &str,
    gate_title: Option<&str>,
    updated_body: &str,
    mined_file_patterns: &[String],
) -> Result<SessionMinedCandidate, String> {
    let target = existing_rules.iter().find(|rule| rule.rule_id == rule_id);
    let title = target
        .and_then(|rule| non_empty_owned(&rule.title))
        .or_else(|| gate_title.and_then(non_empty_owned))
        .ok_or_else(|| format!("MERGE:{rule_id} missing title"))?;
    let candidate_source_repo = target
        .and_then(|rule| rule.source_repo.as_deref())
        .and_then(non_empty_owned)
        .unwrap_or_else(|| source_repo.to_owned());
    let file_patterns = merge_file_patterns(mined_file_patterns, target);

    SessionMinedCandidate::try_new(SessionMinedCandidateArgs {
        session_id: session_id.to_owned(),
        ts_ms,
        source_repo: candidate_source_repo,
        title,
        body: updated_body.to_owned(),
        file_patterns,
        gate_model: gate_model.to_owned(),
        gate_verdict: format!("MERGE:{rule_id}"),
    })
    .map_err(|e| format!("MERGE:{rule_id} invalid candidate: {e}"))
}

fn merge_file_patterns(
    mined_file_patterns: &[String],
    target: Option<&ExistingRule>,
) -> Vec<String> {
    let mut out = Vec::new();
    push_unique_patterns(&mut out, mined_file_patterns.iter().map(String::as_str));
    if let Some(rule) = target {
        push_unique_patterns(&mut out, rule.file_patterns.iter().map(String::as_str));
    }
    out
}

fn push_unique_patterns<'a>(out: &mut Vec<String>, patterns: impl Iterator<Item = &'a str>) {
    for pattern in patterns {
        let pattern = pattern.trim();
        if pattern.is_empty() || out.iter().any(|existing| existing == pattern) {
            continue;
        }
        out.push(pattern.to_owned());
    }
}

fn rule_matches_source_repo(
    repo_owner: Option<&str>,
    repo_name: Option<&str>,
    source_repo: Option<&str>,
    scope_lc: &str,
) -> bool {
    let repo_pair_matches = match (repo_owner, repo_name) {
        (Some(owner), Some(name)) => format!("{owner}/{name}").to_ascii_lowercase() == scope_lc,
        _ => false,
    };
    let has_repo_pair = repo_owner.is_some() && repo_name.is_some();
    let source_repo_matches = source_repo
        .map(str::trim)
        .filter(|repo| !repo.is_empty())
        .is_some_and(|repo| repo.to_ascii_lowercase() == scope_lc);
    let has_source_repo = source_repo
        .map(str::trim)
        .is_some_and(|repo| !repo.is_empty());

    if has_repo_pair || has_source_repo {
        repo_pair_matches || source_repo_matches
    } else {
        true
    }
}

fn parse_file_patterns(raw: Option<&str>) -> Vec<String> {
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<String>>(raw)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|pattern| non_empty_owned(&pattern))
        .collect()
}

fn clean_optional(value: Option<String>) -> Option<String> {
    value.and_then(|value| non_empty_owned(&value))
}

fn looks_like_cloud_rule_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    for (idx, byte) in bytes.iter().enumerate() {
        if matches!(idx, 8 | 13 | 18 | 23) {
            if *byte != b'-' {
                return false;
            }
            continue;
        }
        if !byte.is_ascii_hexdigit() {
            return false;
        }
    }
    true
}

fn non_empty_owned(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_owned())
    }
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

    if let Some(repo) = difflore_core::infra::git::detect_github_repo_full_names(&path_str)
        .into_iter()
        .next()
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
    let payload =
        serde_json::to_string(candidate).map_err(|e| format!("session-mine: serialize: {e}"))?;
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

    fn existing_rule() -> ExistingRule {
        ExistingRule {
            rule_id: "11111111-1111-4111-8111-111111111111".to_owned(),
            title: "Preserve async cleanup".to_owned(),
            body_snippet: "Existing body".to_owned(),
            file_patterns: vec!["crates/difflore-cli/src/**/*.rs".to_owned()],
            source_repo: Some("upstream/repo".to_owned()),
        }
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

        let decoded: SessionMinedCandidate = serde_json::from_str(&payload).expect("decode");
        assert_eq!(decoded.source_repo, "owner/repo");
        assert!(decoded.requires_human_approval);
        assert_eq!(decoded.origin, "session_mined");
    }

    #[test]
    fn merge_candidate_uses_mined_file_evidence_and_target_source_repo() {
        let existing = vec![existing_rule()];
        let mined = vec!["crates/difflore-cli/src/session_mine/worker.rs".to_owned()];

        let candidate = merge_candidate_from_verdict(
            "sess_merge",
            1_714_000_000_000,
            "local/repo",
            "claude-code:gate",
            &existing,
            "11111111-1111-4111-8111-111111111111",
            Some("Gate title"),
            "Merged body the cloud should apply.",
            &mined,
        )
        .expect("valid merge candidate");

        assert_eq!(
            candidate.gate_verdict,
            "MERGE:11111111-1111-4111-8111-111111111111"
        );
        assert_eq!(candidate.source_repo, "upstream/repo");
        assert_eq!(candidate.title, "Preserve async cleanup");
        assert_eq!(candidate.body, "Merged body the cloud should apply.");
        assert_eq!(
            candidate.file_patterns,
            vec![
                "crates/difflore-cli/src/session_mine/worker.rs",
                "crates/difflore-cli/src/**/*.rs"
            ]
        );
        assert!(candidate.requires_human_approval);
    }

    #[test]
    fn merge_candidate_falls_back_to_target_file_patterns_when_gate_omits_scope() {
        let existing = vec![existing_rule()];

        let candidate = merge_candidate_from_verdict(
            "sess_merge",
            1_714_000_000_000,
            "local/repo",
            "claude-code:gate",
            &existing,
            "11111111-1111-4111-8111-111111111111",
            None,
            "Merged body.",
            &[],
        )
        .expect("target file_patterns keep merge candidate scoped");

        assert_eq!(
            candidate.file_patterns,
            vec!["crates/difflore-cli/src/**/*.rs"]
        );
        assert_eq!(
            candidate.gate_verdict,
            "MERGE:11111111-1111-4111-8111-111111111111"
        );
    }

    #[tokio::test]
    async fn load_existing_rules_carries_scope_metadata() {
        let db = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect(":memory:")
            .await
            .expect("memory db");
        sqlx::query(
            "CREATE TABLE skills (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                description TEXT NOT NULL,
                repo_owner TEXT,
                repo_name TEXT,
                cloud_id TEXT,
                source_repo TEXT,
                file_patterns TEXT,
                status TEXT NOT NULL,
                installed_at TEXT NOT NULL
            )",
        )
        .execute(&db)
        .await
        .expect("schema");
        sqlx::query(
            "INSERT INTO skills
             (id, name, description, repo_owner, repo_name, cloud_id, source_repo, file_patterns, status, installed_at)
             VALUES
             ('11111111-1111-4111-8111-111111111111', 'Scoped', 'Body', NULL, NULL, NULL, 'Owner/Repo', '[\"src/**/*.rs\"]', 'active', '2026-01-02'),
             ('local-rule-slug', 'Published local', 'Local Body', NULL, NULL, '33333333-3333-4333-8333-333333333333', 'Owner/Repo', '[\"src/local/**/*.rs\"]', 'active', '2026-01-03'),
             ('local-only-rule', 'Local only', 'Body', NULL, NULL, NULL, 'Owner/Repo', '[\"src/local-only/**/*.rs\"]', 'active', '2026-01-04'),
             ('22222222-2222-4222-8222-222222222222', 'Foreign', 'Body', NULL, NULL, NULL, 'other/repo', '[\"other/**/*.rs\"]', 'active', '2026-01-05')",
        )
        .execute(&db)
        .await
        .expect("insert");

        let rules = load_existing_rules(&db, "owner/repo").await;

        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].rule_id, "33333333-3333-4333-8333-333333333333");
        assert_eq!(rules[0].title, "Published local");
        assert_eq!(rules[0].source_repo.as_deref(), Some("Owner/Repo"));
        assert_eq!(rules[0].file_patterns, vec!["src/local/**/*.rs"]);
        assert_eq!(rules[1].rule_id, "11111111-1111-4111-8111-111111111111");
        assert_eq!(rules[1].file_patterns, vec!["src/**/*.rs"]);
    }

    #[test]
    fn cloud_rule_id_check_accepts_uuid_shape_only() {
        assert!(looks_like_cloud_rule_id(
            "11111111-1111-4111-8111-111111111111"
        ));
        assert!(!looks_like_cloud_rule_id("rule-merge"));
        assert!(!looks_like_cloud_rule_id(
            "11111111-1111-4111-8111-11111111111x"
        ));
    }

    #[test]
    fn resolve_source_repo_prefers_git_remote_then_basename() {
        // Outside a git repo, the cwd basename still satisfies the
        // Project Scope Invariant.
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path();
        let repo = resolve_source_repo(Some(path.to_str().unwrap()));
        assert!(
            repo.is_some(),
            "tempdir basename must satisfy the invariant"
        );
        let repo = repo.unwrap();
        // Basenames are lowercased for stable casing across OSes.
        assert_eq!(repo, repo.to_ascii_lowercase());
    }
}
