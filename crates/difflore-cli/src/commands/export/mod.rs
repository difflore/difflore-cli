//! `difflore export` — static projection of this repo's team rules into
//! `AGENTS.md` / `CLAUDE.md` marker blocks, Cursor `.mdc` rules, and
//! OpenCodeReview JSON.
//!
//! Collection, rendering, and the marker-block writeback engine live in
//! `difflore_core::export`; this module owns the CLI surface: format
//! resolution ([`emitters`]), the export plan report (text/`--json`), exit
//! codes, and the gitignore guidance footer. Everything written stays inside
//! the `BEGIN/END DIFFLORE RULES` markers — DiffLore never commits, pushes,
//! or edits `.gitignore`.

mod emitters;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use difflore_core::export::{
    ExportBlockMeta, ExportCollectOptions, MarkerBlockWrite, WriteAction, WriteOutcome,
    build_export_block, collect_rules_for_export, export_content_hash, has_marker_block,
    render_export_body, upsert_marker_block,
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
    let open_code_review_refusals_are_hard =
        open_code_review_refusals_are_hard_failure(&args.formats);
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
        if collection.rules.is_empty() && !has_existing_managed_export(emitter.kind, &path) {
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

        let (content_hash, write_result) = match emitter.kind {
            emitters::EmitterKind::MarkerBlock => {
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

                let write_result = upsert_marker_block(&MarkerBlockWrite {
                    path: &path,
                    block: &block,
                    content_hash: &content_hash,
                    dry_run: args.dry_run,
                })
                .map_err(|e| e.to_string());
                (content_hash, write_result)
            }
            emitters::EmitterKind::CursorRulesDir => {
                let files = emitters::render_cursor_rule_files(&collection.rules);
                let content_hash =
                    export_content_hash(&emitters::render_cursor_rules_manifest(&collection.rules));
                let write_result = upsert_cursor_rules_dir(&path, &files, args.dry_run);
                (content_hash, write_result)
            }
            emitters::EmitterKind::OwnedJson => {
                let body = emitters::render_ocr_rules(&collection.rules);
                let content_hash = export_content_hash(&body);
                let write_result = upsert_owned_json(&path, &body, args.dry_run);
                (content_hash, write_result)
            }
        };

        match write_result {
            Ok(outcome) => {
                if skipped_outcome_is_hard_failure(
                    emitter.kind,
                    &outcome,
                    open_code_review_refusals_are_hard,
                ) {
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
                    content_hash: content_hash.clone(),
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
                    reason: Some(e),
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

fn open_code_review_refusals_are_hard_failure(formats: &[ExportFormatArg]) -> bool {
    formats
        .iter()
        .any(|format| matches!(format, ExportFormatArg::OpenCodeReview))
        && !formats
            .iter()
            .any(|format| matches!(format, ExportFormatArg::All))
}

fn skipped_outcome_is_hard_failure(
    kind: emitters::EmitterKind,
    outcome: &WriteOutcome,
    open_code_review_refusals_are_hard: bool,
) -> bool {
    if outcome.action != WriteAction::Skipped {
        return false;
    }
    if kind == emitters::EmitterKind::OwnedJson
        && outcome
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains(UNOWNED_OPEN_CODE_REVIEW_JSON_REASON))
    {
        return open_code_review_refusals_are_hard;
    }
    true
}

fn has_existing_managed_export(kind: emitters::EmitterKind, path: &Path) -> bool {
    match kind {
        emitters::EmitterKind::MarkerBlock => has_marker_block(path),
        emitters::EmitterKind::CursorRulesDir => has_difflore_cursor_rule_files(path),
        emitters::EmitterKind::OwnedJson => has_owned_json_marker(path),
    }
}

fn has_difflore_cursor_rule_files(path: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(path) else {
        return false;
    };
    entries.filter_map(Result::ok).any(|entry| {
        entry
            .file_name()
            .to_str()
            .is_some_and(emitters::is_difflore_cursor_rule_file_name)
    })
}

fn has_owned_json_marker(path: &Path) -> bool {
    let Ok(existing) = std::fs::read_to_string(path) else {
        return false;
    };
    serde_json::from_str::<serde_json::Value>(&existing).is_ok_and(|value| {
        value
            .get("_difflore")
            .and_then(serde_json::Value::as_object)
            .is_some_and(|marker| {
                marker.get("generator").and_then(serde_json::Value::as_str) == Some("difflore")
            })
    })
}

const UNOWNED_OPEN_CODE_REVIEW_JSON_REASON: &str =
    "already exists without DiffLore ownership marker `_difflore`";

fn upsert_cursor_rules_dir(
    path: &Path,
    desired_files: &BTreeMap<String, String>,
    dry_run: bool,
) -> Result<WriteOutcome, String> {
    let mut directory_exists = false;
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Ok(WriteOutcome {
                action: WriteAction::Skipped,
                reason: Some(format!(
                    "{} is a symlink; refusing to manage Cursor rules through it",
                    path.display()
                )),
            });
        }
        Ok(meta) if !meta.is_dir() => {
            return Ok(WriteOutcome {
                action: WriteAction::Skipped,
                reason: Some(format!("{} is not a directory", path.display())),
            });
        }
        Ok(_) => {
            directory_exists = true;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(format!("failed to stat {}: {e}", path.display()));
        }
    }

    if !directory_exists && !dry_run {
        std::fs::create_dir_all(path)
            .map_err(|e| format!("creating {} failed: {e}", path.display()))?;
    }

    let existing_managed = if directory_exists {
        existing_difflore_cursor_rule_files(path)?
    } else {
        Vec::new()
    };

    let mut changed = !directory_exists && !desired_files.is_empty();
    for stale in existing_managed.iter().filter(|stale| {
        stale
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| !desired_files.contains_key(name))
    }) {
        changed = true;
        if !dry_run {
            std::fs::remove_file(stale)
                .map_err(|e| format!("removing stale {} failed: {e}", stale.display()))?;
        }
    }

    for (name, body) in desired_files {
        let target = path.join(name);
        match std::fs::symlink_metadata(&target) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Ok(WriteOutcome {
                    action: WriteAction::Skipped,
                    reason: Some(format!(
                        "{} is a symlink; refusing to write through it",
                        target.display()
                    )),
                });
            }
            Ok(meta) if meta.is_dir() => {
                return Ok(WriteOutcome {
                    action: WriteAction::Skipped,
                    reason: Some(format!("{} is a directory, not a file", target.display())),
                });
            }
            Ok(_) => {
                let existing = std::fs::read_to_string(&target)
                    .map_err(|e| format!("reading {} failed: {e}", target.display()))?;
                if existing != *body {
                    changed = true;
                    if !dry_run {
                        std::fs::write(&target, body)
                            .map_err(|e| format!("writing {} failed: {e}", target.display()))?;
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                changed = true;
                if !dry_run {
                    std::fs::write(&target, body)
                        .map_err(|e| format!("writing {} failed: {e}", target.display()))?;
                }
            }
            Err(e) => {
                return Err(format!("failed to stat {}: {e}", target.display()));
            }
        }
    }

    let action = if !directory_exists && !desired_files.is_empty() {
        WriteAction::Created
    } else if changed {
        WriteAction::Updated
    } else {
        WriteAction::Unchanged
    };
    Ok(WriteOutcome {
        action,
        reason: None,
    })
}

fn existing_difflore_cursor_rule_files(path: &Path) -> Result<Vec<PathBuf>, String> {
    let entries =
        std::fs::read_dir(path).map_err(|e| format!("reading {} failed: {e}", path.display()))?;
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("reading {} failed: {e}", path.display()))?;
        if entry
            .file_name()
            .to_str()
            .is_some_and(emitters::is_difflore_cursor_rule_file_name)
        {
            files.push(entry.path());
        }
    }
    Ok(files)
}

fn upsert_owned_json(path: &Path, body: &str, dry_run: bool) -> Result<WriteOutcome, String> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Ok(WriteOutcome {
                action: WriteAction::Skipped,
                reason: Some(format!(
                    "{} is a symlink; refusing to write through it",
                    path.display()
                )),
            });
        }
        Ok(meta) if meta.is_dir() => {
            return Ok(WriteOutcome {
                action: WriteAction::Skipped,
                reason: Some(format!("{} is a directory, not a file", path.display())),
            });
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if !dry_run {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("creating {} failed: {e}", parent.display()))?;
                }
                std::fs::write(path, format!("{body}\n"))
                    .map_err(|e| format!("writing {} failed: {e}", path.display()))?;
            }
            return Ok(WriteOutcome {
                action: WriteAction::Created,
                reason: None,
            });
        }
        Err(e) => {
            return Err(format!("failed to stat {}: {e}", path.display()));
        }
    }

    let existing = std::fs::read_to_string(path)
        .map_err(|e| format!("reading {} failed: {e}", path.display()))?;
    if !has_owned_json_marker(path) {
        return Ok(WriteOutcome {
            action: WriteAction::Skipped,
            reason: Some(format!(
                "{} {UNOWNED_OPEN_CODE_REVIEW_JSON_REASON}; preserving hand-written file. \
                 Default/all export treats this as informational; run \
                 `difflore export --format open-code-review` to inspect this target explicitly",
                path.display()
            )),
        });
    }

    let desired = format!("{body}\n");
    if existing == desired {
        return Ok(WriteOutcome {
            action: WriteAction::Unchanged,
            reason: None,
        });
    }

    if !dry_run {
        std::fs::write(path, desired)
            .map_err(|e| format!("writing {} failed: {e}", path.display()))?;
    }
    Ok(WriteOutcome {
        action: WriteAction::Updated,
        reason: None,
    })
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
            "  {} no supported git remote detected; only explicit local rules were exported",
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

#[cfg(test)]
mod tests {
    use super::*;
    use difflore_core::export::ExportRule;

    fn export_rule(id: &str, name: &str, patterns: &[&str]) -> ExportRule {
        ExportRule {
            id: id.to_owned(),
            name: name.to_owned(),
            description:
                "Prefer stable waits instead of sleeping in tests.\nSource: acme/widgets#42"
                    .to_owned(),
            r#type: "review_standard".to_owned(),
            confidence: 0.8,
            origin: "pr_review".to_owned(),
            source_kind: "human".to_owned(),
            source: "local".to_owned(),
            repo_scope: Some("acme/widgets".to_owned()),
            check_prompt: None,
            file_patterns: patterns
                .iter()
                .map(|pattern| (*pattern).to_owned())
                .collect(),
            examples: Vec::new(),
        }
    }

    #[test]
    fn cursor_rules_dir_writes_generated_mdc_and_preserves_handwritten_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(".cursor/rules");
        std::fs::create_dir_all(&path).expect("create rules dir");
        let handwritten = path.join("team-handwritten.mdc");
        std::fs::write(&handwritten, "manual cursor rule\n").expect("seed handwritten rule");
        let rules = [export_rule(
            "rule-1",
            "Avoid sleeps in tests",
            &["tests/**/*.ts"],
        )];
        let files = emitters::render_cursor_rule_files(&rules);

        let outcome = upsert_cursor_rules_dir(&path, &files, false).expect("write cursor rules");

        assert_eq!(outcome.action, WriteAction::Updated);
        assert_eq!(
            std::fs::read_to_string(&handwritten).expect("handwritten still readable"),
            "manual cursor rule\n"
        );
        let generated_name = files.keys().next().expect("generated name");
        let generated = std::fs::read_to_string(path.join(generated_name))
            .expect("generated cursor rule exists");
        assert!(generated.contains("description: \"Avoid sleeps in tests\""));
        assert!(generated.contains("globs: \"tests/**/*.ts\""));
        assert!(generated.contains("alwaysApply: false"));
        assert!(generated.contains("generated by difflore export"));
    }

    #[test]
    fn cursor_rules_dir_deletes_only_stale_difflore_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(".cursor/rules");
        std::fs::create_dir_all(&path).expect("create rules dir");
        let stale = path.join("difflore-old-rule-deadbeef.mdc");
        let handwritten = path.join("team-handwritten.mdc");
        std::fs::write(&stale, "old generated rule\n").expect("seed stale rule");
        std::fs::write(&handwritten, "manual cursor rule\n").expect("seed handwritten rule");
        let rules = [export_rule(
            "rule-2",
            "Use service helpers",
            &["src/**/*.rs"],
        )];
        let files = emitters::render_cursor_rule_files(&rules);

        let outcome = upsert_cursor_rules_dir(&path, &files, false).expect("sync cursor rules");

        assert_eq!(outcome.action, WriteAction::Updated);
        assert!(!stale.exists(), "stale generated file should be removed");
        assert!(
            handwritten.exists(),
            "hand-written cursor rule must be preserved"
        );
        let generated_name = files.keys().next().expect("generated name");
        assert!(path.join(generated_name).exists());
    }

    #[test]
    fn hand_written_open_code_review_json_is_info_skip_for_all_exports() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rule.json");
        let handwritten = "{\n  \"rules\": [\"manual\"]\n}\n";
        std::fs::write(&path, handwritten).expect("seed handwritten rule");

        let outcome = upsert_owned_json(
            &path,
            r#"{"_difflore":{"generator":"difflore"},"rules":[]}"#,
            false,
        )
        .expect("write outcome");

        assert_eq!(outcome.action, WriteAction::Skipped);
        assert!(
            outcome
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains(UNOWNED_OPEN_CODE_REVIEW_JSON_REASON))
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("handwritten still readable"),
            handwritten
        );
        assert!(!skipped_outcome_is_hard_failure(
            emitters::EmitterKind::OwnedJson,
            &outcome,
            false,
        ));
    }

    #[test]
    fn explicit_open_code_review_export_keeps_unowned_json_as_hard_skip() {
        let outcome = WriteOutcome {
            action: WriteAction::Skipped,
            reason: Some(UNOWNED_OPEN_CODE_REVIEW_JSON_REASON.to_owned()),
        };

        assert!(open_code_review_refusals_are_hard_failure(&[
            ExportFormatArg::OpenCodeReview
        ]));
        assert!(!open_code_review_refusals_are_hard_failure(&[
            ExportFormatArg::All
        ]));
        assert!(!open_code_review_refusals_are_hard_failure(&[
            ExportFormatArg::All,
            ExportFormatArg::OpenCodeReview,
        ]));
        assert!(skipped_outcome_is_hard_failure(
            emitters::EmitterKind::OwnedJson,
            &outcome,
            true,
        ));
    }

    #[test]
    fn unsafe_open_code_review_skips_remain_hard_for_all_exports() {
        let outcome = WriteOutcome {
            action: WriteAction::Skipped,
            reason: Some("target is a symlink; refusing to write through it".to_owned()),
        };

        assert!(skipped_outcome_is_hard_failure(
            emitters::EmitterKind::OwnedJson,
            &outcome,
            false,
        ));
    }
}
