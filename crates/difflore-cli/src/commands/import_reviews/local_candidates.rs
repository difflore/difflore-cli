use difflore_core::domain::models::RememberRuleInput;
use difflore_core::review_store::{ReviewCommentRecord, ReviewItemWithComments};
use sqlx::SqlitePool;

use crate::commands::review_text::strip_review_markdown_noise;
use crate::commands::util::exit_err;
use crate::style;

use super::ValidatedArgs;
use super::scope::{
    candidate_scope_paths, file_pattern_from_path, file_patterns_from_path,
    is_import_review_noise_line, is_review_table_wrapper_line, repo_wide_file_pattern_from_path,
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
    pub(super) candidates_deduped: usize,
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
            "pull request overview",
            "pr overview",
            "walkthrough",
            "automated review",
            "review skipped",
            "actionable comments posted",
        ],
    )
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

pub(super) fn distilled_rule_statement_from_directive(directive: &str, path: &str) -> String {
    let scope = file_pattern_from_path(path).unwrap_or_else(|| {
        let trimmed = path.trim();
        if trimmed.is_empty() {
            "this pattern".to_owned()
        } else {
            trimmed.replace('\\', "/")
        }
    });
    format!("When touching `{scope}`, {directive}.")
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
    source_repo: &str,
) -> Option<LocalCandidate> {
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
        .is_some_and(|repo| !repo.eq_ignore_ascii_case(source_repo));
    let file_patterns = candidate_file_patterns(&scope_paths, widen_for_upstream);
    // Pre-persist secret barrier: the title and body carry reviewer-quoted
    // prose / code snippets that may contain a leaked credential. Scrub it
    // BEFORE the candidate is written to the local SQLite skills store (and
    // lazily embedded), mirroring the cloud's pre-persist `redactSecrets`.
    let input = RememberRuleInput {
        title: difflore_core::observability::privacy::redact_secrets(&candidate_title(&comment.content, &path)),
        body: difflore_core::observability::privacy::redact_secrets(&body),
        file_patterns,
        bad_code: None,
        good_code: None,
        severity: Some("medium".to_owned()),
        origin: Some("pr_review".to_owned()),
    };
    Some(LocalCandidate {
        input,
        confidence,
        route,
    })
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

async fn attach_candidate_repo_scope(
    db: &SqlitePool,
    skill_id: &str,
    repo: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "UPDATE skills SET source_repo = ?1 WHERE id = ?2",
        repo,
        skill_id,
    )
    .execute(db)
    .await?;
    Ok(())
}

pub(super) async fn run_local_candidates(
    db: &SqlitePool,
    repo: &str,
    source_repo: &str,
    max_candidates: usize,
    pr_numbers: &[i32],
    exclude_prs: &std::collections::HashSet<i32>,
) -> LocalCandidateProgress {
    use difflore_core::review_store;
    use std::collections::HashSet;

    let items = match review_store::list_by_source_with_comments(
        db,
        review_store::ReviewSourceInput {
            source: "github".into(),
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
            if !seen_candidate_signatures.insert(signature) {
                progress.candidates_deduped += 1;
                continue;
            }
            // Seed the draft with the gate's capture confidence instead of the
            // flat conversation default, then route: HIGH → promote to active,
            // MEDIUM → leave as a pending candidate for review.
            match difflore_core::skills::remember_as_candidate_with_confidence(
                db, input, confidence,
            )
            .await
            {
                Ok(outcome) => {
                    if outcome.deduped {
                        progress.candidates_deduped += 1;
                    } else {
                        if let Err(e) =
                            attach_candidate_repo_scope(db, &outcome.skill.id, repo).await
                        {
                            exit_err(&format!("failed to attach local memory to repo: {e}"));
                        }
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

fn local_candidate_dedupe_signature(input: &RememberRuleInput) -> String {
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

pub(super) fn print_local_candidate_next_steps(progress: &LocalCandidateProgress) {
    println!();
    if progress.candidates_created == 0 && progress.candidates_deduped == 0 {
        println!(
            "  {} No local memories created from the imported comments.",
            style::pewter(style::sym::BULLET),
        );
        style::println_wrapped(&format!(
            "  {} Try a larger import window, or upload reviews so DiffLore can find deeper team patterns.",
            style::pewter(style::sym::BULLET),
        ));
        println!("    {}", style::cmd("difflore import-reviews --upload"));
        return;
    }

    if progress.candidates_created == 0 {
        println!(
            "  {} No new local review memories created.",
            style::emerald(style::sym::OK),
        );
        println!(
            "  {} strengthened existing memories: {}",
            style::pewter(style::sym::BULLET),
            progress.candidates_deduped,
        );
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
            "  {} active: {}",
            style::pewter(style::sym::BULLET),
            progress.candidates_activated,
        );
        println!(
            "  {} pending review: {}",
            style::pewter(style::sym::BULLET),
            progress.candidates_pending,
        );
        println!(
            "  {} deduped: {}",
            style::pewter(style::sym::BULLET),
            progress.candidates_deduped,
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
        // Review held-back drafts with `difflore status`. Drafts auto-activate
        // after enough adoption signal; there is no manual per-id accept command.
        let (prefix, command, suffix) = pending_drafts_review_hint(progress.candidates_pending);
        style::println_wrapped(&format!(
            "  {} {prefix}{}{suffix}",
            style::pewter(style::sym::BULLET),
            style::cmd(command),
        ));
    }
    if progress.candidates_deduped > 0 {
        style::println_wrapped(&format!(
            "  {} deduped means matching existing memories were strengthened instead of repeated.",
            style::pewter(style::sym::BULLET),
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
    println!(
        "  {} Agents can use this memory now:",
        style::emerald(style::sym::TIP),
    );
    for cmd in local_candidate_next_step_commands() {
        println!("    {}", style::cmd(cmd));
    }
}

pub(super) const fn local_candidate_next_step_commands() -> &'static [&'static str] {
    &[
        "difflore status",
        "difflore recall --diff",
        "difflore fix --preview",
    ]
}

/// Pure copy for the "N medium-confidence drafts held for review" line that
/// follows a local import. Returned as `(prefix, command, suffix)` so the
/// caller wraps the command token in `style::cmd` while the wording stays
/// string-testable. The command must be one that exists: `difflore status`
/// lists the drafts under "Pending memory drafts" (`difflore candidates` is
/// gone); a regression test pins this against ever naming it again.
pub(super) fn pending_drafts_review_hint(count: usize) -> (String, &'static str, &'static str) {
    let plural = if count == 1 { "" } else { "s" };
    (
        format!(
            "{count} medium-confidence draft{plural} held for review; see \"Pending memory drafts\" in "
        ),
        "difflore status",
        ".",
    )
}

pub(super) fn local_candidate_budget(v: &ValidatedArgs) -> usize {
    LOCAL_CANDIDATE_DEFAULT_MIN.max(v.max_prs.saturating_mul(2))
}
