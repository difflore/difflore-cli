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

use super::tools::{
    binary_for, build_model_mapping, default_model_for, provider_display_label, provider_name_for,
};

// Order here drives both the picker's numbering and the default choice
// (the first tool detected on PATH wins). Codex is listed first so it is the
// recommended default when present.
const ALL_AGENT_TOOLS: [CliTool; 4] = [
    CliTool::Codex,
    CliTool::ClaudeCode,
    CliTool::Gemini,
    CliTool::OpenCode,
];

const PROVIDER_LABEL_WIDTH: usize = 14;
const PROVIDER_STATUS_WIDTH: usize = 11;

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
            style::cmd("difflore providers add --tool <codex|claude|gemini|opencode>")
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
        print_provider_picker_line(idx + 1, *tool, *present);
    }
    println!();

    let default = detected
        .iter()
        .position(|(_, present)| *present)
        .map(|i| i + 1);
    let Some(default) = default else {
        crate::support::util::exit_err(
            "no agent CLI detected on PATH. Install one of `codex`, `claude`, `gemini`, or `opencode`, then re-run setup.",
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
        crate::support::util::exit_err(&format!(
            "{choice:?} is not a valid choice. Expected 1..={}.",
            ALL_AGENT_TOOLS.len()
        ));
    };

    if !(1..=ALL_AGENT_TOOLS.len()).contains(&parsed) {
        crate::support::util::exit_err(&format!(
            "{parsed} is out of range. Expected 1..={}.",
            ALL_AGENT_TOOLS.len()
        ));
    }

    let (tool, present) = detected[parsed - 1];
    if !present {
        crate::support::util::exit_err(&format!(
            "{tool} CLI not detected. Install `{}` first.",
            binary_for(tool)
        ));
    }
    setup_agent_cli(db, tool).await;
    SetupOutcome::Configured
}

fn provider_picker_parts(
    index: usize,
    tool: CliTool,
    present: bool,
) -> (String, String, String, String) {
    let marker = if present {
        format!("{index})")
    } else {
        style::sym::BULLET.to_owned()
    };
    let label = format!(
        "{:<width$}",
        provider_display_label(tool),
        width = PROVIDER_LABEL_WIDTH
    );
    let status = format!(
        "{:<width$}",
        if present {
            "OK detected"
        } else {
            "not on PATH"
        },
        width = PROVIDER_STATUS_WIDTH
    );
    let detail = if present {
        "reuses your existing auth, zero extra setup".to_owned()
    } else {
        format!("install `{}` to enable", binary_for(tool))
    };
    (marker, label, status, detail)
}

#[cfg(test)]
fn provider_picker_plain_line(index: usize, tool: CliTool, present: bool) -> String {
    let (marker, label, status, detail) = provider_picker_parts(index, tool, present);
    format!("  {marker:<2} {label}  {status} - {detail}")
}

fn print_provider_picker_line(index: usize, tool: CliTool, present: bool) {
    let (marker, label, status, detail) = provider_picker_parts(index, tool, present);
    let marker = format!("{marker:<2}");
    if present {
        println!(
            "  {} {}  {} - {}",
            style::ok(&marker),
            label.bold(),
            style::ok(&status),
            detail.bold()
        );
    } else {
        println!(
            "  {} {}  {} - {}",
            style::pewter(&marker),
            style::pewter(&label),
            style::pewter(&status),
            style::pewter(&detail)
        );
    }
}

async fn setup_agent_cli(db: &difflore_core::SqlitePool, tool: CliTool) {
    let default_model = default_model_for(tool);
    println!(
        "  {}",
        style::pewter(&model_default_hint(tool, default_model))
    );
    let model_input = prompt(&model_prompt_label(tool, default_model), default_model);
    let mapping = build_model_mapping(&model_input);

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
    let added = match difflore_core::infra::providers::add(
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
            crate::support::util::exit_err(&format!(
                "failed to save provider: {e}\n  Run {} to inspect local DB / migration state.\n  Local data lives at {} — back up before any recovery action.",
                style::cmd("difflore doctor"),
                style::pewter("~/.difflore/data.db")
            ));
        }
    };

    if let Err(e) = difflore_core::infra::providers::set_active(
        db,
        ProviderSetActiveInput {
            id: added.id.clone(),
            is_active: true,
        },
    )
    .await
    {
        crate::support::util::exit_err(&format!(
            "provider saved but failed to mark active: {e}\n  Run {} to inspect local DB / migration state.",
            style::cmd("difflore doctor")
        ));
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
    match io::stdin().lock().read_line(&mut line) {
        Ok(0) => {
            crate::support::util::exit_err(&format!(
                "stdin closed while reading {label}; provider setup cancelled."
            ));
        }
        Ok(_) => {}
        Err(e) => {
            crate::support::util::exit_err(&format!("failed to read {label} from stdin: {e}"));
        }
    }
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
    use super::{
        default_model_for, model_default_hint, model_prompt_label, provider_picker_plain_line,
        trim_prompt_line,
    };
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

    #[test]
    fn provider_picker_rows_align_status_column() {
        let rows = [
            provider_picker_plain_line(1, CliTool::Codex, true),
            provider_picker_plain_line(2, CliTool::ClaudeCode, true),
            provider_picker_plain_line(3, CliTool::Gemini, false),
            provider_picker_plain_line(4, CliTool::OpenCode, false),
        ];
        let status_columns: Vec<usize> = rows
            .iter()
            .map(|row| {
                row.find("OK detected")
                    .or_else(|| row.find("not on PATH"))
                    .expect("status column")
            })
            .collect();
        assert!(
            status_columns.windows(2).all(|pair| pair[0] == pair[1]),
            "status columns should align: {rows:#?}"
        );
        assert!(rows[0].contains("Codex CLI"));
        assert!(rows[2].contains("Gemini CLI"));
        assert!(rows[3].contains("OpenCode CLI"));
    }
}
