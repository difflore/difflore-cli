//! Doctor table renderer: the aligned-column status surface for the default
//! `difflore doctor` invocation (the richer markdown report stays behind
//! `--report`).
//!
//! Rows are grouped into three sections:
//!
//!   Ready for local value   — green/passing, no user action required
//!   Blocks core value       — recall cannot run (DB / corpus broken)
//!   Optional improvements   — provider, MCP wiring, daemon, git hooks
//!
//! Empty sections are omitted. The footer collapses to a single `next:`
//! action line pointing at the highest-priority blocker (or, if all-green,
//! at `difflore recall --diff`).
//!
//! Pure presentation: consumes a precomputed [`probes::Findings`] and never
//! touches a live data source, so shapers can be tested against a hand-built
//! findings value.

use colored::Colorize;

use super::embedding_degradation::{
    SUSTAINED_TRANSIENT_FALLBACK_THRESHOLD, is_persistent_embedding_degradation,
    should_count_embedding_degradation,
};
use super::memory_snapshot;
use super::probes::{
    self, CloudImpactProbe, CloudProbe, DaemonProbe, DaemonProbeState, EmbedderProbe, Findings,
    GateCaptureProbe, GitHookState, ProjectDbProbe, ProviderProbe,
};
use super::util::age_label_ms;
use crate::installer;
use crate::style;
use difflore_core::infra::crypto::{KeyseedStatus, MasterKeyStorageStatus};

const RULE: &str = "-----------------------------------------";
const LABEL_W: usize = 17;

/// Visual marker for a row — orthogonal to `Severity`. A `Ready` row
/// is normally `Ok`, but a `Blocker` row may be `Warn` (degraded) or
/// `Err` (hard failure) depending on what was observed.
#[derive(Clone, Copy)]
enum Status {
    Ok,
    Warn,
    Err,
}

/// Section a row belongs to; drives grouping in the renderer.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Severity {
    Blocker,
    Ready,
    Optional,
}

impl Severity {
    const fn heading(self) -> &'static str {
        match self {
            Self::Blocker => "Blocks core value",
            Self::Ready => "Ready for local value",
            Self::Optional => "Optional improvements",
        }
    }
}

struct Row {
    severity: Severity,
    status: Status,
    label: &'static str,
    value: String,
    /// Follow-up lines rendered indented under the row. For blockers, the
    /// first hint should lead with the consequence.
    hints: Vec<String>,
    /// Repair command suggested as the `next:` action when this row is the
    /// highest-priority blocker. `None` when there's no actionable repair.
    repair: Option<String>,
}

impl Row {
    const fn ready_ok(label: &'static str, value: String) -> Self {
        Self {
            severity: Severity::Ready,
            status: Status::Ok,
            label,
            value,
            hints: Vec::new(),
            repair: None,
        }
    }
}

pub(crate) async fn render_table(ctx: &crate::runtime::CommandContext) -> String {
    let findings = probes::gather(ctx).await;
    render_findings(&findings)
}

/// Shape a fully-probed [`Findings`] into the doctor surface string. Split
/// from `render_table` so tests can feed a hand-built findings value.
fn render_findings(findings: &Findings) -> String {
    let rows = vec![
        binary_row(&findings.binary_version),
        secrets_row(&findings.master_key_storage),
        project_db_row(&findings.project_db),
        mcp_row(&findings.mcp),
        provider_row(&findings.provider),
        cloud_row(&findings.cloud),
        embedder_row(&findings.embedder),
        git_hooks_row(&findings.git_hooks),
        recall_trace_row(&findings.recall_trace),
        gate_capture_row(&findings.gate_capture),
        daemon_row(&findings.daemon),
    ];
    // "What we've learned" preview — renders to "" when the snapshot is empty
    // (fresh install / no ready repo memory).
    let snapshot_block = memory_snapshot::render(&findings.memory_snapshot);
    render_rows(&rows, &snapshot_block)
}

fn gate_capture_row(probe: &GateCaptureProbe) -> Row {
    match &probe.status {
        crate::session_mine::trigger::GateCaptureStatus::Ready => Row::ready_ok(
            "capture",
            "session learning ready | recall unaffected".to_owned(),
        ),
        crate::session_mine::trigger::GateCaptureStatus::Paused {
            reason,
            retry_after_ms,
            ..
        } => {
            let mut hints = vec![
                "recall still works; only new session learning capture paused".to_owned(),
                format!("reason: {}", short_detail(reason)),
            ];
            if *retry_after_ms > 0 {
                hints.push(format!(
                    "next automatic retry in {}",
                    duration_label_ms(*retry_after_ms)
                ));
            }
            Row {
                severity: Severity::Optional,
                status: Status::Warn,
                label: "capture",
                value: "capture paused | recall unaffected".to_owned(),
                hints,
                repair: Some("difflore doctor --report".to_owned()),
            }
        }
    }
}

fn recall_trace_row(
    summary: &difflore_core::observability::injection_log::InjectionPathSummary,
) -> Row {
    let mut hints = Vec::new();
    hints.push("machine-wide 24h trace; not scoped to the current repo".to_owned());
    if let Some(detail) = summary.detail.as_deref() {
        hints.push(detail.to_owned());
    }
    if !summary.dropped_by_reason.is_empty() {
        hints.push(format!(
            "drop reasons: {}",
            format_count_map(&summary.dropped_by_reason)
        ));
    }
    if !summary.injected_by_path.is_empty() {
        hints.push(format!(
            "injected paths: {}",
            format_count_map(&summary.injected_by_path)
        ));
    }
    if let Some(path) = summary.path.as_ref() {
        hints.push(format!("trace log: {}", path.display()));
    }

    let status = if recall_trace_has_abnormal_drop_reason(&summary.dropped_by_reason) {
        Status::Warn
    } else {
        Status::Ok
    };
    let value = if summary.count_24h == 0 {
        "no recall trace events in 24h".to_owned()
    } else {
        format!(
            "{} event{} | {} rule{} injected",
            summary.count_24h,
            if summary.count_24h == 1 { "" } else { "s" },
            summary.total_rules_injected,
            if summary.total_rules_injected == 1 {
                ""
            } else {
                "s"
            },
        )
    };

    Row {
        severity: Severity::Optional,
        status,
        label: "recall trace",
        value,
        hints,
        repair: None,
    }
}

fn recall_trace_has_abnormal_drop_reason(
    dropped_by_reason: &std::collections::BTreeMap<String, usize>,
) -> bool {
    dropped_by_reason
        .keys()
        .any(|reason| !is_benign_recall_trace_drop_reason(reason))
}

fn is_benign_recall_trace_drop_reason(reason: &str) -> bool {
    matches!(
        reason,
        "recent_duplicate"
            | "pre_read_disabled"
            | "non_mutating_tool"
            | "missing_target_file"
            | "retrieval_empty"
            | "no_repo_scope"
            | "short_circuit"
            | "not_applicable"
            | "disabled"
    )
}

fn format_count_map(map: &std::collections::BTreeMap<String, usize>) -> String {
    map.iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn binary_row(version: &str) -> Row {
    // Always Ready: if the binary couldn't run, we wouldn't be here.
    Row::ready_ok("binary", format!("v{version}"))
}

fn secrets_row(status: &MasterKeyStorageStatus) -> Row {
    match status {
        MasterKeyStorageStatus::EnvOverride => Row::ready_ok(
            "secrets",
            "DIFFLORE_MASTER_KEY override (32-byte hex)".to_owned(),
        ),
        MasterKeyStorageStatus::DebugFileOverride { path } => Row::ready_ok(
            "secrets",
            format!("debug master-key file ({})", path.display()),
        ),
        MasterKeyStorageStatus::KeyringReady => {
            Row::ready_ok("secrets", "OS keyring master key".to_owned())
        }
        MasterKeyStorageStatus::KeyringWillCreate => Row::ready_ok(
            "secrets",
            "OS keyring available | master key will be created on first secret".to_owned(),
        ),
        MasterKeyStorageStatus::KeyringInvalid(error) => Row {
            severity: Severity::Optional,
            status: Status::Err,
            label: "secrets",
            value: "OS keyring entry invalid".to_owned(),
            hints: vec![
                format!("stored master key is not a valid 32-byte hex key: {error}"),
                "re-authenticate cloud/provider credentials after repairing the keyring entry"
                    .to_owned(),
            ],
            repair: None,
        },
        MasterKeyStorageStatus::LocalFallback {
            keyring_error,
            keyseed,
        } => local_fallback_secrets_row(keyring_error, keyseed),
        MasterKeyStorageStatus::CiRequiresExplicitKey { keyring_error } => Row {
            severity: Severity::Optional,
            status: Status::Err,
            label: "secrets",
            value: "OS keyring unavailable on CI".to_owned(),
            hints: vec![
                format!("keyring error: {}", short_detail(keyring_error)),
                "set DIFFLORE_MASTER_KEY=<64-char-hex> for CI/headless secret storage".to_owned(),
            ],
            repair: None,
        },
    }
}

fn local_fallback_secrets_row(keyring_error: &str, keyseed: &KeyseedStatus) -> Row {
    let (status, value) = match keyseed {
        KeyseedStatus::Present {
            permissions_ok: Some(true),
            ..
        } => (
            Status::Warn,
            "local fallback | keyseed present (0600)".to_owned(),
        ),
        KeyseedStatus::Present {
            permissions_ok: Some(false),
            ..
        } => (
            Status::Warn,
            "local fallback | keyseed present (permissions need repair)".to_owned(),
        ),
        KeyseedStatus::Present { .. } => {
            (Status::Warn, "local fallback | keyseed present".to_owned())
        }
        KeyseedStatus::Missing { .. } => (
            Status::Warn,
            "local fallback | keyseed will be created on first secret".to_owned(),
        ),
        KeyseedStatus::Invalid { .. } => {
            (Status::Err, "local fallback | keyseed invalid".to_owned())
        }
        KeyseedStatus::Unreadable { .. } => (
            Status::Err,
            "local fallback | keyseed unreadable".to_owned(),
        ),
        KeyseedStatus::Unavailable { .. } => (
            Status::Err,
            "local fallback | keyseed unavailable".to_owned(),
        ),
    };
    let mut hints = vec![
        format!("OS keyring unavailable: {}", short_detail(keyring_error)),
        "fallback secrets are derived from a persisted 32-byte random keyseed".to_owned(),
    ];
    hints.extend(keyseed_hints(keyseed));
    Row {
        severity: Severity::Optional,
        status,
        label: "secrets",
        value,
        hints,
        repair: None,
    }
}

fn keyseed_hints(status: &KeyseedStatus) -> Vec<String> {
    match status {
        KeyseedStatus::Present { path, .. } => vec![format!("keyseed: {}", path.display())],
        KeyseedStatus::Missing { path } => {
            vec![format!("will create {} with mode 0600", path.display())]
        }
        KeyseedStatus::Invalid { path, error, .. }
        | KeyseedStatus::Unreadable { path, error, .. } => {
            vec![format!("{}: {}", path.display(), short_detail(error))]
        }
        KeyseedStatus::Unavailable { error } => vec![short_detail(error)],
    }
}

fn short_detail(detail: &str) -> String {
    const MAX: usize = 140;
    let first = detail.lines().next().unwrap_or(detail).trim();
    let mut chars = first.chars();
    let truncated: String = chars.by_ref().take(MAX).collect();
    if chars.next().is_none() {
        first.to_owned()
    } else {
        format!("{truncated}...")
    }
}

fn duration_label_ms(ms: i64) -> String {
    let label = age_label_ms(ms);
    label.strip_suffix(" ago").unwrap_or(&label).to_owned()
}

fn project_db_row(probe: &ProjectDbProbe) -> Row {
    if !probe.db_available {
        return Row {
            severity: Severity::Blocker,
            status: Status::Err,
            label: "project db",
            value: "unavailable".to_owned(),
            hints: vec![
                "fix and recall both read this DB; they will fail until it opens".to_owned(),
            ],
            repair: Some("difflore doctor --report".to_owned()),
        };
    }
    let total_rules = probe.total_rules;
    let prs_imported = probe.prs_imported;
    if total_rules == 0 {
        // Empty corpus blocks recall: there's nothing to retrieve.
        return Row {
            severity: Severity::Blocker,
            status: Status::Warn,
            label: "project db",
            value: "no memory indexed".to_owned(),
            hints: vec![
                "recall returns nothing without memory: import review history first".to_owned(),
                "difflore status   (shows the shortest local path for this repo)".to_owned(),
                "difflore import-reviews --max-prs 50".to_owned(),
            ],
            repair: Some("difflore status".to_owned()),
        };
    }

    let repo_full_name = probe.repo_full_name.as_deref();
    let review_source_repo_full_name = probe.review_source_repo_full_name.as_deref();
    let scoped_active_rules = probe.scoped_active_rules;
    let review_source_active_rules = probe.review_source_active_rules;
    let repo_ready =
        repo_full_name.is_some() && (scoped_active_rules > 0 || review_source_active_rules > 0);

    if repo_ready {
        let value = match (
            scoped_active_rules,
            review_source_active_rules,
            review_source_repo_full_name,
        ) {
            (0, n, Some(source)) if n > 0 => format!(
                "{} upstream memor{} from {source} · {} on this machine",
                n,
                if n == 1 { "y" } else { "ies" },
                total_rules,
            ),
            (s, n, Some(source)) if n > 0 => {
                format!("{s} scoped + {n} from {source} · {total_rules} on this machine")
            }
            (s, _, _) => format!(
                "{} memor{} for this repo · {} on this machine",
                s,
                if s == 1 { "y" } else { "ies" },
                total_rules,
            ),
        };
        return Row {
            severity: Severity::Ready,
            status: Status::Ok,
            label: "project db",
            value: format!(
                "{} · {} local PR{} imported",
                value,
                prs_imported,
                if prs_imported == 1 { "" } else { "s" },
            ),
            hints: vec![],
            repair: None,
        };
    }

    let (value, import_cmd) = match repo_full_name {
        Some(repo) => (
            format!("0 memories for {repo} | {total_rules} on this machine"),
            format!("difflore import-reviews --repo {repo}"),
        ),
        None => (
            format!("{total_rules} memories on this machine | no supported repo remote detected"),
            "difflore status".to_owned(),
        ),
    };
    let mut hints = vec![
        "no current-repo memory is ready; doctor will not show unrelated repo activity here"
            .to_owned(),
        "difflore status   (shows the repo-scoped value path)".to_owned(),
    ];
    if let Some(repo) = repo_full_name {
        if let Some(source) = review_source_repo_full_name {
            hints.push(format!(
                "difflore import-reviews --repo {repo} --from-upstream {source}"
            ));
        } else {
            hints.push(format!("difflore import-reviews --repo {repo}"));
        }
    } else {
        hints.push("run inside a GitHub-backed repo, or add an origin/upstream remote".to_owned());
    }

    Row {
        severity: Severity::Blocker,
        status: Status::Warn,
        label: "project db",
        value,
        hints,
        repair: Some(import_cmd),
    }
}

fn mcp_row(snapshot: &installer::McpStatusSnapshot) -> Row {
    let installed: Vec<&str> = snapshot
        .clients
        .iter()
        .filter(|c| matches!(c.state, installer::InstallState::Installed))
        .map(|c| c.name)
        .collect();
    let conflicting_clients: Vec<&str> = snapshot
        .clients
        .iter()
        .filter(|c| matches!(c.state, installer::InstallState::Conflict))
        .map(|c| c.name)
        .collect();
    let conflicts = conflicting_clients.len();
    let drift: Vec<String> = snapshot
        .clients
        .iter()
        .filter(|c| {
            c.detected
                && matches!(
                    c.state,
                    installer::InstallState::NotInstalled | installer::InstallState::Unknown
                )
        })
        .map(|c| c.name.to_owned())
        .collect();
    let record_state = snapshot.canonical_record.state;
    let record_ok = matches!(record_state, installer::CanonicalRecordState::Present);
    let diagnosis_hints = mcp_diagnosis_hints(snapshot.diagnosis.as_ref(), &installed);
    let runtime_state = snapshot.runtime_probe.as_ref().map(|probe| probe.state);
    if matches!(
        runtime_state,
        Some(installer::RuntimeProbeState::Failed | installer::RuntimeProbeState::Timeout)
    ) {
        let runtime_label = match runtime_state {
            Some(installer::RuntimeProbeState::Failed) => "runtime failed",
            Some(installer::RuntimeProbeState::Timeout) => "runtime timeout",
            _ => "runtime unavailable",
        };
        return Row {
            severity: Severity::Blocker,
            status: if matches!(runtime_state, Some(installer::RuntimeProbeState::Timeout)) {
                Status::Warn
            } else {
                Status::Err
            },
            label: "MCP server",
            value: format!("{runtime_label} · {} installed", installed.len()),
            hints: diagnosis_hints,
            repair: Some("difflore agents status --json".to_owned()),
        };
    }

    // Drift = detected agents that aren't yet installed. This catches
    // both the first-time case (no install at all) and the post-install
    // case where the user added a new IDE later. `difflore init` is
    // idempotent — re-running picks up the new IDE without re-prompting
    // for already-configured ones — so it's the canonical recovery
    // command in both cases.
    if conflicts > 0 {
        // Conflict means SOME installed clients point at the wrong
        // binary. If at least one client correctly points at DiffLore
        // and the runtime is healthy (we got here past the runtime
        // gate), the agent path is still working for those clients —
        // demote to Optional/Warn so the header doesn't tell the user
        // their core value is blocked when it isn't.
        let runtime_healthy = matches!(runtime_state, Some(installer::RuntimeProbeState::Ok),);
        let some_clients_ok = !installed.is_empty();
        let (severity, status) = if runtime_healthy && some_clients_ok {
            (Severity::Blocker, Status::Warn)
        } else {
            (Severity::Blocker, Status::Err)
        };
        Row {
            severity,
            status,
            label: "MCP server",
            value: format_mcp_conflict_value(&installed, &conflicting_clients),
            hints: vec![
                format!(
                    "{} wired to a different binary; DiffLore won't be invoked there",
                    conflicting_clients.join(", ")
                ),
                "difflore agents status".to_owned(),
            ],
            repair: Some("difflore agents status".to_owned()),
        }
    } else if installed.is_empty() {
        // No agent wired up. CLI recall and memory browsing still work,
        // so this is Optional rather than a hard blocker — wiring an
        // agent unlocks the agent-side experience but is not on the
        // path to local value.
        Row {
            severity: Severity::Optional,
            status: Status::Warn,
            label: "MCP server",
            value: if drift.is_empty() {
                "no clients installed".to_owned()
            } else {
                format!("0 installed · {} detected", drift.len())
            },
            hints: vec![
                "agents can request team rules once wired; CLI commands work either way".to_owned(),
                "difflore init".to_owned(),
            ]
            .into_iter()
            .chain(diagnosis_hints)
            .collect(),
            repair: None,
        }
    } else if !drift.is_empty() || !record_ok {
        // Some clients wired, others detected but not installed —
        // partial coverage is still usable, so this is Ready with a
        // soft hint rather than a blocker.
        let mut value_bits = vec![format!("{} installed", installed.len())];
        if !drift.is_empty() {
            value_bits.push(format!(
                "{} detected without DiffLore: {}",
                drift.len(),
                drift.join(", ")
            ));
        }
        if !record_ok {
            value_bits.push(format!(
                "record {}",
                mcp_canonical_record_state_label(record_state)
            ));
        }
        Row {
            severity: Severity::Ready,
            status: Status::Warn,
            label: "MCP server",
            value: value_bits.join(" · "),
            hints: if diagnosis_hints.is_empty() {
                vec!["difflore init".to_owned()]
            } else {
                diagnosis_hints
            },
            repair: None,
        }
    } else {
        Row::ready_ok(
            "MCP server",
            format!("{} ({})", installed.join(", "), installed.len()),
        )
    }
}

fn format_mcp_conflict_value(installed: &[&str], conflicting_clients: &[&str]) -> String {
    let ready = if installed.is_empty() {
        "0 ready".to_owned()
    } else {
        format!("{} ready ({})", installed.len(), installed.join(", "))
    };
    let conflicts = conflicting_clients.len();
    format!(
        "{ready} · {} conflict{} ({})",
        conflicts,
        if conflicts == 1 { "" } else { "s" },
        conflicting_clients.join(", "),
    )
}

fn mcp_diagnosis_hints(
    diagnosis: Option<&installer::McpStatusDiagnosis>,
    installed: &[&str],
) -> Vec<String> {
    let Some(diagnosis) = diagnosis else {
        return Vec::new();
    };
    let mut hints = Vec::new();
    let needs_wiring = diagnosis
        .affected_clients
        .iter()
        .filter(|client| !installed.iter().any(|ready| ready == &client.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if needs_wiring.is_empty() {
        hints.push(diagnosis.summary.clone());
    } else {
        let verb = if needs_wiring.len() == 1 {
            "needs"
        } else {
            "need"
        };
        hints.push(format!(
            "{} {verb} MCP entries written or refreshed",
            needs_wiring.join(", ")
        ));
        hints.push("difflore agents install".to_owned());
    }
    hints
}

const fn mcp_canonical_record_state_label(state: installer::CanonicalRecordState) -> &'static str {
    match state {
        installer::CanonicalRecordState::Missing => "missing",
        installer::CanonicalRecordState::Present => "present",
        installer::CanonicalRecordState::Stale => "stale",
        installer::CanonicalRecordState::Conflict => "conflict",
    }
}

fn provider_row(probe: &ProviderProbe) -> Row {
    // A missing provider blocks `fix`, not memory browsing or `recall`.
    // Keep absence Optional and call out the gated command in the hint.
    match probe {
        ProviderProbe::DbUnavailable => Row {
            severity: Severity::Blocker,
            status: Status::Err,
            label: "provider",
            value: "db unavailable".to_owned(),
            hints: vec![
                "provider list lives in the project DB which failed to open".to_owned(),
                "difflore doctor --report -   (copy/paste diagnostic for the issue tracker)"
                    .to_owned(),
            ],
            repair: None,
        },
        ProviderProbe::NoneConfigured => Row {
            severity: Severity::Optional,
            status: Status::Warn,
            label: "provider",
            value: "none configured".to_owned(),
            hints: vec![
                "needed for `difflore fix` only; recall and memory work without it".to_owned(),
                "difflore providers setup".to_owned(),
            ],
            repair: None,
        },
        ProviderProbe::Active(name) => Row::ready_ok("provider", name.clone()),
        ProviderProbe::NoneActive => Row {
            severity: Severity::Optional,
            status: Status::Warn,
            label: "provider",
            value: "none active".to_owned(),
            hints: vec![
                "needed for `difflore fix` only; pick one to enable fixes".to_owned(),
                "difflore providers set-active <name>".to_owned(),
            ],
            repair: None,
        },
        ProviderProbe::Error(e) => Row {
            severity: Severity::Optional,
            status: Status::Err,
            label: "provider",
            value: format!("unavailable ({e})"),
            hints: vec!["provider config is unreadable; `difflore fix` won't run".to_owned()],
            repair: None,
        },
    }
}

fn cloud_row(probe: &CloudProbe) -> Row {
    // Cloud login is Ready when present (it unlocks team sync /
    // dashboards) but its absence is Optional — local fix and recall
    // both work fully offline.
    match probe {
        CloudProbe::LoggedIn {
            plan,
            team_name,
            impact,
        } => {
            let suffix = match team_name.as_deref() {
                Some(team) => format!(" | team: {team}"),
                None => String::new(),
            };
            let corpus_suffix = impact
                .coverage
                .as_ref()
                .filter(|coverage| coverage.prs > 0)
                .map(|coverage| format!(" | {} cloud PRs indexed", coverage.prs))
                .unwrap_or_default();
            // Free notes the managed-embedding cap but prints no exact number
            // (the default doctor row has no fresh cap/usage cache; cap hits
            // are surfaced by the embedder row from telemetry).
            let embedding_suffix = if plan.eq_ignore_ascii_case("free") {
                " | managed embedding cap | upgrade or embeddings setup"
            } else {
                " | unlimited embedding"
            };
            let mut hints = cloud_impact_hints(impact);
            if !hints.is_empty() {
                hints.push(format!(
                    "impact dashboard: {}",
                    style::cmd("difflore cloud impact")
                ));
            }
            Row {
                severity: Severity::Ready,
                status: Status::Ok,
                label: "cloud",
                value: format!("logged in | plan: {plan}{suffix}{corpus_suffix}{embedding_suffix}"),
                hints,
                repair: None,
            }
        }
        CloudProbe::NotLoggedIn => Row {
            severity: Severity::Optional,
            status: Status::Warn,
            label: "cloud",
            value: "local runtime".to_owned(),
            hints: vec![
                "team sync, dashboard, and uploaded review analysis: difflore cloud login"
                    .to_owned(),
            ],
            repair: None,
        },
    }
}

fn cloud_impact_hints(impact: &CloudImpactProbe) -> Vec<String> {
    let mut hints = Vec::new();
    if let Some(coverage) = &impact.coverage {
        let mut parts = vec![
            format_count("repo", coverage.repos),
            format_count("PR", coverage.prs),
            format_count("review comment", coverage.review_comments_indexed),
            format_count("file", coverage.files),
        ];
        if coverage.human_review_comments_indexed > 0 {
            parts.push(format_count(
                "human reviewer comment",
                coverage.human_review_comments_indexed,
            ));
        }
        if coverage.ai_reviewer_comments_indexed > 0 {
            parts.push(format_count(
                "AI reviewer signal",
                coverage.ai_reviewer_comments_indexed,
            ));
        }
        hints.push(format!("cloud corpus: {}", parts.join(" · ")));
    } else if let Some(error) = impact.coverage_error.as_deref() {
        hints.push(format!("cloud corpus unavailable: {}", short_detail(error)));
    }

    if let Some(fix) = &impact.fix_scorecard {
        let accepted_outcomes = fix
            .roi
            .as_ref()
            .map_or(0, |roi| roi.accepted_fix_outcomes_last30);
        if fix.last30.total > 0 {
            let accepted = format!(
                "{}/{} accepted edit proof{} in 30d",
                fix.last30.accepted,
                fix.last30.total,
                if fix.last30.total == 1 { "" } else { "s" },
            );
            let mut proof_parts = vec![accepted];
            if let Some(roi) = &fix.roi {
                if roi.source_evidence_items > 0 {
                    proof_parts.push(format_count(
                        "source evidence item",
                        roi.source_evidence_items,
                    ));
                }
                if roi.saved_review_minutes > 0 {
                    proof_parts.push(format!("{} saved review minutes", roi.saved_review_minutes));
                }
            }
            hints.push(format!("accepted-fix proof: {}", proof_parts.join(" · ")));
        } else if accepted_outcomes > 0 {
            let mut proof_parts = vec![format_count("accepted outcome", accepted_outcomes)];
            if let Some(roi) = &fix.roi {
                if roi.source_evidence_items > 0 {
                    proof_parts.push(format_count(
                        "source evidence item",
                        roi.source_evidence_items,
                    ));
                }
                let saved_minutes = roi
                    .saved_review_minutes_last30
                    .max(roi.saved_review_minutes)
                    .max(roi.modeled_review_minutes);
                if saved_minutes > 0 {
                    proof_parts.push(format!("{saved_minutes} saved review minutes"));
                }
            }
            hints.push(format!(
                "accepted outcome activity: {}",
                proof_parts.join(" · ")
            ));
        } else if let Some(roi) = &fix.roi
            && roi.source_evidence_items > 0
        {
            hints.push(format!(
                "source evidence: {}",
                format_count("item", roi.source_evidence_items)
            ));
        }
    } else if let Some(error) = impact.fix_scorecard_error.as_deref() {
        hints.push(format!(
            "accepted-fix proof unavailable: {}",
            short_detail(error)
        ));
    }

    hints
}

fn format_count(noun: &str, count: i64) -> String {
    format!("{count} {noun}{}", if count == 1 { "" } else { "s" })
}

fn embedder_row(probe: &EmbedderProbe) -> Row {
    let recent = recent_embedding_degradation(&probe.activity_tail);
    let row = embedder_row_from_kind(&probe.kind, &recent);
    if let Some(diag) = &probe.diagnostics
        && let Some(mut row) = embedder_row_from_diagnostics(diag)
    {
        if recent.any() {
            row.hints.insert(
                0,
                format!("recent embedding degradation: {}", recent.summary()),
            );
        }
        return row;
    }
    row
}

#[derive(Default)]
struct RecentEmbeddingDegradation {
    fallback_count: usize,
    persistent_fallback_count: usize,
    cap_count: usize,
    latest_reason: Option<String>,
}

impl RecentEmbeddingDegradation {
    const fn any(&self) -> bool {
        self.persistent_fallback_count > 0
            || self.cap_count > 0
            || self.fallback_count >= SUSTAINED_TRANSIENT_FALLBACK_THRESHOLD
    }

    fn summary(&self) -> String {
        let mut parts = Vec::new();
        if self.fallback_count > 0 {
            let reason = self
                .latest_reason
                .as_deref()
                .map_or_else(String::new, |reason| format!(" · latest: {reason}"));
            parts.push(format!("{} fallback{}", self.fallback_count, reason));
        }
        if self.cap_count > 0 {
            parts.push(format!("{} cap hit", self.cap_count));
        }
        parts.join(" · ")
    }
}

fn recent_embedding_degradation(
    events: &[difflore_core::observability::activity_stream::ActivityEvent],
) -> RecentEmbeddingDegradation {
    let mut summary = RecentEmbeddingDegradation::default();
    let now_ms = chrono::Utc::now().timestamp_millis();
    for event in events {
        match &event.payload {
            difflore_core::observability::activity_stream::ActivityPayload::EmbeddingFallback {
                reason,
            } if should_count_embedding_degradation(event.ts_ms, reason, now_ms) => {
                summary.fallback_count += 1;
                if is_persistent_embedding_degradation(reason) {
                    summary.persistent_fallback_count += 1;
                }
                if summary.latest_reason.is_none() {
                    summary.latest_reason = Some(reason.clone());
                }
            }
            difflore_core::observability::activity_stream::ActivityPayload::EmbedCapReached {
                ..
            } => {
                summary.cap_count += 1;
            }
            _ => {}
        }
    }
    summary
}

fn embedder_row_from_kind(
    kind: &difflore_core::context::embedding::ActiveEmbedderKind,
    recent: &RecentEmbeddingDegradation,
) -> Row {
    use difflore_core::context::embedding::ActiveEmbedderKind;
    match kind {
        ActiveEmbedderKind::Cloud => {
            if recent.any() {
                return Row {
                    severity: Severity::Optional,
                    status: Status::Warn,
                    label: "embedder",
                    value: "cloud-managed | semantic search configured | recent keyword fallback"
                        .to_owned(),
                    hints: vec![
                        format!("recent embedding degradation: {}", recent.summary()),
                        "run `difflore doctor --report` for the Memory pipeline breakdown"
                            .to_owned(),
                        "run `difflore cloud login` if credentials or scope may be stale"
                            .to_owned(),
                        "or `difflore embeddings setup` to switch to BYOK".to_owned(),
                    ],
                    repair: None,
                };
            }
            Row::ready_ok("embedder", "cloud-managed | semantic search".to_owned())
        }
        ActiveEmbedderKind::Byok { provider_host, .. } => {
            let host = provider_host.clone();
            if recent.any() {
                return Row {
                    severity: Severity::Optional,
                    status: Status::Warn,
                    label: "embedder",
                    value: format!(
                        "BYOK | {host} | semantic search configured | recent keyword fallback"
                    ),
                    hints: vec![
                        format!("recent embedding degradation: {}", recent.summary()),
                        "run `difflore doctor --report` for the Memory pipeline breakdown"
                            .to_owned(),
                        "check provider reachability and key limits".to_owned(),
                        "difflore embeddings setup".to_owned(),
                    ],
                    repair: None,
                };
            }
            Row::ready_ok("embedder", format!("BYOK | {host} | semantic search"))
        }
        ActiveEmbedderKind::Sha1 => Row {
            severity: Severity::Optional,
            status: Status::Warn,
            label: "embedder",
            value: "semantic recall: local keyword fallback".to_owned(),
            hints: vec![
                "recall still works, but match quality may be lower than semantic vectors"
                    .to_owned(),
                "managed semantic recall: difflore cloud login".to_owned(),
                "advanced/BYOK: difflore embeddings setup".to_owned(),
            ],
            repair: None,
        },
    }
}

fn embedder_row_from_diagnostics(
    diag: &difflore_core::context::EmbeddingDiagnostics,
) -> Option<Row> {
    if diag.is_local_agent_index() {
        return None;
    }
    if !diag.degraded {
        return None;
    }
    let reason = diag
        .degraded_reason
        .as_deref()
        .unwrap_or("embedding_profile_mismatch");
    let display_reason = match reason {
        "dimension_mismatch" | "profile_mismatch" | "embedding_profile_mismatch" => {
            "index needs a rebuild"
        }
        "provider_fallback" => "provider fallback detected",
        _ => "index is out of date",
    };
    let value = if diag.vector_lane_available {
        format!("semantic index needs attention | {display_reason}")
    } else {
        format!("semantic index is paused | {display_reason}")
    };
    let repair_hint = match reason {
        "provider_fallback" => {
            "run `difflore cloud login` or `difflore embeddings setup` to restore semantic search"
        }
        // dimension_mismatch / profile_mismatch / any other paused reason:
        _ => "run `difflore embeddings rebuild` to rebuild this repo's semantic index",
    };
    Some(Row {
        severity: Severity::Optional,
        status: Status::Warn,
        label: "embedder",
        value,
        hints: vec![
            "recall still works with file patterns and keyword matching".to_owned(),
            "difflore embeddings status".to_owned(),
            repair_hint.to_owned(),
        ],
        repair: None,
    })
}

fn git_hooks_row(state: &GitHookState) -> Row {
    // Pre-commit hooks are explicitly listed as optional in the
    // brief — useful, but not on the path to core value.
    match state {
        GitHookState::Installed => Row {
            severity: Severity::Optional,
            status: Status::Ok,
            label: "git hooks",
            value: "pre-commit installed".to_owned(),
            hints: vec![],
            repair: None,
        },
        GitHookState::OtherHook => Row {
            severity: Severity::Optional,
            status: Status::Warn,
            label: "git hooks",
            value: "pre-commit installed by another tool".to_owned(),
            hints: vec![],
            repair: None,
        },
        GitHookState::None => Row {
            severity: Severity::Optional,
            status: Status::Warn,
            label: "git hooks",
            value: "pre-commit not installed".to_owned(),
            hints: vec![
                "optional; run `difflore init` if you want pre-commit memory checks".to_owned(),
            ],
            repair: None,
        },
        GitHookState::Unreadable(msg) => Row {
            severity: Severity::Optional,
            status: Status::Warn,
            label: "git hooks",
            value: format!("pre-commit unreadable ({msg})"),
            hints: vec![
                "fix permissions or check if the hook is locked by another process".to_owned(),
            ],
            repair: None,
        },
        GitHookState::NotARepo => Row {
            severity: Severity::Optional,
            status: Status::Warn,
            label: "git hooks",
            value: "not a git repository".to_owned(),
            hints: vec![],
            repair: None,
        },
    }
}

fn daemon_row(probe: &DaemonProbe) -> Row {
    // A stale pid is cleaned in `probes`; if cleanup succeeded, report the
    // daemon as informationally off rather than warning forever.
    let cloud_pending = probe.cloud_pending.unwrap_or(0);
    let observation_pending = probe.observation_pending.unwrap_or(0);
    let spill_count = probe.hook_spill_count.unwrap_or(0);
    let backlog = cloud_pending + observation_pending + i64::try_from(spill_count).unwrap_or(0);
    let mut hints = Vec::new();
    if let Some(ms) = probe.heartbeat_age_ms {
        hints.push(format!("heartbeat: {}", age_label_ms(ms)));
    }
    if let Some(ms) = probe.last_drain_age_ms {
        let attempted = probe.last_attempted.unwrap_or(0);
        let confirmed = probe.last_confirmed.unwrap_or(0);
        hints.push(format!(
            "last drain: {} ({attempted} attempted, {confirmed} confirmed)",
            age_label_ms(ms)
        ));
    }
    if backlog > 0 {
        hints.push(
            "queued work drains in the background; run `difflore cloud sync --include-observations --include-candidates --include-telemetry` for an immediate pass"
                .to_owned(),
        );
    }

    match probe.state {
        DaemonProbeState::Running => Row {
            severity: Severity::Optional,
            status: Status::Ok,
            label: "daemon",
            value: format!(
                "running | cloud={cloud_pending} observation={observation_pending} spill={spill_count}"
            ),
            hints,
            repair: None,
        },
        DaemonProbeState::StaleCleanupFailed => Row {
            // Only reached when the cleanup attempt failed (locked
            // file etc.). Still optional surface, but flagged as Err
            // because the on-disk state needs a hand.
            severity: Severity::Optional,
            status: Status::Err,
            label: "daemon",
            value: "stale pid (cleanup failed)".to_owned(),
            hints: {
                let mut h = hints;
                h.push("remove the stale DiffLore daemon pid file or rerun the command".to_owned());
                h
            },
            repair: None,
        },
        DaemonProbeState::NotRunning => Row {
            // No queued work means the daemon can remain off. With backlog or
            // spill files present, flag it so the user sees why uploads lag.
            severity: Severity::Optional,
            status: if backlog > 0 {
                Status::Warn
            } else {
                Status::Ok
            },
            label: "daemon",
            value: if backlog > 0 {
                format!(
                    "off | queued cloud={cloud_pending} observation={observation_pending} spill={spill_count}"
                )
            } else {
                "off (no queued uploads)".to_owned()
            },
            hints,
            repair: None,
        },
    }
}

fn render_rows(rows: &[Row], snapshot_block: &str) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{} {}\n",
        style::emerald(style::sym::TIP),
        style::ok("DiffLore doctor"),
    ));
    out.push_str(&format!("{}\n", style::pewter(RULE)));

    let has_blocker = rows.iter().any(|row| row.severity == Severity::Blocker);
    let mut wrote_any_section = false;
    for section in [Severity::Blocker, Severity::Ready, Severity::Optional] {
        let section_rows: Vec<&Row> = rows.iter().filter(|r| r.severity == section).collect();
        if section_rows.is_empty() {
            continue;
        }
        if wrote_any_section {
            out.push('\n');
        }
        wrote_any_section = true;
        let heading = if has_blocker && section == Severity::Ready {
            "Ready pieces"
        } else {
            section.heading()
        };
        out.push_str(&format!("  {}\n", style::pewter(heading).bold()));
        for row in section_rows {
            render_row_into(&mut out, row);
        }
    }
    if !snapshot_block.is_empty() {
        out.push_str(snapshot_block);
    }
    out.push_str(&format!("{}\n", style::pewter(RULE)));

    // Footer: the highest-priority blocker's repair command, or the canonical
    // "now what?" command when nothing's blocking.
    let next_blocker = rows
        .iter()
        .find(|r| r.severity == Severity::Blocker && r.repair.is_some());
    let next_cmd = next_blocker
        .and_then(|r| r.repair.as_deref())
        .unwrap_or("difflore recall --diff");
    if wrote_any_section {
        out.push('\n');
    }
    out.push_str(&format!("  {}\n", style::pewter("Next").bold()));
    out.push_str(&format!("  {}\n", style::cmd(next_cmd)));
    out
}

fn render_row_into(out: &mut String, row: &Row) {
    let glyph = match row.status {
        Status::Ok => style::emerald(style::sym::OK).to_string(),
        Status::Warn => style::amber(style::sym::WARN).to_string(),
        Status::Err => style::danger(style::sym::ERR).to_string(),
    };
    // Value column starts after "  " (2) + glyph (1) + " " (1) + label (LABEL_W)
    // + " " (1). Fold long values under that column instead of at column 0.
    const VALUE_COL: usize = LABEL_W + 5;
    out.push_str(&format!(
        "  {} {:<width$} {}\n",
        glyph,
        row.label,
        style::wrap_after_column(&row.value, VALUE_COL),
        width = LABEL_W
    ));
    for hint in &row.hints {
        // Hint text starts after "  " (2) + pad (LABEL_W+4) + tip (1) + " " (1).
        const HINT_COL: usize = LABEL_W + 8;
        out.push_str(&format!(
            "  {space:<pad$}{tip} {hint}\n",
            space = "",
            // 2 leading + 1 glyph + 1 space (= 4) before label area.
            pad = LABEL_W + 4,
            tip = style::emerald(style::sym::TIP),
            hint = style::pewter(&style::wrap_after_column(hint, HINT_COL)),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CloudImpactProbe, CloudProbe, EmbedderProbe, GateCaptureProbe, ProjectDbProbe, Severity,
        Status, cloud_row, embedder_row, embedder_row_from_diagnostics, embedder_row_from_kind,
        gate_capture_row, project_db_row, recall_trace_row, recent_embedding_degradation,
        secrets_row,
    };
    use difflore_core::context::EmbeddingDiagnostics;
    use difflore_core::context::embedding::ActiveEmbedderKind;
    use difflore_core::contract::dto::{
        ImpactCoverageDto, ImpactFixScorecardDto, ImpactFixWindowDto, ImpactRoiDto,
    };
    use difflore_core::infra::crypto::{KeyseedStatus, MasterKeyStorageStatus};
    use difflore_core::observability::activity_stream::{ActivityEvent, ActivityPayload};
    use difflore_core::observability::injection_log::InjectionPathSummary;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn assert_ready_ok(row: &super::Row, label: &'static str) {
        assert_eq!(row.label, label);
        assert!(matches!(row.severity, Severity::Ready));
        assert!(matches!(row.status, Status::Ok));
    }

    fn event(payload: ActivityPayload) -> ActivityEvent {
        ActivityEvent {
            ts_ms: chrono::Utc::now().timestamp_millis(),
            payload,
        }
    }

    fn old_event(payload: ActivityPayload) -> ActivityEvent {
        ActivityEvent {
            ts_ms: chrono::Utc::now().timestamp_millis()
                - super::super::embedding_degradation::EMBEDDING_DEGRADATION_WINDOW_MS
                - 1,
            payload,
        }
    }

    #[test]
    fn project_db_row_labels_imported_prs_as_local() {
        let row = project_db_row(&ProjectDbProbe {
            db_available: true,
            total_rules: 41,
            prs_imported: 0,
            repo_full_name: Some("warpengine-github/viggle-web".to_owned()),
            review_source_repo_full_name: None,
            scoped_active_rules: 31,
            review_source_active_rules: 0,
        });

        assert_ready_ok(&row, "project db");
        assert!(row.value.contains("0 local PRs imported"), "{}", row.value);
        assert!(!row.value.contains("0 PRs imported"), "{}", row.value);
    }

    #[test]
    fn cloud_row_surfaces_cloud_corpus_and_accepted_outcome_activity() {
        let row = cloud_row(&CloudProbe::LoggedIn {
            plan: "team".to_owned(),
            team_name: Some("islizeqiang max-perm team".to_owned()),
            impact: CloudImpactProbe {
                coverage: Some(ImpactCoverageDto {
                    repos: 7,
                    prs: 237,
                    files: 195,
                    review_comments_indexed: 484,
                    ai_reviewer_comments_indexed: 0,
                    human_review_comments_indexed: 484,
                }),
                fix_scorecard: Some(ImpactFixScorecardDto {
                    last30: ImpactFixWindowDto {
                        accepted: 0,
                        total: 0,
                    },
                    prior30: ImpactFixWindowDto {
                        accepted: 0,
                        total: 0,
                    },
                    trend_pct: None,
                    roi: Some(ImpactRoiDto {
                        accepted_fixes_last30: 0,
                        accepted_fix_outcomes_last30: 58,
                        repeat_comment_signals: 58,
                        review_comments_avoided: 0,
                        modeled_review_minutes: 232,
                        saved_review_minutes: 232,
                        saved_review_minutes_last30: 232,
                        repeat_feedback_reduced: 0,
                        source_evidence_items: 505,
                        agent_rules_served_last30: 0,
                        agent_rules_fired_last30: 61,
                        agent_rules_cited_last30: 2,
                    }),
                }),
                coverage_error: None,
                fix_scorecard_error: None,
            },
        });

        assert_ready_ok(&row, "cloud");
        assert!(row.value.contains("237 cloud PRs indexed"), "{}", row.value);
        let hints = row.hints.join("\n");
        assert!(hints.contains("484 review comments"), "{hints}");
        assert!(hints.contains("505 source evidence items"), "{hints}");
        assert!(hints.contains("58 accepted outcomes"), "{hints}");
        assert!(hints.contains("accepted outcome activity"), "{hints}");
        assert!(hints.contains("difflore cloud impact"), "{hints}");
    }

    fn no_recent() -> super::RecentEmbeddingDegradation {
        super::RecentEmbeddingDegradation::default()
    }

    fn recall_trace_summary(reasons: &[(&str, usize)]) -> InjectionPathSummary {
        let mut dropped_by_reason = BTreeMap::new();
        for (reason, count) in reasons {
            dropped_by_reason.insert((*reason).to_owned(), *count);
        }
        InjectionPathSummary {
            count_24h: reasons.iter().map(|(_, count)| *count).sum(),
            dropped_by_reason,
            ..InjectionPathSummary::default()
        }
    }

    #[test]
    fn recall_trace_row_keeps_benign_drop_reasons_green_and_visible() {
        let row = recall_trace_row(&recall_trace_summary(&[
            ("recent_duplicate", 2),
            ("pre_read_disabled", 1),
            ("non_mutating_tool", 1),
            ("retrieval_empty", 3),
            ("no_repo_scope", 1),
        ]));

        assert_eq!(row.label, "recall trace");
        assert!(matches!(row.severity, Severity::Optional));
        assert!(matches!(row.status, Status::Ok));
        assert!(
            row.hints
                .iter()
                .any(|hint| hint.contains("drop reasons:") && hint.contains("retrieval_empty=3")),
            "hints: {:?}",
            row.hints
        );
    }

    #[test]
    fn recall_trace_row_warns_for_abnormal_or_unknown_drop_reasons() {
        for reason in ["retrieval_error", "parse_error", "unknown", "future_reason"] {
            let row = recall_trace_row(&recall_trace_summary(&[(reason, 1)]));
            assert!(
                matches!(row.status, Status::Warn),
                "{reason} should warn; row value={}",
                row.value
            );
        }
    }

    #[test]
    fn gate_capture_row_surfaces_paused_capture_as_optional_warning() {
        let row = gate_capture_row(&GateCaptureProbe {
            status: crate::session_mine::trigger::GateCaptureStatus::Paused {
                since_ts: 100,
                reason: "codex unauthorized".to_owned(),
                retry_after_ms: 60_000,
            },
        });

        assert_eq!(row.label, "capture");
        assert!(matches!(row.severity, Severity::Optional));
        assert!(matches!(row.status, Status::Warn));
        assert!(row.value.contains("capture paused"));
        assert!(row.value.contains("recall unaffected"));
        assert!(
            row.hints
                .iter()
                .any(|hint| hint.contains("only new session learning capture paused"))
        );
    }

    #[test]
    fn secrets_row_reports_keyring_ready() {
        let row = secrets_row(&MasterKeyStorageStatus::KeyringReady);
        assert_ready_ok(&row, "secrets");
        assert!(row.value.contains("OS keyring"), "value: {}", row.value);
    }

    #[test]
    fn secrets_row_reports_debug_master_key_file() {
        let row = secrets_row(&MasterKeyStorageStatus::DebugFileOverride {
            path: PathBuf::from("/tmp/difflore/master-key"),
        });

        assert_ready_ok(&row, "secrets");
        assert!(
            row.value.contains("debug master-key file"),
            "value: {}",
            row.value
        );
        assert!(
            row.value.contains("/tmp/difflore/master-key"),
            "value: {}",
            row.value
        );
    }

    #[test]
    fn secrets_row_warns_for_local_fallback_with_keyseed() {
        let row = secrets_row(&MasterKeyStorageStatus::LocalFallback {
            keyring_error: "secret service unavailable".to_owned(),
            keyseed: KeyseedStatus::Present {
                path: PathBuf::from("/tmp/difflore/keyseed"),
                permissions_ok: Some(true),
            },
        });

        assert_eq!(row.label, "secrets");
        assert!(matches!(row.severity, Severity::Optional));
        assert!(matches!(row.status, Status::Warn));
        assert!(
            row.value.contains("keyseed present"),
            "value: {}",
            row.value
        );
        assert!(
            row.hints.iter().any(|hint| hint.contains("32-byte random")),
            "hints: {:?}",
            row.hints
        );
        assert!(
            row.hints
                .iter()
                .any(|hint| hint.contains("/tmp/difflore/keyseed")),
            "hints: {:?}",
            row.hints
        );
    }

    #[test]
    fn secrets_row_errors_for_invalid_local_keyseed() {
        let row = secrets_row(&MasterKeyStorageStatus::LocalFallback {
            keyring_error: "secret service unavailable".to_owned(),
            keyseed: KeyseedStatus::Invalid {
                path: PathBuf::from("/tmp/difflore/keyseed"),
                error: "expected 64 lowercase hex characters".to_owned(),
                permissions_ok: Some(true),
            },
        });

        assert_eq!(row.label, "secrets");
        assert!(matches!(row.severity, Severity::Optional));
        assert!(matches!(row.status, Status::Err));
        assert!(
            row.value.contains("keyseed invalid"),
            "value: {}",
            row.value
        );
    }

    #[test]
    fn embedder_row_cloud_managed() {
        let kind = ActiveEmbedderKind::Cloud;
        let row = embedder_row_from_kind(&kind, &no_recent());
        assert_ready_ok(&row, "embedder");
        assert!(row.value.contains("cloud-managed"), "value: {}", row.value);
    }

    #[test]
    fn embedder_row_warns_when_cloud_recently_fell_back() {
        let kind = ActiveEmbedderKind::Cloud;
        let recent = recent_embedding_degradation(&[
            event(ActivityPayload::EmbeddingFallback {
                reason: "network".into(),
            }),
            event(ActivityPayload::EmbedCapReached {
                cap: 200,
                used: 201,
            }),
        ]);
        let row = embedder_row_from_kind(&kind, &recent);
        assert_eq!(row.label, "embedder");
        assert!(matches!(row.severity, Severity::Optional));
        assert!(matches!(row.status, Status::Warn));
        assert!(
            row.value.contains("recent keyword fallback"),
            "value: {}",
            row.value
        );
        assert!(
            row.hints
                .iter()
                .any(|hint| hint.contains("network") && hint.contains("cap hit")),
            "hints: {:?}",
            row.hints
        );
    }

    #[test]
    fn embedder_row_ignores_historical_transient_fallback() {
        let kind = ActiveEmbedderKind::Cloud;
        let recent =
            recent_embedding_degradation(&[old_event(ActivityPayload::EmbeddingFallback {
                reason: "timeout".into(),
            })]);
        let row = embedder_row_from_kind(&kind, &recent);

        assert_ready_ok(&row, "embedder");
        assert!(
            !row.value.contains("recent keyword fallback"),
            "value: {}",
            row.value
        );
    }

    #[test]
    fn embedder_row_ignores_brief_transient_fallbacks_below_threshold() {
        let kind = ActiveEmbedderKind::Cloud;
        let recent = recent_embedding_degradation(&[
            event(ActivityPayload::EmbeddingFallback {
                reason: "timeout".into(),
            }),
            event(ActivityPayload::EmbeddingFallback {
                reason: "network".into(),
            }),
            event(ActivityPayload::EmbeddingFallback {
                reason: "timeout".into(),
            }),
        ]);
        let row = embedder_row_from_kind(&kind, &recent);

        assert_ready_ok(&row, "embedder");
        assert!(
            !row.value.contains("recent keyword fallback"),
            "value: {}",
            row.value
        );
    }

    #[test]
    fn embedder_row_warns_on_sustained_transient_fallbacks() {
        let kind = ActiveEmbedderKind::Cloud;
        let events = (0..super::SUSTAINED_TRANSIENT_FALLBACK_THRESHOLD)
            .map(|_| {
                event(ActivityPayload::EmbeddingFallback {
                    reason: "timeout".into(),
                })
            })
            .collect::<Vec<_>>();
        let recent = recent_embedding_degradation(&events);
        let row = embedder_row_from_kind(&kind, &recent);

        assert!(matches!(row.severity, Severity::Optional));
        assert!(matches!(row.status, Status::Warn));
        assert!(
            row.value.contains("recent keyword fallback"),
            "value: {}",
            row.value
        );
    }

    #[test]
    fn embedder_row_byok_default_host() {
        let kind = ActiveEmbedderKind::Byok {
            provider_host: "api.openai.com".to_owned(),
            model: "text-embedding-3-small".to_owned(),
            dim: 1536,
        };
        let row = embedder_row_from_kind(&kind, &no_recent());
        assert_ready_ok(&row, "embedder");
        assert!(
            row.value.starts_with("BYOK | api.openai.com"),
            "value: {}",
            row.value
        );
    }

    #[test]
    fn embedder_row_warns_when_byok_recently_fell_back() {
        let kind = ActiveEmbedderKind::Byok {
            provider_host: "embed.example.com".to_owned(),
            model: "text-embedding-3-small".to_owned(),
            dim: 1536,
        };
        let events = (0..super::SUSTAINED_TRANSIENT_FALLBACK_THRESHOLD)
            .map(|_| {
                event(ActivityPayload::EmbeddingFallback {
                    reason: "timeout".into(),
                })
            })
            .collect::<Vec<_>>();
        let recent = recent_embedding_degradation(&events);
        let row = embedder_row_from_kind(&kind, &recent);
        assert!(matches!(row.severity, Severity::Optional));
        assert!(matches!(row.status, Status::Warn));
        assert!(
            row.value
                .contains("BYOK | embed.example.com | semantic search configured"),
            "value: {}",
            row.value
        );
        assert!(
            row.hints.iter().any(|hint| hint.contains("timeout")),
            "hints: {:?}",
            row.hints
        );
    }

    #[test]
    fn embedder_row_warns_on_static_embedding_profile_degradation() {
        let row = embedder_row_from_diagnostics(&EmbeddingDiagnostics {
            active_profile: "sha1:local:128".to_owned(),
            index_profile: Some("cloud:text-embedding-3-small:1536".to_owned()),
            profile_match: false,
            degraded: true,
            degraded_reason: Some("provider_fallback".to_owned()),
            vector_lane_available: false,
        })
        .expect("diagnostic row");

        assert_eq!(row.label, "embedder");
        assert!(matches!(row.severity, Severity::Optional));
        assert!(matches!(row.status, Status::Warn));
        assert!(
            row.value.contains("semantic index is paused"),
            "{}",
            row.value
        );
        assert!(
            row.hints
                .iter()
                .any(|hint| hint.contains("recall still works")),
            "{:?}",
            row.hints
        );
        assert!(
            row.hints
                .iter()
                .any(|hint| hint.contains("difflore embeddings status")),
            "{:?}",
            row.hints
        );
    }

    #[test]
    fn embedder_row_keeps_recent_degradation_when_diagnostics_warn() {
        let probe = EmbedderProbe {
            kind: ActiveEmbedderKind::Cloud,
            activity_tail: vec![event(ActivityPayload::EmbeddingFallback {
                reason: "forbidden".into(),
            })],
            diagnostics: Some(EmbeddingDiagnostics {
                active_profile: "cloud:text-embedding-3-small:1536".to_owned(),
                index_profile: Some("sha1:local:128".to_owned()),
                profile_match: false,
                degraded: true,
                degraded_reason: Some("profile_mismatch".to_owned()),
                vector_lane_available: true,
            }),
        };

        let row = embedder_row(&probe);

        assert!(matches!(row.status, Status::Warn));
        assert!(
            row.hints
                .iter()
                .any(|hint| hint.contains("recent embedding degradation: 1 fallback")),
            "{:?}",
            row.hints
        );
        assert!(
            row.hints
                .iter()
                .any(|hint| hint.contains("difflore embeddings status")),
            "{:?}",
            row.hints
        );
    }

    #[test]
    fn embedder_row_surfaces_force_rebuild_for_profile_mismatch() {
        // A profile/dimension mismatch needs force-rebuild: the lazy
        // `recall --diff` refresh is freshness-gated and can skip a same-count
        // inconsistency, so doctor must point at `difflore embeddings rebuild`.
        let row = embedder_row_from_diagnostics(&EmbeddingDiagnostics {
            active_profile: "cloud:text-embedding-3-small:1536".to_owned(),
            index_profile: Some("sha1:local:128".to_owned()),
            profile_match: false,
            degraded: true,
            degraded_reason: Some("profile_mismatch".to_owned()),
            vector_lane_available: true,
        })
        .expect("diagnostic row");
        assert!(
            row.hints
                .iter()
                .any(|hint| hint.contains("difflore embeddings rebuild")),
            "{:?}",
            row.hints
        );
    }

    #[test]
    fn embedder_row_does_not_warn_on_expected_local_agent_index() {
        let row = embedder_row_from_diagnostics(&EmbeddingDiagnostics {
            active_profile: "cloud:managed".to_owned(),
            index_profile: Some("sha1:local:128".to_owned()),
            profile_match: false,
            degraded: false,
            degraded_reason: Some("local_agent_index".to_owned()),
            vector_lane_available: true,
        });

        assert!(
            row.is_none(),
            "local-agent MCP/hook index must not override the healthy embedder row"
        );
    }

    #[test]
    fn embedder_row_keeps_healthy_static_embedding_profile_green() {
        let row = embedder_row_from_diagnostics(&EmbeddingDiagnostics {
            active_profile: "cloud:text-embedding-3-small:1536".to_owned(),
            index_profile: Some("cloud:text-embedding-3-small:1536".to_owned()),
            profile_match: true,
            degraded: false,
            degraded_reason: None,
            vector_lane_available: true,
        });

        assert!(row.is_none());
    }

    #[test]
    fn embedder_row_byok_custom_host() {
        let kind = ActiveEmbedderKind::Byok {
            provider_host: "embed.example.com".to_owned(),
            model: "text-embedding-3-small".to_owned(),
            dim: 1536,
        };
        let row = embedder_row_from_kind(&kind, &no_recent());
        assert!(
            row.value.contains("BYOK | embed.example.com"),
            "value: {}",
            row.value
        );
    }

    #[test]
    fn embedder_row_warns_with_local_fallback_when_sha1() {
        let kind = ActiveEmbedderKind::Sha1;
        let row = embedder_row_from_kind(&kind, &no_recent());
        assert_eq!(row.label, "embedder");
        assert!(matches!(row.severity, Severity::Optional));
        assert!(matches!(row.status, Status::Warn));
        assert!(
            row.value.contains("local keyword fallback"),
            "value: {}",
            row.value
        );
        assert!(
            row.hints.iter().any(|h| h.contains("match quality")),
            "hints: {:?}",
            row.hints
        );
        // Both recovery hints surface in the order the row renders them so
        // the user sees the cloud-login path first, then the BYOK escape.
        assert!(row.hints.iter().any(|h| h.contains("difflore cloud login")));
        assert!(
            row.hints
                .iter()
                .any(|h| h.contains("difflore embeddings setup"))
        );
    }
}
