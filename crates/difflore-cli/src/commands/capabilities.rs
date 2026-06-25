use serde::Serialize;

use crate::commands::ai_contract::{
    CAPABILITIES_SCHEMA_VERSION, CommandContract, command_contract,
};
use crate::style;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CapabilitiesOutput {
    schema_version: &'static str,
    cli_version: &'static str,
    role: CapabilityRole,
    commands: Vec<CommandCapability>,
    mcp: McpCapability,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct CapabilityRole {
    cli: &'static str,
    mcp: &'static str,
    principle: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct CommandCapability {
    command: &'static str,
    description: &'static str,
    #[serde(flatten)]
    contract: CommandContract,
    related_mcp_tools: Vec<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct McpCapability {
    role: &'static str,
    allowed_tools: Vec<&'static str>,
    write_tools: Vec<McpWriteTool>,
    denied_control_plane_tools: Vec<&'static str>,
    cloud_reads: McpCloudReadPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct McpWriteTool {
    tool: &'static str,
    policy: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct McpCloudReadPolicy {
    default: &'static str,
    opt_in_env: &'static str,
}

pub(crate) fn capabilities_payload() -> CapabilitiesOutput {
    CapabilitiesOutput {
        schema_version: CAPABILITIES_SCHEMA_VERSION,
        cli_version: env!("CARGO_PKG_VERSION"),
        role: CapabilityRole {
            cli: "human_control_plane_and_ai_cli_contract",
            mcp: "agent_context_retrieval_explanation_and_proposal",
            principle: "CLI owns approval, auth, sync, publishing, provider setup, and local writes; MCP stays read-mostly and context-focused.",
        },
        commands: vec![
            command(
                "difflore capabilities --json",
                "Print the stable AI-facing CLI/MCP capability contract.",
                &[],
            ),
            command(
                "difflore status --json",
                "Read local readiness, memory counts, autopilot status, and the next recommended action.",
                &[],
            ),
            command(
                "difflore recall --diff --json",
                "Show source-backed rules that match the current diff without running review analysis.",
                &["search_rules", "get_rules"],
            ),
            command(
                "difflore review --diff all --json",
                "Review the current staged and unstaged diff without modifying files.",
                &["plan_pr"],
            ),
            command(
                "difflore review --ci --diff all --json",
                "Gate the current staged and unstaged diff; exits non-zero on actionable findings.",
                &["plan_pr"],
            ),
            command(
                "difflore ask <question> --json",
                "Ask local memory a natural-language question.",
                &["search_rules", "get_rules"],
            ),
            command(
                "difflore memory --json",
                "Read the compact memory summary, queues, background autopilot state, and next action.",
                &["list_memory", "get_memory_digest"],
            ),
            command(
                "difflore memory inbox --json",
                "Read active rules, local drafts, candidate memories, queues, warnings, and next action.",
                &["list_memory"],
            ),
            command(
                "difflore memory digest --json",
                "Read the autopilot digest, candidate grouping, schedule status, and review guidance.",
                &["get_memory_digest"],
            ),
            command(
                "difflore memory log --json",
                "Read the background autopilot audit log.",
                &["get_memory_autopilot_log"],
            ),
            command(
                "difflore memory remember --title <title> --body <body> --json",
                "CLI fallback for user-requested rule memory; saves and enables an active local rule.",
                &["remember_rule"],
            ),
            command(
                "difflore memory review",
                "Human review loop for pending local memory.",
                &[],
            ),
            command(
                "difflore memory approve <item-id> --json",
                "Approve a local memory item into active local rules.",
                &[],
            ),
            command(
                "difflore memory reject <item-id> --json",
                "Reject a local memory item from the local queue.",
                &[],
            ),
            command(
                "difflore memory disable <rule-id> --json",
                "Disable an active local rule so agents no longer receive it.",
                &[],
            ),
            command(
                "difflore import-reviews --dry-run --json",
                "Preview PR/MR review import without writing local memory.",
                &[],
            ),
            command(
                "difflore import-reviews --json",
                "Import PR/MR review history into local source-backed memory.",
                &[],
            ),
            command(
                "difflore fix",
                "Apply local patches after explicit user intent.",
                &[],
            ),
            command(
                "difflore agents status --json",
                "Read installed agent integration status.",
                &[],
            ),
            command(
                "difflore agents install --dry-run",
                "Preview agent integration writes.",
                &[],
            ),
            command(
                "difflore cloud team --json",
                "Read cloud/team readiness after login.",
                &[],
            ),
            command(
                "difflore cloud sync --dry-run --json",
                "Preview optional cloud sync queues.",
                &[],
            ),
        ],
        mcp: McpCapability {
            role: "read_mostly_context_plane",
            allowed_tools: difflore_core::mcp_server::ALLOWED_MCP_TOOL_NAMES.to_vec(),
            write_tools: vec![McpWriteTool {
                tool: "remember_rule",
                policy: "user_requested_active_rule_with_provenance_dedup_rate_limit",
            }],
            denied_control_plane_tools: difflore_core::mcp_server::CONTROL_PLANE_DENIED_TOOL_NAMES
                .to_vec(),
            cloud_reads: McpCloudReadPolicy {
                default: "local_only",
                opt_in_env: "DIFFLORE_MCP_ALLOW_CLOUD_READS=1",
            },
        },
    }
}

pub(crate) fn handle_capabilities(json: bool) {
    let payload = capabilities_payload();
    if json {
        println!("{}", crate::support::util::json_compact_or(&payload, "{}"));
        return;
    }

    println!("{}", style::title("Capabilities"));
    println!("  CLI: {}", payload.role.cli);
    println!("  MCP: {}", payload.role.mcp);
    println!();
    println!(
        "  machine contract: {}",
        style::cmd("difflore capabilities --json")
    );
    println!("  public commands: {}", payload.commands.len());
}

fn command(
    command: &'static str,
    description: &'static str,
    related_mcp_tools: &[&'static str],
) -> CommandCapability {
    CommandCapability {
        command,
        description,
        contract: command_contract(command),
        related_mcp_tools: related_mcp_tools.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_have_schema_and_public_commands_only() {
        let payload = capabilities_payload();
        assert_eq!(payload.schema_version, CAPABILITIES_SCHEMA_VERSION);
        assert!(
            payload
                .commands
                .iter()
                .any(|command| command.command == "difflore status --json")
        );
        for command in payload.commands {
            assert!(
                !command.command.contains("mcp-server")
                    && !command.command.contains("__hook-daemon")
                    && !command.command.contains("__outbox-daemon")
                    && !command.command.contains("skills sweep")
                    && !command.command.contains("dist verify"),
                "hidden/internal command leaked: {}",
                command.command
            );
        }
    }

    #[test]
    fn mcp_contract_keeps_control_plane_out() {
        let payload = capabilities_payload();
        assert!(payload.mcp.allowed_tools.contains(&"remember_rule"));
        assert!(
            payload
                .mcp
                .denied_control_plane_tools
                .contains(&"approve_memory")
        );
        assert_eq!(payload.mcp.cloud_reads.default, "local_only");
    }

    #[test]
    fn mcp_manifest_reuses_enforced_core_lists() {
        let payload = capabilities_payload();
        assert_eq!(
            payload.mcp.allowed_tools,
            difflore_core::mcp_server::ALLOWED_MCP_TOOL_NAMES
        );
        assert_eq!(
            payload.mcp.denied_control_plane_tools,
            difflore_core::mcp_server::CONTROL_PLANE_DENIED_TOOL_NAMES
        );
    }
}
