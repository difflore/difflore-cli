use serde_json::{Value, json};
use sqlx::SqlitePool;

use super::super::{McpState, build_cost_meta, estimate_tokens};
use super::util::{MCP_TEXT_ARG_CHAR_LIMIT, validate_mcp_text_arg};

// plan_pr (Layer 1 plan-time predictor): given an issue/PR description,
// predict likely file categories, median file count, and closest historical
// PRs from the local review corpus.
//
// Data source: local SQLite `review_items` rows from `difflore
// import-reviews`, grouped by `(repo_full_name, pr_number)`.

/// One historical PR record reconstructed from `review_items`.
#[derive(Debug, Clone)]
pub(crate) struct HistoricalPr {
    pub(crate) repo: String,
    pub(crate) pr_number: i32,
    /// Concatenated text used for TF-IDF (`file_path` + author + first
    /// review comment body — proxies for "PR title" until we store
    /// it explicitly). Keep this short to avoid IDF dilution.
    pub(crate) text: String,
    /// File paths represented by this historical PR.
    pub(crate) files: Vec<String>,
    /// Pre-tokenised form for the TF vector.
    pub(crate) tokens: Vec<String>,
}

/// Bucket a file path into a coarse category we can express as a
/// rule. Direct port of `coedit_miner.categorise()` — keep in sync.
pub(crate) fn categorise_path(path: &str) -> String {
    let p = path.replace('\\', "/");
    if p.starts_with(".changeset/") && p.ends_with(".md") {
        return "changeset:.changeset/*.md".into();
    }
    if p.starts_with(".github/workflows/") && (p.ends_with(".yml") || p.ends_with(".yaml")) {
        if p.contains("autofix") {
            return "workflow:autofix.yml".into();
        }
        if p.contains("release") {
            return "workflow:release.yml".into();
        }
        if p.contains("/pr") || p.ends_with("/pr.yml") || p.ends_with("/pr.yaml") {
            return "workflow:pr.yml".into();
        }
        if p.contains("trivy") || p.contains("security") {
            return "workflow:security.yml".into();
        }
        return "workflow:generic.yml".into();
    }
    if p.contains("/__tests__/")
        || p.ends_with(".test.ts")
        || p.ends_with(".test.tsx")
        || p.ends_with(".test.js")
        || p.ends_with(".test.jsx")
        || p.ends_with(".spec.ts")
        || p.ends_with(".spec.tsx")
    {
        return "test:js-ts".into();
    }
    if p.ends_with(".test.go") || p.ends_with("_test.go") {
        return "test:go".into();
    }
    if p.ends_with(".test.py") || p.ends_with("_test.py") {
        return "test:py".into();
    }
    if p.ends_with(".rs") && (p.contains("/tests/") || p.contains("/test/")) {
        return "test:rust".into();
    }
    if p.ends_with(".txtar") {
        return "test:txtar".into();
    }
    let manifest_files = [
        "go.mod",
        "go.sum",
        "package.json",
        "pnpm-lock.yaml",
        "yarn.lock",
        "package-lock.json",
        "Cargo.toml",
        "Cargo.lock",
    ];
    for m in &manifest_files {
        if p == *m || p.ends_with(&format!("/{m}")) {
            return format!("manifest:{m}");
        }
    }
    if p.ends_with(".tsx") && p.contains("src/routes/") {
        return "src:route.tsx".into();
    }
    if p.ends_with(".ts") && p.contains("/middlewares/") {
        return "src:middleware.ts".into();
    }
    if p.ends_with(".ts") || p.ends_with(".tsx") || p.ends_with(".js") || p.ends_with(".jsx") {
        return "src:js-ts".into();
    }
    if p.ends_with(".rs") {
        return "src:rust".into();
    }
    if p.ends_with(".go") {
        return "src:go".into();
    }
    if p.ends_with(".py") {
        return "src:py".into();
    }
    let ext = std::path::Path::new(&p)
        .extension()
        .and_then(|e| e.to_str())
        .map_or_else(|| "no-ext".into(), |e| format!(".{e}"));
    format!("other:{ext}")
}

/// Pull historical PRs out of local `SQLite`. Each row in `review_items`
/// represents one PR's representative file (see `ingest/github`).
/// We GROUP BY (`repo_full_name`, `pr_number`) so future schemas with
/// many rows per PR still aggregate correctly.
pub(crate) async fn load_pr_corpus(db: &SqlitePool) -> Vec<HistoricalPr> {
    let rows = sqlx::query!(
        "SELECT repo_full_name, pr_number as \"pr_number: i32\", file_path, id, author \
         FROM review_items \
         WHERE pr_number IS NOT NULL AND repo_full_name IS NOT NULL"
    )
    .fetch_all(db)
    .await
    .unwrap_or_default();
    if rows.is_empty() {
        return Vec::new();
    }

    // Pull the first comment body per review_item so the TF-IDF text
    // has more signal than just the file_path. Cheap (one extra query)
    // and keeps the function pure-SQL.
    let mut by_pr: std::collections::BTreeMap<(String, i32), HistoricalPr> =
        std::collections::BTreeMap::new();
    for row in rows {
        let (Some(repo), Some(pr)) = (row.repo_full_name, row.pr_number) else {
            continue;
        };
        let file_path = row.file_path;
        let item_id = row.id;
        let entry = by_pr
            .entry((repo.clone(), pr))
            .or_insert_with(|| HistoricalPr {
                repo: repo.clone(),
                pr_number: pr,
                text: String::new(),
                files: Vec::new(),
                tokens: Vec::new(),
            });
        push_plan_file(&mut entry.files, &file_path);
        // Also fold the file_path itself into the text — for
        // GitHub-imported rows, file_path doubles as PR title when
        // there are no inline comments.
        if !entry.text.is_empty() {
            entry.text.push(' ');
        }
        entry.text.push_str(&file_path);

        // Best-effort: append the first review comment body so the
        // TF-IDF vector reflects what reviewers said, not just file
        // names. Skip silently on failure — corpus quality is
        // best-effort.
        if let Ok(Some(body)) = sqlx::query_scalar!(
            "SELECT content FROM review_comments WHERE review_item_id = ?1 \
             ORDER BY created_at ASC LIMIT 1",
            item_id
        )
        .fetch_optional(db)
        .await
        {
            entry.text.push(' ');
            entry.text.push_str(&body);
            for path in extract_review_file_paths(&body) {
                push_plan_file(&mut entry.files, &path);
            }
        }
    }
    enrich_corpus_from_skill_descriptions(db, &mut by_pr).await;

    // Tokenise once per PR. Reuses the intent_filter tokeniser so the
    // stopword set + casing match the reranker.
    let mut out: Vec<HistoricalPr> = by_pr.into_values().collect();
    for pr in &mut out {
        let mut toks: Vec<String> = crate::context::intent_filter::tokenise(&pr.text)
            .into_iter()
            .collect();
        toks.sort();
        pr.tokens = toks;
    }
    out
}

async fn enrich_corpus_from_skill_descriptions(
    db: &SqlitePool,
    by_pr: &mut std::collections::BTreeMap<(String, i32), HistoricalPr>,
) {
    let rows: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT description, source_repo FROM skills \
         WHERE origin = 'pr_review' AND COALESCE(status, 'active') = 'active'",
    )
    .fetch_all(db)
    .await
    .unwrap_or_default();
    for (description, source_repo) in rows {
        let proof = crate::skills::parse_candidate_source_proof(&description);
        let Some(pr_number) = proof
            .as_ref()
            .and_then(|proof| proof.source.as_deref())
            .and_then(pr_number_from_source)
        else {
            continue;
        };
        let repo_hint = source_repo
            .as_deref()
            .filter(|repo| !repo.trim().is_empty())
            .map(str::to_owned)
            .or_else(|| {
                proof
                    .as_ref()
                    .and_then(|proof| proof.source.as_deref())
                    .and_then(|source| source.split_once('#').map(|(repo, _)| repo.to_owned()))
            })
            .unwrap_or_else(|| "unknown".to_owned());
        let entry_key = by_pr
            .keys()
            .find(|(_, pr)| *pr == pr_number)
            .cloned()
            .unwrap_or_else(|| (repo_hint.clone(), pr_number));
        let entry = by_pr.entry(entry_key).or_insert_with(|| HistoricalPr {
            repo: repo_hint,
            pr_number,
            text: String::new(),
            files: Vec::new(),
            tokens: Vec::new(),
        });
        if !entry.text.is_empty() {
            entry.text.push(' ');
        }
        entry.text.push_str(&description);
        if let Some(file) = proof
            .as_ref()
            .and_then(|proof| proof.file.as_deref())
            .filter(|file| !file.trim().is_empty())
        {
            push_plan_file(&mut entry.files, file);
        }
        for path in extract_review_file_paths(&description) {
            push_plan_file(&mut entry.files, &path);
        }
    }
}

fn pr_number_from_source(source: &str) -> Option<i32> {
    source
        .rsplit_once('#')
        .and_then(|(_, pr)| pr.trim().parse::<i32>().ok())
}

fn push_plan_file(files: &mut Vec<String>, path: &str) {
    let path = normalize_review_path(path);
    if path.is_empty() || files.iter().any(|existing| existing == &path) {
        return;
    }
    files.push(path);
}

fn extract_review_file_paths(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in body.lines() {
        if let Some((_, related)) = line.split_once("Related files:") {
            for part in related.split([',', ';']) {
                push_extracted_path(&mut out, part);
            }
        }
        if line.contains('|') {
            for cell in line.split('|') {
                push_extracted_path(&mut out, cell);
            }
        }
        for token in line.split('`').skip(1).step_by(2) {
            push_extracted_path(&mut out, token);
        }
    }
    out
}

fn push_extracted_path(out: &mut Vec<String>, raw: &str) {
    let path = normalize_review_path(raw);
    if path.is_empty() || out.iter().any(|existing| existing == &path) {
        return;
    }
    out.push(path);
}

fn normalize_review_path(raw: &str) -> String {
    if raw.trim().contains('*') {
        return String::new();
    }
    let trimmed = raw
        .trim()
        .trim_matches('*')
        .trim_matches('_')
        .trim_matches('`')
        .trim_matches('"')
        .trim_matches('\'')
        .trim_matches(|ch: char| matches!(ch, '[' | ']' | '(' | ')' | '<' | '>' | ':' | ','));
    let path = trimmed.replace('\\', "/");
    if !looks_like_review_file_path(&path) {
        return String::new();
    }
    path
}

fn looks_like_review_file_path(path: &str) -> bool {
    if path.is_empty()
        || path.contains(' ')
        || path.contains('*')
        || path.contains("://")
        || path.starts_with('/')
        || path.starts_with('#')
        || path.starts_with('@')
    {
        return false;
    }
    if path == "Dockerfile" || path.ends_with("/Dockerfile") || path == "Makefile" {
        return true;
    }
    let lower = path.to_ascii_lowercase();
    let Some(ext) = lower.rsplit_once('.').map(|(_, ext)| ext) else {
        return false;
    };
    matches!(
        ext,
        "go" | "mod"
            | "sum"
            | "yml"
            | "yaml"
            | "json"
            | "ts"
            | "tsx"
            | "js"
            | "jsx"
            | "mjs"
            | "cjs"
            | "mts"
            | "cts"
            | "rs"
            | "py"
            | "md"
            | "toml"
            | "lock"
            | "txtar"
            | "css"
            | "scss"
            | "html"
            | "vue"
            | "svelte"
            | "rb"
            | "rake"
            | "php"
            | "java"
            | "kt"
            | "kts"
            | "scala"
            | "c"
            | "h"
            | "cc"
            | "cpp"
            | "cxx"
            | "hh"
            | "hpp"
            | "cs"
            | "fs"
            | "vb"
            | "csproj"
            | "fsproj"
            | "vbproj"
            | "vcxproj"
            | "props"
            | "targets"
            | "xml"
            | "ps1"
            | "psm1"
            | "psd1"
            | "sh"
            | "bash"
            | "zsh"
            | "cmd"
            | "bat"
            | "sql"
            | "graphql"
            | "proto"
            | "gradle"
            | "swift"
    )
}

/// Predict file scope from an in-memory corpus + a user query string.
/// Pulled out of `tool_plan_pr` so unit tests can drive it with
/// synthetic fixtures without touching `SQLite`.
pub(crate) fn predict_scope_from_corpus(
    corpus: &[HistoricalPr],
    query: &str,
    top_k: usize,
) -> Value {
    if corpus.is_empty() {
        return json!({
            "n_neighbors": 0,
            "predicted_file_count_median": null,
            "predicted_categories": [],
            "neighbors": [],
        });
    }

    // IDF over the corpus.
    let n_docs = corpus.len() as f64;
    let mut df: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for doc in corpus {
        for tok in &doc.tokens {
            *df.entry(tok.as_str()).or_insert(0) += 1;
        }
    }
    let idf = |w: &str| -> f64 {
        let c = *df.get(w).unwrap_or(&0) as f64;
        ((n_docs + 1.0) / (c + 1.0)).ln() + 1.0
    };

    // Vectorise query.
    let q_tokens: Vec<String> = crate::context::intent_filter::tokenise(query)
        .into_iter()
        .collect();
    if q_tokens.is_empty() {
        return json!({
            "n_neighbors": 0,
            "predicted_file_count_median": null,
            "predicted_categories": [],
            "neighbors": [],
        });
    }
    let mut q_tf: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    for t in &q_tokens {
        *q_tf.entry(t.clone()).or_insert(0.0) += 1.0;
    }
    let q_vec: std::collections::HashMap<String, f64> = q_tf
        .into_iter()
        .map(|(k, v)| {
            let w = idf(&k);
            (k, v * w)
        })
        .collect();
    let q_norm: f64 = q_vec.values().map(|v| v * v).sum::<f64>().sqrt();
    if q_norm == 0.0 {
        return json!({
            "n_neighbors": 0,
            "predicted_file_count_median": null,
            "predicted_categories": [],
            "neighbors": [],
        });
    }

    // Score each historical PR by cosine similarity.
    let mut scored: Vec<(f64, &HistoricalPr)> = Vec::new();
    for doc in corpus {
        let mut tf: std::collections::HashMap<&str, f64> = std::collections::HashMap::new();
        for t in &doc.tokens {
            *tf.entry(t.as_str()).or_insert(0.0) += 1.0;
        }
        let mut dot = 0.0;
        let mut d_norm_sq = 0.0;
        for (tok, count) in &tf {
            let w = idf(tok);
            let v = count * w;
            d_norm_sq += v * v;
            if let Some(qv) = q_vec.get(*tok) {
                dot += v * qv;
            }
        }
        let d_norm = d_norm_sq.sqrt();
        if d_norm == 0.0 {
            continue;
        }
        let cos = dot / (q_norm * d_norm);
        if cos > 0.0 {
            scored.push((cos, doc));
        }
    }
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    let neighbors: Vec<(f64, &HistoricalPr)> = scored.into_iter().take(top_k).collect();

    if neighbors.is_empty() {
        return json!({
            "n_neighbors": 0,
            "predicted_file_count_median": null,
            "predicted_categories": [],
            "neighbors": [],
        });
    }

    // Predicted categories: union per neighbour, frequency over
    // neighbours.
    let mut cat_counter: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    let mut file_counts: Vec<usize> = Vec::new();
    for (_, doc) in &neighbors {
        let cats: std::collections::BTreeSet<String> =
            doc.files.iter().map(|f| categorise_path(f)).collect();
        for c in cats {
            *cat_counter.entry(c).or_insert(0) += 1;
        }
        file_counts.push(doc.files.len());
    }
    let n = neighbors.len() as f64;
    let mut cat_freq: Vec<(String, usize, f64)> = cat_counter
        .into_iter()
        .map(|(c, k)| (c, k, k as f64 / n))
        .collect();
    cat_freq.sort_by_key(|(_, count, _)| std::cmp::Reverse(*count));
    file_counts.sort_unstable();
    let median = file_counts[file_counts.len() / 2];
    let upper_quartile = file_counts[((file_counts.len() - 1) * 3 + 2) / 4];
    let nearest_file_count = neighbors.first().map_or(median, |(_, doc)| doc.files.len());
    let nearest_score = neighbors.first().map_or(0.0, |(score, _)| *score);
    let runner_up_score = neighbors.get(1).map_or(0.0, |(score, _)| *score);
    let coedit_file_hints = coedit_file_hints(&neighbors);
    let likely_required_patterns = likely_required_patterns(&neighbors);
    let strong_nearest =
        nearest_score >= 0.15 && (runner_up_score == 0.0 || nearest_score >= runner_up_score * 1.4);
    let recommended = if strong_nearest && nearest_file_count > median {
        median
            .max(upper_quartile)
            .max(nearest_file_count.min(median.saturating_add(3)))
    } else {
        median
    };

    json!({
        "n_neighbors": neighbors.len(),
        "predicted_file_count_median": median,
        "predicted_file_count_recommended": recommended,
        "predicted_file_count_upper_quartile": upper_quartile,
        "nearest_file_count": nearest_file_count,
        "coedit_file_hints": coedit_file_hints,
        "likely_required_patterns": likely_required_patterns,
        "predicted_categories": cat_freq
            .into_iter()
            .map(|(c, k, p)| json!({
                "category": c,
                "in_n_of_neighbors": k,
                "probability": (p * 100.0).round() / 100.0,
            }))
            .collect::<Vec<_>>(),
        "neighbors": neighbors
            .into_iter()
            .map(|(score, doc)| json!({
                "repo": doc.repo,
                "pr_number": doc.pr_number,
                "score": (score * 1000.0).round() / 1000.0,
                "files": doc.files,
            }))
            .collect::<Vec<_>>(),
    })
}

fn coedit_file_hints(neighbors: &[(f64, &HistoricalPr)]) -> Vec<Value> {
    #[derive(Default)]
    struct FileHint {
        path: String,
        count: usize,
        score: f64,
        first_rank: usize,
    }

    let mut by_file: std::collections::BTreeMap<String, FileHint> =
        std::collections::BTreeMap::new();
    for (rank, (score, doc)) in neighbors.iter().enumerate() {
        let mut seen_in_neighbor = std::collections::BTreeSet::new();
        for file in &doc.files {
            let path = file.replace('\\', "/");
            if path.trim().is_empty() {
                continue;
            }
            let key = path.to_ascii_lowercase();
            if !seen_in_neighbor.insert(key.clone()) {
                continue;
            }
            let entry = by_file.entry(key).or_insert_with(|| FileHint {
                path,
                first_rank: rank,
                ..FileHint::default()
            });
            entry.count += 1;
            entry.score += score;
            entry.first_rank = entry.first_rank.min(rank);
        }
    }

    let mut hints = by_file.into_values().collect::<Vec<_>>();
    hints.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.first_rank.cmp(&b.first_rank))
            .then_with(|| a.path.cmp(&b.path))
    });

    // Confidence floor + tighter cap. Held-out validation showed
    // both failure modes:
    //   - long-tail score=0.13 hints from rich corpora dilute precision
    //   - low-absolute-score "majority of 2 neighbours" hints from
    //     sparse corpora (preact hit this) are confidently wrong
    // Combined relative + absolute floor handles both: a hint must
    // either be the top hit, OR carry at least HALF the top score AND
    // an absolute confidence above 0.12. Cap to 6 to fit a glance.
    let strongest_score = hints.first().map_or(0.0, |h| h.score);
    let relative_floor = strongest_score * 0.5;
    const ABSOLUTE_FLOOR: f64 = 0.12;
    hints
        .into_iter()
        .enumerate()
        .filter(|(idx, h)| *idx == 0 || (h.score >= relative_floor && h.score >= ABSOLUTE_FLOOR))
        .map(|(_, hint)| {
            json!({
                "file": hint.path,
                "in_n_of_neighbors": hint.count,
                "score": (hint.score * 1000.0).round() / 1000.0,
            })
        })
        .take(6)
        .collect()
}

/// Surface deterministic co-edit patterns (changesets, generated code,
/// lockfiles) when ≥25% of neighbors include them. Pattern-level rather
/// than exact-path because the specific filename in the *current* PR
/// will differ — what we know is that this *kind* of file is missing.
fn likely_required_patterns(neighbors: &[(f64, &HistoricalPr)]) -> Vec<Value> {
    if neighbors.is_empty() {
        return Vec::new();
    }

    type FilePredicate = fn(&str) -> bool;
    // Each (label, pattern, predicate). Conservative — only patterns where
    // the team's co-edit signal is near-deterministic when present at all.
    let predicates: &[(&str, &str, FilePredicate)] = &[
        ("changeset entry", ".changeset/*.md", |p| {
            p.starts_with(".changeset/") && p.ends_with(".md")
        }),
        ("generated route tree", "**/routeTree.gen.ts", |p| {
            p.ends_with("/routeTree.gen.ts") || p == "routeTree.gen.ts"
        }),
        ("generated code (*.gen.ts)", "**/*.gen.ts", |p| {
            p.ends_with(".gen.ts")
        }),
        ("pnpm lockfile", "pnpm-lock.yaml", |p| {
            p == "pnpm-lock.yaml" || p.ends_with("/pnpm-lock.yaml")
        }),
        ("yarn lockfile", "yarn.lock", |p| {
            p == "yarn.lock" || p.ends_with("/yarn.lock")
        }),
        ("Cargo lockfile", "Cargo.lock", |p| {
            p == "Cargo.lock" || p.ends_with("/Cargo.lock")
        }),
        ("Go module sum", "go.sum", |p| {
            p == "go.sum" || p.ends_with("/go.sum")
        }),
    ];

    let n = neighbors.len() as f64;
    let mut out: Vec<Value> = Vec::new();
    for (label, pattern, pred) in predicates {
        let hits = neighbors
            .iter()
            .filter(|(_, doc)| doc.files.iter().any(|f| pred(&f.replace('\\', "/"))))
            .count();
        if hits == 0 {
            continue;
        }
        let frequency = hits as f64 / n;
        if frequency < 0.25 {
            continue;
        }
        out.push(json!({
            "label": label,
            "pattern": pattern,
            "in_n_of_neighbors": hits,
            "frequency": (frequency * 100.0).round() / 100.0,
        }));
    }
    out
}

pub(crate) async fn tool_plan_pr(state: &McpState, args: &Value) -> Result<Value, (i32, String)> {
    let intent = args
        .get("intent")
        .and_then(|v| v.as_str())
        .ok_or((-32602, "Missing required parameter: intent".to_owned()))?;
    validate_mcp_text_arg("intent", intent, MCP_TEXT_ARG_CHAR_LIMIT)?;
    let top_k = args
        .get("top_k")
        .and_then(Value::as_u64)
        .map_or(5, |v| v.clamp(1, 20) as usize);

    let corpus = load_pr_corpus(&state.db).await;
    let detected_repos = crate::mcp_server::hook::detect_git_remote_owner_repos();

    if corpus.is_empty() {
        let text = "No local PR review data available.\n\n\
                    > `plan_pr` predicts file scope from imported PR review history. \
                    Run `difflore import-reviews <owner/repo>` to populate the local \
                    corpus, then call `plan_pr` again. \
                    Schema note: today's prediction relies on `review_items` rows \
                    with `pr_number` set; richer per-PR file lists arrive when \
                    cloud sync mirrors them locally.";
        return Ok(json!({
            "content": [{ "type": "text", "text": text }],
            "_meta": {
                "cost": build_cost_meta(estimate_tokens(text), None),
                "impact": { "kind": "plan", "neighborsFound": 0, "corpusEmpty": true }
            }
        }));
    }

    let scoped_corpus = crate::mcp_server::repo_scoped_plan_corpus(&corpus, &detected_repos);
    let no_repo_scope_memory = !detected_repos.is_empty() && scoped_corpus.is_empty();
    let prediction_corpus = if no_repo_scope_memory {
        &[][..]
    } else if detected_repos.is_empty() {
        &corpus[..]
    } else {
        &scoped_corpus[..]
    };
    let mut prediction = predict_scope_from_corpus(prediction_corpus, intent, top_k);
    if let Some(obj) = prediction.as_object_mut() {
        obj.insert(
            "repo_scope".to_owned(),
            json!({
                "requested": detected_repos,
                "matched_prs": scoped_corpus.len(),
                "no_repo_scope_memory": no_repo_scope_memory,
            }),
        );
    }
    let n_neighbors = prediction
        .get("n_neighbors")
        .and_then(Value::as_u64)
        .unwrap_or(0);

    if n_neighbors == 0 {
        let text = format!(
            "No similar historical PRs in local corpus (searched {} PRs).\n\n\
             > Either the intent is novel for this repo, or the local corpus is \
             too small. Try a broader intent phrasing, or run `difflore \
             import-reviews <owner/repo>` to grow the corpus.",
            prediction_corpus.len()
        );
        return Ok(json!({
            "content": [{ "type": "text", "text": text }],
            "_meta": {
                "cost": build_cost_meta(estimate_tokens(&text), None),
                "impact": {
                    "kind": "plan",
                    "neighborsFound": 0,
                    "corpusSize": prediction_corpus.len(),
                    "totalCorpusSize": corpus.len()
                },
                "prediction": prediction
            }
        }));
    }

    // Format prediction for the agent. Keep it dense.
    let median = prediction
        .get("predicted_file_count_median")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let recommended = prediction
        .get("predicted_file_count_recommended")
        .and_then(Value::as_u64)
        .unwrap_or(median);
    let nearest = prediction
        .get("nearest_file_count")
        .and_then(Value::as_u64)
        .unwrap_or(median);
    let mut text = String::new();
    text.push_str(&format!(
        "## Plan-time prediction for: {:?}\n\n",
        intent.chars().take(120).collect::<String>()
    ));
    text.push_str(&format!(
        "**Predicted scope**: review ~{} file{} before declaring done \
         (median {}, strongest match touched {})\n\n",
        recommended,
        if recommended == 1 { "" } else { "s" },
        median,
        nearest,
    ));
    text.push_str(&format!(
        "_Based on {} historical neighbour{}._\n\n",
        n_neighbors,
        if n_neighbors == 1 { "" } else { "s" },
    ));
    text.push_str("**Categories likely to co-edit:**\n");
    if let Some(cats) = prediction
        .get("predicted_categories")
        .and_then(|v| v.as_array())
    {
        for c in cats {
            let cat = c.get("category").and_then(|v| v.as_str()).unwrap_or("");
            let p = c.get("probability").and_then(Value::as_f64).unwrap_or(0.0);
            let k = c
                .get("in_n_of_neighbors")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            text.push_str(&format!(
                "  - {:>4.0}%  {}  ({}/{} neighbours)\n",
                p * 100.0,
                cat,
                k,
                n_neighbors,
            ));
        }
    }
    text.push_str("\n**Closest historical PRs:**\n");
    if let Some(neighs) = prediction.get("neighbors").and_then(|v| v.as_array()) {
        for n in neighs {
            let repo = n.get("repo").and_then(|v| v.as_str()).unwrap_or("?");
            let pr = n.get("pr_number").and_then(Value::as_i64).unwrap_or(0);
            let score = n.get("score").and_then(Value::as_f64).unwrap_or(0.0);
            text.push_str(&format!("  - [{score:.2}] {repo}#{pr}\n"));
            if let Some(files) = n.get("files").and_then(|v| v.as_array()) {
                for f in files.iter().take(5) {
                    if let Some(s) = f.as_str() {
                        text.push_str(&format!("      · {s}\n"));
                    }
                }
                if files.len() > 5 {
                    text.push_str(&format!("      · … +{} more\n", files.len() - 5));
                }
            }
        }
    }
    text.push_str(&format!(
        "\n> **DiffLore predicts ~{} file{} based on {} similar PR{}.** \
         Confirm you've touched every category before declaring done — \
         silent under-completion (vite/#22269 pattern) is the failure \
         mode this tool exists to catch.",
        recommended,
        if recommended == 1 { "" } else { "s" },
        n_neighbors,
        if n_neighbors == 1 { "" } else { "s" },
    ));

    let tokens_used = estimate_tokens(&text);
    Ok(json!({
        "content": [{ "type": "text", "text": text.trim_end() }],
        "_meta": {
            "cost": build_cost_meta(tokens_used, None),
            "impact": {
                "kind": "plan",
                "neighborsFound": n_neighbors,
                "predictedFileCount": recommended,
                "predictedFileCountMedian": median,
                "nearestFileCount": nearest,
                "corpusSize": prediction_corpus.len(),
                "totalCorpusSize": corpus.len(),
            },
            "prediction": prediction,
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_review_file_paths_reads_related_files_and_review_tables() {
        let body = "\
Related files: context_test.go, utils_test.go, logger_test.go

| File | Description |
| ---- | ----------- |
| context.go | replace magic number 100 |
| .github/workflows/pr.yml | update workflow |

Please inspect `binding/binding_test.go` and ignore `Context.PDF` plus `maps.Copy`.";
        let body = format!("{body}\nIgnore glob patterns like `**/*.go` and `*.md`.");

        let paths = extract_review_file_paths(&body);

        assert!(paths.contains(&"context_test.go".to_owned()));
        assert!(paths.contains(&"utils_test.go".to_owned()));
        assert!(paths.contains(&"logger_test.go".to_owned()));
        assert!(paths.contains(&"context.go".to_owned()));
        assert!(paths.contains(&".github/workflows/pr.yml".to_owned()));
        assert!(paths.contains(&"binding/binding_test.go".to_owned()));
        assert!(!paths.contains(&"Context.PDF".to_owned()));
        assert!(!paths.contains(&"maps.Copy".to_owned()));
        assert!(!paths.contains(&"/*.go".to_owned()));
        assert!(!paths.contains(&".md".to_owned()));
    }

    #[test]
    fn extract_review_file_paths_keeps_release_engineering_scripts() {
        let paths = extract_review_file_paths(
            "Related files: tools/ReleaseEngineering/Draft-TerminalReleases.ps1, \
             build/Microsoft.Terminal.Settings.ModelLib.vcxproj, \
             eng/pipelines/release.targets",
        );

        assert!(paths.contains(&"tools/ReleaseEngineering/Draft-TerminalReleases.ps1".to_owned()));
        assert!(paths.contains(&"build/Microsoft.Terminal.Settings.ModelLib.vcxproj".to_owned()));
        assert!(paths.contains(&"eng/pipelines/release.targets".to_owned()));
    }

    #[test]
    fn prediction_uses_expanded_review_paths_for_file_count() {
        let mut pr = HistoricalPr {
            repo: "difflore-fixtures/gin".to_owned(),
            pr_number: 4542,
            text: "http.StatusContinue magic number bodyAllowedForStatus".to_owned(),
            files: vec!["context.go".to_owned()],
            tokens: Vec::new(),
        };
        for file in extract_review_file_paths(
            "Related files: context_test.go, utils_test.go, logger_test.go",
        ) {
            push_plan_file(&mut pr.files, &file);
        }
        pr.tokens = crate::context::intent_filter::tokenise(&pr.text)
            .into_iter()
            .collect();

        let prediction = predict_scope_from_corpus(
            &[pr],
            "test(context): use http.StatusContinue constant instead of magic number 100",
            1,
        );

        assert_eq!(prediction["predicted_file_count_median"], 4);
        assert_eq!(prediction["predicted_file_count_recommended"], 4);
        let files = prediction["neighbors"][0]["files"]
            .as_array()
            .expect("neighbor files");
        assert_eq!(files.len(), 4);
    }

    #[test]
    fn prediction_promotes_strong_nearest_scope_over_low_median() {
        let corpus = vec![
            test_pr(
                4542,
                "http StatusContinue magic number bodyAllowedForStatus",
                &[
                    "context.go",
                    "context_test.go",
                    "utils_test.go",
                    "logger_test.go",
                ],
            ),
            test_pr(4342, "status context", &["context_test.go"]),
            test_pr(4336, "magic", &["recovery_test.go"]),
            test_pr(4551, "continue", &["README.md"]),
            test_pr(4554, "body", &[]),
        ];

        let prediction = predict_scope_from_corpus(
            &corpus,
            "test(context): use http.StatusContinue constant instead of magic number 100",
            5,
        );

        assert_eq!(prediction["predicted_file_count_median"], 1);
        assert_eq!(prediction["nearest_file_count"], 4);
        assert_eq!(prediction["predicted_file_count_recommended"], 4);
    }

    #[test]
    fn coedit_file_hints_rank_repeat_files_above_single_neighbor_files() {
        let first = test_pr(1, "first", &["first_only.rs", "shared.rs"]);
        let second = test_pr(2, "second", &["shared.rs", "second_only.rs"]);
        let neighbors = vec![(0.5, &first), (0.2, &second)];

        let hints = coedit_file_hints(&neighbors);

        assert_eq!(hints[0]["file"], "shared.rs");
        assert_eq!(hints[0]["in_n_of_neighbors"], 2);
        assert_eq!(hints[1]["file"], "first_only.rs");
    }

    #[test]
    fn pr_number_from_source_reads_github_source_label() {
        assert_eq!(pr_number_from_source("gin-gonic/gin#4542"), Some(4542));
        assert_eq!(pr_number_from_source("not-a-pr"), None);
    }

    fn test_pr(pr_number: i32, text: &str, files: &[&str]) -> HistoricalPr {
        let mut indexed_text = text.to_owned();
        for file in files {
            indexed_text.push(' ');
            indexed_text.push_str(file);
        }
        HistoricalPr {
            repo: "difflore-fixtures/gin".to_owned(),
            pr_number,
            text: indexed_text.clone(),
            files: files.iter().map(|file| (*file).to_owned()).collect(),
            tokens: crate::context::intent_filter::tokenise(&indexed_text)
                .into_iter()
                .collect(),
        }
    }
}
