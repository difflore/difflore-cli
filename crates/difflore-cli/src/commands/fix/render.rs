use core::fmt::Write as _;
use std::collections::HashMap;
use std::path::Path;

use difflore_core::models::DiffContentRecord;
use difflore_core::review_engine::ReviewIssueRecord;

use crate::commands::util::exit_err;
use crate::style::{self, sym};

use super::pr::PreparedPrFix;
use super::{CONFIDENCE_THRESHOLD, file_loc, percent, review_status_for_outcome};

// Markdown report: Scope → Recalled memories → Findings/patches → Outcome.
pub(super) fn render_fix_report_markdown(
    scope_label: &str,
    matched_rule_ids: &[String],
    matched_rule_titles: &[String],
    suggestions: &[&ReviewIssueRecord],
    attributions: &HashMap<String, String>,
    outcome: &str,
) -> String {
    let mut out = String::new();
    out.push_str("# DiffLore Fix Report\n\n");
    out.push_str("## Scope\n\n");
    writeln!(out, "- {scope_label}").ok();
    out.push('\n');

    out.push_str("## Recalled memories\n\n");
    if matched_rule_titles.is_empty() {
        out.push_str("_No memories recalled for this diff._\n\n");
    } else {
        for (i, title) in matched_rule_titles.iter().enumerate() {
            let id = matched_rule_ids.get(i).map_or("(unknown)", String::as_str);
            let provenance = attributions
                .get(id)
                .map(|repo| format!(" _(learned from `{repo}`)_"))
                .unwrap_or_default();
            writeln!(out, "- **{title}** - `{id}`{provenance}").ok();
        }
        out.push('\n');
    }

    let confident = suggestions
        .iter()
        .filter(|s| s.confidence >= CONFIDENCE_THRESHOLD)
        .count();
    let low = suggestions.len().saturating_sub(confident);
    out.push_str("## Findings / patches\n\n");
    writeln!(
        out,
        "{} suggestion(s): {confident} confident, {low} low-confidence.\n",
        suggestions.len(),
    )
    .ok();
    for (i, issue) in suggestions.iter().enumerate() {
        let pct = percent(issue.confidence);
        let conf_label = if issue.confidence >= CONFIDENCE_THRESHOLD {
            "confident"
        } else {
            "low-confidence"
        };
        let loc = file_loc(issue);
        writeln!(
            out,
            "### {idx}. `{loc}` - {rule} ({pct}% {conf_label})\n",
            idx = i + 1,
            rule = issue.rule,
        )
        .ok();
        if let Some(rule_id) = issue.rule_id.as_deref() {
            let provenance = attributions
                .get(rule_id)
                .map(|repo| format!(" _(learned from `{repo}`)_"))
                .unwrap_or_default();
            writeln!(out, "- rule id: `{rule_id}`{provenance}").ok();
        }
        if !issue.message.trim().is_empty() {
            writeln!(out, "- finding: {}", issue.message.trim()).ok();
        }
        if let Some(s) = issue.suggestion.as_deref()
            && !s.trim().is_empty()
        {
            out.push('\n');
            out.push_str("```diff\n");
            // Cap to 60 lines so a noisy suggestion can't drown the report.
            let lines: Vec<&str> = s.trim().lines().collect();
            let cap = 60;
            for line in lines.iter().take(cap) {
                out.push_str(line);
                out.push('\n');
            }
            if lines.len() > cap {
                writeln!(out, "... ({} more line(s) truncated)", lines.len() - cap).ok();
            }
            out.push_str("```\n\n");
        } else {
            out.push('\n');
        }
    }

    out.push_str("## Outcome\n\n");
    if outcome == "no_changes" {
        out.push_str(
            "- No changed files were found in this scope.\n\
             - No provider call was made and no patches were applied.\n\
             - Make or stage a change, then run `difflore fix --preview` to see recalled memories before applying patches.\n",
        );
    } else {
        out.push_str(
            "- Report generated in observe-only mode; no patches were applied.\n\
             - Run `difflore fix` to walk through suggestions interactively, or `difflore fix --yes` to apply confident patches.\n",
        );
    }
    out
}

pub(super) fn write_fix_report(report_target: &str, md: &str, json: bool) {
    if report_target == "-" {
        print!("{md}");
        return;
    }
    match std::fs::write(report_target, md) {
        Ok(()) => {
            if !json {
                println!(
                    "{} fix report written to {}",
                    style::ok(sym::OK),
                    style::ident(report_target),
                );
            }
        }
        Err(e) => exit_err(&format!(
            "could not write fix report to {report_target}: {e}"
        )),
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn render_agent_handoff_markdown(
    scope_label: &str,
    pr_fix: Option<&PreparedPrFix>,
    repo_root: &Path,
    diff_records: &[DiffContentRecord],
    scope_guardrail: Option<&str>,
    rule_recall_note: Option<&str>,
    matched_rule_ids: &[String],
    matched_rule_titles: &[String],
    suggestions: &[&ReviewIssueRecord],
    attributions: &HashMap<String, String>,
) -> String {
    let mut out = String::new();
    out.push_str("# DiffLore local agent fix task\n\n");
    out.push_str(
        "You are editing this repository locally with team review memory from DiffLore.\n\n",
    );

    out.push_str("## Constraints\n\n");
    out.push_str("- Modify the local working tree only.\n");
    out.push_str("- Do not commit.\n");
    out.push_str("- Do not push.\n");
    out.push_str("- Do not open a PR.\n");
    out.push_str("- Do not post GitHub comments.\n");
    out.push_str("- Keep changes minimal and focused on the findings below.\n");
    out.push_str("- After editing, stop and ask the user to review `git diff`.\n\n");

    out.push_str("## Scope\n\n");
    writeln!(out, "- Diff: {scope_label}").ok();
    writeln!(out, "- Repo root: `{}`", repo_root.display()).ok();
    if let Some(pr) = pr_fix {
        writeln!(out, "- PR: `{}` #{}", pr.repo_full_name, pr.pr_number).ok();
        writeln!(out, "- Title: {}", pr.title).ok();
        writeln!(out, "- Base: `{}`", pr.base_ref).ok();
        writeln!(out, "- Work branch: `{}`", pr.work_branch).ok();
        writeln!(out, "- Head SHA: `{}`", pr.head_sha).ok();
        writeln!(out, "- Merge base: `{}`", pr.merge_base).ok();
        writeln!(out, "- Checked out by DiffLore: `{}`", pr.checked_out).ok();
    }
    out.push('\n');

    if let Some(scope_guardrail) = scope_guardrail.filter(|text| !text.trim().is_empty()) {
        out.push_str("## Scope guardrail\n\n");
        out.push_str(scope_guardrail.trim());
        out.push_str("\n\n");
    }

    if let Some(note) = rule_recall_note.filter(|text| !text.trim().is_empty()) {
        out.push_str("## Rule recall note\n\n");
        writeln!(out, "- {}", note.trim()).ok();
        out.push('\n');
    }

    out.push_str("## Relevant team review rules\n\n");
    if matched_rule_titles.is_empty() {
        out.push_str("_No rules were recalled for this diff._\n");
    } else {
        for (i, title) in matched_rule_titles.iter().enumerate() {
            let id = matched_rule_ids.get(i).map_or("(unknown)", String::as_str);
            let source = attributions
                .get(id)
                .map(|repo| format!(" Learned from: `{repo}`."))
                .unwrap_or_default();
            writeln!(out, "{}. **{title}** (`{id}`).{source}", i + 1).ok();
        }
    }
    out.push('\n');

    out.push_str("## Changed files\n\n");
    if diff_records.is_empty() {
        out.push_str("_No changed files were found in this scope._\n");
    } else {
        for record in diff_records.iter().take(50) {
            writeln!(
                out,
                "- `{}` ({} hunk{})",
                record.file_path,
                record.hunks.len(),
                if record.hunks.len() == 1 { "" } else { "s" }
            )
            .ok();
        }
        if diff_records.len() > 50 {
            writeln!(out, "- ...and {} more file(s)", diff_records.len() - 50).ok();
        }
    }
    out.push('\n');

    out.push_str("## Findings to fix\n\n");
    if suggestions.is_empty() {
        out.push_str("_No actionable findings were generated. Do not invent changes._\n");
    } else {
        for (i, issue) in suggestions.iter().enumerate() {
            let loc = file_loc(issue);
            let pct = percent(issue.confidence);
            writeln!(
                out,
                "### {}. `{loc}` - {} ({pct}% confidence)\n",
                i + 1,
                issue.rule
            )
            .ok();
            if let Some(rule_id) = issue.rule_id.as_deref() {
                let source = attributions
                    .get(rule_id)
                    .map(|repo| format!(" Learned from: `{repo}`."))
                    .unwrap_or_default();
                writeln!(out, "- Rule id: `{rule_id}`.{source}").ok();
            }
            if !issue.message.trim().is_empty() {
                writeln!(out, "- Issue: {}", issue.message.trim()).ok();
            }
            if let Some(suggestion) = issue.suggestion.as_deref().filter(|s| !s.trim().is_empty()) {
                out.push_str("\nSuggested repair:\n\n```diff\n");
                for line in suggestion.trim().lines().take(80) {
                    out.push_str(line);
                    out.push('\n');
                }
                if suggestion.trim().lines().count() > 80 {
                    out.push_str("... (suggestion truncated)\n");
                }
                out.push_str("```\n\n");
            } else {
                out.push('\n');
            }
        }
    }

    out.push_str("## Implementation request\n\n");
    out.push_str(
        "Apply the smallest local changes that satisfy the findings and recalled rules. \
Then stop. Do not commit, push, open a PR, or post comments.\n",
    );

    out
}

fn fix_json_value(
    scope_label: &str,
    matched_rule_ids: &[String],
    matched_rule_titles: &[String],
    suggestions: &[&ReviewIssueRecord],
    attributions: &HashMap<String, String>,
    outcome: &str,
) -> serde_json::Value {
    let findings: Vec<serde_json::Value> = suggestions
        .iter()
        .map(|issue| {
            let source_repo = issue
                .rule_id
                .as_deref()
                .and_then(|id| attributions.get(id))
                .cloned();
            serde_json::json!({
                "id": issue.rule_id,
                "rule": issue.rule,
                "file": issue.file,
                "line": issue.line,
                "confidence": issue.confidence,
                "summary": issue.message,
                "diff": issue.suggestion,
                "sourceRepo": source_repo,
            })
        })
        .collect();
    let recalled_provenance: Vec<serde_json::Value> = matched_rule_ids
        .iter()
        .zip(matched_rule_titles.iter())
        .map(|(id, title)| {
            serde_json::json!({
                "id": id,
                "title": title,
                "sourceRepo": attributions.get(id),
            })
        })
        .collect();
    serde_json::json!({
        "scope": scope_label,
        "recalledRuleIds": matched_rule_ids,
        "recalledRuleTitles": matched_rule_titles,
        "recalled": recalled_provenance,
        "findings": findings,
        "outcome": outcome,
        // Distinguishes a real review (clean or with findings) from a non-review
        // failure surfaced via the same JSON shape.
        "status": review_status_for_outcome(outcome),
    })
}

pub(super) fn emit_fix_json(
    scope_label: &str,
    matched_rule_ids: &[String],
    matched_rule_titles: &[String],
    suggestions: &[&ReviewIssueRecord],
    attributions: &HashMap<String, String>,
    outcome: &str,
) {
    let payload = fix_json_value(
        scope_label,
        matched_rule_ids,
        matched_rule_titles,
        suggestions,
        attributions,
        outcome,
    );
    println!("{}", crate::commands::util::json_compact_or(&payload, "{}"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_review_with_no_findings_reports_reviewed_status() {
        let suggestions: Vec<&ReviewIssueRecord> = Vec::new();
        let attributions = HashMap::new();
        let payload = fix_json_value(
            "working tree",
            &["rule-a".to_owned()],
            &["Some recalled rule".to_owned()],
            &suggestions,
            &attributions,
            "observed",
        );

        // Provider ran and found nothing: a genuine clean pass.
        assert_eq!(payload["outcome"], "observed");
        assert_eq!(payload["status"], "reviewed");
        assert_eq!(payload["findings"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn review_with_findings_still_reports_reviewed_status() {
        let issue = ReviewIssueRecord {
            severity: "warning".to_owned(),
            rule: "Some rule".to_owned(),
            rule_id: Some("rule-a".to_owned()),
            message: "A finding".to_owned(),
            file: Some("src/example.rs".to_owned()),
            line: Some(2),
            suggestion: Some("fix it".to_owned()),
            source_badge: None,
            perspectives: Vec::new(),
            confidence: 0.95,
        };
        let attributions = HashMap::new();
        let payload = fix_json_value(
            "working tree",
            &["rule-a".to_owned()],
            &["Some rule".to_owned()],
            &[&issue],
            &attributions,
            "observed",
        );

        assert_eq!(payload["status"], "reviewed");
        assert_eq!(payload["findings"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn fix_report_distinguishes_empty_scope_from_observe_only_scan() {
        let suggestions: Vec<&ReviewIssueRecord> = Vec::new();
        let attributions = HashMap::new();
        let report = render_fix_report_markdown(
            "working tree",
            &[],
            &[],
            &suggestions,
            &attributions,
            "no_changes",
        );

        assert!(report.contains("No changed files were found"));
        assert!(report.contains("No provider call was made"));
        assert!(!report.contains("Report generated in observe-only mode"));
    }

    #[test]
    fn recalled_memories_show_source_repo_when_available() {
        let suggestions: Vec<&ReviewIssueRecord> = Vec::new();
        let mut attributions = HashMap::new();
        attributions.insert("rule-a".to_owned(), "gin-gonic/gin".to_owned());
        let report = render_fix_report_markdown(
            "working tree",
            &["rule-a".to_owned(), "rule-b".to_owned()],
            &["MaxBytesError handling".to_owned(), "Other rule".to_owned()],
            &suggestions,
            &attributions,
            "observed",
        );

        assert!(
            report
                .contains("**MaxBytesError handling** - `rule-a` _(learned from `gin-gonic/gin`)_")
        );
        // rule-b has no attribution -> no provenance suffix.
        assert!(report.contains("**Other rule** - `rule-b`\n"));
    }

    #[test]
    fn agent_handoff_report_contains_local_only_constraints_and_findings() {
        let issue = ReviewIssueRecord {
            severity: "medium".to_owned(),
            rule: "Do not bypass session expiry checks".to_owned(),
            rule_id: Some("rule-auth".to_owned()),
            message: "Refresh path skips expiry validation.".to_owned(),
            file: Some("src/auth/session.ts".to_owned()),
            line: Some(42),
            suggestion: Some("@@ -1 +1\n-check(false)\n+check(expiry)\n".to_owned()),
            source_badge: None,
            perspectives: Vec::new(),
            confidence: 0.92,
        };
        let diff_records = vec![DiffContentRecord {
            file_path: "src/auth/session.ts".to_owned(),
            hunks: Vec::new(),
        }];
        let mut attributions = HashMap::new();
        attributions.insert("rule-auth".to_owned(), "acme/api".to_owned());

        let report = render_agent_handoff_markdown(
            "PR #123 (main...HEAD)",
            None,
            Path::new("C:/repo"),
            &diff_records,
            Some("- Review about 3 files before declaring done."),
            None,
            &["rule-auth".to_owned()],
            &["Do not bypass session expiry checks".to_owned()],
            &[&issue],
            &attributions,
        );

        assert!(report.contains("# DiffLore local agent fix task"));
        assert!(report.contains("- Do not commit."));
        assert!(report.contains("- Do not push."));
        assert!(report.contains("- Do not post GitHub comments."));
        assert!(report.contains("## Scope guardrail"));
        assert!(report.contains("Review about 3 files"));
        assert!(report.contains("Learned from: `acme/api`"));
        assert!(report.contains("Apply the smallest local changes"));
    }

    #[test]
    fn agent_handoff_report_distinguishes_recall_failure_from_no_rules() {
        let suggestions: Vec<&ReviewIssueRecord> = Vec::new();
        let attributions = HashMap::new();
        let report = render_agent_handoff_markdown(
            "working tree",
            None,
            Path::new("C:/repo"),
            &[],
            None,
            Some("Rule memory retrieval could not complete while trying to open rule index."),
            &[],
            &[],
            &suggestions,
            &attributions,
        );

        assert!(report.contains("## Rule recall note"));
        assert!(report.contains("could not complete"));
        assert!(report.contains("_No rules were recalled for this diff._"));
    }
}
