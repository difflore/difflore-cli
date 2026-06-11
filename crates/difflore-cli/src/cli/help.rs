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
            let use_ = style::pewter("USE").to_string();
            let support = style::pewter("SUPPORT").to_string();
            let more = format!(
                "{} Run any command with --help for details | docs.difflore.dev",
                style::emerald(style::sym::TIP),
            );
            format!(
                "\
Source-backed team rules for local coding agents.
Agents receive your team's review decisions before they code.

USAGE
  difflore [COMMAND]
  difflore                      {tip} show local rule status

{start}
  try                 See it work on a bundled sample
  init                First-time setup for this repo
  status              Show local rule status and the next command
  agents              Wire local agents and inspect install state
  cloud               Log in, sync, and view team impact
  providers           Choose the local AI backend for fixes
  embeddings          Optional semantic search for better recall
  import-reviews      Draft local rules from past GitHub PR reviews
  drafts              Review and approve pending memory drafts

{use_}
  recall              Preview rules agents would see on this change
  fix                 Preview/apply local patches, including PR diffs
  ask                 Ask why the team usually reviews something
  export              Write team rules into AGENTS.md / CLAUDE.md (static snapshot)

{support}
  update              Refresh agent blocks and run doctor checks
  doctor              Diagnose readiness and blockers

{more}
",
                tip = style::emerald(style::sym::TIP),
            )
        })
        .as_str()
}
