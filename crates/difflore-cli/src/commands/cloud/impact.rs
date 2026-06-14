//! `difflore cloud impact`: fetch and render the cloud Impact panels (banner,
//! weekly trend, top rules, coverage, fix scorecard), the JSON payload, and
//! the "not logged in / session unverified" fallbacks.
//!
//! Agent-usage evidence helpers (shared with `cloud status`) live in
//! [`super`]; this module only renders them.

use colored::Colorize;

use crate::style;

pub(crate) async fn handle_impact(ctx: &crate::runtime::CommandContext, json: bool) {
    let client = ctx.cloud().await;
    if !client.is_logged_in() {
        if json {
            let payload = impact_logged_out_value();
            println!("{}", crate::support::util::json_compact_or(&payload, "{}"));
        } else {
            println!(
                "{} Not logged in to DiffLore Cloud.",
                style::pewter(style::sym::BULLET)
            );
            println!(
                "  Impact shows accepted-fix counts, top recalled rules, and review-effort trends."
            );
            println!("  none of which are computable from local-only data.");
            println!();
            println!("  next: {}", style::cmd("difflore cloud login"));
        }
        return;
    }

    let cloud_status = difflore_core::cloud::sync::fetch_cloud_status(client).await;
    if !cloud_status.logged_in {
        if json {
            let payload = impact_unverified_session_value();
            println!("{}", crate::support::util::json_compact_or(&payload, "{}"));
        } else {
            println!(
                "{} Cloud session could not be verified.",
                style::danger(style::sym::ERR)
            );
            println!("  Re-run login, or retry if Cloud is temporarily unreachable.");
            println!("  next: {}", style::cmd("difflore cloud login"));
        }
        return;
    }

    super::refresh_agent_usage_uploads(client).await;

    let (banner, weekly, top_rules, coverage, fix, agent_usage) = tokio::join!(
        client.get_impact_banner(),
        client.get_impact_weekly(),
        client.get_impact_top_rules(),
        client.get_impact_coverage(),
        client.get_impact_fix_scorecard(),
        async { super::load_agent_usage_summary().await },
    );

    if json {
        let accepted_proof_sources =
            crate::support::impact_payload::fetch_accepted_proof_sources_for_top_rules(
                ctx,
                &top_rules,
                usize::MAX,
            )
            .await;
        let mut payload =
            crate::support::impact_payload::shared_sections_with_accepted_proof_sources(
                &crate::support::impact_payload::ImpactPayloadInputs {
                    banner: &banner,
                    weekly: &weekly,
                    top_rules: &top_rules,
                    coverage: &coverage,
                    fix_scorecard: &fix,
                },
                &accepted_proof_sources,
            );
        payload.insert("loggedIn".to_owned(), serde_json::Value::Bool(true));
        payload.insert(
            "plan".to_owned(),
            serde_json::to_value(&cloud_status.plan).unwrap_or(serde_json::Value::Null),
        );
        payload.insert(
            "team".to_owned(),
            serde_json::to_value(&cloud_status.team_name).unwrap_or(serde_json::Value::Null),
        );
        payload.insert(
            "teamId".to_owned(),
            serde_json::to_value(&cloud_status.team_id).unwrap_or(serde_json::Value::Null),
        );
        payload.insert(
            "agentUsage".to_owned(),
            super::agent_usage_value(agent_usage.as_ref()),
        );
        let payload = serde_json::Value::Object(payload);
        println!("{}", crate::support::util::json_or(&payload, "{}"));
        return;
    }

    println!("{}", "DiffLore Impact".bold());
    println!(
        "{}",
        style::pewter("How much DiffLore has helped your reviews, rules, and AI-assisted coding.")
    );
    println!();

    match &banner {
        Ok(b) => {
            println!(
                "  {} {} {}",
                style::pewter(style::sym::BULLET),
                style::ok(&b.past_verdicts_this_week.to_string()),
                style::pewter("past team decisions recalled into your reviews this week")
            );
        }
        Err(e) => println!(
            "  {} {}",
            style::amber("!"),
            impact_panel_error("banner", &e.to_string())
        ),
    }

    if let Some(agent_usage) = agent_usage.as_ref()
        && agent_usage.rule_fires > 0
    {
        println!();
        println!("  {}", "Agent usage - last 7 days".bold());
        let pending = if agent_usage.pending_uploads > 0 {
            format!(
                " | {} pending upload{}",
                agent_usage.pending_uploads,
                if agent_usage.pending_uploads == 1 {
                    ""
                } else {
                    "s"
                },
            )
        } else {
            String::new()
        };
        println!(
            "    {} {} {}{}",
            style::ok(&super::agent_usage_text_label(agent_usage)),
            style::pewter(style::sym::BULLET),
            style::pewter(&format!("{} memory fires observed", agent_usage.rule_fires)),
            style::pewter(&pending),
        );
        if let Some(recovery) = super::agent_usage_pending_upload_recovery(agent_usage) {
            println!("    {} {}", style::amber("!"), style::pewter(recovery));
        }
    }

    if let Ok(w) = &weekly
        && !w.weeks.is_empty()
    {
        let max = w
            .weeks
            .iter()
            .map(|p| p.rules_sedimented + p.past_verdicts_recalled + p.fixes_accepted)
            .max()
            .unwrap_or(0)
            .max(1);
        let blocks = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
        let bar: String = w
            .weeks
            .iter()
            .map(|p| {
                let total = p.rules_sedimented + p.past_verdicts_recalled + p.fixes_accepted;
                let idx = ((total as f64 / max as f64) * 7.0).round() as usize;
                blocks[idx.min(7)]
            })
            .collect();
        println!();
        println!("  {}", "Last 12 weeks".bold());
        println!("  {}  (max {})", style::emerald(&bar), max);
        println!(
            "  {}",
            style::pewter("rules learned | past verdicts recalled | fixes accepted")
        );
    }

    println!();
    println!("  {}", "Top rules this cycle".bold());
    match &top_rules {
        Ok(r) if !r.rules.is_empty() => {
            let local_proof_sources = if r
                .rules
                .iter()
                .any(|rule| rule.accepted_proof_source.is_none())
            {
                let ids: Vec<String> = r.rules.iter().map(|x| x.id.clone()).collect();
                crate::support::impact_payload::fetch_accepted_proof_sources(&ctx.db, &ids).await
            } else {
                std::collections::HashMap::new()
            };
            for rule in &r.rules {
                let meta = match (&rule.severity, &rule.language) {
                    (Some(s), Some(l)) => format!(" [{s} | {l}]"),
                    (Some(s), None) => format!(" [{s}]"),
                    (None, Some(l)) => format!(" [{l}]"),
                    _ => String::new(),
                };
                let trust = rule.trust_rate.map_or_else(String::new, |rate| {
                    let pct = (rate * 100.0).round() as i64;
                    if rule.cited_count > 0 {
                        format!(" | trust {pct}% ({} cited)", rule.cited_count)
                    } else {
                        format!(" | trust {pct}%")
                    }
                });
                let proof = crate::support::impact_payload::accepted_proof_source_label(
                    rule.accepted_proof_source
                        .as_deref()
                        .or_else(|| local_proof_sources.get(&rule.id).map(String::as_str)),
                )
                .map_or_else(String::new, |label| format!(" | {}", style::pewter(label)));
                let agent_ready = crate::support::impact_payload::agent_ready_proof_label(
                    rule.reviewer_proof_ready_count,
                )
                .map_or_else(String::new, |label| format!(" | {}", style::pewter(&label)));
                let reviewer_context =
                    crate::support::impact_payload::reviewer_context_proof_label(
                        rule.reviewer_context_serves,
                        rule.reviewer_mentions,
                    )
                    .map_or_else(String::new, |label| format!(" | {}", style::pewter(&label)));
                let source_repo = rule
                    .source_repo
                    .as_deref()
                    .map_or_else(String::new, |repo| {
                        format!(" | {}", style::pewter(&format!("learned from {repo}")))
                    });
                println!(
                    "    {} {}{} - {} accepted, {} user{}{trust}{proof}{agent_ready}{reviewer_context}{source_repo}",
                    style::pewter(style::sym::BULLET),
                    rule.name.bold(),
                    style::pewter(&meta),
                    style::emerald(&rule.acceptance_count.to_string()),
                    rule.distinct_users,
                    if rule.distinct_users == 1 { "" } else { "s" },
                );
            }
        }
        Ok(r) => {
            if let Some(progress) = r.promotion_progress.first() {
                let target = progress
                    .file_path
                    .as_deref()
                    .or(progress.language.as_deref())
                    .unwrap_or("this pattern");
                println!(
                    "    {}",
                    style::pewter(&format!(
                        "Candidate warming up: {}/{} matching fixes and {}/{} users for {target}.",
                        progress.acceptance_count,
                        progress.required_count,
                        progress.distinct_users,
                        progress.required_distinct_users,
                    ))
                );
            } else {
                println!(
                    "    {}",
                    style::pewter(
                        "No candidate rules yet. As your team accepts fixes, patterns will surface here."
                    )
                );
            }
        }
        Err(e) => println!(
            "    {} {}",
            style::amber("!"),
            impact_panel_error("top rules", &e.to_string())
        ),
    }

    println!();
    println!("  {}", "Coverage".bold());
    match &coverage {
        Ok(c) => {
            let ai_label = if c.ai_reviewer_comments_indexed > 0 {
                format!(" | {} AI reviewer signals", c.ai_reviewer_comments_indexed)
            } else {
                String::new()
            };
            println!(
                "    {} repos | {} PRs | {} review comments{} | {} files",
                style::emerald(&c.repos.to_string()),
                style::emerald(&c.prs.to_string()),
                style::emerald(&c.review_comments_indexed.to_string()),
                style::pewter(&ai_label),
                style::emerald(&c.files.to_string())
            );
        }
        Err(e) => println!(
            "    {} {}",
            style::amber("!"),
            impact_panel_error("coverage", &e.to_string())
        ),
    }

    println!();
    println!("  {}", "Fix acceptance - last 30 days".bold());
    match &fix {
        Ok(f) => {
            let rate = if f.last30.total > 0 {
                Some((f.last30.accepted as f64 / f.last30.total as f64) * 100.0)
            } else {
                None
            };
            let rate_str = match rate {
                Some(r) => format!("{r:.0}%"),
                None => "-".to_owned(),
            };
            let mut line = format!(
                "    {} ({} / {} fixes accepted)",
                style::ok(&rate_str),
                f.last30.accepted,
                f.last30.total
            );
            if let Some(label) = crate::support::impact_payload::saved_review_time_label(
                crate::support::impact_payload::saved_review_minutes_for_scorecard(f),
            ) {
                line.push_str(&format!(" {}", style::pewter(&format!("| {label}"))));
            }
            if let Some(t) = f.trend_pct {
                let sign = if t >= 0.0 { "+" } else { "-" };
                let trend = format!(" {sign}{:.0}% vs prior 30d", t.abs());
                line.push_str(&if t >= 0.0 {
                    style::emerald(&trend).to_string()
                } else {
                    style::danger(&trend).to_string()
                });
            }
            println!("{line}");
        }
        Err(e) => println!(
            "    {} {}",
            style::amber("!"),
            impact_panel_error("fix acceptance", &e.to_string())
        ),
    }

    println!();
    let plan_tier = difflore_core::cloud::sync::cloud_tier_from_status(&cloud_status);
    let prs = coverage.as_ref().map_or(0, |c| c.prs);
    let fixes_total = fix.as_ref().map_or(0, |f| f.last30.total);
    let has_signal = prs > 0 || fixes_total > 0;
    let team_ready = super::team::accepted_fix_proof_ready(
        cloud_status.logged_in,
        cloud_status.team_name.as_deref(),
    );

    if !team_ready {
        println!("  {}", "Next steps".bold());
        println!(
            "    {} next: {}",
            style::pewter(style::sym::BULLET),
            style::cmd(super::team::team_workspace_next_command(
                cloud_status.logged_in,
                cloud_status.team_name.as_deref()
            ))
        );
        println!(
            "    {} then: {}",
            style::pewter(style::sym::BULLET),
            style::cmd("difflore cloud sync")
        );
        return;
    }

    if !has_signal {
        println!("  {}", "Next steps".bold());
        println!(
            "    {} {}",
            style::pewter(style::sym::BULLET),
            style::pewter(
                "Sync more reviews via `difflore cloud sync` so this report can show real signal."
            )
        );
        println!(
            "    {} {}",
            style::pewter(style::sym::BULLET),
            style::pewter("Connect the GitHub App on Cloud Team for auto-review on PR push.")
        );
        return;
    }

    if plan_tier.is_team() {
        let (label, team_suffix) =
            impact_plan_line(plan_tier, cloud_status.team_name.as_deref());
        println!(
            "  {} {} {}{}",
            "Plan".bold(),
            style::ok(&label),
            style::ok(style::sym::OK),
            style::pewter(&team_suffix)
        );
        return;
    }

    println!("  {}", "Why Cloud Team".bold());
    if prs > 0 {
        println!(
            "    You've reviewed {} PR{} locally. Cloud Team's GitHub App learns \
             from review history and shares governed rules with every agent.",
            style::emerald(&prs.to_string()),
            if prs == 1 { "" } else { "s" }
        );
    }
    if fixes_total >= 5 {
        println!(
            "    {} local fix outcome{} were recorded in 30d. Cloud plans add \
             shared team rules, GitHub App ingest, Reviewer Context, team controls, \
             and impact analytics.",
            style::emerald(&fixes_total.to_string()),
            if fixes_total == 1 { "" } else { "s" }
        );
    }
    println!(
        "    {} {}",
        style::pewter("Upgrade:"),
        difflore_core::cloud::endpoints::pricing_url()
    );
}

/// Plan line for the `impact` summary: the shared tier label plus an optional
/// `| team `name`` suffix.
///
/// Deliberately uses [`CloudTier::default_label`] rather than
/// `cloud_plan_label_from_status` (which returns the *team name* when one is
/// set): the team name is already shown in the suffix, so reusing it as the
/// label duplicated it and dropped the tier label. Using the tier label here
/// also matches what `difflore init` / `doctor` render, so the same login shows
/// one consistent plan name across surfaces.
fn impact_plan_line(
    tier: difflore_core::cloud::sync::CloudTier,
    team_name: Option<&str>,
) -> (String, String) {
    let label = tier.default_label().to_owned();
    let team_suffix = team_name
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| format!(" | team `{name}`"))
        .unwrap_or_default();
    (label, team_suffix)
}

fn impact_logged_out_value() -> serde_json::Value {
    impact_needs_login_value(
        "needs_cloud_login",
        "cloud_login_required",
        "Impact needs cloud-linked activity to show accepted-fix counts, top recalled rules, and review-effort trends.",
    )
}

fn impact_unverified_session_value() -> serde_json::Value {
    impact_needs_login_value(
        "cloud_session_unverified",
        "cloud_session_unverified",
        "Cloud session could not be verified; re-run login, or retry if Cloud is temporarily unreachable.",
    )
}

fn impact_needs_login_value(
    state: &str,
    reason: &str,
    value_description: &str,
) -> serde_json::Value {
    serde_json::json!({
        "loggedIn": false,
        "state": state,
        "reason": reason,
        "nextCommand": "difflore cloud login",
        "readinessCommand": "difflore cloud team --json",
        "valueDescription": value_description,
        "unavailablePanels": [
            "acceptedFixCounts",
            "topRecalledRules",
            "reviewEffortTrends",
        ],
    })
}

pub(super) fn impact_panel_error(panel: &str, err: &str) -> String {
    if cloud_scope_missing_error(err) {
        format!("{panel} needs a refreshed cloud session: difflore cloud login")
    } else {
        format!("{panel} unavailable: {err}")
    }
}

fn cloud_scope_missing_error(err: &str) -> bool {
    let lower = err.to_ascii_lowercase();
    lower.contains("scope_missing") || lower.contains("missing required scope")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn impact_panel_error_humanizes_missing_scope() {
        let err = "[impact_banner] returned 403 Forbidden: {\"code\":\"SCOPE_MISSING\",\"message\":\"Forbidden: missing required scope\"}";

        assert_eq!(
            impact_panel_error("banner", err),
            "banner needs a refreshed cloud session: difflore cloud login"
        );
    }

    #[test]
    fn impact_logged_out_json_has_next_action() {
        let value = impact_logged_out_value();

        assert_eq!(value["loggedIn"], false);
        assert_eq!(value["state"], "needs_cloud_login");
        assert_eq!(value["reason"], "cloud_login_required");
        assert_eq!(value["nextCommand"], "difflore cloud login");
        assert_eq!(value["readinessCommand"], "difflore cloud team --json");
        assert!(
            value["unavailablePanels"]
                .as_array()
                .expect("unavailable panels are an array")
                .iter()
                .any(|panel| panel == "acceptedFixCounts")
        );
    }

    #[test]
    fn impact_unverified_session_json_does_not_claim_logged_in() {
        let value = impact_unverified_session_value();

        assert_eq!(value["loggedIn"], false);
        assert_eq!(value["state"], "cloud_session_unverified");
        assert_eq!(value["reason"], "cloud_session_unverified");
        assert_eq!(value["nextCommand"], "difflore cloud login");
        assert!(
            value["valueDescription"]
                .as_str()
                .expect("value description")
                .contains("could not be verified")
        );
    }

    #[test]
    fn impact_plan_line_keeps_tier_label_and_single_team_suffix() {
        use difflore_core::cloud::sync::CloudTier;

        // Team user with a team name: the tier label is kept and the team name
        // appears exactly once (in the suffix, never folded into the label).
        let (label, suffix) = impact_plan_line(CloudTier::Team, Some("Acme"));
        assert_eq!(label, "Cloud Team");
        assert_eq!(suffix, " | team `Acme`");
        assert!(
            !label.contains("Acme"),
            "team name must not leak into the plan label (regression guard)"
        );

        // Team Plus keeps its own label.
        assert_eq!(impact_plan_line(CloudTier::TeamPlus, Some("Acme")).0, "Cloud Team Plus");

        // No team name -> no suffix; blank/whitespace name -> no empty suffix.
        assert_eq!(impact_plan_line(CloudTier::Team, None).1, "");
        assert_eq!(impact_plan_line(CloudTier::Team, Some("  ")).1, "");
    }
}
