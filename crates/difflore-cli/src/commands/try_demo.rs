//! `difflore try` — a zero-config taste of the product.
//!
//! Builds a tiny bundled corpus of real review rules in a throwaway temp index,
//! runs the real retrieval engine against a sample edit, and shows the memories
//! that fire — the same shape a user gets on their own repo after
//! `import-reviews`, without needing `gh` auth or PR history.
//!
//! Isolation guarantees (see `upsert_rule_chunks_isolated`): local SHA1
//! embeddings only (no network, deterministic), no ANN write (a real repo's
//! `~/.difflore/projects/{hash}/` index is never touched), and the index lives
//! in a `TempDir` deleted on return. The canonical rule store (`data.db`) is
//! read-only; the demo writes nothing and adds no rules. The shared activity
//! log does record one retrieval event, the same as any real recall.

use std::time::{Duration, Instant};

use difflore_core::context::index_db;
use difflore_core::context::retrieval::{self, RuleSearchRetrievalOptions};
use difflore_core::context::rule_source::RuleDocument;
use globset::Glob;

use crate::style::{self, sym};

/// File the sample edit lands in. Drives the strict file-pattern rerank:
/// `**/*.go` rules get the in-path boost, off-language rules don't.
const SAMPLE_FILE: &str = "internal/server/upload.go";

/// The lines the agent "just wrote" — a classic unbounded body read. Shown to
/// the user and also fed into the retrieval query, so the match is earned by
/// real overlap, not hand-tuned.
const SAMPLE_EDIT: &str = "data, err := io.ReadAll(r.Body)";

/// Natural-language intent + the edit, mirroring how `recall --diff` builds a
/// query from the changed file plus inferred intent.
const SAMPLE_QUERY: &str =
    "handler reads the entire HTTP request body into memory with io.ReadAll(r.Body), no size limit";

/// One bundled, real-world review rule. Display metadata (`source_repo`,
/// examples) is kept here rather than re-derived from the indexed chunk so
/// attribution is exact and the demo stays self-contained.
struct DemoRule {
    skill_id: &'static str,
    title: &'static str,
    source_repo: &'static str,
    file_patterns: &'static [&'static str],
    body: &'static str,
    bad: &'static str,
    good: &'static str,
}

/// The bundled corpus. Go-flavoured so the sample edit yields a couple of
/// strong hits plus genuine near-misses — a realistic top-K, not a rigged
/// single result.
const fn demo_corpus() -> &'static [DemoRule] {
    &[
        DemoRule {
            skill_id: "demo-413-body-limit",
            title: "Return 413 when a request body exceeds the size limit",
            source_repo: "gin-gonic/gin",
            file_patterns: &["**/*.go"],
            body: "Reading an unbounded request body into memory is a DoS vector. \
                   Enforce a maximum and reject oversized payloads with HTTP 413 \
                   (Payload Too Large) instead of letting io.ReadAll allocate \
                   without limit.",
            bad: "data, _ := io.ReadAll(r.Body) // unbounded",
            good: "r.Body = http.MaxBytesReader(w, r.Body, maxUpload)\n\
                   data, err := io.ReadAll(r.Body)\n\
                   if err != nil { http.Error(w, \"too large\", http.StatusRequestEntityTooLarge); return }",
        },
        DemoRule {
            skill_id: "demo-maxbytesreader",
            title: "Cap request bodies with http.MaxBytesReader before reading",
            source_repo: "golang/go",
            file_patterns: &["**/*.go"],
            body: "Wrap r.Body in http.MaxBytesReader before any io.ReadAll so the \
                   server, not the client, decides the maximum bytes read per \
                   request. This bounds memory and surfaces a clean error.",
            bad: "body, err := io.ReadAll(r.Body)",
            good: "r.Body = http.MaxBytesReader(w, r.Body, 10<<20)\n\
                   body, err := io.ReadAll(r.Body)",
        },
        DemoRule {
            skill_id: "demo-content-length",
            title: "Check Content-Length before allocating buffers from a request",
            source_repo: "kubernetes/kubernetes",
            file_patterns: &["**/*.go"],
            body: "Trust the declared Content-Length only after validating it \
                   against a ceiling. Pre-allocating a buffer sized to an \
                   attacker-controlled header lets a single request exhaust memory.",
            bad: "buf := make([]byte, r.ContentLength)",
            good: "if r.ContentLength > maxBytes { http.Error(w, \"too large\", 413); return }",
        },
        DemoRule {
            skill_id: "demo-no-log-bodies",
            title: "Never log full request bodies; they may carry tokens or PII",
            source_repo: "cli/cli",
            file_patterns: &["**/*.go"],
            body: "Request and response bodies routinely contain credentials, \
                   session tokens, and personal data. Log a size or a redacted \
                   summary, never the raw bytes.",
            bad: "log.Printf(\"request body: %s\", body)",
            good: "log.Printf(\"request body: %d bytes\", len(body))",
        },
        DemoRule {
            skill_id: "demo-context-first-arg",
            title: "Pass context.Context as the first argument; never store it in a struct",
            source_repo: "grpc/grpc-go",
            file_patterns: &["**/*.go"],
            body: "A Context should flow through the call chain as an explicit \
                   first parameter so cancellation and deadlines propagate. \
                   Stashing it in a struct field hides lifetime and leaks goroutines.",
            bad: "type Server struct { ctx context.Context }",
            good: "func (s *Server) Handle(ctx context.Context, req *Req) error",
        },
        DemoRule {
            skill_id: "demo-defer-close-err",
            title: "Check the error returned by Close() in a deferred call",
            source_repo: "golang/go",
            file_patterns: &["**/*.go"],
            body: "A bare `defer f.Close()` silently drops write-flush errors. \
                   Capture and surface the Close error so a failed flush is not \
                   mistaken for a successful write.",
            bad: "defer f.Close()",
            good: "defer func() { if cerr := f.Close(); cerr != nil && err == nil { err = cerr } }()",
        },
    ]
}

impl DemoRule {
    /// Build the indexed document. The `Rule Name:` line is read by the
    /// retrieval title extractor; body + examples become the FTS/embedding text.
    fn to_document(&self) -> RuleDocument {
        let patterns_json = serde_json::to_string(self.file_patterns).unwrap_or_default();
        let content = format!(
            "Rule ID: {id}\nRule Name: {title}\nType: review\nSource: {repo}\nTags: go, http, security\n\n\
             {body}\n\nBad:\n{bad}\n\nGood:\n{good}",
            id = self.skill_id,
            title = self.title,
            repo = self.source_repo,
            body = self.body,
            bad = self.bad,
            good = self.good,
        );
        RuleDocument {
            skill_id: self.skill_id.to_owned(),
            title: self.title.to_owned(),
            content,
            confidence: 0.8,
            file_patterns: (!patterns_json.is_empty()).then_some(patterns_json),
            // Untagged on purpose: a NULL language always satisfies the
            // search fanout's language filter, so a tag-spelling mismatch
            // can't silently empty the demo. The `**/*.go` glob still scopes
            // these to Go via the strict file cascade.
            language: None,
            // Attribution metadata only — recall runs unscoped, so every rule
            // is eligible regardless of the demo CWD.
            repo_scope: Some(self.source_repo.to_owned()),
        }
    }
}

/// True when any of the rule's globs strict-matches the sample file — the same
/// signal `recall` prints as `why: strict file match`.
fn strict_file_match(patterns: &[&str], file: &str) -> bool {
    let normalised = file.trim_start_matches('/').replace('\\', "/");
    patterns.iter().any(|pattern| {
        Glob::new(pattern).is_ok_and(|glob| glob.compile_matcher().is_match(&normalised))
    })
}

/// `difflore try` entry point. Self-contained: no DB, no network, no config.
pub async fn handle_try() {
    let started = Instant::now();
    let corpus = demo_corpus();

    // Throwaway index in a temp dir, deleted when `tmp` drops on return.
    let tmp = match tempfile::tempdir() {
        Ok(t) => t,
        Err(e) => {
            style::report_error(
                "could not create a temporary index for the demo",
                &e.to_string(),
                &[],
            );
            return;
        }
    };
    let pool = match index_db::open_index_pool_at(&tmp.path().join("try-demo.db")).await {
        Ok(p) => p,
        Err(e) => {
            style::report_error("could not open the demo index", &e.to_string(), &[]);
            return;
        }
    };

    let docs: Vec<RuleDocument> = corpus.iter().map(DemoRule::to_document).collect();
    if let Err(e) = index_db::upsert_rule_chunks_isolated(&pool, &docs).await {
        style::report_error("could not build the demo index", &e.to_string(), &[]);
        return;
    }

    // Use the same entry point as the CLI/MCP agent path so the demo's ranking
    // is the real one: RRF hybrid retrieval plus the MCP lexical re-rank.
    // Raw `retrieve_rules_with_confidence` would skip the re-rank, leaving
    // SHA1's near-zero cosine to decide ties.
    let hits = match retrieval::retrieve_rules_for_search(
        &pool,
        RuleSearchRetrievalOptions {
            query: SAMPLE_QUERY,
            lexical_query: SAMPLE_QUERY,
            top_k: 3,
            confidence_map: None,
            age_days_map: None,
            target_file: Some(SAMPLE_FILE),
            repo_scopes: &[],
            // Isolated SHA1 index has no ANN graph; linear scan over ~6 rules.
            ann_enabled: false,
            embedding_timeout: Some(Duration::from_millis(1500)),
            cold_start_retry: false,
            adaptive_prune: false,
        },
    )
    .await
    {
        Ok(h) => h,
        Err(e) => {
            style::report_error("demo recall failed", &e.to_string(), &[]);
            return;
        }
    };

    render(&hits, started.elapsed());
}

fn render(hits: &[retrieval::ScoredRuleChunk], elapsed: Duration) {
    let corpus = demo_corpus();
    let lookup = |skill_id: &str| corpus.iter().find(|r| r.skill_id == skill_id);

    println!();
    println!(
        "  {} {}",
        style::cmd("difflore try"),
        style::pewter(&format!(
            "{} a 5-second taste - no repo, no setup, the real engine",
            sym::BULLET
        )),
    );
    println!();
    println!(
        "  {}",
        style::pewter(&format!("Imagine your agent just edited {SAMPLE_FILE}:")),
    );
    println!();
    println!(
        "      {}   {}",
        style::ident(SAMPLE_EDIT),
        style::amber("← unbounded body read")
    );
    println!();

    let scoped: Vec<&DemoRule> = hits.iter().filter_map(|h| lookup(&h.skill_id)).collect();

    if scoped.is_empty() {
        println!(
            "  {} demo memory returned no match: this should not happen; please report it.",
            style::warn(sym::WARN),
        );
        return;
    }

    println!(
        "  {}",
        style::ok("Before it codes, difflore recalls what your team already learned:"),
    );
    println!();

    for (index, rule) in scoped.iter().enumerate() {
        println!(
            "  {} {}",
            style::emerald(&(index + 1).to_string()),
            style::title(rule.title),
        );
        let why = if strict_file_match(rule.file_patterns, SAMPLE_FILE) {
            format!(
                "this file matches {} · lexically close to the edit",
                rule.file_patterns.join(", "),
            )
        } else {
            "lexically close to the edit".to_owned()
        };
        println!("        {}   {}", style::pewter("why"), style::pewter(&why));
        println!(
            "        {}   {}",
            style::pewter("from"),
            style::emerald(&format!("← learned from {} (PR review)", rule.source_repo)),
        );
        println!(
            "        {}   {}",
            style::pewter("bad"),
            style::danger(rule.bad)
        );
        println!(
            "        {}   {}",
            style::pewter("fix"),
            style::emerald(&first_line(rule.good)),
        );
        println!();
    }

    println!(
        "  {}",
        style::pewter("That is the moment difflore exists for: your agent gets the team's review"),
    );
    println!(
        "  {}",
        style::pewter("judgment before it writes the bug, not in a review comment after."),
    );
    println!();
    println!("  {} Make it real on your repo:", style::emerald(sym::TIP));
    println!(
        "      {}     {}",
        style::cmd("difflore import-reviews"),
        style::pewter("learn from YOUR team's PR history"),
    );
    println!(
        "      {}      {}",
        style::cmd("difflore recall --diff"),
        style::pewter("preview what your agent will see next time"),
    );
    println!();
    println!(
        "  {}",
        style::pewter(&format!(
            "{} bundled rules · real retrieval engine · {} ms · temp index discarded",
            demo_corpus().len(),
            elapsed.as_millis(),
        )),
    );
}

/// The `good` examples can be multi-line; the recall line shows the first.
fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or(s).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corpus_is_well_formed() {
        let corpus = demo_corpus();
        assert!(corpus.len() >= 5, "need enough rules for a credible top-K");
        for rule in corpus {
            assert!(!rule.title.is_empty());
            assert!(!rule.source_repo.is_empty());
            assert!(!rule.file_patterns.is_empty());
            // Every rule must produce a parseable file-pattern JSON document.
            let doc = rule.to_document();
            let patterns = doc.file_patterns.expect("file_patterns json");
            let parsed: Vec<String> =
                serde_json::from_str(&patterns).expect("file_patterns must be valid JSON");
            assert_eq!(parsed.len(), rule.file_patterns.len());
            // The title must round-trip through the indexed content so the
            // retrieval title extractor recovers it.
            assert!(doc.content.contains(&format!("Rule Name: {}", rule.title)));
        }
    }

    #[test]
    fn sample_file_strict_matches_go_rules() {
        // The whole demo hinges on the sample file triggering the body-limit
        // rules via strict glob — guard that invariant.
        assert!(strict_file_match(&["**/*.go"], SAMPLE_FILE));
        assert!(!strict_file_match(&["**/*.rs"], SAMPLE_FILE));
        assert!(!strict_file_match(&["src/**/*.py"], SAMPLE_FILE));
    }

    /// True for the request-body-size rule family (413 / MaxBytesReader /
    /// Content-Length) — the rules genuinely relevant to an unbounded body read.
    fn is_body_size_rule(skill_id: &str) -> bool {
        skill_id.contains("body-limit")
            || skill_id.contains("maxbytesreader")
            || skill_id.contains("content-length")
    }

    #[tokio::test]
    async fn demo_recall_ranks_body_size_rules_above_irrelevant_ones() {
        // End-to-end on the REAL search path (the one the agent uses): build the
        // isolated index, query with the sample intent, and assert the
        // request-body-size family dominates the top results. This is the
        // demo's contract — the lexical re-rank must keep generic rules (defer
        // Close, context-first-arg) out of the headline, or the "aha" breaks.
        let tmp = tempfile::tempdir().expect("tempdir");
        let pool = index_db::open_index_pool_at(&tmp.path().join("t.db"))
            .await
            .expect("open pool");
        let docs: Vec<RuleDocument> = demo_corpus().iter().map(DemoRule::to_document).collect();
        index_db::upsert_rule_chunks_isolated(&pool, &docs)
            .await
            .expect("upsert");

        let hits = retrieval::retrieve_rules_for_search(
            &pool,
            RuleSearchRetrievalOptions {
                query: SAMPLE_QUERY,
                lexical_query: SAMPLE_QUERY,
                top_k: 3,
                confidence_map: None,
                age_days_map: None,
                target_file: Some(SAMPLE_FILE),
                repo_scopes: &[],
                ann_enabled: false,
                embedding_timeout: None,
                cold_start_retry: false,
                adaptive_prune: false,
            },
        )
        .await
        .expect("retrieve");

        assert!(!hits.is_empty(), "demo must return at least one hit");
        let top_ids: Vec<&str> = hits.iter().map(|h| h.skill_id.as_str()).collect();
        assert!(
            is_body_size_rule(top_ids[0]),
            "the #1 hit must be a request-body-size rule, got {top_ids:?}"
        );
        let body_size_in_top = top_ids.iter().filter(|id| is_body_size_rule(id)).count();
        assert!(
            body_size_in_top >= 2,
            "≥2 of the top-3 should be body-size rules after the lexical re-rank, got {top_ids:?}"
        );
    }
}
