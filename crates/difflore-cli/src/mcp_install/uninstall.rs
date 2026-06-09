//! MCP uninstaller: the inverse of `install_all`. Removes the `difflore`
//! entry (and DiffLore hook groups) from every surface DiffLore wired, using
//! the canonical record at `~/.difflore/mcp.json` to know what was installed,
//! then deletes that record. Mirrors `install.rs` in structure and style.
//!
//! Every per-surface remover is a safe no-op when no DiffLore entry exists,
//! so a missing/corrupt canonical record degrades gracefully to "attempt
//! every surface" rather than stranding a half-installed machine.

use std::collections::BTreeSet;

use colored::Colorize;

use super::{
    Status, TargetOutcome,
    common::{canonical_target_key, delete_canonical_record, read_canonical_record_targets},
    registry::{self, AGENTS, AgentSpec},
};
use crate::style::{self, sym};

/// Whether the `claude` remover (which undoes both the MCP entry *and* the
/// lifecycle hooks) should run for a recorded-key set. The record may list
/// either surface — `claude` (MCP) or `claude hooks` — so match on both.
fn record_selects_claude(recorded_keys: &BTreeSet<String>) -> bool {
    recorded_keys.contains("claude") || recorded_keys.contains("claude hooks")
}

/// Pure selection step: pick the `AGENTS` rows to uninstall for
/// `recorded_keys`. An empty set means "no usable record" and selects every
/// surface so a missing or corrupt record still fully cleans up. Claude Code
/// hooks have no standalone remover (the Claude Code MCP row covers them), so
/// that row is dropped here — keeping the exact shape of the legacy list while
/// reading from the single registry table. Split out so the dispatch policy can
/// be unit-tested without touching the filesystem or PATH.
fn selected_specs(recorded_keys: &BTreeSet<String>) -> Vec<&'static AgentSpec> {
    let attempt_all = recorded_keys.is_empty();
    AGENTS
        .iter()
        // Claude Code hooks ride inside the Claude Code MCP remover.
        .filter(|spec| spec.name != "Claude Code hooks")
        .filter(|spec| {
            if attempt_all {
                return true;
            }
            if spec.name == "Claude Code" {
                // The Claude Code MCP remover also strips the lifecycle hooks,
                // so one remover covers Claude MCP + Claude hooks.
                return record_selects_claude(recorded_keys);
            }
            recorded_keys.contains(&canonical_target_key(spec.name))
        })
        .collect()
}

fn uninstall_all_targets(recorded_keys: &BTreeSet<String>, dry_run: bool) -> Vec<TargetOutcome> {
    selected_specs(recorded_keys)
        .into_iter()
        .map(|spec| registry::uninstall(spec, dry_run))
        .collect()
}

const fn uninstall_outcome_verb(status: &Status, dry_run: bool) -> &'static str {
    match status {
        Status::Removed | Status::Installed | Status::Updated if dry_run => "would remove",
        Status::Removed | Status::Installed | Status::Updated => "removed",
        Status::Skipped(_) => "nothing to remove",
        Status::Error(_) => "error",
    }
}

fn print_uninstall_outcomes(outcomes: &[TargetOutcome], dry_run: bool) {
    let mut nothing_to_remove: Vec<&str> = Vec::new();
    for o in outcomes {
        if matches!(o.status, Status::Skipped(_)) {
            nothing_to_remove.push(o.name);
            continue;
        }
        let plain_verb = uninstall_outcome_verb(&o.status, dry_run);
        let (mark, verb) = match &o.status {
            Status::Error(_) => (style::err(sym::ERR), style::danger(plain_verb)),
            _ if dry_run => (style::amber("·"), style::amber(plain_verb)),
            _ => (style::ok(sym::OK), style::emerald(plain_verb)),
        };
        println!("  {mark} {:<14} {verb}", o.name.bold());
        let sub = match &o.status {
            Status::Error(r) => r.as_str(),
            _ => o.detail.as_str(),
        };
        if !sub.is_empty() {
            println!("      {}", style::pewter(sub));
        }
    }
    if !nothing_to_remove.is_empty() {
        println!(
            "  {} {}",
            style::pewter("·"),
            style::pewter(&format!(
                "no DiffLore entry found (already clean): {}",
                nothing_to_remove.join(", ")
            ))
        );
    }
}

fn removed_outcome_names(outcomes: &[TargetOutcome]) -> Vec<&'static str> {
    outcomes
        .iter()
        .filter(|o| matches!(o.status, Status::Removed))
        .map(|o| o.name)
        .collect()
}

fn errored_outcome_names(outcomes: &[TargetOutcome]) -> Vec<&'static str> {
    outcomes
        .iter()
        .filter(|o| matches!(o.status, Status::Error(_)))
        .map(|o| o.name)
        .collect()
}

// ── Public entry point ─────────────────────────────────────────────────────

pub fn uninstall_all(dry_run: bool) {
    let recorded = read_canonical_record_targets();
    let recorded_keys: BTreeSet<String> =
        recorded.iter().map(|t| canonical_target_key(t)).collect();

    let message = if dry_run {
        "Checking DiffLore MCP removal plan for every recorded agent"
    } else {
        "Removing DiffLore MCP server from every recorded agent"
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
    if recorded_keys.is_empty() {
        println!(
            "  {} {}",
            style::pewter("·"),
            style::pewter(
                "no canonical record (~/.difflore/mcp.json) — scanning every supported surface"
            ),
        );
    } else {
        println!(
            "  {} {} {}",
            style::pewter("recorded targets:"),
            style::emerald(&recorded.join(", ")),
            style::pewter(&format!("({})", recorded.len())),
        );
    }
    println!();

    let outcomes = uninstall_all_targets(&recorded_keys, dry_run);
    print_uninstall_outcomes(&outcomes, dry_run);

    let removed = removed_outcome_names(&outcomes);
    let errored = errored_outcome_names(&outcomes);

    println!();
    if dry_run {
        println!(
            "{} dry-run only: no MCP config, hooks, or the canonical record were changed.",
            style::emerald(sym::TIP)
        );
        println!(
            "  {} apply with {} when the plan looks right.",
            style::pewter(sym::BULLET),
            style::cmd("difflore agents uninstall"),
        );
        return;
    }

    // Only delete the canonical record on a clean real run. If a surface
    // errored, keep the record so a re-run (or `difflore agents status`)
    // still knows what remains wired.
    if errored.is_empty() {
        match delete_canonical_record() {
            Ok(Some(path)) => println!(
                "{} removed canonical record {}",
                style::ok(sym::OK),
                style::pewter(&path.display().to_string()),
            ),
            Ok(None) => {}
            Err(e) => eprintln!(
                "{} failed to remove canonical record: {e}",
                style::warn("warning:")
            ),
        }
    } else {
        eprintln!(
            "{} {} failed to clean up; canonical record kept so {} can show what remains.",
            style::warn("warning:"),
            errored.join(", "),
            style::cmd("difflore agents status"),
        );
    }

    println!();
    if removed.is_empty() && errored.is_empty() {
        println!(
            "{} nothing to remove — DiffLore was not wired into any detected agent.",
            style::emerald(sym::TIP)
        );
    } else {
        println!(
            "{} restart/reload any open agents so they drop the DiffLore memory server.",
            style::emerald(sym::TIP),
        );
        println!(
            "  {} re-add later with {} when you want team review memory back.",
            style::pewter(sym::BULLET),
            style::cmd("difflore agents install"),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uninstall_verbs_describe_removal_plan_and_execution() {
        assert_eq!(
            uninstall_outcome_verb(&Status::Removed, true),
            "would remove"
        );
        assert_eq!(uninstall_outcome_verb(&Status::Removed, false), "removed");
        assert_eq!(
            uninstall_outcome_verb(&Status::Skipped("x".into()), false),
            "nothing to remove"
        );
        assert_eq!(
            uninstall_outcome_verb(&Status::Error("x".into()), false),
            "error"
        );
    }

    fn selected_names(recorded_keys: &BTreeSet<String>) -> Vec<String> {
        selected_specs(recorded_keys)
            .into_iter()
            .map(|spec| canonical_target_key(spec.name))
            .collect()
    }

    #[test]
    fn dispatch_filters_to_recorded_targets_when_record_present() {
        // Only Cursor + Cursor hooks recorded → only those surfaces selected.
        let recorded: BTreeSet<String> = [
            canonical_target_key("Cursor"),
            canonical_target_key("Cursor hooks"),
        ]
        .into_iter()
        .collect();
        assert_eq!(selected_names(&recorded), vec!["cursor", "cursor hooks"]);
    }

    #[test]
    fn claude_hooks_only_record_still_selects_the_claude_remover() {
        // The MCP probe can fail while hooks remain, leaving only the hook
        // surface recorded. The combined claude remover must still run.
        let recorded: BTreeSet<String> =
            std::iter::once(canonical_target_key("Claude Code hooks")).collect();
        assert!(selected_names(&recorded).contains(&"claude".to_owned()));
    }

    #[test]
    fn dispatch_attempts_every_surface_when_record_empty() {
        let empty = BTreeSet::new();
        // Mirrors the install dispatch breadth (10 agents + 3 hook surfaces);
        // Claude Code hooks fold into the Claude Code remover, so the standalone
        // hooks row is excluded — every AGENTS row except that one.
        let expected = AGENTS
            .iter()
            .filter(|s| s.name != "Claude Code hooks")
            .count();
        assert_eq!(selected_specs(&empty).len(), expected);
        let names = selected_names(&empty);
        assert!(names.contains(&"claude".to_owned()));
        assert!(names.contains(&"windsurf hooks".to_owned()));
    }

    #[test]
    fn removed_and_errored_partitions_match_status() {
        let outcomes = vec![
            TargetOutcome {
                name: "Cursor",
                status: Status::Removed,
                detail: String::new(),
            },
            TargetOutcome {
                name: "Gemini",
                status: Status::Skipped("none".into()),
                detail: String::new(),
            },
            TargetOutcome {
                name: "Goose",
                status: Status::Error("boom".into()),
                detail: String::new(),
            },
        ];
        assert_eq!(removed_outcome_names(&outcomes), vec!["Cursor"]);
        assert_eq!(errored_outcome_names(&outcomes), vec!["Goose"]);
    }
}
