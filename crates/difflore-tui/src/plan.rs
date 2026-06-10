//! Plan state used by the TUI status bar and modal rendering.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    #[default]
    Free,
    Team,
    TeamPlus,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum SupportSla {
    #[default]
    #[serde(rename = "community")]
    Community,
    #[serde(rename = "48h")]
    H48,
    #[serde(rename = "8h")]
    H8,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Entitlements {
    pub cloud_hosted: bool,
    pub cross_machine_sync: bool,
    pub fix_runs_quota: u32,
    pub fix_runs_used: u32,
    pub publish_to_team: bool,
    pub knowledge_build: bool,
    pub byok_allowed: bool,
    pub support_sla: SupportSla,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventStrip {
    #[default]
    None,
    CrossMachine {
        #[serde(rename = "otherHost")]
        other_host: String,
    },
    TeammateCaught {
        rule: String,
        teammate: String,
        #[serde(rename = "firedAt")]
        fired_at: String,
    },
    FixRunsLow {
        used: u32,
        quota: u32,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Dismissal {
    pub kind: String,
    pub scope: Option<String>,
    pub expires_at: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanState {
    pub tier: Tier,
    pub plan_label: String,
    pub plan_accent: String,
    pub entitlements: Entitlements,
    pub rule_count: u32,
    pub published_count: u32,
    pub event_strip: EventStrip,
    pub dismissals: Vec<Dismissal>,
}

impl Default for PlanState {
    fn default() -> Self {
        Self {
            tier: Tier::Free,
            plan_label: "Free".into(),
            plan_accent: "#7d8588".into(),
            entitlements: Entitlements::default(),
            rule_count: 0,
            published_count: 0,
            event_strip: EventStrip::None,
            dismissals: Vec::new(),
        }
    }
}
