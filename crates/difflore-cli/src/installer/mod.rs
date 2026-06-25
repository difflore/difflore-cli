//! MCP installer: registers difflore as an MCP server with every detected
//! AI coding tool, in one shot. Public entry points are `install_all`,
//! `uninstall_all`, `status`, `collect_status_snapshot`,
//! `detect_install_drift`, and `maybe_print_mcp_hint`.
//!
//! Detect + install + uninstall are all driven by the single `AGENTS` table in
//! `registry.rs` (one [`registry::AgentSpec`] row per surface). The drivers
//! delegate to the leaf format engines — `json_config.rs`, `goose_yaml.rs`,
//! `hooks_install.rs` — and shared path / record helpers live in `common.rs`.
//! Adding an agent means adding one `AGENTS` row, not touching a probe table,
//! a per-agent install fn, an install/uninstall dispatch list, and the
//! name/UX maps.

mod common;
mod diagnosis;
mod goose_yaml;
mod hooks_install;
mod install;
mod json_config;
mod manifest;
mod registry;
mod snapshot;
mod status_display;
mod types;
mod uninstall;

pub use install::{agent_update_nudge, install_all, update_all};
pub use snapshot::{collect_status_snapshot, collect_status_snapshot_with_runtime_probe};
pub use status_display::{
    detect_install_drift, detect_install_repair_targets, maybe_print_mcp_hint, status,
};
pub use types::{
    CanonicalRecordState, CanonicalRecordStatus, InstallState, McpClientStatus, McpRuntimeProbe,
    McpStatusDiagnosis, McpStatusSnapshot, RuntimeProbeState, Status, TargetOutcome, TargetStatus,
};
pub use uninstall::uninstall_all;

#[cfg(test)]
mod test_util {
    use std::path::PathBuf;

    pub(super) fn tmp_settings_path() -> (tempfile::TempDir, PathBuf) {
        tmp_named_path("settings.json")
    }

    pub(super) fn tmp_named_path(filename: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let path = dir.path().join(filename);
        (dir, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::{
        common::{self, canonical_target_key},
        diagnosis::{
            client_name_for_surface, diagnose_status_snapshot, install_repair_targets_for_snapshot,
        },
        install::{
            failed_outcome_names, install_outcome_verb, outcome_already_installed,
            outcome_client_names, should_write_canonical_record,
        },
        registry::AGENTS,
        snapshot::collect_client_statuses_from_agents,
    };
    use std::{collections::BTreeSet, fs};

    // ── Registry single-table guard rails ─────────────────────────

    #[test]
    fn agents_table_orders_claude_then_claude_hooks_then_codex_then_codex_hooks_first() {
        // `collect_agent_statuses` relies on this row order (Claude Code →
        // Claude Code hooks → Codex → Codex hooks first). Guard the first
        // four names.
        let first_four: Vec<&str> = AGENTS.iter().take(4).map(|spec| spec.name).collect();
        assert_eq!(
            first_four,
            vec!["Claude Code", "Claude Code hooks", "Codex", "Codex hooks"]
        );
    }

    #[test]
    fn every_agent_surface_resolves_to_a_known_client() {
        // Surface `name` strings are load-bearing: every one must resolve
        // through `client_name_for_surface` to a real client (never the
        // "unknown client" sentinel), or roll-up and record-matching silently
        // break.
        for spec in AGENTS {
            assert_ne!(
                client_name_for_surface(spec.name),
                "unknown client",
                "surface {:?} did not resolve to a known client",
                spec.name
            );
            // The display client on the row must agree with the derived one.
            assert_eq!(
                client_name_for_surface(spec.name),
                spec.client.display_name(),
                "surface {:?} client mismatch",
                spec.name
            );
        }
    }

    #[test]
    fn agents_table_keeps_legacy_surface_name_set() {
        // Pin the exact surface-name set so a typo (e.g. "Gemini CLI" vs
        // "Gemini") can't slip in and desync the canonical record / client
        // roll-up.
        let names: BTreeSet<&str> = AGENTS.iter().map(|spec| spec.name).collect();
        let expected: BTreeSet<&str> = [
            "Claude Code",
            "Claude Code hooks",
            "Codex",
            "Codex hooks",
            "Cursor",
            "Cursor hooks",
            "Gemini",
            "Gemini hooks",
            "Copilot CLI",
            "Antigravity",
            "Goose",
            "Crush",
            "Roo Code",
            "Warp",
            "Windsurf hooks",
            "OpenCode",
        ]
        .into_iter()
        .collect();
        assert_eq!(names, expected);
    }

    #[test]
    fn client_matrix_collapses_raw_surfaces_to_twelve_clients() {
        let clients = collect_client_statuses_from_agents(&[
            TargetStatus {
                name: "Claude Code",
                detected: true,
                state: InstallState::Installed,
                detail: None,
            },
            TargetStatus {
                name: "Claude Code hooks",
                detected: true,
                state: InstallState::Installed,
                detail: None,
            },
            TargetStatus {
                name: "Codex",
                detected: true,
                state: InstallState::Installed,
                detail: None,
            },
            TargetStatus {
                name: "Codex hooks",
                detected: true,
                state: InstallState::Installed,
                detail: None,
            },
            TargetStatus {
                name: "Cursor",
                detected: true,
                state: InstallState::Installed,
                detail: None,
            },
            TargetStatus {
                name: "Cursor hooks",
                detected: true,
                state: InstallState::NotInstalled,
                detail: None,
            },
        ]);
        assert_eq!(clients.len(), 12);
        let claude = clients
            .iter()
            .find(|client| client.name == "Claude Code")
            .expect("claude client");
        assert_eq!(claude.state, InstallState::Installed);
        let codex = clients
            .iter()
            .find(|client| client.name == "Codex")
            .expect("codex client");
        assert_eq!(codex.state, InstallState::Installed);
        let cursor = clients
            .iter()
            .find(|client| client.name == "Cursor")
            .expect("cursor client");
        assert_eq!(cursor.state, InstallState::Conflict);
    }

    #[test]
    fn client_detail_ignores_undetected_optional_surfaces() {
        let clients = collect_client_statuses_from_agents(&[
            TargetStatus {
                name: "Cursor",
                detected: true,
                state: InstallState::Installed,
                detail: Some("~/.cursor/mcp.json".to_owned()),
            },
            TargetStatus {
                name: "Cursor hooks",
                detected: false,
                state: InstallState::NotInstalled,
                detail: Some("./.cursor/hooks.json not found".to_owned()),
            },
        ]);
        let cursor = clients
            .iter()
            .find(|client| client.name == "Cursor")
            .expect("cursor client");

        assert_eq!(cursor.state, InstallState::Installed);
        assert_eq!(
            cursor.detail.as_deref(),
            Some("1/1 detected surface(s) installed")
        );
    }

    #[test]
    fn canonical_target_key_normalizes_display_and_cli_names() {
        assert_eq!(canonical_target_key("Claude Code"), "claude");
        assert_eq!(canonical_target_key("Claude Code hooks"), "claude hooks");
        assert_eq!(canonical_target_key("claude"), "claude");
        assert_eq!(canonical_target_key("Codex"), "codex");
        assert_eq!(canonical_target_key("Codex hooks"), "codex hooks");
        assert_eq!(canonical_target_key("codex"), "codex");
        assert_eq!(canonical_target_key("Gemini hooks"), "gemini hooks");
    }

    #[test]
    fn dry_run_outcome_verbs_describe_plan_not_execution() {
        assert_eq!(
            install_outcome_verb(&Status::Installed, true, false),
            "would install"
        );
        assert_eq!(
            install_outcome_verb(&Status::Updated, true, false),
            "would update"
        );
        assert_eq!(
            install_outcome_verb(&Status::Installed, true, true),
            "already installed"
        );
        assert_eq!(
            install_outcome_verb(&Status::Updated, true, true),
            "already installed"
        );
        assert_eq!(
            install_outcome_verb(
                &Status::Skipped("DiffLore plugin already installed".to_owned()),
                true,
                true
            ),
            "already installed"
        );
        assert_eq!(
            install_outcome_verb(&Status::Installed, false, false),
            "installed"
        );
        assert_eq!(
            install_outcome_verb(&Status::Updated, false, false),
            "updated"
        );
    }

    #[test]
    fn dry_run_already_installed_uses_canonical_surface_names() {
        let installed_surfaces = BTreeSet::from([canonical_target_key("Claude Code hooks")]);
        let outcome = TargetOutcome {
            name: "Claude Code hooks",
            status: Status::Updated,
            detail: "~/.claude/settings.json".to_owned(),
        };

        assert!(outcome_already_installed(&outcome, &installed_surfaces));
    }

    #[test]
    fn outcome_client_names_collapses_hook_surfaces_to_restart_clients() {
        let outcomes = vec![
            TargetOutcome {
                name: "Cursor",
                status: Status::Installed,
                detail: "~/.cursor/mcp.json".to_owned(),
            },
            TargetOutcome {
                name: "Cursor hooks",
                status: Status::Updated,
                detail: "./.cursor/hooks.json".to_owned(),
            },
            TargetOutcome {
                name: "Codex hooks",
                status: Status::Installed,
                detail: "~/.codex/hooks.json".to_owned(),
            },
            TargetOutcome {
                name: "Gemini hooks",
                status: Status::Skipped("not found".to_owned()),
                detail: String::new(),
            },
        ];

        assert_eq!(
            outcome_client_names(&outcomes),
            vec!["Codex".to_owned(), "Cursor".to_owned()]
        );
    }

    #[test]
    fn canonical_record_is_skipped_on_partial_install_failure() {
        let installed = vec!["Claude Code"];
        let failed = vec!["Cursor"];
        assert!(!should_write_canonical_record(false, &installed, &failed));
        assert!(should_write_canonical_record(false, &installed, &[]));
        assert!(!should_write_canonical_record(true, &installed, &[]));
        assert!(!should_write_canonical_record(false, &[], &[]));
    }

    #[test]
    fn json_probe_requires_command_and_mcp_server_arg() {
        let (_tmp, path) = test_util::tmp_named_path("mcp.json");
        fs::write(
            &path,
            r#"{ "mcpServers": { "difflore": { "command": "/tmp/fake/difflore", "args": [] } } }"#,
        )
        .expect("write config");

        let status =
            common::probe_json_install("Cursor", &path, "mcpServers", "/tmp/fake/difflore");
        assert_eq!(status.state, InstallState::Conflict);
        assert!(
            status
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("args=[]"))
        );

        fs::write(
            &path,
            r#"{ "mcpServers": { "difflore": { "command": "/tmp/fake/difflore", "args": ["mcp-server"] } } }"#,
        )
        .expect("write config");
        let status =
            common::probe_json_install("Cursor", &path, "mcpServers", "/tmp/fake/difflore");
        assert_eq!(status.state, InstallState::Installed);
    }

    #[test]
    fn failed_outcome_names_only_counts_real_errors() {
        let outcomes = vec![
            TargetOutcome {
                name: "Claude Code",
                status: Status::Installed,
                detail: String::new(),
            },
            TargetOutcome {
                name: "Cursor",
                status: Status::Skipped("not detected".to_owned()),
                detail: String::new(),
            },
            TargetOutcome {
                name: "Gemini",
                status: Status::Error("write failed".to_owned()),
                detail: String::new(),
            },
        ];

        assert_eq!(failed_outcome_names(&outcomes), vec!["Gemini"]);
    }

    #[test]
    fn runtime_probe_output_accepts_initialize_and_tools_list() {
        let stdout = concat!(
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05"}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"search_rules"},{"name":"get_rules"}]}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"{\"results\":[{\"id\":\"rule-1\",\"title\":\"Probe rule title\"}]}"}],"_meta":{"impact":{"kind":"rules_index","rulesInjected":1,"rulesIndexed":12}}}}"#,
            "\n"
        );
        let probe = common::evaluate_runtime_probe_output(stdout, "", true);
        assert_eq!(probe.state, RuntimeProbeState::Ok);
        assert!(probe.initialized);
        assert!(probe.tools_listed);
        assert!(probe.tool_call_completed);
        assert_eq!(probe.tool_call_name.as_deref(), Some("search_rules"));
        assert_eq!(probe.tool_call_rules_injected, Some(1));
        assert_eq!(probe.tool_call_rules_indexed, Some(12));
        assert_eq!(
            probe.tool_call_top_result.as_deref(),
            Some("Probe rule title")
        );
        assert_eq!(probe.tool_count, Some(2));
        assert_eq!(
            probe.tool_names,
            vec!["search_rules".to_owned(), "get_rules".to_owned()]
        );
        // The human-facing detail line summarizes a clean handshake without
        // leaking internal probe wiring (the search_rules tool call itself is
        // asserted via the structured `tool_call_name` field above).
        assert!(
            probe.detail.contains("MCP handshake and tool listing OK"),
            "{}",
            probe.detail
        );
    }

    #[test]
    fn runtime_probe_input_scopes_search_to_changed_file() {
        let input = common::build_runtime_probe_input(Some("crates/app/src/lib.rs".to_owned()));
        let messages = input
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("valid json"))
            .collect::<Vec<_>>();

        assert_eq!(messages.len(), 3);
        assert_eq!(messages[2]["method"], "tools/call");
        assert_eq!(messages[2]["params"]["name"], "search_rules");
        assert_eq!(
            messages[2]["params"]["arguments"]["file"],
            "crates/app/src/lib.rs"
        );
        assert!(
            messages[2]["params"]["arguments"]["intent"]
                .as_str()
                .expect("intent")
                .contains("crates/app/src/lib.rs")
        );
        assert_eq!(
            messages[2]["params"]["arguments"]["session_id"],
            "difflore-mcp-status"
        );
    }

    #[test]
    fn runtime_probe_input_omits_file_when_no_diff_exists() {
        let input = common::build_runtime_probe_input(None);
        let messages = input
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("valid json"))
            .collect::<Vec<_>>();

        assert_eq!(messages.len(), 3);
        assert!(messages[2]["params"]["arguments"].get("file").is_none());
        assert_eq!(
            messages[2]["params"]["arguments"]["intent"],
            "verify DiffLore MCP can return team rules"
        );
    }

    #[test]
    fn runtime_probe_output_reports_missing_tool_list() {
        let stdout = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05"}}"#;
        let probe = common::evaluate_runtime_probe_output(stdout, "boom", false);
        assert_eq!(probe.state, RuntimeProbeState::Failed);
        assert!(probe.initialized);
        assert!(!probe.tools_listed);
        assert!(probe.detail.contains("stderr: boom"));
    }

    #[test]
    fn runtime_probe_output_requires_search_rules_tool_call() {
        let stdout = concat!(
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05"}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"search_rules"},{"name":"get_rules"}]}}"#,
            "\n"
        );
        let probe = common::evaluate_runtime_probe_output(stdout, "", true);
        assert_eq!(probe.state, RuntimeProbeState::Failed);
        assert!(probe.initialized);
        assert!(probe.tools_listed);
        assert!(!probe.tool_call_completed);
        assert!(probe.detail.contains("did not complete search_rules"));
    }

    fn diagnosis_fixture(
        runtime_state: RuntimeProbeState,
        record_state: CanonicalRecordState,
    ) -> McpStatusSnapshot {
        let (recorded_targets, actual_targets) =
            if matches!(record_state, CanonicalRecordState::Stale) {
                (
                    vec!["Claude Code".to_owned()],
                    vec!["Claude Code".to_owned(), "Claude Code hooks".to_owned()],
                )
            } else {
                (
                    vec!["Claude Code".to_owned()],
                    vec!["Claude Code".to_owned()],
                )
            };
        McpStatusSnapshot {
            binary: "difflore".to_owned(),
            canonical_record: CanonicalRecordStatus {
                path: Some("mcp.json".to_owned()),
                state: record_state,
                detail: None,
                recorded_targets,
                actual_targets,
            },
            runtime_probe: Some(McpRuntimeProbe {
                state: runtime_state,
                detail: "probe detail".to_owned(),
                initialized: matches!(runtime_state, RuntimeProbeState::Ok),
                tools_listed: matches!(runtime_state, RuntimeProbeState::Ok),
                tool_call_completed: matches!(runtime_state, RuntimeProbeState::Ok),
                tool_call_name: matches!(runtime_state, RuntimeProbeState::Ok)
                    .then(|| "search_rules".to_owned()),
                tool_call_rules_injected: None,
                tool_call_rules_indexed: None,
                tool_call_top_result: None,
                tool_count: Some(7),
                tool_names: Vec::new(),
            }),
            diagnosis: None,
            clients: vec![McpClientStatus {
                name: "Claude Code",
                detected: true,
                state: InstallState::Installed,
                detail: None,
                surfaces: Vec::new(),
            }],
            agents: Vec::new(),
        }
    }

    #[test]
    fn diagnosis_distinguishes_healthy_runtime_from_install_record_drift() {
        let snapshot = diagnosis_fixture(RuntimeProbeState::Ok, CanonicalRecordState::Stale);
        let diagnosis = diagnose_status_snapshot(&snapshot);
        assert!(diagnosis.summary.contains("ready for agents"));
        assert!(diagnosis.summary.contains("client-wiring drift"));
        assert!(diagnosis.next_step.contains("difflore agents install"));
        assert_eq!(diagnosis.affected_clients, vec!["Claude Code".to_owned()]);
        assert!(
            diagnosis
                .actions
                .iter()
                .any(|action| action.contains("Restart/reload affected client(s): Claude Code"))
        );
        assert!(
            diagnosis
                .actions
                .iter()
                .any(|action| action.contains("Claude Code: restart Claude Code"))
        );
    }

    #[test]
    fn diagnosis_for_clean_runtime_lists_installed_client_reload_steps() {
        let snapshot = diagnosis_fixture(RuntimeProbeState::Ok, CanonicalRecordState::Present);
        let diagnosis = diagnose_status_snapshot(&snapshot);
        assert!(diagnosis.next_step.contains("Transport closed"));
        assert!(
            diagnosis
                .actions
                .iter()
                .any(|action| action.contains("Claude Code: restart Claude Code"))
        );
        assert!(
            diagnosis
                .actions
                .iter()
                .any(|action| action.contains("completes a search_rules"))
        );
    }

    #[test]
    fn install_repair_targets_include_canonical_hook_drift() {
        let snapshot = diagnosis_fixture(RuntimeProbeState::Ok, CanonicalRecordState::Stale);
        assert_eq!(
            install_repair_targets_for_snapshot(&snapshot),
            vec!["Claude Code".to_owned()]
        );
    }

    #[test]
    fn install_repair_targets_are_empty_for_clean_installed_client() {
        let snapshot = diagnosis_fixture(RuntimeProbeState::Ok, CanonicalRecordState::Present);
        assert!(install_repair_targets_for_snapshot(&snapshot).is_empty());
    }

    #[test]
    fn diagnosis_flags_runtime_failure_as_memory_server_problem() {
        let snapshot = diagnosis_fixture(RuntimeProbeState::Failed, CanonicalRecordState::Present);
        let diagnosis = diagnose_status_snapshot(&snapshot);
        assert!(diagnosis.summary.contains("failed the status check"));
        assert!(diagnosis.next_step.contains("stderr/details"));
        assert!(
            diagnosis
                .actions
                .iter()
                .any(|action| action.contains("Rebuild or upgrade"))
        );
    }
}
