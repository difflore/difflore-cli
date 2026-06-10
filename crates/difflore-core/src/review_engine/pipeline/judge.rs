//! LLM rule-applicability judge for the review pipeline.
//!
//! Recall-oriented retrieval surfaces rules that merely *mention* a diff
//! token even when their lesson is unrelated, leaking off-topic rules into
//! the review prompt. This optional step (gated by
//! `review_engine.rule_applicability_judge`, default off) asks the review
//! LLM, in one batched call, whether each candidate rule applies to the
//! diff, then drops the ones it judges non-applicable.
//!
//! Graceful degradation is the #1 invariant, mirroring
//! `validate::verify_pass`: on ANY failure (disabled, empty, LLM error,
//! parser error, or a response that would drop every rule) the candidate
//! pool comes back untouched. The judge can only narrow a pool, never
//! empty it.

use super::super::ReviewLlm;
use crate::context::types::ContextSourceItemRecord;
use std::collections::HashMap;

/// Per-rule verdict from the judge: whether the rule applies, plus a 0..1
/// relevance score used only for ordering/telemetry (the keep/drop
/// decision is `applies`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct JudgeVerdict {
    pub applies: bool,
    pub relevance: f32,
}

/// System prompt for the applicability judge. Strict and narrow so a cheap
/// model stays calibrated: relevance only, no invented findings, and "when
/// unsure, keep" (a recoverable review false positive beats dropping a rule
/// that did apply).
pub(super) const JUDGE_SYSTEM_PROMPT: &str = r#"You are a code-review rule-relevance judge. You are given a diff and a numbered list of candidate review rules. For EACH rule, decide whether that rule's lesson actually applies to THIS diff — i.e. the diff touches the kind of code, pattern, or concern the rule is about.

Return ONLY a JSON array. Each element must be an object:
{"id": <index>, "applies": true|false, "relevance": <float 0..1>, "reason": "<short>"}

Judge relevance only — do NOT report code issues, do NOT invent rules, do NOT rewrite the rule. A rule "applies" when the diff plausibly involves the rule's subject, even if the diff does not necessarily violate it. When you are unsure, set "applies": true (keep the rule). Mark "applies": false only when the rule is clearly about unrelated code or concerns.
Return the raw JSON array only, no markdown, no explanation."#;

/// One candidate rule flattened for the judge prompt, borrowing from the
/// `ContextSourceItemRecord` to avoid cloning the pool.
struct JudgeCandidate<'a> {
    title: &'a str,
    content: &'a str,
}

fn candidate_title(item: &ContextSourceItemRecord) -> &str {
    item.title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or(item.source_id.as_str())
}

/// Build the judge user-prompt: trimmed diff + candidate rules enumerated
/// with stable `id` indices for deterministic matching. Rule bodies are
/// length-capped so verbose rules can't blow the prompt budget.
fn build_judge_user_prompt(diff: &str, candidates: &[JudgeCandidate<'_>]) -> String {
    const DIFF_LIMIT: usize = 8_000;
    const RULE_BODY_LIMIT: usize = 600;

    let trimmed = if diff.len() > DIFF_LIMIT {
        &diff[..DIFF_LIMIT]
    } else {
        diff
    };

    let mut s = String::new();
    s.push_str("## Diff\n```diff\n");
    s.push_str(trimmed);
    s.push_str("\n```\n\n## Candidate rules\n");
    for (i, c) in candidates.iter().enumerate() {
        let body: String = c.content.trim().chars().take(RULE_BODY_LIMIT).collect();
        s.push_str(&format!(
            "- id: {i}\n  title: {}\n  rule: {body}\n",
            c.title
        ));
    }
    s
}

/// Parse a judge response into `{index → JudgeVerdict}`. Uses the same
/// three-tier extraction (direct → fenced block → bracket scan) as
/// `parse::parse_verify_response`. Returns `None` on structural failure;
/// an empty-but-valid array yields `Some(empty_map)`.
pub(super) fn parse_judge_response(text: &str) -> Option<HashMap<usize, JudgeVerdict>> {
    let arr: Vec<serde_json::Value> =
        if let Ok(v) = serde_json::from_str::<Vec<serde_json::Value>>(text.trim()) {
            v
        } else if let Some(start) = text.find("```") {
            let after = &text[start + 3..];
            let content_start = after.find('\n').map_or(0, |i| i + 1);
            let end = after[content_start..].find("```")?;
            let block = &after[content_start..content_start + end];
            serde_json::from_str::<Vec<serde_json::Value>>(block.trim()).ok()?
        } else if let (Some(start), Some(end)) = (text.find('['), text.rfind(']')) {
            if end <= start {
                return None;
            }
            serde_json::from_str::<Vec<serde_json::Value>>(&text[start..=end]).ok()?
        } else {
            return None;
        };

    let mut out = HashMap::new();
    for item in arr {
        let Some(obj) = item.as_object() else {
            continue;
        };
        let Some(id) = obj
            .get("id")
            .and_then(serde_json::Value::as_u64)
            .map(|n| n as usize)
        else {
            continue;
        };
        // Default to keep when the field is missing/garbled — enforces the
        // "when unsure, keep" contract at parse time.
        let applies = obj
            .get("applies")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);
        let relevance = obj
            .get("relevance")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(if applies { 1.0 } else { 0.0 }) as f32;
        out.insert(
            id,
            JudgeVerdict {
                applies,
                relevance: relevance.clamp(0.0, 1.0),
            },
        );
    }
    Some(out)
}

/// Filter `rules` (in pool order) by `verdicts` keyed on pool index,
/// preserving order. Contract:
/// * A rule the judge did NOT score is kept (never drop on silence).
/// * If the verdicts would drop every rule, the original pool is returned
///   unchanged — a review must never lose all its rules to one bad judge
///   call.
pub(super) fn judge_filter_rules(
    rules: Vec<ContextSourceItemRecord>,
    verdicts: &HashMap<usize, JudgeVerdict>,
) -> Vec<ContextSourceItemRecord> {
    if rules.is_empty() {
        return rules;
    }
    let kept: Vec<ContextSourceItemRecord> = rules
        .iter()
        .enumerate()
        .filter(|(idx, _)| verdicts.get(idx).is_none_or(|v| v.applies))
        .map(|(_, item)| item.clone())
        .collect();

    if kept.is_empty() {
        // Every candidate judged non-applicable — fall back to the full
        // pool rather than strip the review of all rules.
        return rules;
    }
    kept
}

/// Run the applicability judge over a recalled rule pool, returning the
/// possibly narrowed pool. When disabled, empty, on LLM error, or on an
/// unparseable response, the pool is returned untouched. Only drops rules
/// explicitly marked non-applicable, never the whole pool (see
/// [`judge_filter_rules`]).
pub(super) async fn run_applicability_judge(
    llm: &dyn ReviewLlm,
    enabled: bool,
    diff: &str,
    rules: Vec<ContextSourceItemRecord>,
) -> Vec<ContextSourceItemRecord> {
    if !enabled {
        return rules;
    }
    // A single rule (or none) isn't worth a round-trip: nothing to gain,
    // and dropping it would just hit the "never empty" fallback.
    if rules.len() < 2 {
        return rules;
    }
    if diff.is_empty() {
        return rules;
    }

    let candidates: Vec<JudgeCandidate<'_>> = rules
        .iter()
        .map(|item| JudgeCandidate {
            title: candidate_title(item),
            content: item.content.as_str(),
        })
        .collect();

    let user_prompt = build_judge_user_prompt(diff, &candidates);
    let response = match llm.chat(JUDGE_SYSTEM_PROMPT, &user_prompt).await {
        Ok(r) => r,
        Err(e) => {
            if crate::env::fix_debug() {
                eprintln!("[applicability_judge] judge call failed: {e:?}; keeping all rules");
            }
            return rules;
        }
    };

    let verdicts = match parse_judge_response(&response) {
        Some(map) if !map.is_empty() => map,
        _ => {
            if crate::env::fix_debug() {
                eprintln!(
                    "[applicability_judge] could not parse judge response; keeping all rules unchanged"
                );
            }
            return rules;
        }
    };

    let before = rules.len();
    let filtered = judge_filter_rules(rules, &verdicts);
    if crate::env::fix_debug() {
        eprintln!(
            "[applicability_judge] pool {before} -> {} after judge filter",
            filtered.len(),
        );
    }
    filtered
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::ReviewLlm;
    use std::sync::Mutex;

    fn rule(source_id: &str, title: &str, content: &str, score: f64) -> ContextSourceItemRecord {
        ContextSourceItemRecord {
            source_type: "rule".into(),
            source_id: source_id.into(),
            relative_path: None,
            start_line: None,
            end_line: None,
            title: Some(title.into()),
            content: content.into(),
            score,
        }
    }

    fn v(applies: bool, relevance: f32) -> JudgeVerdict {
        JudgeVerdict { applies, relevance }
    }

    // Mock provider (never calls a real LLM)

    /// A `ReviewLlm` that returns a canned response and records the prompts
    /// it was handed, so tests can assert the judge invoked the provider
    /// with the expected payload.
    struct MockReviewLlm {
        response: Result<String, ()>,
        calls: Mutex<Vec<(String, String)>>,
    }

    impl MockReviewLlm {
        fn ok(body: &str) -> Self {
            Self {
                response: Ok(body.to_owned()),
                calls: Mutex::new(Vec::new()),
            }
        }
        fn erroring() -> Self {
            Self {
                response: Err(()),
                calls: Mutex::new(Vec::new()),
            }
        }
        fn call_count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
        fn last_user_prompt(&self) -> Option<String> {
            self.calls.lock().unwrap().last().map(|(_, u)| u.clone())
        }
    }

    #[async_trait::async_trait]
    impl ReviewLlm for MockReviewLlm {
        async fn chat(&self, system: &str, user: &str) -> crate::Result<String> {
            self.calls
                .lock()
                .unwrap()
                .push((system.to_owned(), user.to_owned()));
            self.response
                .clone()
                .map_err(|()| crate::errors::CoreError::Internal("mock failure".into()))
        }
    }

    // parse_judge_response

    #[test]
    fn parse_direct_json_array() {
        let map = parse_judge_response(
            r#"[{"id":0,"applies":true,"relevance":0.9},{"id":1,"applies":false,"relevance":0.1}]"#,
        )
        .unwrap();
        assert_eq!(map.get(&0).copied(), Some(v(true, 0.9)));
        assert_eq!(map.get(&1).copied(), Some(v(false, 0.1)));
    }

    #[test]
    fn parse_fenced_code_block() {
        let text = "Here you go:\n```json\n[{\"id\":0,\"applies\":false,\"relevance\":0.2}]\n```\n";
        let map = parse_judge_response(text).unwrap();
        assert_eq!(map.get(&0).copied(), Some(v(false, 0.2)));
    }

    #[test]
    fn parse_bracket_scan_with_prose_around() {
        let text = "I think the answer is [{\"id\":2,\"applies\":true,\"relevance\":0.7}] overall.";
        let map = parse_judge_response(text).unwrap();
        assert_eq!(map.get(&2).copied(), Some(v(true, 0.7)));
    }

    #[test]
    fn parse_missing_applies_defaults_to_keep() {
        // "when unsure, keep" contract enforced at parse time.
        let map = parse_judge_response(r#"[{"id":0,"relevance":0.5}]"#).unwrap();
        assert_eq!(map.get(&0).copied(), Some(v(true, 0.5)));
    }

    #[test]
    fn parse_missing_relevance_derives_from_applies() {
        let map =
            parse_judge_response(r#"[{"id":0,"applies":false},{"id":1,"applies":true}]"#).unwrap();
        assert_eq!(map.get(&0).copied(), Some(v(false, 0.0)));
        assert_eq!(map.get(&1).copied(), Some(v(true, 1.0)));
    }

    #[test]
    fn parse_clamps_out_of_range_relevance() {
        let map = parse_judge_response(r#"[{"id":0,"applies":true,"relevance":4.2}]"#).unwrap();
        assert!((map.get(&0).unwrap().relevance - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn parse_garbage_returns_none() {
        assert!(parse_judge_response("not json at all").is_none());
        assert!(parse_judge_response("").is_none());
    }

    #[test]
    fn parse_empty_array_is_some_empty_map() {
        let map = parse_judge_response("[]").unwrap();
        assert!(map.is_empty());
    }

    #[test]
    fn parse_skips_entries_without_id() {
        let map = parse_judge_response(r#"[{"applies":false},{"id":1,"applies":true}]"#).unwrap();
        assert!(!map.contains_key(&0));
        assert_eq!(map.get(&1).copied(), Some(v(true, 1.0)));
    }

    // judge_filter_rules (pure core)

    #[test]
    fn filter_drops_only_non_applicable_rules() {
        let rules = vec![
            rule("a", "Rule A", "body a", 0.9),
            rule("b", "Rule B", "body b", 0.8),
            rule("c", "Rule C", "body c", 0.7),
        ];
        let mut verdicts = HashMap::new();
        verdicts.insert(0, v(true, 0.9)); // keep
        verdicts.insert(1, v(false, 0.1)); // drop
        verdicts.insert(2, v(true, 0.6)); // keep

        let out = judge_filter_rules(rules, &verdicts);
        let ids: Vec<&str> = out.iter().map(|r| r.source_id.as_str()).collect();
        assert_eq!(ids, vec!["a", "c"], "only the non-applicable rule b drops");
    }

    #[test]
    fn filter_keeps_rules_the_judge_did_not_score() {
        // Model returned a verdict for index 0 only; indices 1,2 are silent
        // and must be kept (never drop on silence).
        let rules = vec![
            rule("a", "Rule A", "body a", 0.9),
            rule("b", "Rule B", "body b", 0.8),
            rule("c", "Rule C", "body c", 0.7),
        ];
        let mut verdicts = HashMap::new();
        verdicts.insert(0, v(false, 0.0)); // explicit drop

        let out = judge_filter_rules(rules, &verdicts);
        let ids: Vec<&str> = out.iter().map(|r| r.source_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["b", "c"],
            "unscored rules survive, scored drop applies"
        );
    }

    #[test]
    fn filter_preserves_pool_order() {
        let rules = vec![
            rule("a", "A", "x", 0.5),
            rule("b", "B", "y", 0.5),
            rule("c", "C", "z", 0.5),
            rule("d", "D", "w", 0.5),
        ];
        let mut verdicts = HashMap::new();
        verdicts.insert(1, v(false, 0.0)); // drop b
        let out = judge_filter_rules(rules, &verdicts);
        let ids: Vec<&str> = out.iter().map(|r| r.source_id.as_str()).collect();
        assert_eq!(ids, vec!["a", "c", "d"]);
    }

    #[test]
    fn filter_never_returns_empty_pool_when_all_dropped() {
        // Backward-safety: a verdict set that drops every rule must yield the
        // ORIGINAL pool, not an empty review context.
        let rules = vec![rule("a", "A", "x", 0.5), rule("b", "B", "y", 0.5)];
        let mut verdicts = HashMap::new();
        verdicts.insert(0, v(false, 0.0));
        verdicts.insert(1, v(false, 0.0));
        let out = judge_filter_rules(rules, &verdicts);
        let ids: Vec<&str> = out.iter().map(|r| r.source_id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b"], "all-dropped falls back to full pool");
    }

    #[test]
    fn filter_empty_input_is_empty() {
        let out = judge_filter_rules(Vec::new(), &HashMap::new());
        assert!(out.is_empty());
    }

    // run_applicability_judge (end-to-end with mock)

    #[tokio::test]
    async fn judge_disabled_is_noop_and_makes_no_call() {
        let llm = MockReviewLlm::ok("[]");
        let rules = vec![rule("a", "A", "x", 0.5), rule("b", "B", "y", 0.5)];
        let out = run_applicability_judge(&llm, false, "some diff", rules.clone()).await;
        assert_eq!(out.len(), rules.len());
        assert_eq!(llm.call_count(), 0, "disabled must not hit the provider");
    }

    #[tokio::test]
    async fn judge_filters_pool_on_valid_response() {
        let llm = MockReviewLlm::ok(
            r#"[{"id":0,"applies":true,"relevance":0.9},{"id":1,"applies":false,"relevance":0.1},{"id":2,"applies":true,"relevance":0.8}]"#,
        );
        let rules = vec![
            rule("keep1", "Keep 1", "relevant body", 0.9),
            rule("drop", "Drop", "irrelevant body", 0.8),
            rule("keep2", "Keep 2", "relevant body 2", 0.7),
        ];
        let out = run_applicability_judge(&llm, true, "diff body", rules).await;
        let ids: Vec<&str> = out.iter().map(|r| r.source_id.as_str()).collect();
        assert_eq!(ids, vec!["keep1", "keep2"]);
        assert_eq!(llm.call_count(), 1);
    }

    #[tokio::test]
    async fn judge_keeps_all_on_llm_error() {
        let llm = MockReviewLlm::erroring();
        let rules = vec![
            rule("a", "A", "x", 0.5),
            rule("b", "B", "y", 0.5),
            rule("c", "C", "z", 0.5),
        ];
        let out = run_applicability_judge(&llm, true, "diff", rules.clone()).await;
        assert_eq!(out.len(), rules.len(), "LLM error => untouched pool");
    }

    #[tokio::test]
    async fn judge_keeps_all_on_unparseable_response() {
        let llm = MockReviewLlm::ok("the model rambled and returned no json");
        let rules = vec![rule("a", "A", "x", 0.5), rule("b", "B", "y", 0.5)];
        let out = run_applicability_judge(&llm, true, "diff", rules.clone()).await;
        assert_eq!(out.len(), rules.len(), "parse failure => untouched pool");
    }

    #[tokio::test]
    async fn judge_skips_single_rule_pool() {
        // One rule => nothing to filter; skip the round-trip entirely.
        let llm = MockReviewLlm::ok(r#"[{"id":0,"applies":false}]"#);
        let rules = vec![rule("only", "Only", "x", 0.9)];
        let out = run_applicability_judge(&llm, true, "diff", rules).await;
        assert_eq!(out.len(), 1);
        assert_eq!(
            llm.call_count(),
            0,
            "single-rule pool must not call provider"
        );
    }

    #[tokio::test]
    async fn judge_skips_empty_diff() {
        let llm = MockReviewLlm::ok(r#"[{"id":0,"applies":false}]"#);
        let rules = vec![rule("a", "A", "x", 0.5), rule("b", "B", "y", 0.5)];
        let out = run_applicability_judge(&llm, true, "", rules).await;
        assert_eq!(out.len(), 2);
        assert_eq!(llm.call_count(), 0, "empty diff must not call provider");
    }

    #[tokio::test]
    async fn judge_all_dropped_falls_back_to_full_pool() {
        let llm = MockReviewLlm::ok(
            r#"[{"id":0,"applies":false,"relevance":0.0},{"id":1,"applies":false,"relevance":0.0}]"#,
        );
        let rules = vec![rule("a", "A", "x", 0.5), rule("b", "B", "y", 0.5)];
        let out = run_applicability_judge(&llm, true, "diff", rules).await;
        let ids: Vec<&str> = out.iter().map(|r| r.source_id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b"], "all-dropped => full pool, never empty");
    }

    #[tokio::test]
    async fn judge_prompt_enumerates_rules_with_indices() {
        let llm = MockReviewLlm::ok("[]"); // empty -> unparseable-ish (empty map) -> noop
        let rules = vec![
            rule(
                "a",
                "Pin Actions to SHAs",
                "Always pin GitHub Actions.",
                0.9,
            ),
            rule("b", "Avoid unwrap", "Do not unwrap in library code.", 0.8),
        ];
        // Empty array parses to an empty map -> treated as "could not use" ->
        // pool returned untouched, but the CALL still happened with our prompt.
        let _ = run_applicability_judge(&llm, true, "diff text here", rules).await;
        assert_eq!(llm.call_count(), 1);
        let prompt = llm.last_user_prompt().unwrap();
        assert!(prompt.contains("- id: 0"), "rule 0 enumerated");
        assert!(prompt.contains("- id: 1"), "rule 1 enumerated");
        assert!(
            prompt.contains("Pin Actions to SHAs"),
            "title carried into prompt"
        );
        assert!(
            prompt.contains("diff text here"),
            "diff carried into prompt"
        );
    }
}
