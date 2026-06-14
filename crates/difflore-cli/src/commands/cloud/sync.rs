use crate::runtime::CommandContext;
use crate::style;
use crate::support::util::{exit_code, exit_err};

const MAX_OBSERVATION_SYNC_FLUSHES: usize = 10;
const MAX_CLOUD_OUTBOX_SYNC_ITEMS: usize = 64;
const MAX_MCP_QUERY_OUTBOX_SYNC_ITEMS: usize = 0;
const VALUE_OUTBOX_SYNC_PRIORITY_KINDS: &[(&str, Option<usize>)] = &[
    (difflore_core::cloud::outbox::kind::ACCEPTED_EDIT, None),
    (difflore_core::cloud::outbox::kind::IMPORTED_REVIEWS, None),
    (difflore_core::cloud::outbox::kind::REVIEW_METRICS, None),
    (difflore_core::cloud::outbox::kind::TRAJECTORY, None),
    // Raw MCP query telemetry can be high-volume and server-side
    // materialization is intentionally best-effort. Foreground cloud sync
    // must not wait on this backlog before rules/settings/team memory return.
    (
        difflore_core::cloud::outbox::kind::MCP_QUERY,
        Some(MAX_MCP_QUERY_OUTBOX_SYNC_ITEMS),
    ),
];

type AcceptedEditAttributionSummary = difflore_core::cloud::outbox::AcceptedEditAttributionSummary;

/// CLI-side args bundle for `difflore cloud sync` (`--pull`, `--push`,
/// `--dry-run`, `--json`).
pub(crate) struct SyncArgs {
    pub pull: bool,
    pub push: bool,
    pub dry_run: bool,
    pub json: bool,
}

impl From<crate::cli::SyncCliArgs> for SyncArgs {
    fn from(args: crate::cli::SyncCliArgs) -> Self {
        Self {
            pull: args.pull,
            push: args.push,
            dry_run: args.dry_run,
            json: args.json,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum SyncDirection {
    Pull,
    Push,
    Both,
}

impl SyncDirection {
    const fn from_flags(pull: bool, push: bool) -> Self {
        match (pull, push) {
            (true, false) => Self::Pull,
            (false, true) => Self::Push,
            _ => Self::Both,
        }
    }

    const fn do_pull(self) -> bool {
        matches!(self, Self::Pull | Self::Both)
    }

    const fn do_push(self) -> bool {
        matches!(self, Self::Push | Self::Both)
    }
}

/// Aggregated counters surfaced by `handle_sync` to the output formatters.
struct SyncOutcome {
    created: usize,
    updated: usize,
    deleted: usize,
    team_count: i32,
    team_synced: usize,
    settings_pull_applied: usize,
    providers_added: usize,
    observations_attempted: usize,
    observations_uploaded: usize,
    observations_queued: usize,
    telemetry_attempted: usize,
    telemetry_uploaded: usize,
    accepted_edit_attribution: AcceptedEditAttributionSummary,
}

pub(crate) async fn handle_sync(ctx: &CommandContext, args: SyncArgs) {
    let SyncArgs {
        pull,
        push,
        dry_run,
        json,
    } = args;
    let direction = SyncDirection::from_flags(pull, push);
    let client = ctx.cloud().await;
    if !client.is_logged_in() {
        emit_not_logged_in(json);
    }

    if dry_run {
        emit_dry_run(json, direction);
        return;
    }

    let db = &ctx.db;
    let mut spinner = sync_spinner(json, "Uploading local memory activity");
    let (observations_attempted, observations_uploaded, observations_queued) =
        run_observations_phase(client).await;
    sync_spinner_tick(spinner.as_ref());

    sync_spinner_set_message(&mut spinner, "Uploading local activity");
    let (telemetry_attempted, telemetry_uploaded, accepted_edit_attribution) =
        run_cloud_outbox_phase(db, client).await;
    sync_spinner_tick(spinner.as_ref());

    let local_skills = match difflore_core::skills::list(db).await {
        Ok(skills) => skills,
        Err(e) => exit_err(&format!("Failed to list local rules: {e}")),
    };

    let excluded_ids = prepare_excluded_ids(db, &local_skills).await;

    sync_spinner_set_message(&mut spinner, "Syncing rules from cloud");

    let synced_local = if direction.do_pull() {
        match difflore_core::cloud::sync::sync_skills_filtered(client, &local_skills, &excluded_ids)
            .await
        {
            Ok(Some(result)) => result,
            Ok(None) => difflore_core::cloud::sync::SyncResult {
                created: vec![],
                updated: vec![],
                deleted: vec![],
            },
            Err(e) => {
                sync_spinner_finish_err(spinner.take(), "Cloud sync failed");
                exit_err(&format_cloud_err(
                    "Failed to sync local rules",
                    &e.to_string(),
                ));
            }
        }
    } else {
        difflore_core::cloud::sync::SyncResult {
            created: vec![],
            updated: vec![],
            deleted: vec![],
        }
    };
    sync_spinner_tick(spinner.as_ref());

    // `--push` skips applying the cloud's pull-side response locally.
    // Skills sync is server-driven (the cloud computes the diff from
    // localHashes + serverHashes), so we always send hashes; we only
    // gate whether the server's "you're missing X / you have stale Y"
    // response gets written back to the local skills table.
    let apply_pull = direction.do_pull();
    if apply_pull && let Err(e) = difflore_core::skills::apply_sync_result(db, &synced_local).await
    {
        eprintln!(
            "{} Failed to apply sync changes: {e}",
            style::amber(style::sym::WARN),
        );
    }

    sync_spinner_set_message(&mut spinner, "Syncing team rules");
    let (team_count, team_synced) = if direction.do_pull() {
        match difflore_core::cloud::sync::sync_team_skills(client).await {
            Ok(team) => {
                if let Err(e) = difflore_core::skills::apply_sync_result(db, &team.synced).await {
                    eprintln!(
                        "{} Failed to apply team rule sync changes: {e}",
                        style::amber(style::sym::WARN),
                    );
                }
                (team.visible_count, team.synced.created_count())
            }
            Err(e) => {
                sync_spinner_finish_err(spinner.take(), "Cloud sync failed");
                exit_err(&format_cloud_err(
                    "Failed to sync team rules",
                    &e.to_string(),
                ));
            }
        }
    } else {
        (0, 0)
    };
    sync_spinner_tick(spinner.as_ref());

    sync_spinner_set_message(&mut spinner, "Syncing settings");
    let settings_pull_applied = run_settings_phase(client, direction).await;
    sync_spinner_tick(spinner.as_ref());

    sync_spinner_set_message(&mut spinner, "Syncing providers");
    let providers_added = run_providers_phase(client, db, direction).await;
    sync_spinner_tick(spinner.as_ref());

    sync_spinner_finish_ok(spinner.take(), "Cloud sync completed.");

    let outcome = SyncOutcome {
        created: synced_local.created_count(),
        updated: synced_local.updated_count(),
        deleted: synced_local.deleted_count(),
        team_count,
        team_synced,
        settings_pull_applied,
        providers_added,
        observations_attempted,
        observations_uploaded,
        observations_queued,
        telemetry_attempted,
        telemetry_uploaded,
        accepted_edit_attribution,
    };

    if json {
        emit_summary_json(&outcome);
        return;
    }

    emit_summary_human(&outcome, db).await;
}

fn sync_spinner(json: bool, label: &str) -> Option<style::Spinner> {
    if json {
        None
    } else {
        Some(style::Spinner::new(label))
    }
}

fn sync_spinner_tick(spinner: Option<&style::Spinner>) {
    if let Some(spinner) = spinner {
        spinner.tick();
    }
}

fn sync_spinner_set_message(spinner: &mut Option<style::Spinner>, message: &str) {
    if let Some(spinner) = spinner {
        spinner.set_message(message);
    }
}

fn sync_spinner_finish_ok(spinner: Option<style::Spinner>, message: &str) {
    if let Some(spinner) = spinner {
        spinner.finish_ok(message);
    }
}

fn sync_spinner_finish_err(spinner: Option<style::Spinner>, message: &str) {
    if let Some(spinner) = spinner {
        spinner.finish_err(message);
    }
}

fn emit_not_logged_in(json: bool) -> ! {
    if json {
        let payload = serde_json::json!({
            "ok": false,
            "reason": "not_logged_in",
        });
        println!("{}", crate::support::util::json_compact_or(&payload, "{}"));
        exit_code(1);
    }
    exit_err(
        "not logged in.\n\n  > run `difflore cloud login` to sync source-backed team rules into local agents",
    );
}

fn emit_dry_run(json: bool, direction: SyncDirection) {
    if json {
        let payload = serde_json::json!({
            "ok": true,
            "dryRun": true,
            "pullOnly": matches!(direction, SyncDirection::Pull),
            "pushOnly": matches!(direction, SyncDirection::Push),
        });
        println!("{}", crate::support::util::json_compact_or(&payload, "{}"));
        return;
    }
    println!(
        "{} dry-run: would {} cloud (no changes written).",
        style::ok(style::sym::OK),
        match direction {
            SyncDirection::Pull => "pull from",
            SyncDirection::Push => "push to",
            SyncDirection::Both => "pull from + push to",
        },
    );
    println!();
    println!(
        "next: {}  {}",
        style::cmd("difflore cloud sync"),
        style::pewter("then `difflore recall --diff` to see what agents would receive"),
    );
}

async fn prepare_excluded_ids(
    db: &difflore_core::SqlitePool,
    local_skills: &[difflore_core::domain::models::SkillRecord],
) -> Vec<String> {
    // TODO(launch+1): wire candidate sync — see crates/difflore-core/src/cloud/sync.rs
    // (candidateRules cloud table not yet wired). Until then keep pending
    // rules out of `/rules/sync` so the cloud doesn't round-trip them as
    // missing-active rules.
    let pending_ids = match difflore_core::skills::list_candidate_ids(db).await {
        Ok(ids) => ids,
        Err(e) => exit_err(&format!(
            "Failed to load pending memory drafts (would risk syncing them as active): {e}"
        )),
    };
    let source_repos = match difflore_core::skills::list_source_repos(db).await {
        Ok(repos) => repos,
        Err(e) => exit_err(&format!(
            "Failed to load rule source-repo metadata (cannot safely compute exclusions): {e}"
        )),
    };

    let mut excluded_ids = pending_ids;
    for skill in local_skills {
        // `/rules/sync` is a cloud-managed pull protocol. Local rules
        // captured via remember_rule/manual/conversation may not exist in
        // cloud yet, and the server represents "not in my map" as
        // `deleted`. Never let a routine pull delete private local
        // memories. A future publish/upsert path should handle `--push`
        // explicitly instead of smuggling local rules through localHashes.
        if skill.source != "cloud" {
            excluded_ids.push(skill.id.clone());
            continue;
        }

        let has_source_repo = source_repos
            .get(&skill.id)
            .and_then(|repo| repo.as_deref())
            .is_some_and(|repo| !repo.trim().is_empty());
        let should_have_source_repo = skill.source == "cloud"
            && (skill.origin == "extracted"
                || skill
                    .tags
                    .iter()
                    .any(|tag| tag == "origin:review-extraction" || tag == "auto-from-accept"));
        if should_have_source_repo && !has_source_repo {
            excluded_ids.push(skill.id.clone());
        }
    }
    excluded_ids.sort();
    excluded_ids.dedup();
    excluded_ids
}

async fn run_settings_phase(
    client: &difflore_core::cloud::client::CloudClient,
    direction: SyncDirection,
) -> usize {
    let do_pull = direction.do_pull();
    let do_push = direction.do_push();
    let mut applied = 0usize;
    if do_pull {
        match difflore_core::cloud::sync::pull_settings(client).await {
            Ok(Some((cloud_settings, _updated_at))) => {
                if let Ok(merged_input) = serde_json::from_value::<
                    difflore_core::domain::models::AppSettingsRecord,
                >(cloud_settings.clone())
                    && difflore_core::infra::settings::update(merged_input)
                        .await
                        .is_ok()
                {
                    applied = cloud_settings.as_object().map_or(0, serde_json::Map::len);
                }
            }
            Ok(None) => {}
            Err(e) => eprintln!(
                "{} Settings pull failed: {e}",
                style::amber(style::sym::WARN),
            ),
        }
    }

    if do_push {
        let settings_value = match difflore_core::infra::settings::get().await {
            Ok(s) => match serde_json::to_value(&s) {
                Ok(v) => v,
                Err(e) => {
                    // Settings serialization shape mismatch — bail rather
                    // than silently push an empty body that overwrites
                    // cloud state with `{}`.
                    exit_err(&format!(
                        "unexpected settings shape: failed to serialize local settings ({e})"
                    ));
                }
            },
            Err(e) => {
                eprintln!(
                    "{} Settings push skipped: failed to read local settings: {e}",
                    style::amber(style::sym::WARN),
                );
                return applied;
            }
        };
        if let Err(e) = difflore_core::cloud::sync::sync_settings(client, &settings_value).await {
            eprintln!(
                "{} Settings push failed: {e}",
                style::amber(style::sym::WARN),
            );
        }
    }
    applied
}

async fn run_providers_phase(
    client: &difflore_core::cloud::client::CloudClient,
    db: &difflore_core::SqlitePool,
    direction: SyncDirection,
) -> usize {
    // Pull cloud list, add any missing metadata rows locally.
    // API keys are NOT synced (masked in cloud) — users must re-enter per device.
    let do_pull = direction.do_pull();
    let do_push = direction.do_push();
    let mut providers_added = 0usize;
    if do_pull {
        match difflore_core::cloud::sync::pull_providers(client).await {
            Ok(Some((cloud_providers, _updated_at))) => {
                providers_added += apply_cloud_providers(db, &cloud_providers).await;
            }
            Ok(None) => {}
            Err(e) => eprintln!(
                "{} Providers pull failed: {e}",
                style::amber(style::sym::WARN),
            ),
        }
    }

    if do_push {
        let provider_entries = match difflore_core::infra::providers::list(db).await {
            Ok(providers) => difflore_core::cloud::sync::build_provider_sync_entries(&providers),
            Err(_) => Vec::new(),
        };
        if let Err(e) = difflore_core::cloud::sync::sync_providers(client, &provider_entries).await
        {
            eprintln!(
                "{} Providers push failed: {e}",
                style::amber(style::sym::WARN),
            );
        }
    }
    providers_added
}

async fn run_observations_phase(
    client: &difflore_core::cloud::client::CloudClient,
) -> (usize, usize, usize) {
    let Ok(emitter) = difflore_core::cloud::observations::ObservationEmitter::open_default().await
    else {
        return (0, 0, 0);
    };
    let _ = emitter.retry_pending_uploads_now().await;

    let mut attempted = 0usize;
    let mut uploaded = 0usize;
    for _ in 0..MAX_OBSERVATION_SYNC_FLUSHES {
        match emitter.flush_to_cloud(client).await {
            Ok((batch_attempted, batch_uploaded)) => {
                if batch_attempted == 0 {
                    break;
                }
                attempted += batch_attempted;
                uploaded += batch_uploaded;
                if batch_uploaded < batch_attempted {
                    break;
                }
            }
            Err(e) => {
                eprintln!(
                    "{} Local memory activity upload skipped: {e}",
                    style::amber(style::sym::WARN),
                );
                break;
            }
        }
    }

    let queued = emitter
        .pending_upload_count()
        .await
        .unwrap_or_default()
        .max(0) as usize;
    (attempted, uploaded, queued)
}

async fn run_cloud_outbox_phase(
    db: &difflore_core::SqlitePool,
    client: &difflore_core::cloud::client::CloudClient,
) -> (usize, usize, AcceptedEditAttributionSummary) {
    let mut attempted = 0usize;
    let mut uploaded = 0usize;
    let mut accepted_edit_attribution = AcceptedEditAttributionSummary::default();
    let mut remaining = MAX_CLOUD_OUTBOX_SYNC_ITEMS;

    for (kind, per_kind_limit) in value_outbox_sync_priority_kinds() {
        if remaining == 0 {
            break;
        }
        let limit = per_kind_limit.unwrap_or(remaining).min(remaining);
        if limit == 0 {
            continue;
        }
        let queue = difflore_core::cloud::outbox::OutboxQueue::new(db.clone());
        match difflore_core::cloud::outbox::drain_outbox_kind_report(&queue, client, kind, limit)
            .await
        {
            Ok(report) => {
                attempted += report.attempted;
                uploaded += report.confirmed;
                accepted_edit_attribution.add(report.accepted_edit_attribution);
                remaining = remaining.saturating_sub(report.attempted);
            }
            Err(e) => {
                eprintln!(
                    "{} Local activity upload skipped: {e}",
                    style::amber(style::sym::WARN),
                );
                return (attempted, uploaded, accepted_edit_attribution);
            }
        }
    }

    (attempted, uploaded, accepted_edit_attribution)
}

const fn value_outbox_sync_priority_kinds() -> &'static [(&'static str, Option<usize>)] {
    VALUE_OUTBOX_SYNC_PRIORITY_KINDS
}

async fn apply_cloud_providers(
    db: &difflore_core::SqlitePool,
    cloud_providers: &serde_json::Value,
) -> usize {
    let mut added = 0usize;
    let Ok(local_providers) = difflore_core::infra::providers::list(db).await else {
        return 0;
    };
    let existing: std::collections::HashSet<(String, String)> = local_providers
        .into_iter()
        .map(|p| (p.name, p.base_url))
        .collect();

    let Some(list) = cloud_providers.as_array() else {
        return 0;
    };
    for item in list {
        let name = item
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let base_url = item
            .get("baseUrl")
            .or_else(|| item.get("base_url"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        if name.is_empty() || base_url.is_empty() {
            continue;
        }
        if existing.iter().any(|(n, b)| n == &name && b == &base_url) {
            continue;
        }
        let model_mapping = match parse_provider_model_mapping(item) {
            Ok(m) => m,
            Err(e) => {
                // The cloud's provider record carried a `modelMapping`
                // field we couldn't decode. Skip this provider row
                // rather than silently dropping the mapping (which would
                // import the provider with no aliases and cause `fix`
                // to dispatch against unmapped model names).
                eprintln!(
                    "{} skipping cloud provider {name:?}: unexpected modelMapping shape ({e})",
                    style::amber(style::sym::WARN),
                );
                continue;
            }
        };
        let input = difflore_core::domain::models::ProviderAddInput {
            name: name.clone(),
            base_url,
            model_mapping,
        };
        if difflore_core::infra::providers::add(db, input)
            .await
            .is_ok()
        {
            added += 1;
        }
    }
    added
}

/// Decode the `modelMapping` field from a cloud provider record.
/// `Ok(empty)` when the field is absent / null (genuinely optional per
/// the API contract). `Err` when present-but-malformed so the caller
/// can skip the row instead of silently dropping the alias map.
fn parse_provider_model_mapping(
    item: &serde_json::Value,
) -> Result<std::collections::HashMap<String, String>, String> {
    let raw = item
        .get("modelMapping")
        .or_else(|| item.get("model_mapping"));
    match raw {
        None => Ok(std::collections::HashMap::new()),
        Some(v) if v.is_null() => Ok(std::collections::HashMap::new()),
        Some(v) => serde_json::from_value::<std::collections::HashMap<String, String>>(v.clone())
            .map_err(|e| e.to_string()),
    }
}

fn sync_summary_payload(outcome: &SyncOutcome) -> serde_json::Value {
    serde_json::json!({
        "ok": true,
        "dryRun": false,
        "memory": {
            "created": outcome.created,
            "updated": outcome.updated,
            "deleted": outcome.deleted,
        },
        "team": {
            "visible": outcome.team_count,
            "synced": outcome.team_synced,
        },
        "settings": { "fieldsMerged": outcome.settings_pull_applied },
        "providers": { "added": outcome.providers_added },
        "observations": {
            "attempted": outcome.observations_attempted,
            "uploaded": outcome.observations_uploaded,
            "queued": outcome.observations_queued,
        },
        "telemetryOutbox": {
            "attempted": outcome.telemetry_attempted,
            "uploaded": outcome.telemetry_uploaded,
            "queued": outcome.telemetry_attempted.saturating_sub(outcome.telemetry_uploaded),
        },
        "acceptedEditProof": {
            "uploaded": outcome.accepted_edit_attribution.uploaded,
            "launchGrade": outcome.accepted_edit_attribution.launch_grade,
            "missingTeamWorkspace": outcome.accepted_edit_attribution.missing_team_workspace,
            "missingRuleIds": outcome.accepted_edit_attribution.missing_rule_ids,
            "unlinkedRuleObservations": outcome.accepted_edit_attribution.unlinked_rule_observations,
            "warningCount": outcome.accepted_edit_attribution.warning_count(),
        },
    })
}

fn emit_summary_json(outcome: &SyncOutcome) {
    let payload = sync_summary_payload(outcome);
    println!("{}", crate::support::util::json_compact_or(&payload, "{}"));
}

fn proof_summary_line(
    observations_attempted: usize,
    observations_uploaded: usize,
    observations_queued: usize,
) -> String {
    if observations_attempted > 0 || observations_queued > 0 {
        format!(
            "  activity   {observations_uploaded} local event{} uploaded | {} pending",
            if observations_uploaded == 1 { "" } else { "s" },
            observations_queued,
        )
    } else {
        "  activity   current".to_owned()
    }
}

fn accepted_edit_proof_summary_line(summary: AcceptedEditAttributionSummary) -> Option<String> {
    if summary.uploaded == 0 {
        return None;
    }
    Some(format!(
        "  accepted edits  {} uploaded | {} pending",
        summary.launch_grade,
        summary.uploaded.saturating_sub(summary.launch_grade),
    ))
}

async fn emit_summary_human(outcome: &SyncOutcome, db: &difflore_core::SqlitePool) {
    let SyncOutcome {
        created,
        updated,
        deleted,
        team_count,
        team_synced,
        settings_pull_applied,
        providers_added,
        observations_attempted,
        observations_uploaded,
        observations_queued,
        telemetry_attempted,
        telemetry_uploaded,
        accepted_edit_attribution,
    } = *outcome;

    // Output contract: lead with a single status headline, then one row per
    // category in fixed order (memory / settings / providers / team), ending
    // with the `next: difflore recall --diff` bridge.
    println!("{} Sync complete", style::ok(style::sym::OK));
    println!("  memory     {created} created | {updated} updated | {deleted} deleted");
    if settings_pull_applied > 0 {
        println!("  settings   {settings_pull_applied} fields merged");
    } else {
        println!("  settings   pushed (no cloud updates to merge)");
    }
    if providers_added > 0 {
        println!(
            "  providers  {providers_added} added from cloud (API keys still need to be set locally)"
        );
    } else {
        println!("  providers  current");
    }
    if team_count > 0 {
        println!(
            "  team       {team_count} published memories visible | {team_synced} synced locally"
        );
    } else {
        println!("  team       0 published memories visible");
    }
    println!(
        "{}",
        proof_summary_line(
            observations_attempted,
            observations_uploaded,
            observations_queued
        )
    );
    if telemetry_attempted > 0 {
        println!(
            "  activity   {telemetry_uploaded} local event{} uploaded | {} pending",
            if telemetry_uploaded == 1 { "" } else { "s" },
            telemetry_attempted.saturating_sub(telemetry_uploaded),
        );
    }
    if let Some(line) = accepted_edit_proof_summary_line(accepted_edit_attribution) {
        println!("{line}");
    }
    if accepted_edit_attribution.warning_count() > 0 {
        println!(
            "  {} {} accepted edit upload{} need review: {} missing team workspace | {} missing recalled memory ids | {} missing linked memory activity",
            style::amber(style::sym::WARN),
            accepted_edit_attribution.warning_count(),
            if accepted_edit_attribution.warning_count() == 1 {
                ""
            } else {
                "s"
            },
            accepted_edit_attribution.missing_team_workspace,
            accepted_edit_attribution.missing_rule_ids,
            accepted_edit_attribution.unlinked_rule_observations,
        );
    }

    let cold_start = created == 0 && updated == 0 && team_count == 0 && team_synced == 0;
    if created > 0 {
        println!();
        println!(
            "  {} {} new rule{} pulled. {}",
            style::emerald(style::sym::TIP),
            created,
            if created == 1 { "" } else { "s" },
            style::pewter("Run `difflore recall --diff` to preview them."),
        );
    } else if cold_start {
        emit_cold_start_hint(db).await;
    }

    if !cold_start {
        println!();
        println!(
            "next: {}  {}",
            style::cmd("difflore recall --diff"),
            style::pewter("see what Claude/Codex/Cursor would receive"),
        );
    }
}

async fn emit_cold_start_hint(db: &difflore_core::SqlitePool) {
    // First-time / cold-start state: nothing came down, no team pool to
    // draw from. Distinguish two flavours so the user gets the right
    // next move:
    //   (a) zero imported reviews — they haven't started the pipeline
    //       yet, so the import-PR-reviews advice applies.
    //   (b) imported reviews exist — extraction is mid-flight on the
    //       cloud, so telling the user to "import PR reviews again"
    //       loops them. Tell them to wait + retry instead.
    let imported_review_count = difflore_core::review_store::list_by_source(
        db,
        difflore_core::review_store::ReviewSourceInput {
            source: "github".into(),
        },
    )
    .await
    .map_or(0, |v| v.len());
    println!();
    if imported_review_count > 0 {
        println!(
            "  {} {}; cloud is still extracting rules. Try {} in ~30s.",
            style::emerald(style::sym::TIP),
            cold_start_extracting_line(imported_review_count),
            style::cmd("difflore cloud sync"),
        );
        println!(
            "  {} watch progress: {}",
            style::pewter(style::sym::BULLET),
            style::cmd(&difflore_core::cloud::endpoints::web_link(
                "?from=cli-sync&intent=extracting"
            )),
        );
    } else {
        println!(
            "  {} {}",
            style::emerald(style::sym::TIP),
            style::pewter(
                "No new rules yet. Import PR reviews, extract team rules in cloud, then sync again."
            ),
        );
        println!(
            "  {} import: {}",
            style::pewter(style::sym::BULLET),
            style::cmd("difflore import-reviews --max-prs 50 --upload"),
        );
        println!(
            "  {} dashboard: {}",
            style::pewter(style::sym::BULLET),
            style::cmd(&difflore_core::cloud::endpoints::web_link(
                "?from=cli-sync&intent=memory-empty"
            )),
        );
    }
}

/// Frame the cold-start extracting hint so the count reads as global.
/// `imported_review_count` is the local DB total across every repo the user
/// has run `import-reviews` against, not just the current repo.
pub(crate) fn cold_start_extracting_line(imported_review_count: usize) -> String {
    let plural = if imported_review_count == 1 { "" } else { "s" };
    format!("{imported_review_count} review{plural} imported across all your repos")
}

/// Map raw cloud-API error strings into user-actionable hints, keeping the
/// raw error at the end so debug info isn't lost. Detection is substring-based
/// on purpose: the underlying error format crosses serde / reqwest / orpc
/// layers and stable parsing isn't worth the effort.
pub(crate) fn format_cloud_err(label: &str, e: &str) -> String {
    // Cloud-defined error codes (LLM gate, auth) are the most specific, so
    // match them first.
    if e.contains("LlmNotConfigured") || e.contains("llmNotConfigured") {
        return format!(
            "{label}: no LLM API key configured on the cloud side.\n  Set one at cloud `/settings` (BYOK) before querying corpora."
        );
    }
    if e.contains("not_logged_in") {
        return format!("{label}: not logged in to cloud.\n  Run `difflore cloud login` first.");
    }
    // Zod UUID rejection, usually a user passing a short id prefix. The raw
    // 400 dumps a regex pattern that isn't actionable, so translate to a clear
    // next-step before the generic "Input validation failed" case below.
    if e.contains("Input validation failed") && e.contains("\"format\":\"uuid\"") {
        return format!(
            "{label}: cloud rejected the id - short prefixes aren't supported here.\n  Pass the full UUID from DiffLore Cloud, then retry."
        );
    }
    // Generic HTTP / network / timeout / fallback all share shape with
    // the github-import path — delegate to the core helper.
    difflore_core::domain::origins::format_api_error(label, e)
}

#[cfg(test)]
mod tests {
    use super::{
        AcceptedEditAttributionSummary, MAX_MCP_QUERY_OUTBOX_SYNC_ITEMS, SyncOutcome,
        accepted_edit_proof_summary_line, cold_start_extracting_line, format_cloud_err,
        parse_provider_model_mapping, proof_summary_line, sync_summary_payload,
        value_outbox_sync_priority_kinds,
    };

    #[test]
    fn cold_start_extracting_line_disambiguates_per_repo_vs_global() {
        let one = cold_start_extracting_line(1);
        assert!(one.contains("1 review "), "msg: {one}");
        assert!(one.contains("across all your repos"), "msg: {one}");
        assert!(!one.contains("reviews "), "singular form leaked: {one}");

        let many = cold_start_extracting_line(155);
        assert!(many.contains("155 reviews "), "msg: {many}");
        assert!(many.contains("across all your repos"), "msg: {many}");
    }

    #[test]
    fn sync_summary_json_includes_local_proof_upload_counts() {
        let payload = sync_summary_payload(&SyncOutcome {
            created: 0,
            updated: 0,
            deleted: 0,
            team_count: 0,
            team_synced: 0,
            settings_pull_applied: 0,
            providers_added: 0,
            observations_attempted: 5,
            observations_uploaded: 3,
            observations_queued: 2,
            telemetry_attempted: 4,
            telemetry_uploaded: 4,
            accepted_edit_attribution: AcceptedEditAttributionSummary {
                uploaded: 3,
                launch_grade: 1,
                missing_team_workspace: 1,
                missing_rule_ids: 1,
                unlinked_rule_observations: 0,
            },
        });

        assert_eq!(payload["observations"]["attempted"], 5);
        assert_eq!(payload["observations"]["uploaded"], 3);
        assert_eq!(payload["observations"]["queued"], 2);
        assert_eq!(payload["telemetryOutbox"]["attempted"], 4);
        assert_eq!(payload["telemetryOutbox"]["uploaded"], 4);
        assert_eq!(payload["telemetryOutbox"]["queued"], 0);
        assert_eq!(payload["acceptedEditProof"]["uploaded"], 3);
        assert_eq!(payload["acceptedEditProof"]["launchGrade"], 1);
        assert_eq!(payload["acceptedEditProof"]["missingTeamWorkspace"], 1);
        assert_eq!(payload["acceptedEditProof"]["missingRuleIds"], 1);
        assert_eq!(payload["acceptedEditProof"]["warningCount"], 2);
    }

    #[test]
    fn accepted_edit_summary_line_distinguishes_uploaded_and_pending() {
        let line = accepted_edit_proof_summary_line(AcceptedEditAttributionSummary {
            uploaded: 4,
            launch_grade: 1,
            missing_team_workspace: 2,
            missing_rule_ids: 1,
            unlinked_rule_observations: 0,
        })
        .expect("uploaded accepted edits render a summary");

        assert!(line.contains("1 uploaded"), "{line}");
        assert!(line.contains("3 pending"), "{line}");

        assert!(
            accepted_edit_proof_summary_line(AcceptedEditAttributionSummary::default()).is_none()
        );
    }

    #[test]
    fn proof_summary_line_explains_uploaded_and_pending_events() {
        let uploaded = proof_summary_line(5, 3, 2);
        assert!(uploaded.contains("3 local events uploaded"), "{uploaded}");
        assert!(uploaded.contains("2 pending"), "{uploaded}");

        let singular = proof_summary_line(1, 1, 0);
        assert!(singular.contains("1 local event uploaded"), "{singular}");
        assert!(!singular.contains("local events uploaded"), "{singular}");

        let queued_only = proof_summary_line(0, 0, 7);
        assert!(
            queued_only.contains("0 local events uploaded"),
            "{queued_only}"
        );
        assert!(queued_only.contains("7 pending"), "{queued_only}");

        assert_eq!(proof_summary_line(0, 0, 0), "  activity   current");
    }

    #[test]
    fn telemetry_outbox_priority_keeps_value_proof_ahead_of_raw_observations() {
        let kinds = value_outbox_sync_priority_kinds();

        assert_eq!(
            kinds.first().map(|(kind, _)| *kind),
            Some(difflore_core::cloud::outbox::kind::ACCEPTED_EDIT)
        );
        assert!(
            kinds
                .iter()
                .any(|(kind, _)| *kind == difflore_core::cloud::outbox::kind::ACCEPTED_EDIT),
            "accepted-edit proof must drain before old raw observation backlog"
        );
        assert!(
            !kinds.iter().any(|(kind, _)| *kind == "fix_acceptance"),
            "fix_acceptance rows must not drain as current value-loop accepted-edit evidence"
        );
        assert!(
            !kinds
                .iter()
                .any(|(kind, _)| *kind == difflore_core::cloud::outbox::kind::OBSERVATION),
            "raw change observations are noisy and must not preempt value-proof telemetry"
        );
        assert_eq!(
            kinds
                .iter()
                .find(|(kind, _)| *kind == difflore_core::cloud::outbox::kind::MCP_QUERY)
                .and_then(|(_, limit)| *limit),
            Some(MAX_MCP_QUERY_OUTBOX_SYNC_ITEMS),
            "high-volume MCP telemetry must not make cloud sync drain the entire backlog"
        );
        assert_eq!(
            MAX_MCP_QUERY_OUTBOX_SYNC_ITEMS, 0,
            "foreground cloud sync should skip raw MCP telemetry backlog"
        );
    }

    #[test]
    fn parse_provider_model_mapping_accepts_missing_field() {
        // Field is genuinely optional per the API contract — providers
        // without aliases are valid, just rare.
        let item = serde_json::json!({"name": "openai", "baseUrl": "https://x"});
        let m = parse_provider_model_mapping(&item).expect("missing field is ok");
        assert!(m.is_empty());
    }

    #[test]
    fn parse_provider_model_mapping_accepts_null() {
        let item = serde_json::json!({"modelMapping": null});
        let m = parse_provider_model_mapping(&item).expect("null is ok");
        assert!(m.is_empty());
    }

    #[test]
    fn parse_provider_model_mapping_errors_on_wrong_shape() {
        // A malformed mapping must error (not silently import zero aliases),
        // so the caller can skip the row rather than dispatch unmapped models.
        let item = serde_json::json!({"modelMapping": ["not", "an", "object"]});
        let err = parse_provider_model_mapping(&item).expect_err("wrong shape must error");
        assert!(!err.is_empty(), "error must carry context");

        let nested_wrong = serde_json::json!({"modelMapping": {"gpt-4": 42}});
        assert!(
            parse_provider_model_mapping(&nested_wrong).is_err(),
            "non-string values must error"
        );
    }

    #[test]
    fn format_cloud_err_classifies_known_errors_and_falls_through_unknown() {
        let cases: &[(&str, &str)] = &[
            ("LlmNotConfigured: ...", "BYOK"),
            ("received not_logged_in from cloud", "difflore cloud login"),
            ("API error 401: token revoked", "session expired"),
            ("API error 403: plan_required", "rejected"),
            ("API error 429: too many requests", "rate-limited"),
            (
                r#"API error: API error 500: {"code":"INTERNAL_SERVER_ERROR"}"#,
                "server error",
            ),
            ("request failed: connection refused", "unreachable"),
            ("DNS error: no such host", "unreachable"),
            ("connection reset by peer", "unreachable"),
            ("Network is unreachable", "unreachable"),
            ("request timed out after 30s", "timed out"),
            // Windows winsock phrasings — the macOS/Linux "Connection
            // refused" string never appears; localhost-down on Windows
            // surfaces as "actively refused" / os error 10061. Lock these
            // in so a future cleanup of the regex chain doesn't drop them.
            (
                "error sending request for url (http://localhost:3017/api/rules/sync): error trying to connect: tcp connect error: No connection could be made because the target machine actively refused it. (os error 10061)",
                "unreachable",
            ),
            ("os error 10061", "unreachable"),
        ];
        for (raw, expect) in cases {
            let out = format_cloud_err("Action", raw);
            assert!(
                out.contains(expect),
                "want {expect:?} for {raw:?}, got: {out}"
            );
        }

        let raw_4xx_5xx: &[&str] = &[
            "API error 401: token revoked because team_seat_revoked",
            "API error 403: plan_required",
            "API error 429: retry-after=60",
            r#"API error 500: {"code":"INTERNAL_SERVER_ERROR"}"#,
        ];
        for raw in raw_4xx_5xx {
            let out = format_cloud_err("L", raw);
            assert!(
                out.contains(raw),
                "raw input {raw:?} missing from output: {out}"
            );
        }

        assert_eq!(
            format_cloud_err("Custom action", "totally novel error xyz123"),
            "Custom action: totally novel error xyz123"
        );
    }
}
