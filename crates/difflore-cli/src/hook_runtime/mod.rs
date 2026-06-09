use crate::hooks;

mod dispatch;
mod fire_log;
mod stated_vs_actual;

pub(crate) use dispatch::hook_output_for_raw;
pub(crate) use fire_log::{HookFireSummary, hook_fire_summary_24h};

pub async fn output_for_raw(client_name: &str, raw: &str, debug: bool) -> anyhow::Result<String> {
    let adapter = hooks::get_platform_adapter(client_name);
    hook_output_for_raw(client_name, &*adapter, raw, debug, false, None).await
}
