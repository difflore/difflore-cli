use std::collections::BTreeSet;

use super::{
    InstallState, McpClientStatus, McpStatusSnapshot, TargetStatus,
    common::{canonical_record_snapshot, probe_runtime_mcp_server, resolve_difflore_binary},
    diagnosis::diagnose_status_snapshot,
    registry::{self, AGENTS},
};

// ── Status aggregation ──────────────────────────────────────────────────

fn find_surface(agents: &[TargetStatus], name: &str) -> Option<TargetStatus> {
    agents.iter().find(|agent| agent.name == name).cloned()
}

fn client_status(name: &'static str, surfaces: Vec<TargetStatus>) -> McpClientStatus {
    let detected = surfaces.iter().any(|surface| surface.detected);
    let detected_surfaces: Vec<&TargetStatus> =
        surfaces.iter().filter(|surface| surface.detected).collect();
    let installed = detected_surfaces
        .iter()
        .filter(|surface| matches!(surface.state, InstallState::Installed))
        .count();
    let conflicts = detected_surfaces
        .iter()
        .filter(|surface| matches!(surface.state, InstallState::Conflict))
        .count();
    let unknowns = detected_surfaces
        .iter()
        .filter(|surface| matches!(surface.state, InstallState::Unknown))
        .count();
    let missing = detected_surfaces
        .iter()
        .filter(|surface| matches!(surface.state, InstallState::NotInstalled))
        .count();

    let state = if conflicts > 0 || (installed > 0 && missing > 0) {
        InstallState::Conflict
    } else if unknowns > 0 {
        InstallState::Unknown
    } else if detected && installed == detected_surfaces.len() && installed > 0 {
        InstallState::Installed
    } else {
        InstallState::NotInstalled
    };

    let detail = match state {
        InstallState::Installed => Some(format!(
            "{installed}/{} detected surface(s) installed",
            detected_surfaces.len()
        )),
        InstallState::Conflict if installed > 0 && missing > 0 => Some(format!(
            "partial install: {installed} installed, {missing} missing"
        )),
        InstallState::Conflict => Some(format!("{conflicts} conflicting surface(s)")),
        InstallState::Unknown => Some(format!("{unknowns} unknown surface(s)")),
        InstallState::NotInstalled if detected => {
            Some("detected, but no DiffLore surface installed".into())
        }
        InstallState::NotInstalled => Some("not detected".into()),
    };

    McpClientStatus {
        name,
        detected,
        state,
        detail,
        surfaces,
    }
}

/// Roll the per-surface `agents` up into one [`McpClientStatus`] per client.
/// The client list and surface→client mapping derive from the `AGENTS` table
/// (`spec.client`, a [`crate::clients::ClientId`]), so a new agent row appears
/// here automatically. Clients are emitted in first-seen `AGENTS` order, each
/// client's surfaces in row order.
pub(super) fn collect_client_statuses_from_agents(agents: &[TargetStatus]) -> Vec<McpClientStatus> {
    let mut clients: Vec<crate::clients::ClientId> = Vec::new();
    let mut seen: BTreeSet<crate::clients::ClientId> = BTreeSet::new();
    for spec in AGENTS {
        if seen.insert(spec.client) {
            clients.push(spec.client);
        }
    }
    clients
        .into_iter()
        .map(|client| {
            let surfaces: Vec<TargetStatus> = AGENTS
                .iter()
                .filter(|spec| spec.client == client)
                .filter_map(|spec| find_surface(agents, spec.name))
                .collect();
            client_status(client.display_name(), surfaces)
        })
        .collect()
}

/// Probe every surface in the `AGENTS` table. Row order is load-bearing
/// (Claude Code → Claude Code hooks → Codex come first) and is encoded in the
/// table itself.
pub(super) fn collect_agent_statuses(bin: &str) -> Vec<TargetStatus> {
    AGENTS
        .iter()
        .map(|spec| registry::detect(spec, bin))
        .collect()
}

pub(super) fn installed_targets_from_agents(agents: &[TargetStatus]) -> Vec<&'static str> {
    agents
        .iter()
        .filter(|o| matches!(o.state, InstallState::Installed))
        .map(|o| o.name)
        .collect()
}

pub fn collect_status_snapshot() -> McpStatusSnapshot {
    let bin = resolve_difflore_binary().unwrap_or_else(|_| "difflore".to_owned());
    let agents = collect_agent_statuses(&bin);
    let installed_targets = installed_targets_from_agents(&agents);
    let canonical_record = canonical_record_snapshot(&bin, &installed_targets);
    let clients = collect_client_statuses_from_agents(&agents);
    McpStatusSnapshot {
        binary: bin,
        canonical_record,
        runtime_probe: None,
        diagnosis: None,
        clients,
        agents,
    }
}

pub fn collect_status_snapshot_with_runtime_probe() -> McpStatusSnapshot {
    let mut snapshot = collect_status_snapshot();
    snapshot.runtime_probe = Some(probe_runtime_mcp_server(&snapshot.binary));
    snapshot.diagnosis = Some(diagnose_status_snapshot(&snapshot));
    snapshot
}
