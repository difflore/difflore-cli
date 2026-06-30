use serde::Serialize;

use difflore_core::memory_autopilot_schedule::MemoryAutopilotScheduleStatus;
use difflore_core::memory_inbox::{
    MemoryInbox, MemoryInboxWarning, MemoryQueueSection, MemoryRuleItem, MemoryUsage,
    SessionMinedDiscovery,
};

use crate::commands::ai_contract::{CLI_SCHEMA_VERSION, NextActionContract};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct MemoryCloudSummary {
    pub logged_in: bool,
    pub team_ready: Option<bool>,
    pub blocker: Option<&'static str>,
    pub note: Option<&'static str>,
}

impl MemoryCloudSummary {
    pub(super) async fn load() -> Self {
        let logged_in = difflore_core::cloud::client::CloudClient::load_token_quiet()
            .await
            .is_some();
        if logged_in {
            Self {
                logged_in: true,
                team_ready: None,
                blocker: None,
                note: Some("approved local memory can be shared with your team"),
            }
        } else {
            Self {
                logged_in: false,
                team_ready: Some(false),
                blocker: Some("needs_cloud_login"),
                note: Some("team sync starts with difflore cloud login"),
            }
        }
    }
}

pub(super) type MemoryNextAction = NextActionContract;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct MemoryLocalDiscoveriesOutput {
    pub session_mined_candidates: i64,
    pub latest: Vec<SessionMinedDiscovery>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct MemoryInboxOutput {
    pub schema_version: &'static str,
    pub active_rules: i64,
    pub active_rule_items: Vec<MemoryRuleItem>,
    pub local_drafts: i64,
    pub local_draft_items: Vec<MemoryRuleItem>,
    pub local_discoveries: MemoryLocalDiscoveriesOutput,
    pub autopilot: MemoryAutopilotScheduleStatus,
    pub queues: MemoryQueueSection,
    pub cloud: MemoryCloudSummary,
    pub next: MemoryNextAction,
    pub usage: MemoryUsage,
    pub warnings: Vec<MemoryInboxWarning>,
}

impl MemoryInboxOutput {
    pub(super) fn from_parts(
        inbox: &MemoryInbox,
        autopilot: MemoryAutopilotScheduleStatus,
        cloud: MemoryCloudSummary,
        next: MemoryNextAction,
    ) -> Self {
        Self {
            schema_version: CLI_SCHEMA_VERSION,
            active_rules: inbox.active_rule_count(),
            active_rule_items: inbox.active_rules.latest.clone(),
            local_drafts: inbox.local_draft_count(),
            local_draft_items: inbox.local_drafts.latest.clone(),
            local_discoveries: MemoryLocalDiscoveriesOutput {
                session_mined_candidates: inbox.session_mined_count(),
                latest: inbox.local_discoveries.latest.clone(),
            },
            autopilot,
            queues: inbox.queues.clone(),
            cloud,
            next,
            usage: inbox.usage.clone(),
            warnings: inbox.warnings.clone(),
        }
    }
}
