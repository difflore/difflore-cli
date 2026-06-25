use std::sync::OnceLock;

use clap::CommandFactory;

use crate::style;

use super::Cli;

// Built in code rather than via `#[derive]` so the help/version templates can
// use the truecolor wordmark, computed lazily at first use.
pub(crate) fn build_cli() -> clap::Command {
    let cmd = Cli::command();
    cmd.override_help(help_template()).version(version_string())
}

static HELP_TEMPLATE: OnceLock<String> = OnceLock::new();
static VERSION_STRING: OnceLock<String> = OnceLock::new();

// Clap prefixes the command name onto this, so supply the bare version.
fn version_string() -> &'static str {
    VERSION_STRING
        .get_or_init(|| env!("CARGO_PKG_VERSION").to_owned())
        .as_str()
}

// Hand-rolled help template (clap `override_help`) keeps the top-level surface
// curated while still honoring `NO_COLOR`.
fn help_template() -> &'static str {
    HELP_TEMPLATE
        .get_or_init(|| {
            let start = style::pewter("START").to_string();
            let build_rules = style::pewter("BUILD RULES").to_string();
            let connect_agents = style::pewter("CONNECT AGENTS").to_string();
            let use_rules = style::pewter("USE RULES").to_string();
            let more = style::pewter("MORE").to_string();
            let tip = style::emerald(style::sym::TIP).to_string();
            format!(
                "\
Source-backed team rules for local coding agents.
Your agents recall how your team reviews code before they write it.

USAGE
  difflore                      Show what's ready and your next step
  difflore <command> --help

{start}
  try                 See it work on a bundled sample, no setup
  init                Set up DiffLore for this repo

{build_rules}
  import-reviews      Turn past GitHub/GitLab review comments into rules
  memory              Autopilot local rules and review what remains
  learn               Force DiffLore to learn from the latest session now

{connect_agents}
  agents              Connect DiffLore to local coding agents
  export              Write a static snapshot to AGENTS.md or CLAUDE.md

{use_rules}
  recall              Preview the rules an agent would see for a diff
  review              Review a diff without modifying files
  ask                 Ask why the team usually reviews something
  fix                 Apply rule-aware local patches

{more}
  cloud               Optional: sync team state and selected cloud queues
  providers           Choose the local AI backend for rule-aware fixes
  embeddings          Tune semantic recall quality
  auth                Store GitLab import credentials
  doctor              Diagnose installs, hooks, sync, and recall
  update              Refresh installed agent blocks and run diagnostics

{tip} New here? Run difflore try, then difflore init.
",
            )
        })
        .as_str()
}
