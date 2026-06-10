use crate::commands::audit_history::load_audit_history;
use crate::commands::doctor::labels::doctor_probe_freshness;
use crate::commands::util::git_str;
use crate::hook_runtime;

use super::formatters::doctor_command_version;
use super::validators::{
    corpus_health_subsection, embedder_status_subsection, embedding_profile_match_subsection,
    self_recall_section,
};

pub(super) async fn versions_section(s: &mut String) {
    sw!(s, "## ✓ Versions\n");
    sw!(s, "- difflore: `{}`", env!("CARGO_PKG_VERSION"));
    let rustc = doctor_command_version("rustc").await;
    sw!(s, "- rustc: `{rustc}`");
    for (label, cmd) in [
        ("claude", "claude"),
        ("cursor-agent", "cursor-agent"),
        ("gemini", "gemini"),
        ("codex", "codex"),
    ] {
        let version = doctor_command_version(cmd).await;
        sw!(s, "- {label}: `{version}`");
    }
}

pub(super) fn platform_section(s: &mut String) {
    sw!(s, "\n## ✓ Platform\n");
    sw!(s, "- os: `{}`", std::env::consts::OS);
    sw!(s, "- arch: `{}`", std::env::consts::ARCH);
    sw!(s, "- family: `{}`", std::env::consts::FAMILY);
}

pub(super) fn env_and_git_section(s: &mut String) {
    // Boolean-only env presence so a pasted report never leaks key material.
    sw!(s, "\n## ✓ Environment\n");
    let env_keys = [
        "DIFFLORE_DEBUG_HOOKS",
        // Optional: traces cloud-client transport errors to stderr. Off by
        // default — degraded paths return empty/false sentinels silently.
        "DIFFLORE_DEBUG_CLOUD",
        "DIFFLORE_HOOK_CLIENT",
        "DIFFLORE_HOME",
        // Kill-switch for MCP rule injection — recommended for haiku-tier.
        "DIFFLORE_DISABLE_RULES",
        // BYOK embedding key for `difflore embeddings setup` (env/stdin input).
        "DIFFLORE_EMBEDDING_KEY",
    ];
    for key in env_keys {
        let present = difflore_core::env::var_os(key).is_some_and(|v| !v.is_empty());
        let mark = if present { "✓" } else { "·" };
        let label = if present { "set" } else { "unset" };
        sw!(s, "- {mark} `{key}`: {label}");
    }

    if let Some(head) = git_str(&["rev-parse", "--short", "HEAD"]).filter(|h| !h.is_empty()) {
        let branch = git_str(&["rev-parse", "--abbrev-ref", "HEAD"])
            .unwrap_or_else(|| "(detached)".to_owned());
        let dirty_count = git_str(&["status", "--porcelain"])
            .map_or(0, |s| s.lines().filter(|l| !l.is_empty()).count());
        sw!(
            s,
            "- git: `{head}` on `{branch}` ({dirty_count} dirty file(s))"
        );
    } else {
        sw!(s, "- git: (not in a git repo)");
    }
}

pub(super) async fn startup_section(
    ctx: &crate::runtime::CommandContext,
    s: &mut String,
) -> (bool, String) {
    sw!(s, "\n## ✓ Startup health\n");
    let now = chrono::Utc::now();
    let mut cloud_logged_in = false;
    let cloud_probe: String;
    match difflore_core::startup::ensure_ready(false).await {
        Ok(status) => {
            sw!(s, "- startup cache version: `{}`", status.version);
            sw!(
                s,
                "- migrations probe: `{}` ({})",
                status.migrations_applied_at.to_rfc3339(),
                doctor_probe_freshness(Some(status.migrations_applied_at), now)
            );
            let provider_line = match status.provider_ok_at {
                Some(ts) => format!(
                    "`{}` ({})",
                    ts.to_rfc3339(),
                    doctor_probe_freshness(Some(ts), now)
                ),
                None => "missing (provider config unreadable; retry remains non-blocking)".into(),
            };
            sw!(s, "- provider probe: {provider_line}");

            let logged_in = ctx.cloud().await.is_logged_in();
            cloud_logged_in = logged_in;
            let cloud_line = match (logged_in, status.cloud_ok_at) {
                (false, None) => "not logged in (probe skipped)".to_owned(),
                (true, None) => {
                    "missing (cloud unreachable or probe failed; CLI still degrades locally)"
                        .to_owned()
                }
                (_, Some(ts)) => format!(
                    "`{}` ({})",
                    ts.to_rfc3339(),
                    doctor_probe_freshness(Some(ts), now)
                ),
            };
            cloud_probe = cloud_line.clone();
            sw!(s, "- cloud probe: {cloud_line}");
            sw!(
                s,
                "- degradation policy: `provider` / `cloud` probe failures do not block CLI startup; they clear their timestamps and retry on a later invocation"
            );
        }
        Err(e) => {
            sw!(s, "- ✗ startup gate failed: `{e}`");
            cloud_probe = format!("startup gate failed: {e}");
        }
    }
    (cloud_logged_in, cloud_probe)
}

pub(super) fn paths_section(s: &mut String) {
    sw!(s, "\n## ✓ Paths\n");
    let cwd =
        std::env::current_dir().map_or_else(|_| "(unknown)".into(), |p| p.display().to_string());
    sw!(s, "- cwd: `{cwd}`");
    let project_root = difflore_core::db::current_project_root();
    sw!(s, "- project root: `{}`", project_root.display());
    if let Some(home) = dirs::home_dir() {
        let difflore_dir = home.join(".difflore");
        sw!(s, "- `~/.difflore/` present: `{}`", difflore_dir.exists());
        if difflore_dir.exists() {
            match std::fs::read_dir(&difflore_dir) {
                Ok(entries) => {
                    let names: Vec<String> = entries
                        .filter_map(Result::ok)
                        .map(|e| e.file_name().to_string_lossy().to_string())
                        .collect();
                    sw!(s, "- `~/.difflore/` top-level: `{}`", names.join(", "));
                }
                Err(e) => {
                    sw!(s, "- (failed to list: {e})");
                }
            }
            let projects_dir = difflore_dir.join("projects");
            if projects_dir.exists()
                && let Ok(entries) = std::fs::read_dir(&projects_dir)
            {
                let count = entries.filter_map(Result::ok).count();
                sw!(s, "- per-project index count: {count}");
            }
        }
    }
}

pub(super) async fn database_section(ctx: &crate::runtime::CommandContext, s: &mut String) {
    sw!(s, "\n## · Database\n");
    let pool = &ctx.db;
    db_tables_subsection(pool, s).await;
    db_outbox_subsection(pool, s).await;
    corpus_health_subsection(pool, s).await;
    embedder_status_subsection(s).await;
    embedding_profile_match_subsection(s).await;
    self_recall_section(pool, s).await;
}

async fn db_tables_subsection(pool: &difflore_core::SqlitePool, s: &mut String) {
    let tables = [
        "skills",
        "review_items",
        "review_comments",
        "providers",
        "cloud_outbox",
    ];
    let counts = difflore_core::db::table_counts(pool, &tables).await;
    for (table, result) in counts {
        match result {
            Ok(n) => sw!(s, "- {table}: {n}"),
            Err(e) => sw!(s, "- {table}: (error: {e})"),
        }
    }
}

async fn db_outbox_subsection(pool: &difflore_core::SqlitePool, s: &mut String) {
    let queue = difflore_core::cloud::outbox::OutboxQueue::new(pool.clone());
    match queue.counts().await {
        Ok(counts) => {
            sw!(
                s,
                "- outbox status: pending={}, processing={}, failed={}, abandoned={}",
                counts.pending,
                counts.processing,
                counts.failed,
                counts.abandoned,
            );
        }
        Err(e) => {
            sw!(s, "- outbox status: (error: {e})");
        }
    }
}

pub(super) fn hook_activity_section(s: &mut String) -> hook_runtime::HookFireSummary {
    let hook_summary = hook_runtime::hook_fire_summary_24h();
    let hook_mark = if hook_summary.detail.as_deref() == Some("hook fire log is unreadable") {
        "✗"
    } else if hook_summary.count_24h > 0 {
        "✓"
    } else {
        "⚠"
    };
    sw!(s, "\n## {hook_mark} Hook activity 24h\n");
    sw!(s, "- hook fire count 24h: {}", hook_summary.count_24h);
    if let Some(path) = &hook_summary.path {
        sw!(s, "- hook fire log: `{}`", path.display());
    }
    if let Some(detail) = &hook_summary.detail {
        sw!(s, "- detail: {detail}");
    }
    if !hook_summary.by_client.is_empty() {
        let by_client = hook_summary
            .by_client
            .iter()
            .map(|(client, count)| format!("{client}={count}"))
            .collect::<Vec<_>>()
            .join(", ");
        sw!(s, "- by client: {by_client}");
    }
    if !hook_summary.by_event.is_empty() {
        let by_event = hook_summary
            .by_event
            .iter()
            .map(|(event, count)| format!("{event}={count}"))
            .collect::<Vec<_>>()
            .join(", ");
        sw!(s, "- by event: {by_event}");
    }
    // Entries predating the instrumentation count toward count_24h but not
    // injected_fires, so the rate may dip right after upgrade.
    if hook_summary.count_24h > 0 {
        let pct = (hook_summary.injected_fires as f64 / hook_summary.count_24h as f64) * 100.0;
        let avg = if hook_summary.injected_fires > 0 {
            hook_summary.total_rules_injected as f64 / hook_summary.injected_fires as f64
        } else {
            0.0
        };
        sw!(
            s,
            "- rule-injection hit rate: {}/{} ({:.1}%) — total rules injected: {} (avg {:.1}/inject)",
            hook_summary.injected_fires,
            hook_summary.count_24h,
            pct,
            hook_summary.total_rules_injected,
            avg,
        );
    }
    if let Some(ms) = hook_summary.median_elapsed_ms {
        sw!(
            s,
            "- hook median latency 24h: {ms} ms (over {} timed fire(s))",
            hook_summary.timed_fires,
        );
    }

    // Audit-history rollup: passive prune nudge without forcing the user
    // to remember `--history`. Capped at 50 most-recent runs.
    let audit_runs = load_audit_history(50).unwrap_or_else(|e| {
        eprintln!("warn: audit history rollup skipped — {e}");
        Vec::new()
    });
    if !audit_runs.is_empty() {
        let agg = difflore_core::context::intent_filter::aggregate_audit_history(&audit_runs);
        let always_noise = agg.iter().filter(|s| s.matched >= 3 && s.top == 0).count();
        let healthy = agg.iter().filter(|s| s.top >= 1).count();
        sw!(
            s,
            "- memory audit rollup: {} run(s), {} rule(s) seen, {} always-noise (matched >=3 runs / never top-N), {} healthy",
            audit_runs.len(),
            agg.len(),
            always_noise,
            healthy,
        );
        if always_noise > 0 {
            sw!(
                s,
                "  ▸ run `difflore doctor --report` after a few fixes to refresh this signal"
            );
        }
    }

    hook_summary
}

pub(super) fn injection_paths_section(s: &mut String) {
    let path_summary = difflore_core::injection_log::summary_24h();
    let path_mark = if path_summary
        .detail
        .as_deref()
        .is_some_and(|d| d.contains("unreadable"))
    {
        "✗"
    } else if path_summary.count_24h > 0 {
        "✓"
    } else {
        "⚠"
    };
    sw!(s, "\n## {path_mark} Injection paths 24h\n");
    sw!(s, "- injection events 24h: {}", path_summary.count_24h);
    if let Some(path) = &path_summary.path {
        sw!(s, "- injection path log: `{}`", path.display());
    }
    if let Some(detail) = &path_summary.detail {
        sw!(s, "- detail: {detail}");
    }
    if !path_summary.by_path.is_empty() {
        let by_path = path_summary
            .by_path
            .iter()
            .map(|(path, count)| {
                let injected = path_summary
                    .injected_by_path
                    .get(path)
                    .copied()
                    .unwrap_or(0);
                format!("{path}={injected}/{count}")
            })
            .collect::<Vec<_>>()
            .join(", ");
        sw!(s, "- by path (injected/seen): {by_path}");
        sw!(
            s,
            "- total rules injected across paths: {}",
            path_summary.total_rules_injected
        );
    }
}

pub(super) async fn rules_origin_section(ctx: &crate::runtime::CommandContext, s: &mut String) {
    sw!(s, "\n## ✓ Rules by origin\n");
    match difflore_core::skills::stats(&ctx.db).await {
        Ok(stats) if stats.by_origin.is_empty() => {
            sw!(s, "- ⚠ no rules installed");
        }
        Ok(stats) => {
            for row in stats.by_origin {
                sw!(s, "- {}: {}", row.origin, row.count);
            }
        }
        Err(e) => {
            sw!(s, "- ✗ failed to load rule origin counts: {e}");
        }
    }
}

pub(super) fn memory_pipeline_section(s: &mut String) {
    let stream_events = difflore_core::activity_stream::tail(200);
    let stream_mark = if stream_events.is_empty() {
        "⚠"
    } else {
        "✓"
    };
    sw!(s, "\n## {stream_mark} Memory pipeline\n");
    if stream_events.is_empty() {
        sw!(
            s,
            "- activity log: empty (run an MCP-wired agent against a file in this repo to populate)"
        );
    } else {
        let mut recalled = 0usize;
        let mut injected = 0usize;
        let mut reinforced = 0usize;
        let mut embedding = 0usize;
        let mut cap_hits = 0usize;
        let mut latest_cap: Option<(u32, u32)> = None;
        let mut embedding_fallbacks = 0usize;
        let mut fallback_reasons = std::collections::BTreeMap::<String, usize>::new();
        let mut newest_ts: i64 = 0;
        let mut oldest_ts: i64 = i64::MAX;
        for ev in &stream_events {
            newest_ts = newest_ts.max(ev.ts_ms);
            oldest_ts = oldest_ts.min(ev.ts_ms);
            use difflore_core::activity_stream::ActivityPayload as P;
            match &ev.payload {
                P::RuleRecalled { .. } => recalled += 1,
                P::RuleInjected { .. } => injected += 1,
                P::RuleReinforced { .. } => reinforced += 1,
                P::RetrievalEmbedding { .. } => embedding += 1,
                P::EmbedCapReached { cap, used } => {
                    cap_hits += 1;
                    latest_cap.get_or_insert((*cap, *used));
                }
                P::EmbeddingFallback { reason } => {
                    embedding_fallbacks += 1;
                    *fallback_reasons.entry(reason.clone()).or_default() += 1;
                }
            }
        }
        let now_ms = chrono::Utc::now().timestamp_millis();
        let newest_age_secs = (now_ms - newest_ts).max(0) / 1000;
        let span_secs = (newest_ts - oldest_ts).max(0) / 1000;
        sw!(
            s,
            "- activity log: {} events spanning {}s (newest {}s ago)",
            stream_events.len(),
            span_secs,
            newest_age_secs,
        );
        sw!(
            s,
            "- by kind: {recalled} recalled · {injected} injected · {reinforced} reinforced · {embedding} embedding · {embedding_fallbacks} embedding fallback"
        );
        if embedding_fallbacks > 0 {
            let reasons = fallback_reasons
                .iter()
                .map(|(reason, count)| format!("{reason}×{count}"))
                .collect::<Vec<_>>()
                .join(", ");
            sw!(
                s,
                "- ⚠ embedding provider fell back to local SHA1 {embedding_fallbacks}× ({reasons}) — run `difflore doctor` or `difflore embeddings setup` if this persists"
            );
        }
        if cap_hits > 0 {
            let cap_detail = latest_cap.map_or_else(
                || "cloud embedding cap".to_owned(),
                |(cap, used)| format!("cloud embedding cap ({used}/{cap})"),
            );
            sw!(
                s,
                "- ⚠ {cap_detail} reached {cap_hits}× — capped managed embeds fell back to local-lexical\n\
                 \x20 → `difflore embeddings setup` to switch to BYOK, or upgrade Team for unlimited managed embedding"
            );
        }
        if reinforced == 0 && recalled > 0 {
            sw!(
                s,
                "- ⚠ recalls but zero reinforcements — rules are getting recalled but no local fix or agent edit outcome has been accepted/rejected yet, so half-life decay can't promote what works"
            );
            sw!(
                s,
                "                       ▸ accept/reject a `difflore fix --preview` suggestion, or accept a matching MCP-wired agent edit, then run `difflore status`"
            );
        }
        if newest_age_secs > 86_400 {
            let days = newest_age_secs / 86_400;
            sw!(
                s,
                "- ⚠ newest event is {days}d old — agent activity has paused; run `difflore recall --diff` or open an editor with a wired agent to re-warm the stream"
            );
        }
    }
    let bfs_env = difflore_core::env::var(difflore_core::env::DIFFLORE_BFS_RETRIEVAL);
    let bfs_state = match bfs_env.as_deref() {
        Some(v) if matches!(v.trim(), "1" | "true" | "on" | "yes") => "ON (cloud-side)",
        Some(_) => "explicitly OFF",
        None => "default OFF (cloud-side)",
    };
    sw!(s, "- BFS cascade retrieval: {bfs_state}");
    if bfs_env.is_none() {
        sw!(
            s,
            "  → self-host only: set `DIFFLORE_BFS_RETRIEVAL=1` on the cloud worker (experimental — expands matches via Supersedes/RelatesTo edges, capped 3 hops). Managed cloud will flip default-on once eval clears regression bar."
        );
    }
    let rerank_env = difflore_core::env::var(difflore_core::env::DIFFLORE_INTENT_RERANK);
    let rerank_state = match rerank_env.as_deref() {
        None => "default ON · cap=5".to_owned(),
        Some(v) if matches!(v.trim(), "0" | "false" | "" | "off") => "explicitly OFF".to_owned(),
        Some(v) => format!("ON · cap=`{}`", v.trim()),
    };
    sw!(s, "- intent rerank: {rerank_state}");
    let disable_rules =
        difflore_core::env::var(difflore_core::env::DIFFLORE_DISABLE_RULES).is_some();
    if disable_rules {
        sw!(
            s,
            "- ⚠ DIFFLORE_DISABLE_RULES set — rule injection short-circuited (haiku-tier kill switch)"
        );
    }
    // Mirror `haiku_auto_disable_active()` so doctor and the actual injection
    // path can never disagree.
    if let Some(model) = difflore_core::mcp_server::detect_active_model() {
        if difflore_core::mcp_server::is_haiku_model(&model) {
            if difflore_core::mcp_server::haiku_auto_disable_active() {
                sw!(
                    s,
                    "- ⚠ haiku model detected (`{model}`) — rule injection auto-applied OFF (override: `DIFFLORE_FORCE_RULES_ON_HAIKU=1`)"
                );
            } else {
                sw!(
                    s,
                    "- ⚠ haiku model detected (`{model}`) — rule injection forced ON via DIFFLORE_FORCE_RULES_ON_HAIKU; expect −20pp recall vs bare"
                );
            }
        }
    }
}

pub(super) async fn sync_timestamps_section(
    ctx: &crate::runtime::CommandContext,
    s: &mut String,
    cloud_probe: &str,
) {
    sw!(s, "\n## ⚠ Sync timestamps\n");
    let pool = &ctx.db;
    match difflore_core::skills::list(pool).await {
        Ok(skills) => {
            let last_cloud_rule = skills
                .iter()
                .filter(|skill| {
                    matches!(
                        skill.origin.as_str(),
                        "cloud" | "team" | "extracted" | "pr_review"
                    )
                })
                .map(|skill| skill.updated_at.as_str())
                .max();
            let line = last_cloud_rule.unwrap_or("not recorded");
            sw!(s, "- last cloud/team rule sync timestamp: `{line}`");
        }
        Err(e) => {
            sw!(s, "- ✗ rule sync timestamp unavailable: {e}");
        }
    }
    match difflore_core::reviews::list_recent(pool, 500).await {
        Ok(items) => {
            let last_review_sync = items
                .iter()
                .filter_map(|item| item.synced_at.as_deref())
                .max()
                .unwrap_or("not recorded");
            sw!(
                s,
                "- last review import sync timestamp: `{last_review_sync}`"
            );
        }
        Err(e) => {
            sw!(s, "- ✗ review sync timestamp unavailable: {e}");
        }
    }
    sw!(s, "- last cloud reachability probe: {cloud_probe}");
}

pub(super) async fn cloud_section(
    s: &mut String,
    cloud_logged_in: bool,
    cloud_probe: &str,
    hook_summary: &hook_runtime::HookFireSummary,
) {
    cloud_auth_subsection(s, cloud_logged_in, cloud_probe);
    cloud_flags_subsection(s, cloud_logged_in, hook_summary);
    cloud_workspace_subsection(s).await;
}

fn cloud_auth_subsection(s: &mut String, cloud_logged_in: bool, cloud_probe: &str) {
    let cloud_mark = if cloud_logged_in && cloud_probe.contains("missing") {
        "✗"
    } else if cloud_logged_in {
        "✓"
    } else {
        "⚠"
    };
    sw!(s, "\n## {cloud_mark} Cloud reachability\n");
    sw!(s, "- logged in: `{cloud_logged_in}`");
    sw!(s, "- probe: {cloud_probe}");
    sw!(
        s,
        "- base URL: `{}`",
        difflore_core::cloud::client::CloudClient::resolve_cloud_url()
    );
}

// Telemetry-loss warning: unauthenticated `track_mcp_query` short-circuits
// silently in `post_fire_and_forget`, leaving the dashboard empty.
fn cloud_flags_subsection(
    s: &mut String,
    cloud_logged_in: bool,
    hook_summary: &hook_runtime::HookFireSummary,
) {
    if !cloud_logged_in && hook_summary.count_24h > 0 {
        sw!(
            s,
            "- ! activity: {} hook fire(s) in 24h did not reach cloud (unauthenticated). Dashboard activity feed + weekly digest will stay empty until `difflore cloud login`.",
            hook_summary.count_24h,
        );
    }
}

// Tier badge mirrors `difflore init`'s readiness block so OSS/Cloud looks
// identical across surfaces. Single line, no nag.
async fn cloud_workspace_subsection(s: &mut String) {
    let cloud_client = difflore_core::cloud::client::CloudClient::create().await;
    let cloud_status = difflore_core::cloud::sync::fetch_cloud_status(&cloud_client).await;
    let pricing = difflore_core::cloud::endpoints::pricing_url();
    let tier = crate::commands::init::tier_badge_line(&cloud_status);
    sw!(s, "- tier: {tier}");
    if cloud_status.logged_in {
        let plan = cloud_status.plan.as_deref().unwrap_or("free");
        if let Some(team) = cloud_status.team_name.as_deref() {
            sw!(s, "- plan: {plan} · team: `{team}` ✓");
        } else {
            sw!(s, "- plan: {plan}");
        }
        if !crate::commands::init::is_cloud_team(&cloud_status) {
            sw!(
                s,
                "  Team unlocks GitHub App review-memory ingest, governed team rules, \
                 Reviewer Context, and cross-machine sync — {pricing} · local BYOK stays free."
            );
        }
    } else {
        sw!(
            s,
            "- plan: OSS local runtime (not logged in). See `difflore cloud login` and {pricing}"
        );
    }
}
