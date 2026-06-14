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
