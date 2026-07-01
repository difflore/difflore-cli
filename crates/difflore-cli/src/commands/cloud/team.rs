//! Team workspace status (`difflore cloud team`) and rule publish /
//! unpublish (`difflore cloud publish` / `unpublish`).
//!
//! Also owns the accepted-fix readiness contract shared by several cloud
//! surfaces (status, team, login): whether a logged-in user has a team
//! workspace so accepted fixes can link to team review history. Those
//! helpers are `pub(super)` so sibling cloud modules reuse the same state.

use crate::style;
use crate::support::util::{confirm_destructive, exit_code, exit_err, json_compact_or};
use serde::Serialize;
use sqlx::Row;
use std::collections::{BTreeMap, BTreeSet};

use crate::runtime::CommandContext;

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
        "run `difflore cloud login`, then create or join a team before uploading accepted edits"
    } else if accepted_fix_proof_ready(logged_in, team_name) {
        "accepted edits can upload audit proof and link to team review history"
    } else {
        "create or join a team before uploading accepted edits: https://difflore.dev/team"
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
        "linked accepted fix_outcome activity",
        "non-empty rule_id",
        "fix_acceptances.created_at >= captureBaseline.capturedAtIso",
    ]
}

const fn accepted_edit_audit_proof_sources() -> [&'static str; 2] {
    [
        "acceptanceSource=agent_retained_edit, client=difflore_hook",
        "acceptanceSource=difflore_fix, client=difflore_cli",
    ]
}

const fn accepted_fix_proof_non_counting_warnings() -> [&'static str; 6] {
    [
        "missing team workspace",
        "missing recalled rule id",
        "missing linked memory activity",
        "unexpected client",
        "missing target PR number",
        "unlinked local rule id",
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
        "auditEvidence": "cloud DB fix_acceptances rows uploaded from retained agent edits or accepted DiffLore fixes",
        "auditProofSources": accepted_edit_audit_proof_sources(),
        "countingEvidence": "cloud DB fix_acceptances rows count as accepted edit audit proof; launch-grade rule attribution additionally requires acceptanceSource=difflore_fix, client=difflore_cli, targetPrNumber>0, team_id, and linked accepted fix_outcome activity",
        "launchGradeRequiredFields": accepted_fix_proof_per_fix_required_fields(),
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
    println!("  accepted edits  {rendered}");
}

fn team_workspace_state(logged_in: bool, team_name: Option<&str>) -> &'static str {
    accepted_fix_proof_readiness_state(logged_in, team_name)
}

pub(super) fn team_workspace_next_command(
    logged_in: bool,
    team_name: Option<&str>,
) -> &'static str {
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
        println!("{}", json_compact_or(&payload, "{}"));
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
        println!(
            "  next:     {}",
            style::cmd(team_workspace_next_command(
                status.logged_in,
                status.team_name.as_deref()
            ))
        );
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
                println!("{}", json_compact_or(&payload, "{}"));
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

#[derive(Debug, Clone)]
pub(crate) struct PublishUsedArgs {
    pub(crate) team_id: Option<String>,
    pub(crate) enforcement: String,
    pub(crate) limit: Option<usize>,
    pub(crate) dry_run: bool,
    pub(crate) yes: bool,
    pub(crate) json: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct PublishUsedRule {
    local_rule_id: String,
    cloud_rule_id: Option<String>,
    outbox_rows: Vec<i64>,
    title: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct PublishUsedReport {
    dry_run: bool,
    planned: usize,
    published: usize,
    rewritten_outbox_rows: usize,
    failed: usize,
    rules: Vec<PublishUsedRule>,
}

pub(crate) async fn handle_publish_used(ctx: &CommandContext, args: PublishUsedArgs) {
    let mut plan = load_publish_used_plan(&ctx.db, args.limit)
        .await
        .unwrap_or_else(|err| exit_err(&format!("failed to inspect used local rules: {err}")));

    if args.dry_run || plan.is_empty() {
        print_publish_used_report(
            &PublishUsedReport {
                dry_run: true,
                planned: plan.len(),
                published: 0,
                rewritten_outbox_rows: 0,
                failed: 0,
                rules: plan,
            },
            args.json,
        );
        return;
    }

    if let Err(err) = confirm_destructive(
        args.yes,
        &format!(
            "publish {} used local rule(s) to the current team and rewrite accepted-edit proof rows?",
            plan.len()
        ),
    ) {
        exit_err(&err.to_string());
    }

    let mut published = 0usize;
    let mut rewritten = 0usize;
    let mut failed = 0usize;
    for item in &mut plan {
        let input = difflore_core::team::TeamRulePublishInput {
            rule_id: item.local_rule_id.clone(),
            enforcement: Some(args.enforcement.clone()),
            team_id: args.team_id.clone(),
            origin: None,
        };
        match difflore_core::team::publish_rule(input).await {
            Ok(cloud_rule_id) => {
                match rewrite_accepted_edit_outbox_rule_id(
                    &ctx.db,
                    &item.local_rule_id,
                    &cloud_rule_id,
                )
                .await
                {
                    Ok(count) => {
                        item.cloud_rule_id = Some(cloud_rule_id);
                        rewritten += count;
                        published += 1;
                    }
                    Err(err) => {
                        item.error = Some(format!("published but failed to rewrite outbox: {err}"));
                        failed += 1;
                    }
                }
            }
            Err(err) => {
                item.error = Some(err.to_string());
                failed += 1;
            }
        }
    }

    print_publish_used_report(
        &PublishUsedReport {
            dry_run: false,
            planned: plan.len(),
            published,
            rewritten_outbox_rows: rewritten,
            failed,
            rules: plan,
        },
        args.json,
    );
    if failed > 0 {
        exit_code(1);
    }
}

async fn load_publish_used_plan(
    db: &difflore_core::SqlitePool,
    limit: Option<usize>,
) -> difflore_core::Result<Vec<PublishUsedRule>> {
    let rows = sqlx::query(
        "SELECT id, payload_json \
         FROM cloud_outbox \
         WHERE kind = 'accepted_edit' \
           AND status IN ('pending', 'failed', 'abandoned', 'parked') \
         ORDER BY created_at DESC, id DESC",
    )
    .fetch_all(db)
    .await?;

    let mut outbox_by_rule: BTreeMap<String, BTreeSet<i64>> = BTreeMap::new();
    for row in rows {
        let id: i64 = row.try_get("id").unwrap_or_default();
        let payload: String = row.try_get("payload_json").unwrap_or_default();
        let Ok(request) =
            serde_json::from_str::<difflore_core::contract::RecordAcceptedEditRequest>(&payload)
        else {
            continue;
        };
        for rule_id in request.rule_ids {
            if looks_like_cloud_uuid(&rule_id) {
                continue;
            }
            outbox_by_rule.entry(rule_id).or_default().insert(id);
        }
    }

    let mut plan = Vec::new();
    for (rule_id, rows) in outbox_by_rule {
        if plan.len() >= limit.unwrap_or(usize::MAX) {
            break;
        }
        let title: Option<String> = sqlx::query_scalar("SELECT name FROM skills WHERE id = ?1")
            .bind(&rule_id)
            .fetch_optional(db)
            .await
            .ok()
            .flatten();
        plan.push(PublishUsedRule {
            local_rule_id: rule_id,
            cloud_rule_id: None,
            outbox_rows: rows.into_iter().collect(),
            title,
            error: None,
        });
    }
    Ok(plan)
}

async fn rewrite_accepted_edit_outbox_rule_id(
    db: &difflore_core::SqlitePool,
    local_rule_id: &str,
    cloud_rule_id: &str,
) -> difflore_core::Result<usize> {
    let rows = sqlx::query(
        "SELECT id, payload_json \
         FROM cloud_outbox \
         WHERE kind = 'accepted_edit' \
           AND status IN ('pending', 'failed', 'abandoned', 'parked')",
    )
    .fetch_all(db)
    .await?;
    let mut rewritten = 0usize;
    for row in rows {
        let id: i64 = row.try_get("id").unwrap_or_default();
        let payload: String = row.try_get("payload_json").unwrap_or_default();
        let Ok(mut request) =
            serde_json::from_str::<difflore_core::contract::RecordAcceptedEditRequest>(&payload)
        else {
            continue;
        };
        let mut changed = false;
        for rule_id in &mut request.rule_ids {
            if rule_id == local_rule_id {
                *rule_id = cloud_rule_id.to_owned();
                changed = true;
            }
        }
        if !changed {
            continue;
        }
        let payload = serde_json::to_string(&request)?;
        sqlx::query(
            "UPDATE cloud_outbox \
             SET payload_json = ?1, status = 'pending', retry_count = 0, claimed_at = NULL, last_error = NULL \
             WHERE id = ?2",
        )
        .bind(payload)
        .bind(id)
        .execute(db)
        .await?;
        rewritten += 1;
    }
    Ok(rewritten)
}

fn looks_like_cloud_uuid(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    for (idx, byte) in bytes.iter().enumerate() {
        if matches!(idx, 8 | 13 | 18 | 23) {
            if *byte != b'-' {
                return false;
            }
        } else if !byte.is_ascii_hexdigit() {
            return false;
        }
    }
    true
}

fn print_publish_used_report(report: &PublishUsedReport, json: bool) {
    if json {
        println!("{}", json_compact_or(report, "{}"));
        return;
    }

    println!("{}", style::title("Publish Used Rules"));
    if report.planned == 0 {
        println!("  no accepted-edit proof rows contain local-only rule ids");
        println!("  next: {}", style::cmd("difflore status --json"));
        return;
    }

    if report.dry_run {
        println!("  preview only; no cloud rules were published");
    } else {
        println!(
            "  published {} rule(s), rewrote {} outbox row(s)",
            report.published, report.rewritten_outbox_rows
        );
    }
    for item in &report.rules {
        let title = item.title.as_deref().unwrap_or("-");
        let target = item.cloud_rule_id.as_deref().unwrap_or("pending");
        println!(
            "  - {} -> {}  {}",
            style::ident(&item.local_rule_id),
            style::ident(target),
            title
        );
        if let Some(error) = &item.error {
            println!("    {}", style::amber(error));
        }
    }
    if report.dry_run {
        println!();
        println!(
            "  apply: {}",
            style::cmd("difflore cloud publish-used --yes")
        );
    } else if report.rewritten_outbox_rows > 0 {
        println!();
        println!("  sync: {}", style::cmd("difflore cloud sync"));
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
                println!("{}", json_compact_or(&payload, "{}"));
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
    println!("{}", json_compact_or(&payload, "{}"));
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
                .expect("counting accepted-edit description")
                .contains("fix_acceptances")
        );
        assert!(
            logged_out["auditEvidence"]
                .as_str()
                .expect("audit accepted-edit description")
                .contains("retained agent edits")
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
            ready["auditProofSources"]
                .as_array()
                .expect("audit proof sources are an array")
                .iter()
                .any(|value| value == "acceptanceSource=agent_retained_edit, client=difflore_hook")
        );
        assert!(
            ready["launchGradeRequiredFields"]
                .as_array()
                .expect("launch-grade requirements are an array")
                .iter()
                .any(|value| value == "acceptanceSource=difflore_fix")
        );
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
