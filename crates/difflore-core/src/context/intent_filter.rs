//! Re-rank a list of retrieved rules by token overlap with the PR's
//! intent (title, file paths, headline diff lines), then cap to top N.
//!
//! A cheap intent-token-overlap secondary score (Jaccard-like) trims a
//! file-pattern-matched candidate set (often hundreds of rules, mostly
//! noise) down to the few a human reviewer would actually flag.

use std::collections::HashSet;

use super::types::ContextSourceItemRecord;

const STOP: &[&str] = &[
    "a", "an", "and", "or", "but", "the", "in", "on", "of", "to", "for", "with", "from", "by",
    "is", "are", "was", "were", "be", "been", "this", "that", "these", "those", "it", "its", "as",
    "at", "if", "when", "fix", "feat", "chore", "refactor", "test", "docs", "use", "add", "set",
    "not", "via", "all", "any", "new", "old",
];

/// Tokenise a string into lowercased ASCII alphanumeric words ≥2
/// chars, dropping a small stopword list. The set is deliberately
/// short — this is a cheap heuristic, not embedding similarity.
pub fn tokenise(s: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut cur = String::new();
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            cur.push(ch);
        } else if !cur.is_empty() {
            push_token(&mut out, &cur);
            cur.clear();
        }
    }
    if !cur.is_empty() {
        push_token(&mut out, &cur);
    }
    out
}

fn push_token(out: &mut HashSet<String>, w: &str) {
    let normalized = w.to_ascii_lowercase();
    push_simple_token(out, &normalized);
    for part in camel_case_parts(w) {
        push_simple_token(out, &part.to_ascii_lowercase());
    }
}

fn push_simple_token(out: &mut HashSet<String>, w: &str) {
    if w.len() < 2 || !w.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) {
        return;
    }
    if STOP.contains(&w) {
        return;
    }
    out.insert(w.to_owned());
}

fn camel_case_parts(w: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let chars = w.char_indices().collect::<Vec<_>>();
    for idx in 1..chars.len() {
        let (byte_idx, ch) = chars[idx];
        let prev = chars[idx - 1].1;
        let next = chars.get(idx + 1).map(|(_, next)| *next);
        let boundary = (prev.is_ascii_lowercase() && ch.is_ascii_uppercase())
            || (prev.is_ascii_digit() && ch.is_ascii_alphabetic())
            || (prev.is_ascii_alphabetic() && ch.is_ascii_digit())
            || (prev.is_ascii_uppercase()
                && ch.is_ascii_uppercase()
                && next.is_some_and(|next| next.is_ascii_lowercase()));
        if boundary {
            if start < byte_idx {
                parts.push(&w[start..byte_idx]);
            }
            start = byte_idx;
        }
    }
    if start < w.len() {
        parts.push(&w[start..]);
    }
    parts
}

/// Jaccard-flavoured overlap score: `|A ∩ B| / sqrt(|A| * |B|)`. Stays
/// in `[0, 1]`. Verbose rules don't dominate just by being long.
#[allow(
    clippy::implicit_hasher,
    reason = "stable public API; callers always pass the default-hash HashSet built by tokenise"
)]
pub fn overlap_score(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count();
    if inter == 0 {
        return 0.0;
    }
    inter as f64 / ((a.len() * b.len()) as f64).sqrt()
}

/// Re-rank `rules` in-place by combining their existing retrieval
/// `score` with PR-intent-overlap. Rules with zero intent overlap
/// keep their order at the bottom. Optionally cap to `top_n`.
///
/// The returned vector is the same data, sorted by combined score
/// descending, optionally truncated.
pub fn rerank_by_intent(
    mut rules: Vec<ContextSourceItemRecord>,
    intent_text: &str,
    top_n: Option<usize>,
) -> Vec<ContextSourceItemRecord> {
    let intent_tokens = tokenise(intent_text);
    if intent_tokens.is_empty() {
        if let Some(n) = top_n {
            rules.truncate(n);
        }
        return rules;
    }
    // Pre-compute combined score for stable sorting.
    let mut scored: Vec<(f64, ContextSourceItemRecord)> = rules
        .into_iter()
        .map(|r| {
            let rule_text = format!("{} {}", r.title.as_deref().unwrap_or(""), r.content);
            let rule_tokens = tokenise(&rule_text);
            let intent_match = overlap_score(&intent_tokens, &rule_tokens);
            // Combined score: existing retrieval score boosted by intent
            // overlap. We multiply rather than add so a zero intent
            // overlap doesn't drown out the retrieval signal entirely
            // (we add a tiny floor to keep ordering stable for zero
            // overlap cases).
            let combined = (r.score.max(0.0)) * (intent_match + 0.001);
            (combined, r)
        })
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    let mut out: Vec<ContextSourceItemRecord> = scored.into_iter().map(|(_, r)| r).collect();
    if let Some(n) = top_n {
        out.truncate(n);
    }
    out
}

/// One audit run's persisted view: which rules matched and which earned a
/// top-N slot. Append-only to `~/.difflore/audit-history.jsonl`; rules that
/// consistently land in the noise bucket across PRs are the strongest pruning
/// candidates.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AuditRunRecord {
    pub ts_ms: i64,
    pub project_id: String,
    pub scope: String,
    /// Every rule that matched on `file_patterns`.
    pub matched: Vec<String>,
    /// The top-N relevant subset surfaced after intent rerank.
    pub top: Vec<String>,
    /// `matched - top` — the noise bucket for this single run.
    pub noise: Vec<String>,
}

/// Aggregate stats for one rule across N audit runs. Used by
/// `aggregate_audit_history` to produce the cross-run noise ranking.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AggregatedRuleStat {
    pub rule_id: String,
    /// How many runs this rule matched at all.
    pub matched: usize,
    /// How many runs this rule earned a top-N slot.
    pub top: usize,
    /// How many runs this rule was in the noise bucket
    /// (matched but not top-N).
    pub noise: usize,
}

/// Walk the run records (newest first or arbitrary order — we don't
/// rely on order) and produce per-rule aggregate counts. Pure
/// function — no I/O. Caller is responsible for reading the JSONL.
pub fn aggregate_audit_history(runs: &[AuditRunRecord]) -> Vec<AggregatedRuleStat> {
    use std::collections::BTreeMap;
    let mut by_rule: BTreeMap<String, AggregatedRuleStat> = BTreeMap::new();
    for run in runs {
        for id in &run.matched {
            by_rule
                .entry(id.clone())
                .or_insert(AggregatedRuleStat {
                    rule_id: id.clone(),
                    matched: 0,
                    top: 0,
                    noise: 0,
                })
                .matched += 1;
        }
        for id in &run.top {
            if let Some(s) = by_rule.get_mut(id) {
                s.top += 1;
            }
        }
        for id in &run.noise {
            if let Some(s) = by_rule.get_mut(id) {
                s.noise += 1;
            }
        }
    }
    by_rule.into_values().collect()
}

/// Build the intent string a review caller passes to retrieval and
/// `maybe_rerank_for_review`. We use `file_path` plus the first few
/// semantically interesting diff lines: hunk headers and actual changed
/// `+`/`-` lines, skipping file headers. Looking only at the first few
/// non-blank lines can stop before the actual edit on ordinary unified
/// diffs, which makes rules about the changed API invisible to recall.
pub fn build_review_intent_text(file_path: Option<&str>, diff_content: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(p) = file_path {
        if !p.is_empty() {
            parts.push(p.to_owned());
        }
    }

    let structural_hints = diff_structural_hints(diff_content);
    if !structural_hints.is_empty() {
        parts.push(structural_hints.join("\n"));
    }

    let changed: Vec<&str> = diff_content
        .lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            trimmed.starts_with("@@")
                || ((trimmed.starts_with('+') || trimmed.starts_with('-'))
                    && !trimmed.starts_with("+++")
                    && !trimmed.starts_with("---"))
        })
        .take(24)
        .collect();

    let lines: Vec<&str> = if changed.is_empty() {
        diff_content
            .lines()
            .filter(|line| {
                let trimmed = line.trim();
                !trimmed.is_empty()
                    && !trimmed.starts_with("diff --git")
                    && !trimmed.starts_with("index ")
                    && !trimmed.starts_with("+++")
                    && !trimmed.starts_with("---")
            })
            .take(24)
            .collect()
    } else {
        changed
    };

    if !lines.is_empty() {
        parts.push(lines.join("\n"));
    }
    parts.join("\n")
}

fn diff_structural_hints(diff_content: &str) -> Vec<&'static str> {
    let mut hints = Vec::new();
    let mut seen = HashSet::new();
    let mut previous_added_blockquote = false;
    let mut blank_after_added_blockquote = false;

    for raw in diff_content.lines() {
        let trimmed = raw.trim_start();
        let is_added = trimmed.starts_with('+') && !trimmed.starts_with("+++");
        if !is_added {
            previous_added_blockquote = false;
            blank_after_added_blockquote = false;
            continue;
        }

        let body = &trimmed[1..];
        let body_trimmed = body.trim_start();
        let is_blockquote = body_trimmed.starts_with('>');
        if is_blockquote {
            push_hint(
                &mut hints,
                &mut seen,
                "markdown blockquote no-blanks-blockquote",
            );
            if blank_after_added_blockquote {
                push_hint(
                    &mut hints,
                    &mut seen,
                    "blank line between blockquotes markdownlint MD028",
                );
            }
        }
        if previous_added_blockquote && body.trim().is_empty() {
            blank_after_added_blockquote = true;
        } else if !is_blockquote {
            blank_after_added_blockquote = false;
        }
        previous_added_blockquote = is_blockquote;

        let fence = body_trimmed.trim();
        if fence == "```" || fence == "~~~" {
            push_hint(
                &mut hints,
                &mut seen,
                "markdown fenced code block missing language tag MD040",
            );
        }
        if body.contains("uses:")
            && (body.contains("@v")
                || body.contains("@main")
                || body.contains("@master")
                || body.contains("@latest"))
        {
            push_hint(
                &mut hints,
                &mut seen,
                "github actions workflow uses mutable ref pin commit sha",
            );
        }
    }

    hints
}

fn push_hint(hints: &mut Vec<&'static str>, seen: &mut HashSet<&'static str>, hint: &'static str) {
    if seen.insert(hint) {
        hints.push(hint);
    }
}

/// Read the env var that controls intent-rerank for review. Returns the
/// cap (top N) the caller should apply, or `None` to bypass rerank.
///
/// Env var values:
///   - unset                              → on with cap = `DEFAULT_RERANK_TOP_N`
///   - `"0"` / `"false"` / `""`           → off (return None)
///   - `"1"` / `"true"` / `"on"`          → on with explicit cap of 8
///   - any positive integer N              → on with cap = N
///
/// On by default because a bounded, reranked rule pack is less noisy and
/// cheaper than injecting every candidate. Users who want the full pack can
/// set `DIFFLORE_INTENT_RERANK=0`.
pub fn rerank_review_top_n_from_env() -> Option<usize> {
    let Some(raw) = crate::infra::env::var(crate::infra::env::DIFFLORE_INTENT_RERANK) else {
        return Some(DEFAULT_RERANK_TOP_N);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "0" || trimmed.eq_ignore_ascii_case("false") {
        return None;
    }
    if trimmed == "1" || trimmed.eq_ignore_ascii_case("true") || trimmed.eq_ignore_ascii_case("on")
    {
        return Some(8);
    }
    trimmed.parse::<usize>().ok().filter(|n| *n > 0)
}

/// Default cap when `DIFFLORE_INTENT_RERANK` is unset.
pub const DEFAULT_RERANK_TOP_N: usize = 5;

/// If `DIFFLORE_INTENT_RERANK` is set, return a (`rule_context`,
/// `rules_text`) pair re-ranked by intent and joined for the LLM. Else
/// return `None` so the caller keeps the existing pack untouched.
///
/// `rule_context` is the original pack's items (re-ranked, capped).
/// `rules_text` is `Some(joined_content)` when the rerank produced any
/// items, else `None`.
pub fn maybe_rerank_for_review(
    rule_context: &[ContextSourceItemRecord],
    intent_text: &str,
) -> Option<(Vec<ContextSourceItemRecord>, Option<String>)> {
    let top_n = rerank_review_top_n_from_env()?;
    let reranked = rerank_by_intent(rule_context.to_vec(), intent_text, Some(top_n));
    let joined = if reranked.is_empty() {
        None
    } else {
        Some(
            reranked
                .iter()
                .map(|r| r.content.clone())
                .collect::<Vec<_>>()
                .join("\n\n"),
        )
    };
    Some((reranked, joined))
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::float_cmp,
    reason = "test scaffolding asserts exact scores produced by deterministic helpers"
)]
mod tests {
    use super::*;

    fn rule(title: &str, content: &str, score: f64) -> ContextSourceItemRecord {
        ContextSourceItemRecord {
            source_type: "rule".into(),
            source_id: title.to_owned(),
            relative_path: None,
            start_line: None,
            end_line: None,
            title: Some(title.to_owned()),
            content: content.to_owned(),
            score,
        }
    }

    #[test]
    fn tokenise_drops_stopwords_and_short() {
        let t = tokenise("Fix the HMR patch a config in workflow");
        assert!(t.contains("hmr"));
        assert!(t.contains("patch"));
        assert!(t.contains("config"));
        assert!(t.contains("workflow"));
        assert!(!t.contains("fix"), "stopword");
        assert!(!t.contains("the"), "stopword");
        assert!(!t.contains("a"), "too short");
    }

    #[test]
    fn tokenise_splits_camelcase_file_terms() {
        let t = tokenise(
            "packages/vite/src/node/server/environments/fullBundleEnvironment.ts \
             rejectNoCorsRequest StatusContinue",
        );
        assert!(t.contains("full"));
        assert!(t.contains("bundle"));
        assert!(t.contains("environment"));
        assert!(t.contains("reject"));
        assert!(t.contains("cors"));
        assert!(t.contains("request"));
        assert!(t.contains("status"));
        assert!(t.contains("continue"));
        assert!(t.contains("fullbundleenvironment"));
    }

    #[test]
    fn overlap_score_is_zero_for_disjoint() {
        let a = tokenise("hmr security cors");
        let b = tokenise("changeset bump version");
        assert_eq!(overlap_score(&a, &b), 0.0);
    }

    #[test]
    fn overlap_score_is_positive_for_shared_term() {
        let a = tokenise("HMR security cors origin");
        let b = tokenise("origin trustworthy reject patch HMR");
        assert!(overlap_score(&a, &b) > 0.0);
    }

    #[test]
    fn review_intent_includes_changed_lines_not_just_diff_headers() {
        let diff = r"diff --git a/recovery.go b/recovery.go
index bbf1d56..e518541 100644
--- a/recovery.go
+++ b/recovery.go
@@ -63,9 +63,9 @@ func CustomRecoveryWithWriter(out io.Writer, handle RecoveryFunc) HandlerFunc {
                var isBrokenPipe bool
                err, ok := rec.(error)
                if ok {
-                   isBrokenPipe = errors.Is(err, syscall.EPIPE) ||
-                       errors.Is(err, syscall.ECONNRESET) ||
-                       errors.Is(err, http.ErrAbortHandler)
+                   isBrokenPipe = err == syscall.EPIPE ||
+                       err == syscall.ECONNRESET ||
+                       err == http.ErrAbortHandler
                }
";
        let intent = build_review_intent_text(Some("recovery.go"), diff);
        assert!(intent.contains("recovery.go"));
        assert!(intent.contains("errors.Is"));
        assert!(intent.contains("ErrAbortHandler"));
        assert!(!intent.contains("diff --git"));
        assert!(!intent.contains("+++ b/recovery.go"));
    }

    #[test]
    fn review_intent_adds_markdown_blockquote_structure_hints() {
        let diff = r"diff --git a/README.md b/README.md
index 111..222 100644
--- a/README.md
+++ b/README.md
@@ -1,2 +1,5 @@
 # Docs
+> First warning.
+
+> Second warning.
";
        let intent = build_review_intent_text(Some("README.md"), diff);

        assert!(intent.contains("README.md"));
        assert!(intent.contains("markdown blockquote"));
        assert!(intent.contains("blank line between blockquotes"));
        assert!(intent.contains("MD028"));
    }

    #[test]
    fn review_intent_adds_syntax_hints_for_common_low_text_diffs() {
        let diff = r"diff --git a/.github/workflows/pr.yml b/.github/workflows/pr.yml
--- a/.github/workflows/pr.yml
+++ b/.github/workflows/pr.yml
@@ -1,3 +1,7 @@
+      - uses: actions/checkout@v4
+      - run: echo ok
+```
+hello
+```
";
        let intent = build_review_intent_text(Some(".github/workflows/pr.yml"), diff);

        assert!(intent.contains("github actions workflow uses mutable ref"));
        assert!(intent.contains("markdown fenced code block missing language tag"));
    }

    #[test]
    fn form_2086_real_world_fixture_surfaces_workflow_rules_first() {
        // Reproduces the cloud-bot scenario for form/#2086 (CI workflow
        // update). Out of 5 candidate rules, the 3 actually-relevant
        // workflow ones must outrank a random Go-style + a test rule.
        let intent = "ci: update github workflows pr.yml release.yml";
        let candidates = vec![
            rule(
                "Format Go structs explicitly",
                "Use a dedicated formatter before passing Go structs to %s.",
                0.5,
            ),
            rule(
                "Pin GitHub Actions refs to SHAs",
                "Always pin uses entries in workflows to immutable commit SHAs, never floating refs like main.",
                0.5,
            ),
            rule(
                "Update GitHub Actions versions atomically",
                "When upgrading actions versions, update all occurrences across workflow files in one PR.",
                0.5,
            ),
            rule(
                "Pin Release Actions",
                "Pin GitHub Actions in release workflows to immutable full commit SHAs when workflows can publish packages.",
                0.5,
            ),
            rule(
                "Use spec.ts for runtime tests",
                "Runtime tests must use spec.ts; type-level tests must use test-d.ts.",
                0.5,
            ),
        ];
        let ranked = rerank_by_intent(candidates, intent, Some(3));
        assert_eq!(ranked.len(), 3);
        for r in &ranked {
            let title = r.title.as_deref().unwrap_or("");
            assert!(
                title.contains("Pin") || title.contains("GitHub Actions"),
                "expected workflow rule in top 3, got: {title}",
            );
        }
    }

    #[test]
    fn empty_intent_just_truncates_existing_order() {
        let candidates = vec![
            rule("rule A", "content a", 0.9),
            rule("rule B", "content b", 0.8),
            rule("rule C", "content c", 0.7),
        ];
        let ranked = rerank_by_intent(candidates, "", Some(2));
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].title.as_deref(), Some("rule A"));
        assert_eq!(ranked[1].title.as_deref(), Some("rule B"));
    }

    #[test]
    fn rules_with_zero_overlap_sink_to_bottom_not_dropped() {
        let intent = "hmr security workflow";
        let candidates = vec![
            rule("Disjoint rule", "completely unrelated text", 1.0),
            rule("HMR rule", "about hmr workflow security", 0.5),
        ];
        let ranked = rerank_by_intent(candidates, intent, None);
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].title.as_deref(), Some("HMR rule"));
        assert_eq!(ranked[1].title.as_deref(), Some("Disjoint rule"));
    }

    fn with_env<F: FnOnce()>(value: Option<&str>, f: F) {
        temp_env::with_var("DIFFLORE_INTENT_RERANK", value, f);
    }

    #[test]
    fn env_gate_table() {
        // Default-on with DEFAULT_RERANK_TOP_N when unset. Falsy strings
        // mean "off"; truthy non-numeric strings use the legacy cap of 8;
        // explicit positive integers win.
        let cases: &[(Option<&str>, Option<usize>)] = &[
            (None, Some(DEFAULT_RERANK_TOP_N)),
            (Some("0"), None),
            (Some("false"), None),
            (Some(""), None),
            (Some("1"), Some(8)),
            (Some("true"), Some(8)),
            (Some("on"), Some(8)),
            (Some("3"), Some(3)),
            (Some("20"), Some(20)),
        ];
        for (val, expected) in cases {
            with_env(*val, || {
                assert_eq!(rerank_review_top_n_from_env(), *expected, "val: {val:?}");
            });
        }
    }

    #[test]
    fn default_rerank_top_n_is_5() {
        // Keep this pinned unless rerank evaluation justifies a new default.
        assert_eq!(DEFAULT_RERANK_TOP_N, 5);
    }

    #[test]
    fn maybe_rerank_returns_none_when_env_off() {
        with_env(Some("0"), || {
            let candidates = vec![rule("r", "c", 0.5)];
            assert!(maybe_rerank_for_review(&candidates, "intent").is_none());
        });
    }

    #[test]
    fn maybe_rerank_caps_to_default_when_env_unset() {
        with_env(None, || {
            // 8 candidates, default cap is 5 → expect 5 reranked.
            let candidates: Vec<_> = (0..8)
                .map(|i| rule(&format!("r{i}"), &format!("content {i}"), 0.5))
                .collect();
            let (reranked, joined) =
                maybe_rerank_for_review(&candidates, "intent").expect("default-on");
            assert_eq!(reranked.len(), DEFAULT_RERANK_TOP_N);
            assert!(joined.is_some());
        });
    }

    #[test]
    fn aggregate_audit_history_counts_matched_top_noise_per_rule() {
        let runs = vec![
            AuditRunRecord {
                ts_ms: 1,
                project_id: "p".into(),
                scope: "staged".into(),
                matched: vec!["A".into(), "B".into(), "C".into()],
                top: vec!["A".into()],
                noise: vec!["B".into(), "C".into()],
            },
            AuditRunRecord {
                ts_ms: 2,
                project_id: "p".into(),
                scope: "staged".into(),
                matched: vec!["A".into(), "B".into()],
                top: vec!["A".into()],
                noise: vec!["B".into()],
            },
            AuditRunRecord {
                ts_ms: 3,
                project_id: "p".into(),
                scope: "staged".into(),
                matched: vec!["B".into(), "D".into()],
                top: vec!["D".into()],
                noise: vec!["B".into()],
            },
        ];
        let stats = aggregate_audit_history(&runs);
        let by_id: std::collections::HashMap<_, _> =
            stats.into_iter().map(|s| (s.rule_id.clone(), s)).collect();
        // A: matched 2, top 2 — healthy
        assert_eq!(by_id["A"].matched, 2);
        assert_eq!(by_id["A"].top, 2);
        assert_eq!(by_id["A"].noise, 0);
        // B: matched 3, top 0, noise 3 — strongest pruning candidate
        assert_eq!(by_id["B"].matched, 3);
        assert_eq!(by_id["B"].top, 0);
        assert_eq!(by_id["B"].noise, 3);
        // D: matched 1, top 1 — healthy on small sample
        assert_eq!(by_id["D"].matched, 1);
        assert_eq!(by_id["D"].top, 1);
    }

    #[test]
    fn maybe_rerank_returns_some_capped_when_env_on() {
        with_env(Some("2"), || {
            let candidates = vec![
                rule("workflow rule", "pin actions in workflows", 0.5),
                rule("go style", "format structs", 0.5),
                rule("changeset rule", "patch bumps in changesets", 0.5),
            ];
            let result = maybe_rerank_for_review(&candidates, "ci workflow yaml");
            let (ranked, text) = result.expect("env on");
            assert_eq!(ranked.len(), 2);
            assert!(text.is_some());
            // workflow rule should be first (highest intent overlap)
            assert_eq!(ranked[0].title.as_deref(), Some("workflow rule"));
        });
    }
}
