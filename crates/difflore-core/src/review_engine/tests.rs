#![cfg(test)]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "test scaffolding"
)]

use super::parse::{
    map_issues, parse_issues, parse_summary_response, parse_verify_response, severity_rank,
};
use super::pipeline::{collect_diff_files, count_blocking, run_review_summary, verify_pass};
use super::prompts::{build_system_prompt, build_user_prompt};
use super::*;
use crate::context::types::PastVerdict;
use crate::error::CoreError;
use crate::observability::trajectory::{TrajectoryBuilder, TrajectoryStep};

// Prompt builders

#[test]
fn user_prompt_orders_rules_before_file_section() {
    // Without rules / file: clean diff section only.
    let bare = build_user_prompt("--- a\n+++ b\n", None, None);
    assert!(bare.contains("## Diff to Review"));
    assert!(!bare.contains("## Review Rules"));
    assert!(!bare.contains("## File:"));

    // With both: rules section must precede file section. Wrong order
    // breaks the prompt-cache contract (rules are shared/team-stable,
    // files are per-review).
    let full = build_user_prompt("diff body", Some("Rule: x"), Some("src/foo.ts"));
    let rules_idx = full.find("## Review Rules").unwrap();
    let file_idx = full.find("## File:").unwrap();
    assert!(rules_idx < file_idx);
    assert!(full.contains("Rule: x"));
    assert!(full.contains("src/foo.ts"));
}

// Parser

#[test]
fn parse_issues_handles_multiple_response_shapes() {
    // Each case carries a name so a failure points at the specific shape.
    // A regression in any one path silently drops findings — the
    // user-visible scenario is "review found 0 issues but actually said
    // something".
    struct Case {
        name: &'static str,
        raw: &'static str,
        expected_len: usize,
        expected_rule: Option<&'static str>,
    }
    let cases: &[Case] = &[
        Case {
            name: "direct_json_array",
            raw: r#"[{"severity":"error","rule":"no-any","message":"x","line":42}]"#,
            expected_len: 1,
            expected_rule: Some("no-any"),
        },
        Case {
            name: "fenced_markdown_block",
            raw: "Findings:\n```json\n[{\"severity\":\"warning\",\"rule\":\"r1\",\"message\":\"m\"}]\n```",
            expected_len: 1,
            expected_rule: Some("r1"),
        },
        Case {
            name: "bracket_scan_fallback",
            raw: "noise [{\"severity\":\"info\",\"rule\":\"x\",\"message\":\"y\"}] more",
            expected_len: 1,
            expected_rule: Some("x"),
        },
        Case {
            name: "unparseable_text_yields_empty",
            raw: "not json",
            expected_len: 0,
            expected_rule: None,
        },
        Case {
            name: "empty_string_yields_empty",
            raw: "",
            expected_len: 0,
            expected_rule: None,
        },
    ];
    for case in cases {
        let issues = parse_issues(case.raw);
        assert_eq!(issues.len(), case.expected_len, "[{}]", case.name);
        if let Some(rule) = case.expected_rule {
            assert_eq!(issues[0].rule, rule, "[{}]", case.name);
        }
    }
}

#[test]
fn map_issues_fills_defaults_for_missing_fields() {
    // Each case names a per-issue shape so a default-bleed regression
    // (e.g. severity from `full` leaking into `minimal`) names the
    // offending row directly.
    struct Case {
        name: &'static str,
        json: &'static str,
        severity: &'static str,
        rule_id: Option<&'static str>,
        file: Option<&'static str>,
        line: Option<i32>,
        suggestion: Option<&'static str>,
    }
    let cases: &[Case] = &[
        Case {
            name: "minimal_only_rule_field",
            json: r#"{"rule":"only-rule"}"#,
            severity: "info",
            rule_id: None,
            file: None,
            line: None,
            suggestion: None,
        },
        Case {
            name: "full_object_keeps_every_field",
            json: r#"{"severity":"error","rule":"r","message":"m","ruleId":"rid","file":"f.ts","line":7,"suggestion":"fix it"}"#,
            severity: "error",
            rule_id: Some("rid"),
            file: Some("f.ts"),
            line: Some(7),
            suggestion: Some("fix it"),
        },
    ];

    // Map all cases in one batch so we also check defaults don't bleed
    // between entries.
    let combined = format!(
        "[{}]",
        cases.iter().map(|c| c.json).collect::<Vec<_>>().join(",")
    );
    let arr: Vec<serde_json::Value> = serde_json::from_str(&combined).unwrap();
    let issues = map_issues(&arr);
    assert_eq!(issues.len(), cases.len());
    for (i, case) in cases.iter().enumerate() {
        assert_eq!(issues[i].severity, case.severity, "[{}]", case.name);
        assert_eq!(
            issues[i].rule_id.as_deref(),
            case.rule_id,
            "[{}]",
            case.name
        );
        assert_eq!(issues[i].file.as_deref(), case.file, "[{}]", case.name);
        assert_eq!(issues[i].line, case.line, "[{}]", case.name);
        assert_eq!(
            issues[i].suggestion.as_deref(),
            case.suggestion,
            "[{}]",
            case.name
        );
    }
}

// Perspectives

fn issue(
    severity: &str,
    rule: &str,
    rule_id: Option<&str>,
    file: Option<&str>,
    line: Option<i32>,
) -> ReviewIssueRecord {
    ReviewIssueRecord {
        severity: severity.to_owned(),
        rule: rule.to_owned(),
        rule_id: rule_id.map(String::from),
        message: String::new(),
        file: file.map(String::from),
        line,
        suggestion: None,
        source_badge: None,
        perspectives: Vec::new(),
        confidence: default_confidence(),
    }
}

#[test]
fn build_system_prompt_some_appends_addendum_for_each_perspective() {
    // For every perspective: prompt grows past the base, contains the
    // perspective's own addendum, and includes the perspective header.
    let base = build_system_prompt(None);
    for p in ReviewPerspective::all() {
        let with = build_system_prompt(Some(p));
        assert!(with.starts_with(&base));
        assert!(with.len() > base.len());
        assert!(with.contains(p.system_prompt_addendum()));
        assert!(with.contains("## Perspective:"));
    }
}

// Segmented prompt (prompt-cache contract)

fn rule(id: &str, content: &str) -> TeamRuleDigest {
    TeamRuleDigest {
        id: id.to_owned(),
        content: content.to_owned(),
    }
}

fn pv(id: &str, snippet: &str, issue: &str) -> PastVerdict {
    PastVerdict {
        extraction_id: id.into(),
        code_snippet: snippet.into(),
        issue_text: issue.into(),
        status: "approved".into(),
        reason: Some(format!("reason-{id}")),
        similarity: 0.91,
        created_at: "2026-04-10T00:00:00Z".into(),
        signature: None,
        source_pr_number: None,
        source_pr_title: None,
        source_pr_url: None,
    }
}

/// Past-verdict injection: header+entries land in dynamic suffix and
/// appear BEFORE the current diff. None / empty must NOT emit the
/// header, preserving byte-equivalent prompts and cache hits.
#[test]
fn segmented_prompt_past_verdict_injection() {
    let verdicts = vec![
        pv("e1", "value.unwrap()", "unwrap can panic"),
        pv("e2", "println!(\"x\")", "debug print"),
    ];
    let seg = build_segmented_prompt(
        None,
        &[],
        "--- a\n+++ b\n+some change\n",
        "",
        None,
        Some(&verdicts),
    );
    assert!(
        seg.dynamic_suffix
            .contains("## Past verdicts on similar code")
    );
    assert!(seg.dynamic_suffix.contains("value.unwrap()"));
    let verdict_pos = seg.dynamic_suffix.find("## Past verdicts").unwrap();
    let diff_pos = seg.dynamic_suffix.find("## Current Diff").unwrap();
    assert!(verdict_pos < diff_pos, "past verdicts must precede diff");
    assert!(!seg.stable_prefix.contains("Past verdicts"));

    // None / empty: no header, preserving byte-equivalence.
    for verds in [None, Some(&Vec::new()[..])] {
        let s = build_segmented_prompt(None, &[], "+change\n", "", None, verds);
        assert!(!s.dynamic_suffix.contains("Past verdicts"));
    }
}

/// Reassembled segmented prompt MUST equal the flat prompt for every
/// perspective when no extras are supplied.
#[test]
fn segmented_equals_legacy_when_reassembled() {
    for perspective in [
        None,
        Some(ReviewPerspective::Safety),
        Some(ReviewPerspective::Performance),
        Some(ReviewPerspective::Style),
        Some(ReviewPerspective::Docs),
        Some(ReviewPerspective::ApiDesign),
    ] {
        let legacy = build_system_prompt(perspective);
        let seg = build_segmented_prompt(perspective, &[], "", "", None, None);
        let reassembled = format!("{}{}", seg.stable_prefix, seg.dynamic_suffix);
        assert_eq!(legacy, reassembled, "perspective {perspective:?}");
        assert!(seg.dynamic_suffix.is_empty());
    }
}

/// Stable-prefix invariants for the prompt cache:
/// 1. Same team inputs + different diffs → identical `stable_prefix`.
/// 2. Adding/editing a rule → `stable_prefix` changes (no stale cache).
/// 3. Reordering rule slice → `stable_prefix` unchanged (sorted by id).
#[test]
fn stable_prefix_cache_invariants() {
    let rules = vec![
        rule("no-any", "Disallow `any`."),
        rule("no-todo", "No TODOs."),
    ];
    let a = build_segmented_prompt(
        Some(ReviewPerspective::Safety),
        &rules,
        "diff A",
        "",
        None,
        None,
    );
    let b = build_segmented_prompt(
        Some(ReviewPerspective::Safety),
        &rules,
        "diff B different",
        "instructions",
        None,
        None,
    );
    assert_eq!(a.stable_prefix, b.stable_prefix);
    assert_ne!(a.dynamic_suffix, b.dynamic_suffix);

    // Adding a rule changes the prefix.
    let single = build_segmented_prompt(
        Some(ReviewPerspective::Safety),
        &[rule("no-any", "Disallow `any`.")],
        "",
        "",
        None,
        None,
    );
    assert_ne!(single.stable_prefix, a.stable_prefix);

    // Reordering does NOT change the prefix.
    let shuffled = build_segmented_prompt(
        Some(ReviewPerspective::Safety),
        &[
            rule("no-todo", "No TODOs."),
            rule("no-any", "Disallow `any`."),
        ],
        "diff A",
        "",
        None,
        None,
    );
    assert_eq!(a.stable_prefix, shuffled.stable_prefix);

    // Editing rule content changes the prefix (avoids stale cache).
    let edited = build_segmented_prompt(
        Some(ReviewPerspective::Safety),
        &[rule("no-any", "Disallow `any` EVERYWHERE.")],
        "",
        "",
        None,
        None,
    );
    assert_ne!(single.stable_prefix, edited.stable_prefix);
}

// Merge / dedup invariants

#[test]
fn merge_dedupes_and_picks_highest_severity() {
    // Same (file,line,rule_id) flagged at info/warning/error across
    // three perspectives in worst→best order. Result: one issue, with
    // severity "error" and canonically-ordered perspectives — proves
    // severity wins over insertion order.
    let mk = |sev: &str| issue(sev, "r", Some("r1"), Some("f.rs"), Some(1));
    let merged = merge_perspective_issues(vec![
        (ReviewPerspective::Safety, vec![mk("info")]),
        (ReviewPerspective::Performance, vec![mk("warning")]),
        (ReviewPerspective::Style, vec![mk("error")]),
    ]);
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].severity, "error");
    assert_eq!(
        merged[0].perspectives,
        vec!["safety", "performance", "style"]
    );
}

#[test]
fn merge_falls_back_to_rule_when_rule_id_missing() {
    // Without rule_id, dedup key uses `rule` instead — invariant prevents
    // a flood of duplicate findings when the LLM omits ruleId.
    let a = issue("info", "naming", None, Some("lib.rs"), Some(5));
    let b = issue("info", "naming", None, Some("lib.rs"), Some(5));
    let merged = merge_perspective_issues(vec![
        (ReviewPerspective::Style, vec![a]),
        (ReviewPerspective::Safety, vec![b]),
    ]);
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].perspectives, vec!["safety", "style"]);
}

#[test]
fn merge_canonical_perspective_order_under_shuffled_input() {
    // All five perspectives flag the same issue, fed in shuffled order.
    // Output must be canonically ordered regardless of insertion order.
    let mk = |sev: &str| issue(sev, "shared", Some("s1"), Some("lib.rs"), Some(99));
    let merged = merge_perspective_issues(vec![
        (ReviewPerspective::Docs, vec![mk("info")]),
        (ReviewPerspective::ApiDesign, vec![mk("warning")]),
        (ReviewPerspective::Safety, vec![mk("error")]),
        (ReviewPerspective::Style, vec![mk("info")]),
        (ReviewPerspective::Performance, vec![mk("warning")]),
    ]);
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].severity, "error");
    assert_eq!(
        merged[0].perspectives,
        vec!["safety", "performance", "style", "docs", "api_design"]
    );
}

#[test]
fn merge_preserves_first_seen_order_across_perspectives() {
    // Distinct issues across perspectives must keep insertion order
    // (not sort by hash key) — caller relies on this for stable display.
    let s1 = issue("error", "bounds-check", Some("s1"), Some("a.rs"), Some(10));
    let s2 = issue(
        "warning",
        "panic-unwrap",
        Some("s2"),
        Some("a.rs"),
        Some(20),
    );
    let p1 = issue("info", "clone-in-loop", Some("p1"), Some("z.rs"), Some(5));
    let merged = merge_perspective_issues(vec![
        (ReviewPerspective::Safety, vec![s1, s2]),
        (ReviewPerspective::Performance, vec![p1]),
        (ReviewPerspective::Style, vec![]),
    ]);
    assert_eq!(merged.len(), 3);
    assert_eq!(merged[0].rule, "bounds-check");
    assert_eq!(merged[1].rule, "panic-unwrap");
    assert_eq!(merged[2].rule, "clone-in-loop");
}

#[test]
fn severity_rank_ordering() {
    assert!(severity_rank("error") > severity_rank("warning"));
    assert!(severity_rank("warning") > severity_rank("info"));
    assert!(severity_rank("info") > severity_rank("unknown"));
}

// Verify pass and summary.

struct StubLlm {
    response: std::sync::Mutex<StubLlmResponse>,
}

enum StubLlmResponse {
    Ok(String),
    Err(String),
}
impl StubLlm {
    fn ok(s: &str) -> Self {
        Self {
            response: std::sync::Mutex::new(StubLlmResponse::Ok(s.to_owned())),
        }
    }
    fn err(s: &str) -> Self {
        Self {
            response: std::sync::Mutex::new(StubLlmResponse::Err(s.to_owned())),
        }
    }
}
#[async_trait::async_trait]
impl ReviewLlm for StubLlm {
    async fn chat(&self, _system_prompt: &str, _user_prompt: &str) -> crate::Result<String> {
        match &*self.response.lock().unwrap() {
            StubLlmResponse::Ok(s) => Ok(s.clone()),
            StubLlmResponse::Err(e) => Err(CoreError::Internal(e.clone())),
        }
    }
}

#[tokio::test]
async fn verify_pass_drops_low_confidence_keeps_others() {
    let issues = vec![
        issue("error", "a", Some("r-a"), Some("a.rs"), Some(1)),
        issue("warning", "b", Some("r-b"), Some("b.rs"), Some(2)),
        issue("info", "c", Some("r-c"), Some("c.rs"), Some(3)),
    ];
    let stub = StubLlm::ok(
        r#"[
                {"id":0,"confidence":0.9,"verdict":"keep","reason":""},
                {"id":1,"confidence":0.2,"verdict":"drop","reason":"fp"},
                {"id":2,"confidence":0.55,"verdict":"keep","reason":""}
            ]"#,
    );
    let out = verify_pass(&stub, true, "diff", issues).await;
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].rule, "a");
    assert!((out[0].confidence - 0.9).abs() < 1e-5);
    assert_eq!(out[1].rule, "c");
    assert!((out[1].confidence - 0.55).abs() < 1e-5);
}

#[tokio::test]
async fn verify_pass_returns_unchanged_on_failure_modes() {
    // Three failure modes must all leave issues + confidence untouched:
    // garbage parse, provider error, and the disable flag.
    let issues = vec![
        issue("error", "a", Some("r-a"), Some("a.rs"), Some(1)),
        issue("warning", "b", Some("r-b"), Some("b.rs"), Some(2)),
    ];

    // Parser failure.
    let out = verify_pass(&StubLlm::ok("not json"), true, "diff", issues.clone()).await;
    assert_eq!(out.len(), 2);
    assert!((out[0].confidence - 1.0).abs() < 1e-5);

    // Provider error.
    let out = verify_pass(&StubLlm::err("upstream 500"), true, "diff", issues.clone()).await;
    assert_eq!(out.len(), 2);
    assert!((out[0].confidence - 1.0).abs() < 1e-5);

    // Flag disabled — even a "drop" response is ignored.
    let stub = StubLlm::ok(r#"[{"id":0,"confidence":0.1,"verdict":"drop","reason":""}]"#);
    let out = verify_pass(&stub, false, "diff", issues).await;
    assert_eq!(out.len(), 2);
    assert!((out[0].confidence - 1.0).abs() < 1e-5);
}

#[tokio::test]
async fn verify_pass_keeps_originals_when_verifier_drops_everything() {
    let issues = vec![
        issue("warning", "a", Some("r-a"), Some("a.rs"), Some(1)),
        issue("warning", "b", Some("r-b"), Some("b.rs"), Some(2)),
    ];
    let stub = StubLlm::ok(
        r#"[
                {"id":0,"confidence":0.1,"verdict":"drop","reason":"too strict"},
                {"id":1,"confidence":0.2,"verdict":"drop","reason":"too strict"}
            ]"#,
    );

    let out = verify_pass(&stub, true, "diff", issues).await;
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].rule, "a");
    assert_eq!(out[1].rule, "b");
}

#[tokio::test]
async fn review_summary_parses_and_counts_blocking() {
    let issues = vec![
        issue("error", "a", None, Some("a.rs"), Some(1)),
        issue("critical", "b", None, Some("b.rs"), Some(2)),
        issue("warning", "c", None, Some("c.rs"), Some(3)),
        issue("info", "d", None, Some("d.rs"), Some(4)),
    ];
    let stub = StubLlm::ok(
        r#"{
                "oneLineSummary": "Refactor error handling",
                "walkthroughByFile": [
                    {"file": "a.rs", "intent": "x"},
                    {"file": "b.rs", "intent": "y"}
                ]
            }"#,
    );
    let out = run_review_summary(&stub, true, "diff", &issues)
        .await
        .unwrap();
    assert_eq!(out.one_line_summary, "Refactor error handling");
    assert_eq!(out.walkthrough_by_file.len(), 2);
    assert_eq!(out.blocking_count, 2); // error + critical
    assert_eq!(out.non_blocking_count, 2); // warning + info
}

#[tokio::test]
async fn review_summary_returns_none_on_failures_or_disabled() {
    let issues = vec![issue("error", "a", None, Some("a.rs"), Some(1))];
    // Provider error.
    assert!(
        run_review_summary(&StubLlm::err("500"), true, "diff", &issues)
            .await
            .is_none()
    );
    // Parser failure.
    assert!(
        run_review_summary(&StubLlm::ok("not json"), true, "diff", &issues)
            .await
            .is_none()
    );
    // Flag disabled.
    let stub = StubLlm::ok(r#"{"oneLineSummary":"x","walkthroughByFile":[]}"#);
    assert!(
        run_review_summary(&stub, false, "diff", &issues)
            .await
            .is_none()
    );
}

#[test]
fn count_blocking_splits_error_critical_from_rest() {
    let issues = vec![
        issue("error", "a", None, None, None),
        issue("critical", "b", None, None, None),
        issue("warning", "c", None, None, None),
        issue("info", "d", None, None, None),
        issue("unknown", "e", None, None, None),
    ];
    let (blocking, non_blocking) = count_blocking(&issues);
    assert_eq!(blocking, 2);
    assert_eq!(non_blocking, 3);
}

#[test]
fn collect_diff_files_dedupes_and_preserves_order() {
    let diff = "--- a/src/a.rs\n+++ b/src/a.rs\n+x\n--- a/src/b.rs\n+++ b/src/b.rs\n+y\n--- a/src/a.rs\n+++ b/src/a.rs\n+z\n";
    let files = collect_diff_files(diff);
    assert_eq!(files, vec!["src/a.rs".to_owned(), "src/b.rs".to_owned()]);
}

// Diff context packing.

fn diff_context_file<'a>(
    path: &'a str,
    patch: &'a str,
    relevance: u16,
    change: DiffContextFileChange,
) -> DiffContextFile<'a> {
    DiffContextFile {
        path,
        patch,
        relevance,
        change,
    }
}

#[test]
fn diff_context_pack_includes_all_when_budget_allows() {
    let files = vec![
        diff_context_file(
            "src/a.rs",
            "diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1 +1 @@\n-old\n+new\n",
            20,
            DiffContextFileChange::Modified,
        ),
        diff_context_file(
            "src/b.rs",
            "diff --git a/src/b.rs b/src/b.rs\n--- a/src/b.rs\n+++ b/src/b.rs\n@@ -2 +2 @@\n-before\n+after\n",
            10,
            DiffContextFileChange::Modified,
        ),
    ];

    let packed = pack_diff_context(
        &files,
        DiffContextOptions {
            char_budget: Some(10_000),
            mode: DiffContextMode::ReviewExtraction,
        },
    );

    assert_eq!(packed.summaries, Vec::new());
    assert_eq!(packed.included_files.len(), 2);
    assert!(packed.included_files.iter().all(|file| !file.truncated));
    assert!(packed.text.contains("## File: src/a.rs"));
    assert!(packed.text.contains("## File: src/b.rs"));
    assert!(packed.packed_chars <= 10_000);
}

#[test]
fn diff_context_pack_truncates_large_file_to_key_patch_context() {
    let large_patch = "diff --git a/src/large.rs b/src/large.rs\n\
index 1111111..2222222 100644\n\
--- a/src/large.rs\n\
+++ b/src/large.rs\n\
@@ -1,8 +1,8 @@\n\
 fn important() {\n\
-    call_old_dependency();\n\
+    call_new_dependency();\n\
     finish();\n\
 }\n\
 context one\n\
 context two\n\
 context three\n\
 context four\n";
    let files = vec![diff_context_file(
        "src/large.rs",
        large_patch,
        99,
        DiffContextFileChange::Modified,
    )];

    let packed = pack_diff_context(
        &files,
        DiffContextOptions {
            char_budget: Some(260),
            mode: DiffContextMode::ReviewExtraction,
        },
    );

    assert_eq!(packed.included_files.len(), 1);
    assert!(packed.included_files[0].truncated);
    assert_eq!(packed.summaries.len(), 1);
    assert_eq!(
        packed.summaries[0].reason,
        DiffContextSummaryReason::TruncatedForBudget
    );
    assert!(packed.text.contains("## File: src/large.rs"));
    assert!(packed.text.contains("+    call_new_dependency();"));
    assert!(packed.text.contains("... [diff context truncated]"));
    assert!(packed.packed_chars <= 260);
}

#[test]
fn diff_context_pack_sorts_by_relevance_then_smaller_files() {
    let high_large = "diff --git a/src/high_large.rs b/src/high_large.rs\n\
--- a/src/high_large.rs\n\
+++ b/src/high_large.rs\n\
@@ -1,5 +1,5 @@\n\
 context a\n\
 context b\n\
-old high\n\
+new high\n\
 context c\n";
    let high_small = "diff --git a/src/high_small.rs b/src/high_small.rs\n\
--- a/src/high_small.rs\n\
+++ b/src/high_small.rs\n\
@@ -1 +1 @@\n\
-x\n\
+y\n";
    let low_tiny = "diff --git a/src/low_tiny.rs b/src/low_tiny.rs\n\
--- a/src/low_tiny.rs\n\
+++ b/src/low_tiny.rs\n\
@@ -1 +1 @@\n\
-a\n\
+b\n";
    let files = vec![
        diff_context_file(
            "src/low_tiny.rs",
            low_tiny,
            1,
            DiffContextFileChange::Modified,
        ),
        diff_context_file(
            "src/high_large.rs",
            high_large,
            50,
            DiffContextFileChange::Modified,
        ),
        diff_context_file(
            "src/high_small.rs",
            high_small,
            50,
            DiffContextFileChange::Modified,
        ),
    ];

    let packed = pack_diff_context(
        &files,
        DiffContextOptions {
            char_budget: Some(10_000),
            mode: DiffContextMode::ReviewExtraction,
        },
    );
    let paths: Vec<_> = packed
        .included_files
        .iter()
        .map(|file| file.path.as_str())
        .collect();

    assert_eq!(
        paths,
        vec!["src/high_small.rs", "src/high_large.rs", "src/low_tiny.rs"]
    );
}

#[test]
fn diff_context_pack_summarizes_deleted_and_oversized_files() {
    let deleted_patch = "diff --git a/src/old.rs b/src/old.rs\n\
deleted file mode 100644\n\
--- a/src/old.rs\n\
+++ /dev/null\n\
@@ -1,2 +0,0 @@\n\
-old line\n\
-other old line\n";
    let huge_patch = "diff --git a/src/huge.rs b/src/huge.rs\n\
--- a/src/huge.rs\n\
+++ b/src/huge.rs\n\
@@ -1,3 +1,3 @@\n\
-old very long changed line that cannot fit in the tiny budget\n\
+new very long changed line that cannot fit in the tiny budget\n";
    let files = vec![
        diff_context_file(
            "src/old.rs",
            deleted_patch,
            100,
            DiffContextFileChange::Modified,
        ),
        diff_context_file(
            "src/huge.rs",
            huge_patch,
            90,
            DiffContextFileChange::Modified,
        ),
    ];

    let packed = pack_diff_context(
        &files,
        DiffContextOptions {
            char_budget: Some(20),
            mode: DiffContextMode::FixPr,
        },
    );

    assert!(packed.included_files.is_empty());
    assert_eq!(packed.summaries.len(), 2);
    assert!(
        packed
            .summaries
            .iter()
            .any(|summary| summary.path == "src/old.rs"
                && summary.reason == DiffContextSummaryReason::DeletedFile)
    );
    assert!(
        packed
            .summaries
            .iter()
            .any(|summary| summary.path == "src/huge.rs"
                && summary.reason == DiffContextSummaryReason::OmittedForBudget)
    );
}

#[test]
fn parse_verify_response_accepts_code_block() {
    let raw = "Here:\n```json\n[{\"id\":0,\"confidence\":0.8,\"verdict\":\"keep\"}]\n```";
    let map = parse_verify_response(raw).expect("parse");
    let (conf, keep) = map.get(&0).copied().unwrap();
    assert!((conf - 0.8).abs() < 1e-5);
    assert!(keep);
}

#[test]
fn parse_summary_response_tolerates_noise() {
    let raw = "Sure:\n{\"oneLineSummary\":\"Fix bug\",\"walkthroughByFile\":[{\"file\":\"a.rs\",\"intent\":\"x\"}]}\n";
    let (line, walk) = parse_summary_response(raw).expect("parse");
    assert_eq!(line, "Fix bug");
    assert_eq!(walk.len(), 1);
    assert_eq!(walk[0].file, "a.rs");
}

#[test]
fn parse_summary_response_returns_none_on_malformed_json() {
    // Documented contract: malformed input returns None instead of
    // panicking. Pins down each of the three extraction tiers
    // (direct / fenced / brace-scan) bailing out cleanly.
    assert!(parse_summary_response("").is_none());
    assert!(parse_summary_response("not even json").is_none());
    assert!(parse_summary_response("{not: valid, json}").is_none());
    assert!(parse_summary_response("```json\n{broken\n```").is_none());
    // Valid JSON but wrong shape (array, not object) — must also bail.
    assert!(parse_summary_response("[1, 2, 3]").is_none());
}

#[test]
fn parse_verify_response_returns_none_on_malformed_json() {
    // Same contract for the verify-pass parser. Callers fall back to
    // "keep everything unchanged" when this returns None — a panic
    // here would tear down the whole review pipeline.
    assert!(parse_verify_response("").is_none());
    assert!(parse_verify_response("not even json").is_none());
    assert!(parse_verify_response("[broken").is_none());
    assert!(parse_verify_response("```json\n[broken\n```").is_none());
    // Object instead of expected array — must bail.
    assert!(parse_verify_response("{\"id\": 0}").is_none());
}

#[test]
fn telemetry_extracts_past_verdict_count_from_trajectory() {
    // Cloud telemetry reads PastVerdictsRecalled.count regardless of
    // its position in the trajectory — must keep working when other
    // step kinds are inserted before it.
    let mut traj = TrajectoryBuilder::new();
    traj.push(TrajectoryStep::ChunksRetrieved {
        count: 4,
        symbols: vec!["a".into()],
        similarity_scores: vec![],
    });
    traj.push(TrajectoryStep::PastVerdictsRecalled {
        count: 3,
        top_similarities: vec![0.9, 0.8],
        recalled_items: vec![],
    });
    traj.push(TrajectoryStep::FinalDecision {
        issue_ids_emitted: vec!["i1".into()],
    });

    let count = traj.steps().iter().find_map(|step| match step {
        TrajectoryStep::PastVerdictsRecalled { count, .. } => Some(*count as u32),
        _ => None,
    });
    assert_eq!(count, Some(3));

    // Missing step → None (cloud leaves the column unchanged).
    let mut empty = TrajectoryBuilder::new();
    empty.push(TrajectoryStep::FinalDecision {
        issue_ids_emitted: vec![],
    });
    let missing = empty.steps().iter().find_map(|step| match step {
        TrajectoryStep::PastVerdictsRecalled { count, .. } => Some(*count as u32),
        _ => None,
    });
    assert_eq!(missing, None);
}
