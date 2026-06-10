//! Shared JSON shape for the hidden local impact command and `difflore cloud
//! impact` (team). Both call sites build the same 5 sections — banner,
//! weekly, topRules, coverage, fixScorecard — from the same cloud DTOs.
//! This module owns that shape; per-handler quirks (e.g. `loggedIn` /
//! `team` vs `hasData` / `windowDays`) stay in the callers.

use difflore_core::cloud::api_types::{
    ImpactBannerDto, ImpactCoverageDto, ImpactFixScorecardDto, ImpactTopRulesDto, ImpactWeeklyDto,
};
use serde_json::{Map, Value, json};
use std::collections::HashMap;

const REVIEW_MINUTES_PER_ACCEPTED_FIX: i64 = 4;

pub(crate) struct ImpactPayloadInputs<'a, E> {
    pub banner: &'a Result<ImpactBannerDto, E>,
    pub weekly: &'a Result<ImpactWeeklyDto, E>,
    pub top_rules: &'a Result<ImpactTopRulesDto, E>,
    pub coverage: &'a Result<ImpactCoverageDto, E>,
    pub fix_scorecard: &'a Result<ImpactFixScorecardDto, E>,
}

pub(crate) fn shared_sections_with_accepted_proof_sources<E>(
    input: &ImpactPayloadInputs<'_, E>,
    accepted_proof_sources: &HashMap<String, String>,
) -> Map<String, Value> {
    let mut out = Map::with_capacity(5);
    out.insert("banner".to_owned(), banner_value(input.banner));
    out.insert("weekly".to_owned(), weekly_value(input.weekly));
    out.insert(
        "topRules".to_owned(),
        top_rules_value(input.top_rules, accepted_proof_sources),
    );
    out.insert("coverage".to_owned(), coverage_value(input.coverage));
    out.insert(
        "fixScorecard".to_owned(),
        fix_scorecard_value(input.fix_scorecard),
    );
    out
}

fn banner_value<E>(r: &Result<ImpactBannerDto, E>) -> Value {
    r.as_ref().ok().map_or(Value::Null, |b| {
        json!({
            "pastVerdictsThisWeek": b.past_verdicts_this_week,
            "weekStartIso": b.week_start_iso,
        })
    })
}

fn weekly_value<E>(r: &Result<ImpactWeeklyDto, E>) -> Value {
    r.as_ref().ok().map_or(Value::Null, |w| {
        json!({
            "weeks": w.weeks.iter().map(|p| json!({
                "weekStartIso": p.week_start_iso,
                "rulesSedimented": p.rules_sedimented,
                "pastVerdictsRecalled": p.past_verdicts_recalled,
                "fixesAccepted": p.fixes_accepted,
            })).collect::<Vec<_>>(),
        })
    })
}

fn top_rules_value<E>(
    r: &Result<ImpactTopRulesDto, E>,
    accepted_proof_sources: &HashMap<String, String>,
) -> Value {
    r.as_ref().ok().map_or(Value::Null, |r| {
        json!({
            "rules": r.rules.iter().map(|x| {
                let accepted_proof_source = x.accepted_proof_source
                    .as_deref()
                    .or_else(|| accepted_proof_sources.get(&x.id).map(String::as_str));
                json!({
                    "id": x.id,
                    "name": x.name,
                    "severity": x.severity,
                    "language": x.language,
                    "acceptanceCount": x.acceptance_count,
                    "distinctUsers": x.distinct_users,
                    "citedCount": x.cited_count,
                    "trustRate": x.trust_rate,
                    "reviewerProofReadyCount": x.reviewer_proof_ready_count,
                    "agentReadyProofLabel": agent_ready_proof_label(x.reviewer_proof_ready_count),
                    "reviewerContextServes": x.reviewer_context_serves,
                    "reviewerMentions": x.reviewer_mentions,
                    "reviewerContextProofLabel": reviewer_context_proof_label(
                        x.reviewer_context_serves,
                        x.reviewer_mentions,
                    ),
                    "sourceRepo": x.source_repo,
                    "acceptedProofSource": accepted_proof_source,
                    "acceptedProofLabel": accepted_proof_source_label(accepted_proof_source),
                })
            }).collect::<Vec<_>>(),
            "promotionProgress": r.promotion_progress.iter().map(|p| json!({
                "filePath": p.file_path,
                "language": p.language,
                "acceptanceCount": p.acceptance_count,
                "requiredCount": p.required_count,
                "distinctUsers": p.distinct_users,
                "requiredDistinctUsers": p.required_distinct_users,
            })).collect::<Vec<_>>(),
        })
    })
}

pub(crate) fn accepted_proof_source_label(source: Option<&str>) -> Option<&'static str> {
    match source {
        Some("local_fix") => Some("Local Fix activity"),
        Some("cloud_fix") => Some("Imported activity"),
        Some("historical_backfill") => Some("Historical activity"),
        Some("mixed") => Some("Mixed activity"),
        _ => None,
    }
}

pub(crate) fn agent_ready_proof_label(count: i64) -> Option<String> {
    if count <= 0 {
        return None;
    }
    let noun = if count == 1 { "outcome" } else { "outcomes" };
    Some(format!("{count} accepted {noun} ready for agent recall"))
}

pub(crate) fn reviewer_context_proof_label(serves: i64, mentions: i64) -> Option<String> {
    let serves = serves.max(0);
    let mentions = mentions.max(0);
    match (serves, mentions) {
        (0, 0) => None,
        (0, mentions) => Some(format_count(
            mentions,
            "reviewer mention",
            "reviewer mentions",
        )),
        (serves, 0) => Some(format!(
            "{}; waiting for first reviewer mention",
            format_count(
                serves,
                "reviewer context recall",
                "reviewer context recalls",
            )
        )),
        (serves, mentions) => Some(format!(
            "{} after {}",
            format_count(mentions, "reviewer mention", "reviewer mentions"),
            format_count(
                serves,
                "reviewer context recall",
                "reviewer context recalls"
            )
        )),
    }
}

fn format_count(count: i64, singular: &str, plural: &str) -> String {
    format!("{count} {}", if count == 1 { singular } else { plural })
}

pub(crate) async fn fetch_accepted_proof_sources(
    db: &difflore_core::SqlitePool,
    rule_ids: &[String],
) -> HashMap<String, String> {
    let mut out = fetch_fix_outcome_proof_sources(db, rule_ids).await;
    if let Ok(observation_sources) =
        difflore_core::cloud::observations::accepted_fix_proof_sources_default(rule_ids).await
    {
        for (rule_id, source) in observation_sources {
            out.entry(rule_id).or_insert(source);
        }
    }
    out
}

pub(crate) async fn fetch_accepted_proof_sources_for_top_rules<E>(
    ctx: &crate::runtime::CommandContext,
    top_rules: &Result<ImpactTopRulesDto, E>,
    limit: usize,
) -> HashMap<String, String> {
    let Ok(top_rules) = top_rules else {
        return HashMap::new();
    };
    let ids: Vec<String> = top_rules
        .rules
        .iter()
        .take(limit)
        .filter(|rule| rule.accepted_proof_source.is_none())
        .map(|rule| rule.id.clone())
        .collect();
    if ids.is_empty() {
        return HashMap::new();
    }
    fetch_accepted_proof_sources(&ctx.db, &ids).await
}

async fn fetch_fix_outcome_proof_sources(
    db: &difflore_core::SqlitePool,
    rule_ids: &[String],
) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if rule_ids.is_empty() {
        return out;
    }

    let placeholders = std::iter::repeat_n("?", rule_ids.len())
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT rule_id, COUNT(*) as accepted_count \
         FROM fix_outcomes \
         WHERE rule_id IN ({placeholders}) \
           AND accepted = 1 \
           AND applied_ok = 1 \
         GROUP BY rule_id"
    );
    let mut q = sqlx::query_as::<_, (String, i64)>(&sql);
    for id in rule_ids {
        q = q.bind(id);
    }

    if let Ok(rows) = q.fetch_all(db).await {
        for (rule_id, accepted_count) in rows {
            if accepted_count > 0 {
                out.insert(rule_id, "local_fix".to_owned());
            }
        }
    }
    out
}

fn coverage_value<E>(r: &Result<ImpactCoverageDto, E>) -> Value {
    r.as_ref().ok().map_or(Value::Null, |c| {
        json!({
            "repos": c.repos,
            "prs": c.prs,
            "files": c.files,
            "reviewCommentsIndexed": c.review_comments_indexed,
            "aiReviewerCommentsIndexed": c.ai_reviewer_comments_indexed,
            "humanReviewCommentsIndexed": c.human_review_comments_indexed,
        })
    })
}

fn fix_scorecard_value<E>(r: &Result<ImpactFixScorecardDto, E>) -> Value {
    r.as_ref().ok().map_or(Value::Null, |f| {
        let saved_review_minutes = saved_review_minutes_for_scorecard(f);
        json!({
            "last30": { "accepted": f.last30.accepted, "total": f.last30.total },
            "prior30": { "accepted": f.prior30.accepted, "total": f.prior30.total },
            "trendPct": f.trend_pct,
            "roi": f.roi.as_ref().map_or_else(
                || json!({
                    "acceptedFixesLast30": f.last30.accepted,
                    "reviewCommentsAvoided": f.last30.accepted,
                    "savedReviewMinutes": saved_review_minutes,
                    "repeatFeedbackReduced": 0,
                    "sourceEvidenceItems": 0,
                }),
                |roi| json!({
                    "acceptedFixesLast30": roi.accepted_fixes_last30,
                    "reviewCommentsAvoided": roi.review_comments_avoided,
                    "savedReviewMinutes": saved_review_minutes,
                    "repeatFeedbackReduced": roi.repeat_feedback_reduced,
                    "sourceEvidenceItems": roi.source_evidence_items,
                }),
            ),
        })
    })
}

pub(crate) fn saved_review_minutes_for_scorecard(scorecard: &ImpactFixScorecardDto) -> i64 {
    let server_minutes = scorecard
        .roi
        .as_ref()
        .map_or(0, |roi| roi.saved_review_minutes);
    if server_minutes > 0 {
        return server_minutes;
    }

    scorecard
        .last30
        .accepted
        .max(0)
        .saturating_mul(REVIEW_MINUTES_PER_ACCEPTED_FIX)
}

pub(crate) fn saved_review_time_label(minutes: i64) -> Option<String> {
    if minutes <= 0 {
        return None;
    }

    let hours = minutes / 60;
    let remainder = minutes % 60;
    let label = if hours == 0 {
        format!("{minutes} min review time saved")
    } else if remainder == 0 {
        format!("{hours}h review time saved")
    } else {
        format!("{hours}h {remainder}m review time saved")
    };
    Some(label)
}

#[cfg(test)]
mod tests {
    use super::*;
    use difflore_core::cloud::api_types::{
        ImpactBannerDto, ImpactCoverageDto, ImpactFixScorecardDto, ImpactFixWindowDto,
        ImpactPromotionProgressDto, ImpactRoiDto, ImpactTopRuleDto, ImpactTopRulesDto,
        ImpactWeeklyDto,
    };
    use sqlx::sqlite::SqlitePoolOptions;

    #[test]
    fn shared_top_rules_json_includes_accepted_proof_source() {
        let top_rules: Result<ImpactTopRulesDto, ()> = Ok(ImpactTopRulesDto {
            rules: vec![ImpactTopRuleDto {
                id: "rule-1".to_owned(),
                name: "Prefer structured parsing".to_owned(),
                severity: None,
                language: Some("rust".to_owned()),
                acceptance_count: 2,
                distinct_users: 1,
                cited_count: 4,
                trust_rate: Some(0.5),
                accepted_proof_source: Some("local_fix".to_owned()),
                reviewer_proof_ready_count: 2,
                reviewer_context_serves: 5,
                reviewer_mentions: 2,
                source_repo: Some("gin-gonic/gin".to_owned()),
            }],
            promotion_progress: Vec::<ImpactPromotionProgressDto>::new(),
        });
        let banner: Result<ImpactBannerDto, ()> = Err(());
        let weekly: Result<ImpactWeeklyDto, ()> = Err(());
        let coverage: Result<ImpactCoverageDto, ()> = Err(());
        let fix_scorecard: Result<ImpactFixScorecardDto, ()> = Ok(ImpactFixScorecardDto {
            last30: ImpactFixWindowDto {
                accepted: 0,
                total: 0,
            },
            prior30: ImpactFixWindowDto {
                accepted: 0,
                total: 0,
            },
            trend_pct: None,
            roi: Some(ImpactRoiDto {
                accepted_fixes_last30: 2,
                review_comments_avoided: 2,
                saved_review_minutes: 8,
                repeat_feedback_reduced: 1,
                source_evidence_items: 4,
            }),
        });

        let payload = shared_sections_with_accepted_proof_sources(
            &ImpactPayloadInputs {
                banner: &banner,
                weekly: &weekly,
                top_rules: &top_rules,
                coverage: &coverage,
                fix_scorecard: &fix_scorecard,
            },
            &HashMap::new(),
        );

        assert_eq!(
            payload["topRules"]["rules"][0]["acceptedProofSource"],
            "local_fix"
        );
        assert_eq!(
            payload["topRules"]["rules"][0]["reviewerProofReadyCount"],
            2
        );
        assert_eq!(
            payload["topRules"]["rules"][0]["agentReadyProofLabel"],
            "2 accepted outcomes ready for agent recall"
        );
        assert_eq!(payload["topRules"]["rules"][0]["reviewerContextServes"], 5);
        assert_eq!(payload["topRules"]["rules"][0]["reviewerMentions"], 2);
        assert_eq!(
            payload["topRules"]["rules"][0]["reviewerContextProofLabel"],
            "2 reviewer mentions after 5 reviewer context recalls"
        );
        assert_eq!(
            payload["topRules"]["rules"][0]["sourceRepo"],
            "gin-gonic/gin"
        );
        assert_eq!(
            accepted_proof_source_label(Some("local_fix")),
            Some("Local Fix activity")
        );
        assert_eq!(payload["fixScorecard"]["roi"]["savedReviewMinutes"], 8);
        assert_eq!(
            saved_review_time_label(125),
            Some("2h 5m review time saved".to_owned())
        );
    }

    #[test]
    fn shared_top_rules_json_falls_back_to_local_accepted_proof_source() {
        let top_rules: Result<ImpactTopRulesDto, ()> = Ok(ImpactTopRulesDto {
            rules: vec![ImpactTopRuleDto {
                id: "rule-1".to_owned(),
                name: "Prefer structured parsing".to_owned(),
                severity: None,
                language: Some("rust".to_owned()),
                acceptance_count: 2,
                distinct_users: 1,
                cited_count: 4,
                trust_rate: Some(0.5),
                accepted_proof_source: None,
                reviewer_proof_ready_count: 0,
                reviewer_context_serves: 0,
                reviewer_mentions: 0,
                source_repo: None,
            }],
            promotion_progress: Vec::<ImpactPromotionProgressDto>::new(),
        });
        let banner: Result<ImpactBannerDto, ()> = Err(());
        let weekly: Result<ImpactWeeklyDto, ()> = Err(());
        let coverage: Result<ImpactCoverageDto, ()> = Err(());
        let fix_scorecard: Result<ImpactFixScorecardDto, ()> = Err(());
        let mut proof_sources = HashMap::new();
        proof_sources.insert("rule-1".to_owned(), "local_fix".to_owned());

        let payload = shared_sections_with_accepted_proof_sources(
            &ImpactPayloadInputs {
                banner: &banner,
                weekly: &weekly,
                top_rules: &top_rules,
                coverage: &coverage,
                fix_scorecard: &fix_scorecard,
            },
            &proof_sources,
        );

        assert_eq!(
            payload["topRules"]["rules"][0]["acceptedProofSource"],
            "local_fix"
        );
        assert_eq!(
            payload["topRules"]["rules"][0]["acceptedProofLabel"],
            "Local Fix activity"
        );
        assert!(payload["topRules"]["rules"][0]["agentReadyProofLabel"].is_null());
    }

    #[test]
    fn agent_ready_proof_label_only_shows_positive_counts() {
        assert_eq!(
            agent_ready_proof_label(2).as_deref(),
            Some("2 accepted outcomes ready for agent recall")
        );
        assert_eq!(
            agent_ready_proof_label(1).as_deref(),
            Some("1 accepted outcome ready for agent recall")
        );
        assert_eq!(agent_ready_proof_label(0), None);
    }

    #[test]
    fn reviewer_context_proof_label_summarizes_serves_and_mentions() {
        assert_eq!(
            reviewer_context_proof_label(5, 2).as_deref(),
            Some("2 reviewer mentions after 5 reviewer context recalls")
        );
        assert_eq!(
            reviewer_context_proof_label(1, 0).as_deref(),
            Some("1 reviewer context recall; waiting for first reviewer mention")
        );
        assert_eq!(
            reviewer_context_proof_label(0, 1).as_deref(),
            Some("1 reviewer mention")
        );
        assert_eq!(reviewer_context_proof_label(0, 0), None);
    }

    #[test]
    fn shared_coverage_json_includes_indexed_review_comments() {
        let coverage: Result<ImpactCoverageDto, ()> = Ok(ImpactCoverageDto {
            repos: 3,
            prs: 12,
            files: 40,
            review_comments_indexed: 118,
            ai_reviewer_comments_indexed: 41,
            human_review_comments_indexed: 77,
        });
        let banner: Result<ImpactBannerDto, ()> = Err(());
        let weekly: Result<ImpactWeeklyDto, ()> = Err(());
        let top_rules: Result<ImpactTopRulesDto, ()> = Err(());
        let fix_scorecard: Result<ImpactFixScorecardDto, ()> = Err(());

        let payload = shared_sections_with_accepted_proof_sources(
            &ImpactPayloadInputs {
                banner: &banner,
                weekly: &weekly,
                top_rules: &top_rules,
                coverage: &coverage,
                fix_scorecard: &fix_scorecard,
            },
            &HashMap::new(),
        );

        assert_eq!(payload["coverage"]["reviewCommentsIndexed"], 118);
        assert_eq!(payload["coverage"]["aiReviewerCommentsIndexed"], 41);
        assert_eq!(payload["coverage"]["humanReviewCommentsIndexed"], 77);
    }

    #[test]
    fn saved_review_minutes_falls_back_to_accepts_when_cloud_roi_is_zero() {
        let scorecard = ImpactFixScorecardDto {
            last30: ImpactFixWindowDto {
                accepted: 46,
                total: 46,
            },
            prior30: ImpactFixWindowDto {
                accepted: 0,
                total: 0,
            },
            trend_pct: None,
            roi: Some(ImpactRoiDto {
                accepted_fixes_last30: 46,
                review_comments_avoided: 46,
                saved_review_minutes: 0,
                repeat_feedback_reduced: 0,
                source_evidence_items: 2040,
            }),
        };

        assert_eq!(saved_review_minutes_for_scorecard(&scorecard), 184);

        let scorecard_result: Result<ImpactFixScorecardDto, ()> = Ok(scorecard);
        let top_rules: Result<ImpactTopRulesDto, ()> = Ok(ImpactTopRulesDto {
            rules: Vec::new(),
            promotion_progress: Vec::new(),
        });
        let banner: Result<ImpactBannerDto, ()> = Err(());
        let weekly: Result<ImpactWeeklyDto, ()> = Err(());
        let coverage: Result<ImpactCoverageDto, ()> = Err(());
        let payload = shared_sections_with_accepted_proof_sources(
            &ImpactPayloadInputs {
                banner: &banner,
                weekly: &weekly,
                top_rules: &top_rules,
                coverage: &coverage,
                fix_scorecard: &scorecard_result,
            },
            &HashMap::new(),
        );

        assert_eq!(payload["fixScorecard"]["roi"]["savedReviewMinutes"], 184);
    }

    #[tokio::test]
    async fn local_accepted_proof_sources_only_marks_applied_accepts() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE fix_outcomes (
                rule_id TEXT,
                diff_signature TEXT,
                accepted INTEGER NOT NULL,
                applied_ok INTEGER NOT NULL
            )",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query!(
            "INSERT INTO fix_outcomes (rule_id, accepted, applied_ok)
             VALUES
               ('rule-applied', 1, 1),
               ('rule-rejected', 0, 0),
               ('rule-failed', 1, 0)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let ids = vec![
            "rule-applied".to_owned(),
            "rule-rejected".to_owned(),
            "rule-failed".to_owned(),
        ];
        let sources = fetch_fix_outcome_proof_sources(&pool, &ids).await;

        assert_eq!(
            sources.get("rule-applied").map(String::as_str),
            Some("local_fix")
        );
        assert!(!sources.contains_key("rule-rejected"));
        assert!(!sources.contains_key("rule-failed"));
    }
}
