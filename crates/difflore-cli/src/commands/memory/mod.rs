mod agent_files;
mod autopilot;
mod inbox;
mod package;
mod types;

use serde_json::json;

use crate::commands::cloud::sync::handle_sync as handle_cloud_sync;
use crate::runtime::CommandContext;
use crate::support::util::{exit_code, exit_err, json_compact_or};

/// Emit `message` as a structured error and terminate. In `--json` mode the
/// error is a compact `{"error": ...}` object (consumed line-by-line by hook
/// adapters); otherwise it is rendered as a styled human error. Shared across
/// the memory submodules so their failure surface stays uniform.
fn exit_structured_err(message: &str, json: bool) -> ! {
    if json {
        println!("{}", json_compact_or(&json!({ "error": message }), "{}"));
        exit_code(1);
    }
    exit_err(message);
}

/// `"1 rule"` / `"3 rules"` — `count` paired with the correctly pluralized
/// noun. Shared by the memory inbox and autopilot summaries.
fn count_phrase(count: i64, singular: &str, plural_word: &str) -> String {
    format!("{count} {}", plural(count, singular, plural_word))
}

/// Pick `singular` when `count == 1`, otherwise `plural_word`.
const fn plural<'a>(count: i64, singular: &'a str, plural_word: &'a str) -> &'a str {
    if count == 1 { singular } else { plural_word }
}

pub(crate) use agent_files::handle_import_agent_files;
pub(crate) use autopilot::{
    handle_autopilot, handle_cleanup, handle_conflicts, handle_digest, handle_disable, handle_log,
    handle_recommended, mark_memory_autopilot_dirty_best_effort,
    schedule_memory_autopilot_best_effort,
};
pub(crate) use inbox::{
    handle_active, handle_activity, handle_approve, handle_inbox, handle_reject, handle_remember,
    handle_review, handle_show, handle_summary,
};
pub(crate) use package::{handle_export_package, handle_import_package};

pub(crate) async fn handle_sync(
    ctx: &CommandContext,
    args: crate::commands::cloud::sync::SyncArgs,
) {
    handle_cloud_sync(ctx, args).await;
}
