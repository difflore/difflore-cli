use std::fmt::Write as _;
use std::path::PathBuf;

use difflore_core::models::DiffContentRecord;

use crate::commands::path_hints::missing_file_hints_from_prediction;
use crate::commands::util::project_path;

use super::context::FixContext;

const HANDOFF_NEAREST_SCOPE_SCORE_FLOOR: f64 = 0.09;
const MIN_REPO_SCOPE_MATCHED_PRS_FOR_COUNT: u64 = 5;

pub(super) async fn scope_guardrail_for_handoff(ctx: &FixContext) -> Option<String> {
    let intent = handoff_scope_intent(ctx)?;
    let prediction = difflore_core::mcp_server::predict_pr_scope_for_repos(
        &ctx.db,
        &intent,
        5,
        &ctx.repo_full_name_aliases,
    )
    .await;
    render_scope_guardrail(&prediction, &ctx.diff_records)
}

fn handoff_scope_intent(ctx: &FixContext) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(pr) = ctx.pr_fix.as_ref() {
        if !pr.title.trim().is_empty() {
            parts.push(pr.title.trim().to_owned());
        }
        parts.push(format!("{}#{}", pr.repo_full_name, pr.pr_number));
    }
    if let Some(repo) = ctx
        .repo_full_name
        .as_deref()
        .filter(|repo| !repo.trim().is_empty())
    {
        parts.push(repo.to_owned());
    }
    if !ctx.diff_records.is_empty() {
        let files = ctx
            .diff_records
            .iter()
            .take(12)
            .map(|record| record.file_path.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        parts.push(format!("Changed files: {files}"));
    }
    let intent = parts.join("\n");
    (!intent.trim().is_empty()).then_some(intent)
}

fn render_scope_guardrail(
    prediction: &serde_json::Value,
    diff_records: &[DiffContentRecord],
) -> Option<String> {
    let neighbors = prediction
        .get("n_neighbors")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    if neighbors == 0 {
        return None;
    }
    let median = prediction
        .get("predicted_file_count_median")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let memory_recommended = prediction
        .get("predicted_file_count_recommended")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(median);
    let changed_count = u64::try_from(diff_records.len()).unwrap_or(u64::MAX);
    let recommended = effective_handoff_recommended_file_count(prediction, changed_count);
    let nearest = prediction
        .get("nearest_file_count")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(median);

    let mut out = String::new();
    writeln!(
        out,
        "- DiffLore memory predicts reviewing about {} file{} before declaring done (median {}; strongest match touched {}).",
        recommended,
        if recommended == 1 { "" } else { "s" },
        median,
        nearest,
    )
    .ok();
    if memory_recommended != recommended {
        writeln!(
            out,
            "- Raw memory estimate was {memory_recommended}; handoff kept it conservative for the current diff and repo evidence."
        )
        .ok();
    }
    if let Some(scope_line) = handoff_repo_scope_line(prediction) {
        writeln!(out, "- {scope_line}").ok();
    }
    if !diff_records.is_empty() {
        writeln!(
            out,
            "- Current diff has {} file{}.",
            changed_count,
            if changed_count == 1 { "" } else { "s" }
        )
        .ok();
        if recommended > changed_count {
            writeln!(
                out,
                "- Scope check: likely under-scoped by about {} file{}; inspect the likely missing files before stopping.",
                recommended - changed_count,
                if recommended - changed_count == 1 { "" } else { "s" },
            )
            .ok();
            let hints = handoff_missing_file_hints(prediction, diff_records);
            if !hints.is_empty() {
                writeln!(
                    out,
                    "- Likely missing files from similar PRs: {}.",
                    hints.iter().take(6).cloned().collect::<Vec<_>>().join(", ")
                )
                .ok();
            }
        } else {
            writeln!(
                out,
                "- Scope check: current file count meets the review-memory scope estimate."
            )
            .ok();
        }
    }

    if let Some(categories) = prediction
        .get("predicted_categories")
        .and_then(serde_json::Value::as_array)
        .filter(|categories| !categories.is_empty())
    {
        let labels = categories
            .iter()
            .take(4)
            .filter_map(|category| {
                let name = category
                    .get("category")
                    .and_then(serde_json::Value::as_str)?;
                let probability = category
                    .get("probability")
                    .and_then(serde_json::Value::as_f64)
                    .unwrap_or(0.0);
                Some(format!("{name} ({:.0}%)", probability * 100.0))
            })
            .collect::<Vec<_>>();
        if !labels.is_empty() {
            writeln!(out, "- Likely co-edit categories: {}.", labels.join(", ")).ok();
        }
    }

    if let Some(neighbor_rows) = prediction
        .get("neighbors")
        .and_then(serde_json::Value::as_array)
        .filter(|rows| !rows.is_empty())
    {
        out.push_str("- Closest historical PRs:\n");
        for neighbor in neighbor_rows.iter().take(3) {
            let repo = neighbor
                .get("repo")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?");
            let pr_number = neighbor
                .get("pr_number")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0);
            let score = neighbor
                .get("score")
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(0.0);
            let files = neighbor
                .get("files")
                .and_then(serde_json::Value::as_array)
                .map(|files| {
                    files
                        .iter()
                        .filter_map(serde_json::Value::as_str)
                        .take(3)
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .filter(|files| !files.is_empty())
                .unwrap_or_else(|| "no file evidence".to_owned());
            writeln!(out, "  - {repo}#{pr_number} ({score:.2}): {files}").ok();
        }
    }

    Some(out.trim_end().to_owned())
}

fn effective_handoff_recommended_file_count(
    prediction: &serde_json::Value,
    changed_count: u64,
) -> u64 {
    let median = prediction
        .get("predicted_file_count_median")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let memory_recommended = prediction
        .get("predicted_file_count_recommended")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(median);
    if handoff_repo_scope_evidence_sparse(prediction) {
        return memory_recommended.min(changed_count);
    }
    let nearest_count = prediction
        .get("nearest_file_count")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(memory_recommended);
    let nearest_score = prediction
        .get("neighbors")
        .and_then(serde_json::Value::as_array)
        .and_then(|neighbors| neighbors.first())
        .and_then(|neighbor| neighbor.get("score"))
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(0.0);

    if nearest_score >= HANDOFF_NEAREST_SCOPE_SCORE_FLOOR && nearest_count > changed_count {
        return memory_recommended.max(nearest_count.min(changed_count.saturating_add(3)));
    }
    memory_recommended
}

fn handoff_repo_scope_evidence_sparse(prediction: &serde_json::Value) -> bool {
    let Some(scope) = prediction
        .get("repo_scope")
        .and_then(serde_json::Value::as_object)
    else {
        return false;
    };
    let no_repo_scope_memory = scope
        .get("no_repo_scope_memory")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if no_repo_scope_memory {
        return false;
    }
    scope
        .get("matched_prs")
        .and_then(serde_json::Value::as_u64)
        .is_some_and(|matched| matched < MIN_REPO_SCOPE_MATCHED_PRS_FOR_COUNT)
}

fn handoff_repo_scope_line(prediction: &serde_json::Value) -> Option<String> {
    let scope = prediction
        .get("repo_scope")
        .and_then(serde_json::Value::as_object)?;
    let matched = scope
        .get("matched_prs")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let no_repo_scope_memory = scope
        .get("no_repo_scope_memory")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if no_repo_scope_memory {
        return Some(
            "Repo scope: no same-repo plan history matched; this guardrail stays silent until local repo-scoped memory exists."
                .to_owned(),
        );
    }
    if matched > 0 && matched < MIN_REPO_SCOPE_MATCHED_PRS_FOR_COUNT {
        return Some(format!(
            "Repo scope: only {matched} same-repo historical PR{} matched; count warnings stay conservative.",
            if matched == 1 { "" } else { "s" }
        ));
    }
    if matched >= MIN_REPO_SCOPE_MATCHED_PRS_FOR_COUNT {
        return Some(format!(
            "Repo scope: scoped to {matched} same-repo historical PR{}.",
            if matched == 1 { "" } else { "s" }
        ));
    }
    None
}

fn handoff_missing_file_hints(
    prediction: &serde_json::Value,
    diff_records: &[DiffContentRecord],
) -> Vec<String> {
    let changed_files = diff_records
        .iter()
        .map(|record| record.file_path.clone())
        .collect::<Vec<_>>();
    missing_file_hints_from_prediction(prediction, &changed_files, &PathBuf::from(project_path()))
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
    fn scope_guardrail_renders_plan_pr_prediction() {
        let prediction = serde_json::json!({
            "n_neighbors": 5,
            "predicted_file_count_median": 1,
            "predicted_file_count_recommended": 4,
            "nearest_file_count": 7,
            "coedit_file_hints": [
                { "file": "utils_test.go", "in_n_of_neighbors": 3, "score": 0.9 },
                { "file": "context.go", "in_n_of_neighbors": 2, "score": 0.6 }
            ],
            "repo_scope": {
                "matched_prs": 19,
                "no_repo_scope_memory": false
            },
            "predicted_categories": [
                { "category": "test:go", "probability": 0.6 },
                { "category": "src:go", "probability": 0.4 }
            ],
            "neighbors": [
                {
                    "repo": "gin-gonic/gin",
                    "pr_number": 4542,
                    "score": 0.205,
                    "files": ["context.go", "context_test.go", "utils_test.go"]
                }
            ]
        });

        let diff_records = vec![diff_record("context_test.go")];
        let guardrail = render_scope_guardrail(&prediction, &diff_records).expect("guardrail");

        assert!(guardrail.contains("reviewing about 4 files"));
        assert!(guardrail.contains("median 1"));
        assert!(guardrail.contains("scoped to 19 same-repo historical PRs"));
        assert!(guardrail.contains("Current diff has 1 file"));
        assert!(guardrail.contains("likely under-scoped by about 3 files"));
        assert!(
            guardrail.contains("Likely missing files from similar PRs: context.go, utils_test.go")
        );
        assert!(!guardrail.contains("context_test.go, context_test.go"));
        assert!(guardrail.contains("test:go (60%)"));
        assert!(guardrail.contains("gin-gonic/gin#4542"));
    }

    #[test]
    fn scope_guardrail_keeps_sparse_repo_scope_count_conservative() {
        let prediction = serde_json::json!({
            "n_neighbors": 2,
            "predicted_file_count_median": 66,
            "predicted_file_count_recommended": 66,
            "nearest_file_count": 66,
            "repo_scope": {
                "matched_prs": 2,
                "no_repo_scope_memory": false
            },
            "predicted_categories": [],
            "neighbors": [
                {
                    "repo": "tanstack/store",
                    "pr_number": 295,
                    "score": 0.18,
                    "files": ["package.json", "pnpm-lock.yaml"]
                }
            ]
        });
        let diff_records = vec![
            diff_record(".github/workflows/autofix.yml"),
            diff_record(".github/workflows/pr.yml"),
            diff_record(".github/workflows/release.yml"),
        ];

        let guardrail = render_scope_guardrail(&prediction, &diff_records).expect("guardrail");

        assert!(guardrail.contains("reviewing about 3 files"));
        assert!(guardrail.contains("Raw memory estimate was 66"));
        assert!(guardrail.contains("only 2 same-repo historical PRs matched"));
        assert!(guardrail.contains("current file count meets"));
        assert!(!guardrail.contains("likely under-scoped"));
    }
}
