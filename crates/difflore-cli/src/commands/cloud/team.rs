//! Team workspace status (`difflore cloud team`) and rule publish /
//! unpublish (`difflore cloud publish` / `unpublish`).
//!
//! Also owns the "accepted-fix proof readiness" contract that several
//! cloud surfaces share (status, team, login): whether a logged-in user
//! has a team workspace wired up so accepted fixes can link to team
//! Impact evidence. Those helpers are `pub(super)` so the sibling cloud
//! modules can render the same readiness state without duplicating it.

use crate::commands::util::{exit_code, exit_err};
use crate::style;

pub(super) const TEAM_WORKSPACE_URL: &str = "https://difflore.dev/team";

pub(super) fn accepted_fix_proof_ready(logged_in: bool, team_name: Option<&str>) -> bool {
    logged_in && team_name.is_some_and(|team| !team.trim().is_empty())
}

pub(super) fn accepted_fix_proof_readiness_state(
    logged_in: bool,
    team_name: Option<&str>,
) -> &'static str {
    if !logged_in {
        "needs_cloud_login"
    } else if accepted_fix_proof_ready(logged_in, team_name) {
        "team_link_ready"
    } else {
        "needs_team_workspace"
    }
}

fn accepted_fix_proof_readiness_message(logged_in: bool, team_name: Option<&str>) -> &'static str {
    if !logged_in {
        "run `difflore cloud login`, then create or join a team before collecting cloud-linked accepted-fix evidence"
    } else if accepted_fix_proof_ready(logged_in, team_name) {
        "accepted fixes can link to team Impact evidence"
    } else {
        "create or join a team before collecting cloud-linked accepted-fix evidence: https://difflore.dev/team"
    }
}

const fn accepted_fix_proof_pre_capture_required_fields() -> [&'static str; 3] {
    [
        "loggedIn=true",
        "acceptedFixProof.teamWorkspaceReady=true",
        "acceptedFixProof.state=team_link_ready",
    ]
}

const fn accepted_fix_proof_per_fix_required_fields() -> [&'static str; 7] {
    [
        "acceptanceSource=difflore_fix",
        "client=difflore_cli",
        "targetPrNumber>0",
        "team_id IS NOT NULL",
        "linked accepted fix_outcome observation",
        "non-empty rule_id",
        "fix_acceptances.created_at >= captureBaseline.capturedAtIso",
    ]
}

const fn accepted_fix_proof_non_counting_warnings() -> [&'static str; 5] {
    [
        "missing team workspace",
        "missing recalled rule id",
        "missing linked rule observation",
        "unexpected client",
        "missing target PR number",
    ]
}

pub(super) fn accepted_fix_proof_readiness_value(
    logged_in: bool,
    team_name: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "teamWorkspaceReady": accepted_fix_proof_ready(logged_in, team_name),
        "state": accepted_fix_proof_readiness_state(logged_in, team_name),
        "message": accepted_fix_proof_readiness_message(logged_in, team_name),
        "readinessScope": "pre_capture_only",
        "countsAsEvidence": false,
        "countingEvidence": "cloud DB fix_acceptances rows with acceptanceSource=difflore_fix, client=difflore_cli, targetPrNumber>0, team_id, and linked accepted fix_outcome observations",
        "preCaptureRequiredFields": accepted_fix_proof_pre_capture_required_fields(),
        "perFixRequiredFields": accepted_fix_proof_per_fix_required_fields(),
        "nonCountingWarnings": accepted_fix_proof_non_counting_warnings(),
    })
}

pub(super) fn print_accepted_fix_proof_readiness(logged_in: bool, team_name: Option<&str>) {
    let message = accepted_fix_proof_readiness_message(logged_in, team_name);
    let rendered = if accepted_fix_proof_ready(logged_in, team_name) {
        style::pewter(message)
    } else {
        style::amber(message)
    };
    println!("  evidence  {rendered}");
}

fn team_workspace_state(logged_in: bool, team_name: Option<&str>) -> &'static str {
    accepted_fix_proof_readiness_state(logged_in, team_name)
}

fn team_workspace_next_command(logged_in: bool, team_name: Option<&str>) -> &'static str {
    if !logged_in {
        "difflore cloud login"
    } else if accepted_fix_proof_ready(logged_in, team_name) {
        "difflore cloud sync"
    } else {
        "open https://difflore.dev/team"
    }
}

fn team_workspace_value(logged_in: bool, team_name: Option<&str>) -> serde_json::Value {
    serde_json::json!({
        "loggedIn": logged_in,
        "teamName": team_name,
        "teamWorkspaceReady": accepted_fix_proof_ready(logged_in, team_name),
        "state": team_workspace_state(logged_in, team_name),
        "teamUrl": TEAM_WORKSPACE_URL,
        "nextCommand": team_workspace_next_command(logged_in, team_name),
        "acceptedFixProof": accepted_fix_proof_readiness_value(logged_in, team_name),
    })
}

pub(crate) async fn handle_team(json: bool) {
    let client = difflore_core::cloud::client::CloudClient::create().await;
    let status = difflore_core::cloud::sync::fetch_cloud_status(&client).await;
    if json {
        let payload = team_workspace_value(status.logged_in, status.team_name.as_deref());
        println!("{}", crate::commands::util::json_compact_or(&payload, "{}"));
        return;
    }

    if !status.logged_in {
        println!(
            "{} Not logged in to DiffLore Cloud.",
            style::pewter(style::sym::BULLET)
        );
        println!("  next: {}", style::cmd("difflore cloud login"));
        println!("  team: {TEAM_WORKSPACE_URL}");
        return;
    }

    if let Some(team) = status
        .team_name
        .as_deref()
        .filter(|team| !team.trim().is_empty())
    {
        println!("{} Team workspace ready", style::ok(style::sym::OK));
        println!("  team      {team}");
        println!("  dashboard {TEAM_WORKSPACE_URL}");
        print_accepted_fix_proof_readiness(status.logged_in, status.team_name.as_deref());
        println!("  next:     {}", style::cmd("difflore cloud sync"));
    } else {
        println!("{} No team workspace found.", style::warn(style::sym::WARN));
        println!("  Create or join one at {TEAM_WORKSPACE_URL}.");
        print_accepted_fix_proof_readiness(status.logged_in, status.team_name.as_deref());
    }
}

pub(crate) async fn handle_publish(
    rule_id: String,
    enforcement: String,
    team_id: Option<String>,
    json: bool,
) {
    let input = difflore_core::team::TeamRulePublishInput {
        rule_id: rule_id.clone(),
        enforcement: Some(enforcement.clone()),
        team_id,
        origin: None,
    };

    match difflore_core::team::publish_rule(input).await {
        Ok(published_rule_id) => {
            if json {
                let payload = serde_json::json!({
                    "success": true,
                    "ruleId": published_rule_id,
                    "enforcement": enforcement,
                });
                println!("{}", crate::commands::util::json_compact_or(&payload, "{}"));
                return;
            }

            println!(
                "{} Published rule {} to team ({enforcement}).",
                style::ok(style::sym::OK),
                style::pewter(&published_rule_id)
            );
            println!(
                "  {} next: {}",
                style::emerald(style::sym::TIP),
                style::cmd("difflore cloud sync --pull"),
            );
        }
        Err(e) => {
            let message = format!("Failed to publish rule: {e}");
            if json {
                exit_json_err("publish", &message);
            }
            exit_err(&message);
        }
    }
}

pub(crate) async fn handle_unpublish(rule_id: String, team_id: Option<String>, json: bool) {
    let input = difflore_core::team::TeamRuleUnpublishInput {
        rule_id: rule_id.clone(),
        team_id,
    };

    match difflore_core::team::unpublish_rule(input).await {
        Ok(()) => {
            if json {
                let payload = serde_json::json!({
                    "success": true,
                    "ruleId": rule_id,
                });
                println!("{}", crate::commands::util::json_compact_or(&payload, "{}"));
                return;
            }

            println!(
                "{} Unpublished rule {} from team.",
                style::ok(style::sym::OK),
                style::pewter(&rule_id)
            );
        }
        Err(e) => {
            let message = format!("Failed to unpublish rule: {e}");
            if json {
                exit_json_err("unpublish", &message);
            }
            exit_err(&message);
        }
    }
}

fn cloud_command_error_value(action: &str, message: &str) -> serde_json::Value {
    serde_json::json!({
        "success": false,
        "action": action,
        "error": message,
        "nextCommand": "difflore cloud status --json",
    })
}

fn exit_json_err(action: &str, message: &str) -> ! {
    let payload = cloud_command_error_value(action, message);
    println!("{}", crate::commands::util::json_compact_or(&payload, "{}"));
    exit_code(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepted_fix_proof_readiness_requires_team_workspace() {
        assert!(!accepted_fix_proof_ready(false, Some("Launch Partners")));
        assert!(!accepted_fix_proof_ready(true, None));
        assert!(!accepted_fix_proof_ready(true, Some("  ")));
        assert_eq!(
            accepted_fix_proof_readiness_state(false, None),
            "needs_cloud_login"
        );
        assert!(accepted_fix_proof_readiness_message(false, None).contains("difflore cloud login"),);
        assert_eq!(
            accepted_fix_proof_readiness_state(true, None),
            "needs_team_workspace"
        );
        assert!(accepted_fix_proof_readiness_message(true, None).contains("create or join a team"),);

        assert!(accepted_fix_proof_ready(true, Some("Launch Partners")));
        assert_eq!(
            accepted_fix_proof_readiness_state(true, Some("Launch Partners")),
            "team_link_ready"
        );
    }

    #[test]
    fn accepted_fix_proof_readiness_json_is_machine_readable() {
        let logged_out = accepted_fix_proof_readiness_value(false, None);
        assert_eq!(logged_out["teamWorkspaceReady"], false);
        assert_eq!(logged_out["state"], "needs_cloud_login");
        assert_eq!(logged_out["readinessScope"], "pre_capture_only");
        assert_eq!(logged_out["countsAsEvidence"], false);
        assert!(
            logged_out["countingEvidence"]
                .as_str()
                .expect("counting evidence description")
                .contains("fix_acceptances")
        );
        assert_eq!(logged_out["preCaptureRequiredFields"][0], "loggedIn=true");

        let missing_team = accepted_fix_proof_readiness_value(true, None);
        assert_eq!(missing_team["teamWorkspaceReady"], false);
        assert_eq!(missing_team["state"], "needs_team_workspace");
        assert_eq!(
            missing_team["preCaptureRequiredFields"][1],
            "acceptedFixProof.teamWorkspaceReady=true"
        );
        assert_eq!(
            missing_team["preCaptureRequiredFields"][2],
            "acceptedFixProof.state=team_link_ready"
        );

        let ready = accepted_fix_proof_readiness_value(true, Some("Launch Partners"));
        assert_eq!(ready["teamWorkspaceReady"], true);
        assert_eq!(ready["state"], "team_link_ready");
        assert!(
            ready["perFixRequiredFields"]
                .as_array()
                .expect("per-fix requirements are an array")
                .iter()
                .any(|value| value == "acceptanceSource=difflore_fix")
        );
        assert!(
            ready["perFixRequiredFields"]
                .as_array()
                .expect("per-fix requirements are an array")
                .iter()
                .any(|value| value == "targetPrNumber>0")
        );
        assert!(
            ready["nonCountingWarnings"]
                .as_array()
                .expect("non-counting warnings are an array")
                .iter()
                .any(|value| value == "missing target PR number")
        );
    }

    #[test]
    fn team_workspace_json_points_to_the_next_capture_step() {
        let logged_out = team_workspace_value(false, None);
        assert_eq!(logged_out["state"], "needs_cloud_login");
        assert_eq!(logged_out["nextCommand"], "difflore cloud login");
        assert_eq!(logged_out["teamUrl"], TEAM_WORKSPACE_URL);
        assert_eq!(logged_out["acceptedFixProof"]["state"], "needs_cloud_login");

        let missing_team = team_workspace_value(true, None);
        assert_eq!(missing_team["state"], "needs_team_workspace");
        assert_eq!(
            missing_team["nextCommand"],
            "open https://difflore.dev/team"
        );

        let ready = team_workspace_value(true, Some("Launch Partners"));
        assert_eq!(ready["state"], "team_link_ready");
        assert_eq!(ready["nextCommand"], "difflore cloud sync");
        assert_eq!(ready["acceptedFixProof"]["state"], "team_link_ready");
    }

    #[test]
    fn cloud_json_error_value_is_machine_readable() {
        let value = cloud_command_error_value("publish", "Failed to publish rule: nope");

        assert_eq!(value["success"], false);
        assert_eq!(value["action"], "publish");
        assert_eq!(value["error"], "Failed to publish rule: nope");
        assert_eq!(value["nextCommand"], "difflore cloud status --json");
    }
}
