use std::collections::BTreeSet;

use colored::Colorize;

use super::{
    InstallState, Status, TargetOutcome,
    common::{
        MCP_SERVER_ARG, canonical_target_key, resolve_difflore_binary, write_install_manifest,
    },
    diagnosis::client_name_for_surface,
    manifest::{self, ManifestTarget},
    registry::{self, AGENTS, AgentSpec, BlockKind},
    snapshot::{collect_agent_statuses, installed_targets_from_agents},
};
use crate::style::{self, sym};

fn successful_outcome_names(outcomes: &[TargetOutcome]) -> Vec<&'static str> {
    outcomes
        .iter()
        .filter(|o| matches!(o.status, Status::Installed | Status::Updated))
        .map(|o| o.name)
        .collect()
}

pub(super) fn failed_outcome_names(outcomes: &[TargetOutcome]) -> Vec<&'static str> {
    outcomes
        .iter()
        .filter(|o| matches!(o.status, Status::Error(_)))
        .map(|o| o.name)
        .collect()
}

pub(super) fn outcome_client_names(outcomes: &[TargetOutcome]) -> Vec<String> {
    outcomes
        .iter()
        .filter(|o| matches!(o.status, Status::Installed | Status::Updated))
        .map(|o| client_name_for_surface(o.name).to_owned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn installed_surface_keys(bin: &str) -> BTreeSet<String> {
    collect_agent_statuses(bin)
        .into_iter()
        .filter(|agent| matches!(agent.state, InstallState::Installed))
        .map(|agent| canonical_target_key(agent.name))
        .collect()
}

pub(super) fn outcome_already_installed(
    outcome: &TargetOutcome,
    installed_surfaces: &BTreeSet<String>,
) -> bool {
    installed_surfaces.contains(&canonical_target_key(outcome.name))
        || skipped_because_already_installed(&outcome.status)
}

fn skipped_because_already_installed(status: &Status) -> bool {
    matches!(status, Status::Skipped(reason) if reason.contains("already installed"))
}

pub(super) const fn install_outcome_verb(
    status: &Status,
    dry_run: bool,
    already_installed: bool,
) -> &'static str {
    match status {
        Status::Installed | Status::Updated | Status::Skipped(_)
            if dry_run && already_installed =>
        {
            "already installed"
        }
        Status::Installed if dry_run => "would install",
        Status::Updated if dry_run => "would update",
        Status::Installed => "installed",
        Status::Updated => "updated",
        // `Removed` never arises on the install path; map it defensively.
        Status::Removed if dry_run => "would remove",
        Status::Removed => "removed",
        Status::Skipped(_) => "skipped",
        Status::Error(_) => "error",
    }
}

pub(super) const fn should_write_canonical_record(
    dry_run: bool,
    installed: &[&str],
    failed: &[&str],
) -> bool {
    !dry_run && !installed.is_empty() && failed.is_empty()
}

/// Install DiffLore into every detected agent. Returns `true` only when a
/// real (non-dry-run) run freshly installed or updated at least one surface
/// with zero failures — the signal the dispatch layer uses to follow up with
/// the post-install import offer.
pub fn install_all(dry_run: bool) -> bool {
    let cli_bin = match resolve_difflore_binary() {
        Ok(b) => b,
        Err(e) => crate::support::util::exit_err(&e),
    };
    let mcp_bin = cli_bin.clone();

    let install_message = if dry_run {
        "Checking DiffLore MCP install plan for every detected agent"
    } else {
        "Getting DiffLore ready for every detected agent"
    };
    let dry_tag = if dry_run {
        format!(" {}", style::amber("(dry-run; no changes)"))
    } else {
        String::new()
    };
    println!(
        "{} {}{dry_tag}",
        style::emerald(sym::TIP),
        style::pewter(install_message),
    );
    println!(
        "  {} {} {}",
        style::pewter("mcp command:"),
        style::emerald(&mcp_bin),
        style::emerald(MCP_SERVER_ARG)
    );
    println!();

    let outcomes = install_all_targets(&mcp_bin, &cli_bin, dry_run);
    let installed_surfaces = if dry_run {
        installed_surface_keys(&mcp_bin)
    } else {
        BTreeSet::new()
    };
    print_install_outcomes(&outcomes, dry_run, &installed_surfaces, &mcp_bin);

    let installed = successful_outcome_names(&outcomes);
    let has_detected_or_planned = if dry_run {
        outcomes.iter().any(|o| {
            matches!(o.status, Status::Installed | Status::Updated)
                || outcome_already_installed(o, &installed_surfaces)
        })
    } else {
        !installed.is_empty()
    };

    let failed = failed_outcome_names(&outcomes);
    if should_write_canonical_record(dry_run, &installed, &failed) {
        let agents = collect_agent_statuses(&mcp_bin);
        let current_installed = installed_targets_from_agents(&agents);
        let record_targets = if current_installed.is_empty() {
            installed.as_slice()
        } else {
            current_installed.as_slice()
        };
        // Build the v2 manifest: per-target config path, block_kind,
        // block_version, and a hash of the exact block we rendered, preserving
        // the prior `installed_at` for any re-installed target. The v1
        // `command`/`args`/`installed_targets` fields are still emitted for
        // compatibility readers.
        let prior = manifest::load();
        let manifest_targets =
            manifest::build_targets(record_targets, &mcp_bin, &cli_bin, prior.as_ref());
        if let Err(e) = write_install_manifest(&mcp_bin, manifest_targets) {
            eprintln!(
                "{} failed to write canonical record: {e}",
                style::warn("warning:")
            );
        }
    } else if !dry_run && !installed.is_empty() && !failed.is_empty() {
        eprintln!(
            "{} partial MCP install: canonical record not updated because {} failed. Run {} after fixing those clients.",
            style::warn("warning:"),
            failed.join(", "),
            style::cmd("difflore agents status"),
        );
    }

    if !has_detected_or_planned {
        println!();
        println!(
            "{} no agents were detected. Install a supported agent (Claude Code, Codex, Cursor, Gemini, Copilot CLI, Antigravity, Goose, Crush, Roo Code, Warp) and re-run.",
            style::warn("!")
        );
        return false;
    }

    print_post_install_help(dry_run, &outcomes);
    !dry_run && !installed.is_empty() && failed.is_empty()
}

/// One row → one outcome, driven by the `AGENTS` table: adding an agent row
/// makes it install automatically. Claude Code hooks ride along inside the
/// Claude Code MCP install (their row's installer is a no-op skip); other hook
/// rows install independently.
fn install_all_targets(mcp_bin: &str, cli_bin: &str, dry_run: bool) -> Vec<TargetOutcome> {
    AGENTS
        .iter()
        // Claude Code hooks install as a side effect of the Claude Code MCP row;
        // omit its standalone (skip) outcome from the printed plan.
        .filter(|spec| spec.name != "Claude Code hooks")
        .map(|spec| registry::install(spec, mcp_bin, cli_bin, dry_run))
        .collect()
}

fn print_install_outcomes(
    outcomes: &[TargetOutcome],
    dry_run: bool,
    installed_surfaces: &BTreeSet<String>,
    mcp_bin: &str,
) {
    let mut skipped_summary: Vec<&str> = Vec::new();
    for o in outcomes {
        let already_installed = dry_run && outcome_already_installed(o, installed_surfaces);
        if dry_run && !already_installed && matches!(o.status, Status::Skipped(_)) {
            skipped_summary.push(o.name);
            continue;
        }
        let plain_verb = install_outcome_verb(&o.status, dry_run, already_installed);
        let (mark, verb) = match &o.status {
            Status::Installed | Status::Updated | Status::Skipped(_) if already_installed => {
                (style::ok(sym::OK), style::emerald(plain_verb))
            }
            Status::Installed | Status::Updated if dry_run => {
                (style::amber("-"), style::amber(plain_verb))
            }
            // Removed isn't reachable on the install path; it renders the same
            // OK/emerald line as Installed/Updated.
            Status::Installed | Status::Updated | Status::Removed => {
                (style::ok(sym::OK), style::emerald(plain_verb))
            }
            Status::Skipped(_) => (style::pewter("-"), style::pewter(plain_verb)),
            Status::Error(_) => (style::err(sym::ERR), style::danger(plain_verb)),
        };
        println!("  {mark} {:<14} {verb}", o.name.bold());
        let sub = match &o.status {
            Status::Skipped(r) | Status::Error(r) => r.as_str(),
            _ => o.detail.as_str(),
        };
        if !sub.is_empty() {
            println!(
                "      {}",
                style::pewter(&public_install_detail(sub, mcp_bin))
            );
        }
    }
    if !skipped_summary.is_empty() {
        let (hooks, agents): (Vec<_>, Vec<_>) = skipped_summary
            .into_iter()
            .partition(|name| name.to_ascii_lowercase().contains("hooks"));
        let mut agents = agents;
        if hooks.contains(&"Windsurf hooks") && !agents.contains(&"Windsurf") {
            agents.push("Windsurf");
        }
        if !agents.is_empty() {
            println!(
                "  {} {}",
                style::pewter("-"),
                style::pewter(&format!(
                    "agents skipped/not detected: {}",
                    agents.join(", ")
                ))
            );
        }
        if !hooks.is_empty() {
            println!(
                "  {} {}",
                style::pewter("-"),
                style::pewter(&format!("hooks skipped/not detected: {}", hooks.join(", ")))
            );
        }
    }
}

fn public_install_detail(detail: &str, mcp_bin: &str) -> String {
    let mut out = detail.replace(mcp_bin, "difflore");
    while let Some((start, end, label)) = public_config_suffix_match(&out) {
        out.replace_range(start..end, label);
    }
    out
}

fn public_config_suffix_match(detail: &str) -> Option<(usize, usize, &'static str)> {
    let normalized = detail.replace('\\', "/");
    for (suffix, label) in [
        ("/.github/copilot/mcp.json", "~/.github/copilot/mcp.json"),
        (
            "/.gemini/antigravity/mcp_config.json",
            "~/.gemini/antigravity/mcp_config.json",
        ),
        ("/.config/crush/mcp.json", "~/.config/crush/mcp.json"),
        ("/.roo/mcp.json", "./.roo/mcp.json"),
        ("/.warp/mcp.json", "~/.warp/mcp.json"),
    ] {
        let mut search_from = 0;
        while let Some(relative_pos) = normalized[search_from..].find(suffix) {
            let pos = search_from + relative_pos;
            if pos > 0 && matches!(normalized.as_bytes().get(pos - 1), Some(b'~' | b'.')) {
                search_from = pos + suffix.len();
                continue;
            }
            let start = normalized[..pos]
                .char_indices()
                .rfind(|(_, ch)| ch.is_whitespace())
                .map_or(0, |(idx, ch)| idx + ch.len_utf8());
            return Some((start, pos + suffix.len(), label));
        }
    }
    None
}

static MCP_TOOLS_HELP: &[(&str, &str)] = &[
    (
        "search_rules",
        "        - find matched rules by id and title",
    ),
    ("get_rules", "           - fetch full rule bodies by id"),
    (
        "get_past_verdicts",
        "     - recall past PR review decisions",
    ),
    (
        "remember_rule",
        "        - propose \"remember this rule\" drafts",
    ),
    (
        "list_memory",
        "           - inspect active rules and pending memory",
    ),
    (
        "get_memory_item",
        "      - fetch one rule, draft, or candidate",
    ),
    (
        "get_memory_activity",
        "  - show retrieved/surfaced rule evidence",
    ),
];

fn print_post_install_help(dry_run: bool, outcomes: &[TargetOutcome]) {
    let clients = outcome_client_names(outcomes);
    let restart_targets = if clients.is_empty() {
        "any agent you use with DiffLore".to_owned()
    } else {
        clients.join(", ")
    };
    println!();
    if dry_run {
        println!(
            "{} dry-run only: no MCP config or hooks were changed.",
            style::emerald(sym::TIP)
        );
        println!(
            "  {} apply with {} when the plan looks right.",
            style::pewter(sym::BULLET),
            style::cmd("difflore agents install"),
        );
    } else {
        println!(
            "{} restart/reload {} so DiffLore is ready for agents.",
            style::emerald(sym::TIP),
            if clients.is_empty() {
                "Claude/Codex/Cursor/etc.".to_owned()
            } else {
                clients.join(", ")
            }
        );
    }
    println!(
        "  {} installed once; use {} later to refresh source-backed team rules.",
        style::pewter(sym::BULLET),
        style::cmd("difflore cloud sync"),
    );
    println!();
    println!(
        "{} memory tools your local agent can now call:",
        style::emerald(sym::TIP)
    );
    for (name, desc) in MCP_TOOLS_HELP {
        println!("  * {}{desc}", style::ident(name));
    }
    println!();
    println!(
        "  {} For large rule libraries prefer search_rules -> get_rules to expand only matched rules.",
        style::pewter("*")
    );
    println!();
    println!("{} status check:", style::emerald(sym::TIP));
    println!(
        "  {} run {} after applying to check config, startup, tool listing, and the built-in search_rules check.",
        style::pewter(sym::BULLET),
        style::cmd("difflore agents status"),
    );
    println!(
        "  {} restart/reload: {}.",
        style::pewter(sym::BULLET),
        style::ident(&restart_targets),
    );
    println!(
        "  {} in one restarted agent, call {} to check that DiffLore can find team rules.",
        style::pewter(sym::BULLET),
        style::cmd("search_rules"),
    );
}

/// What `update` decides to do with one manifest target. Pure (filesystem +
/// hashing happen in the caller); split out so the compare/skip/upgrade policy
/// is unit-testable without touching disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum UpdateAction {
    /// Already current (hash matches, version current) — no rewrite.
    UpToDate,
    /// Block was unchanged since DiffLore wrote it AND a newer version exists →
    /// re-render in place and re-stamp (carries old→new version for the report).
    Upgrade { from: u32, to: u32 },
    /// v1-record / unknown-hash target whose on-disk block matches our standard
    /// render → adopt it (stamp the hash + current version) without rewriting.
    Adopt,
    /// On-disk block differs from the recorded hash (or, for a v1 record, from
    /// our standard render): the human edited it. Skip unless `--force`.
    SkippedLocalEdits,
    /// `--force` opted in to overwrite a locally-edited block with our render.
    ForceOverwrite,
    /// Config file missing or difflore block absent → offer reinstall.
    Gone,
    /// External-CLI-managed and the shape version bumped → re-issue the CLI add.
    ReissueCli { from: u32, to: u32 },
    /// External-CLI-managed and already at the current shape version.
    UpToDateExternal,
}

/// Decide the action for one target from the facts: the recorded hash (`None`
/// for v1 records / external-cli), the recorded version, the current in-binary
/// version, the *on-disk* hash (`None` = gone), the *standard render* hash
/// (what the current writer would produce), and whether the on-disk bytes
/// match a *known legacy render* (a shape an older DiffLore wrote verbatim;
/// see `manifest::legacy_render_hashes`).
pub(super) fn plan_update_target(
    is_external: bool,
    recorded_hash: Option<&str>,
    recorded_version: u32,
    current_version: u32,
    on_disk_hash: Option<&str>,
    standard_render_hash: Option<&str>,
    on_disk_is_legacy_render: bool,
) -> UpdateAction {
    if is_external {
        return if recorded_version < current_version {
            UpdateAction::ReissueCli {
                from: recorded_version,
                to: current_version,
            }
        } else {
            UpdateAction::UpToDateExternal
        };
    }

    // No difflore block on disk → it's gone (uninstalled or never installed).
    let Some(on_disk) = on_disk_hash else {
        return UpdateAction::Gone;
    };

    match recorded_hash {
        // We know exactly what we last wrote.
        Some(recorded) if recorded == on_disk => {
            // Byte-identical to our record → safe to upgrade in place.
            if recorded_version < current_version {
                UpdateAction::Upgrade {
                    from: recorded_version,
                    to: current_version,
                }
            } else {
                UpdateAction::UpToDate
            }
        }
        // The recorded hash can be stale through no fault of the user's: the
        // claude-code render/merge matcher drift recorded install-time hashes
        // that never matched the bytes the merge actually wrote. Recognise a
        // pristine block by its bytes instead of giving up: on-disk == our
        // current standard render → just re-stamp the manifest (Adopt);
        // on-disk == a known legacy render → rewrite it (Upgrade). Only a
        // block matching neither is treated as a real local edit.
        Some(_) if standard_render_hash == Some(on_disk) => UpdateAction::Adopt,
        Some(_) if on_disk_is_legacy_render => UpdateAction::Upgrade {
            from: recorded_version,
            to: current_version,
        },
        Some(_) => UpdateAction::SkippedLocalEdits,
        // v1 record adoption: no recorded hash. On-disk matching the current
        // writer's standard render is adopted in place; a known legacy render
        // is upgraded; anything else is a local edit → skip.
        None => {
            if standard_render_hash == Some(on_disk) {
                UpdateAction::Adopt
            } else if on_disk_is_legacy_render {
                UpdateAction::Upgrade {
                    from: recorded_version,
                    to: current_version,
                }
            } else {
                UpdateAction::SkippedLocalEdits
            }
        }
    }
}

pub fn update_all(dry_run: bool, force: bool) {
    let cli_bin = match resolve_difflore_binary() {
        Ok(b) => b,
        Err(e) => crate::support::util::exit_err(&e),
    };
    let mcp_bin = cli_bin.clone();

    let Some(mut manifest) = manifest::load() else {
        println!(
            "{} no DiffLore install manifest (~/.difflore/mcp.json) found.",
            style::warn("!")
        );
        println!(
            "  {} run {} first to wire DiffLore into your agents.",
            style::pewter(sym::BULLET),
            style::cmd("difflore agents install"),
        );
        return;
    };

    // v1 records have no per-target `targets` array, only `installed_targets`.
    // Seed provisional targets (hash unknown) so the loop's adoption path can
    // recognise and claim our standard blocks without clobbering a user edit.
    if manifest.targets.is_empty() && !manifest.installed_targets.is_empty() {
        manifest.targets = manifest::v1_provisional_targets(&manifest.installed_targets);
    }

    let message = if dry_run {
        "Checking DiffLore block upgrade plan for every recorded target"
    } else {
        "Upgrading DiffLore blocks that are unchanged since DiffLore wrote them"
    };
    let dry_tag = if dry_run {
        format!(" {}", style::amber("(dry-run; no changes)"))
    } else {
        String::new()
    };
    println!(
        "{} {}{dry_tag}",
        style::emerald(sym::TIP),
        style::pewter(message),
    );
    if force {
        println!(
            "  {} {}",
            style::amber("-"),
            style::amber("--force: locally-edited blocks will be overwritten"),
        );
    }
    println!();

    let mut any_changed = false;
    let mut any_gone = false;
    let mut any_skipped = false;
    // Index targets by surface_key so we can re-stamp them in place.
    for idx in 0..manifest.targets.len() {
        let target_name = manifest.targets[idx].name.clone();
        let Some(spec) = registry::find_spec(&target_name) else {
            // Unknown surface (e.g. an agent removed from the registry) — leave
            // the row untouched rather than guessing.
            continue;
        };
        let block_kind = registry::block_kind_of(spec);
        let current_version = block_kind.current_version();
        let is_external = block_kind == BlockKind::ExternalCli;

        let on_disk_hash = if is_external {
            None
        } else {
            manifest::on_disk_block_hash(spec, &cli_bin)
        };
        let standard_render_hash = if is_external {
            None
        } else {
            manifest::render_block_hash(spec, &mcp_bin, &cli_bin)
        };

        let recorded_version = manifest.targets[idx].block_version;
        let recorded_hash = manifest.targets[idx].block_hash.clone();
        let on_disk_is_legacy_render = on_disk_hash.as_deref().is_some_and(|h| {
            manifest::legacy_render_hashes(spec, &cli_bin)
                .iter()
                .any(|legacy| legacy == h)
        });

        let mut action = plan_update_target(
            is_external,
            recorded_hash.as_deref(),
            recorded_version,
            current_version,
            on_disk_hash.as_deref(),
            standard_render_hash.as_deref(),
            on_disk_is_legacy_render,
        );
        // `--force` overrides the protected "local edits" skip, overwriting the
        // hand-edited block with our current render.
        if force && matches!(action, UpdateAction::SkippedLocalEdits) {
            action = UpdateAction::ForceOverwrite;
        }

        let changed = apply_update_action(
            &action,
            spec,
            &mut manifest.targets[idx],
            &mcp_bin,
            &cli_bin,
            current_version,
            standard_render_hash.as_deref(),
            on_disk_hash.as_deref(),
            dry_run,
        );
        any_changed |= changed;
        any_gone |= matches!(action, UpdateAction::Gone);
        any_skipped |= matches!(action, UpdateAction::SkippedLocalEdits);
    }

    // Persist re-stamped versions/hashes only on a real run that changed
    // something; dry-run and no-op runs leave the file alone.
    if !dry_run && any_changed {
        // A v1 record we just adopted/upgraded becomes a v2 manifest on save.
        manifest.manifest_version = manifest::MANIFEST_VERSION;
        if let Err(e) = manifest::save(&manifest) {
            eprintln!(
                "{} failed to update install manifest: {e}",
                style::warn("warning:")
            );
        }
    }

    print_update_footer(dry_run, force, any_changed, any_gone, any_skipped);
}

pub fn agent_update_nudge() -> Option<String> {
    let mut manifest = manifest::load()?;
    if manifest.targets.is_empty() && !manifest.installed_targets.is_empty() {
        manifest.targets = manifest::v1_provisional_targets(&manifest.installed_targets);
    }

    let behind: Vec<String> = manifest
        .targets
        .iter()
        .filter_map(|target| {
            let spec = registry::find_spec(&target.name)?;
            let current_version = registry::block_kind_of(spec).current_version();
            (target.block_version < current_version).then(|| {
                format!(
                    "{} v{}->v{}",
                    target.name, target.block_version, current_version
                )
            })
        })
        .collect();
    if behind.is_empty() {
        return None;
    }

    Some(format!(
        "agent blocks behind ({}); run `{}`",
        behind.join(", "),
        "difflore agents update"
    ))
}

/// Execute (or, in `dry_run`, just report) one [`UpdateAction`] against its
/// target, re-stamping the manifest row on a real upgrade/adopt. Returns
/// whether the manifest was mutated (so the caller knows to persist).
#[allow(clippy::too_many_arguments)]
// reason: each input is an independent fact the action needs; bundling them
// into a struct would add indirection without improving clarity.
fn apply_update_action(
    action: &UpdateAction,
    spec: &'static AgentSpec,
    target: &mut ManifestTarget,
    mcp_bin: &str,
    cli_bin: &str,
    current_version: u32,
    standard_render_hash: Option<&str>,
    on_disk_hash: Option<&str>,
    dry_run: bool,
) -> bool {
    let now = manifest::now_rfc3339();
    match action {
        UpdateAction::UpToDate => {
            report_update_line(
                spec.name,
                style::pewter("-"),
                style::pewter("up to date"),
                "",
            );
            false
        }
        UpdateAction::UpToDateExternal => {
            report_update_line(
                spec.name,
                style::pewter("-"),
                style::pewter("up to date"),
                &format!(
                    "managed by {} (no local block to upgrade)",
                    external_cli_label(spec)
                ),
            );
            false
        }
        UpdateAction::Adopt => {
            let verb = if dry_run { "would adopt" } else { "adopted" };
            report_update_line(
                spec.name,
                style::ok(sym::OK),
                style::emerald(verb),
                "recognised the on-disk block as DiffLore's standard render",
            );
            if dry_run {
                return false;
            }
            target.block_hash = standard_render_hash.or(on_disk_hash).map(ToOwned::to_owned);
            target.block_version = current_version;
            target.updated_at = now;
            true
        }
        UpdateAction::Upgrade { from, to } => {
            let verb = if dry_run {
                format!("would upgrade v{from}->v{to}")
            } else {
                format!("upgraded v{from}->v{to}")
            };
            report_update_line(spec.name, style::ok(sym::OK), style::emerald(&verb), "");
            if dry_run {
                return false;
            }
            // Re-render the current block in place via the registry installer's
            // destructive merge. Claude Code hooks have no standalone installer,
            // so re-render through the Claude Code MCP surface.
            let render_spec = effective_install_spec(spec);
            let outcome = registry::install(render_spec, mcp_bin, cli_bin, false);
            if let Status::Error(e) = &outcome.status {
                eprintln!("      {}", style::danger(e));
                return false;
            }
            // Re-hash from the standard render we just wrote, then re-stamp.
            target.block_hash = standard_render_hash
                .map(ToOwned::to_owned)
                .or_else(|| manifest::on_disk_block_hash(spec, cli_bin));
            target.block_version = current_version;
            target.updated_at = now;
            true
        }
        UpdateAction::ReissueCli { from, to } => {
            let verb = if dry_run {
                format!(
                    "would re-issue {} add (v{from}->v{to})",
                    external_cli_label(spec)
                )
            } else {
                format!(
                    "re-issued {} add (v{from}->v{to})",
                    external_cli_label(spec)
                )
            };
            report_update_line(spec.name, style::ok(sym::OK), style::emerald(&verb), "");
            if dry_run {
                return false;
            }
            // Re-run the idempotent CLI add through the registry driver.
            let outcome = registry::install(spec, mcp_bin, cli_bin, false);
            if let Status::Error(e) = &outcome.status {
                eprintln!("      {}", style::danger(e));
                return false;
            }
            target.block_version = current_version;
            target.updated_at = now;
            true
        }
        UpdateAction::Gone => {
            report_update_line(
                spec.name,
                style::warn("!"),
                style::amber("gone"),
                "no DiffLore block on disk; reinstall with `difflore agents install`",
            );
            false
        }
        UpdateAction::SkippedLocalEdits => {
            report_update_line(
                spec.name,
                style::pewter("-"),
                style::pewter("skipped: local edits since DiffLore wrote it"),
                &format!(
                    "{}; re-run with --force to overwrite",
                    target.config_path.as_deref().map_or_else(
                        || spec.display.to_owned(),
                        |p| public_install_detail(p, mcp_bin)
                    ),
                ),
            );
            false
        }
        UpdateAction::ForceOverwrite => {
            let verb = if dry_run {
                "would overwrite (--force)"
            } else {
                "overwrote (--force)"
            };
            report_update_line(
                spec.name,
                style::ok(sym::OK),
                style::amber(verb),
                "replaced the locally-edited block with DiffLore's current render",
            );
            if dry_run {
                return false;
            }
            let render_spec = effective_install_spec(spec);
            let outcome = registry::install(render_spec, mcp_bin, cli_bin, false);
            if let Status::Error(e) = &outcome.status {
                eprintln!("      {}", style::danger(e));
                return false;
            }
            target.block_hash = standard_render_hash
                .map(ToOwned::to_owned)
                .or_else(|| manifest::on_disk_block_hash(spec, cli_bin));
            target.block_version = current_version;
            target.updated_at = now;
            true
        }
    }
}

/// The surface to drive `registry::install` through when re-rendering `spec`'s
/// block. Claude Code hooks have no standalone installer (their merge rides the
/// Claude Code MCP install), so re-render them via the Claude Code MCP row;
/// every other surface re-renders through itself.
fn effective_install_spec(spec: &'static AgentSpec) -> &'static AgentSpec {
    if spec.name == "Claude Code hooks"
        && let Some(claude) = registry::find_spec("Claude Code")
    {
        return claude;
    }
    spec
}

fn external_cli_label(spec: &AgentSpec) -> &'static str {
    // Codex / Claude are the only external-CLI surfaces; key off the name.
    match spec.name {
        "Codex" => "codex",
        "Claude Code" => "claude",
        other => other,
    }
}

// `mark`/`verb` are freshly-constructed styled strings passed exactly once; by
// value is the natural signature for this small print helper (no caller reuses
// them), so the needless-pass-by-value pedantic lint is a false positive here.
#[allow(clippy::needless_pass_by_value)]
fn report_update_line(
    name: &str,
    mark: colored::ColoredString,
    verb: colored::ColoredString,
    sub: &str,
) {
    println!("  {mark} {:<14} {verb}", name.bold());
    if !sub.is_empty() {
        println!("      {}", style::pewter(sub));
    }
}

fn print_update_footer(dry_run: bool, force: bool, changed: bool, gone: bool, skipped: bool) {
    println!();
    if dry_run {
        println!(
            "{} dry-run only: no blocks were re-rendered and the manifest was not touched.",
            style::emerald(sym::TIP)
        );
        println!(
            "  {} apply with {} when the plan looks right.",
            style::pewter(sym::BULLET),
            style::cmd("difflore agents update"),
        );
        return;
    }
    if changed {
        println!(
            "{} restart/reload the affected agents so they pick up the refreshed DiffLore blocks.",
            style::emerald(sym::TIP),
        );
    } else {
        println!(
            "{} everything is already up to date; no blocks needed re-rendering.",
            style::emerald(sym::TIP),
        );
    }
    if skipped && !force {
        println!(
            "  {} some blocks were skipped because they had local edits; re-run with {} to overwrite them.",
            style::pewter(sym::BULLET),
            style::cmd("difflore agents update --force"),
        );
    }
    if gone {
        println!(
            "  {} some recorded targets had no DiffLore block on disk; reinstall with {}.",
            style::pewter(sym::BULLET),
            style::cmd("difflore agents install"),
        );
    }
}

#[cfg(test)]
mod update_tests {
    use super::*;

    #[test]
    fn public_install_detail_rewrites_multiple_config_paths() {
        let detail = "skipped C:\\Users\\me\\.warp\\mcp.json and /home/me/.roo/mcp.json";

        assert_eq!(
            public_install_detail(detail, "C:\\bin\\difflore.exe"),
            "skipped ~/.warp/mcp.json and ./.roo/mcp.json"
        );
    }

    #[test]
    fn public_install_detail_does_not_rewrite_public_labels_again() {
        assert_eq!(
            public_install_detail("already ~/.warp/mcp.json", "difflore"),
            "already ~/.warp/mcp.json"
        );
    }

    #[test]
    fn external_cli_reissues_only_on_version_bump() {
        // Behind → re-issue; current → up to date. No file is ever touched.
        assert_eq!(
            plan_update_target(true, None, 1, 2, None, None, false),
            UpdateAction::ReissueCli { from: 1, to: 2 }
        );
        assert_eq!(
            plan_update_target(true, None, 2, 2, None, None, false),
            UpdateAction::UpToDateExternal
        );
    }

    #[test]
    fn unchanged_block_behind_version_upgrades() {
        // recorded hash == on-disk hash, version behind → in-place upgrade.
        assert_eq!(
            plan_update_target(
                false,
                Some("sha256:aa"),
                1,
                2,
                Some("sha256:aa"),
                Some("sha256:bb"),
                false
            ),
            UpdateAction::Upgrade { from: 1, to: 2 }
        );
    }

    #[test]
    fn unchanged_block_at_current_version_is_up_to_date() {
        assert_eq!(
            plan_update_target(
                false,
                Some("sha256:aa"),
                1,
                1,
                Some("sha256:aa"),
                Some("sha256:aa"),
                false
            ),
            UpdateAction::UpToDate
        );
    }

    #[test]
    fn edited_block_is_skipped_not_clobbered() {
        // recorded hash != on-disk hash → human edited it → skip.
        assert_eq!(
            plan_update_target(
                false,
                Some("sha256:aa"),
                1,
                2,
                Some("sha256:zz"),
                Some("sha256:bb"),
                false
            ),
            UpdateAction::SkippedLocalEdits
        );
    }

    #[test]
    fn missing_on_disk_block_is_gone() {
        assert_eq!(
            plan_update_target(
                false,
                Some("sha256:aa"),
                1,
                1,
                None,
                Some("sha256:bb"),
                false
            ),
            UpdateAction::Gone
        );
    }

    #[test]
    fn v1_record_adopts_standard_render_else_skips() {
        // No recorded hash (v1 record). On-disk == standard render → adopt.
        assert_eq!(
            plan_update_target(
                false,
                None,
                1,
                1,
                Some("sha256:std"),
                Some("sha256:std"),
                false
            ),
            UpdateAction::Adopt
        );
        // On-disk != standard render → it's been edited → skip (don't clobber).
        assert_eq!(
            plan_update_target(
                false,
                None,
                1,
                1,
                Some("sha256:edited"),
                Some("sha256:std"),
                false
            ),
            UpdateAction::SkippedLocalEdits
        );
    }

    #[test]
    fn stale_recorded_hash_with_standard_on_disk_adopts_not_skips() {
        // The claude-code matcher drift recorded hashes that never matched
        // the merged bytes. If the on-disk block IS our current standard
        // render, the stale record must be corrected (Adopt), not treated as
        // a local edit.
        assert_eq!(
            plan_update_target(
                false,
                Some("sha256:drifted-render"),
                1,
                2,
                Some("sha256:std"),
                Some("sha256:std"),
                false
            ),
            UpdateAction::Adopt
        );
    }

    #[test]
    fn legacy_render_on_disk_upgrades_not_skips() {
        // Old-matcher install: on-disk bytes equal a known legacy render →
        // recognised as ours and rewritten, regardless of the recorded hash.
        assert_eq!(
            plan_update_target(
                false,
                Some("sha256:drifted-render"),
                1,
                2,
                Some("sha256:legacy"),
                Some("sha256:std"),
                true
            ),
            UpdateAction::Upgrade { from: 1, to: 2 }
        );
        // Same recognition for v1 records (no recorded hash).
        assert_eq!(
            plan_update_target(
                false,
                None,
                0,
                2,
                Some("sha256:legacy"),
                Some("sha256:std"),
                true
            ),
            UpdateAction::Upgrade { from: 0, to: 2 }
        );
    }

    #[test]
    fn unknown_on_disk_bytes_still_skip_even_with_stale_record() {
        // Neither the standard render nor a legacy render → a real local
        // edit; never clobber without --force.
        assert_eq!(
            plan_update_target(
                false,
                Some("sha256:drifted-render"),
                1,
                2,
                Some("sha256:user-edited"),
                Some("sha256:std"),
                false
            ),
            UpdateAction::SkippedLocalEdits
        );
    }
}
