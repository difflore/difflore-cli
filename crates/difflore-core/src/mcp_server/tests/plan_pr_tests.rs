//! Fixture tests for the pure helper behind the MCP `plan_pr` tool. No
//! `SQLite` is touched.
use super::super::{HistoricalPr, predict_scope_from_corpus};

fn pr(repo: &str, pr_number: i32, text: &str, files: &[&str]) -> HistoricalPr {
    let mut toks: Vec<String> = crate::context::intent_filter::tokenise(text)
        .into_iter()
        .collect();
    toks.sort();
    HistoricalPr {
        repo: repo.to_owned(),
        pr_number,
        text: text.to_owned(),
        files: files.iter().map(ToString::to_string).collect(),
        tokens: toks,
    }
}

fn n_neighbors(v: &serde_json::Value) -> usize {
    v["n_neighbors"].as_u64().unwrap_or(0) as usize
}

fn top_repo_pr(v: &serde_json::Value) -> Option<(String, i64)> {
    let arr = v["neighbors"].as_array()?;
    let first = arr.first()?;
    Some((
        first["repo"].as_str()?.to_owned(),
        first["pr_number"].as_i64()?,
    ))
}

#[test]
fn vite_security_middleware_query_predicts_middleware_neighbour() {
    // Mirrors predict.py demo 1 (vite/#22269 silent-under-completion case).
    let corpus = vec![
        pr(
            "vite",
            22269,
            "fix HMR patch reject untrusted origins middleware",
            &[
                "packages/vite/src/node/server/middlewares/rejectNoCorsRequest.ts",
                "packages/vite/src/node/server/index.ts",
                "packages/vite/src/node/server/environments/fullBundleEnvironment.ts",
            ],
        ),
        pr(
            "tokio",
            8077,
            "add track_caller and panic docs to timeout_at",
            &["tokio/src/time/timeout.rs", "tokio/tests/time_panic.rs"],
        ),
        pr(
            "gin",
            4580,
            "upgrade go dependencies and CI action versions",
            &[".github/workflows/gin.yml", "go.mod", "go.sum"],
        ),
    ];
    let result = predict_scope_from_corpus(
        &corpus,
        "fix HMR patch security reject untrusted middleware",
        5,
    );
    assert!(
        n_neighbors(&result) >= 1,
        "expected ≥1 neighbour, got {result:#?}"
    );
    let top = top_repo_pr(&result).expect("at least one neighbour");
    assert_eq!(
        top,
        ("vite".into(), 22269),
        "vite security PR should rank #1"
    );
}

#[test]
fn dep_bump_query_predicts_manifest_neighbour() {
    // Mirrors predict.py demo 2 (gin/#4580 dep bump case).
    let corpus = vec![
        pr(
            "vite",
            22269,
            "fix HMR patch reject untrusted origins middleware",
            &["packages/vite/src/node/server/middlewares/rejectNoCorsRequest.ts"],
        ),
        pr(
            "gin",
            4580,
            "upgrade go dependencies and CI action versions",
            &[".github/workflows/gin.yml", "go.mod", "go.sum"],
        ),
        pr(
            "tokio",
            8047,
            "update GitHub Actions workflows to use latest tool versions",
            &[".github/workflows/ci.yml", ".github/workflows/audit.yml"],
        ),
    ];
    let result = predict_scope_from_corpus(
        &corpus,
        "upgrade golang dependencies and trivy action versions",
        5,
    );
    assert!(n_neighbors(&result) >= 1, "expected ≥1 neighbour");
    let top = top_repo_pr(&result).expect("neighbour");
    assert_eq!(top, ("gin".into(), 4580), "gin dep-bump PR should rank #1");
}

#[test]
fn empty_corpus_returns_zero_neighbors_with_null_scope() {
    let result = predict_scope_from_corpus(&[], "anything", 5);
    assert_eq!(n_neighbors(&result), 0);
    assert!(result["predicted_file_count_median"].is_null());
    assert_eq!(result["neighbors"].as_array().map(Vec::len), Some(0));
}

#[test]
fn query_with_no_meaningful_tokens_returns_empty_result() {
    // After stopword filtering "the and a the" tokenises to empty.
    let corpus = vec![pr("vite", 1, "fix bug", &["a.ts"])];
    let result = predict_scope_from_corpus(&corpus, "the and a the", 5);
    assert_eq!(n_neighbors(&result), 0);
    assert!(result["predicted_file_count_median"].is_null());
}
