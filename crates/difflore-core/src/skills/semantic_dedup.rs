//! Provider-neutral near-duplicate matching for review-imported rule drafts.
//!
//! GitHub and GitLab import paths both distill comments into the same local
//! rule shape before persistence. Matching at this layer avoids platform
//! keyword filters and keeps duplicate suppression tied to rule meaning and
//! path-hint evidence rather than comment source.

use std::collections::HashSet;

pub(crate) const SEMANTIC_DEDUP_SCORE_THRESHOLD: f32 = 0.58;
const TITLE_DEDUP_JACCARD_THRESHOLD: f32 = 0.70;
const CONTENT_DEDUP_JACCARD_THRESHOLD: f32 = 0.56;
const NORMALIZED_TITLE_CONTENT_FLOOR: f32 = 0.35;

#[derive(Debug, Clone)]
pub(crate) struct SemanticRuleKey {
    title: String,
    content: String,
    file_patterns: Vec<String>,
}

impl SemanticRuleKey {
    pub(crate) fn new(title: &str, content: &str, file_patterns: &[String]) -> Self {
        Self {
            title: title.to_owned(),
            content: semantic_rule_content(content),
            file_patterns: file_patterns.to_owned(),
        }
    }
}

pub(crate) fn semantic_rule_content(content: &str) -> String {
    content
        .split("\n\nSource evidence:")
        .next()
        .unwrap_or(content)
        .to_owned()
}

pub(crate) fn semantic_rules_match(
    incoming: &SemanticRuleKey,
    candidate: &SemanticRuleKey,
) -> bool {
    let incoming_title_key = normalized_title_key(&incoming.title);
    if !incoming_title_key.is_empty()
        && incoming_title_key == normalized_title_key(&candidate.title)
    {
        let scored = semantic_dedup_score(incoming, candidate);
        return scored.file_pattern_overlap > 0.0
            || (file_pattern_languages_compatible(
                &incoming.file_patterns,
                &candidate.file_patterns,
            ) && scored.content_jaccard >= NORMALIZED_TITLE_CONTENT_FLOOR);
    }

    let scored = semantic_dedup_score(incoming, candidate);
    if scored.file_pattern_overlap > 0.0
        && scored.content_jaccard >= CONTENT_DEDUP_JACCARD_THRESHOLD
    {
        return true;
    }
    let score = if scored.title_jaccard >= TITLE_DEDUP_JACCARD_THRESHOLD {
        scored.score.max(scored.title_jaccard)
    } else {
        scored.score
    };
    scored.file_pattern_overlap > 0.0 && score >= SEMANTIC_DEDUP_SCORE_THRESHOLD
}

#[derive(Debug, Clone, Copy)]
struct SemanticDedupScore {
    score: f32,
    title_jaccard: f32,
    content_jaccard: f32,
    file_pattern_overlap: f32,
}

fn semantic_dedup_score(
    incoming: &SemanticRuleKey,
    candidate: &SemanticRuleKey,
) -> SemanticDedupScore {
    let title_jaccard = jaccard(
        &tokenize_semantic_title(&incoming.title),
        &tokenize_semantic_title(&candidate.title),
    );
    let content_jaccard = jaccard(
        &tokenize_knowledge_text(&incoming.content),
        &tokenize_knowledge_text(&candidate.content),
    );
    let file_pattern_overlap =
        file_pattern_overlap_score(&incoming.file_patterns, &candidate.file_patterns);
    let weighted = 0.2f32.mul_add(
        file_pattern_overlap,
        0.42f32.mul_add(content_jaccard, 0.38 * title_jaccard),
    );
    let score = (weighted - 0.05).clamp(0.0, 1.0);

    SemanticDedupScore {
        score,
        title_jaccard,
        content_jaccard,
        file_pattern_overlap,
    }
}

pub(crate) fn normalized_title_key(title: &str) -> String {
    let mut tokens = tokenize_semantic_title(title)
        .into_iter()
        .collect::<Vec<_>>();
    tokens.sort();
    tokens.join(" ")
}

pub(crate) fn tokenize_semantic_title(title: &str) -> HashSet<String> {
    tokenize_with_allowed_chars(title, |ch| {
        ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_')
    })
}

pub(crate) fn tokenize_knowledge_text(text: &str) -> HashSet<String> {
    tokenize_with_allowed_chars(text, |ch| {
        ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '/' | ':' | '-')
    })
}

fn tokenize_with_allowed_chars(value: &str, allowed: impl Fn(char) -> bool) -> HashSet<String> {
    value
        .to_ascii_lowercase()
        .chars()
        .map(|ch| if allowed(ch) { ch } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .filter_map(normalize_lexical_token)
        .collect()
}

fn normalize_lexical_token(token: &str) -> Option<String> {
    let mut t = token
        .trim_matches(|ch: char| matches!(ch, '.' | '_' | ':' | '/' | '-'))
        .to_owned();
    if t.chars().all(|ch| ch.is_ascii_digit()) || is_low_information_id_token(&t) {
        return None;
    }
    if t.len() > 5 && t.ends_with("ies") {
        t.truncate(t.len() - 3);
        t.push('y');
    } else if t.len() > 6 && t.ends_with("ing") {
        t.truncate(t.len() - 3);
        trim_doubled_suffix(&mut t);
    } else if t.len() > 5 && t.ends_with("ed") {
        t.truncate(t.len() - 2);
        trim_doubled_suffix(&mut t);
    } else if t.len() > 5 && t.ends_with("es") {
        t.truncate(t.len() - 2);
    } else if t.len() > 4 && t.ends_with('s') {
        t.truncate(t.len() - 1);
    }
    (t.len() >= 3 && !SEMANTIC_DEDUP_STOPWORDS.contains(&t.as_str())).then_some(t)
}

fn trim_doubled_suffix(value: &mut String) {
    let mut chars = value.chars().rev();
    let Some(last) = chars.next() else {
        return;
    };
    if chars.next() == Some(last) {
        value.pop();
    }
}

fn is_low_information_id_token(token: &str) -> bool {
    token.len() >= 7 && token.chars().all(|ch| ch.is_ascii_hexdigit())
}

const SEMANTIC_DEDUP_STOPWORDS: &[&str] = &[
    "a", "an", "the", "to", "of", "in", "on", "for", "and", "or", "with", "is", "are", "be", "by",
    "from", "as", "at", "into", "onto", "this", "that", "these", "those", "it", "its", "use",
    "using", "review", "rule", "source", "evidence", "reviewer", "said", "touching",
];

pub(crate) fn jaccard(left: &HashSet<String>, right: &HashSet<String>) -> f32 {
    if left.is_empty() && right.is_empty() {
        return 0.0;
    }
    let intersection = left.intersection(right).count();
    let union = left.len() + right.len() - intersection;
    if union == 0 {
        0.0
    } else {
        intersection as f32 / union as f32
    }
}

fn file_pattern_overlap_score(incoming: &[String], existing: &[String]) -> f32 {
    let incoming = normalize_patterns(incoming);
    let existing = normalize_patterns(existing);
    if incoming.is_empty() || existing.is_empty() {
        return 0.0;
    }
    let mut best = 0.0f32;
    for left in &incoming {
        for right in &existing {
            best = best.max(file_pattern_pair_overlap_score(left, right));
        }
    }
    best
}

fn normalize_patterns(patterns: &[String]) -> Vec<String> {
    let mut out = patterns
        .iter()
        .map(|pattern| normalize_pattern_string(pattern))
        .filter(|pattern| !pattern.is_empty())
        .collect::<Vec<_>>();
    out.sort();
    out.dedup();
    out
}

fn normalize_pattern_string(pattern: &str) -> String {
    let mut out = pattern.trim().replace('\\', "/");
    while out.contains("//") {
        out = out.replace("//", "/");
    }
    while let Some(stripped) = out.strip_prefix("./") {
        out = stripped.to_owned();
    }
    out.trim_end_matches('/').to_owned()
}

fn file_pattern_pair_overlap_score(left: &str, right: &str) -> f32 {
    if left == right {
        return 1.0;
    }
    if !has_compatible_language_scope(left, right) {
        return 0.0;
    }
    if is_project_wide_pattern(left) || is_project_wide_pattern(right) {
        return 1.0;
    }

    let left_prefix = glob_prefix(left);
    let right_prefix = glob_prefix(right);
    if left_prefix == right_prefix {
        return 1.0;
    }
    if prefix_subsumes(&left_prefix, &right_prefix) {
        return 0.9;
    }
    if left_prefix.is_empty() || right_prefix.is_empty() {
        return 0.86;
    }
    0.0
}

fn is_project_wide_pattern(pattern: &str) -> bool {
    matches!(pattern, "**" | "**/*" | "**/**")
}

fn has_compatible_language_scope(left: &str, right: &str) -> bool {
    let left_families = language_families_for_pattern(left);
    let right_families = language_families_for_pattern(right);
    if left_families.is_empty() || right_families.is_empty() {
        return true;
    }
    left_families
        .iter()
        .any(|family| right_families.contains(family))
}

fn file_pattern_languages_compatible(incoming: &[String], existing: &[String]) -> bool {
    let incoming = normalize_patterns(incoming);
    let existing = normalize_patterns(existing);
    if incoming.is_empty() || existing.is_empty() {
        return false;
    }
    incoming.iter().any(|left| {
        existing
            .iter()
            .any(|right| has_compatible_language_scope(left, right))
    })
}

fn language_families_for_pattern(pattern: &str) -> HashSet<String> {
    extensions_for_pattern(pattern)
        .into_iter()
        .map(|ext| match ext.as_str() {
            "c" | "cc" | "cpp" | "cxx" | "h" | "hpp" | "hxx" => "c-family".to_owned(),
            "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" => "js-family".to_owned(),
            other => other.to_owned(),
        })
        .collect()
}

fn extensions_for_pattern(pattern: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    for part in pattern.split("*.").skip(1) {
        let ext = part
            .chars()
            .take_while(char::is_ascii_alphanumeric)
            .collect::<String>();
        if !ext.is_empty() {
            out.insert(ext.to_ascii_lowercase());
        }
    }

    let mut rest = pattern;
    while let Some(start) = rest.find("*.{") {
        let after = &rest[start + "*.{".len()..];
        let Some(end) = after.find('}') else {
            break;
        };
        for part in after[..end].split(',') {
            let ext = part.trim().trim_start_matches('.').to_ascii_lowercase();
            if !ext.is_empty() && ext.chars().all(|ch| ch.is_ascii_alphanumeric()) {
                out.insert(ext);
            }
        }
        rest = &after[end + 1..];
    }
    out
}

fn glob_prefix(pattern: &str) -> String {
    if is_project_wide_pattern(pattern) {
        return String::new();
    }
    if let Some((idx, _)) = pattern
        .char_indices()
        .find(|(_, ch)| matches!(ch, '*' | '?' | '{' | '['))
    {
        return pattern[..idx].trim_end_matches('/').to_owned();
    }
    pattern
        .rsplit_once('/')
        .map(|(prefix, _)| prefix.to_owned())
        .unwrap_or_default()
}

fn prefix_subsumes(left: &str, right: &str) -> bool {
    if left.is_empty() || right.is_empty() {
        return true;
    }
    left == right
        || left.starts_with(&format!("{right}/"))
        || right.starts_with(&format!("{left}/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(title: &str, content: &str, patterns: &[&str]) -> SemanticRuleKey {
        SemanticRuleKey::new(
            title,
            content,
            &patterns
                .iter()
                .map(|pattern| (*pattern).to_owned())
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn semantic_match_collapses_reworded_same_scope_rules() {
        let first = key(
            "Review: Avoid leaking secrets into logs",
            "Rule:\nWhen touching `src/**/*.ts`, avoid logging Authorization headers or API tokens.",
            &["src/**/*.ts"],
        );
        let second = key(
            "Review: Do not log API tokens",
            "Rule:\nWhen touching `src/**/*.ts`, never include API tokens or Authorization headers in logs.",
            &["src/**/*.ts"],
        );

        assert!(semantic_rules_match(&second, &first));
    }

    #[test]
    fn semantic_match_ignores_variable_numbers() {
        let first = key(
            "Review: Keep generated file warnings under 1360 lines",
            "Rule:\nWhen touching `tools/**/*.go`, keep generated file warnings under the configured line budget.",
            &["tools/**/*.go"],
        );
        let second = key(
            "Review: Keep generated file warnings under 1525 lines",
            "Rule:\nWhen touching `tools/**/*.go`, keep generated file warnings under the configured line budget.",
            &["tools/**/*.go"],
        );

        assert!(semantic_rules_match(&second, &first));
    }

    #[test]
    fn semantic_match_respects_language_scope() {
        let first = key(
            "Review: Avoid leaking secrets into logs",
            "Rule:\nWhen touching `src/**/*.ts`, avoid logging Authorization headers or API tokens.",
            &["src/**/*.ts"],
        );
        let second = key(
            "Review: Avoid leaking secrets into logs",
            "Rule:\nWhen touching `cmd/**/*.go`, avoid logging Authorization headers or API tokens.",
            &["cmd/**/*.go"],
        );

        assert!(!semantic_rules_match(&second, &first));
    }

    #[test]
    fn semantic_match_keeps_distinct_same_scope_rules() {
        let first = key(
            "Review: Prefer Arc over Rc in async tasks",
            "Rule:\nWhen touching `tokio/src/io/**/*.rs`, use Arc rather than Rc for values spawned onto a multi-threaded runtime.",
            &["tokio/src/io/**/*.rs"],
        );
        let second = key(
            "Review: Check loom Arc semantics under cfg",
            "Rule:\nWhen touching `tokio/src/io/**/*.rs`, use loom::sync::Arc under cfg(loom) so concurrency tests exercise modeled atomics.",
            &["tokio/src/io/**/*.rs"],
        );

        assert!(!semantic_rules_match(&second, &first));
    }
}
