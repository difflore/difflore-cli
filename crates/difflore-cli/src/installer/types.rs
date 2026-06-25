use serde::Serialize;

#[derive(Debug, Clone)]
pub enum Status {
    Installed,
    Updated,
    /// A `difflore` entry was removed by `agents uninstall`.
    Removed,
    Skipped(String),
    Error(String),
}

#[derive(Debug, Clone)]
pub struct TargetOutcome {
    pub name: &'static str,
    pub status: Status,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallState {
    Installed,
    NotInstalled,
    Conflict,
    Unknown,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetStatus {
    pub name: &'static str,
    pub detected: bool,
    pub state: InstallState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpClientStatus {
    pub name: &'static str,
    pub detected: bool,
    pub state: InstallState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub surfaces: Vec<TargetStatus>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CanonicalRecordState {
    Missing,
    Present,
    Stale,
    Conflict,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CanonicalRecordStatus {
    pub path: Option<String>,
    pub state: CanonicalRecordState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recorded_targets: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actual_targets: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeProbeState {
    Ok,
    Failed,
    Timeout,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpRuntimeProbe {
    pub state: RuntimeProbeState,
    pub detail: String,
    pub initialized: bool,
    pub tools_listed: bool,
    pub tool_call_completed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_rules_injected: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_rules_indexed: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_top_result: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_names: Vec<String>,
}

impl McpRuntimeProbe {
    /// Probe result for a self-check that never produced a usable handshake,
    /// carrying only the failure `detail` with every diagnostic field cleared.
    /// Mirrors `GateResult::errored_with` so the many early-return failure sites
    /// in `common.rs` collapse to one-liners.
    pub(super) fn failed(detail: impl Into<String>) -> Self {
        Self::aborted(RuntimeProbeState::Failed, detail)
    }

    /// Probe result for a self-check that exceeded its deadline. Same shape as
    /// [`Self::failed`] but with [`RuntimeProbeState::Timeout`].
    pub(super) fn timed_out(detail: impl Into<String>) -> Self {
        Self::aborted(RuntimeProbeState::Timeout, detail)
    }

    fn aborted(state: RuntimeProbeState, detail: impl Into<String>) -> Self {
        Self {
            state,
            detail: detail.into(),
            initialized: false,
            tools_listed: false,
            tool_call_completed: false,
            tool_call_name: None,
            tool_call_rules_injected: None,
            tool_call_rules_indexed: None,
            tool_call_top_result: None,
            tool_count: None,
            tool_names: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpStatusDiagnosis {
    pub summary: String,
    pub next_step: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub affected_clients: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpStatusSnapshot {
    pub binary: String,
    pub canonical_record: CanonicalRecordStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_probe: Option<McpRuntimeProbe>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnosis: Option<McpStatusDiagnosis>,
    #[serde(default)]
    pub clients: Vec<McpClientStatus>,
    pub agents: Vec<TargetStatus>,
}
