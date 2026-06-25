//! `difflore ask` — friendly Q&A wrapper over the team's source-backed rules.
//!
//! Today this is a delegating alias for `difflore recall`: same retrieval,
//! conversational framing, and a footer that points users back to sync /
//! recall when they need a fresher corpus. The `--file` flag is forwarded
//! through so users can scope the question to one file's diff.

use crate::commands::recall::{RecallArgs, handle_recall};
use crate::runtime::CommandContext;
use crate::style::{self, sym};
use crate::support::util::exit_code;

const DEFAULT_TOP_K: usize = 5;

pub(crate) async fn handle_ask(
    ctx: &CommandContext,
    query: String,
    file: Option<String>,
    json: bool,
) {
    let query = query.trim().to_owned();
    if query.is_empty() {
        eprintln!(
            "{} `difflore ask` needs a question; try `difflore ask \"why do we ban unwrap?\"`",
            style::err(sym::ERR),
        );
        exit_code(2);
    }

    handle_recall(
        ctx,
        RecallArgs {
            intent: Some(query),
            file,
            diff: false,
            top_k: DEFAULT_TOP_K,
            json,
            verbose: false,
            copy: false,
        },
    )
    .await;

    if !json {
        let client = ctx.cloud().await;
        let (message, command) = if client.is_logged_in() {
            (
                "For fresher answers, sync team rules first",
                "difflore cloud sync",
            )
        } else {
            (
                "For team rules, sign in to Cloud first",
                "difflore cloud login",
            )
        };
        println!(
            "  {} {message}: {}",
            style::pewter(sym::BULLET),
            style::cmd(command),
        );
    }
}
