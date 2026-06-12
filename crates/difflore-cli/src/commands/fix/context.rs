use std::path::PathBuf;

use difflore_core::domain::models::DiffContentRecord;

use crate::runtime::CommandContext;
use crate::support::util::{ensure_project, project_path};

use super::pr::{PreparePrOptions, PreparedPrFix, prepare_pr_fix};
use super::scope::{DiffScope, collect_diff, parse_diff_scope};

pub(super) struct FixContext {
    pub(super) db: difflore_core::SqlitePool,
    pub(super) path: PathBuf,
    pub(super) project_id: String,
    pub(super) diff_records: Vec<DiffContentRecord>,
    pub(super) diff_scope: DiffScope,
    pub(super) repo_full_name: Option<String>,
    pub(super) repo_full_name_aliases: Vec<String>,
    pub(super) target_file: Option<String>,
    pub(super) review_id: Option<String>,
    pub(super) pr_fix: Option<PreparedPrFix>,
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn prepare_fix_context(
    cmd_ctx: &CommandContext,
    diff_scope_arg: Option<&str>,
    pr: Option<&str>,
    pr_repo: Option<&str>,
    pr_base: Option<&str>,
    pr_work_branch: Option<&str>,
    pr_no_checkout: bool,
    pr_allow_dirty: bool,
    pr_yes: bool,
    pr_preview: bool,
    path_arg: Option<&PathBuf>,
) -> anyhow::Result<FixContext> {
    let db = cmd_ctx.db.clone();
    let cwd = PathBuf::from(project_path());

    let pr_fix = if let Some(raw_pr) = pr {
        Some(
            prepare_pr_fix(
                &cwd,
                PreparePrOptions {
                    raw_pr,
                    repo_hint: pr_repo,
                    base_override: pr_base,
                    work_branch: pr_work_branch,
                    no_checkout: pr_no_checkout,
                    allow_dirty: pr_allow_dirty,
                    yes: pr_yes,
                    preview: pr_preview,
                },
            )
            .await?,
        )
    } else {
        None
    };

    let path = pr_fix
        .as_ref()
        .map(|pr| pr.repo_root.clone())
        .or_else(|| path_arg.filter(|p| p.is_dir()).cloned())
        .unwrap_or_else(|| cwd.clone());
    let target_file = path_arg
        .filter(|p| !p.is_dir())
        .map(|p| normalize_target_path(&path, p));
    let path_str = path.to_string_lossy().to_string();
    let project = ensure_project(&db, &path_str).await;

    let (mut diff_records, diff_scope, repo_full_name_aliases, repo_full_name, review_id) =
        if let Some(pr) = pr_fix.as_ref() {
            (
                pr.diff_records.clone(),
                DiffScope::PullRequest {
                    label: pr.scope_label.clone(),
                },
                pr.repo_full_name_aliases.clone(),
                Some(pr.repo_full_name.clone()),
                Some(pr.review_id.clone()),
            )
        } else {
            let requested_scope = parse_diff_scope(diff_scope_arg)?;
            let (diff_records, diff_scope) = collect_diff(&path, requested_scope).await?;
            let configured_gitlab_hosts =
                difflore_core::ingest::gitlab::auth::configured_hosts().await;
            let repo_full_name_aliases =
                difflore_core::infra::git::detect_repo_full_names_with_gitlab_hosts(
                    &path_str,
                    &configured_gitlab_hosts,
                );
            let repo_full_name = repo_full_name_aliases.first().cloned();
            (
                diff_records,
                diff_scope,
                repo_full_name_aliases,
                repo_full_name,
                None,
            )
        };
    if let Some(target) = target_file.as_deref() {
        diff_records.retain(|record| record.file_path == target);
    }

    Ok(FixContext {
        db,
        path,
        project_id: project.id,
        diff_records,
        diff_scope,
        repo_full_name,
        repo_full_name_aliases,
        target_file,
        review_id,
        pr_fix,
    })
}

fn normalize_target_path(repo_root: &std::path::Path, path: &std::path::Path) -> String {
    let relative = if path.is_absolute() {
        path.strip_prefix(repo_root).unwrap_or(path)
    } else {
        path
    };
    relative.to_string_lossy().replace('\\', "/")
}

// Prefer a real source file so retrieval has a language signal rather than
// only diff headers and import noise.
pub(super) fn primary_file_for_retrieval(diff_records: &[DiffContentRecord]) -> Option<String> {
    let first_changed = diff_records.iter().find_map(non_empty_file_path);
    diff_records
        .iter()
        .find_map(|record| {
            let file = non_empty_file_path(record)?;
            if is_source_or_test_file(&file) {
                Some(file)
            } else {
                None
            }
        })
        .or(first_changed)
}

/// Every changed file path in the diff, in record order (the authoritative
/// changeset for retrieval's strict file-pattern cascade). Deduped and
/// blank-filtered; empty when the diff carries no usable paths.
pub(super) fn changed_files_for_retrieval(diff_records: &[DiffContentRecord]) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    diff_records
        .iter()
        .filter_map(non_empty_file_path)
        .filter(|file| seen.insert(file.clone()))
        .collect()
}

fn non_empty_file_path(record: &DiffContentRecord) -> Option<String> {
    let file = record.file_path.trim();
    if file.is_empty() {
        None
    } else {
        Some(file.to_owned())
    }
}

fn is_source_or_test_file(file: &str) -> bool {
    let normalized = file.replace('\\', "/").to_ascii_lowercase();
    let Some(ext) = std::path::Path::new(&normalized)
        .extension()
        .and_then(|ext| ext.to_str())
    else {
        return false;
    };
    matches!(
        ext,
        "c" | "cc"
            | "cpp"
            | "cxx"
            | "h"
            | "hpp"
            | "cs"
            | "go"
            | "java"
            | "js"
            | "jsx"
            | "mjs"
            | "cjs"
            | "ts"
            | "tsx"
            | "mts"
            | "cts"
            | "py"
            | "rb"
            | "rs"
            | "swift"
            | "kt"
            | "kts"
            | "php"
            | "vue"
            | "svelte"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diff_record(file_path: &str) -> DiffContentRecord {
        DiffContentRecord {
            file_path: file_path.to_owned(),
            hunks: Vec::new(),
        }
    }

    #[test]
    fn primary_file_prefers_source_over_changeset() {
        let records = vec![
            diff_record(".changeset/whole-views-wear.md"),
            diff_record("packages/form-core/src/FieldApi.ts"),
            diff_record("packages/form-core/tests/DynamicValidation.spec.ts"),
        ];

        assert_eq!(
            primary_file_for_retrieval(&records).as_deref(),
            Some("packages/form-core/src/FieldApi.ts")
        );
    }

    #[test]
    fn primary_file_falls_back_to_docs_when_only_docs_changed() {
        let records = vec![diff_record(".changeset/whole-views-wear.md")];

        assert_eq!(
            primary_file_for_retrieval(&records).as_deref(),
            Some(".changeset/whole-views-wear.md")
        );
    }

    #[test]
    fn changed_files_keep_order_dedupe_and_skip_blanks() {
        let records = vec![
            diff_record("db/schema/users.sql"),
            diff_record("  "),
            diff_record("src/api.ts"),
            diff_record("db/schema/users.sql"),
        ];
        assert_eq!(
            changed_files_for_retrieval(&records),
            vec!["db/schema/users.sql".to_owned(), "src/api.ts".to_owned()],
        );
        assert!(changed_files_for_retrieval(&[]).is_empty());
    }
}
