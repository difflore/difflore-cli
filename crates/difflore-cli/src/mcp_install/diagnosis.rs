use std::collections::BTreeSet;

use super::{
    CanonicalRecordState, CanonicalRecordStatus, InstallState, McpStatusDiagnosis,
    McpStatusSnapshot, RuntimeProbeState, common::canonical_target_key,
};

pub(super) fn diagnose_status_snapshot(snapshot: &McpStatusSnapshot) -> McpStatusDiagnosis {
    let runtime_state = snapshot.runtime_probe.as_ref().map(|probe| probe.state);
    let affected_clients = diagnose_affected_clients(snapshot);
    let installed_clients = installed_client_names(snapshot);
    let conflict_count = snapshot
        .clients
        .iter()
        .filter(|client| matches!(client.state, InstallState::Conflict))
        .count();
    let drift_count = snapshot
        .clients
        .iter()
        .filter(|client| {
            client.detected
                && matches!(
                    client.state,
                    InstallState::NotInstalled | InstallState::Unknown
                )
        })
        .count();
    let record_ok = matches!(
        snapshot.canonical_record.state,
        CanonicalRecordState::Present
    );

    match runtime_state {
        Some(RuntimeProbeState::Ok) if record_ok && conflict_count == 0 && drift_count == 0 => {
            let mut actions = vec![restart_clients_action(
                "installed client(s) that still report `Transport closed`",
                &installed_clients,
            )];
            actions.extend(client_reload_actions(&installed_clients));
            actions.push("If the error persists, compare that client's MCP entry with `difflore agents status --json`; the runtime self-check already proved the DiffLore server starts, lists tools, and completes a search_rules tool call.".to_owned());
            build_diagnosis(
                "DiffLore MCP server starts, serves tools, and completes a search_rules recall probe; installed client wiring matches the current probe snapshot.",
                actions,
                affected_clients,
            )
        }
        Some(RuntimeProbeState::Ok) => {
            let mut actions = vec![
                "Run `difflore agents install` to refresh MCP entries and hooks.".to_owned(),
                restart_clients_action("affected client(s)", &affected_clients),
            ];
            actions.extend(client_reload_actions(&affected_clients));
            actions.push("If a refreshed client still reports `Transport closed`, compare that client's config in `difflore agents status --json`; the stdio self-check already proved the DiffLore server can serve tools and complete a search_rules call.".to_owned());
            build_diagnosis(
                "DiffLore MCP server is healthy and can complete a recall tool call; remaining MCP issues are install-record or client-wiring drift, not a broken memory server.",
                actions,
                affected_clients,
            )
        }
        Some(RuntimeProbeState::Timeout) => build_diagnosis(
            "DiffLore MCP server started but did not answer the stdio self-check before the timeout.",
            vec![
                "Run `difflore agents status --json` for stderr/details.".to_owned(),
                "After checking provider/network startup latency, run `difflore agents install` and restart affected clients.".to_owned(),
            ],
            affected_clients,
        ),
        Some(RuntimeProbeState::Failed) => build_diagnosis(
            "DiffLore MCP server failed the stdio self-check; clients will not receive review-memory tools until the runtime starts cleanly.",
            vec![
                "Run `difflore agents status --json` for stderr/details.".to_owned(),
                "Rebuild or upgrade the binary before reinstalling agents.".to_owned(),
            ],
            affected_clients,
        ),
        None => build_diagnosis(
            "MCP install snapshot collected without a runtime self-check.",
            vec![
                "Run `difflore agents status` or `difflore doctor --report` to verify the MCP server can actually serve tools.".to_owned(),
            ],
            affected_clients,
        ),
    }
}

fn build_diagnosis(
    summary: &str,
    actions: Vec<String>,
    affected_clients: Vec<String>,
) -> McpStatusDiagnosis {
    McpStatusDiagnosis {
        summary: summary.to_owned(),
        next_step: actions.first().cloned().unwrap_or_default(),
        affected_clients,
        actions,
    }
}

fn diagnose_affected_clients(snapshot: &McpStatusSnapshot) -> Vec<String> {
    install_repair_targets_for_snapshot(snapshot)
}

fn installed_client_names(snapshot: &McpStatusSnapshot) -> Vec<String> {
    snapshot
        .clients
        .iter()
        .filter(|client| client.detected && matches!(client.state, InstallState::Installed))
        .map(|client| client.name.to_owned())
        .collect()
}

pub(super) fn install_repair_targets_for_snapshot(snapshot: &McpStatusSnapshot) -> Vec<String> {
    let mut clients = BTreeSet::new();
    for client in &snapshot.clients {
        if client.detected
            && matches!(
                client.state,
                InstallState::Conflict | InstallState::NotInstalled | InstallState::Unknown
            )
        {
            clients.insert(client.name.to_owned());
        }
    }

    if !matches!(
        snapshot.canonical_record.state,
        CanonicalRecordState::Present
    ) {
        for surface in canonical_record_drift_surfaces(&snapshot.canonical_record) {
            // Canonical-record drift (e.g. a hooks surface present on disk but
            // not captured in `~/.difflore/mcp.json`) still needs the client's
            // wiring refreshed even when its MCP entry already probes as
            // Installed -- the hook surface is a separate signal from the MCP
            // entry. Always list the client so the idempotent installer reruns.
            clients.insert(client_name_for_surface(&surface).to_owned());
        }
    }

    clients.into_iter().collect()
}

fn canonical_record_drift_surfaces(record: &CanonicalRecordStatus) -> Vec<String> {
    let recorded: BTreeSet<String> = record
        .recorded_targets
        .iter()
        .map(|target| canonical_target_key(target))
        .collect();
    let actual: BTreeSet<String> = record
        .actual_targets
        .iter()
        .map(|target| canonical_target_key(target))
        .collect();

    let mut out = Vec::new();
    for target in &record.actual_targets {
        if !recorded.contains(&canonical_target_key(target)) {
            out.push(target.clone());
        }
    }
    for target in &record.recorded_targets {
        if !actual.contains(&canonical_target_key(target)) {
            out.push(target.clone());
        }
    }
    out
}

/// Map a surface name to its display client (e.g. `Cursor hooks` → `Cursor`).
/// The mapping now lives on the `AGENTS` registry
/// ([`super::registry::client_name_for_surface`]); this wrapper keeps the
/// `diagnosis::client_name_for_surface` call site / import path stable for
/// every existing caller and test.
pub(super) fn client_name_for_surface(surface: &str) -> &'static str {
    super::registry::client_name_for_surface(surface)
}

fn restart_clients_action(scope: &str, clients: &[String]) -> String {
    if clients.is_empty() {
        return format!("Restart/reload {scope} so they pick up MCP config.");
    }
    format!("Restart/reload {scope}: {}.", clients.join(", "))
}

fn client_reload_actions(clients: &[String]) -> Vec<String> {
    clients
        .iter()
        .map(|client| {
            let instruction = match client.as_str() {
                "Claude Code" => "restart Claude Code or start a fresh `claude` session so it reloads `~/.claude/settings.json`",
                "Codex" => "restart the Codex app/session so it reloads the `difflore` MCP entry",
                "Cursor" => "run `Developer: Reload Window` from the command palette, or restart Cursor",
                "Gemini CLI" => "start a fresh `gemini` session so it reloads `~/.gemini/settings.json`",
                "Copilot CLI" => "start a fresh Copilot CLI session so it reloads `~/.github/copilot/mcp.json`",
                "Antigravity" => "restart Antigravity so it reloads MCP config",
                "Goose" => "restart Goose so it reloads `.goose/config.yaml`",
                "Crush" => "restart Crush so it reloads MCP config",
                "Roo Code" => "reload the Roo Code host editor so it reloads MCP config",
                "Warp" => "restart Warp so it reloads `~/.warp/mcp.json`",
                "Windsurf" => "reload Windsurf so it reloads hooks and MCP config",
                _ => "restart the client so it reloads MCP config",
            };
            format!("{client}: {instruction}.")
        })
        .collect()
}
