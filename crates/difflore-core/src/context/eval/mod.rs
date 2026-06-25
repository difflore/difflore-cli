//! Golden-case retrieval eval — precision / recall + forbidden-exclusion
//! scoring over a fixed, committed fixture.
//!
//! This is the complement to `difflore eval` (self-recall). Self-recall asks
//! the corpus to find a rule from a distillation of its OWN text — an
//! optimistic upper bound. Golden eval asks a *paraphrased agent query* to
//! recall the RIGHT rule, rank it above unrelated ones, and stay quiet on a
//! doc-only edit. It is the guardrail to run BEFORE changing ranking: a
//! ranking change that regresses precision/recall here should fail CI.
//!
//! Layering: the engine is corpus-agnostic and tempfile-free. Callers build an
//! isolated index from [`golden_rules_to_documents`] +
//! `index_db::upsert_rule_chunks_isolated`, then hand the pool to
//! [`score_golden_cases`]. The committed smoke corpus is embedded as
//! [`GOLDEN_SMOKE_FIXTURE`] so `difflore eval --golden` is offline and
//! deterministic.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::CoreError;
use crate::context::retrieval::{self, RuleSearchRetrievalOptions};
use crate::context::rule_source::RuleDocument;
use crate::domain::glob_match::{GlobErrorPolicy, glob_match};

/// The committed smoke fixture — the single source of truth shared with the
/// `rag_eval_seed_fixture_keeps_minimum_contract` contract test. Embedded via
/// `include_str!` (from `tests/fixtures/`, which ships in the published crate)
/// so the eval is self-contained and offline. Keep this path in sync if the
/// fixture ever moves.
pub const GOLDEN_SMOKE_FIXTURE: &str =
    include_str!("../../../tests/fixtures/rag-eval-seed-cases.json");

/// Rank cutoff at which precision / recall / forbidden-exclusion are scored.
/// The smoke corpus is tiny (5 rules), so the discriminating signal is the
/// *ordering* of the top few, not whether a rule appears at all — at `top_k`
/// equal to the corpus size every rule is trivially "recalled".
pub const GOLDEN_K: usize = 3;

/// A golden fixture: a small rule corpus plus the cases that probe it.
#[derive(Debug, Clone, Deserialize)]
pub struct GoldenFixture {
    pub rules: Vec<GoldenRule>,
    pub cases: Vec<GoldenCase>,
}

/// One rule in the golden corpus. Mirrors the public rule shape (id / title /
/// body / file globs / source repo).
#[derive(Debug, Clone, Deserialize)]
pub struct GoldenRule {
    pub id: String,
    pub title: String,
    pub body: String,
    #[serde(default, rename = "filePatterns")]
    pub file_patterns: Vec<String>,
    #[serde(default, rename = "sourceRepo")]
    pub source_repo: Option<String>,
}

/// One probe: a paraphrased query plus the rule ids that SHOULD and MUST-NOT be
/// recalled. An empty `expected_rule_ids` marks an abstention case (a doc-only
/// edit that should recall nothing).
#[derive(Debug, Clone, Deserialize)]
pub struct GoldenCase {
    pub id: String,
    pub query: String,
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default, rename = "expectedRuleIds")]
    pub expected_rule_ids: Vec<String>,
    #[serde(default, rename = "forbiddenRuleIds")]
    pub forbidden_rule_ids: Vec<String>,
}

/// Per-case scored outcome.
#[derive(Debug, Clone, Serialize)]
pub struct GoldenCaseResult {
    pub case_id: String,
    /// Number of rules the case expects (0 = abstention case).
    pub expected: usize,
    /// 1-based rank of the first expected rule in the top-[`GOLDEN_K`] window.
    pub first_relevant_rank: Option<usize>,
    /// Fraction of expected rules present in the top-[`GOLDEN_K`]. `None` for
    /// abstention cases (no expected set to recall).
    pub recall_at_k: Option<f64>,
    /// Fraction of the top-[`GOLDEN_K`] slots filled by an expected rule.
    /// `None` for abstention cases.
    pub precision_at_k: Option<f64>,
    /// Count of forbidden rules that leaked into the top-[`GOLDEN_K`].
    pub forbidden_hits: usize,
    /// For positive cases with a file: whether every expected rule that was
    /// recalled in the top-[`GOLDEN_K`] has a file glob matching the case file.
    pub strict_file_match: Option<bool>,
    /// For abstention cases: whether the top-[`GOLDEN_K`] correctly stayed clear
    /// of every forbidden rule.
    pub abstained_correctly: Option<bool>,
    /// The top-ranked rule id, if any (handy when debugging a miss).
    pub top_rule: Option<String>,
}

/// Aggregated golden-eval report.
#[derive(Debug, Clone, Serialize)]
pub struct GoldenReport {
    pub k: usize,
    pub total_cases: usize,
    pub positive_cases: usize,
    pub negative_cases: usize,
    /// Mean recall@k over positive cases.
    pub mean_recall_at_k: f64,
    /// Mean precision@k over positive cases.
    pub mean_precision_at_k: f64,
    /// Mean reciprocal rank@k over positive cases (0 when a case recalls nothing in top-k).
    pub mean_reciprocal_rank: f64,
    /// Total forbidden rules that leaked into the top-k of a POSITIVE case —
    /// the precision-regression signal. Should stay 0.
    pub positive_forbidden_hits: usize,
    /// Negative (abstention) cases that correctly recalled no forbidden rule.
    pub negative_clean: usize,
    /// Positive cases whose recalled expected rules all matched the case file.
    pub strict_file_correct: usize,
    /// Positive cases for which a strict-file judgement was possible.
    pub strict_file_total: usize,
    pub cases: Vec<GoldenCaseResult>,
}

/// Parse a golden fixture from JSON.
pub fn parse_golden_fixture(json: &str) -> Result<GoldenFixture, CoreError> {
    Ok(serde_json::from_str(json)?)
}

/// Turn golden rules into indexable [`RuleDocument`]s. Title and body are
/// indexed together so a paraphrased query can match body terms. Scope is left
/// unset: the smoke corpus is cross-repo and [`score_golden_cases`] queries
/// with no repo filter, so ranking is purely relevance-driven.
#[must_use]
pub fn golden_rules_to_documents(fixture: &GoldenFixture) -> Vec<RuleDocument> {
    fixture
        .rules
        .iter()
        .map(|rule| {
            let file_patterns = if rule.file_patterns.is_empty() {
                None
            } else {
                serde_json::to_string(&rule.file_patterns).ok()
            };
            RuleDocument {
                skill_id: rule.id.clone(),
                title: rule.title.clone(),
                content: format!("{}\n\n{}", rule.title, rule.body),
                confidence: 1.0,
                file_patterns,
                language: None,
                repo_scope: None,
            }
        })
        .collect()
}

/// Run every case in `fixture` against an already-populated isolated
/// `index_pool` and score it. The caller is responsible for having built the
/// index from [`golden_rules_to_documents`] (keeps this engine tempfile-free).
pub async fn score_golden_cases(
    index_pool: &crate::SqlitePool,
    fixture: &GoldenFixture,
    top_k: usize,
) -> Result<GoldenReport, CoreError> {
    // Map rule id -> its file-pattern JSON blob for strict-file scoring.
    let file_patterns: std::collections::HashMap<&str, Option<String>> = fixture
        .rules
        .iter()
        .map(|rule| {
            let blob = if rule.file_patterns.is_empty() {
                None
            } else {
                serde_json::to_string(&rule.file_patterns).ok()
            };
            (rule.id.as_str(), blob)
        })
        .collect();

    let mut results = Vec::with_capacity(fixture.cases.len());
    for case in &fixture.cases {
        let hits = retrieval::retrieve_rules_for_search(
            index_pool,
            RuleSearchRetrievalOptions {
                query: &case.query,
                lexical_query: &case.query,
                top_k,
                confidence_map: None,
                age_days_map: None,
                effectiveness_map: None,
                target_scope: None,
                repo_scopes: &[],
                ann_enabled: false,
                local_query_embedding: false,
                embedding_timeout: None,
                cold_start_retry: false,
                adaptive_prune: false,
            },
        )
        .await?;

        // De-dupe to a ranked list of unique rule ids (a rule can surface as
        // more than one chunk; the first, highest-scoring occurrence wins).
        let mut ranked: Vec<String> = Vec::new();
        for hit in &hits {
            if !ranked.iter().any(|id| id == &hit.skill_id) {
                ranked.push(hit.skill_id.clone());
            }
        }

        results.push(score_one_case(case, &ranked, &file_patterns));
    }

    Ok(aggregate(results))
}

/// Score a single case against its de-duped ranked rule ids.
fn score_one_case(
    case: &GoldenCase,
    ranked: &[String],
    file_patterns: &std::collections::HashMap<&str, Option<String>>,
) -> GoldenCaseResult {
    let expected: BTreeSet<&str> = case.expected_rule_ids.iter().map(String::as_str).collect();
    let forbidden: BTreeSet<&str> = case.forbidden_rule_ids.iter().map(String::as_str).collect();

    let cutoff = GOLDEN_K.min(ranked.len());
    let top: Vec<&str> = ranked.iter().take(cutoff).map(String::as_str).collect();

    let first_relevant_rank = top
        .iter()
        .position(|id| expected.contains(*id))
        .map(|pos| pos + 1);

    let expected_in_top = top.iter().filter(|id| expected.contains(*id)).count();
    let forbidden_hits = top.iter().filter(|id| forbidden.contains(*id)).count();

    let (recall_at_k, precision_at_k, strict_file_match, abstained_correctly) =
        if expected.is_empty() {
            // Abstention case: no expected set, so recall/precision do not apply.
            // "Abstained" means the reranked path recalled NOTHING in the top-k
            // — a true abstention, independent of how many rules the fixture
            // marked forbidden (a doc-only edit should surface no rule at all).
            (None, None, None, Some(top.is_empty()))
        } else {
            let recall = expected_in_top as f64 / expected.len() as f64;
            let precision = if top.is_empty() {
                0.0
            } else {
                expected_in_top as f64 / top.len() as f64
            };
            let strict = case.file.as_deref().map(|file| {
                // Every expected rule recalled in the top-k should have a file glob
                // that actually matches the edited file.
                top.iter().filter(|id| expected.contains(*id)).all(|id| {
                    let blob = file_patterns.get(id).and_then(Option::as_deref);
                    glob_match(blob, file, GlobErrorPolicy::OverRecall)
                })
            });
            (Some(recall), Some(precision), strict, None)
        };

    GoldenCaseResult {
        case_id: case.id.clone(),
        expected: expected.len(),
        first_relevant_rank,
        recall_at_k,
        precision_at_k,
        forbidden_hits,
        strict_file_match,
        abstained_correctly,
        top_rule: ranked.first().cloned(),
    }
}

/// Aggregate per-case results into a [`GoldenReport`].
fn aggregate(cases: Vec<GoldenCaseResult>) -> GoldenReport {
    let total_cases = cases.len();
    let positive: Vec<&GoldenCaseResult> = cases.iter().filter(|c| c.expected > 0).collect();
    let positive_cases = positive.len();
    let negative_cases = total_cases - positive_cases;

    let mean = |sum: f64, n: usize| if n == 0 { 0.0 } else { sum / n as f64 };

    let recall_sum: f64 = positive.iter().filter_map(|c| c.recall_at_k).sum();
    let precision_sum: f64 = positive.iter().filter_map(|c| c.precision_at_k).sum();
    let rr_sum: f64 = positive
        .iter()
        .map(|c| c.first_relevant_rank.map_or(0.0, |rank| 1.0 / rank as f64))
        .sum();

    let positive_forbidden_hits = positive.iter().map(|c| c.forbidden_hits).sum();
    let negative_clean = cases
        .iter()
        .filter(|c| c.abstained_correctly == Some(true))
        .count();
    let strict_file_total = positive
        .iter()
        .filter(|c| c.strict_file_match.is_some())
        .count();
    let strict_file_correct = positive
        .iter()
        .filter(|c| c.strict_file_match == Some(true))
        .count();

    GoldenReport {
        k: GOLDEN_K,
        total_cases,
        positive_cases,
        negative_cases,
        mean_recall_at_k: mean(recall_sum, positive_cases),
        mean_precision_at_k: mean(precision_sum, positive_cases),
        mean_reciprocal_rank: mean(rr_sum, positive_cases),
        positive_forbidden_hits,
        negative_clean,
        strict_file_correct,
        strict_file_total,
        cases,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn golden_case(expected_rule_ids: Vec<&str>) -> GoldenCase {
        GoldenCase {
            id: "case-1".to_owned(),
            query: "query".to_owned(),
            file: None,
            expected_rule_ids: expected_rule_ids.into_iter().map(str::to_owned).collect(),
            forbidden_rule_ids: Vec::new(),
        }
    }

    #[test]
    fn score_one_case_does_not_credit_mrr_past_top_k() {
        let ranked = ["a", "b", "c", "target"]
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let result = score_one_case(&golden_case(vec!["target"]), &ranked, &HashMap::new());

        assert_eq!(result.first_relevant_rank, None);
        assert_eq!(result.recall_at_k, Some(0.0));
        assert_eq!(result.precision_at_k, Some(0.0));

        let report = aggregate(vec![result]);
        assert!(report.mean_reciprocal_rank.abs() < f64::EPSILON);
    }
}
