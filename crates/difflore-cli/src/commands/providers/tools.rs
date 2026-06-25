//! Per-tool provider lookup tables, shared between the scripted
//! `providers add` path ([`super`]) and the interactive `providers setup`
//! picker ([`super::setup`]). Centralized so the model/name/binary/label
//! tables for each [`CliTool`] stay in lock-step instead of drifting across
//! two hand-maintained `match` arms.

use std::collections::HashMap;

use gate4agent::CliTool;

/// Default model per tool. Empty string means "let the CLI pick its own
/// default", used for tools whose CLI default already tracks the latest
/// model (Codex, `OpenCode`).
pub(super) const fn default_model_for(tool: CliTool) -> &'static str {
    match tool {
        CliTool::ClaudeCode => "claude-sonnet-4-6",
        CliTool::Gemini => "gemini-2.5-pro",
        CliTool::Codex | CliTool::OpenCode => "",
    }
}

/// Stored provider name (the `*-cli` slug persisted in the local DB).
pub(super) const fn provider_name_for(tool: CliTool) -> &'static str {
    match tool {
        CliTool::ClaudeCode => "claude-cli",
        CliTool::Codex => "codex-cli",
        CliTool::Gemini => "gemini-cli",
        CliTool::OpenCode => "opencode-cli",
    }
}

/// Executable name expected on `PATH` for the tool.
pub(super) const fn binary_for(tool: CliTool) -> &'static str {
    match tool {
        CliTool::ClaudeCode => "claude",
        CliTool::Codex => "codex",
        CliTool::Gemini => "gemini",
        CliTool::OpenCode => "opencode",
    }
}

/// Human-facing label for the interactive picker.
pub(super) const fn provider_display_label(tool: CliTool) -> &'static str {
    match tool {
        CliTool::ClaudeCode => "Claude Code",
        CliTool::Codex => "Codex CLI",
        CliTool::Gemini => "Gemini CLI",
        CliTool::OpenCode => "OpenCode CLI",
    }
}

/// Build the `review`/`default` model mapping persisted for a provider. Both
/// roles point at the same `model` string (the CLI resolves a single model
/// per provider today).
pub(super) fn build_model_mapping(model: &str) -> HashMap<String, String> {
    let mut mapping = HashMap::new();
    mapping.insert("review".to_owned(), model.to_owned());
    mapping.insert("default".to_owned(), model.to_owned());
    mapping
}
