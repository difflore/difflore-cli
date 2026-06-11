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

use super::memory_snapshot;
use super::probes::{
    self, CloudProbe, DaemonProbe, EmbedderProbe, Findings, GitHookState, ProjectDbProbe,
    ProviderProbe,
};
use crate::installer;
use crate::style;

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
        project_db_row(&findings.project_db),
        mcp_row(&findings.mcp),
        provider_row(&findings.provider),
        cloud_row(&findings.cloud),
        embedder_row(&findings.embedder),
        git_hooks_row(&findings.git_hooks),
        daemon_row(&findings.daemon),
    ];
    // "What we've learned" preview — renders to "" when the snapshot is empty
    // (fresh install / no ready repo memory).
    let snapshot_block = memory_snapshot::render(&findings.memory_snapshot);
    render_rows(&rows, &snapshot_block)
}

fn binary_row(version: &str) -> Row {
    // Always Ready: if the binary couldn't run, we wouldn't be here.
    Row::ready_ok("binary", format!("v{version}"))
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
                "{} · {} PR{} imported",
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
            format!("{total_rules} memories on this machine | no GitHub repo detected"),
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
                "agents can recall team memory once wired; CLI commands work either way".to_owned(),
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
                "difflore doctor --report   (full diagnostic for the issue tracker)".to_owned(),
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
        CloudProbe::LoggedIn { plan, team_name } => {
            let suffix = match team_name.as_deref() {
                Some(team) => format!(" | team: {team}"),
                None => String::new(),
            };
            // Free notes the managed-embedding cap but prints no exact number
            // (the default doctor row has no fresh cap/usage cache; cap hits
            // are surfaced by the embedder row from telemetry).
            let embedding_suffix = if plan.eq_ignore_ascii_case("free") {
                " | managed embedding cap | upgrade or embeddings setup"
            } else {
                " | unlimited embedding"
            };
            Row::ready_ok(
                "cloud",
                format!("logged in | plan: {plan}{suffix}{embedding_suffix}"),
            )
        }
        CloudProbe::NotLoggedIn => Row {
            severity: Severity::Optional,
            status: Status::Warn,
            label: "cloud",
            value: "not logged in".to_owned(),
            hints: vec![
                "log in to enable team sync, dashboard, and uploaded review analysis".to_owned(),
                "difflore cloud login".to_owned(),
            ],
            repair: None,
        },
    }
}

fn embedder_row(probe: &EmbedderProbe) -> Row {
    let recent = recent_embedding_degradation(&probe.activity_tail);
    let row = embedder_row_from_kind(&probe.kind, &recent);
    if let Some(diag) = &probe.diagnostics
        && let Some(row) = embedder_row_from_diagnostics(diag)
    {
        return row;
    }
    row
}

#[derive(Default)]
struct RecentEmbeddingDegradation {
    fallback_count: usize,
    cap_count: usize,
    latest_reason: Option<String>,
}

impl RecentEmbeddingDegradation {
    const fn any(&self) -> bool {
        self.fallback_count > 0 || self.cap_count > 0
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
    for event in events {
        match &event.payload {
            difflore_core::observability::activity_stream::ActivityPayload::EmbeddingFallback {
                reason,
            } => {
                summary.fallback_count += 1;
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
        ActiveEmbedderKind::Cloud { .. } => {
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
                "free semantic recall: difflore cloud login".to_owned(),
                "advanced/BYOK: difflore embeddings setup".to_owned(),
            ],
            repair: None,
        },
    }
}

fn embedder_row_from_diagnostics(
    diag: &difflore_core::context::EmbeddingDiagnostics,
) -> Option<Row> {
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
    match probe {
        DaemonProbe::Running => Row {
            severity: Severity::Optional,
            status: Status::Ok,
            label: "daemon",
            value: "running".to_owned(),
            hints: vec![],
            repair: None,
        },
        DaemonProbe::StaleCleanupFailed => Row {
            // Only reached when the cleanup attempt failed (locked
            // file etc.). Still optional surface, but flagged as Err
            // because the on-disk state needs a hand.
            severity: Severity::Optional,
            status: Status::Err,
            label: "daemon",
            value: "stale pid (cleanup failed)".to_owned(),
            hints: vec![
                "remove the stale DiffLore daemon pid file or rerun the command".to_owned(),
            ],
            repair: None,
        },
        DaemonProbe::NotRunning => Row {
            // Daemon is optional; CLI/MCP invocations drain the SQLite outbox.
            // Backlogs are handled by the slow-drain warning instead.
            severity: Severity::Optional,
            status: Status::Ok,
            label: "daemon",
            value: "off (not needed)".to_owned(),
            hints: vec![],
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
    out.push_str(&format!(
        "  {} {:<width$} {}\n",
        glyph,
        row.label,
        row.value,
        width = LABEL_W
    ));
    for hint in &row.hints {
        out.push_str(&format!(
            "  {space:<pad$}{tip} {hint}\n",
            space = "",
            // 2 leading + 1 glyph + 1 space (= 4) before label area.
            pad = LABEL_W + 4,
            tip = style::emerald(style::sym::TIP),
            hint = style::pewter(hint),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Severity, Status, embedder_row_from_diagnostics, embedder_row_from_kind,
        recent_embedding_degradation,
    };
    use difflore_core::context::EmbeddingDiagnostics;
    use difflore_core::context::embedding::ActiveEmbedderKind;
    use difflore_core::observability::activity_stream::{ActivityEvent, ActivityPayload};

    fn assert_ready_ok(row: &super::Row, label: &'static str) {
        assert_eq!(row.label, label);
        assert!(matches!(row.severity, Severity::Ready));
        assert!(matches!(row.status, Status::Ok));
    }

    fn event(payload: ActivityPayload) -> ActivityEvent {
        ActivityEvent { ts_ms: 1, payload }
    }

    fn no_recent() -> super::RecentEmbeddingDegradation {
        super::RecentEmbeddingDegradation::default()
    }

    #[test]
    fn embedder_row_cloud_managed() {
        let kind = ActiveEmbedderKind::Cloud {
            model: "text-embedding-3-small".to_owned(),
            dim: 1536,
        };
        let row = embedder_row_from_kind(&kind, &no_recent());
        assert_ready_ok(&row, "embedder");
        assert!(row.value.contains("cloud-managed"), "value: {}", row.value);
    }

    #[test]
    fn embedder_row_warns_when_cloud_recently_fell_back() {
        let kind = ActiveEmbedderKind::Cloud {
            model: "text-embedding-3-small".to_owned(),
            dim: 1536,
        };
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
        let recent = recent_embedding_degradation(&[event(ActivityPayload::EmbeddingFallback {
            reason: "timeout".into(),
        })]);
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
