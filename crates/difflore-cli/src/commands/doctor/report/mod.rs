// `writeln!` on a `String` is infallible but returns `fmt::Result`; this
// macro swallows the unused `Ok(())` without scattering `let _` everywhere.
macro_rules! sw {
    ($s:expr, $($arg:tt)*) => {{
        use std::fmt::Write as _;
        let _ = writeln!($s, $($arg)*);
    }};
}

mod env_probes;
mod formatters;
mod validators;

use env_probes::{
    cloud_section, database_section, env_and_git_section, hook_activity_section,
    injection_paths_section, memory_pipeline_section, paths_section, platform_section,
    rules_origin_section, startup_section, sync_timestamps_section, versions_section,
};
use formatters::{
    daemon_section, distribution_section, embedding_section, footer_section, mcp_section,
    settings_section,
};

pub(crate) async fn build_doctor_report(ctx: &crate::runtime::CommandContext) -> String {
    let mut s = String::new();
    sw!(s, "# difflore doctor report");
    sw!(
        s,
        "\n_Generated {}_\n",
        chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC")
    );

    versions_section(&mut s).await;
    platform_section(&mut s);
    env_and_git_section(&mut s);

    let (cloud_logged_in, cloud_probe) = startup_section(ctx, &mut s).await;
    paths_section(&mut s);
    database_section(ctx, &mut s).await;

    let hook_summary = hook_activity_section(&mut s);
    injection_paths_section(&mut s);
    rules_origin_section(ctx, &mut s).await;
    memory_pipeline_section(&mut s);
    sync_timestamps_section(ctx, &mut s, &cloud_probe).await;
    cloud_section(&mut s, cloud_logged_in, &cloud_probe, &hook_summary).await;
    embedding_section(ctx, &mut s).await;
    daemon_section(&mut s);
    distribution_section(&mut s);
    mcp_section(ctx, &mut s).await;
    settings_section(&mut s);
    footer_section(&mut s);

    s
}
