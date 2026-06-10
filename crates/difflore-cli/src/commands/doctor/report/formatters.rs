use crate::commands::doctor::labels::{
    doctor_canonical_mark, doctor_canonical_record_state_label, doctor_install_mark,
    doctor_install_state_label,
};
use crate::commands::{dist, util::format_recall_edit_proof_breakdown};
use crate::mcp_install;

/// Map the canonical language slug emitted by `language_from_tags` /
/// `language_from_file_patterns` to a short display label suitable for
/// the doctor markdown breakdown.
pub(super) fn lang_short_label(canonical: &str) -> String {
    match canonical {
        "rust" => "Rust".to_owned(),
        "typescript" => "TS".to_owned(),
        "javascript" => "JS".to_owned(),
        "python" => "Python".to_owned(),
        "go" => "Go".to_owned(),
        "java" => "Java".to_owned(),
        "kotlin" => "Kotlin".to_owned(),
        "swift" => "Swift".to_owned(),
        "ruby" => "Ruby".to_owned(),
        "php" => "PHP".to_owned(),
        "cpp" => "C++".to_owned(),
        "csharp" => "C#".to_owned(),
        "c" => "C".to_owned(),
        other => other.to_owned(),
    }
}

pub(super) async fn doctor_command_version(cmd: &str) -> String {
    use std::time::Duration;

    use tokio::process::Command as TokioCommand;
    use tokio::time::timeout;

    let mut command = TokioCommand::new(cmd);
    command.kill_on_drop(true).arg("--version");
    let output = match timeout(Duration::from_secs(2), command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(_)) => return "(not found on PATH)".into(),
        Err(_) => return "(timed out after 2s)".into(),
    };

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    let text = if !stdout.is_empty() {
        stdout
    } else if !stderr.is_empty() {
        stderr
    } else if output.status.success() {
        "(no output)".into()
    } else {
        format!("(exit {})", output.status)
    };

    if output.status.success() {
        text
    } else {
        format!("{text} (exit {})", output.status)
    }
}

pub(super) fn daemon_section(s: &mut String) {
    let daemon_status = difflore_core::infra::daemon::status();
    let daemon_mark = match &daemon_status {
        difflore_core::infra::daemon::DaemonStatus::Running { .. } => "✓",
        difflore_core::infra::daemon::DaemonStatus::Stale { .. } => "✗",
        difflore_core::infra::daemon::DaemonStatus::NotRunning => "⚠",
    };
    sw!(s, "\n## {daemon_mark} Daemon\n");
    sw!(s, "- status: `{}`", daemon_status.short());
    if let Ok(pid_path) = difflore_core::infra::daemon::pid_path() {
        sw!(s, "- pid path: `{}`", pid_path.display());
    }
    sw!(
        s,
        "- degradation policy: hook uploads enqueue locally first; the daemon drains opportunistically and stale pid files are treated as recoverable state"
    );
}

pub(super) fn distribution_section(s: &mut String) {
    sw!(s, "\n## ✓ Distribution\n");
    match dist::verify_from_cwd() {
        Ok(report) => {
            sw!(s, "- repo root: `{}`", report.repo_root);
            if let Some(version) = &report.expected_version {
                sw!(s, "- manifest version: `{version}`");
            }
            sw!(
                s,
                "- dist verify: `{}` ({} error(s), {} warning(s))",
                if report.ok() { "ok" } else { "failed" },
                report.error_count(),
                report.warning_count()
            );
            for issue in report.issues.iter().take(10) {
                sw!(
                    s,
                    "- {:?}: `{}` — {}",
                    issue.severity,
                    issue.path,
                    issue.message
                );
            }
        }
        Err(e) => {
            sw!(s, "- dist verify: unavailable ({e})");
        }
    }
}

#[derive(Debug, Clone, Default)]
struct McpValueProof {
    active_rules: Option<i64>,
    imported_prs: Option<i64>,
    installed_clients: usize,
    mcp_tools: Option<usize>,
    local_accepted_edits_last30: Option<i64>,
    local_accepted_hook_outcomes_last30: Option<i64>,
    local_accepted_outcomes_linked_to_prior_recall_last30: Option<i64>,
    local_accepted_outcomes_linked_to_rule_recall_last30: Option<i64>,
    local_accepted_outcomes_linked_to_mcp_rule_serve_last30: Option<i64>,
    local_accepted_outcomes_linked_to_edit_attribution_last30: Option<i64>,
    local_total_outcomes_last30: Option<i64>,
    local_saved_review_time: Option<String>,
    accepted_fixes_last30: Option<i64>,
    total_fixes_last30: Option<i64>,
    saved_review_time: Option<String>,
}

pub(super) async fn mcp_section(ctx: &crate::runtime::CommandContext, s: &mut String) {
    let snapshot = mcp_install::collect_status_snapshot_with_runtime_probe();
    let value_proof = load_mcp_value_proof(ctx, &snapshot).await;
    mcp_installed_subsection(s, &snapshot, &value_proof);
    mcp_per_tool_subsection(s, &snapshot);
}

fn mcp_installed_subsection(
    s: &mut String,
    snapshot: &mcp_install::McpStatusSnapshot,
    value_proof: &McpValueProof,
) {
    sw!(
        s,
        "\n## {} MCP 11-client status\n",
        doctor_canonical_mark(snapshot.canonical_record.state)
    );
    sw!(
        s,
        "_Run `difflore agents status --json` for the full machine-readable snapshot._\n"
    );
    let detected_count = snapshot.agents.iter().filter(|a| a.detected).count();
    let installed_count = snapshot
        .agents
        .iter()
        .filter(|a| matches!(a.state, mcp_install::InstallState::Installed))
        .count();
    let conflict_count = snapshot
        .agents
        .iter()
        .filter(|a| matches!(a.state, mcp_install::InstallState::Conflict))
        .count();
    let unknown_count = snapshot
        .agents
        .iter()
        .filter(|a| matches!(a.state, mcp_install::InstallState::Unknown))
        .count();
    sw!(s, "- binary: `{}`", snapshot.binary);
    sw!(
        s,
        "- canonical record: `{}`",
        doctor_canonical_record_state_label(snapshot.canonical_record.state)
    );
    if let Some(path) = &snapshot.canonical_record.path {
        sw!(s, "- canonical record path: `{path}`");
    }
    if let Some(detail) = &snapshot.canonical_record.detail {
        sw!(s, "- canonical detail: {detail}");
    }
    if let Some(probe) = &snapshot.runtime_probe {
        sw!(
            s,
            "- runtime self-check: {} `{}`",
            doctor_runtime_probe_mark(probe.state),
            doctor_runtime_probe_state_label(probe.state)
        );
        sw!(s, "  detail: {}", probe.detail);
        sw!(s, "  initialized: `{}`", probe.initialized);
        sw!(s, "  tools listed: `{}`", probe.tools_listed);
        if let Some(count) = probe.tool_count {
            sw!(s, "  tool count: {count}");
        }
        if !probe.tool_names.is_empty() {
            sw!(s, "  tools: `{}`", probe.tool_names.join("`, `"));
        }
    }
    if let Some(diagnosis) = &snapshot.diagnosis {
        sw!(s, "- diagnosis: {}", diagnosis.summary);
        sw!(s, "- next step: {}", diagnosis.next_step);
        if !diagnosis.affected_clients.is_empty() {
            sw!(
                s,
                "- affected clients: `{}`",
                diagnosis.affected_clients.join("`, `")
            );
        }
        if !diagnosis.actions.is_empty() {
            sw!(s, "- actions:");
            for action in &diagnosis.actions {
                sw!(s, "  - {action}");
            }
        }
    }
    mcp_support_bundle_subsection(s, snapshot, value_proof);
    sw!(
        s,
        "- detected targets: {detected_count}, installed: {installed_count}, conflicts: {conflict_count}, unknown: {unknown_count}"
    );
    for client in &snapshot.clients {
        sw!(
            s,
            "- {} {}: {}",
            doctor_install_mark(client.detected, client.state),
            client.name,
            doctor_install_state_label(client.detected, client.state)
        );
        if let Some(detail) = &client.detail {
            sw!(s, "  detail: {detail}");
        }
    }
}

async fn load_mcp_value_proof(
    ctx: &crate::runtime::CommandContext,
    snapshot: &mcp_install::McpStatusSnapshot,
) -> McpValueProof {
    let mut proof = McpValueProof {
        installed_clients: report_installed_clients(snapshot).len(),
        mcp_tools: snapshot
            .runtime_probe
            .as_ref()
            .and_then(|probe| probe.tool_count),
        ..McpValueProof::default()
    };

    let pool = &ctx.db;
    if let Ok(stats) = difflore_core::skills::stats(pool).await {
        proof.active_rules = Some(stats.total);
    }
    let counts = difflore_core::infra::db::table_counts(pool, &["review_items"]).await;
    for (table, result) in counts {
        if table == "review_items"
            && let Ok(n) = result
        {
            proof.imported_prs = Some(n);
        }
    }
    if let Ok(summary) = difflore_core::observability::fix_outcomes::summary(pool, 30).await {
        let total = summary.applied + summary.failed + summary.rejected;
        proof.local_accepted_edits_last30 = Some(summary.applied);
        proof.local_total_outcomes_last30 = Some(total);
        proof.local_saved_review_time =
            crate::commands::impact_payload::saved_review_time_label(summary.applied * 4);
    }
    if let Ok(emitter) =
        difflore_core::cloud::observations::ObservationEmitter::open_default().await
    {
        if let Ok(hook_outcomes) = emitter.accepted_fix_outcome_count(30).await
            && hook_outcomes > 0
        {
            proof.local_accepted_hook_outcomes_last30 = Some(hook_outcomes);
            proof.local_total_outcomes_last30 =
                Some(proof.local_total_outcomes_last30.unwrap_or(0) + hook_outcomes);
            let accepted_total = local_accepted_proof_total(&proof);
            proof.local_saved_review_time =
                crate::commands::impact_payload::saved_review_time_label(accepted_total * 4);
        }
        if let Ok(summary) = emitter.accepted_recall_link_summary(30, 7).await {
            proof.local_accepted_outcomes_linked_to_prior_recall_last30 =
                Some(summary.linked_to_prior_recall);
            proof.local_accepted_outcomes_linked_to_rule_recall_last30 =
                Some(summary.linked_to_rule_recall);
            proof.local_accepted_outcomes_linked_to_mcp_rule_serve_last30 =
                Some(summary.linked_to_mcp_rule_serve);
            proof.local_accepted_outcomes_linked_to_edit_attribution_last30 =
                Some(summary.linked_to_edit_attribution);
        }
    }

    let cloud = ctx.cloud().await;
    if let Ok(scorecard) = cloud.get_impact_fix_scorecard().await {
        proof.accepted_fixes_last30 = Some(scorecard.last30.accepted);
        proof.total_fixes_last30 = Some(scorecard.last30.total);
        let saved_minutes =
            crate::commands::impact_payload::saved_review_minutes_for_scorecard(&scorecard);
        proof.saved_review_time =
            crate::commands::impact_payload::saved_review_time_label(saved_minutes);
    }

    proof
}

fn mcp_support_bundle_subsection(
    s: &mut String,
    snapshot: &mcp_install::McpStatusSnapshot,
    value_proof: &McpValueProof,
) {
    sw!(s, "\n### MCP support bundle\n");
    sw!(
        s,
        "_Copy this block when an editor/agent reports `Transport closed`._\n"
    );
    sw!(s, "```text");
    sw!(s, "binary: {}", one_line(&snapshot.binary));
    if let Some(probe) = &snapshot.runtime_probe {
        sw!(
            s,
            "runtime: {} | {}",
            doctor_runtime_probe_state_label(probe.state),
            one_line(&probe.detail)
        );
        let tools = if probe.tool_names.is_empty() {
            "not listed".to_owned()
        } else {
            probe.tool_names.join(", ")
        };
        let count = probe
            .tool_count
            .map_or_else(|| "unknown".to_owned(), |count| count.to_string());
        sw!(s, "tools: {count} | {tools}");
        if probe.tool_call_completed {
            let tool_name = probe.tool_call_name.as_deref().unwrap_or("tools/call");
            let injected = probe.tool_call_rules_injected.map_or_else(
                || "unknown injected".to_owned(),
                |count| format!("{count} injected"),
            );
            let indexed = probe.tool_call_rules_indexed.map_or_else(
                || "unknown indexed".to_owned(),
                |count| format!("{count} indexed"),
            );
            let top = probe
                .tool_call_top_result
                .as_deref()
                .map_or("none".to_owned(), one_line);
            sw!(
                s,
                "tool call: {tool_name} | {injected} | {indexed} | top={top}"
            );
        }
    } else {
        sw!(s, "runtime: not checked");
    }
    let record_path = snapshot
        .canonical_record
        .path
        .as_deref()
        .unwrap_or("not recorded");
    sw!(
        s,
        "canonical record: {} | {}",
        doctor_canonical_record_state_label(snapshot.canonical_record.state),
        one_line(record_path)
    );

    let installed = report_installed_clients(snapshot);
    sw!(s, "installed clients: {}", report_list_or_none(&installed));
    sw!(s, "value summary:");
    for line in mcp_value_proof_lines(value_proof) {
        sw!(s, "- {line}");
    }
    if let Some(diagnosis) = &snapshot.diagnosis {
        sw!(
            s,
            "affected clients: {}",
            report_list_or_none(&diagnosis.affected_clients)
        );
        sw!(s, "next step: {}", one_line(&diagnosis.next_step));
        sw!(s, "actions:");
        for action in &diagnosis.actions {
            sw!(s, "- {}", one_line(action));
        }
    } else {
        sw!(s, "affected clients: unknown");
        sw!(s, "next step: run difflore agents status --json");
    }

    let surfaces = report_installed_surfaces(snapshot);
    if !surfaces.is_empty() {
        sw!(s, "installed surfaces:");
        for surface in surfaces {
            sw!(s, "- {surface}");
        }
    }
    sw!(s, "```\n");
}

fn mcp_value_proof_lines(proof: &McpValueProof) -> Vec<String> {
    let mut lines = Vec::new();
    let memory = match (proof.active_rules, proof.imported_prs) {
        (Some(rules), Some(prs)) => {
            format!(
                "synced memory: {rules} active rule{} ready for recall | {prs} imported PR{}",
                plural(rules),
                plural(prs)
            )
        }
        (Some(rules), None) => format!(
            "synced memory: {rules} active rule{} ready for recall",
            plural(rules)
        ),
        (None, Some(prs)) => format!("synced memory: {prs} imported PR{}", plural(prs)),
        (None, None) => "synced memory: unavailable in this report".to_owned(),
    };
    lines.push(memory);

    let tools = proof.mcp_tools.map_or_else(
        || "unknown MCP tools".to_owned(),
        |n| format!("{n} MCP tools"),
    );
    lines.push(format!(
        "agent reach: {} installed client{} | {tools} served by the runtime",
        proof.installed_clients,
        plural_usize(proof.installed_clients),
    ));

    let local_accepted = local_accepted_proof_total(proof);
    if local_accepted > 0
        && let Some(total) = proof.local_total_outcomes_last30
    {
        let saved = proof
            .local_saved_review_time
            .as_deref()
            .unwrap_or("saved review time unavailable");
        let source_note = local_accepted_source_note(proof);
        let recall_note = local_accepted_recall_note(proof);
        lines.push(format!(
            "local accepted activity: {local_accepted} accepted edit{}{} in the last 30d ({total} local outcome{}){recall_note} | {saved}",
            plural(local_accepted),
            source_note,
            plural(total),
        ));
    }

    if let (Some(accepted), Some(total)) = (proof.accepted_fixes_last30, proof.total_fixes_last30) {
        let saved = proof
            .saved_review_time
            .as_deref()
            .unwrap_or("saved review time unavailable");
        lines.push(format!(
            "remote Impact activity: {accepted}/{total} accepted edits in the last 30d | {saved}"
        ));
    } else if local_accepted > 0 {
        lines.push("remote Impact activity: unavailable; local activity captured above".to_owned());
    } else {
        lines.push("remote Impact activity: unavailable in this report; run `difflore status` for local activity or `difflore cloud impact` after login".to_owned());
    }

    lines
}

fn local_accepted_proof_total(proof: &McpValueProof) -> i64 {
    proof.local_accepted_edits_last30.unwrap_or(0)
        + proof.local_accepted_hook_outcomes_last30.unwrap_or(0)
}

fn local_accepted_source_note(proof: &McpValueProof) -> String {
    let signed = proof.local_accepted_edits_last30.unwrap_or(0);
    let hook = proof.local_accepted_hook_outcomes_last30.unwrap_or(0);
    if signed > 0 && hook > 0 {
        format!(
            " ({signed} signed local fix{} + {hook} agent/hook outcome{})",
            if signed == 1 { "" } else { "es" },
            plural(hook),
        )
    } else if hook > 0 {
        format!(" ({hook} agent/hook outcome{})", plural(hook))
    } else {
        String::new()
    }
}

fn local_accepted_recall_note(proof: &McpValueProof) -> String {
    let linked = proof
        .local_accepted_outcomes_linked_to_prior_recall_last30
        .unwrap_or(0);
    if linked <= 0 {
        return String::new();
    }
    let breakdown = format_recall_edit_proof_breakdown(
        proof
            .local_accepted_outcomes_linked_to_rule_recall_last30
            .unwrap_or(0),
        proof
            .local_accepted_outcomes_linked_to_mcp_rule_serve_last30
            .unwrap_or(0),
        proof
            .local_accepted_outcomes_linked_to_edit_attribution_last30
            .unwrap_or(0),
    );
    format!(" | {linked} after prior memory recall{breakdown} within 7d")
}

const fn plural(n: i64) -> &'static str {
    if n == 1 { "" } else { "s" }
}

const fn plural_usize(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn report_installed_clients(snapshot: &mcp_install::McpStatusSnapshot) -> Vec<String> {
    snapshot
        .clients
        .iter()
        .filter(|client| {
            client.detected && matches!(client.state, mcp_install::InstallState::Installed)
        })
        .map(|client| client.name.to_owned())
        .collect()
}

fn report_installed_surfaces(snapshot: &mcp_install::McpStatusSnapshot) -> Vec<String> {
    snapshot
        .clients
        .iter()
        .flat_map(|client| &client.surfaces)
        .filter(|surface| {
            surface.detected && matches!(surface.state, mcp_install::InstallState::Installed)
        })
        .map(|surface| {
            let detail = surface.detail.as_deref().unwrap_or("installed");
            format!("{}: {}", surface.name, one_line(detail))
        })
        .collect()
}

fn report_list_or_none(values: &[String]) -> String {
    if values.is_empty() {
        "none".to_owned()
    } else {
        values.join(", ")
    }
}

fn one_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn mcp_per_tool_subsection(s: &mut String, snapshot: &mcp_install::McpStatusSnapshot) {
    sw!(s, "\n## ✓ MCP raw surfaces\n");
    for agent in &snapshot.agents {
        if !agent.detected && matches!(agent.state, mcp_install::InstallState::NotInstalled) {
            continue;
        }
        sw!(
            s,
            "- {} {}: {}",
            doctor_install_mark(agent.detected, agent.state),
            agent.name,
            doctor_install_state_label(agent.detected, agent.state)
        );
        if let Some(detail) = &agent.detail {
            sw!(s, "  detail: {detail}");
        }
    }
}

const fn doctor_runtime_probe_state_label(state: mcp_install::RuntimeProbeState) -> &'static str {
    match state {
        mcp_install::RuntimeProbeState::Ok => "ok",
        mcp_install::RuntimeProbeState::Failed => "failed",
        mcp_install::RuntimeProbeState::Timeout => "timeout",
    }
}

const fn doctor_runtime_probe_mark(state: mcp_install::RuntimeProbeState) -> &'static str {
    match state {
        mcp_install::RuntimeProbeState::Ok => "✓",
        mcp_install::RuntimeProbeState::Failed => "✗",
        mcp_install::RuntimeProbeState::Timeout => "⚠",
    }
}

pub(super) fn settings_section(s: &mut String) {
    sw!(s, "\n## ⚠ Settings (redacted)\n");
    let Ok(config) = difflore_core::infra::paths::config_file() else {
        return;
    };
    if config.exists() {
        match std::fs::read_to_string(&config) {
            Ok(raw) => {
                let redacted = redact_secrets(&raw);
                sw!(s, "```toml");
                sw!(s, "{}", redacted.trim());
                sw!(s, "```");
            }
            Err(e) => {
                sw!(s, "- (failed to read: {e})");
            }
        }
    } else {
        sw!(s, "- `~/.difflore/config.toml` not present");
    }
}

pub(super) fn footer_section(s: &mut String) {
    sw!(s, "\n---\n");
    let issues = difflore_core::cloud::endpoints::github_issues_url();
    sw!(
        s,
        "_Paste into a GitHub issue at {issues} —\
         API keys are redacted, but scan once more before sharing._"
    );

    let repo = difflore_core::cloud::endpoints::github_repo_url();
    sw!(
        s,
        "\n_If DiffLore helped, a star at {repo} keeps the project visible — thanks!_"
    );
}

/// `## ✓ Embedding` section. Surfaces the active embedder source (cloud-managed,
/// BYOK, or local-lexical) for triage; keys are never logged. Classification
/// delegates to `probe_active_embedder`, mirroring the resolver `get_embedder`
/// uses.
pub(super) async fn embedding_section(ctx: &crate::runtime::CommandContext, s: &mut String) {
    use difflore_core::context::embedding::ActiveEmbedderKind;

    sw!(s, "\n## ✓ Embedding\n");
    let activity = embedding_activity_summary();
    let kind = difflore_core::context::embedding::probe_active_embedder().await;
    match &kind {
        ActiveEmbedderKind::Byok { provider_host, .. } => {
            sw!(s, "- mode: `byok`");
            sw!(s, "- provider host: `{provider_host}` (key redacted)");
            sw!(s, "- switch modes: run `difflore embeddings setup`");
            activity.write_degradation_line(s);
        }
        ActiveEmbedderKind::Cloud { .. } => {
            let plan = difflore_core::cloud::sync::fetch_cloud_status(ctx.cloud().await)
                .await
                .plan
                .unwrap_or_else(|| "free".into());
            sw!(s, "- mode: `cloud-managed`");
            if plan.eq_ignore_ascii_case("free") {
                let cap = activity.cap_detail().unwrap_or_else(|| {
                    "managed embedding cap (usage count appears after a cap event)".to_owned()
                });
                sw!(s, "- plan: `{plan}` · {cap}");
                sw!(
                    s,
                    "- over-cap behaviour: capped managed embeds fall back to local-lexical; previously-embedded rules retained"
                );
            } else {
                sw!(s, "- plan: `{plan}` · unlimited");
            }
            sw!(
                s,
                "- switch modes: run `difflore embeddings setup` to bring your own OpenAI-compatible key"
            );
            activity.write_degradation_line(s);
        }
        ActiveEmbedderKind::Sha1 => {
            sw!(
                s,
                "- mode: `local-lexical` (offline hybrid: local hash + FTS5 BM25)"
            );
            sw!(
                s,
                "- self-recall: measured in the Self-recall sanity check below; local lexical is deterministic but less semantic than cloud-managed or BYOK embeddings"
            );
            sw!(
                s,
                "- switch modes: `difflore cloud login` for cloud-managed, \
                 or `difflore embeddings setup` to bring your own OpenAI-compatible key"
            );
        }
    }
}

#[derive(Default)]
struct EmbeddingActivitySummary {
    fallback_count: usize,
    cap_hits: usize,
    latest_reason: Option<String>,
    latest_cap: Option<(u32, u32)>,
}

impl EmbeddingActivitySummary {
    const fn has_degradation(&self) -> bool {
        self.fallback_count > 0 || self.cap_hits > 0
    }

    fn cap_detail(&self) -> Option<String> {
        self.latest_cap
            .map(|(cap, used)| format!("managed embedding cap observed at {used}/{cap}"))
    }

    fn write_degradation_line(&self, s: &mut String) {
        if !self.has_degradation() {
            return;
        }
        let mut parts = Vec::new();
        if self.fallback_count > 0 {
            let reason = self
                .latest_reason
                .as_deref()
                .map_or_else(String::new, |reason| format!(" · latest: {reason}"));
            parts.push(format!("{} fallback{}", self.fallback_count, reason));
        }
        if self.cap_hits > 0 {
            let cap = self
                .cap_detail()
                .unwrap_or_else(|| "managed embedding cap reached".to_owned());
            parts.push(format!("{} cap hit ({cap})", self.cap_hits));
        }
        sw!(
            s,
            "- recent degradation: {} in the last 200 activity events; run `difflore embeddings setup` if SHA1 fallback persists",
            parts.join(" · ")
        );
    }
}

fn embedding_activity_summary() -> EmbeddingActivitySummary {
    let mut summary = EmbeddingActivitySummary::default();
    for event in difflore_core::observability::activity_stream::tail(200) {
        match event.payload {
            difflore_core::observability::activity_stream::ActivityPayload::EmbeddingFallback { reason } => {
                summary.fallback_count += 1;
                if summary.latest_reason.is_none() {
                    summary.latest_reason = Some(reason);
                }
            }
            difflore_core::observability::activity_stream::ActivityPayload::EmbedCapReached { cap, used } => {
                summary.cap_hits += 1;
                summary.latest_cap.get_or_insert((cap, used));
            }
            _ => {}
        }
    }
    summary
}

/// Walk the parsed TOML and redact values whose key matches a secret-shaped
/// pattern. Falls back to passthrough on parse failure rather than crashing.
pub(super) fn redact_secrets(raw: &str) -> String {
    let Ok(mut value) = raw.parse::<toml::Value>() else {
        return raw.to_owned();
    };
    walk_redact(&mut value);
    toml::to_string(&value).unwrap_or_else(|_| raw.to_owned())
}

fn key_is_secret(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    k.contains("token") || k.contains("secret") || k.contains("api_key") || k.contains("password")
}

fn walk_redact(value: &mut toml::Value) {
    match value {
        toml::Value::Table(t) => {
            for (k, v) in t.iter_mut() {
                if key_is_secret(k) {
                    redact_value(v);
                } else {
                    walk_redact(v);
                }
            }
        }
        toml::Value::Array(arr) => {
            for v in arr.iter_mut() {
                walk_redact(v);
            }
        }
        _ => {}
    }
}

fn redact_value(value: &mut toml::Value) {
    let len = match value {
        toml::Value::String(s) => s.len(),
        toml::Value::Integer(i) => i.to_string().len(),
        toml::Value::Float(f) => f.to_string().len(),
        toml::Value::Boolean(_)
        | toml::Value::Datetime(_)
        | toml::Value::Array(_)
        | toml::Value::Table(_) => 0,
    };
    *value = toml::Value::String(format!("<redacted:len={len}>"));
}

#[cfg(test)]
mod tests {
    use super::{
        McpValueProof, mcp_support_bundle_subsection, mcp_value_proof_lines, redact_secrets,
    };
    use crate::mcp_install::{
        CanonicalRecordState, CanonicalRecordStatus, InstallState, McpClientStatus,
        McpRuntimeProbe, McpStatusDiagnosis, McpStatusSnapshot, RuntimeProbeState, TargetStatus,
    };

    #[test]
    fn redacts_top_level_token() {
        let input = "token = \"abc123xyz\"\nname = \"alice\"\n";
        let out = redact_secrets(input);
        assert!(out.contains("redacted:len=9"), "out = {out}");
        assert!(out.contains("alice"));
    }

    #[test]
    fn redacts_nested_api_key() {
        let input = "[provider]\napi_key = \"sk-1234567890\"\nmodel = \"gpt-4\"\n";
        let out = redact_secrets(input);
        assert!(out.contains("redacted"));
        assert!(out.contains("gpt-4"));
    }

    #[test]
    fn handles_multiline_string_without_breaking() {
        let input =
            "[doc]\nbody = \"\"\"\nline one with token in it\nline two\n\"\"\"\nname = \"safe\"\n";
        let out = redact_secrets(input);
        assert!(out.contains("line one"), "out = {out}");
        assert!(out.contains("safe"));
    }

    #[test]
    fn preserves_array_of_tables_header() {
        let input = "[[providers]]\nname = \"openai\"\nsecret = \"hush\"\n";
        let out = redact_secrets(input);
        assert!(out.contains("openai"));
        assert!(out.contains("redacted"));
        assert!(!out.contains("hush"));
    }

    #[test]
    fn parse_failure_falls_through() {
        let input = "this is not = = toml";
        let out = redact_secrets(input);
        assert_eq!(out, input);
    }

    #[test]
    fn mcp_support_bundle_lists_runtime_proof_and_client_actions() {
        let surface = TargetStatus {
            name: "Cursor",
            detected: true,
            state: InstallState::Installed,
            detail: Some("~/.cursor/mcp.json".to_owned()),
        };
        let snapshot = McpStatusSnapshot {
            binary: "C:/Users/me/bin/difflore.exe".to_owned(),
            canonical_record: CanonicalRecordStatus {
                path: Some("~/.difflore/mcp.json".to_owned()),
                state: CanonicalRecordState::Present,
                detail: None,
                recorded_targets: vec!["Cursor".to_owned()],
                actual_targets: vec!["Cursor".to_owned()],
            },
            runtime_probe: Some(McpRuntimeProbe {
                state: RuntimeProbeState::Ok,
                detail: "stdio self-check served initialize + tools/list".to_owned(),
                initialized: true,
                tools_listed: true,
                tool_call_completed: true,
                tool_call_name: Some("search_rules".to_owned()),
                tool_call_rules_injected: Some(1),
                tool_call_rules_indexed: Some(3),
                tool_call_top_result: Some("Review memory probe rule".to_owned()),
                tool_count: Some(2),
                tool_names: vec!["search_rules".to_owned(), "get_rules".to_owned()],
            }),
            diagnosis: Some(McpStatusDiagnosis {
                summary: "healthy".to_owned(),
                next_step: "Restart/reload installed client(s) that still report `Transport closed`: Cursor.".to_owned(),
                affected_clients: Vec::new(),
                actions: vec![
                    "Cursor: run `Developer: Reload Window` from the command palette, or restart Cursor.".to_owned(),
                    "If the error persists, compare that client's MCP entry with `difflore agents status --json`.".to_owned(),
                ],
            }),
            clients: vec![McpClientStatus {
                name: "Cursor",
                detected: true,
                state: InstallState::Installed,
                detail: Some("1/1 surface(s) installed".to_owned()),
                surfaces: vec![surface],
            }],
            agents: Vec::new(),
        };
        let mut out = String::new();
        let proof = McpValueProof {
            active_rules: Some(3882),
            imported_prs: Some(169),
            installed_clients: 1,
            mcp_tools: Some(2),
            local_accepted_edits_last30: Some(3),
            local_accepted_hook_outcomes_last30: Some(1),
            local_accepted_outcomes_linked_to_prior_recall_last30: Some(2),
            local_accepted_outcomes_linked_to_rule_recall_last30: Some(1),
            local_accepted_outcomes_linked_to_mcp_rule_serve_last30: Some(1),
            local_accepted_outcomes_linked_to_edit_attribution_last30: Some(0),
            local_total_outcomes_last30: Some(4),
            local_saved_review_time: Some("16m review time saved".to_owned()),
            accepted_fixes_last30: Some(46),
            total_fixes_last30: Some(46),
            saved_review_time: Some("3h 4m review time saved".to_owned()),
        };
        mcp_support_bundle_subsection(&mut out, &snapshot, &proof);

        assert!(out.contains("### MCP support bundle"));
        assert!(out.contains("runtime: ok | stdio self-check served initialize + tools/list"));
        assert!(out.contains("tools: 2 | search_rules, get_rules"));
        assert!(out.contains("tool call: search_rules | 1 injected | 3 indexed"));
        assert!(out.contains("top=Review memory probe rule"));
        assert!(out.contains("installed clients: Cursor"));
        assert!(out.contains("synced memory: 3882 active rules ready for recall"));
        assert!(out.contains("agent reach: 1 installed client | 2 MCP tools served"));
        assert!(out.contains("local accepted activity: 4 accepted edits"));
        assert!(out.contains("(3 signed local fixes + 1 agent/hook outcome)"));
        assert!(
            out.contains("2 after prior memory recall (1 rule recall + 1 agent recall) within 7d")
        );
        assert!(out.contains("16m review time saved"));
        assert!(out.contains("remote Impact activity: 46/46 accepted edits in the last 30d"));
        assert!(out.contains("affected clients: none"));
        assert!(out.contains("Cursor: run `Developer: Reload Window`"));
        assert!(out.contains("installed surfaces:"));
        assert!(out.contains("Cursor: ~/.cursor/mcp.json"));
    }

    #[test]
    fn value_proof_lines_degrade_without_cloud_impact() {
        let proof = McpValueProof {
            active_rules: Some(1),
            imported_prs: Some(2),
            installed_clients: 3,
            mcp_tools: Some(7),
            local_accepted_edits_last30: Some(2),
            local_accepted_hook_outcomes_last30: None,
            local_accepted_outcomes_linked_to_prior_recall_last30: None,
            local_accepted_outcomes_linked_to_rule_recall_last30: None,
            local_accepted_outcomes_linked_to_mcp_rule_serve_last30: None,
            local_accepted_outcomes_linked_to_edit_attribution_last30: None,
            local_total_outcomes_last30: Some(3),
            local_saved_review_time: Some("8m review time saved".to_owned()),
            accepted_fixes_last30: None,
            total_fixes_last30: None,
            saved_review_time: None,
        };
        let lines = mcp_value_proof_lines(&proof);

        assert!(lines[0].contains("1 active rule ready for recall"));
        assert!(lines[0].contains("2 imported PRs"));
        assert!(lines[1].contains("3 installed clients | 7 MCP tools"));
        assert!(lines[2].contains("local accepted activity: 2 accepted edits"));
        assert!(lines[2].contains("8m review time saved"));
        assert_eq!(
            lines[3],
            "remote Impact activity: unavailable; local activity captured above"
        );
    }

    #[test]
    fn value_proof_lines_count_hook_only_accepted_outcomes() {
        let proof = McpValueProof {
            active_rules: Some(1),
            imported_prs: None,
            installed_clients: 1,
            mcp_tools: Some(7),
            local_accepted_edits_last30: Some(0),
            local_accepted_hook_outcomes_last30: Some(2),
            local_accepted_outcomes_linked_to_prior_recall_last30: Some(2),
            local_accepted_outcomes_linked_to_rule_recall_last30: Some(0),
            local_accepted_outcomes_linked_to_mcp_rule_serve_last30: Some(1),
            local_accepted_outcomes_linked_to_edit_attribution_last30: Some(1),
            local_total_outcomes_last30: Some(2),
            local_saved_review_time: Some("8m review time saved".to_owned()),
            accepted_fixes_last30: None,
            total_fixes_last30: None,
            saved_review_time: None,
        };
        let lines = mcp_value_proof_lines(&proof);

        assert!(lines[2].contains("local accepted activity: 2 accepted edits"));
        assert!(lines[2].contains("(2 agent/hook outcomes)"));
        assert!(
            lines[2].contains(
                "2 after prior memory recall (1 agent recall + 1 accepted edit) within 7d"
            )
        );
        assert!(lines[2].contains("8m review time saved"));
        assert_eq!(
            lines[3],
            "remote Impact activity: unavailable; local activity captured above"
        );
    }
}
