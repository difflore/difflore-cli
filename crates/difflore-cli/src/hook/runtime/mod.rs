use crate::hook::adapters;

mod bash_error;
mod correction_nudge;
mod dispatch;
mod drift_report;
mod fire_log;
mod pre_submit_nudge;
mod project;
mod remember_nudge;

pub(crate) use dispatch::hook_output_for_raw;
pub(crate) use fire_log::{HookFireSummary, hook_fire_summary_24h};

pub async fn output_for_raw(client_name: &str, raw: &str, debug: bool) -> anyhow::Result<String> {
    output_for_raw_with_forward_miss(client_name, raw, debug, false).await
}

pub async fn output_for_raw_with_forward_miss(
    client_name: &str,
    raw: &str,
    debug: bool,
    forward_miss: bool,
) -> anyhow::Result<String> {
    let adapter = adapters::get_platform_adapter(client_name);
    hook_output_for_raw(
        client_name,
        &*adapter,
        raw,
        debug,
        false,
        None,
        forward_miss,
    )
    .await
}
