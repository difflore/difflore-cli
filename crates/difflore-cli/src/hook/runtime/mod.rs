use crate::hook::adapters;

mod bash_error;
mod dispatch;
mod drift_report;
mod fire_log;
mod remember_nudge;

pub(crate) use dispatch::hook_output_for_raw;
pub(crate) use fire_log::{HookFireSummary, hook_fire_summary_24h};

pub async fn output_for_raw(client_name: &str, raw: &str, debug: bool) -> anyhow::Result<String> {
    let adapter = adapters::get_platform_adapter(client_name);
    hook_output_for_raw(client_name, &*adapter, raw, debug, false, None).await
}
