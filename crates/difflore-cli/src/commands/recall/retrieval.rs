//! Recall data-gathering: local + cloud retrieval, semantic/embedder probing,
//! example extraction, and zero-match diagnostics gathering.
//!
//! This is the "what did we find" half of `difflore recall`. Presentation of
//! these results (JSON payloads, human/markdown rendering) lives in the sibling
//! `presentation` module; orchestration, arg validation, and the shared result
//! types live in the parent `mod.rs`.

use difflore_core::context::retrieval::ScoredRuleChunk;
use difflore_core::context::types::{PastVerdict, PastVerdictScope};
use difflore_core::skills::SearchSkillMeta;

use crate::commands::util::project_path;
use crate::style::{self, sym};

use super::{
    CloudRecallResult, CommandContext, DiagnosticItem, DiagnosticStep, LocalRecallResult,
    LocalRuleHit, RecallDiagnostics, candidate_pool_size, local_rule_title,
    more_specific_query_example, query_looks_broad, recall_command, recall_command_for_zero_match,
    strict_file_pattern_match, truncate_one_line,
};

/// Bounded embedding budget for the interactive recall/ask index pass.
/// On an unreachable cloud provider this caps each batch's wait so the
/// command falls back to local SHA1 + FTS in seconds instead of hanging
/// through the full provider retry budget. Matches the bounded budget the
/// MCP and query paths already use.
const RECALL_INDEX_EMBEDDING_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(2500);

pub(super) async fn recall_local_rules(
    ctx: &CommandContext,
    intent: &str,
    file: Option<&str>,
    top_k: usize,
) -> LocalRecallResult {
    let top_k = crate::commands::util::clamp_with_warn("--top-k", top_k, 1, 50, false);
    let db = &ctx.db;
    let rules = match difflore_core::context::rule_source::load_rules_from_db(db).await {
        Ok(rules) => rules,
        Err(error) => {
            eprintln!(
                "{} failed to load local rules: {error}",
                style::err(sym::ERR)
            );
            return LocalRecallResult {
                rules_indexed: 0,
                repo_full_name: None,
                matches: Vec::new(),
                file_scope_fallback: false,
            };
        }
    };
    let mut rules_indexed = 0usize;
    // Detect both origin and upstream so recall can use imported review
    // history from either remote. `repo_full_name` remains the primary
    // display label; `repo_scopes` carries the full list into retrieval.
    let detected_repo_full_names =
        difflore_core::git::detect_github_repo_full_names(&project_path());
    let repo_full_names = difflore_core::skills::expand_repo_scopes_with_source_aliases(
        db,
        &detected_repo_full_names,
    )
    .await
    .unwrap_or(detected_repo_full_names);
    let repo_full_name = repo_full_names.first().cloned();
    let Some(primary_scope) = repo_full_name.clone() else {
        return LocalRecallResult {
            rules_indexed,
            repo_full_name: None,
            matches: Vec::new(),
            file_scope_fallback: false,
        };
    };
    let repo_scopes: Vec<String> = if repo_full_names.is_empty() {
        vec![primary_scope.clone()]
    } else {
        repo_full_names.clone()
    };
    let index_pool = match difflore_core::context::index_db::get_pool_for_cwd().await {
        Ok(pool) => pool,
        Err(error) => {
            eprintln!(
                "{} failed to open local index DB: {error}",
                style::err(sym::ERR)
            );
            return LocalRecallResult {
                rules_indexed,
                repo_full_name: None,
                matches: Vec::new(),
                file_scope_fallback: false,
            };
        }
    };
    match difflore_core::context::orchestrator::ensure_rules_indexed_for_repo_scopes_with_embedding_timeout(
        db,
        &index_pool,
        &repo_scopes,
        Some(RECALL_INDEX_EMBEDDING_TIMEOUT),
    )
    .await
    {
        Ok(count) => rules_indexed = count,
        Err(error) => {
            eprintln!(
                "{} failed to refresh local rule index: {error}",
                style::err(sym::ERR)
            );
        }
    }

    let query = match file {
        Some(file) => format!("{file} {intent}"),
        None => intent.to_owned(),
    };
    let ranking_inputs = difflore_core::context::rule_source::load_rule_ranking_inputs(db).await;

    // Pull a wider candidate pool than top_k so the strict-pattern
    // re-rank below has room to surface file-pattern matches that the
    // content-only retriever would otherwise drop. We truncate back to
    // top_k after the sort.
    let pool_k = candidate_pool_size(top_k);
    let scored = match crate::commands::search::retrieve_rules_for_search(
        &index_pool,
        &query,
        intent,
        pool_k,
        ranking_inputs.confidence_map.as_ref(),
        ranking_inputs.age_days_map.as_ref(),
        file,
        repo_scopes.as_slice(),
    )
    .await
    {
        Ok(scored) => scored,
        Err(error) => {
            eprintln!(
                "{} local rule retrieval failed: {error}",
                style::err(sym::ERR)
            );
            Vec::new()
        }
    };
    let mut scored = crate::commands::search::merge_exact_title_matches(
        &rules,
        intent,
        repo_scopes.as_slice(),
        scored,
        pool_k,
    );

    // Intent-alignment gate (precision fix), kept consistent with the MCP
    // `search_rules` tool. Drops candidates whose DIRECTIVE addresses a
    // different action/subject than the query intent — the topically-
    // adjacent rules (same file area, shared topical token, wrong subject)
    // that the leak-free A/B identified as distracting the agent. Strongly
    // scored hits (exact-title matches, lexically-boosted) are exempt, so a
    // genuinely strong match is never suppressed. Run before the relevance
    // floor so the floor operates on the intent-aligned set.
    difflore_core::context::retrieval::apply_intent_alignment_gate(&mut scored, intent);

    // Launch-blocker ③ — adaptive relevance gate on the explicit CLI
    // recall path, kept consistent with the MCP `search_rules` tool and
    // the hook injection path. A wrong-file / low-relevance query that
    // previously surfaced ~5 weak filler rules now returns zero, so the
    // command renders its existing zero-match diagnostics ("no relevant
    // memory") instead of confident-but-irrelevant noise. Applied on the
    // score-sorted candidate set (post exact-title merge) before the
    // strict-file re-rank, so a genuinely strong match — which clears the
    // floor by a wide margin — is never suppressed.
    difflore_core::context::retrieval::apply_explicit_recall_threshold(&mut scored);

    let ids: Vec<String> = scored.iter().map(|hit| hit.skill_id.clone()).collect();
    let metas = difflore_core::skills::fetch_search_meta(db, &ids).await;
    let mut hits = build_local_hits(&scored, &metas);
    // Stable re-rank: rules whose file_patterns strictly cover the queried
    // file come above content-only matches, preserving relative order within
    // each group. Repo scoping prevents cross-project leakage; this overlay
    // keeps useful same-repo generic rules available while highlighting
    // file-specific evidence first.
    if file.is_some() {
        hits.sort_by(|a, b| {
            let a_strict = strict_file_pattern_match(&a.file_patterns, file);
            let b_strict = strict_file_pattern_match(&b.file_patterns, file);
            b_strict.cmp(&a_strict)
        });
    }
    hits.truncate(top_k);
    // Hydrate each surviving hit with its FULL rule body + structured examples
    // straight from the DB. The retrieval chunk only carries the indexed body
    // text (whose example section is often absent), so without this the
    // fix/bad/good bodies came back NULL in `recall --json` — agents saw
    // headlines, not the team memory. Done once on the final, truncated set so
    // we pay one batched query for the rules we actually return.
    hydrate_full_rule_bodies(db, &mut hits).await;
    let file_scope_fallback = content_only_file_scope_fallback(&hits, file);
    LocalRecallResult {
        rules_indexed,
        repo_full_name,
        matches: hits,
        file_scope_fallback,
    }
}

pub(super) fn content_only_file_scope_fallback(hits: &[LocalRuleHit], file: Option<&str>) -> bool {
    file.is_some()
        && !hits.is_empty()
        && !hits
            .iter()
            .any(|hit| strict_file_pattern_match(&hit.file_patterns, file))
}

pub(super) fn build_local_hits(
    scored: &[ScoredRuleChunk],
    metas: &std::collections::HashMap<String, SearchSkillMeta>,
) -> Vec<LocalRuleHit> {
    let max_score = scored
        .iter()
        .map(|hit| hit.score)
        .fold(f64::NEG_INFINITY, f64::max);
    scored
        .iter()
        .filter_map(|hit| {
            // Missing metadata is a soft skip, matching the MCP `search_rules`
            // path: a chunk whose `skills` row was deleted/deactivated but not
            // yet pruned from the index is stale. Dropping it keeps `difflore
            // recall` and `search_rules` in agreement instead of surfacing a
            // ghost rule with empty file_patterns and a raw skill_id title.
            let meta = metas.get(&hit.skill_id)?;
            let rank_score = if max_score > 0.0 {
                hit.score / max_score
            } else {
                0.0
            };
            // Pull the bad→fix snippets out of the FULL rule body (not the
            // truncated preview) so real recall shows the same sharp, concrete
            // contrast the `difflore try` demo does — the #1 "felt value".
            let (bad, fix) = extract_rule_examples(&hit.content);
            Some(LocalRuleHit {
                id: hit.skill_id.clone(),
                title: local_rule_title(&hit.content, &hit.skill_id),
                preview: truncate_one_line(&hit.content, 200),
                bad,
                fix,
                rank_score,
                raw_score: hit.score,
                confidence: hit.confidence,
                file_patterns: meta.file_patterns.clone(),
                source_repo: meta.source_repo.clone(),
                // Filled in by `hydrate_full_rule_bodies` once we know the
                // final result set; chunk-only construction can't see the
                // `rule_examples` rows the full body needs.
                body: None,
            })
        })
        .collect()
}

/// Attach the full rule body (rendered code-spec + structured examples + the
/// fix/check/trigger fields) to each hit, loaded in one batch from the DB.
///
/// This is the fix for "recall --json returns only titles/previews with bodies
/// NULL": the retrieval `ScoredRuleChunk` carries the indexed body string but
/// NOT the `rule_examples` rows, so the heuristic `bad`/`fix` extraction over
/// the chunk content frequently came up empty even when the rule had real
/// bad/good code. Here we reuse the same public renderer + example loader the
/// MCP `get_rules` detail path uses, then prefer the authoritative DB example
/// code for the `bad`/`fix` snippet lines so the human and JSON surfaces agree.
///
/// Best-effort: a DB error or a stale (already-pruned) id simply leaves that
/// hit's `body` as `None`, degrading to the prior chunk-only display rather
/// than failing the recall.
pub(super) async fn hydrate_full_rule_bodies(
    db: &difflore_core::SqlitePool,
    hits: &mut [LocalRuleHit],
) {
    if hits.is_empty() {
        return;
    }
    let ids: Vec<String> = hits.iter().map(|hit| hit.id.clone()).collect();
    let mut bodies = difflore_core::context::retrieval::render_full_rule_bodies(db, &ids)
        .await
        .unwrap_or_default();
    for hit in hits.iter_mut() {
        let Some(rendered) = bodies.remove(&hit.id) else {
            continue;
        };
        // The `rule_examples` bad/good code is the authoritative source for the
        // one-line `bad`/`fix` snippets the human + JSON headlines show. We run
        // it through the SAME divergence walk the chunk-content extractor uses
        // (`divergent_example_lines`) so the snippet stays a single concise line
        // — e.g. the first line that actually changes for a before/after of the
        // same function — keeping the human `bad`/`fix` rows consistent rather
        // than dumping a multi-line block. The full multi-line code remains in
        // the JSON `examples[]`/`body`. We only overwrite a side when the DB
        // carries a non-empty example for it, so a rule with no examples keeps
        // whatever the chunk heuristic found.
        let db_bad = rendered.first_bad_code();
        let db_fix = rendered.first_good_code();
        if db_bad.is_some() || db_fix.is_some() {
            let (bad_line, fix_line) =
                divergent_example_lines(db_bad.as_deref(), db_fix.as_deref());
            if bad_line.is_some() {
                hit.bad = bad_line;
            }
            if fix_line.is_some() {
                hit.fix = fix_line;
            }
        }
        hit.body = Some(rendered);
    }
}

/// Which side of a bad→fix example a heading introduces.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum ExampleSide {
    Bad,
    Fix,
}

/// Classify a single line as the start of a "bad" or "fix" example section,
/// tolerating the many shapes real rules use. Returns `None` for any line that
/// is not a recognized heading.
///
/// Matching is case-insensitive and markdown-aware. The decision rule:
///   1. The line must be keyword-led (after stripping `#`/`*`/`-`/`>` and the
///      ❌/✅ glyphs): `bad`/`wrong`/`anti(-pattern)`/… → Bad,
///      `good`/`correct`/`right`/`fix`/`better`/… → Fix.
///   2. A keyword-led line counts as a heading when EITHER it carries markdown
///      decoration (started with `#`, `*`, `-`, `>`, or held a ❌/✅ glyph) — so
///      `### ❌ Anti-pattern: Separate state` and `### ✅ Better: do X` are
///      headings even with a descriptive title — OR, for an undecorated line,
///      the remainder after the keyword is only qualifier words
///      (`example`/`code`/`pattern`/…). The latter accepts bare `Bad:` /
///      `Good example:` / `Fix:` while rejecting inline prose like
///      `Bad: this silently leaks a file descriptor`, which would otherwise be
///      mistaken for a section marker.
pub(super) fn classify_example_heading(line: &str) -> Option<ExampleSide> {
    let trimmed = line.trim();
    // Decoration = markdown heading/list/quote markers or the example glyphs.
    // Its presence is what lets a heading carry a descriptive title without
    // being confused for an inline-prose sentence.
    let decorated = trimmed.starts_with('#')
        || trimmed.starts_with('*')
        || trimmed.starts_with('-')
        || trimmed.starts_with('>')
        || trimmed.contains('❌')
        || trimmed.contains('✅');
    // Strip markdown/list/quote decoration and the example glyphs, then collapse
    // to lowercase alphanumerics + spaces so punctuation can't hide the keyword.
    let stripped: String = trimmed
        .trim_start_matches(['#', '*', '-', '>', ' '])
        .chars()
        .map(|c| {
            if c == '❌' || c == '✅' || c == '*' || c == '`' {
                ' '
            } else {
                c
            }
        })
        .collect();
    let lower = stripped.to_ascii_lowercase();
    let tokens: Vec<&str> = lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect();
    let [first, rest @ ..] = tokens.as_slice() else {
        return None;
    };
    // An undecorated keyword line is a heading only if nothing but qualifier
    // words follows it; this is what rejects inline `Bad: this leaks ...` prose.
    // A decorated line is always allowed a descriptive title.
    let rest_is_qualifier_only = rest.iter().all(|t| {
        matches!(
            *t,
            "example" | "examples" | "code" | "way" | "approach" | "pattern"
        )
    });
    if !decorated && !rest_is_qualifier_only {
        return None;
    }
    // Only unambiguous example markers are accepted. Weak words like `avoid`,
    // `do`, `don't`, `prefer`, `before`, `after` are deliberately EXCLUDED: rule
    // titles and prose routinely start with them (`# Avoid unbounded reads`,
    // `Prefer guard clauses`), so treating them as section headings produces
    // false positives that swallow the title's prose as the "bad" snippet.
    match *first {
        "bad" | "wrong" | "incorrect" | "anti" | "antipattern" => Some(ExampleSide::Bad),
        "good" | "correct" | "right" | "fix" | "fixed" | "better" => Some(ExampleSide::Fix),
        _ => None,
    }
}

/// Collect the meaningful code lines from a block of body text that follows an
/// example heading, in source order.
///
/// Skips ``` / ~~~ fence markers and blank / bare-bullet lines, and STOPS at the
/// first markdown section heading (`#`/`##`/`###` …) or horizontal rule (`---`)
/// so a block that over-extends into trailing prose (e.g. a `✅ Good` example
/// that is the last heading and runs to the end of the body, through the closing
/// fence and into a later `## How to Apply` section) never captures a heading.
/// This also makes the helper robust to the dominant real format where ❌/✅
/// markers are inline comments inside ONE shared code fence: the block starts
/// mid-fence, so the lone closing ``` is just skipped and the code lines win.
///
/// Returns every retained line trimmed; the result is used both for the first
/// snippet line and (by the divergence walk) to find where a bad/good pair that
/// shares a leading signature actually differs.
pub(super) fn meaningful_example_code_lines(block: &str) -> Vec<String> {
    let mut lines = Vec::new();
    for raw in block.lines() {
        let trimmed = raw.trim();
        // A new markdown section heading or horizontal rule marks the end of the
        // example — stop rather than wander into prose.
        if is_markdown_section_break(trimmed) {
            break;
        }
        // Fence markers (``` / ~~~, with or without a language tag) and empty /
        // bare-bullet lines are not content; skip them.
        if trimmed.is_empty()
            || trimmed.starts_with("```")
            || trimmed.starts_with("~~~")
            || trimmed == "-"
            || trimmed == "*"
        {
            continue;
        }
        lines.push(trimmed.to_owned());
    }
    lines
}

/// Extract the first meaningful code line from a block of body text that
/// follows an example heading. Thin wrapper over `meaningful_example_code_lines`
/// (the first retained line); returns `None` when the block has no usable code
/// line. Kept as a `#[cfg(test)]` helper that pins the section-break/fence-skip
/// contract of `meaningful_example_code_lines` from the first-line angle; the
/// production path consumes the full line list directly.
#[cfg(test)]
pub(super) fn first_example_code_line(block: &str) -> Option<String> {
    meaningful_example_code_lines(block).into_iter().next()
}

/// True for a markdown section heading (`# …` … `###### …`) or a horizontal
/// rule (`---`/`***`). Used to bound an example block so trailing prose never
/// leaks into the extracted snippet. A bare `#` with no following text is not
/// a heading.
pub(super) fn is_markdown_section_break(trimmed: &str) -> bool {
    if trimmed == "---" || trimmed == "***" || trimmed == "___" {
        return true;
    }
    let hashes = trimmed.chars().take_while(|&c| c == '#').count();
    (1..=6).contains(&hashes)
        && trimmed[hashes..]
            .chars()
            .next()
            .is_some_and(char::is_whitespace)
}

/// PURE: pull the (bad, fix) example snippets out of a rule's full body text.
///
/// Real rules carry examples in many shapes; this tolerates all of them
/// (case-insensitive, markdown-aware). Recognized section headings include:
///   - `Bad:` / `Good:` (the `try` demo + bundled-corpus format)
///   - `### Examples` then `❌ Bad:` ```code``` `✅ Good:` ```code```
///     (the dominant generated/imported-review format)
///   - `### ❌ Wrong` / `### ✅ Correct` (or `✅ Right`), with or without `###`
///   - `Bad example:` / `Good example:`, `Wrong:` / `Correct:`, `Bad:` / `Fix:`
///
/// For each side we take the text from just after its heading up to the next
/// recognized heading (or end of body) and return that block's first
/// meaningful code line (fences + markers stripped). Either side may be present
/// without the other. When no recognizable example heading exists at all, both
/// are `None` and the caller degrades to today's preview-only output.
pub(super) fn extract_rule_examples(content: &str) -> (Option<String>, Option<String>) {
    // Locate every example heading and the side it introduces, in order.
    let lines: Vec<&str> = content.lines().collect();
    let mut headings: Vec<(usize, ExampleSide)> = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        if let Some(side) = classify_example_heading(line) {
            headings.push((idx, side));
        }
    }
    if headings.is_empty() {
        return (None, None);
    }

    // Capture the FULL first block for each side (not just its first line) so we
    // can compute where bad and good actually diverge below. The first
    // occurrence of each side wins (matches how `try` shows one pair).
    let mut bad_block: Option<String> = None;
    let mut fix_block: Option<String> = None;
    for (n, &(start, side)) in headings.iter().enumerate() {
        // The block for this heading runs until the next heading (any side) or
        // the end of the body, so a `bad` block never bleeds into the `fix`
        // code and vice versa.
        let end = headings
            .get(n + 1)
            .map_or(lines.len(), |&(next_start, _)| next_start);
        let block = lines[start + 1..end].join("\n");
        match side {
            ExampleSide::Bad if bad_block.is_none() => bad_block = Some(block),
            ExampleSide::Fix if fix_block.is_none() => fix_block = Some(block),
            _ => {}
        }
    }
    divergent_example_lines(bad_block.as_deref(), fix_block.as_deref())
}

/// PURE: turn a (bad block, fix block) pair into the (bad, fix) snippet lines.
///
/// Normally each side's snippet is just its first meaningful code line. But when
/// a rule's bad and good examples are a before/after of the SAME function, both
/// first lines are the identical signature (e.g. both
/// `func apply(opts *Options) {`) and the rendered `bad`/`fix` look broken
/// because they show the same text. In that case we advance BOTH sides past the
/// shared leading lines and surface the first line where they actually diverge —
/// a minimal diff that shows the real change.
///
/// Rules:
///   - One side missing → return the present side's first line (other `None`).
///   - First lines already differ (the common, demo-style case) → return them.
///   - First lines equal → walk both line lists, skipping leading lines that are
///     identical on both sides (after trimming), and return the first divergent
///     pair. If the fix side has extra lines (bad is a strict prefix), surface
///     the bad block's first line on the bad side and the first added line on the
///     fix side. If only the bad side has extra trailing lines (fix is a strict
///     prefix), there is no new fix line to show, so keep the first lines.
///   - Single-line / fully-identical blocks → keep the first lines as-is.
pub(super) fn divergent_example_lines(
    bad_block: Option<&str>,
    fix_block: Option<&str>,
) -> (Option<String>, Option<String>) {
    let bad_lines = bad_block.map(meaningful_example_code_lines);
    let fix_lines = fix_block.map(meaningful_example_code_lines);

    let bad_first = bad_lines.as_ref().and_then(|l| l.first().cloned());
    let fix_first = fix_lines.as_ref().and_then(|l| l.first().cloned());

    // Either side missing (or empty) → nothing to diff; return what we have.
    let (Some(bad_lines), Some(fix_lines)) = (bad_lines, fix_lines) else {
        return (bad_first, fix_first);
    };
    let (Some(bad_head), Some(fix_head)) = (bad_first.clone(), fix_first.clone()) else {
        return (bad_first, fix_first);
    };

    // First lines already differ → demo-style behavior, return them unchanged.
    if bad_head.trim() != fix_head.trim() {
        return (Some(bad_head), Some(fix_head));
    }

    // First lines are identical: skip the shared leading lines (common
    // signature/boilerplate) and surface the first line that diverges.
    let mut i = 0;
    while i < bad_lines.len() && i < fix_lines.len() && bad_lines[i].trim() == fix_lines[i].trim() {
        i += 1;
    }

    match (bad_lines.get(i), fix_lines.get(i)) {
        // Both sides have a differing line at the divergence point → the real change.
        (Some(bad_div), Some(fix_div)) => (Some(bad_div.clone()), Some(fix_div.clone())),
        // Bad ran out first: the good block adds lines (bad is a strict prefix).
        // Surface the first added line on the fix side; the bad side has nothing
        // past the shared prefix, so fall back to its first line.
        (None, Some(fix_div)) => (Some(bad_head), Some(fix_div.clone())),
        // Fix ran out first (bad has extra trailing lines) or both ran out
        // simultaneously (blocks identical): no new fix line to show → keep the
        // first lines so we never invent a divergence the good example doesn't have.
        (Some(_) | None, None) => (Some(bad_head), Some(fix_head)),
    }
}

/// Minimum retrieval score a cross-repo starter hit must clear to be shown.
///
/// Starter rules come from OTHER repos, so the bar to surface one as a useful
/// suggestion is higher than for in-scope recall: a rule that merely matches the
/// file extension (e.g. any `**/*.go` rule on a Go file) but shares no intent
/// signal is noise, not memory — and on a cold-start repo, noise labelled as
/// "what your team already learned" actively erodes trust. We drop starter hits
/// whose intent relevance falls below this floor so a cold-start repo sees
/// genuinely transferable rules or nothing, never confident-but-irrelevant
/// filler. In-scope recall is unaffected (the user has real memory there).
const STARTER_RELEVANCE_FLOOR: f64 = 0.12;

/// Drop cross-repo starter hits whose intent relevance (`raw_score`) is below
/// `floor`. Pure helper so the cold-start relevance gate is unit-testable.
pub(super) fn filter_starter_by_relevance(
    hits: Vec<LocalRuleHit>,
    floor: f64,
) -> Vec<LocalRuleHit> {
    hits.into_iter()
        .filter(|hit| hit.raw_score >= floor)
        .collect()
}

/// Goal G2 cold-start fallback: when the current repo has no scoped memory,
/// retrieve transferable rules from the shared cross-repo starter index — but
/// keep ONLY those whose `file_patterns` strict-match the edited file AND that
/// clear `STARTER_RELEVANCE_FLOOR`, so a `**/*.go` rule surfaces on Go code only
/// when it is actually relevant to the intent, not as arbitrary cross-repo
/// noise. Each returned hit carries its `source_repo` for the "↪ from {repo}"
/// label.
///
/// The per-project scope invariant is untouched: the caller invokes this only
/// when the scoped index is empty, and these hits are presented as a clearly
/// labeled, separate "from other repos" suggestion — never as this repo's own
/// memory and never silently injected.
pub(super) async fn cross_repo_starter_hits(
    ctx: &CommandContext,
    intent: &str,
    file: &str,
    top_k: usize,
) -> Vec<LocalRuleHit> {
    let db = &ctx.db;
    let Ok(starter_pool) =
        difflore_core::context::orchestrator::ensure_cross_repo_starter_indexed(db).await
    else {
        return Vec::new();
    };
    let query = format!("{file} {intent}");
    let ranking_inputs = difflore_core::context::rule_source::load_rule_ranking_inputs(db).await;
    let pool_k = candidate_pool_size(top_k);
    let Ok(scored) = crate::commands::search::retrieve_rules_for_search(
        &starter_pool,
        &query,
        intent,
        pool_k,
        ranking_inputs.confidence_map.as_ref(),
        ranking_inputs.age_days_map.as_ref(),
        Some(file),
        // No repo scope: every repo's rules are eligible in the starter index.
        // Transferability is enforced by the strict file-pattern filter below.
        &[],
    )
    .await
    else {
        return Vec::new();
    };
    let ids: Vec<String> = scored.iter().map(|hit| hit.skill_id.clone()).collect();
    let metas = difflore_core::skills::fetch_search_meta(db, &ids).await;
    let mut hits = build_local_hits(&scored, &metas);
    hits.retain(|hit| strict_file_pattern_match(&hit.file_patterns, Some(file)));
    // Intent-relevance gate: a file-extension match alone is not memory. Drop
    // hits with no real intent signal so cold-start surfaces transferable rules
    // or nothing — never confident-but-irrelevant filler.
    let mut hits = filter_starter_by_relevance(hits, STARTER_RELEVANCE_FLOOR);
    hits.truncate(top_k);
    hits
}

pub(super) async fn record_local_recall(
    ctx: &CommandContext,
    local: &LocalRecallResult,
    intent: &str,
    file: Option<&str>,
    top_k: usize,
    session_id: &str,
) {
    if local.matches.is_empty() {
        return;
    }
    let db = &ctx.db;
    let query = match file {
        Some(file) => format!("{file} {intent}"),
        None => intent.to_owned(),
    };
    let recalls: Vec<_> = local
        .matches
        .iter()
        .enumerate()
        .map(
            |(index, hit)| difflore_core::rule_outcomes::RuleRecallInput {
                rule_id: hit.id.as_str(),
                session_id: Some(session_id),
                repo_full_name: local.repo_full_name.as_deref(),
                file_path: file,
                query_text: query.as_str(),
                rank: index as i64 + 1,
                top_k: top_k as i64,
                strict_file_match: strict_file_pattern_match(&hit.file_patterns, file),
            },
        )
        .collect();
    let _ = difflore_core::rule_outcomes::record_recalled_with_context(db, &recalls).await;
    let ids: Vec<String> = local.matches.iter().map(|hit| hit.id.clone()).collect();
    emit_rule_fired_observation(ctx, &ids, intent, file, session_id).await;
}

pub(super) fn build_zero_match_diagnostics(
    local: &LocalRecallResult,
    cloud: &CloudRecallResult,
    intent: &str,
    file: Option<&str>,
) -> RecallDiagnostics {
    let mut possible_causes = Vec::new();
    let mut next_steps = Vec::new();

    // A missing repo scope is a more fundamental cause than an empty corpus: a
    // no-remote / non-GitHub checkout also reports rules_indexed == 0 (the scope
    // filter copies nothing into the per-project index), so diagnose the missing
    // scope FIRST. Otherwise such a checkout is mislabeled "corpus empty, import
    // reviews" when the real fix is adding a GitHub remote.
    let no_scope = local.repo_full_name.is_none();
    let empty_corpus = !no_scope && local.rules_indexed == 0;

    if no_scope {
        possible_causes.push(DiagnosticItem {
            code: "repo_scope_missing",
            message: "No GitHub origin/upstream remote was detected; local recall scopes rules by repo, so an unscoped checkout retrieves nothing. This is by design, not an empty corpus.".to_owned(),
        });
    } else if empty_corpus {
        possible_causes.push(DiagnosticItem {
            code: "local_corpus_empty",
            message: "No accepted local rules are indexed for this repo yet, so offline recall has nothing to retrieve.".to_owned(),
        });
    } else {
        possible_causes.push(DiagnosticItem {
            code: "repo_scoped_no_overlap",
            message: format!(
                "{} local rule{} exist for this repo scope, but none overlapped the query strongly enough.",
                local.rules_indexed,
                if local.rules_indexed == 1 { "" } else { "s" },
            ),
        });
    }

    if let Some(file) = file.map(str::trim).filter(|file| !file.is_empty()) {
        possible_causes.push(DiagnosticItem {
            code: "file_pattern_scope",
            message: format!(
                "`{file}` may not match the accepted rules' file patterns, or the file scope may be narrower than the memory you need."
            ),
        });
        next_steps.push(DiagnosticStep {
            command: Some(recall_command_for_zero_match(intent, None)),
            message: "retry without the file scope to test whether file patterns are filtering out relevant memory".to_owned(),
        });
    } else {
        possible_causes.push(DiagnosticItem {
            code: "no_file_scope",
            message: "Most review memory is scoped to file patterns, so a bare query often matches nothing without a file to anchor it.".to_owned(),
        });
        next_steps.push(DiagnosticStep {
            command: Some(recall_command(intent, Some("path/to/file"))),
            message: "add --file <path> so DiffLore can match the rules scoped to that file"
                .to_owned(),
        });
    }

    if query_looks_broad(intent) {
        possible_causes.push(DiagnosticItem {
            code: "query_too_broad",
            message: "The query is broad; recall works best with review-language details like API names, failure modes, or the convention being checked.".to_owned(),
        });
        next_steps.push(DiagnosticStep {
            command: Some(recall_command(
                &more_specific_query_example(intent, file),
                file,
            )),
            message: "retry with a more specific review phrase".to_owned(),
        });
    }

    if !cloud.logged_in {
        possible_causes.push(DiagnosticItem {
            code: "cloud_not_logged_in",
            message: "Cloud review memory was skipped because you are not logged in.".to_owned(),
        });
    } else if cloud.repo_full_name.is_none() {
        possible_causes.push(DiagnosticItem {
            code: "cloud_repo_scope_missing",
            message: "Cloud review memory was skipped because no GitHub repo remote was detected."
                .to_owned(),
        });
    } else {
        possible_causes.push(DiagnosticItem {
            code: "cloud_no_overlap",
            message: "Cloud review memory did not find an imported PR review verdict for this repo, file, and query.".to_owned(),
        });
    }

    if no_scope {
        // Without a recognized GitHub remote there is no scope to attach
        // imported rules to, so the actionable first step is the remote — not
        // import-reviews, which would also find no scope.
        next_steps.push(DiagnosticStep {
            command: Some("git remote -v".to_owned()),
            message: "local recall is repo-scoped; add a GitHub origin/upstream remote (or run inside a repo that has one) so this checkout has memory to retrieve".to_owned(),
        });
    } else if empty_corpus {
        next_steps.push(DiagnosticStep {
            command: Some("difflore import-reviews --max-prs 50".to_owned()),
            message: "create local memories from recent PR review history".to_owned(),
        });
    } else {
        next_steps.push(DiagnosticStep {
            command: Some("difflore status".to_owned()),
            message: "inspect local memory readiness and the current next action".to_owned(),
        });
        next_steps.push(DiagnosticStep {
            command: Some("difflore import-reviews --max-prs 50".to_owned()),
            message:
                "mine more review history if the current repo has no memory for this topic yet"
                    .to_owned(),
        });
    }

    if empty_corpus {
        prioritize_empty_corpus_steps(&mut next_steps);
    }

    RecallDiagnostics {
        summary: "No local rules or cloud review memories matched; recall ran, but the available memory did not overlap this scope.".to_owned(),
        possible_causes,
        next_steps,
    }
}

pub(super) fn prioritize_empty_corpus_steps(next_steps: &mut [DiagnosticStep]) {
    next_steps.sort_by_key(|step| match step.command.as_deref() {
        Some("difflore import-reviews --max-prs 50") => 0,
        _ => 3,
    });
}

pub(super) async fn emit_rule_fired_observation(
    ctx: &CommandContext,
    rule_ids: &[String],
    intent: &str,
    file: Option<&str>,
    session_id: &str,
) {
    if rule_ids.is_empty() {
        return;
    }
    let client = ctx.cloud().await;
    let event = difflore_core::cloud::observations::ObservationEvent::RuleFired {
        rule_ids: rule_ids.iter().take(10).cloned().collect(),
        file_path: file.map(ToOwned::to_owned),
        intent: Some(intent.to_owned()),
        session_id: session_id.to_owned(),
        fired_at: chrono::Utc::now(),
    };
    let _ = difflore_core::cloud::observations::enqueue_and_flush_default(event, client).await;
}

pub(super) async fn recall_cloud_review_memory(
    ctx: &CommandContext,
    intent: &str,
    file: Option<&str>,
    top_k: usize,
) -> CloudRecallResult {
    let client = ctx.cloud().await;
    let has_saved_token = client.is_logged_in();
    let detected_repo_full_names =
        difflore_core::git::detect_github_repo_full_names(&project_path());
    let repo_full_names = difflore_core::skills::expand_repo_scopes_with_source_aliases(
        &ctx.db,
        &detected_repo_full_names,
    )
    .await
    .unwrap_or(detected_repo_full_names);
    let repo_full_name = repo_full_names.first().cloned();
    if !has_saved_token {
        return CloudRecallResult {
            logged_in: false,
            repo_full_name,
            scope: PastVerdictScope::Personal.as_str(),
            team_id: None,
            verdicts: Vec::new(),
        };
    }

    let cloud_status = difflore_core::cloud::sync::fetch_cloud_status(client).await;
    if !cloud_status.logged_in {
        return CloudRecallResult {
            logged_in: false,
            repo_full_name,
            scope: PastVerdictScope::Personal.as_str(),
            team_id: None,
            verdicts: Vec::new(),
        };
    }

    let team_id = cloud_status.team_id.clone();
    let scope = if team_id.is_some() {
        PastVerdictScope::Team
    } else {
        PastVerdictScope::Personal
    };

    let top_k = crate::commands::util::clamp_with_warn("--top-k", top_k, 1, 10, false);
    if repo_full_names.is_empty() {
        return CloudRecallResult {
            logged_in: true,
            repo_full_name,
            scope: scope.as_str(),
            team_id,
            verdicts: Vec::new(),
        };
    }

    // Mirror local-rules multi-scope recall in the cloud path. Repo probes
    // are independent HTTP calls, so run them together; CLI latency should
    // be bounded by the slowest scope, not their sum.
    let repos: Vec<String> = repo_full_names.iter().take(4).cloned().collect();
    let groups = match repos.as_slice() {
        [] => Vec::new(),
        [repo] => {
            vec![
                recall_cloud_repo_verdicts(
                    client,
                    intent,
                    file,
                    top_k,
                    repo,
                    scope,
                    team_id.as_deref(),
                )
                .await,
            ]
        }
        [repo_a, repo_b] => {
            let (a, b) = tokio::join!(
                recall_cloud_repo_verdicts(
                    client,
                    intent,
                    file,
                    top_k,
                    repo_a,
                    scope,
                    team_id.as_deref()
                ),
                recall_cloud_repo_verdicts(
                    client,
                    intent,
                    file,
                    top_k,
                    repo_b,
                    scope,
                    team_id.as_deref()
                )
            );
            vec![a, b]
        }
        [repo_a, repo_b, repo_c] => {
            let (a, b, c) = tokio::join!(
                recall_cloud_repo_verdicts(
                    client,
                    intent,
                    file,
                    top_k,
                    repo_a,
                    scope,
                    team_id.as_deref()
                ),
                recall_cloud_repo_verdicts(
                    client,
                    intent,
                    file,
                    top_k,
                    repo_b,
                    scope,
                    team_id.as_deref()
                ),
                recall_cloud_repo_verdicts(
                    client,
                    intent,
                    file,
                    top_k,
                    repo_c,
                    scope,
                    team_id.as_deref()
                )
            );
            vec![a, b, c]
        }
        [repo_a, repo_b, repo_c, repo_d, ..] => {
            let (a, b, c, d) = tokio::join!(
                recall_cloud_repo_verdicts(
                    client,
                    intent,
                    file,
                    top_k,
                    repo_a,
                    scope,
                    team_id.as_deref()
                ),
                recall_cloud_repo_verdicts(
                    client,
                    intent,
                    file,
                    top_k,
                    repo_b,
                    scope,
                    team_id.as_deref()
                ),
                recall_cloud_repo_verdicts(
                    client,
                    intent,
                    file,
                    top_k,
                    repo_c,
                    scope,
                    team_id.as_deref()
                ),
                recall_cloud_repo_verdicts(
                    client,
                    intent,
                    file,
                    top_k,
                    repo_d,
                    scope,
                    team_id.as_deref()
                )
            );
            vec![a, b, c, d]
        }
    };
    let mut seen = std::collections::HashSet::new();
    let mut verdicts: Vec<PastVerdict> = Vec::new();
    for group in groups {
        for v in group {
            if seen.insert(v.extraction_id.clone()) {
                verdicts.push(v);
            }
        }
    }
    verdicts.truncate(top_k);
    CloudRecallResult {
        logged_in: true,
        repo_full_name,
        scope: scope.as_str(),
        team_id,
        verdicts,
    }
}

pub(super) async fn recall_cloud_repo_verdicts(
    client: &difflore_core::cloud::client::CloudClient,
    intent: &str,
    file: Option<&str>,
    top_k: usize,
    repo: &str,
    scope: PastVerdictScope,
    team_id: Option<&str>,
) -> Vec<PastVerdict> {
    difflore_core::context::retrieval::retrieve_past_verdicts_by_text_with_team(
        client,
        intent,
        Some(repo),
        scope,
        top_k as u32,
        file,
        team_id,
    )
    .await
}
