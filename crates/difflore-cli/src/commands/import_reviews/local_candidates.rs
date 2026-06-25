use difflore_core::domain::models::RememberRuleInput;
use difflore_core::infra::git::RepoScope;
use difflore_core::review_store::{ReviewCommentRecord, ReviewItemWithComments};
use sqlx::SqlitePool;

use crate::style;
use crate::support::review_text::strip_review_markdown_noise;
use crate::support::util::exit_err;

use super::ValidatedArgs;
use super::scope::{
    candidate_scope_paths, file_patterns_from_path, is_import_review_noise_line,
    is_review_table_wrapper_line, repo_wide_file_pattern_from_path,
};

/// Floor for the auto-scaling local memory budget.
const LOCAL_CANDIDATE_DEFAULT_MIN: usize = 25;
const LOCAL_CANDIDATE_RELATED_FILES_BODY_LIMIT: usize = 12;
const FALLBACK_REVIEW_DIRECTIVE: &str =
    "capture the repeatable review judgement before accepting this candidate";

// Each surviving high-signal comment is scored on [0.0, 1.0] from a handful
// of features (directive strength, adoption proxy = resolved thread,
// reaction approval, later-reply contradiction, bot authorship). The score
// drives a 3-way route:
//   * `>= HIGH` → promote to active immediately.
//   * `[LOW, HIGH)` → leave as a `status='pending'` candidate for review.
//   * `< LOW`  → drop (don't even draft a candidate).

/// At/above this, a captured review memory is auto-activated.
pub(super) const CAPTURE_CONFIDENCE_HIGH: f32 = 0.62;
/// Below this, the comment is dropped entirely (too weak/contradicted).
pub(super) const CAPTURE_CONFIDENCE_LOW: f32 = 0.40;
/// Baseline confidence a comment earns just by clearing the existing
/// content gate (a real human-or-bot directive sentence, score >= 4).
/// Tuned so a bare strong directive with no adoption/approval signal lands
/// as a *pending* candidate (>= LOW, < HIGH) — the v1 quarantine the spec
/// asks for — and only adoption/approval lifts it to auto-active.
const CAPTURE_CONFIDENCE_BASE: f32 = 0.50;
/// A resolved review thread is the v1 adoption proxy (the maintainer marked
/// the discussion settled — the suggestion was almost always applied).
const CAPTURE_BONUS_RESOLVED: f32 = 0.20;
/// Net-positive reactions (👍 outweighs 👎, or any approval with no
/// pushback) nudge confidence up.
const CAPTURE_BONUS_APPROVAL: f32 = 0.10;
/// Net-negative reactions (👎 strictly outweighs 👍) withhold auto-activation.
/// Sized so a resolved-but-disapproved directive drops into the pending band
/// (0.50 + 0.20 − 0.15 = 0.55, still ≥ LOW so it is reviewed) while a bare
/// disapproved directive falls below LOW (0.50 − 0.15 = 0.35) and is dropped.
const CAPTURE_PENALTY_DISAPPROVAL: f32 = 0.15;
/// A later reply in the same thread retracting the suggestion ("actually
/// no", "nvm", "disregard", …) is a strong negative — push below LOW so a
/// bare directive with a contradiction is dropped.
const CAPTURE_PENALTY_CONTRADICTION: f32 = 0.30;
/// Bot authorship is a *small* negative feature (not a veto): a bot can
/// still clear HIGH on a strong, adopted directive.
const CAPTURE_PENALTY_BOT: f32 = 0.08;
/// An extra-strong directive (score well past the gate floor) earns a small
/// top-up so an obviously imperative review line can reach active on its own
/// merit plus any approval, without needing a resolved thread.
const CAPTURE_BONUS_STRONG_DIRECTIVE: f32 = 0.05;

#[derive(Debug, Default)]
pub(super) struct LocalCandidateProgress {
    pub(super) comments_considered: usize,
    /// Newly drafted rules (both auto-activated AND left pending). The
    /// budget gate counts this total so a flood of either kind still caps.
    pub(super) candidates_created: usize,
    /// Of `candidates_created`, those whose capture confidence cleared the
    /// HIGH threshold and were promoted to active immediately.
    pub(super) candidates_activated: usize,
    /// Of `candidates_created`, those left as `status='pending'` drafts
    /// because their confidence landed in the medium band.
    pub(super) candidates_pending: usize,
    /// Comments that distilled to a candidate signature already handled during
    /// this import run. These are skipped before touching the store, so they do
    /// not strengthen an existing memory.
    pub(super) candidates_duplicate_in_run: usize,
    /// Store-level dedupe into an existing pending memory. This strengthens the
    /// stored memory instead of writing a duplicate row.
    pub(super) candidates_deduped: usize,
    /// Re-imported comments whose content already exists as an `active` rule
    /// (a previously promoted import seen again). Deduped into the approved
    /// rule WITHOUT strengthening it, so it is tracked apart from
    /// `candidates_deduped` (which does bump confidence) to avoid reporting an
    /// untouched rule as "strengthened".
    pub(super) candidates_matched_active: usize,
    /// Comments whose re-derived candidate matches a `rejected_signatures`
    /// tombstone (the user already rejected this exact content). Suppressed
    /// rather than re-created so an import can't resurrect a rejected draft.
    pub(super) candidates_suppressed_rejected: u64,
    pub(super) budget: usize,
    pub(super) comments_skipped: usize,
    pub(super) capped: bool,
}

pub(super) const fn local_candidate_budget_reached(progress: &LocalCandidateProgress) -> bool {
    progress.candidates_created >= progress.budget
}

pub(super) fn clean_review_comment(content: &str) -> String {
    let joined: String = content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| !line.starts_with("```"))
        .filter(|line| !is_import_review_noise_line(line))
        // CodeRabbit / AI-reviewer noise: HTML-style summary toggles and
        // boilerplate banners that wrap real review prose.
        .filter(|line| {
            let lower = line.to_ascii_lowercase();
            !(lower.starts_with("<details>")
                || lower.starts_with("</details>")
                || lower.starts_with("<summary>")
                || lower.starts_with("</summary>")
                || lower.starts_with("<!--")
                || lower.starts_with("---")
                || lower == "<br>"
                || lower.starts_with("actionable comments posted"))
        })
        .map(|line| line.trim_start_matches(['>', '-', '*']).trim())
        .collect::<Vec<_>>()
        .join(" ");
    let stripped = strip_review_markdown_noise(&joined);
    stripped.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn is_platform_review_wrapper_comment(content: &str) -> bool {
    let lower = content.to_ascii_lowercase();
    if (lower.contains("[!caution]") && lower.contains("outside the diff"))
        || lower.contains("some comments are outside the diff")
        || lower.contains("outside diff range comments")
    {
        return true;
    }
    content.lines().any(is_review_table_wrapper_line)
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}

fn is_coverage_report_noise(lower: &str) -> bool {
    contains_any(
        lower,
        &[
            "codecov",
            "coverage report",
            "patch coverage",
            "project coverage",
            "coverage diff",
            "coverage changed",
            "covered by tests",
            "coverage decreased",
        ],
    )
}

fn is_ai_review_summary_noise(lower: &str) -> bool {
    contains_any(
        lower,
        &[
            "coderabbit",
            "code rabbit",
            // Greptile PR-level summary blocks: header, per-PR confidence
            // score, and the "Important Files Changed" table. None are reusable
            // coding conventions; the per-finding inline comments survive.
            "greptile summary",
            "confidence score",
            "important files changed",
            "pull request overview",
            "pr overview",
            "pr summary",
            "walkthrough",
            "automated review",
            "review skipped",
            "actionable comments posted",
            "updates since last review",
        ],
    )
}

/// Instructions addressed to the AI reviewer itself (echoed from `.greptile`
/// or bot configuration), not coding conventions for the agent. e.g.
/// "If you propose a fix, please make it concise".
fn is_reviewer_meta_instruction(lower: &str) -> bool {
    lower.contains("if you propose a fix")
        || lower.contains("when reviewing this pr")
        || lower.contains("as an ai reviewer")
}

fn is_dependency_maintenance_noise(lower: &str) -> bool {
    lower.starts_with("bump ")
        || lower.starts_with("build(deps")
        || lower.starts_with("build(deps-dev")
        || lower.starts_with("chore(deps")
        || (lower.contains("release notes") && lower.contains("dependenc"))
        || (lower.contains("changelog") && lower.contains("dependenc"))
        || lower.contains("dependencies dashboard")
        || lower.contains("renovatebot")
        || lower.contains("dependabot")
}

fn is_acknowledgement_noise(lower: &str) -> bool {
    let trimmed = lower.trim_matches(['.', '!', ' ']);
    matches!(
        trimmed,
        "done" | "fixed" | "resolved" | "addressed" | "thank you" | "thanks"
    ) || lower.starts_with("thanks,")
        || lower.starts_with("thanks ")
        || lower.starts_with("thank you")
        || lower.starts_with("fixed in ")
        || lower.starts_with("addressed in ")
        || lower.starts_with("resolved in ")
        || lower.starts_with("i pushed")
        || lower.starts_with("i updated")
        || lower.starts_with("i fixed")
        || lower.starts_with("i have updated")
        || lower.starts_with("i've updated")
        || lower.starts_with("i have gone through")
        || lower.starts_with("i've gone through")
        || lower.starts_with("i added")
        || lower.starts_with("all the tests are passing")
        || lower.starts_with("let me know what you think")
}

fn is_directive_question(lower: &str) -> bool {
    let starts_with_request = lower.starts_with("can you ")
        || lower.starts_with("could you ")
        || lower.starts_with("would you ")
        || lower.starts_with("can we ")
        || lower.starts_with("could we ")
        || lower.starts_with("should we ");
    starts_with_request
        && contains_any(
            lower,
            &[
                " add ",
                " assert",
                " avoid ",
                " check ",
                " cover ",
                " ensure ",
                " guard ",
                " prefer ",
                " rename ",
                " test",
                " validate ",
                " verify ",
            ],
        )
}

fn is_pr_process_noise(lower: &str) -> bool {
    lower.starts_with("can i make changes to this pr")
        || lower.contains("should i fork your repo")
        || lower.contains("would you merge main")
        || lower.contains("merge main to this branch")
        || lower.contains("merge `main`")
        || lower.contains("rebase needed")
        || lower.contains("update the pr base branch")
        || lower.contains("update the [pr base branch]")
        || lower.contains("years old pr")
        || lower.contains("ready for feedback before merging")
        || lower.contains("await additional reviews")
        || lower.contains("this pr is ready")
        || lower.contains("from the last team meeting")
        || lower.contains("offers to fund")
        || lower.contains("lowest maintained branch")
        || lower.contains("symfony releases calendar")
        || lower.contains("target 4.4 instead")
        || lower.contains("target 6.4 instead")
        || lower.contains("submitted against the")
}

fn is_weak_question_noise(lower: &str) -> bool {
    if is_directive_question(lower) {
        return false;
    }
    lower.starts_with("do we need ")
        || lower.starts_with("do you know ")
        || lower.starts_with("does this ")
        || lower.starts_with("is there ")
        || lower.starts_with("are there ")
        || lower.starts_with("why ")
        || lower.starts_with("what ")
        || lower.starts_with("how ")
        || (lower.ends_with('?')
            && !contains_any(
                lower,
                &[
                    " should ",
                    " need to ",
                    " needs to ",
                    " must ",
                    " please ",
                    " add ",
                    " test",
                    " verify",
                    " validate",
                ],
            ))
}

fn is_import_review_noise_comment(clean: &str) -> bool {
    let lower = clean.to_ascii_lowercase();
    is_coverage_report_noise(&lower)
        || is_ai_review_summary_noise(&lower)
        || is_dependency_maintenance_noise(&lower)
        || is_acknowledgement_noise(&lower)
        || is_pr_process_noise(&lower)
        || is_weak_question_noise(&lower)
        || is_reviewer_meta_instruction(&lower)
}

pub(super) fn is_high_signal_review_comment_for_paths(
    content: &str,
    scope_paths: &[String],
) -> bool {
    if is_platform_review_wrapper_comment(content) {
        return false;
    }
    let clean = clean_review_comment(content);
    if clean.chars().count() < 32 {
        return false;
    }
    let lower = clean.to_ascii_lowercase();
    if is_import_review_noise_comment(&clean) {
        return false;
    }
    if is_low_value_docs_translation_comment(&clean, scope_paths) {
        return false;
    }
    let noise_prefixes = [
        "agree with",
        "because ",
        "my personal preference",
        "personal preference",
        "this is the problem",
        "~~",
    ];
    if noise_prefixes
        .iter()
        .any(|needle| lower == *needle || lower.starts_with(needle))
    {
        return false;
    }
    let low_signal = [
        "lgtm",
        "looks good",
        "thank you",
        "thanks",
        "+1",
        "nit:",
        "nitpick",
        "question:",
    ];
    if low_signal
        .iter()
        .any(|needle| lower == *needle || lower.starts_with(needle))
    {
        return false;
    }
    best_review_directive_sentence(content).is_some()
}

fn is_low_value_docs_translation_comment(clean: &str, scope_paths: &[String]) -> bool {
    if !scope_paths
        .iter()
        .any(|path| is_non_english_docs_translation_path(path))
    {
        return false;
    }
    let lower = clean.to_ascii_lowercase();
    if mostly_non_ascii_letters(clean) {
        return true;
    }
    if has_reusable_docs_engineering_signal(clean) {
        return false;
    }
    if contains_any(
        &lower,
        &[
            "translation",
            "translated",
            "translate ",
            "wording",
            "grammar",
            "typo",
            "sentence",
            "phrase",
            "native speaker",
            "more natural",
            "sounds natural",
            "improves the reading",
        ],
    ) {
        return true;
    }
    !has_reusable_docs_engineering_signal(clean)
}

fn is_non_english_docs_translation_path(path: &str) -> bool {
    let normalized = path.replace('\\', "/").to_ascii_lowercase();
    let parts = normalized.split('/').collect::<Vec<_>>();
    if parts.len() < 4 || parts[0] != "docs" || parts[2] != "docs" {
        return false;
    }
    let locale = parts[1];
    locale != "en"
        && locale.len() <= 12
        && locale
            .chars()
            .all(|ch| ch.is_ascii_alphabetic() || ch == '-')
}

fn mostly_non_ascii_letters(text: &str) -> bool {
    let mut ascii_letters = 0usize;
    let mut non_ascii_letters = 0usize;
    for ch in text.chars().filter(|ch| ch.is_alphabetic()) {
        if ch.is_ascii() {
            ascii_letters += 1;
        } else {
            non_ascii_letters += 1;
        }
    }
    non_ascii_letters >= 4 && non_ascii_letters > ascii_letters
}

fn has_reusable_docs_engineering_signal(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let has_code_span = text.contains('`');
    if contains_any(
        &lower,
        &[
            "api symbol",
            "api name",
            "parameter name",
            "class name",
            "function name",
            "exception name",
            "keep it untranslated",
            "keep them untranslated",
        ],
    ) {
        return true;
    }
    if has_code_span
        && contains_any(
            &lower,
            &[
                "api",
                "attribute",
                "class",
                "exception",
                "function",
                "http",
                "method",
                "openapi",
                "parameter",
                "response",
                "schema",
                "status_code",
                "status code",
                "type",
                "validation",
            ],
        )
    {
        return true;
    }
    contains_any(
        &lower,
        &[
            "regression test",
            "security",
            "public api",
            "breaking change",
            "runtime behavior",
        ],
    )
}

fn review_sentences(content: &str) -> Vec<String> {
    let clean = clean_review_comment(content);
    let chars = clean.char_indices().collect::<Vec<_>>();
    let mut out = Vec::new();
    let mut start = 0usize;
    for (i, (idx, ch)) in chars.iter().enumerate() {
        if !matches!(ch, '.' | '!' | '?') {
            continue;
        }
        if *ch == '.'
            && chars
                .get(i.wrapping_sub(1))
                .is_some_and(|(_, prev)| prev.is_ascii_alphanumeric())
            && chars
                .get(i + 1)
                .is_some_and(|(_, next)| next.is_ascii_alphanumeric())
        {
            continue;
        }
        let sentence = clean[start..*idx].trim();
        if !sentence.is_empty() {
            out.push(sentence.to_owned());
        }
        start = idx + ch.len_utf8();
        if out.len() >= 24 {
            break;
        }
    }
    if out.len() < 24 {
        let sentence = clean[start..].trim();
        if !sentence.is_empty() {
            out.push(sentence.to_owned());
        }
    }
    out
}

fn first_sentence(content: &str) -> String {
    review_sentences(content)
        .into_iter()
        .next()
        .unwrap_or_default()
}

fn normalize_review_directive_sentence(sentence: &str) -> String {
    let mut text = strip_leading_review_address(sentence).to_owned();
    text = trim_leading_praise_clause(text);
    text = trim_review_directive_prefixes(text);
    loop {
        let lower = text.to_ascii_lowercase();
        let prefixes = [
            "also, ",
            "also ",
            "and ",
            "but ",
            "however, ",
            "however ",
            "as suggested, ",
            "as suggested ",
            "one nitpicking tho, ",
            "one nit, ",
            "nit: ",
        ];
        let Some(prefix) = prefixes.iter().find(|prefix| lower.starts_with(**prefix)) else {
            break;
        };
        text.drain(..prefix.len());
        let leading_ws = text.len() - text.trim_start().len();
        text.drain(..leading_ws);
    }
    lower_first_ascii(text.trim().trim_end_matches(['.', '!', '?']).trim())
}

fn directive_candidate_score(candidate: &str) -> usize {
    let trimmed = candidate.trim();
    if trimmed.is_empty() || is_bad_review_directive(trimmed) {
        return 0;
    }
    let lower = trimmed.to_ascii_lowercase();
    if is_import_review_noise_comment(trimmed) || is_pr_process_noise(&lower) {
        return 0;
    }
    let mut score = 0usize;
    if contains_any(
        &lower,
        &[
            "should",
            "need to",
            "needs to",
            "must",
            "please",
            "make sure",
            "ensure",
            "avoid",
            "prefer",
            "instead",
            "rather than",
            "don't",
            "do not",
        ],
    ) {
        score += 3;
    }
    if lower.starts_with("add ")
        || lower.starts_with("rename ")
        || lower.starts_with("replace ")
        || lower.starts_with("remove ")
        || lower.starts_with("keep ")
        || lower.starts_with("drop ")
        || lower.starts_with("move ")
        || lower.starts_with("extract ")
        || lower.starts_with("document ")
        || lower.starts_with("mention ")
        || lower.starts_with("mark ")
        || lower.starts_with("fixed ")
        || lower.starts_with("return ")
        || lower.starts_with("handle ")
        || lower.starts_with("validate ")
        || lower.starts_with("verify ")
        || lower.starts_with("assert ")
        || lower.starts_with("guard ")
    {
        score += 4;
    }
    if contains_any(
        &lower,
        &[
            " test",
            " regression",
            " panic",
            " allocation",
            " slowdown",
            " security",
            " consistent",
            " behavior",
            " public api",
        ],
    ) {
        score += 1;
    }
    if trimmed.contains('`') {
        score += 1;
    }
    score
}

fn best_review_directive_sentence(content: &str) -> Option<String> {
    best_review_directive_scored(content).map(|(_, candidate)| candidate)
}

/// Like [`best_review_directive_sentence`] but also returns the winning
/// directive's `directive_candidate_score`. The confidence gate uses the
/// raw score to award a small bonus for unusually strong directives.
fn best_review_directive_scored(content: &str) -> Option<(usize, String)> {
    let mut best: Option<(usize, String)> = None;
    for sentence in review_sentences(content).into_iter().take(16) {
        let candidate = normalize_review_directive_sentence(&sentence);
        let score = directive_candidate_score(&candidate);
        if score >= 4 {
            return Some((score, candidate));
        }
        if score > 0
            && best
                .as_ref()
                .is_none_or(|(best_score, _)| score > *best_score)
        {
            best = Some((score, candidate));
        }
    }
    best
}

fn reviewer_evidence_excerpt(content: &str, directive: &str) -> String {
    let sentences = review_sentences(content);
    for (index, sentence) in sentences.iter().enumerate() {
        if normalize_review_directive_sentence(sentence) == directive {
            return sentences
                .iter()
                .skip(index)
                .take(3)
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(" ");
        }
    }
    clean_review_comment(content)
}

fn truncate_chars(input: &str, max: usize) -> String {
    if input.chars().count() <= max {
        return input.to_owned();
    }
    let mut out = input
        .chars()
        .take(max.saturating_sub(3))
        .collect::<String>();
    out.push_str("...");
    out
}

fn related_files_body_line(scope_paths: &[String]) -> Option<String> {
    let related = scope_paths.get(1..)?;
    if related.is_empty() {
        return None;
    }
    let shown = related
        .iter()
        .take(LOCAL_CANDIDATE_RELATED_FILES_BODY_LIMIT)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    let remaining = related
        .len()
        .saturating_sub(LOCAL_CANDIDATE_RELATED_FILES_BODY_LIMIT);
    if remaining == 0 {
        Some(format!("Related files: {shown}\n"))
    } else {
        Some(format!("Related files: {shown}, and {remaining} more\n"))
    }
}

pub(super) fn candidate_title(content: &str, fallback_path: &str) -> String {
    let directive = review_directive(content);
    if directive.chars().count() >= 12 && directive != FALLBACK_REVIEW_DIRECTIVE {
        format!(
            "Review: {}",
            truncate_chars(&upper_first_ascii(&directive), 76)
        )
    } else if fallback_path.trim().is_empty() {
        "Review rule from imported PR comment".to_owned()
    } else {
        let sentence = first_sentence(content);
        if sentence.chars().count() >= 12 {
            format!("Review: {}", truncate_chars(&sentence, 76))
        } else {
            format!("Review rule for {}", truncate_chars(fallback_path, 64))
        }
    }
}

const fn review_directive_prefixes() -> &'static [&'static str] {
    &[
        "please make sure to ",
        "please make sure ",
        "please ensure that ",
        "please ensure ",
        "please ",
        "we should ",
        "you should ",
        "should ",
        "must ",
        "can you ",
        "can we ",
        "could you ",
        "could we ",
        "would you ",
        "i think we should ",
        "i think we can ",
        "we need to ",
        "you need to ",
        "it needs to be ",
        "it needs to ",
        "needs to be ",
        "needs to ",
        "need to ",
        "make sure to ",
        "make sure ",
        "ensure that ",
        "ensure ",
    ]
}

fn trim_review_directive_prefixes(mut value: String) -> String {
    loop {
        let mut changed = false;
        for prefix in review_directive_prefixes() {
            let trimmed = trim_ascii_prefix_ci(&value, prefix);
            if trimmed != value {
                value = trimmed;
                changed = true;
                break;
            }
        }
        if !changed {
            return value;
        }
    }
}

fn upper_first_ascii(value: &str) -> String {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    if first.is_ascii_lowercase() {
        format!(
            "{}{}",
            first.to_ascii_uppercase(),
            chars.collect::<String>()
        )
    } else {
        value.to_owned()
    }
}

#[cfg(test)]
pub(super) fn distilled_rule_statement(content: &str, path: &str) -> String {
    let directive = review_directive(content);
    distilled_rule_statement_from_directive(&directive, path)
}

pub(super) fn distilled_rule_statement_from_directive(directive: &str, _path: &str) -> String {
    let statement = directive.trim().trim_end_matches(['.', '!', '?']).trim();
    if statement.is_empty() {
        return format!("{FALLBACK_REVIEW_DIRECTIVE}.");
    }
    format!("{}.", upper_first_ascii(statement))
}

fn review_directive(content: &str) -> String {
    let mut text =
        best_review_directive_sentence(content).unwrap_or_else(|| first_sentence(content));
    text = trim_leading_praise_clause(text);
    text = trim_review_directive_prefixes(text);
    let text = text.trim().trim_end_matches(['.', '!', '?']).to_owned();
    let text = lower_first_ascii(&text);
    if text.trim().is_empty() || is_bad_review_directive(&text) {
        FALLBACK_REVIEW_DIRECTIVE.to_owned()
    } else {
        truncate_chars(text.trim(), 280)
    }
}

fn trim_leading_praise_clause(value: String) -> String {
    let lower = value.to_ascii_lowercase();
    let praise_prefix = lower.starts_with("this is great")
        || lower.starts_with("this is excellent")
        || lower.starts_with("this looks good")
        || lower.starts_with("thanks")
        || lower.starts_with("thank you");
    if !praise_prefix {
        return value;
    }
    let Some((idx, _)) = lower
        .match_indices(", but ")
        .next()
        .or_else(|| lower.match_indices(" but ").next())
    else {
        return value;
    };
    value[idx + lower[idx..].find("but ").unwrap_or(0) + "but ".len()..]
        .trim()
        .to_owned()
}

fn is_bad_review_directive(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.chars().count() < 12 {
        return true;
    }
    let letters = trimmed.chars().filter(|ch| ch.is_alphabetic()).count();
    if letters < 8 {
        return true;
    }
    let lower = trimmed.to_ascii_lowercase();
    let addressed = strip_leading_review_address(trimmed).to_ascii_lowercase();
    is_bad_review_directive_prefix(&lower)
        || is_bad_review_directive_prefix(&addressed)
        || is_pr_process_noise(&lower)
        || contains_any(
            &lower,
            &[
                "massive thank you",
                "thank you for your work",
                "wonderful work",
                "nice work",
                "looks okay to me",
                "code looks okay",
                "works great",
                "it's good",
                "i don't have any suggestions",
                "i tested this",
                "i can confirm",
                "confirmed that this now works",
            ],
        )
        || lower.contains("some comments are outside the diff")
}

fn strip_leading_review_address(value: &str) -> &str {
    let trimmed = value.trim_start();
    if !trimmed.starts_with('@') {
        return trimmed;
    }
    let Some((idx, ch)) = trimmed
        .char_indices()
        .find(|(_, ch)| ch.is_whitespace() || matches!(ch, ':' | ','))
    else {
        return trimmed;
    };
    trimmed[idx + ch.len_utf8()..]
        .trim_start()
        .trim_start_matches([':', ',', '-'])
        .trim_start()
}

fn is_bad_review_directive_prefix(lower: &str) -> bool {
    lower.starts_with("view ")
        || lower.starts_with("show ")
        || lower.starts_with("expand ")
        || lower.starts_with("collapse ")
        || lower.starts_with("thanks for")
        || lower.starts_with("thanks @")
        || lower.starts_with("thank you")
        || lower.starts_with("hi, thanks")
        || lower.starts_with("hi thanks")
        || (lower.starts_with("hi ") && lower.contains("thanks"))
        || lower.starts_with("apologies for")
        || lower.starts_with("sorry for")
        || lower.starts_with("you're right")
        || lower.starts_with("you are right")
        || lower.starts_with("overall lgtm")
        || lower.starts_with("lgtm")
        || lower.starts_with("looks good")
        || lower.starts_with("in the end, we use")
        || lower.starts_with("i tested this")
        || lower.starts_with("i can confirm")
        || lower.starts_with("i don't have any suggestions")
        || lower.starts_with("no suggestions")
        || lower.starts_with("https://")
        || lower.starts_with("http://")
        || lower.starts_with("ok -- it is fine")
        || lower.starts_with("ok - it is fine")
        || lower.starts_with("be fine as-is")
        || lower.starts_with("fine as-is")
        || lower.starts_with("fixed in ")
        || lower.starts_with("addressed in ")
        || lower.starts_with("resolved in ")
        || lower.starts_with("i pushed")
        || lower.starts_with("i updated")
        || lower.starts_with("i fixed")
        || lower.starts_with("i have updated")
        || lower.starts_with("i've updated")
        || lower.starts_with("outside diff range")
}

fn trim_ascii_prefix_ci(value: &str, prefix: &str) -> String {
    value
        .get(..prefix.len())
        .filter(|head| head.eq_ignore_ascii_case(prefix))
        .map_or_else(
            || value.to_owned(),
            |_| value[prefix.len()..].trim_start().to_owned(),
        )
}

fn lower_first_ascii(value: &str) -> String {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    if first.is_ascii_uppercase() {
        format!(
            "{}{}",
            first.to_ascii_lowercase(),
            chars.collect::<String>()
        )
    } else {
        value.to_owned()
    }
}

fn is_pr_author_response(item: &ReviewItemWithComments, comment: &ReviewCommentRecord) -> bool {
    let Some(pr_author) = item
        .item
        .author
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return false;
    };
    let Some(comment_author) = comment
        .author
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return false;
    };
    pr_author.eq_ignore_ascii_case(comment_author)
}

/// Whether a comment author name looks like an automated reviewer/bot. Used as
/// a small negative confidence feature (not a veto), so a bot can still pass on
/// strong, adopted content.
fn is_bot_author(author: Option<&str>) -> bool {
    let Some(author) = author.map(str::trim).filter(|s| !s.is_empty()) else {
        return false;
    };
    let lower = author.to_ascii_lowercase();
    lower == "github-actions"
        || lower == "github-actions[bot]"
        || lower.ends_with("[bot]")
        || lower.ends_with("-bot")
        || lower.contains("codecov")
        || lower.contains("coderabbit")
        || lower.contains("code-rabbit")
        || lower.contains("dependabot")
        || lower.contains("renovate")
}

/// Correctness/durability signal recovered from a comment's metadata JSON
/// (written by `ingest::github`). Every field degrades to neutral when the
/// key is absent so pre-signal imports (and older API shapes) score the
/// same as a comment that genuinely had no signal.
#[derive(Debug, Default)]
struct CaptureDurabilitySignal {
    resolved: bool,
    reactions_total: i64,
    thumbs_up: i64,
    thumbs_down: i64,
    later_replies: Vec<String>,
}

fn parse_durability_signal(comment: &ReviewCommentRecord) -> CaptureDurabilitySignal {
    let Some(metadata) = comment.metadata.as_deref() else {
        return CaptureDurabilitySignal::default();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(metadata) else {
        return CaptureDurabilitySignal::default();
    };
    let i64_field = |key: &str| {
        value
            .get(key)
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0)
    };
    CaptureDurabilitySignal {
        resolved: value
            .get("resolved")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        reactions_total: i64_field("reactionsTotal"),
        thumbs_up: i64_field("thumbsUp"),
        thumbs_down: i64_field("thumbsDown"),
        later_replies: value
            .get("laterReplies")
            .and_then(serde_json::Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(ToOwned::to_owned))
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// Detect a later reply in the same thread that retracts/contradicts the
/// original directive. Conservative substring match against a fixed set of
/// retraction markers so an unrelated discussion reply doesn't trip it.
fn replies_contradict(later_replies: &[String]) -> bool {
    const RETRACTION_MARKERS: &[&str] = &[
        "actually no",
        "actually, no",
        "nvm",
        "never mind",
        "nevermind",
        "disregard",
        "ignore that",
        "ignore this",
        "revert",
        "that's wrong",
        "thats wrong",
        "this is wrong",
        "scratch that",
    ];
    later_replies.iter().any(|reply| {
        let lower = reply.to_ascii_lowercase();
        RETRACTION_MARKERS
            .iter()
            .any(|marker| lower.contains(marker))
    })
}

/// Compute the per-comment capture confidence in [0.0, 1.0] from the
/// directive strength, adoption proxy (resolved thread), reaction approval,
/// later-reply contradiction, and bot authorship.
fn capture_confidence(
    directive_score: usize,
    is_bot: bool,
    signal: &CaptureDurabilitySignal,
) -> f32 {
    let mut score = CAPTURE_CONFIDENCE_BASE;
    if directive_score >= 6 {
        score += CAPTURE_BONUS_STRONG_DIRECTIVE;
    }
    if signal.resolved {
        score += CAPTURE_BONUS_RESOLVED;
    }
    // Approval / disapproval keyed on the net thumbs balance. Both branches
    // guard on `reactions_total > 0` so phantom thumbs with no actual reactions
    // mint neither. A strict majority is required (a lone 👎 or a tie yields
    // nothing), so a single down-react can't sink a strong resolved directive,
    // but a clear net-negative still lowers confidence rather than pass inert.
    if signal.reactions_total > 0 {
        if signal.thumbs_up > signal.thumbs_down && signal.thumbs_up > 0 {
            score += CAPTURE_BONUS_APPROVAL;
        } else if signal.thumbs_down > signal.thumbs_up {
            score -= CAPTURE_PENALTY_DISAPPROVAL;
        }
    }
    if replies_contradict(&signal.later_replies) {
        score -= CAPTURE_PENALTY_CONTRADICTION;
    }
    if is_bot {
        score -= CAPTURE_PENALTY_BOT;
    }
    score.clamp(0.0, 1.0)
}

/// 3-way route a scored comment takes through the candidate pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CaptureRoute {
    /// `>= HIGH`: promote to an active rule immediately.
    Active,
    /// `[LOW, HIGH)`: keep as a pending candidate for review.
    Candidate,
    /// `< LOW`: drop without drafting.
    Drop,
}

pub(super) fn route_for_confidence(confidence: f32) -> CaptureRoute {
    if confidence >= CAPTURE_CONFIDENCE_HIGH {
        CaptureRoute::Active
    } else if confidence >= CAPTURE_CONFIDENCE_LOW {
        CaptureRoute::Candidate
    } else {
        CaptureRoute::Drop
    }
}

/// A drafted local candidate plus the confidence/route the gate assigned it.
pub(super) struct LocalCandidate {
    pub(super) input: RememberRuleInput,
    pub(super) confidence: f32,
    pub(super) route: CaptureRoute,
}

pub(super) fn local_candidate_input(
    item: &ReviewItemWithComments,
    comment: &ReviewCommentRecord,
    source_repo: &RepoScope,
) -> Option<LocalCandidate> {
    let source_repo = source_repo.as_str();
    if is_pr_author_response(item, comment) {
        return None;
    }
    // Bot authorship is folded into the capture-confidence score below as a
    // small negative feature rather than a veto. The CONTENT noise pre-filters
    // below still drop genuine noise regardless of source.
    if is_platform_review_wrapper_comment(&comment.content) {
        return None;
    }
    let scope_paths = candidate_scope_paths(item, comment);
    if scope_paths.is_empty() {
        return None;
    }
    if !is_high_signal_review_comment_for_paths(&comment.content, &scope_paths) {
        return None;
    }
    let (directive_score, directive) = match best_review_directive_scored(&comment.content) {
        Some((score, _)) => (score, review_directive(&comment.content)),
        None => return None,
    };
    if directive == FALLBACK_REVIEW_DIRECTIVE {
        return None;
    }

    // ── Correctness-aware confidence + routing ──────────────────────────────
    let signal = parse_durability_signal(comment);
    let is_bot = is_bot_author(comment.author.as_deref());
    let confidence = capture_confidence(directive_score, is_bot, &signal);
    let route = route_for_confidence(confidence);
    if route == CaptureRoute::Drop {
        return None;
    }

    let path = scope_paths
        .first()
        .cloned()
        .unwrap_or_else(|| item.item.file_path.clone());
    let mut body = String::new();
    let source_repo_line = if source_repo.trim().is_empty() {
        item.item.repo_full_name.as_deref().unwrap_or("unknown")
    } else {
        source_repo
    };
    body.push_str("Rule:\n");
    body.push_str(&distilled_rule_statement_from_directive(&directive, &path));
    body.push_str("\n\nSource evidence:\n");
    body.push_str(&format!(
        "Source: {}#{}\n",
        source_repo_line,
        item.item
            .pr_number
            .map_or_else(|| "?".to_owned(), |n| n.to_string())
    ));
    if let Some(url) = comment
        .comment_url
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        body.push_str(&format!("Comment: {url}\n"));
    }
    if !path.trim().is_empty() {
        body.push_str(&format!("File: {path}\n"));
    }
    if let Some(line) = related_files_body_line(&scope_paths) {
        body.push_str(&line);
    }
    body.push_str("\nReviewer said:\n");
    body.push_str(&truncate_chars(
        &reviewer_evidence_excerpt(&comment.content, &directive),
        1_200,
    ));

    let widen_for_upstream = item
        .item
        .repo_full_name
        .as_deref()
        .and_then(|repo| candidate_repo_scope_for_comparison(repo, source_repo))
        .is_some_and(|repo| repo.as_str() != source_repo);
    let file_patterns = candidate_file_patterns(&scope_paths, widen_for_upstream);
    // Pre-persist secret barrier: the title and body carry reviewer-quoted
    // prose / code snippets that may contain a leaked credential. Scrub it
    // BEFORE the candidate is written to the local SQLite skills store (and
    // lazily embedded), mirroring the cloud's pre-persist `redactSecrets`.
    let input = RememberRuleInput {
        title: difflore_core::observability::privacy::redact_secrets(&candidate_title(
            &comment.content,
            &path,
        )),
        body: difflore_core::observability::privacy::redact_secrets(&body),
        file_patterns,
        bad_code: None,
        good_code: None,
        severity: Some("medium".to_owned()),
        kind: None,
        category: None,
        origin: Some("pr_review".to_owned()),
        captured_by_client: Some("import-reviews".to_owned()),
    };
    Some(LocalCandidate {
        input,
        confidence,
        route,
    })
}

fn candidate_repo_scope_for_comparison(repo: &str, source_repo: &str) -> Option<RepoScope> {
    if let Some((host, _)) = source_repo.split_once('/')
        && host.contains('.')
        && let Some(scope) = RepoScope::gitlab(host, repo)
    {
        return Some(scope);
    }
    RepoScope::canonical(repo)
}

fn candidate_file_patterns(
    scope_paths: &[String],
    widen_for_upstream: bool,
) -> Option<Vec<String>> {
    // Keep both narrow and sibling-broadening patterns for monorepos, but
    // never let a large PR-summary review exceed remember_rule's schema cap.
    let mut patterns = Vec::new();
    let mut seen_patterns = std::collections::HashSet::new();
    for scope_path in scope_paths {
        for pattern in file_patterns_from_path(scope_path) {
            if !seen_patterns.insert(pattern.clone()) {
                continue;
            }
            patterns.push(pattern);
            if patterns.len() >= difflore_core::skills::REMEMBER_FILE_PATTERN_LIMIT {
                return Some(patterns);
            }
        }
        if widen_for_upstream
            && let Some(pattern) = repo_wide_file_pattern_from_path(scope_path)
            && seen_patterns.insert(pattern.clone())
        {
            patterns.push(pattern);
            if patterns.len() >= difflore_core::skills::REMEMBER_FILE_PATTERN_LIMIT {
                return Some(patterns);
            }
        }
    }
    (!patterns.is_empty()).then_some(patterns)
}

pub(super) async fn run_local_candidates(
    db: &SqlitePool,
    source: &str,
    repo: &str,
    source_repo: &RepoScope,
    max_candidates: usize,
    pr_numbers: &[i32],
    exclude_prs: &std::collections::HashSet<i32>,
) -> LocalCandidateProgress {
    use difflore_core::review_store;
    use std::collections::HashSet;

    let items = match review_store::list_by_source_with_comments(
        db,
        review_store::ReviewSourceInput {
            source: source.into(),
        },
    )
    .await
    {
        Ok(v) => v,
        Err(e) => exit_err(&format!("failed to load imported reviews: {e}")),
    };

    let mut progress = LocalCandidateProgress {
        budget: max_candidates,
        ..LocalCandidateProgress::default()
    };
    let mut seen_candidate_signatures: HashSet<String> = HashSet::new();
    let target_pr_numbers = pr_numbers.iter().copied().collect::<HashSet<_>>();
    'items: for item in items
        .iter()
        .filter(|item| item.item.repo_full_name.as_deref() == Some(repo))
        // Leak-free eval: drop excluded PRs BEFORE their comments are turned
        // into candidates so an excluded PR contributes zero rules.
        .filter(|item| {
            item.item
                .pr_number
                .is_none_or(|n| !exclude_prs.contains(&n))
        })
        .filter(|item| {
            target_pr_numbers.is_empty()
                || item
                    .item
                    .pr_number
                    .is_some_and(|n| target_pr_numbers.contains(&n))
        })
    {
        for comment in &item.comments {
            progress.comments_considered += 1;
            // `local_candidate_input` already dropped LOW-confidence comments
            // (returns None), so anything here routes to Active or Candidate.
            let Some(LocalCandidate {
                input,
                confidence,
                route,
            }) = local_candidate_input(item, comment, source_repo)
            else {
                progress.comments_skipped += 1;
                continue;
            };
            let signature = local_candidate_dedupe_signature(&input);
            // Honour rejection tombstones BEFORE claiming the in-run dedupe
            // signature. A rejected comment must be skipped WITHOUT consuming
            // its signature: otherwise a later comment that distills to the
            // same rule but carries different (non-rejected) source evidence
            // would be dropped as an in-run duplicate, making whether valid
            // re-evidence survives depend on comment order. The content-hash
            // dedup in `remember_inner` only matches *surviving* pending rows,
            // so a rejected (deleted) draft would otherwise be re-created here.
            match difflore_core::skills::is_rejected_signature(db, &input).await {
                Ok(true) => {
                    progress.candidates_suppressed_rejected += 1;
                    continue;
                }
                Ok(false) => {}
                Err(e) => {
                    exit_err(&format!("failed to check rejection tombstone: {e}"));
                }
            }
            if seen_candidate_signatures.contains(&signature) {
                progress.candidates_duplicate_in_run += 1;
                continue;
            }
            seen_candidate_signatures.insert(signature);
            // Seed the draft with the gate's capture confidence instead of the
            // flat conversation default, then route: HIGH → promote to active,
            // MEDIUM → leave as a pending candidate for review.
            match difflore_core::skills::remember_as_candidate_with_confidence_for_repo(
                db,
                input,
                confidence,
                source_repo,
            )
            .await
            {
                Ok(outcome) => {
                    if outcome.deduped {
                        if outcome.matched_existing_active {
                            // Re-import of an already-approved rule: collapsed
                            // into the active rule, left untouched (no bump).
                            progress.candidates_matched_active += 1;
                        } else {
                            progress.candidates_deduped += 1;
                        }
                    } else {
                        match route {
                            CaptureRoute::Active => {
                                if let Err(e) =
                                    difflore_core::skills::promote_candidate(db, &outcome.skill.id)
                                        .await
                                {
                                    exit_err(&format!("failed to activate imported memory: {e}"));
                                }
                                progress.candidates_activated += 1;
                            }
                            // MEDIUM band: keep quarantined as a pending draft;
                            // do NOT promote.
                            CaptureRoute::Candidate => {
                                progress.candidates_pending += 1;
                            }
                            // Dropped comments never reach here.
                            CaptureRoute::Drop => {}
                        }
                        progress.candidates_created += 1;
                    }
                }
                Err(e) => exit_err(&format!("failed to create local memory: {e}")),
            }
            if local_candidate_budget_reached(&progress) {
                progress.capped = true;
                break 'items;
            }
        }
    }
    progress
}

pub(super) fn local_candidate_dedupe_signature(input: &RememberRuleInput) -> String {
    let rule_statement = input
        .body
        .split("\n\nSource evidence:")
        .next()
        .unwrap_or(input.body.as_str());
    let patterns = input
        .file_patterns
        .as_ref()
        .map(|items| items.join("\0"))
        .unwrap_or_default();
    format!(
        "{}\n{}\n{}",
        normalize_candidate_signature_part(&input.title),
        normalize_candidate_signature_part(rule_statement),
        patterns
    )
}

fn normalize_candidate_signature_part(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(super) fn print_local_candidate_next_steps(progress: &LocalCandidateProgress, repo: &str) {
    println!();
    if progress.candidates_created == 0
        && progress.candidates_deduped == 0
        && progress.candidates_duplicate_in_run == 0
        && progress.candidates_matched_active == 0
    {
        // Nothing landed AND nothing matched an existing rule. If the only
        // reason is that every match was a previously-rejected suggestion,
        // say so plainly rather than nudging the user to widen/upload —
        // the import worked, it just honored their earlier rejections.
        if progress.candidates_suppressed_rejected > 0 {
            println!(
                "  {} No new local rules — {} previously-rejected suggestion{} skipped.",
                style::emerald(style::sym::OK),
                progress.candidates_suppressed_rejected,
                if progress.candidates_suppressed_rejected == 1 {
                    ""
                } else {
                    "s"
                },
            );
            return;
        }
        println!(
            "  {} No local rules created from the imported comments.",
            style::pewter(style::sym::BULLET),
        );
        style::println_wrapped(&format!(
            "  {} Try a larger import window, or upload reviews so DiffLore can find deeper team patterns.",
            style::pewter(style::sym::BULLET),
        ));
        println!("    {}", style::cmd("difflore import-reviews --upload"));
        style::println_wrapped(&format!(
            "  {} Or see recall fire on a bundled sample in seconds: {}",
            style::pewter(style::sym::BULLET),
            style::cmd("difflore try"),
        ));
        return;
    }

    if progress.candidates_created == 0 {
        println!(
            "  {} No new local review memories created.",
            style::emerald(style::sym::OK),
        );
        if progress.candidates_deduped > 0 {
            println!(
                "  {} strengthened existing memories: {}",
                style::pewter(style::sym::BULLET),
                progress.candidates_deduped,
            );
        }
    } else {
        println!(
            "  {} Created {} local review memor{}.",
            style::emerald(style::sym::OK),
            progress.candidates_created,
            if progress.candidates_created == 1 {
                "y"
            } else {
                "ies"
            },
        );
        println!(
            "  +{} local memory write{} ({} active, {} pending, {} strengthened).",
            progress.candidates_created,
            if progress.candidates_created == 1 {
                ""
            } else {
                "s"
            },
            progress.candidates_activated,
            progress.candidates_pending,
            progress.candidates_deduped,
        );
        println!(
            "  {} active: {}",
            style::pewter(style::sym::BULLET),
            progress.candidates_activated,
        );
        if progress.candidates_activated > 0 {
            style::println_wrapped(&format!(
                "    already guiding agents for {}.",
                style::pewter(repo),
            ));
        }
        println!(
            "  {} pending review: {}",
            style::pewter(style::sym::BULLET),
            progress.candidates_pending,
        );
        if progress.candidates_pending > 0 {
            style::println_wrapped("    waiting for you or an agent to inspect before activation.");
        }
        println!(
            "  {} strengthened: {}",
            style::pewter(style::sym::BULLET),
            progress.candidates_deduped,
        );
    }
    if progress.candidates_duplicate_in_run > 0 {
        println!(
            "  {} in-run duplicate comments: {}",
            style::pewter(style::sym::BULLET),
            progress.candidates_duplicate_in_run,
        );
    }
    println!(
        "  {} skipped comments: {}",
        style::pewter(style::sym::BULLET),
        progress.comments_skipped,
    );
    println!(
        "  {} local budget: {}",
        style::pewter(style::sym::BULLET),
        progress.budget,
    );
    if progress.candidates_pending > 0 {
        let (prefix, command, suffix) = pending_drafts_review_hint(progress.candidates_pending);
        style::println_wrapped(&format!(
            "  {} {prefix}{}{suffix}",
            style::pewter(style::sym::BULLET),
            style::cmd(&command),
        ));
    }
    if progress.candidates_deduped > 0 {
        style::println_wrapped(&format!(
            "  {} strengthened means matching existing memories were reinforced instead of repeated.",
            style::pewter(style::sym::BULLET),
        ));
    }
    if progress.candidates_duplicate_in_run > 0 {
        style::println_wrapped(&format!(
            "  {} in-run duplicates matched a candidate already handled in this import and were skipped before touching the store.",
            style::pewter(style::sym::BULLET),
        ));
    }
    if progress.candidates_matched_active > 0 {
        style::println_wrapped(&format!(
            "  {} {} already-active rule{} matched again — left unchanged, not strengthened.",
            style::pewter(style::sym::BULLET),
            progress.candidates_matched_active,
            if progress.candidates_matched_active == 1 {
                ""
            } else {
                "s"
            },
        ));
    }
    if progress.candidates_suppressed_rejected > 0 {
        style::println_wrapped(&format!(
            "  {} {} previously-rejected suggestion{} skipped.",
            style::pewter(style::sym::BULLET),
            progress.candidates_suppressed_rejected,
            if progress.candidates_suppressed_rejected == 1 {
                ""
            } else {
                "s"
            },
        ));
    }
    if progress.capped {
        style::println_wrapped(&format!(
            "  {} hit the local memory budget.",
            style::pewter(style::sym::BULLET),
        ));
        style::println_wrapped(&format!(
            "    Raise {} or import targeted PRs with {}.",
            style::cmd("--max-prs <N>"),
            style::cmd("--pr <NUMBER>"),
        ));
    }
    println!();
    if progress.candidates_activated > 0 {
        println!(
            "  {} Agents can use approved local memory now:",
            style::emerald(style::sym::TIP),
        );
        for cmd in active_candidate_next_step_commands() {
            println!("    {}", style::cmd(cmd));
        }
        if progress.candidates_pending > 0 {
            println!(
                "  {} Review remaining pending memory before agents use it:",
                style::emerald(style::sym::TIP),
            );
            for cmd in pending_candidate_next_step_commands(repo) {
                println!("    {}", style::cmd(&cmd));
            }
        }
    } else if progress.candidates_pending > 0 {
        println!(
            "  {} Review pending memory before agents use it:",
            style::emerald(style::sym::TIP),
        );
        for cmd in pending_candidate_next_step_commands(repo) {
            println!("    {}", style::cmd(&cmd));
        }
    }
}

pub(super) const fn active_candidate_next_step_commands() -> &'static [&'static str] {
    &[
        "difflore status",
        "difflore memory active",
        "difflore recall --diff",
        "difflore review --diff all",
    ]
}

pub(super) fn pending_candidate_next_step_commands(repo: &str) -> Vec<String> {
    vec![
        "difflore memory review".to_owned(),
        format!("difflore drafts list --repo {repo} --json"),
        format!("difflore drafts approve --all --repo {repo} --yes"),
    ]
}

/// Pure copy for the "N medium-confidence drafts held for review" line that
/// follows a local import. Returned as `(prefix, command, suffix)` so the
/// caller wraps the command token in `style::cmd` while the wording stays
/// string-testable. The command must be one that exists, and the wording must
/// preserve the explicit local review/approval boundary.
pub(super) fn pending_drafts_review_hint(count: usize) -> (String, String, &'static str) {
    let plural = if count == 1 { "" } else { "s" };
    (
        format!("{count} medium-confidence draft{plural} held for review; decide in "),
        "difflore memory review".to_owned(),
        ", or let an agent inspect with `difflore drafts list --json`.",
    )
}

pub(super) fn local_candidate_budget(v: &ValidatedArgs) -> usize {
    LOCAL_CANDIDATE_DEFAULT_MIN.max(v.max_prs.saturating_mul(2))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signal() -> CaptureDurabilitySignal {
        CaptureDurabilitySignal::default()
    }

    fn assert_confidence(actual: f32, expected: f32) {
        assert!(
            (actual - expected).abs() < 0.000_01,
            "expected confidence {expected}, got {actual}"
        );
    }

    #[test]
    fn capture_route_uses_inclusive_high_and_low_thresholds() {
        assert_eq!(
            route_for_confidence(CAPTURE_CONFIDENCE_HIGH),
            CaptureRoute::Active
        );
        assert_eq!(
            route_for_confidence(CAPTURE_CONFIDENCE_HIGH - 0.001),
            CaptureRoute::Candidate
        );
        assert_eq!(
            route_for_confidence(CAPTURE_CONFIDENCE_LOW),
            CaptureRoute::Candidate
        );
        assert_eq!(
            route_for_confidence(CAPTURE_CONFIDENCE_LOW - 0.001),
            CaptureRoute::Drop
        );
    }

    #[test]
    fn capture_confidence_routes_resolved_human_directive_to_active() {
        let signal = CaptureDurabilitySignal {
            resolved: true,
            ..signal()
        };

        let confidence = capture_confidence(4, false, &signal);

        assert_confidence(confidence, 0.70);
        assert_eq!(route_for_confidence(confidence), CaptureRoute::Active);
    }

    #[test]
    fn bare_strong_directive_without_signal_is_quarantined_as_pending() {
        let confidence = capture_confidence(4, false, &signal());

        assert_confidence(confidence, CAPTURE_CONFIDENCE_BASE);
        assert_eq!(route_for_confidence(confidence), CaptureRoute::Candidate);
    }

    #[test]
    fn capture_confidence_requires_real_reactions_for_approval_bonus() {
        let phantom_approval = CaptureDurabilitySignal {
            thumbs_up: 3,
            thumbs_down: 0,
            reactions_total: 0,
            ..signal()
        };
        let real_approval = CaptureDurabilitySignal {
            thumbs_up: 3,
            thumbs_down: 0,
            reactions_total: 3,
            ..signal()
        };

        assert_confidence(capture_confidence(4, false, &phantom_approval), 0.50);
        assert_confidence(capture_confidence(4, false, &real_approval), 0.60);
    }

    #[test]
    fn capture_confidence_penalizes_clear_disapproval_contradiction_and_bot_authorship() {
        let signal = CaptureDurabilitySignal {
            reactions_total: 2,
            thumbs_up: 0,
            thumbs_down: 2,
            later_replies: vec!["Actually, no - ignore that suggestion.".to_owned()],
            ..signal()
        };

        let confidence = capture_confidence(4, true, &signal);

        assert!((confidence - 0.0).abs() < f32::EPSILON);
        assert_eq!(route_for_confidence(confidence), CaptureRoute::Drop);
    }

    #[test]
    fn strong_directive_bonus_branch_is_reachable_and_affects_route() {
        let directive = "Return `StatusNoContent` and add a regression test";
        let directive_score = directive_candidate_score(directive);

        assert!(
            directive_score >= 6,
            "expected directive score >= 6, got {directive_score}"
        );

        let confidence = capture_confidence(directive_score, false, &signal());

        assert_confidence(confidence, 0.55);
        assert_eq!(route_for_confidence(confidence), CaptureRoute::Candidate);
    }

    #[test]
    fn strong_directive_with_real_approval_can_auto_activate_without_resolved_thread() {
        let signal = CaptureDurabilitySignal {
            reactions_total: 1,
            thumbs_up: 1,
            thumbs_down: 0,
            ..signal()
        };

        let confidence = capture_confidence(6, false, &signal);

        assert_confidence(confidence, 0.65);
        assert_eq!(route_for_confidence(confidence), CaptureRoute::Active);
    }

    #[test]
    fn greptile_summary_block_is_rejected_as_noise() {
        let summary = clean_review_comment(
            "<h3>Greptile Summary</h3>\nThis PR adds an SSE channel.\n\
             <h3>Confidence Score: 4/5</h3>\n<h3>Important Files Changed</h3>",
        );
        assert!(
            is_import_review_noise_comment(&summary),
            "greptile summary should be rejected: {summary:?}"
        );
    }

    #[test]
    fn greptile_inline_finding_survives_noise_gate() {
        // After stripping the anchor, a real finding is a reusable convention
        // and must NOT be filtered — AI-reviewer findings are kept by decision.
        let finding = clean_review_comment(
            "<a href=\"https://app.greptile.com/x?V=7\" align=\"top\"></a> \
             Move the defer Close() to immediately after the successful Open so \
             the file handle is always released on the error path.",
        );
        assert!(
            !is_import_review_noise_comment(&finding),
            "clean finding should survive: {finding:?}"
        );
        assert!(!finding.contains('<'), "html residue remains: {finding:?}");
    }

    #[test]
    fn reviewer_meta_instruction_is_rejected() {
        let meta = clean_review_comment(
            "If you propose a fix, please make it concise and limit it to the changed lines.",
        );
        assert!(
            is_import_review_noise_comment(&meta),
            "reviewer meta-instruction should be rejected: {meta:?}"
        );
    }

    #[test]
    fn local_candidate_progress_keeps_in_run_duplicates_out_of_store_dedupes() {
        let progress = LocalCandidateProgress {
            candidates_duplicate_in_run: 1,
            candidates_deduped: 0,
            ..LocalCandidateProgress::default()
        };

        assert_eq!(progress.candidates_duplicate_in_run, 1);
        assert_eq!(
            progress.candidates_deduped, 0,
            "in-run duplicate comments must not be reported as store-level strengthening"
        );
    }
}
