//! Plan-state assembly: local rule counts + cloud status → [`PlanState`],
//! plus the status-bar view mapping the bottom strip renders from.

use std::time::Duration;

use difflore_core::cloud::sync::CloudStatus;
use difflore_core::domain::models::SkillRecord;
use difflore_core::observability::activity_stream::{ActivityEvent, ActivityPayload};

use crate::plan::{Entitlements, EventStrip, PlanState, SupportSla, Tier};
use crate::tabs::memory::filter::primary_memory_rule_count;
use crate::widgets::{EventStripState, PlanStateView, PlanTier};

const CLOUD_STATUS_TIMEOUT: Duration = Duration::from_secs(2);
pub(super) const FREE_CLOUD_MEMORY_CAP: usize = 200;

pub(super) fn build_status_bar_view(plan: &PlanState) -> PlanStateView {
    let tier = match plan.tier {
        Tier::Free => PlanTier::Free,
        Tier::Team => PlanTier::Team,
        Tier::TeamPlus => PlanTier::TeamPlus,
    };

    let event_strip = match &plan.event_strip {
        EventStrip::None => EventStripState::None,
        EventStrip::CrossMachine { .. } => EventStripState::CrossMachine,
        EventStrip::TeammateCaught {
            teammate, fired_at, ..
        } => EventStripState::TeammateCaught {
            teammate: teammate.clone(),
            when_label: fired_at.clone(),
        },
        EventStrip::FixRunsLow { used, quota } => EventStripState::FixRunsLow {
            used: *used,
            quota: *quota,
        },
    };

    PlanStateView {
        tier,
        plan_label: plan.plan_label.clone(),
        rule_count: plan.rule_count,
        published_count: plan.published_count,
        event_strip,
        fix_runs_used: plan.entitlements.fix_runs_used,
        fix_runs_quota: plan.entitlements.fix_runs_quota,
    }
}

pub(super) fn derive_event_strip_from_plan(plan: &mut PlanState) {
    if !matches!(plan.event_strip, EventStrip::None) {
        return;
    }

    let used = plan.entitlements.fix_runs_used;
    let quota = plan.entitlements.fix_runs_quota;
    if matches!(plan.tier, Tier::Team) && quota > 0 && u64::from(used) * 5 >= u64::from(quota) * 4 {
        plan.event_strip = EventStrip::FixRunsLow { used, quota };
    }
}

pub(super) fn derive_event_strip_from_events(plan: &mut PlanState, events: &[ActivityEvent]) {
    if !matches!(plan.event_strip, EventStrip::None) {
        return;
    }

    if !matches!(plan.tier, Tier::Free) {
        return;
    }

    if let Some((used, quota)) = latest_embed_cap(events) {
        plan.entitlements.fix_runs_used = used;
        plan.entitlements.fix_runs_quota = quota;
        plan.event_strip = EventStrip::FixRunsLow { used, quota };
    }
}

fn latest_embed_cap(events: &[ActivityEvent]) -> Option<(u32, u32)> {
    events.iter().find_map(|event| {
        if let ActivityPayload::EmbedCapReached { cap, used } = &event.payload {
            Some((*used, *cap))
        } else {
            None
        }
    })
}

pub(super) async fn load_plan_state(
    rules: &[SkillRecord],
    wiring: &crate::WiringSnapshot,
) -> PlanState {
    let mut plan = PlanState {
        rule_count: count_to_u32(primary_memory_rule_count(rules)),
        ..Default::default()
    };

    if wiring.cloud_logged_in {
        apply_cloud_login_baseline(&mut plan, rules);
        let client = difflore_core::cloud::client::CloudClient::create().await;
        if let Ok(status) = tokio::time::timeout(
            CLOUD_STATUS_TIMEOUT,
            difflore_core::cloud::sync::fetch_cloud_status(&client),
        )
        .await
        {
            apply_cloud_status_to_plan(&mut plan, &status);
        }
    }

    plan
}

fn apply_cloud_login_baseline(plan: &mut PlanState, rules: &[SkillRecord]) {
    if crate::tabs::memory::filter::cloud_memory_rule_count(rules) > FREE_CLOUD_MEMORY_CAP {
        plan.tier = Tier::Team;
        "Cloud Team".clone_into(&mut plan.plan_label);
        "#5ee0c8".clone_into(&mut plan.plan_accent);
        plan.entitlements = entitlements_for_tier(Tier::Team);
    } else {
        "Cloud Free".clone_into(&mut plan.plan_label);
    }
}

fn apply_cloud_status_to_plan(plan: &mut PlanState, status: &CloudStatus) {
    if !status.logged_in {
        return;
    }

    let tier = tier_from_cloud_status(status);
    plan.tier = tier;
    plan.plan_label = plan_label_from_cloud_status(status, tier);
    match tier {
        Tier::Free => "#7d8588",
        Tier::Team => "#5ee0c8",
        Tier::TeamPlus => "#a78bfa",
    }
    .clone_into(&mut plan.plan_accent);
    plan.entitlements = entitlements_for_tier(tier);
}

fn tier_from_cloud_status(status: &CloudStatus) -> Tier {
    let plan = status
        .plan
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase()
        .replace('-', "_");
    match plan.as_str() {
        "team_plus" | "enterprise" => Tier::TeamPlus,
        "team" | "pro" | "business" => Tier::Team,
        _ if status.team_name.is_some() => Tier::Team,
        _ => Tier::Free,
    }
}

fn plan_label_from_cloud_status(status: &CloudStatus, tier: Tier) -> String {
    if let Some(team) = status.team_name.as_deref().map(str::trim)
        && !team.is_empty()
    {
        return team.to_owned();
    }

    match tier {
        Tier::Free => "Cloud Free".to_owned(),
        Tier::Team => "Cloud Team".to_owned(),
        Tier::TeamPlus => "Cloud Team Plus".to_owned(),
    }
}

fn entitlements_for_tier(tier: Tier) -> Entitlements {
    match tier {
        Tier::Free => Entitlements::default(),
        Tier::Team => Entitlements {
            cloud_hosted: true,
            cross_machine_sync: true,
            publish_to_team: true,
            knowledge_build: true,
            byok_allowed: true,
            support_sla: SupportSla::H48,
            ..Default::default()
        },
        Tier::TeamPlus => Entitlements {
            cloud_hosted: true,
            cross_machine_sync: true,
            publish_to_team: true,
            knowledge_build: true,
            byok_allowed: true,
            support_sla: SupportSla::H8,
            ..Default::default()
        },
    }
}

fn count_to_u32(count: usize) -> u32 {
    u32::try_from(count).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tabs::memory::filter::rule_with_origin;

    #[test]
    fn build_status_bar_view_maps_event_strip_variants() {
        let mut plan = PlanState {
            tier: Tier::Team,
            event_strip: EventStrip::FixRunsLow {
                used: 240,
                quota: 300,
            },
            ..Default::default()
        };
        plan.entitlements.fix_runs_used = 240;
        plan.entitlements.fix_runs_quota = 300;
        let view = build_status_bar_view(&plan);
        assert_eq!(view.tier, PlanTier::Team);
        assert!(matches!(
            view.event_strip,
            EventStripState::FixRunsLow {
                used: 240,
                quota: 300
            }
        ));
        let right = view.right_side();
        assert!(
            right
                .as_deref()
                .is_some_and(|text| text.contains("240/300"))
        );
    }

    #[test]
    fn derive_event_strip_flags_team_capacity_at_eighty_percent() {
        let mut plan = PlanState {
            tier: Tier::Team,
            ..Default::default()
        };
        plan.entitlements.fix_runs_used = 80;
        plan.entitlements.fix_runs_quota = 100;

        derive_event_strip_from_plan(&mut plan);

        assert_eq!(
            plan.event_strip,
            EventStrip::FixRunsLow {
                used: 80,
                quota: 100
            }
        );
    }

    #[test]
    fn derive_event_strip_uses_recent_embed_cap_activity() {
        let mut plan = PlanState::default();
        let events = vec![ActivityEvent {
            ts_ms: 1,
            payload: ActivityPayload::EmbedCapReached {
                used: 198,
                cap: 200,
            },
        }];

        derive_event_strip_from_events(&mut plan, &events);

        assert_eq!(
            plan.event_strip,
            EventStrip::FixRunsLow {
                used: 198,
                quota: 200
            }
        );
        assert_eq!(plan.entitlements.fix_runs_used, 198);
        assert_eq!(plan.entitlements.fix_runs_quota, 200);
    }

    #[test]
    fn paid_plan_ignores_stale_free_embed_cap_events() {
        let mut plan = PlanState {
            tier: Tier::Team,
            ..Default::default()
        };
        let events = vec![ActivityEvent {
            ts_ms: 1,
            payload: ActivityPayload::EmbedCapReached {
                used: 198,
                cap: 200,
            },
        }];

        derive_event_strip_from_events(&mut plan, &events);

        assert_eq!(plan.event_strip, EventStrip::None);
    }

    #[test]
    fn count_to_u32_saturates() {
        assert_eq!(count_to_u32(42), 42);
        assert_eq!(count_to_u32(usize::MAX), u32::MAX);
    }

    #[test]
    fn team_cloud_status_drives_plan_badge() {
        let mut plan = PlanState::default();
        let status = CloudStatus {
            logged_in: true,
            email: Some("hello@difflore.dev".to_owned()),
            plan: Some("team".to_owned()),
            team_name: Some("invite-smoke-60377e".to_owned()),
            team_id: None,
        };

        apply_cloud_status_to_plan(&mut plan, &status);

        assert_eq!(plan.tier, Tier::Team);
        assert_eq!(plan.plan_label, "invite-smoke-60377e");
        assert!(plan.entitlements.cross_machine_sync);
        assert!(plan.entitlements.publish_to_team);
    }

    #[test]
    fn logged_in_large_cloud_cache_starts_as_team_before_remote_status_returns() {
        let mut plan = PlanState::default();
        let rules: Vec<SkillRecord> = (0..=FREE_CLOUD_MEMORY_CAP)
            .map(|i| {
                let mut rule = rule_with_origin("cloud");
                rule.id = format!("cloud-{i}");
                rule
            })
            .collect();

        apply_cloud_login_baseline(&mut plan, &rules);

        assert_eq!(plan.tier, Tier::Team);
        assert_eq!(plan.plan_label, "Cloud Team");
        assert!(plan.entitlements.cross_machine_sync);
    }
}
