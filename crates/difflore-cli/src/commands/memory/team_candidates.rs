use difflore_core::cloud::candidates::{
    CandidateApprovalEdits, CandidateSeverity, CandidateStatus, CountCandidatesRequest,
    ListCandidatesRequest, RuleCandidate, approve_candidate, count_candidates, get_candidate,
    list_candidates, reject_candidate,
};
use serde_json::json;

use crate::cli::{TeamCandidateCommands, TeamCandidateSeverityArg, TeamCandidateStatusArg};
use crate::runtime::CommandContext;
use crate::style;
use crate::support::util::json_compact_or;

use super::exit_structured_err;

pub(crate) struct TeamCandidateListArgs {
    pub(crate) team_id: Option<String>,
    pub(crate) limit: i64,
    pub(crate) offset: i64,
    pub(crate) status: TeamCandidateStatusArg,
    pub(crate) json: bool,
    pub(crate) command: Option<TeamCandidateCommands>,
}

pub(crate) async fn handle_team_candidates(ctx: &CommandContext, args: TeamCandidateListArgs) {
    let TeamCandidateListArgs {
        team_id,
        limit,
        offset,
        status,
        json,
        command,
    } = args;
    let parent_json = json;
    match command {
        None => {
            handle_team_candidates_list(ctx, team_id, limit, offset, status, parent_json).await;
        }
        Some(TeamCandidateCommands::Count {
            team_id: count_team_id,
            status: count_status,
            json,
        }) => {
            handle_team_candidates_count(
                ctx,
                count_team_id.or(team_id),
                count_status.unwrap_or(status),
                json || parent_json,
            )
            .await;
        }
        Some(TeamCandidateCommands::Show { candidate_id, json }) => {
            handle_team_candidate_show(ctx, candidate_id, json || parent_json).await;
        }
        Some(TeamCandidateCommands::Approve {
            candidate_id,
            name,
            description,
            severity,
            content,
            json,
        }) => {
            handle_team_candidate_approve(
                ctx,
                candidate_id,
                CandidateApprovalInput {
                    name,
                    description,
                    severity,
                    content,
                },
                json || parent_json,
            )
            .await;
        }
        Some(TeamCandidateCommands::Reject {
            candidate_id,
            reason,
            json,
        }) => {
            handle_team_candidate_reject(ctx, candidate_id, reason, json || parent_json).await;
        }
    }
}

async fn handle_team_candidates_list(
    ctx: &CommandContext,
    team_id: Option<String>,
    limit: i64,
    offset: i64,
    status: TeamCandidateStatusArg,
    json: bool,
) {
    let team_id = resolve_team_id(ctx, team_id, json).await;
    let status_filter = status.into_candidate_status();
    let candidates = list_candidates(
        ctx.cloud().await,
        ListCandidatesRequest {
            team_id: team_id.clone(),
            limit: Some(limit.clamp(1, 20)),
            offset: Some(offset.max(0)),
            status: status_filter,
        },
    )
    .await
    .unwrap_or_else(|err| {
        exit_structured_err(
            &format!("failed to load team memory suggestions: {err}"),
            json,
        )
    });

    if json {
        println!(
            "{}",
            json_compact_or(
                &json!({
                    "schemaVersion": "team-memory-candidates.v1",
                    "teamId": team_id,
                    "status": status.as_wire_value(),
                    "count": candidates.len(),
                    "candidates": candidates,
                }),
                "{}"
            )
        );
        return;
    }

    print_candidate_list(&team_id, status, &candidates);
}

async fn handle_team_candidates_count(
    ctx: &CommandContext,
    team_id: Option<String>,
    status: TeamCandidateStatusArg,
    json: bool,
) {
    let team_id = resolve_team_id(ctx, team_id, json).await;
    let count = count_candidates(
        ctx.cloud().await,
        CountCandidatesRequest {
            team_id: team_id.clone(),
            status: status.into_candidate_status(),
        },
    )
    .await
    .unwrap_or_else(|err| {
        exit_structured_err(
            &format!("failed to count team memory suggestions: {err}"),
            json,
        )
    });

    if json {
        println!(
            "{}",
            json_compact_or(
                &json!({
                    "schemaVersion": "team-memory-candidates.v1",
                    "teamId": team_id,
                    "status": status.as_wire_value(),
                    "total": count.total,
                }),
                "{}"
            )
        );
        return;
    }

    println!("{}", style::title("Team Memory Suggestions"));
    println!(
        "  {} {} for team {}",
        style::ident(&format_count(count.total)),
        status_label(status),
        style::ident(&team_id)
    );
}

async fn handle_team_candidate_show(ctx: &CommandContext, candidate_id: String, json: bool) {
    ensure_logged_in(ctx, json).await;
    let detail = get_candidate(ctx.cloud().await, candidate_id.trim())
        .await
        .unwrap_or_else(|err| {
            exit_structured_err(
                &format!("failed to load team memory suggestion: {err}"),
                json,
            )
        });

    if json {
        println!("{}", json_compact_or(&detail, "{}"));
        return;
    }

    print_candidate_detail(&detail.candidate);
    if !detail.events.is_empty() {
        println!();
        println!("{}", style::title("History"));
        for event in detail.events.iter().take(5) {
            println!(
                "  {} {} {}",
                style::pewter(style::sym::BULLET),
                event.event_type,
                style::pewter(&event.created_at)
            );
        }
    }
}

struct CandidateApprovalInput {
    name: Option<String>,
    description: Option<String>,
    severity: Option<TeamCandidateSeverityArg>,
    content: Option<String>,
}

async fn handle_team_candidate_approve(
    ctx: &CommandContext,
    candidate_id: String,
    input: CandidateApprovalInput,
    json: bool,
) {
    ensure_logged_in(ctx, json).await;
    let edits = approval_edits(input);
    let response = approve_candidate(ctx.cloud().await, candidate_id.trim(), edits)
        .await
        .unwrap_or_else(|err| {
            exit_structured_err(
                &format!("failed to approve team memory suggestion: {err}"),
                json,
            )
        });

    if json {
        println!(
            "{}",
            json_compact_or(
                &json!({
                    "action": "approved",
                    "candidateId": response.candidate_id,
                    "ruleId": response.rule_id,
                }),
                "{}"
            )
        );
        return;
    }

    println!(
        "{} Approved team memory suggestion {} into rule {}.",
        style::ok(style::sym::OK),
        style::ident(&response.candidate_id),
        style::ident(&response.rule_id)
    );
    println!("  next: {}", style::cmd("difflore memory sync"));
}

async fn handle_team_candidate_reject(
    ctx: &CommandContext,
    candidate_id: String,
    reason: Option<String>,
    json: bool,
) {
    ensure_logged_in(ctx, json).await;
    reject_candidate(ctx.cloud().await, candidate_id.trim(), reason)
        .await
        .unwrap_or_else(|err| {
            exit_structured_err(
                &format!("failed to reject team memory suggestion: {err}"),
                json,
            )
        });

    if json {
        println!(
            "{}",
            json_compact_or(
                &json!({
                    "action": "rejected",
                    "candidateId": candidate_id,
                }),
                "{}"
            )
        );
        return;
    }

    println!(
        "{} Rejected team memory suggestion {}.",
        style::ok(style::sym::OK),
        style::ident(&candidate_id)
    );
}

async fn resolve_team_id(ctx: &CommandContext, explicit: Option<String>, json: bool) -> String {
    ensure_logged_in(ctx, json).await;
    if let Some(team_id) = explicit
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
    {
        return team_id;
    }

    let status = difflore_core::cloud::sync::fetch_cloud_status(ctx.cloud().await).await;
    status
        .team_id
        .filter(|team_id| !team_id.trim().is_empty())
        .unwrap_or_else(|| {
            exit_structured_err(
                "no cloud team workspace found; pass --team-id or create/join a team",
                json,
            )
        })
}

async fn ensure_logged_in(ctx: &CommandContext, json: bool) {
    if !ctx.cloud().await.is_logged_in() {
        exit_structured_err(
            "not logged in to DiffLore Cloud; run `difflore cloud login`",
            json,
        );
    }
}

fn approval_edits(input: CandidateApprovalInput) -> Option<CandidateApprovalEdits> {
    let edits = CandidateApprovalEdits {
        name: non_empty(input.name),
        description: non_empty(input.description),
        severity: input
            .severity
            .map(TeamCandidateSeverityArg::into_candidate_severity),
        content: non_empty(input.content),
    };
    if edits.name.is_none()
        && edits.description.is_none()
        && edits.severity.is_none()
        && edits.content.is_none()
    {
        None
    } else {
        Some(edits)
    }
}

fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn print_candidate_list(
    team_id: &str,
    status: TeamCandidateStatusArg,
    candidates: &[RuleCandidate],
) {
    println!("{}", style::title("Team Memory Suggestions"));
    println!(
        "  team   {}",
        style::ident(if team_id.is_empty() { "-" } else { team_id })
    );
    println!("  view   {}", status_label(status));
    if candidates.is_empty() {
        println!("  result no suggestions need attention");
        println!("  next:  {}", style::cmd("difflore memory"));
        return;
    }

    println!();
    for candidate in candidates {
        println!(
            "  {} {} {}",
            style::warn(style::sym::WARN),
            style::ident(&candidate.generated_name),
            style::pewter(&candidate.id)
        );
        println!(
            "    signal: {} accepted fixes, {} users | severity {} | {}",
            format_count(candidate.acceptance_count),
            format_count(candidate.distinct_users),
            candidate.generated_severity,
            candidate.language.as_deref().unwrap_or("unknown language")
        );
        println!(
            "    review: {}",
            style::cmd(&format!(
                "difflore memory team-candidates show {}",
                candidate.id
            ))
        );
    }
}

fn print_candidate_detail(candidate: &RuleCandidate) {
    println!("{}", style::title("Team Memory Suggestion"));
    println!("  id       {}", style::ident(&candidate.id));
    println!("  title    {}", candidate.generated_name);
    println!("  status   {}", candidate.status);
    println!(
        "  signal   {} accepted fixes, {} users",
        format_count(candidate.acceptance_count),
        format_count(candidate.distinct_users)
    );
    println!("  severity {}", candidate.generated_severity);
    if let Some(language) = candidate.language.as_deref() {
        println!("  language {language}");
    }
    println!();
    println!("{}", style::title("Rule Text"));
    println!("  {}", candidate.generated_description);
    println!();
    println!(
        "approve: {}",
        style::cmd(&format!(
            "difflore memory team-candidates approve {}",
            candidate.id
        ))
    );
    println!(
        "reject:  {}",
        style::cmd(&format!(
            "difflore memory team-candidates reject {}",
            candidate.id
        ))
    );
}

const fn status_label(status: TeamCandidateStatusArg) -> &'static str {
    match status {
        TeamCandidateStatusArg::Pending => "pending suggestions",
        TeamCandidateStatusArg::Approved => "approved suggestions",
        TeamCandidateStatusArg::Rejected => "rejected suggestions",
        TeamCandidateStatusArg::All => "all suggestions",
    }
}

fn format_count(value: f64) -> String {
    if value.fract().abs() < f64::EPSILON {
        format!("{}", value as i64)
    } else {
        format!("{value:.1}")
    }
}

impl TeamCandidateStatusArg {
    const fn into_candidate_status(self) -> Option<CandidateStatus> {
        match self {
            Self::Pending => Some(CandidateStatus::Pending),
            Self::Approved => Some(CandidateStatus::Approved),
            Self::Rejected => Some(CandidateStatus::Rejected),
            Self::All => None,
        }
    }

    const fn as_wire_value(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
            Self::All => "all",
        }
    }
}

impl TeamCandidateSeverityArg {
    const fn into_candidate_severity(self) -> CandidateSeverity {
        match self {
            Self::Info => CandidateSeverity::Info,
            Self::Warning => CandidateSeverity::Warning,
            Self::Error => CandidateSeverity::Error,
        }
    }
}
