//! Interactive provider picker.
//!
//! Providers are local agent CLIs (Claude Code, Codex, Gemini, OpenCode)
//! detected on PATH via `gate4agent`. We never prompt for API keys: the
//! local CLI reuses the user's existing agent auth rather than storing
//! keys of its own.

use crate::style;
use std::collections::HashMap;
use std::io::{self, BufRead, IsTerminal, Write};

use colored::Colorize;
use gate4agent::CliTool;

use difflore_core::domain::models::{ProviderAddInput, ProviderSetActiveInput};

/// Default model per tool. Empty string means "let the CLI pick its own
/// default", used for tools whose CLI default already tracks the latest
/// model (Codex, `OpenCode`).
const fn default_model_for(tool: CliTool) -> &'static str {
    match tool {
        CliTool::ClaudeCode => "claude-sonnet-4-6",
        CliTool::Gemini => "gemini-2.5-pro",
        CliTool::Codex | CliTool::OpenCode => "",
    }
}

const fn binary_for(tool: CliTool) -> &'static str {
    match tool {
        CliTool::ClaudeCode => "claude",
        CliTool::Codex => "codex",
        CliTool::Gemini => "gemini",
        CliTool::OpenCode => "opencode",
    }
}

const fn provider_name_for(tool: CliTool) -> &'static str {
    match tool {
        CliTool::ClaudeCode => "claude-cli",
        CliTool::Codex => "codex-cli",
        CliTool::Gemini => "gemini-cli",
        CliTool::OpenCode => "opencode-cli",
    }
}

const ALL_AGENT_TOOLS: [CliTool; 4] = [
    CliTool::ClaudeCode,
    CliTool::Codex,
    CliTool::Gemini,
    CliTool::OpenCode,
];

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum SetupOutcome {
    Configured,
    Skipped,
}

pub async fn run_setup(db: &difflore_core::SqlitePool) -> SetupOutcome {
    if !io::stdin().is_terminal() {
        println!(
            "{} providers setup skipped: no interactive TTY.",
            style::warn("warning:")
        );
        println!(
            "  Use {} for scripted configuration.",
            style::cmd("difflore providers add --tool <claude|codex|gemini|opencode>")
        );
        return SetupOutcome::Skipped;
    }

    let detected: Vec<(CliTool, bool)> = ALL_AGENT_TOOLS
        .iter()
        .map(|t| (*t, which::which(binary_for(*t)).is_ok()))
        .collect();

    println!();
    println!("{}", "Pick a review provider:".bold());
    println!();

    for (idx, (tool, present)) in detected.iter().enumerate() {
        let n = idx + 1;
        if *present {
            println!(
                "  {}) {}",
                style::ok(&n.to_string()),
                format!(
                    "{:<14}     {} detected - reuses your existing auth, zero extra setup",
                    tool.to_string(),
                    style::sym::OK
                )
                .bold()
            );
        } else {
            println!(
                "  {}  {}",
                style::pewter(style::sym::BULLET),
                style::pewter(&format!(
                    "{:<14}     not on PATH, install `{}` to enable",
                    tool.to_string(),
                    binary_for(*tool)
                ))
            );
        }
    }
    println!();

    let default = detected
        .iter()
        .position(|(_, present)| *present)
        .map(|i| i + 1);
    let Some(default) = default else {
        crate::commands::util::exit_err(
            "no agent CLI detected on PATH. Install one of `claude`, `codex`, `gemini`, or `opencode`, then re-run setup.",
        );
    };
    let default_tool = detected[default - 1].0;
    println!(
        "  {} Enter defaults to {} (choice {default}).",
        style::pewter(style::sym::BULLET),
        style::ident(&default_tool.to_string()),
    );

    let choice = prompt("Choice", &default.to_string());
    let parsed: usize = if let Ok(n) = choice.parse() {
        n
    } else {
        crate::commands::util::exit_err(&format!(
            "{choice:?} is not a valid choice. Expected 1..={}.",
            ALL_AGENT_TOOLS.len()
        ));
    };

    if !(1..=ALL_AGENT_TOOLS.len()).contains(&parsed) {
        crate::commands::util::exit_err(&format!(
            "{parsed} is out of range. Expected 1..={}.",
            ALL_AGENT_TOOLS.len()
        ));
    }

    let (tool, present) = detected[parsed - 1];
    if !present {
        crate::commands::util::exit_err(&format!(
            "{tool} CLI not detected. Install `{}` first.",
            binary_for(tool)
        ));
    }
    setup_agent_cli(db, tool).await;
    SetupOutcome::Configured
}

async fn setup_agent_cli(db: &difflore_core::SqlitePool, tool: CliTool) {
    let default_model = default_model_for(tool);
    println!(
        "  {}",
        style::pewter(&model_default_hint(tool, default_model))
    );
    let model_input = prompt(&model_prompt_label(tool, default_model), default_model);
    let mut mapping = HashMap::new();
    mapping.insert("review".into(), model_input.clone());
    mapping.insert("default".into(), model_input.clone());

    let summary = if model_input.is_empty() {
        format!("{tool} CLI (CLI-default model)")
    } else {
        format!("{tool} CLI (model: {model_input})")
    };

    save_provider(
        db,
        provider_name_for(tool),
        difflore_core::review_engine::agent_cli_sentinel(tool),
        mapping,
        summary,
    )
    .await;
}

fn model_prompt_label(tool: CliTool, default_model: &str) -> String {
    if default_model.is_empty() {
        format!("Model (Enter = {tool} CLI default)")
    } else {
        "Model (Enter = recommended default)".to_owned()
    }
}

fn model_default_hint(tool: CliTool, default_model: &str) -> String {
    if default_model.is_empty() {
        format!(
            "Press Enter to let {tool} choose its own default model; type a model only if you need to pin one."
        )
    } else {
        format!(
            "Press Enter to use {default_model}; type another model only if your local {tool} account should pin it."
        )
    }
}

async fn save_provider(
    db: &difflore_core::SqlitePool,
    name: &str,
    base_url: &str,
    model_mapping: HashMap<String, String>,
    human_summary: String,
) {
    let added = match difflore_core::domain::providers::add(
        db,
        ProviderAddInput {
            name: name.to_owned(),
            base_url: base_url.to_owned(),
            model_mapping,
        },
    )
    .await
    {
        Ok(p) => p,
        Err(e) => {
            crate::commands::util::exit_err(&format!(
                "failed to save provider: {e}\n  Run {} to inspect local DB / migration state.\n  Local data lives at {} — back up before any recovery action.",
                style::cmd("difflore doctor"),
                style::pewter("~/.difflore/data.db")
            ));
        }
    };

    if let Err(e) = difflore_core::domain::providers::set_active(
        db,
        ProviderSetActiveInput {
            id: added.id.clone(),
            is_active: true,
        },
    )
    .await
    {
        eprintln!(
            "{} provider saved but failed to mark active: {e}",
            style::warn("warning:")
        );
    }

    println!();
    println!(
        "  {} active provider: {}",
        style::ok(style::sym::OK),
        style::ident(&human_summary)
    );
}

fn prompt(label: &str, default: &str) -> String {
    if default.is_empty() {
        print!("{label}: ");
    } else {
        print!("{label} [{}]: ", style::pewter(default));
    }
    io::stdout().flush().ok();
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line).ok();
    let trimmed = trim_prompt_line(&line);
    if trimmed.is_empty() {
        default.to_owned()
    } else {
        trimmed
    }
}

fn trim_prompt_line(line: &str) -> String {
    line.trim_matches(|c: char| c.is_whitespace() || c == '\0' || c == '\u{feff}')
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::{default_model_for, model_default_hint, model_prompt_label, trim_prompt_line};
    use gate4agent::CliTool;

    #[test]
    fn prompt_trim_handles_crlf_and_control_bytes_without_lowercasing() {
        assert_eq!(trim_prompt_line("Codex\r\n"), "Codex");
        assert_eq!(
            trim_prompt_line("\u{feff}gemini-2.5-pro\0\r\n"),
            "gemini-2.5-pro"
        );
    }

    #[test]
    fn model_prompt_distinguishes_pinned_and_cli_defaults() {
        let claude_default = default_model_for(CliTool::ClaudeCode);
        assert!(model_prompt_label(CliTool::ClaudeCode, claude_default).contains("recommended"));
        assert!(model_default_hint(CliTool::ClaudeCode, claude_default).contains(claude_default));

        let codex_default = default_model_for(CliTool::Codex);
        assert!(codex_default.is_empty());
        assert!(model_prompt_label(CliTool::Codex, codex_default).contains("CLI default"));
        assert!(model_default_hint(CliTool::Codex, codex_default).contains("choose its own"));
    }
}
