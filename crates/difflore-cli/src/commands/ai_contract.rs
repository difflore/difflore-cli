use serde::Serialize;

pub(crate) const CLI_SCHEMA_VERSION: &str = "2026-06-16.cli.v1";
pub(crate) const CAPABILITIES_SCHEMA_VERSION: &str = "2026-06-16.capabilities.v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CommandContract {
    pub(crate) safety_tier: u8,
    pub(crate) side_effects: Vec<&'static str>,
    pub(crate) requires_user_intent: bool,
    pub(crate) dry_run_command: Option<String>,
    pub(crate) json_command: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NextActionContract {
    pub(crate) command: String,
    pub(crate) reason: String,
    pub(crate) blocked_by: Option<String>,
    #[serde(flatten)]
    pub(crate) contract: CommandContract,
}

impl NextActionContract {
    pub(crate) fn new(command: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::with_blocked_by(command, reason, None)
    }

    pub(crate) fn with_blocked_by(
        command: impl Into<String>,
        reason: impl Into<String>,
        blocked_by: Option<String>,
    ) -> Self {
        let command = command.into();
        Self {
            contract: command_contract(&command),
            command,
            reason: reason.into(),
            blocked_by,
        }
    }
}

pub(crate) fn command_contract(command: &str) -> CommandContract {
    let command = command.trim();
    let tokens = CommandTokens::new(command);
    let (safety_tier, side_effects, requires_user_intent) = safety_profile(&tokens);
    CommandContract {
        safety_tier,
        side_effects,
        requires_user_intent,
        dry_run_command: dry_run_command(&tokens),
        json_command: json_command(&tokens),
    }
}

struct CommandTokens<'a> {
    raw: &'a str,
    words: Vec<&'a str>,
}

impl<'a> CommandTokens<'a> {
    fn new(command: &'a str) -> Self {
        Self {
            raw: command,
            words: command.split_whitespace().collect(),
        }
    }

    fn starts_with(&self, prefix: &[&str]) -> bool {
        self.words.starts_with(prefix)
    }

    fn is_exact(&self, words: &[&str]) -> bool {
        self.words.as_slice() == words
    }

    fn has_flag(&self, flag: &str) -> bool {
        self.words.contains(&flag)
    }
}

fn safety_profile(command: &CommandTokens<'_>) -> (u8, Vec<&'static str>, bool) {
    if command.starts_with(&["difflore", "mcp-server"])
        || command.starts_with(&["difflore", "__hook-daemon"])
        || command.starts_with(&["difflore", "__outbox-daemon"])
        || command.has_flag("--background")
        || command.starts_with(&["difflore", "skills"])
        || command.starts_with(&["difflore", "dist"])
    {
        return (4, vec!["internal_background_or_maintainer"], true);
    }

    if command.starts_with(&["difflore", "status"])
        || command.starts_with(&["difflore", "recall"])
        || command.starts_with(&["difflore", "ask"])
        || command.is_exact(&["difflore", "memory"])
        || (command.starts_with(&["difflore", "memory"]) && command.has_flag("--json"))
        || command.starts_with(&["difflore", "memory", "inbox"])
        || command.starts_with(&["difflore", "memory", "active"])
        || command.starts_with(&["difflore", "memory", "activity"])
        || command.starts_with(&["difflore", "memory", "show"])
        || command.starts_with(&["difflore", "memory", "digest"])
        || command.starts_with(&["difflore", "memory", "log"])
        || command.starts_with(&["difflore", "agents", "status"])
        || command.starts_with(&["difflore", "providers", "list"])
        || command.starts_with(&["difflore", "embeddings", "status"])
        || command.starts_with(&["difflore", "capabilities"])
    {
        return (0, vec!["read_local_state"], false);
    }

    if command.starts_with(&["difflore", "review"]) {
        return (
            1,
            vec!["read_working_tree", "run_configured_ai_provider"],
            true,
        );
    }

    if command.has_flag("--dry-run") {
        return (
            1,
            if command.starts_with(&["difflore", "cloud"]) {
                vec!["preview_external_sync"]
            } else {
                vec!["preview_local_write"]
            },
            true,
        );
    }

    if command.starts_with(&["difflore", "cloud", "team"])
        || command.starts_with(&["difflore", "cloud", "impact"])
        || command.starts_with(&["difflore", "cloud", "status"])
    {
        return (1, vec!["read_cloud_state"], true);
    }

    if command.starts_with(&["difflore", "cloud", "login"])
        || command.starts_with(&["difflore", "cloud", "sync"])
        || command.starts_with(&["difflore", "cloud", "publish"])
        || command.starts_with(&["difflore", "cloud", "unpublish"])
    {
        return (3, vec!["network", "cloud_account_or_team_state"], true);
    }

    if command.starts_with(&["difflore", "fix"]) {
        return (
            2,
            vec![
                "read_working_tree",
                "write_working_tree",
                "run_configured_ai_provider",
            ],
            true,
        );
    }

    if command.starts_with(&["difflore", "import-reviews"])
        || command.starts_with(&["difflore", "init"])
        || command.starts_with(&["difflore", "memory", "review"])
        || command.starts_with(&["difflore", "memory", "approve"])
        || command.starts_with(&["difflore", "memory", "reject"])
        || command.starts_with(&["difflore", "memory", "disable"])
        || command.starts_with(&["difflore", "memory", "remember"])
        || command.starts_with(&["difflore", "memory", "autopilot"])
        || command.starts_with(&["difflore", "export"])
        || command.starts_with(&["difflore", "agents", "install"])
        || command.starts_with(&["difflore", "agents", "uninstall"])
        || command.starts_with(&["difflore", "agents", "update"])
        || command.starts_with(&["difflore", "update"])
        || command.starts_with(&["difflore", "providers", "add"])
        || command.starts_with(&["difflore", "providers", "set-active"])
        || command.starts_with(&["difflore", "providers", "remove"])
        || command.starts_with(&["difflore", "embeddings", "setup"])
    {
        return (2, vec!["write_local_state"], true);
    }

    (2, vec!["unknown_or_mixed"], true)
}

fn dry_run_command(command: &CommandTokens<'_>) -> Option<String> {
    if command.starts_with(&["difflore", "import-reviews"]) {
        return Some(append_flags(command.raw, &["--dry-run", "--json"]));
    }
    if command.starts_with(&["difflore", "fix"]) {
        return Some("difflore review --diff all --json".to_owned());
    }
    if command.starts_with(&["difflore", "cloud", "sync"]) {
        return Some(append_flags(command.raw, &["--dry-run", "--json"]));
    }
    if command.starts_with(&["difflore", "memory", "autopilot"]) {
        return Some("difflore memory autopilot --dry-run --json".to_owned());
    }
    if command.starts_with(&["difflore", "export"]) {
        return Some(append_flags(command.raw, &["--dry-run", "--json"]));
    }
    if command.starts_with(&["difflore", "agents", "install"])
        || command.starts_with(&["difflore", "agents", "uninstall"])
        || command.starts_with(&["difflore", "agents", "update"])
        || command.starts_with(&["difflore", "update"])
    {
        return Some(append_flags(command.raw, &["--dry-run"]));
    }
    None
}

fn json_command(command: &CommandTokens<'_>) -> Option<String> {
    if command.has_flag("--json") {
        return Some(command.raw.to_owned());
    }
    if command.starts_with(&["difflore", "status"])
        || command.starts_with(&["difflore", "recall"])
        || command.starts_with(&["difflore", "ask"])
        || command.is_exact(&["difflore", "memory"])
        || command.starts_with(&["difflore", "memory", "inbox"])
        || command.starts_with(&["difflore", "memory", "active"])
        || command.starts_with(&["difflore", "memory", "activity"])
        || command.starts_with(&["difflore", "memory", "show"])
        || command.starts_with(&["difflore", "memory", "digest"])
        || command.starts_with(&["difflore", "memory", "log"])
        || command.starts_with(&["difflore", "memory", "approve"])
        || command.starts_with(&["difflore", "memory", "reject"])
        || command.starts_with(&["difflore", "memory", "disable"])
        || command.starts_with(&["difflore", "memory", "remember"])
        || command.starts_with(&["difflore", "memory", "autopilot"])
        || command.starts_with(&["difflore", "import-reviews"])
        || command.starts_with(&["difflore", "review"])
        || (command.starts_with(&["difflore", "fix"]) && command.has_flag("--yes"))
        || command.starts_with(&["difflore", "export"])
        || command.starts_with(&["difflore", "cloud", "status"])
        || command.starts_with(&["difflore", "cloud", "team"])
        || command.starts_with(&["difflore", "cloud", "sync"])
        || command.starts_with(&["difflore", "cloud", "publish"])
        || command.starts_with(&["difflore", "cloud", "unpublish"])
        || command.starts_with(&["difflore", "cloud", "impact"])
        || command.starts_with(&["difflore", "agents", "status"])
        || command.starts_with(&["difflore", "providers", "list"])
        || command.starts_with(&["difflore", "embeddings", "status"])
        || command.starts_with(&["difflore", "capabilities"])
    {
        return Some(append_flags(command.raw, &["--json"]));
    }
    None
}

fn append_flags(command: &str, flags: &[&str]) -> String {
    let mut out = command.trim().to_owned();
    for flag in flags {
        if !out.split_whitespace().any(|part| part == *flag) {
            out.push(' ');
            out.push_str(flag);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_commands_are_tier_zero() {
        let contract = command_contract("difflore recall --diff");
        assert_eq!(contract.safety_tier, 0);
        assert!(!contract.requires_user_intent);
        assert_eq!(
            contract.json_command.as_deref(),
            Some("difflore recall --diff --json")
        );
    }

    #[test]
    fn control_commands_have_user_intent_and_dry_run_when_available() {
        let contract = command_contract("difflore import-reviews");
        assert_eq!(contract.safety_tier, 2);
        assert!(contract.requires_user_intent);
        assert_eq!(
            contract.dry_run_command.as_deref(),
            Some("difflore import-reviews --dry-run --json")
        );
        let dry_run = command_contract("difflore import-reviews --dry-run --json");
        assert_eq!(dry_run.safety_tier, 1);
    }

    #[test]
    fn fix_apply_is_local_write_tier() {
        let contract = command_contract("difflore fix");
        assert_eq!(contract.safety_tier, 2);
        assert_eq!(
            contract.dry_run_command.as_deref(),
            Some("difflore review --diff all --json")
        );
        assert_eq!(contract.json_command, None);

        let auto_apply = command_contract("difflore fix --yes");
        assert_eq!(
            auto_apply.json_command.as_deref(),
            Some("difflore fix --yes --json")
        );
    }

    #[test]
    fn review_is_non_mutating_provider_tier() {
        let contract = command_contract("difflore review --diff all");
        assert_eq!(contract.safety_tier, 1);
        assert_eq!(
            contract.side_effects,
            vec!["read_working_tree", "run_configured_ai_provider"]
        );
        assert_eq!(
            contract.json_command.as_deref(),
            Some("difflore review --diff all --json")
        );
    }

    #[test]
    fn command_matching_is_token_based() {
        let status_like = command_contract("difflore statusx");
        assert_eq!(status_like.safety_tier, 2);
        assert!(status_like.requires_user_intent);

        let quoted_background = command_contract("difflore recall '--background'");
        assert_eq!(quoted_background.safety_tier, 0);

        let quoted_json = command_contract("difflore recall '--json'");
        assert_eq!(
            quoted_json.json_command.as_deref(),
            Some("difflore recall '--json' --json")
        );
    }
}
