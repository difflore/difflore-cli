//! `difflore eval` — a fast, repeatable self-recall sanity check.
//!
//! IMPORTANT: this is a SELF-RECALL probe, NOT real-world recall. Each query is
//! the rule's OWN first ~8 significant words, so the corpus is being asked to
//! find a rule from (a distillation of) its own text. That is an OPTIMISTIC
//! UPPER BOUND on retrieval quality — useful as a fast, offline, deterministic
//! sanity check that the index/embedder/rerank are wired up and ranking the
//! obvious case, but it overstates how well DiffLore recalls a rule from a
//! *paraphrase* (the real agent query). Real-world paraphrase recall needs
//! separate task-query evaluation; this command only checks the local
//! self-match path.
//!
//! Until this module, the doctor self-recall check measured the WRONG path: it
//! called raw `retrieve_rules_with_confidence`, while every real recall surface
//! (`recall`, `fix`, MCP `search_rules`, the hook hot path) applies the lexical
//! and strict-file re-rank on top, so the reported numbers understated what the
//! self-recall query itself would surface. This module measures self-recall
//! through `retrieve_rules_for_search` — the exact reranked path the agent uses
//! — so the upper bound is computed over the real ranking pipeline. Both
//! `difflore eval` and the doctor section call [`measure_self_recall`], so the
//! metric can never drift between the two (part of the public eval-seed contract).
//!
//! `difflore eval` builds the measurement index in an isolated `TempDir` with
//! the local lexical (SHA1) embedder, so it is deterministic, offline, fast,
//! and leaves the user's real per-project indexes untouched. SHA1 is also the
//! zero-setup mode every new user starts in (and the effective mode whenever
//! the cloud embedder is paused), so this is the recall quality that matters
//! most for the "first fired rule" goal.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use difflore_core::context::retrieval::{self, RuleSearchRetrievalOptions};
use difflore_core::context::rule_source::RuleDocument;
use difflore_core::context::{index_db, rule_source};

use crate::runtime::CommandContext;
use crate::style::{self, sym};

/// Stop-words dropped when distilling a rule's body into a self-recall query.
/// Kept identical to the historical doctor list so numbers stay comparable.
const STOP: &[&str] = &[
    "the", "a", "an", "and", "or", "of", "to", "for", "in", "on", "at", "by", "with", "when",
    "use", "using", "as", "is", "are", "be", "this", "that", "from", "into", "do", "not", "should",
    "must", "via", "than", "then", "but", "if", "else",
];

/// Default number of rules sampled per run.
const DEFAULT_SAMPLES: usize = 20;

/// One self-recall probe: query the index with `query` and check whether
/// `skill_id`'s own rule comes back in the top-K.
pub(crate) struct SelfRecallSample {
    pub skill_id: String,
    pub query: String,
    pub language: Option<String>,
}

/// Aggregated self-recall outcome. Per-language tuple is `(n, @1, @5, rr_sum)`.
#[derive(Default)]
pub(crate) struct SelfRecallReport {
    pub tested: usize,
    pub hits_at_1: usize,
    pub hits_at_5: usize,
    pub reciprocal_rank_sum: f64,
    pub per_lang: BTreeMap<String, (usize, usize, usize, f64)>,
}

impl SelfRecallReport {
    pub fn at5_pct(&self) -> f64 {
        pct(self.hits_at_5, self.tested)
    }
    pub fn at1_pct(&self) -> f64 {
        pct(self.hits_at_1, self.tested)
    }
    /// Mean reciprocal rank. Misses contribute 0 (divided by `tested`, not by
    /// the hit count) so a corpus that ranks the right rule first scores above
    /// one that merely lands it in the top-5.
    pub fn mrr(&self) -> f64 {
        if self.tested == 0 {
            0.0
        } else {
            self.reciprocal_rank_sum / self.tested as f64
        }
    }
}

fn pct(hits: usize, total: usize) -> f64 {
    if total == 0 {
        0.0
    } else {
        (hits as f64 / total as f64) * 100.0
    }
}

/// Distil a rule's indexed content into its self-recall query: the first 8
/// significant words of the rule body (the text after the `Rule ID:/Name:/…`
/// header block), with stop-words removed. Falls back to the whole content
/// when there is no header/body split.
pub(crate) fn self_recall_query(content: &str) -> String {
    let body = content.split_once("\n\n").map_or(content, |(_, rest)| rest);
    let mut out: Vec<&str> = Vec::new();
    for word in body.split_whitespace() {
        let trimmed = word.trim_matches(|c: char| !c.is_alphanumeric());
        if trimmed.is_empty() || STOP.contains(&trimmed.to_ascii_lowercase().as_str()) {
            continue;
        }
        out.push(word);
        if out.len() >= 8 {
            break;
        }
    }
    out.join(" ")
}

/// Deterministic stride sampling over the active corpus: the same N rules every
/// run when the corpus is unchanged, evenly spread across the list.
pub(crate) fn build_samples(rules: &[RuleDocument], n_target: usize) -> Vec<SelfRecallSample> {
    if rules.is_empty() || n_target == 0 {
        return Vec::new();
    }
    let step = rules.len().div_ceil(n_target).max(1);
    rules
        .iter()
        .step_by(step)
        .take(n_target)
        .filter_map(|rule| {
            let query = self_recall_query(&rule.content);
            (!query.is_empty()).then(|| SelfRecallSample {
                skill_id: rule.skill_id.clone(),
                query,
                language: rule.language.clone(),
            })
        })
        .collect()
}

/// Measure self-recall against `index_pool` through the REAL reranked search
/// path (`retrieve_rules_for_search`). This is the shared metric used by both
/// `difflore eval` and the doctor report.
pub(crate) async fn measure_self_recall(
    index_pool: &difflore_core::SqlitePool,
    samples: &[SelfRecallSample],
    embedding_timeout: Option<Duration>,
) -> SelfRecallReport {
    let mut report = SelfRecallReport::default();
    for sample in samples {
        let lang_key = sample.language.as_deref().unwrap_or("(unknown)").to_owned();
        let entry = report.per_lang.entry(lang_key).or_insert((0, 0, 0, 0.0));
        entry.0 += 1;
        report.tested += 1;

        let Ok(hits) = retrieval::retrieve_rules_for_search(
            index_pool,
            RuleSearchRetrievalOptions {
                query: &sample.query,
                lexical_query: &sample.query,
                top_k: 5,
                confidence_map: None,
                age_days_map: None,
                target_file: None,
                repo_scopes: &[],
                ann_enabled: false,
                embedding_timeout,
                cold_start_retry: false,
                adaptive_prune: false,
            },
        )
        .await
        else {
            continue;
        };

        if let Some(pos) = hits.iter().position(|h| h.skill_id == sample.skill_id) {
            report.hits_at_5 += 1;
            entry.2 += 1;
            let reciprocal_rank = 1.0 / (pos as f64 + 1.0);
            report.reciprocal_rank_sum += reciprocal_rank;
            entry.3 += reciprocal_rank;
            if pos == 0 {
                report.hits_at_1 += 1;
                entry.1 += 1;
            }
        }
    }
    report
}

// ── Health marks (shared thresholds with the doctor section) ─────────────

pub(crate) fn at5_mark(pct: f64) -> &'static str {
    if pct >= 80.0 {
        sym::OK
    } else if pct >= 50.0 {
        sym::WARN
    } else {
        sym::ERR
    }
}

pub(crate) fn at1_mark(pct: f64) -> &'static str {
    if pct >= 50.0 {
        sym::OK
    } else if pct >= 25.0 {
        sym::WARN
    } else {
        sym::ERR
    }
}

pub(crate) fn mrr_mark(mrr: f64) -> &'static str {
    if mrr >= 0.7 {
        sym::OK
    } else if mrr >= 0.5 {
        sym::WARN
    } else {
        sym::ERR
    }
}

/// `difflore eval` entry point.
pub(crate) async fn handle_eval(ctx: &CommandContext, samples: Option<usize>, json: bool) {
    let started = Instant::now();
    let n = samples.unwrap_or(DEFAULT_SAMPLES).clamp(1, 200);

    let rules = match rule_source::load_rules_from_db(&ctx.db).await {
        Ok(r) => r,
        Err(e) => {
            style::report_error("could not load rules for eval", &e.to_string(), &[]);
            return;
        }
    };
    if rules.len() < 5 {
        emit_too_few(rules.len(), json);
        return;
    }

    // Progress notice on stderr (keeps stdout clean for `--json`): the
    // whole-corpus index build + sampling can take a few seconds.
    if !json {
        eprintln!(
            "  {} measuring recall over {} rules ({} sample{})…",
            style::pewter(sym::BULLET),
            rules.len(),
            n,
            if n == 1 { "" } else { "s" },
        );
    }

    // Isolated, deterministic, offline SHA1 index — no pollution of the user's
    // real per-project indexes (see `upsert_rule_chunks_isolated`).
    let tmp = match tempfile::tempdir() {
        Ok(t) => t,
        Err(e) => {
            style::report_error("could not create eval index", &e.to_string(), &[]);
            return;
        }
    };
    let index_pool = match index_db::open_index_pool_at(&tmp.path().join("eval.db")).await {
        Ok(p) => p,
        Err(e) => {
            style::report_error("could not open eval index", &e.to_string(), &[]);
            return;
        }
    };
    if let Err(e) = index_db::upsert_rule_chunks_isolated(&index_pool, &rules).await {
        style::report_error("could not build eval index", &e.to_string(), &[]);
        return;
    }

    let sample_set = build_samples(&rules, n);
    let report = measure_self_recall(&index_pool, &sample_set, None).await;

    if json {
        emit_json(&report, rules.len(), started.elapsed());
    } else {
        emit_text(&report, rules.len(), started.elapsed());
    }
}

fn emit_too_few(count: usize, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::json!({ "ok": false, "reason": "too_few_rules", "rules": count })
        );
    } else {
        println!(
            "  {} only {count} rule(s) — need ≥5 to measure recall. Try {} or {}.",
            style::pewter(sym::WARN),
            style::cmd("difflore try"),
            style::cmd("difflore import-reviews"),
        );
    }
}

fn emit_text(report: &SelfRecallReport, corpus: usize, elapsed: Duration) {
    let at5 = report.at5_pct();
    let at1 = report.at1_pct();
    let mrr = report.mrr();

    println!();
    println!(
        "  {} {}",
        style::cmd("difflore eval"),
        style::pewter(
            "· self-recall sanity check · local lexical (SHA1) · the reranked search path"
        ),
    );
    println!(
        "  {}",
        style::pewter(
            "query = the rule's own text → an optimistic upper bound, NOT real-world recall"
        ),
    );
    println!();
    println!(
        "  {} self-recall@5  {}/{} ({:.0}%)",
        style::pewter(at5_mark(at5)),
        report.hits_at_5,
        report.tested,
        at5,
    );
    println!(
        "  {} self-recall@1  {}/{} ({:.0}%)",
        style::pewter(at1_mark(at1)),
        report.hits_at_1,
        report.tested,
        at1,
    );
    println!(
        "  {} MRR           {:.3}",
        style::pewter(mrr_mark(mrr)),
        mrr,
    );

    let by_lang = top_languages(report, 4);
    if by_lang.len() >= 2 {
        println!();
        println!("  {}", style::pewter("by language:"));
        for (lang, (n, h1, h5, rr)) in by_lang {
            let lang_mrr = if n == 0 { 0.0 } else { rr / n as f64 };
            println!(
                "    {} @1 {}/{} · @5 {}/{} · MRR {:.2}",
                style::pewter(&lang),
                h1,
                n,
                h5,
                n,
                lang_mrr,
            );
        }
    }

    println!();
    println!(
        "  {}",
        style::pewter(&format!(
            "{corpus} rules · {} sampled · {} ms · same rerank path recall/fix/MCP/hook use",
            report.tested,
            elapsed.as_millis(),
        )),
    );
    println!(
        "  {}",
        style::pewter("real-world paraphrase recall needs separate task-query evaluation"),
    );
    if at5 < 80.0 {
        println!(
            "  {} low @5 — semantic embeddings usually lift ranking: {} or {}",
            style::pewter(sym::TIP),
            style::cmd("difflore cloud login"),
            style::cmd("difflore embeddings setup"),
        );
    }
}

fn emit_json(report: &SelfRecallReport, corpus: usize, elapsed: Duration) {
    let by_lang: serde_json::Map<String, serde_json::Value> = report
        .per_lang
        .iter()
        .map(|(lang, (n, h1, h5, rr))| {
            let mrr = if *n == 0 { 0.0 } else { rr / *n as f64 };
            (
                lang.clone(),
                serde_json::json!({ "n": n, "at1": h1, "at5": h5, "mrr": mrr }),
            )
        })
        .collect();
    println!(
        "{}",
        serde_json::json!({
            "ok": true,
            "mode": "sha1",
            // The metric is self-recall (query = the rule's own text): an
            // optimistic upper bound, NOT real-world recall.
            "metric": "self-recall",
            "real_world_recall_note": "requires separate task-query evaluation",
            "path": "reranked_search",
            "corpus_rules": corpus,
            "samples": report.tested,
            "at1": report.hits_at_1,
            "at5": report.hits_at_5,
            "at5_pct": report.at5_pct(),
            "at1_pct": report.at1_pct(),
            "mrr": report.mrr(),
            "elapsed_ms": elapsed.as_millis(),
            "by_language": by_lang,
        })
    );
}

/// Top-N languages by sample count (ties broken alphabetically), with the
/// remainder folded into an `other` bucket — mirrors the doctor breakdown.
fn top_languages(
    report: &SelfRecallReport,
    limit: usize,
) -> Vec<(String, (usize, usize, usize, f64))> {
    let mut entries: Vec<(String, (usize, usize, usize, f64))> = report
        .per_lang
        .iter()
        .map(|(k, v)| (k.clone(), *v))
        .collect();
    entries.sort_by(|a, b| b.1.0.cmp(&a.1.0).then_with(|| a.0.cmp(&b.0)));
    if entries.len() <= limit {
        return entries;
    }
    let (top, rest) = entries.split_at(limit);
    let mut out = top.to_vec();
    let folded = rest.iter().fold((0, 0, 0, 0.0), |acc, (_, t)| {
        (acc.0 + t.0, acc.1 + t.1, acc.2 + t.2, acc.3 + t.3)
    });
    out.push(("other".to_owned(), folded));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_recall_query_takes_body_significant_words() {
        let content = "Rule ID: x\nRule Name: T\nType: review\nSource: r\nTags: t\n\n\
                       Return 413 when the request body exceeds the configured size limit always";
        let q = self_recall_query(content);
        // Stop-words (the/when/than…) dropped; capped at 8 significant words.
        assert_eq!(q.split_whitespace().count(), 8);
        assert!(q.starts_with("Return 413"), "got {q:?}");
        assert!(
            !q.to_lowercase().contains(" the "),
            "stop-words must be dropped: {q:?}"
        );
    }

    #[test]
    fn report_math_matches_definitions() {
        let mut r = SelfRecallReport {
            tested: 4,
            hits_at_1: 2,
            hits_at_5: 3,
            reciprocal_rank_sum: 1.0 + 0.5 + 0.25, // ranks 1, 1, 4, miss
            per_lang: BTreeMap::new(),
        };
        assert!((r.at5_pct() - 75.0).abs() < 1e-9);
        assert!((r.at1_pct() - 50.0).abs() < 1e-9);
        assert!((r.mrr() - (1.75 / 4.0)).abs() < 1e-9);
        r.tested = 0;
        assert!(r.mrr().abs() < 1e-9, "no divide-by-zero on empty");
    }

    #[test]
    fn marks_follow_thresholds() {
        assert_eq!(mrr_mark(0.7), sym::OK);
        assert_eq!(mrr_mark(0.5), sym::WARN);
        assert_eq!(mrr_mark(0.49), sym::ERR);
        assert_eq!(at5_mark(80.0), sym::OK);
        assert_eq!(at1_mark(24.0), sym::ERR);
    }

    #[test]
    fn build_samples_is_deterministic_and_capped() {
        let rules: Vec<RuleDocument> = (0..50)
            .map(|i| RuleDocument {
                skill_id: format!("r{i}"),
                title: format!("t{i}"),
                content: format!("Rule ID: r{i}\nRule Name: t{i}\n\nbody token alpha{i} bravo"),
                confidence: 0.7,
                file_patterns: None,
                language: None,
                repo_scope: None,
            })
            .collect();
        let a = build_samples(&rules, 10);
        let b = build_samples(&rules, 10);
        assert_eq!(a.len(), 10);
        assert_eq!(
            a.iter().map(|s| &s.skill_id).collect::<Vec<_>>(),
            b.iter().map(|s| &s.skill_id).collect::<Vec<_>>(),
            "stride sampling must be deterministic"
        );
    }
}
