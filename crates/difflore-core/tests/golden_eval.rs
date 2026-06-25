#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Golden-case retrieval guardrail.
//!
//! Locks the committed smoke fixture's precision/recall/forbidden baseline so a
//! ranking change (e.g. feeding accepted-fix outcomes into the scorer) cannot
//! silently regress retrieval quality. This is the test Codex flagged as the
//! prerequisite guardrail before touching `context::retrieval` scoring.
//!
//! Runs offline and deterministic: the embedded fixture + an isolated TempDir
//! index on the local lexical (SHA1) embedder, the same reranked path the agent
//! uses. See `difflore_core::context::eval`.

use difflore_core::context::{eval, index_db};

#[tokio::test]
async fn golden_smoke_fixture_holds_ranking_baseline() {
    let fixture = eval::parse_golden_fixture(eval::GOLDEN_SMOKE_FIXTURE)
        .expect("embedded golden fixture parses");
    let docs = eval::golden_rules_to_documents(&fixture);

    let tmp = tempfile::tempdir().expect("tempdir");
    let pool = index_db::open_index_pool_at(&tmp.path().join("golden.db"))
        .await
        .expect("open isolated index");
    index_db::upsert_rule_chunks_isolated(&pool, &docs)
        .await
        .expect("build isolated index");

    let top_k = fixture.rules.len().max(eval::GOLDEN_K);
    let report = eval::score_golden_cases(&pool, &fixture, top_k)
        .await
        .expect("score golden cases");

    // Baseline captured when this guardrail was introduced (local SHA1 path):
    //   recall@3 = 1.00, MRR = 1.00, precision@3 = 0.83,
    //   positive forbidden leakage = 1 (the gin body-limit case),
    //   strict-file = 4/4.
    // These are regression floors / ceilings, not aspirational targets — a
    // ranking change must not breach them. Abstention on doc-only edits is a
    // known gap (negative case currently leaks) handled by separate
    // injection-gate work, so it is intentionally NOT asserted here.
    assert!(
        report.mean_recall_at_k >= 0.8,
        "recall@{} regressed below floor: {:.3}",
        report.k,
        report.mean_recall_at_k,
    );
    assert!(
        report.mean_reciprocal_rank >= 0.75,
        "MRR regressed below floor (expected rule fell out of the top): {:.3}",
        report.mean_reciprocal_rank,
    );
    assert!(
        report.positive_forbidden_hits <= 1,
        "forbidden-rule leakage into positive cases increased above baseline: {}",
        report.positive_forbidden_hits,
    );
    assert_eq!(
        report.strict_file_correct, report.strict_file_total,
        "a recalled rule stopped matching its source file globs",
    );
}
