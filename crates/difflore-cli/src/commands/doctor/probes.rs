//! Doctor data-probing for the default table view.
//!
//! Collects every `difflore_core` / `installer` / `CommandContext` call the
//! default `difflore doctor` table needs and decodes it into the plain
//! [`Findings`] struct for `table.rs` to render. Returns only decoded scalars,
//! options and small enums — severity / status / hint strings live in
//! `table.rs`.

use std::path::Path;

use super::memory_snapshot::{self, MemorySnapshot};
use crate::installer;
use crate::support::util::count_rules_for_repo;

/// Everything the readiness table needs, already fetched and decoded.
/// Construct one with [`gather`].
pub(crate) struct Findings {
    pub(crate) binary_version: String,
    pub(crate) master_key_storage: difflore_core::infra::crypto::MasterKeyStorageStatus,
    pub(crate) project_db: ProjectDbProbe,
    /// Pre-loaded "what the AI has learned" snapshot. Defaults (rendering to
    /// "") when the current repo has no ready memory, so no load is issued.
    pub(crate) memory_snapshot: MemorySnapshot,
    pub(crate) mcp: installer::McpStatusSnapshot,
    pub(crate) provider: ProviderProbe,
    pub(crate) cloud: CloudProbe,
    pub(crate) embedder: EmbedderProbe,
    pub(crate) git_hooks: GitHookState,
    pub(crate) daemon: DaemonProbe,
    pub(crate) recall_trace: difflore_core::observability::injection_log::InjectionPathSummary,
    pub(crate) gate_capture: GateCaptureProbe,
}

/// Decoded project-DB counts. `db_available == false` means the pool
/// failed to open and every numeric field is meaningless.
pub(crate) struct ProjectDbProbe {
    pub(crate) db_available: bool,
    pub(crate) total_rules: i64,
    pub(crate) prs_imported: i64,
    pub(crate) repo_full_name: Option<String>,
    pub(crate) review_source_repo_full_name: Option<String>,
    pub(crate) scoped_active_rules: i64,
    pub(crate) review_source_active_rules: i64,
}

/// Decoded provider-list outcome.
pub(crate) enum ProviderProbe {
    DbUnavailable,
    /// Provider config was unreadable; carries the error text.
    Error(String),
    NoneConfigured,
    /// Providers exist; the resolved active provider's name.
    Active(String),
    /// Providers exist but none is active and none could be defaulted.
    NoneActive,
}

/// Decoded cloud-login outcome.
pub(crate) enum CloudProbe {
    NotLoggedIn,
    LoggedIn {
        plan: String,
        team_name: Option<String>,
        impact: CloudImpactProbe,
    },
}

#[derive(Default)]
pub(crate) struct CloudImpactProbe {
    pub(crate) coverage: Option<difflore_core::contract::dto::ImpactCoverageDto>,
    pub(crate) fix_scorecard: Option<difflore_core::contract::dto::ImpactFixScorecardDto>,
    pub(crate) coverage_error: Option<String>,
    pub(crate) fix_scorecard_error: Option<String>,
}

/// Raw embedder inputs; the `table.rs` shapers decode the activity tail and
/// pick the final row.
pub(crate) struct EmbedderProbe {
    pub(crate) kind: difflore_core::context::embedding::ActiveEmbedderKind,
    pub(crate) activity_tail: Vec<difflore_core::observability::activity_stream::ActivityEvent>,
    /// `None` when the per-project index DB could not be opened.
    pub(crate) diagnostics: Option<difflore_core::context::EmbeddingDiagnostics>,
}

/// Decoded daemon state, after the stale-pid cleanup attempt.
pub(crate) struct DaemonProbe {
    pub(crate) state: DaemonProbeState,
    pub(crate) heartbeat_age_ms: Option<i64>,
    pub(crate) last_drain_age_ms: Option<i64>,
    pub(crate) last_attempted: Option<usize>,
    pub(crate) last_confirmed: Option<usize>,
    pub(crate) cloud_pending: Option<i64>,
    pub(crate) observation_pending: Option<i64>,
    pub(crate) hook_spill_count: Option<usize>,
}

pub(crate) enum DaemonProbeState {
    Running,
    /// Stale pid whose cleanup attempt failed (locked file etc.).
    StaleCleanupFailed,
    NotRunning,
}

pub(crate) enum GitHookState {
    NotARepo,
    None,
    Installed,
    OtherHook,
    /// Hook file exists on disk but we couldn't read it (permissions,
    /// IO error). Distinct from `OtherHook` — we don't actually know
    /// what's in there, and saying "another tool installed it" would
    /// be misleading (the file might be `DiffLore`'s own hook in a
    /// state we just can't open).
    Unreadable(String),
}

pub(crate) struct GateCaptureProbe {
    pub(crate) status: crate::session_mine::trigger::GateCaptureStatus,
}

/// Probe every data source the default doctor table reads and return a
/// fully decoded [`Findings`]. The single entry point for `table.rs`.
pub(crate) async fn gather(ctx: &crate::runtime::CommandContext) -> Findings {
    let pool = Some(&ctx.db);
    // Probe side effects run in display order (e.g. daemon stale-pid cleanup
    // near the row that reports it).
    let project_db = probe_project_db(pool, &ctx.project).await;
    let binary_version = env!("CARGO_PKG_VERSION").to_owned();
    let master_key_storage = difflore_core::infra::crypto::probe_master_key_storage();
    let mcp = installer::collect_status_snapshot_with_runtime_probe();
    let provider = probe_provider(pool).await;
    let cloud = probe_cloud(ctx).await;
    let embedder = probe_embedder().await;
    let git_hooks = probe_git_hook_state();
    let daemon = probe_daemon(&ctx.db).await;
    let recall_trace = difflore_core::observability::injection_log::summary_24h();
    let gate_capture = probe_gate_capture(&ctx.project);
    // Load the "what the AI has learned" snapshot only when the repo has
    // ready memory; otherwise hand the renderer a default (renders to "").
    let memory_snapshot = if project_db.repo_memory_ready {
        memory_snapshot::load_for_repo(&ctx.db, &project_db.repo_aliases).await
    } else {
        MemorySnapshot::default()
    };
    Findings {
        binary_version,
        master_key_storage,
        project_db: project_db.probe,
        memory_snapshot,
        mcp,
        provider,
        cloud,
        embedder,
        git_hooks,
        daemon,
        recall_trace,
        gate_capture,
    }
}

fn probe_gate_capture(project: &Path) -> GateCaptureProbe {
    let project = project.to_string_lossy();
    let status =
        crate::session_mine::trigger::gate_capture_status_for_project(Some(project.as_ref()))
            .unwrap_or(crate::session_mine::trigger::GateCaptureStatus::Ready);
    GateCaptureProbe { status }
}

/// Carrier so the snapshot load can be gated on repo readiness without
/// leaking `repo_memory_ready` / `repo_aliases` into the renderer.
struct ProjectDbResult {
    probe: ProjectDbProbe,
    repo_memory_ready: bool,
    repo_aliases: Vec<String>,
}

async fn probe_project_db(
    pool: Option<&difflore_core::SqlitePool>,
    project: &Path,
) -> ProjectDbResult {
    let Some(pool) = pool else {
        return ProjectDbResult {
            repo_memory_ready: false,
            repo_aliases: Vec::new(),
            probe: ProjectDbProbe {
                db_available: false,
                total_rules: 0,
                prs_imported: 0,
                repo_full_name: None,
                review_source_repo_full_name: None,
                scoped_active_rules: 0,
                review_source_active_rules: 0,
            },
        };
    };
    let total_rules = match difflore_core::skills::stats(pool).await {
        Ok(s) => s.total,
        Err(_) => 0,
    };
    let counts = difflore_core::infra::db::table_counts(pool, &["review_items"]).await;
    let prs_imported = counts
        .into_iter()
        .find(|(table, _)| table == "review_items")
        .and_then(|(_, result)| result.ok())
        .unwrap_or(0);
    if total_rules == 0 {
        // Empty corpus blocks recall: there's nothing to retrieve.
        return ProjectDbResult {
            repo_memory_ready: false,
            repo_aliases: Vec::new(),
            probe: ProjectDbProbe {
                db_available: true,
                total_rules: 0,
                prs_imported,
                repo_full_name: None,
                review_source_repo_full_name: None,
                scoped_active_rules: 0,
                review_source_active_rules: 0,
            },
        };
    }

    let configured_gitlab_hosts = difflore_core::ingest::gitlab::auth::configured_hosts().await;
    let detected_repo_remotes = difflore_core::infra::git::detect_repo_full_names_with_gitlab_hosts(
        &project.to_string_lossy(),
        &configured_gitlab_hosts,
    );
    let repo_remotes =
        difflore_core::skills::expand_repo_scopes_with_source_aliases(pool, &detected_repo_remotes)
            .await
            .unwrap_or(detected_repo_remotes);
    let repo_full_name = repo_remotes.first().cloned();
    let review_source_repo_full_name = repo_remotes.get(1).cloned();
    let active_rules = difflore_core::skills::list(pool).await.unwrap_or_default();
    let source_repos = difflore_core::skills::list_source_repos(pool)
        .await
        .unwrap_or_default();
    let scoped_active_rules =
        count_rules_for_repo(&active_rules, &source_repos, repo_full_name.as_deref());
    let review_source_active_rules = count_rules_for_repo(
        &active_rules,
        &source_repos,
        review_source_repo_full_name.as_deref(),
    );
    let repo_ready =
        repo_full_name.is_some() && (scoped_active_rules > 0 || review_source_active_rules > 0);

    ProjectDbResult {
        repo_memory_ready: repo_ready,
        repo_aliases: repo_remotes,
        probe: ProjectDbProbe {
            db_available: true,
            total_rules,
            prs_imported,
            repo_full_name,
            review_source_repo_full_name,
            scoped_active_rules,
            review_source_active_rules,
        },
    }
}

async fn probe_provider(pool: Option<&difflore_core::SqlitePool>) -> ProviderProbe {
    let Some(pool) = pool else {
        return ProviderProbe::DbUnavailable;
    };
    match difflore_core::infra::providers::list(pool).await {
        Ok(providers) if providers.is_empty() => ProviderProbe::NoneConfigured,
        Ok(providers) => {
            let active = providers
                .iter()
                .find(|p| p.is_active)
                .or_else(|| providers.first());
            match active {
                Some(p) => ProviderProbe::Active(p.name.clone()),
                None => ProviderProbe::NoneActive,
            }
        }
        Err(e) => ProviderProbe::Error(e.to_string()),
    }
}

async fn probe_cloud(ctx: &crate::runtime::CommandContext) -> CloudProbe {
    let cloud_client = ctx.cloud().await;
    if cloud_client.is_logged_in() {
        let (status, coverage, fix_scorecard) = tokio::join!(
            difflore_core::cloud::sync::fetch_cloud_status(cloud_client),
            cloud_client.get_impact_coverage(),
            cloud_client.get_impact_fix_scorecard(),
        );
        let impact = CloudImpactProbe {
            coverage: coverage.as_ref().ok().cloned(),
            fix_scorecard: fix_scorecard.as_ref().ok().cloned(),
            coverage_error: coverage.err().map(|e| e.to_string()),
            fix_scorecard_error: fix_scorecard.err().map(|e| e.to_string()),
        };
        CloudProbe::LoggedIn {
            plan: status.plan.as_deref().unwrap_or("free").to_owned(),
            team_name: status.team_name,
            impact,
        }
    } else {
        CloudProbe::NotLoggedIn
    }
}

/// Embedder readiness probe. Delegates to `probe_active_embedder` so doctor
/// agrees with the runtime resolver, and consults the per-project embedding
/// profile diagnostic so the table doesn't show green over a dead vector lane.
/// Runs no live embed call, so it stays cheap and imports no network failures
/// (`doctor --report` owns the measured self-recall number).
async fn probe_embedder() -> EmbedderProbe {
    let kind = difflore_core::context::embedding::probe_active_embedder().await;
    let activity_tail = difflore_core::observability::activity_stream::tail(200);
    let diagnostics = match difflore_core::context::index_db::get_pool_for_cwd().await {
        Ok(index_pool) => Some(
            difflore_core::context::gather_embedding_diagnostics_with_activity(&index_pool).await,
        ),
        Err(_) => None,
    };
    EmbedderProbe {
        kind,
        activity_tail,
        diagnostics,
    }
}

async fn probe_daemon(pool: &difflore_core::SqlitePool) -> DaemonProbe {
    // Stale pid is purely a leftover lock file from a dead process —
    // there's nothing for the user to debug, so we clean it on read
    // and report "off" with an informational status instead of a
    // permanent Warn that just trains users to ignore Optional hints.
    let mut daemon_status = difflore_core::infra::daemon::status();
    if let difflore_core::infra::daemon::DaemonStatus::Stale { .. } = daemon_status
        && let Ok(pid_path) = difflore_core::infra::daemon::pid_path()
        && std::fs::remove_file(&pid_path).is_ok()
    {
        daemon_status = difflore_core::infra::daemon::DaemonStatus::NotRunning;
    }
    let state = match daemon_status {
        difflore_core::infra::daemon::DaemonStatus::Running { .. } => DaemonProbeState::Running,
        // Only reached when the cleanup attempt failed (locked file etc.).
        difflore_core::infra::daemon::DaemonStatus::Stale { .. } => {
            DaemonProbeState::StaleCleanupFailed
        }
        difflore_core::infra::daemon::DaemonStatus::NotRunning => DaemonProbeState::NotRunning,
    };

    let now_ms = chrono::Utc::now().timestamp_millis();
    let daemon_state = difflore_core::infra::daemon::read_state().ok().flatten();
    let queue = difflore_core::cloud::outbox::OutboxQueue::new(pool.clone());
    let cloud_pending = queue
        .counts()
        .await
        .ok()
        .map(|counts| counts.pending + counts.processing);
    let observation_pending =
        match difflore_core::cloud::observations::ObservationEmitter::open_default().await {
            Ok(emitter) => emitter.pending_upload_count().await.ok(),
            Err(_) => None,
        };
    let hook_spill_count = difflore_core::cloud::outbox::hook_spill_stats()
        .ok()
        .map(|stats| stats.count);

    DaemonProbe {
        state,
        heartbeat_age_ms: daemon_state
            .as_ref()
            .map(|state| now_ms.saturating_sub(state.heartbeat_at_ms)),
        last_drain_age_ms: daemon_state
            .as_ref()
            .and_then(|state| state.last_drain_at_ms)
            .map(|ts| now_ms.saturating_sub(ts)),
        last_attempted: daemon_state.as_ref().map(|state| state.last_attempted),
        last_confirmed: daemon_state.as_ref().map(|state| state.last_confirmed),
        cloud_pending,
        observation_pending,
        hook_spill_count,
    }
}

fn probe_git_hook_state() -> GitHookState {
    let cwd = difflore_core::infra::paths::current_project_root();
    // Resolve the git dir via `git rev-parse --git-dir` so worktrees (where
    // `.git` is a file, not a dir) find the right hooks/ location. A naive
    // `cwd.join(".git/hooks")` would dead-end and report "no hook".
    let output = difflore_core::infra::git::git_command(&cwd)
        .args(["rev-parse", "--git-dir"])
        .output();
    let git_dir = match output {
        Ok(o) if o.status.success() => {
            let raw = String::from_utf8_lossy(&o.stdout).trim().to_owned();
            if raw.is_empty() {
                return GitHookState::NotARepo;
            }
            let p = std::path::PathBuf::from(&raw);
            if p.is_absolute() { p } else { cwd.join(p) }
        }
        _ => return GitHookState::NotARepo,
    };
    let hook_path = git_dir.join("hooks").join("pre-commit");
    if !hook_path.exists() {
        return GitHookState::None;
    }
    let body = match std::fs::read_to_string(&hook_path) {
        Ok(b) => b,
        Err(e) => return GitHookState::Unreadable(e.to_string()),
    };
    if body.contains("difflore") {
        GitHookState::Installed
    } else {
        GitHookState::OtherHook
    }
}
