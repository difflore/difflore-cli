pub(crate) mod login;
pub(crate) mod sync;

mod auth;
mod impact;
mod team;

use crate::style;

use crate::support::util::{exit_code, exit_err};
use difflore_core::cloud::observations::{ActualCitationSummary, ObservationUploadIssue};

use auth::DeviceRegistrationState;

// Re-exported from the per-domain modules so the dispatch layer keeps calling
// `commands::cloud::handle_*`.
pub(crate) use impact::handle_impact;
pub(crate) use team::{handle_publish, handle_team, handle_unpublish};

pub(crate) async fn handle_status(json: bool) {
    let client = difflore_core::cloud::client::CloudClient::create().await;
    let status = difflore_core::cloud::sync::fetch_cloud_status(&client).await;
    if status.logged_in {
        refresh_agent_usage_uploads(&client).await;
    }
    let agent_usage = if status.logged_in {
        load_agent_usage_summary().await
    } else {
        None
    };
    let device_registration = if status.logged_in {
        auth::load_device_registration_state()
    } else {
        None
    };

    if json {
        let payload =
            cloud_status_value(&status, agent_usage.as_ref(), device_registration.as_ref());
        println!("{}", crate::support::util::json_compact_or(&payload, "{}"));
        return;
    }

    if !status.logged_in {
        println!(
            "{} Not logged in to DiffLore Cloud.",
            style::pewter(style::sym::BULLET)
        );
        println!(
            "  Local memory still works. Connect with {} to enable team sync.",
            style::cmd("difflore cloud login")
        );
        println!("  Team impact and accepted-fix counts unlock after login.");
        println!("  next: {}", style::cmd("difflore cloud login"));
        return;
    }

    println!("{} Logged in", style::ok(style::sym::OK));
    if let Some(email) = status.email.as_deref() {
        println!("  account   {email}");
    }
    if let Some(plan) = status.plan.as_deref() {
        println!("  plan      {plan}");
    }
    if let Some(team) = status.team_name.as_deref() {
        println!("  team      {team}");
    }
    team::print_accepted_fix_proof_readiness(status.logged_in, status.team_name.as_deref());
    auth::print_device_registration_status(device_registration.as_ref());
    if let Some(line) = agent_usage_pending_upload_line(agent_usage.as_ref()) {
        println!("  activity  {}", style::pewter(&line));
    }
    println!();
    println!(
        "  {} next: {}",
        style::emerald(style::sym::TIP),
        style::cmd(team::team_workspace_next_command(
            status.logged_in,
            status.team_name.as_deref()
        )),
    );
}

fn cloud_status_value(
    status: &difflore_core::cloud::sync::CloudStatus,
    agent_usage: Option<&ActualCitationSummary>,
    device_registration: Option<&DeviceRegistrationState>,
) -> serde_json::Value {
    serde_json::json!({
        "loggedIn": status.logged_in,
        "email": status.email,
        "plan": status.plan,
        "teamId": status.team_id,
        "teamName": status.team_name,
        "acceptedFixProof": team::accepted_fix_proof_readiness_value(
            status.logged_in,
            status.team_name.as_deref(),
        ),
        "agentUsage": agent_usage_value(agent_usage),
        "deviceRegistration": auth::device_registration_value(device_registration),
    })
}

pub(crate) async fn handle_login_dispatch(
    token_flag: Option<String>,
    force_browser: bool,
    github: bool,
) {
    let used_token_flag = token_flag.as_ref().is_some_and(|s| !s.trim().is_empty());
    if let Err(e) = auth::try_login_dispatch_with_github(token_flag, force_browser, github).await {
        eprintln!("{} {e}", style::err(style::sym::ERR));
        if github {
            auth::print_github_login_recovery();
        } else if used_token_flag {
            eprintln!();
            eprintln!("  next: {}", style::cmd("difflore cloud login"));
            eprintln!(
                "  Headless/CI? See {}.",
                style::cmd("difflore cloud login --help")
            );
        } else {
            auth::print_browser_login_recovery();
        }
        exit_code(1);
    }
}

pub(crate) async fn try_login_dispatch(
    token_flag: Option<String>,
    force_browser: bool,
) -> Result<(), String> {
    auth::try_login_dispatch_with_github(token_flag, force_browser, false).await
}

async fn load_agent_usage_summary() -> Option<ActualCitationSummary> {
    let summary = difflore_core::cloud::observations::actual_citation_summary_default(7)
        .await
        .ok()?;
    if summary.actual_citations == 0 && summary.rule_fires == 0 {
        None
    } else {
        Some(summary)
    }
}

async fn refresh_agent_usage_uploads(client: &difflore_core::cloud::client::CloudClient) {
    let Ok(emitter) = difflore_core::cloud::observations::ObservationEmitter::open_default().await
    else {
        return;
    };
    let _ = emitter.retry_pending_uploads_now().await;
    let _ = emitter.flush_to_cloud(client).await;
}

fn agent_usage_text_label(summary: &ActualCitationSummary) -> String {
    format!(
        "{} actual agent citation{}",
        summary.actual_citations,
        if summary.actual_citations == 1 {
            ""
        } else {
            "s"
        },
    )
}

fn agent_usage_value(summary: Option<&ActualCitationSummary>) -> serde_json::Value {
    summary.map_or(serde_json::Value::Null, |s| {
        serde_json::json!({
            "windowDays": 7,
            "actualCitations": s.actual_citations,
            "ruleFires": s.rule_fires,
            "pendingUploads": s.pending_uploads,
            "pendingUploadIssue": s.pending_upload_issue.map(ObservationUploadIssue::as_str),
            "pendingUploadState": agent_usage_pending_upload_state(s),
            "pendingUploadAction": agent_usage_pending_upload_recovery(s),
            "actualCitationRate": if s.rule_fires > 0 {
                Some(s.actual_citations as f64 / s.rule_fires as f64)
            } else {
                None
            },
        })
    })
}

const fn agent_usage_pending_upload_state(summary: &ActualCitationSummary) -> Option<&'static str> {
    if summary.pending_uploads == 0 {
        return None;
    }
    Some(match summary.pending_upload_issue {
        Some(ObservationUploadIssue::MissingCloudScope) => "queued_needs_login_refresh",
        Some(ObservationUploadIssue::RateLimited) => "queued_retrying",
        Some(ObservationUploadIssue::InvalidBatch) => "queued_needs_schema_update",
        Some(ObservationUploadIssue::ServerRejected) => "queued_needs_attention",
        Some(ObservationUploadIssue::Unknown) | None => "queued",
    })
}

const fn agent_usage_pending_upload_recovery(
    summary: &ActualCitationSummary,
) -> Option<&'static str> {
    if summary.pending_uploads == 0 {
        return None;
    }
    Some(match summary.pending_upload_issue {
        Some(ObservationUploadIssue::MissingCloudScope) => {
            "memory activity is pending; refresh login once to upload: difflore cloud login"
        }
        Some(ObservationUploadIssue::RateLimited) => {
            "memory activity uploads are rate-limited and will retry automatically"
        }
        Some(ObservationUploadIssue::InvalidBatch) => {
            "memory activity uploads need the latest cloud version"
        }
        Some(ObservationUploadIssue::ServerRejected) => {
            "memory activity uploads were rejected; run difflore doctor --report"
        }
        Some(ObservationUploadIssue::Unknown) | None => {
            "memory activity uploads are pending; run difflore doctor --report if they stay pending"
        }
    })
}

fn agent_usage_pending_upload_line(summary: Option<&ActualCitationSummary>) -> Option<String> {
    let summary = summary?;
    if summary.pending_uploads == 0 {
        return None;
    }
    let mut line = format!(
        "{} memory activity upload{} pending",
        summary.pending_uploads,
        if summary.pending_uploads == 1 {
            ""
        } else {
            "s"
        },
    );
    if let Some(recovery) = agent_usage_pending_upload_recovery(summary) {
        line.push_str(" | ");
        line.push_str(recovery);
    }
    Some(line)
}

pub(crate) async fn handle_logout() {
    match difflore_core::cloud::client::CloudClient::clear_token().await {
        Ok(()) => {
            auth::clear_device_registration_state();
            println!(
                "{} Cloud token cleared on this device.",
                style::ok(style::sym::OK)
            );
        }
        Err(e) => exit_err(&format!("Failed to clear token: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_usage_text_label_formats_actual_citation_count() {
        let summary = ActualCitationSummary {
            actual_citations: 2,
            rule_fires: 5,
            pending_uploads: 0,
            pending_upload_issue: None,
        };

        assert_eq!(agent_usage_text_label(&summary), "2 actual agent citations");
    }

    #[test]
    fn agent_usage_value_keeps_pending_upload_count_visible() {
        let summary = ActualCitationSummary {
            actual_citations: 2,
            rule_fires: 5,
            pending_uploads: 1,
            pending_upload_issue: Some(ObservationUploadIssue::MissingCloudScope),
        };

        let value = agent_usage_value(Some(&summary));

        assert_eq!(value["actualCitations"], 2);
        assert_eq!(value["ruleFires"], 5);
        assert_eq!(value["pendingUploads"], 1);
        assert_eq!(value["pendingUploadIssue"], "missing_cloud_scope");
        assert_eq!(value["pendingUploadState"], "queued_needs_login_refresh");
        assert_eq!(
            value["pendingUploadAction"],
            "memory activity is pending; refresh login once to upload: difflore cloud login"
        );
        assert_eq!(value["actualCitationRate"], 0.4);
    }

    #[test]
    fn pending_upload_line_carries_count_and_recovery() {
        let summary = ActualCitationSummary {
            actual_citations: 2,
            rule_fires: 5,
            pending_uploads: 2,
            pending_upload_issue: Some(ObservationUploadIssue::MissingCloudScope),
        };

        assert_eq!(
            agent_usage_pending_upload_line(Some(&summary)).as_deref(),
            Some(
                "2 memory activity uploads pending | memory activity is pending; refresh login once to upload: difflore cloud login"
            )
        );
    }

    #[test]
    fn cloud_status_json_surfaces_partner_readiness_contract() {
        let status = difflore_core::cloud::sync::CloudStatus {
            logged_in: true,
            email: Some("partner@example.com".to_owned()),
            plan: Some("team".to_owned()),
            team_id: Some("team_123".to_owned()),
            team_name: Some("Launch Partners".to_owned()),
        };
        let value = cloud_status_value(&status, None, None);

        assert_eq!(value["loggedIn"], true);
        assert_eq!(value["teamId"], "team_123");
        assert_eq!(value["teamName"], "Launch Partners");
        assert_eq!(value["acceptedFixProof"]["teamWorkspaceReady"], true);
        assert_eq!(value["acceptedFixProof"]["state"], "team_link_ready");
        assert_eq!(
            value["acceptedFixProof"]["readinessScope"],
            "pre_capture_only"
        );
        assert_eq!(value["acceptedFixProof"]["countsAsEvidence"], false);
    }
}
