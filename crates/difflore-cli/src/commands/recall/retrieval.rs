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

use crate::style::{self, sym};
use crate::support::util::{project_path, repo_scopes_for_path};

use super::{
    CloudRecallResult, CommandContext, DiagnosticItem, DiagnosticStep, LocalRecallResult,
    LocalRuleHit, RecallDiagnostics, candidate_pool_size, local_rule_title,
    more_specific_query_example, query_looks_broad, recall_command, strict_pattern_match_any_file,
    strict_scope_files, truncate_one_line,
};

/// Bounded embedding budget for the interactive recall/ask index pass. Caps
/// each batch's wait so an unreachable cloud provider falls back to local
/// SHA1 + FTS in seconds instead of hanging through the full retry budget.
const RECALL_INDEX_EMBEDDING_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(2500);

pub(super) async fn recall_local_rules(
    ctx: &CommandContext,
    intent: &str,
    file: Option<&str>,
    diff_files: &[String],
    top_k: usize,
) -> LocalRecallResult {
    let top_k = crate::support::util::clamp_with_warn("--top-k", top_k, 1, 50, false);
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
                trace: super::RecallTrace::default(),
            };
        }
    };
    let mut rules_indexed = 0usize;
    // Detect both origin and upstream so recall can use imported review
    // history from either remote. `repo_full_name` is the display label;
    // `repo_scopes` carries the full list into retrieval.
    let repo_full_names = repo_scopes_for_path(db, &project_path()).await;
    let repo_full_name = repo_full_names.first().cloned();
    if repo_full_name.is_none() {
        return LocalRecallResult {
            rules_indexed,
            repo_full_name: None,
            matches: Vec::new(),
            file_scope_fallback: false,
            trace: super::RecallTrace::default(),
        };
    }
    let repo_scopes: Vec<String> = repo_full_names.clone();
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
                trace: super::RecallTrace::default(),
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
    let embedding_diag =
        difflore_core::context::index_db::gather_embedding_diagnostics(&index_pool).await;
    if should_force_rebuild_semantic_index(&embedding_diag, rules_indexed) {
        match difflore_core::context::orchestrator::rebuild_rules_index_for_repo_scopes(
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
                    "{} failed to rebuild semantic rule index: {error}",
                    style::err(sym::ERR)
                );
            }
        }
    }

    let query = match file {
        Some(file) => format!("{file} {intent}"),
        None => intent.to_owned(),
    };
    let ranking_inputs = difflore_core::context::rule_source::load_rule_ranking_inputs(db).await;

    // `--diff` sends the whole changeset as the path-hint scope, so one query
    // can boost rules whose evidence paths match any changed file. Single-file
    // recall keeps the historical single-file hint.
    let target_scope = if diff_files.is_empty() {
        file.map(difflore_core::context::retrieval::TargetScope::File)
    } else {
        Some(difflore_core::context::retrieval::TargetScope::Changeset(
            diff_files,
        ))
    };

    // Pull a wider candidate pool than top_k so the strict-pattern re-rank
    // below has room to surface file-pattern matches the content-only
    // retriever would drop; truncated back to top_k after the sort.
    let pool_k = candidate_pool_size(top_k);
    let scored = match crate::commands::recall::search::retrieve_rules_for_search(
        &index_pool,
        &query,
        intent,
        pool_k,
        ranking_inputs.confidence_map.as_ref(),
        ranking_inputs.age_days_map.as_ref(),
        ranking_inputs.effectiveness_map.as_ref(),
        target_scope,
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
    let candidates_retrieved = scored.len();
    let mut scored = crate::commands::recall::search::merge_exact_title_matches(
        &rules,
        intent,
        repo_scopes.as_slice(),
        scored,
        pool_k,
    );
    let candidates_after_exact_merge = scored.len();

    // Intent-alignment gate (consistent with the MCP `search_rules` tool):
    // drops candidates whose directive addresses a different action/subject than
    // the query intent. Strongly scored hits (exact-title, lexically-boosted) are
    // exempt. Run before the relevance floor so the floor sees the aligned set.
    difflore_core::context::retrieval::apply_intent_alignment_gate(&mut scored, intent);
    let candidates_after_intent_gate = scored.len();

    // Adaptive relevance gate: a low-relevance query returns zero so the command
    // renders its zero-match diagnostics instead of irrelevant noise. Applied
    // after the exact-title merge and before presentation.
    difflore_core::context::retrieval::apply_explicit_recall_threshold(&mut scored);
    let candidates_after_relevance_gate = scored.len();

    let ids: Vec<String> = scored.iter().map(|hit| hit.skill_id.clone()).collect();
    let metas = difflore_core::skills::fetch_search_meta(db, &ids).await;
    let mut hits = build_local_hits(&scored, &metas);
    let metadata_missing_dropped = scored.len().saturating_sub(hits.len());
    // Path hints are already a small score boost in core retrieval. Keep the
    // final order relevance-driven instead of grouping every glob match above
    // semantically stronger project-level memories.
    let scope_files = strict_scope_files(file, diff_files);
    dedupe_local_hits(&mut hits);
    hits.truncate(top_k);
    // Hydrate each surviving hit with its full rule body + structured examples
    // from the DB; the retrieval chunk only carries the indexed body text (whose
    // example section is often absent). Done once on the truncated set so we pay
    // one batched query for the rules we actually return.
    hydrate_full_rule_bodies(db, &mut hits).await;
    let file_scope_fallback = content_only_file_scope_fallback(&hits, &scope_files);
    let returned = hits.len();
    LocalRecallResult {
        rules_indexed,
        repo_full_name,
        matches: hits,
        file_scope_fallback,
        trace: super::RecallTrace {
            repo_scopes,
            candidate_limit: pool_k,
            candidates_retrieved,
            candidates_after_exact_merge,
            candidates_after_intent_gate,
            candidates_after_relevance_gate,
            metadata_missing_dropped,
            returned,
        },
    }
}

fn should_force_rebuild_semantic_index(
    diag: &difflore_core::context::EmbeddingDiagnostics,
    rules_indexed: usize,
) -> bool {
    if rules_indexed == 0 || difflore_core::context::index_db::embedding_provider_recently_down() {
        return false;
    }
    let active_semantic =
        diag.active_profile.starts_with("cloud:") || diag.active_profile.starts_with("byok:");
    active_semantic
        && matches!(
            diag.degraded_reason.as_deref(),
            Some("index_not_built" | "profile_mismatch" | "dimension_mismatch")
        )
}

pub(super) fn dedupe_local_hits(hits: &mut Vec<LocalRuleHit>) {
    let mut seen = std::collections::HashSet::new();
    hits.retain(|hit| seen.insert(local_hit_dedupe_key(hit)));
}

fn local_hit_dedupe_key(hit: &LocalRuleHit) -> String {
    let mut patterns: Vec<String> = hit
        .file_patterns
        .iter()
        .map(|pattern| pattern.trim().to_ascii_lowercase())
        .filter(|pattern| !pattern.is_empty())
        .collect();
    patterns.sort();
    patterns.dedup();
    format!(
        "{}\u{1f}{}\u{1f}{}",
        hit.source_repo
            .as_deref()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase(),
        hit.title.trim().to_ascii_lowercase(),
        patterns.join("\u{1e}")
    )
}

pub(super) fn content_only_file_scope_fallback(
    hits: &[LocalRuleHit],
    scope_files: &[String],
) -> bool {
    !scope_files.is_empty()
        && !hits.is_empty()
        && !hits
            .iter()
            .any(|hit| strict_pattern_match_any_file(&hit.file_patterns, scope_files))
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
            // Missing metadata is a soft skip: a chunk whose `skills` row was
            // deleted/deactivated but not yet pruned from the index is stale.
            // Dropping it avoids surfacing a ghost rule with empty file_patterns
            // and a raw skill_id title.
            let meta = metas.get(&hit.skill_id)?;
            let origin = meta.origin.clone();
            let source_rank = origin
                .as_deref()
                .map(difflore_core::context::retrieval::source_rank);
            let rank_score = if max_score > 0.0 {
                hit.score / max_score
            } else {
                0.0
            };
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
                origin,
                source_rank,
                // Filled in by `hydrate_full_rule_bodies`; chunk-only
                // construction can't see the `rule_examples` rows.
                body: None,
            })
        })
        .collect()
}

/// Attach the full rule body (rendered code-spec + structured examples + the
/// fix/check/trigger fields) to each hit, loaded in one batch from the DB.
///
/// The retrieval `ScoredRuleChunk` carries the indexed body string but NOT the
/// `rule_examples` rows, so reuse the same renderer + example loader the MCP
/// `get_rules` detail path uses, preferring the DB example code for the
/// `bad`/`fix` snippet lines so the human and JSON surfaces agree.
///
/// Best-effort: a DB error or a stale (already-pruned) id leaves that hit's
/// `body` as `None`, degrading to chunk-only display rather than failing.
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
        // Run the DB `rule_examples` code through the same `divergent_example_lines`
        // walk the chunk extractor uses so the snippet stays one line. Only
        // overwrite a side when the DB carries a non-empty example for it, so a
        // rule with no examples keeps whatever the chunk heuristic found.
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
/// Matching is case-insensitive and markdown-aware:
///   1. The line must be keyword-led (after stripping `#`/`*`/`-`/`>` and the
///      ❌/✅ glyphs): `bad`/`wrong`/`anti(-pattern)`/… → Bad,
///      `good`/`correct`/`right`/`fix`/`better`/… → Fix.
///   2. A keyword-led line counts as a heading when EITHER it carries markdown
///      decoration (`#`, `*`, `-`, `>`, or a ❌/✅ glyph) — so `### ❌ Anti-pattern:
///      Separate state` is a heading even with a descriptive title — OR, for an
///      undecorated line, only qualifier words (`example`/`code`/`pattern`/…)
///      follow the keyword. The latter accepts bare `Bad:` / `Fix:` while
///      rejecting inline prose like `Bad: this silently leaks a file descriptor`.
pub(super) fn classify_example_heading(line: &str) -> Option<ExampleSide> {
    let trimmed = line.trim();
    // Decoration (markdown heading/list/quote markers or example glyphs) is what
    // lets a heading carry a descriptive title without being mistaken for prose.
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
    // titles and prose routinely start with them, so treating them as headings
    // would swallow the title's prose as the "bad" snippet.
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
/// first markdown section heading or horizontal rule so a block that over-extends
/// into trailing prose never captures a heading. This also handles the common
/// format where ❌/✅ markers are inline comments inside one shared code fence:
/// the lone closing ``` is skipped and the code lines win.
///
/// Returns every retained line trimmed; used for the first snippet line and (by
/// the divergence walk) to find where a bad/good pair sharing a leading
/// signature actually differs.
pub(super) fn meaningful_example_code_lines(block: &str) -> Vec<String> {
    let mut lines = Vec::new();
    for raw in block.lines() {
        let trimmed = raw.trim();
        // A new section heading or horizontal rule ends the example.
        if is_markdown_section_break(trimmed) {
            break;
        }
        // Skip fence markers and empty / bare-bullet lines.
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

/// First meaningful code line of a block (or `None`). Thin `#[cfg(test)]`
/// wrapper over `meaningful_example_code_lines` that pins its
/// section-break/fence-skip contract from the first-line angle; the production
/// path consumes the full line list directly.
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

/// Pull the (bad, fix) example snippets out of a rule's full body text,
/// tolerating the many heading shapes real rules use (case-insensitive,
/// markdown-aware): `Bad:`/`Good:`, `### ❌ Bad:`/`### ✅ Good:`, `Wrong:`/
/// `Correct:`, `Bad:`/`Fix:`, etc.
///
/// For each side, takes the text from just after its heading up to the next
/// recognized heading (or end of body) and returns that block's first
/// meaningful code line. Either side may be absent. When no recognizable
/// heading exists, both are `None` and the caller degrades to preview-only.
pub(super) fn extract_rule_examples(content: &str) -> (Option<String>, Option<String>) {
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
    // occurrence of each side wins.
    let mut bad_block: Option<String> = None;
    let mut fix_block: Option<String> = None;
    for (n, &(start, side)) in headings.iter().enumerate() {
        // Each block runs until the next heading or end of body, so a `bad`
        // block never bleeds into the `fix` code and vice versa.
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

/// Turn a (bad block, fix block) pair into the (bad, fix) snippet lines.
///
/// Normally each side's snippet is its first meaningful code line. But when the
/// bad and good examples are a before/after of the SAME function, both first
/// lines are the identical signature and the rendered `bad`/`fix` look broken.
/// In that case advance both sides past the shared leading lines and surface the
/// first line where they actually diverge — a minimal diff of the real change.
///
/// Rules:
///   - One side missing → return the present side's first line (other `None`).
///   - First lines already differ → return them.
///   - First lines equal → skip identical leading lines and return the first
///     divergent pair. If the fix side adds lines (bad is a strict prefix),
///     surface the bad first line and the first added fix line. If only the bad
///     side has extra trailing lines, there is no new fix line, so keep the
///     first lines.
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

    // First lines already differ → return them unchanged.
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
/// Starter rules come from OTHER repos, so the bar to surface one is higher than
/// for in-scope recall: a rule that merely matches the file extension but shares
/// no intent signal is noise, not memory, and erodes trust on a cold-start repo.
/// Hits below this floor are dropped so cold-start shows genuinely transferable
/// rules or nothing. In-scope recall is unaffected.
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

/// Cold-start fallback: when the current repo has no scoped memory, retrieve
/// transferable rules from the shared cross-repo starter index, keeping only
/// those that clear `STARTER_RELEVANCE_FLOOR`. Path hints can boost ranking in
/// core retrieval but are not a hard transferability gate because old project
/// paths drift.
///
/// The per-project scope invariant holds: the caller invokes this only when the
/// scoped index is empty, and these hits are presented as a separate, labeled
/// "from other repos" suggestion — never as this repo's own memory.
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
    let Ok(scored) = crate::commands::recall::search::retrieve_rules_for_search(
        &starter_pool,
        &query,
        intent,
        pool_k,
        ranking_inputs.confidence_map.as_ref(),
        ranking_inputs.age_days_map.as_ref(),
        ranking_inputs.effectiveness_map.as_ref(),
        Some(difflore_core::context::retrieval::TargetScope::File(file)),
        // No repo scope: every repo's rules are eligible in the starter index;
        // results remain explicitly labeled as cross-repo starter suggestions.
        &[],
    )
    .await
    else {
        return Vec::new();
    };
    let ids: Vec<String> = scored.iter().map(|hit| hit.skill_id.clone()).collect();
    let metas = difflore_core::skills::fetch_search_meta(db, &ids).await;
    let hits = build_local_hits(&scored, &metas);
    // A file-extension match alone is not memory; drop hits with no real intent
    // signal so cold-start surfaces transferable rules or nothing.
    let mut hits = filter_starter_by_relevance(hits, STARTER_RELEVANCE_FLOOR);
    hits.truncate(top_k);
    hits
}

pub(super) async fn record_local_recall(
    ctx: &CommandContext,
    local: &LocalRecallResult,
    intent: &str,
    file: Option<&str>,
    diff_files: &[String],
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
    // The recorded strict flag must agree with the scope retrieval ran
    // against: any changed file for `--diff`, the single file otherwise.
    let scope_files = strict_scope_files(file, diff_files);
    let recalls: Vec<_> = local
        .matches
        .iter()
        .enumerate()
        .map(
            |(index, hit)| difflore_core::observability::rule_outcomes::RuleRecallInput {
                rule_id: hit.id.as_str(),
                session_id: Some(session_id),
                repo_full_name: local.repo_full_name.as_deref(),
                file_path: file,
                query_text: query.as_str(),
                rank: index as i64 + 1,
                top_k: top_k as i64,
                strict_file_match: strict_pattern_match_any_file(&hit.file_patterns, &scope_files),
            },
        )
        .collect();
    let _ = difflore_core::observability::rule_outcomes::record_recalled_with_context(db, &recalls)
        .await;
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

    // A no-remote / non-GitHub checkout also reports rules_indexed == 0 (the
    // scope filter copies nothing in), so diagnose a missing scope FIRST.
    // Otherwise it is mislabeled "no memory, import reviews" when the real fix is
    // adding a supported git remote.
    let no_scope = local.repo_full_name.is_none();
    let empty_corpus = !no_scope && local.rules_indexed == 0;

    if no_scope {
        possible_causes.push(DiagnosticItem {
            code: "repo_scope_missing",
            message: "No supported origin/upstream git remote was detected; local recall scopes rules by repo, so an unscoped checkout retrieves nothing. This is by design, not empty local memory.".to_owned(),
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
            code: "file_path_hint",
            message: format!(
                "`{file}` was used only as a path hint; no repo rule was semantically close enough for this query."
            ),
        });
        next_steps.push(DiagnosticStep {
            command: Some("difflore status".to_owned()),
            message:
                "inspect the local rules for this repo, then retry with review-specific wording"
                    .to_owned(),
        });
    } else {
        possible_causes.push(DiagnosticItem {
            code: "no_file_hint",
            message: "No target file was supplied, so recall could not use path hints to break close relevance ties.".to_owned(),
        });
        next_steps.push(DiagnosticStep {
            command: Some(recall_command(intent, Some("path/to/file"))),
            message: "add --file <path> as a ranking hint for close matches".to_owned(),
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
            message: "Cloud PR review memory is available after sign-in.".to_owned(),
        });
    } else if cloud.repo_full_name.is_none() {
        possible_causes.push(DiagnosticItem {
            code: "cloud_repo_scope_missing",
            message: "Cloud PR review memory needs a supported repo remote.".to_owned(),
        });
    } else {
        possible_causes.push(DiagnosticItem {
            code: "cloud_no_overlap",
            message: "Cloud PR review rules did not find an imported PR review verdict for this repo, file, and query.".to_owned(),
        });
    }

    if no_scope {
        // Without a recognized supported git remote there is no scope to attach
        // imported rules to, so the first step is the remote, not import-reviews.
        next_steps.push(DiagnosticStep {
            command: Some("git remote -v".to_owned()),
            message: "local recall is repo-scoped; add a supported origin/upstream git remote (or run inside a repo that has one) so this checkout has memory to retrieve".to_owned(),
        });
    } else if empty_corpus {
        next_steps.push(DiagnosticStep {
            command: Some("difflore import-reviews --max-prs 50".to_owned()),
            message: "create local rules from recent PR review history".to_owned(),
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
    let repo_full_names = repo_scopes_for_path(&ctx.db, &project_path()).await;
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

    let top_k = crate::support::util::clamp_with_warn("--top-k", top_k, 1, 10, false);
    if repo_full_names.is_empty() {
        return CloudRecallResult {
            logged_in: true,
            repo_full_name,
            scope: scope.as_str(),
            team_id,
            verdicts: Vec::new(),
        };
    }

    // Repo probes are independent HTTP calls, so run them together; CLI latency
    // is bounded by the slowest scope, not their sum. Bounded to 4 repos so the
    // concurrent fan-out can never outrun the cloud's per-client budget.
    let repos: Vec<String> = repo_full_names.iter().take(4).cloned().collect();
    let probes = repos.iter().map(|repo| {
        recall_cloud_repo_verdicts(client, intent, file, top_k, repo, scope, team_id.as_deref())
    });
    let groups = futures_util::future::join_all(probes).await;
    let mut seen = std::collections::HashSet::new();
    let mut verdicts: Vec<PastVerdict> = Vec::new();
    for group in groups {
        for v in group {
            if seen.insert(v.extraction_id.clone()) {
                verdicts.push(v);
            }
        }
    }
    verdicts.sort_by(|a, b| {
        b.similarity
            .partial_cmp(&a.similarity)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
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

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(id: &str, title: &str, source_repo: Option<&str>, patterns: &[&str]) -> LocalRuleHit {
        LocalRuleHit {
            id: id.to_owned(),
            title: title.to_owned(),
            preview: String::new(),
            bad: None,
            fix: None,
            rank_score: 1.0,
            raw_score: 1.0,
            confidence: 0.9,
            file_patterns: patterns
                .iter()
                .map(|pattern| (*pattern).to_owned())
                .collect(),
            source_repo: source_repo.map(str::to_owned),
            origin: None,
            source_rank: None,
            body: None,
        }
    }

    #[test]
    fn dedupe_local_hits_collapses_same_source_title_and_patterns() {
        let mut hits = vec![
            hit(
                "keep",
                "Try to avoid using 'any'",
                Some("acme/web"),
                &["src/**/*.ts"],
            ),
            hit(
                "drop",
                " try to avoid using 'any' ",
                Some("ACME/WEB"),
                &["SRC/**/*.ts"],
            ),
            hit(
                "other-source",
                "Try to avoid using 'any'",
                Some("acme/api"),
                &["src/**/*.ts"],
            ),
            hit(
                "other-pattern",
                "Try to avoid using 'any'",
                Some("acme/web"),
                &["app/**/*.ts"],
            ),
        ];

        dedupe_local_hits(&mut hits);

        let ids: Vec<&str> = hits.iter().map(|hit| hit.id.as_str()).collect();
        assert_eq!(ids, ["keep", "other-source", "other-pattern"]);
    }
}
