use std::path::Path;

use colored::Colorize;

use super::{
    CanonicalRecordState, InstallState, RuntimeProbeState,
    diagnosis::install_repair_targets_for_snapshot,
    snapshot::{collect_status_snapshot, collect_status_snapshot_with_runtime_probe},
};
use crate::style::{self, sym};

pub fn status(json: bool) {
    let snapshot = collect_status_snapshot_with_runtime_probe();

    if json {
        println!("{}", crate::support::util::json_or(&snapshot, "{}"));
        return;
    }

    let mut ready: Vec<&str> = Vec::new();
    let mut drift: Vec<&str> = Vec::new();
    let mut conflicts: Vec<&str> = Vec::new();
    let mut not_detected: Vec<&str> = Vec::new();

    for client in &snapshot.clients {
        match (client.detected, client.state) {
            (true, InstallState::Installed) => ready.push(client.name),
            (true, InstallState::Conflict) => conflicts.push(client.name),
            (true, InstallState::NotInstalled | InstallState::Unknown) => {
                drift.push(client.name);
            }
            (false, _) => not_detected.push(client.name),
        }
    }

    println!(
        "{} {}",
        style::pewter("DiffLore MCP binary:"),
        style::emerald(&display_mcp_binary(&snapshot.binary))
    );
    if let Some(probe) = &snapshot.runtime_probe {
        let (mark, label) = match probe.state {
            RuntimeProbeState::Ok => (style::ok(sym::OK), style::emerald("passed")),
            RuntimeProbeState::Failed => (style::err(sym::ERR), style::danger("failed")),
            RuntimeProbeState::Timeout => (style::amber("!"), style::amber("timeout")),
        };
        println!(
            "{} {} {}",
            style::pewter("MCP runtime self-check:"),
            mark,
            label
        );
        println!(
            "  {}",
            style::pewter(&public_status_detail(&probe.detail, &snapshot.binary))
        );
    }
    if !matches!(
        snapshot.canonical_record.state,
        CanonicalRecordState::Present
    ) {
        let (mark, label) = match snapshot.canonical_record.state {
            CanonicalRecordState::Missing => (style::amber("!"), style::amber("not created yet")),
            CanonicalRecordState::Stale => (style::amber("!"), style::amber("stale")),
            CanonicalRecordState::Conflict => (style::err(sym::ERR), style::danger("conflict")),
            CanonicalRecordState::Present => {
                unreachable!("present canonical record is handled above")
            }
        };
        println!("{} {} {}", style::pewter("Install record:"), mark, label);
        if let Some(detail) = &snapshot.canonical_record.detail {
            println!(
                "  {}",
                style::pewter(&public_status_detail(detail, &snapshot.binary))
            );
        }
    }
    println!();

    let mut wrote_section = if ready.is_empty() {
        false
    } else {
        println!("  {}", style::pewter("Ready"));
        for name in &ready {
            println!("    {} {}", style::ok(sym::OK), name.bold());
        }
        true
    };
    if !drift.is_empty() {
        if wrote_section {
            println!();
        }
        println!("  {}", style::pewter("Detected but not wired"));
        for name in &drift {
            println!("    {} {}", style::amber("-"), name.bold());
        }
        wrote_section = true;
    }
    if !conflicts.is_empty() {
        if wrote_section {
            println!();
        }
        println!("  {}", style::pewter("Conflict"));
        for client in snapshot
            .clients
            .iter()
            .filter(|c| c.detected && matches!(c.state, InstallState::Conflict))
        {
            println!("    {} {}", style::err(sym::ERR), client.name.bold());
            println!(
                "        {}",
                style::pewter(&format!(
                    "{} MCP entry points to a different binary; `difflore agents install` will rewrite it",
                    client.name
                ))
            );
        }
        wrote_section = true;
    }
    if !not_detected.is_empty() {
        if wrote_section {
            println!();
        }
        println!(
            "  {} {}",
            style::pewter("Not detected:"),
            style::pewter(&format!("{} agents", not_detected.len()))
        );
    }

    let record_needs_action = !matches!(
        snapshot.canonical_record.state,
        CanonicalRecordState::Present
    );
    let needs_action = record_needs_action || !drift.is_empty() || !conflicts.is_empty();
    if needs_action {
        println!();
        // Direct command: when we already know agents need wiring, skip
        // the broader first-time setup summary.
        println!("  next: {}", style::cmd("difflore agents install"));
    }
}

fn display_mcp_binary(binary: &str) -> String {
    let Some(file_name) = Path::new(binary).file_name().and_then(|name| name.to_str()) else {
        return binary.to_owned();
    };
    if matches!(file_name, "difflore" | "difflore.exe") {
        "difflore".to_owned()
    } else {
        binary.to_owned()
    }
}

fn public_status_detail(detail: &str, binary: &str) -> String {
    let mut out = detail.replace(binary, "difflore").replace('\\', "/");
    for (suffix, label) in [
        ("/.github/copilot/mcp.json", "~/.github/copilot/mcp.json"),
        (
            "/.gemini/antigravity/mcp_config.json",
            "~/.gemini/antigravity/mcp_config.json",
        ),
        ("/.codex/config.toml", "~/.codex/config.toml"),
        ("/.claude/settings.json", "~/.claude/settings.json"),
        ("/.cursor/mcp.json", "~/.cursor/mcp.json"),
        ("/.config/crush/mcp.json", "~/.config/crush/mcp.json"),
        ("/.warp/mcp.json", "~/.warp/mcp.json"),
    ] {
        out = replace_path_ending(&out, suffix, label);
    }
    scrub_command_path(&out)
}

fn replace_path_ending(input: &str, suffix: &str, label: &str) -> String {
    let Some(pos) = input.find(suffix) else {
        return input.to_owned();
    };
    let start = input[..pos]
        .rfind(|c: char| c.is_whitespace() || matches!(c, '(' | '`' | '='))
        .map_or(0, |idx| idx + 1);
    let end = pos + suffix.len();
    let mut out = input.to_owned();
    out.replace_range(start..end, label);
    out
}

fn scrub_command_path(input: &str) -> String {
    let Some(pos) = input.find("command=") else {
        return input.to_owned();
    };
    let value_start = pos + "command=".len();
    let tail = &input[value_start..];
    let value_len = tail.find([',', ')', ' ']).unwrap_or(tail.len());
    let value = &tail[..value_len];
    let normalized = value.replace('\\', "/");
    if normalized.ends_with("/difflore.exe") || normalized.ends_with("/difflore") {
        let mut out = input.to_owned();
        out.replace_range(value_start..value_start + value_len, "difflore");
        out
    } else {
        input.to_owned()
    }
}

/// Names of AI-agent surfaces detected on this machine that don't yet have
/// `DiffLore` installed. Empty means everything wireable is wired.
pub fn detect_install_drift() -> Vec<&'static str> {
    let snapshot = collect_status_snapshot();
    snapshot
        .clients
        .iter()
        .filter(|c| c.detected && !matches!(c.state, InstallState::Installed))
        .map(|c| c.name)
        .collect()
}

/// Client display names whose MCP wiring can be refreshed by rerunning
/// the idempotent installer. Includes classic drift (detected but not
/// installed/conflicting) plus canonical-record drift such as hooks that
/// exist on disk but were not captured in `~/.difflore/mcp.json`.
pub fn detect_install_repair_targets() -> Vec<String> {
    let snapshot = collect_status_snapshot();
    install_repair_targets_for_snapshot(&snapshot)
}

pub async fn maybe_print_mcp_hint() {
    match difflore_core::infra::settings::get().await {
        Ok(s) if s.hints_mcp => {}
        _ => return,
    }

    let drift = detect_install_drift();
    if drift.is_empty() {
        return;
    }

    let names = drift.join(", ");
    println!();
    println!(
        "{} {} detected without DiffLore - install once so rules reach your next agent run:",
        style::emerald(sym::TIP),
        style::ident(&names),
    );
    println!("  -> run {}", style::cmd("difflore init"));
}
