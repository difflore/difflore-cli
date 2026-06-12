//! `difflore export` — static projection of this repo's team rules into
//! `AGENTS.md` / `CLAUDE.md` marker blocks.
//!
//! Collection, rendering, and the marker-block writeback engine live in
//! `difflore_core::export`; this module owns the CLI surface: format
//! resolution ([`emitters`]), the export plan report (text/`--json`), exit
//! codes, and the gitignore guidance footer. Everything written stays inside
//! the `BEGIN/END DIFFLORE RULES` markers — DiffLore never commits, pushes,
//! or edits `.gitignore`.

mod emitters;

use difflore_core::export::{
    ExportBlockMeta, ExportCollectOptions, MarkerBlockWrite, WriteAction, build_export_block,
    collect_rules_for_export, export_content_hash, has_marker_block, render_export_body,
    upsert_marker_block,
};
use serde::Serialize;

use crate::cli::ExportFormatArg;
use crate::runtime::CommandContext;
use crate::style::{self, sym};
use crate::support::util::{exit_code, json_or};

pub(crate) struct ExportArgs {
    pub(crate) formats: Vec<ExportFormatArg>,
    pub(crate) dry_run: bool,
    pub(crate) json: bool,
    pub(crate) no_examples: bool,
    pub(crate) local_only: bool,
    /// `--max-rules <N>`: cap the export to the first N rules of the
    /// deterministic collection order. `None` = unlimited (the default).
    pub(crate) max_rules: Option<usize>,
}

impl From<crate::cli::ExportCliArgs> for ExportArgs {
    fn from(args: crate::cli::ExportCliArgs) -> Self {
        Self {
            formats: args.format,
            dry_run: args.dry_run,
            json: args.json,
            no_examples: args.no_examples,
            local_only: args.local_only,
            // clap parses the cap as u64 (range-checked >= 1); saturate on
            // 32-bit targets rather than wrap.
            max_rules: args
                .max_rules
                .map(|n| usize::try_from(n).unwrap_or(usize::MAX)),
        }
    }
}

#[derive(Serialize)]
struct TargetReport {
    format: &'static str,
    file: &'static str,
    path: String,
    action: &'static str,
    /// Rules actually exported (after any `--max-rules` cap).
    rules: usize,
    /// In-scope rules before the cap; `total_rules > rules` ⇔ `truncated`.
    total_rules: usize,
    /// Whether `--max-rules` dropped rules from this target.
    truncated: bool,
    content_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

#[derive(Serialize)]
struct ExportReport {
    dry_run: bool,
    local_only: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_rules: Option<usize>,
    repo_scopes: Vec<String>,
    targets: Vec<TargetReport>,
}

pub(crate) async fn handle_export(ctx: &CommandContext, args: ExportArgs) {
    let emitters = emitters::resolve(&args.formats);
    let generated_at = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

    let mut repo_scopes: Vec<String> = Vec::new();
    let mut targets: Vec<TargetReport> = Vec::new();
    // Refusals (symlink / corrupted markers) and IO errors fail the run;
    // "no rules in scope" skips stay informational.
    let mut hard_failure = false;

    for emitter in emitters {
        let collection = match collect_rules_for_export(
            &ctx.db,
            &ctx.project,
            ExportCollectOptions {
                engine: emitter.engine,
                local_only: args.local_only,
                include_examples: !args.no_examples,
                max_rules: args.max_rules,
            },
        )
        .await
        {
            Ok(collection) => collection,
            Err(e) => {
                hard_failure = true;
                targets.push(TargetReport {
                    format: emitter.format,
                    file: emitter.file_name,
                    path: ctx.project.join(emitter.file_name).display().to_string(),
                    action: "skipped",
                    rules: 0,
                    total_rules: 0,
                    truncated: false,
                    content_hash: String::new(),
                    reason: Some(format!("failed to collect rules: {e}")),
                });
                continue;
            }
        };
        repo_scopes.clone_from(&collection.repo_scopes);
        let truncated = collection.total_in_scope > collection.rules.len();

        let path = ctx.project.join(emitter.file_name);
        // An empty rule set refreshes an existing block (so a stale export
        // never lingers) but does not litter the repo with a new file.
        if collection.rules.is_empty() && !has_marker_block(&path) {
            targets.push(TargetReport {
                format: emitter.format,
                file: emitter.file_name,
                path: path.display().to_string(),
                action: "skipped",
                rules: 0,
                total_rules: collection.total_in_scope,
                truncated,
                content_hash: String::new(),
                reason: Some(
                    "no rules in scope for this repo; run `difflore import-reviews` first"
                        .to_owned(),
                ),
            });
            continue;
        }

        let body = render_export_body(&collection.rules);
        let content_hash = export_content_hash(&body);
        let block = build_export_block(
            &ExportBlockMeta {
                tool_version: env!("CARGO_PKG_VERSION"),
                generated_at_utc: &generated_at,
                rule_count: collection.rules.len(),
                repo_scopes: &collection.repo_scopes,
                local_only: args.local_only,
            },
            &body,
        );

        match upsert_marker_block(&MarkerBlockWrite {
            path: &path,
            block: &block,
            content_hash: &content_hash,
            dry_run: args.dry_run,
        }) {
            Ok(outcome) => {
                if outcome.action == WriteAction::Skipped {
                    hard_failure = true;
                }
                targets.push(TargetReport {
                    format: emitter.format,
                    file: emitter.file_name,
                    path: path.display().to_string(),
                    action: outcome.action.as_str(),
                    rules: collection.rules.len(),
                    total_rules: collection.total_in_scope,
                    truncated,
                    content_hash,
                    reason: outcome.reason,
                });
            }
            Err(e) => {
                hard_failure = true;
                targets.push(TargetReport {
                    format: emitter.format,
                    file: emitter.file_name,
                    path: path.display().to_string(),
                    action: "skipped",
                    rules: collection.rules.len(),
                    total_rules: collection.total_in_scope,
                    truncated,
                    content_hash,
                    reason: Some(e.to_string()),
                });
            }
        }
    }

    let report = ExportReport {
        dry_run: args.dry_run,
        local_only: args.local_only,
        max_rules: args.max_rules,
        repo_scopes,
        targets,
    };

    if args.json {
        println!("{}", json_or(&report, "{\"error\":\"serialize failed\"}"));
    } else {
        print_human(&report);
    }

    if hard_failure {
        exit_code(1);
    }
}

fn print_human(report: &ExportReport) {
    if report.dry_run {
        println!(
            "{}",
            style::title("Export plan (dry run — nothing written):")
        );
    } else {
        println!("{}", style::title("Exported team rules:"));
    }
    for target in &report.targets {
        let line = match target.action {
            "created" => format!(
                "{} {} {} — {} (hash {})",
                style::ok(sym::OK),
                style::ident(target.file),
                if report.dry_run {
                    "would be created"
                } else {
                    "created"
                },
                rules_phrase(target),
                target.content_hash,
            ),
            "updated" => format!(
                "{} {} {} — {} (hash {})",
                style::ok(sym::OK),
                style::ident(target.file),
                if report.dry_run {
                    "would be updated"
                } else {
                    "updated"
                },
                rules_phrase(target),
                target.content_hash,
            ),
            "unchanged" => format!(
                "{} {} unchanged — {} (hash {})",
                style::pewter(sym::BULLET),
                style::ident(target.file),
                rules_phrase(target),
                target.content_hash,
            ),
            _ => format!(
                "{} {} skipped: {}",
                style::warn(sym::WARN),
                style::ident(target.file),
                target.reason.as_deref().unwrap_or("unknown reason"),
            ),
        };
        println!("  {line}");
    }

    if report.repo_scopes.is_empty() {
        println!(
            "  {} no GitHub remote detected; only explicit local rules were exported",
            style::pewter(sym::BULLET),
        );
    } else {
        println!(
            "  {} repo scope: {}",
            style::pewter(sym::BULLET),
            report.repo_scopes.join(", "),
        );
    }

    println!();
    println!(
        "{} This export is a static snapshot and goes stale as rules evolve; run {} for live diff-aware injection.",
        style::emerald(sym::TIP),
        style::cmd("difflore agents install"),
    );
    println!(
        "{} Commit the exported file(s) to share rules with your repo, or add them to .gitignore yourself — DiffLore never edits .gitignore.",
        style::emerald(sym::TIP),
    );
}

/// `"N rules"` normally; `"N of M rules (--max-rules cap)"` when the cap
/// dropped rules, so a truncated plan is visible without `--json`.
fn rules_phrase(target: &TargetReport) -> String {
    if target.truncated {
        format!(
            "{} of {} rules (--max-rules cap)",
            target.rules, target.total_rules
        )
    } else {
        format!("{} rule{}", target.rules, plural_s(target.rules))
    }
}

const fn plural_s(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}
