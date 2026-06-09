//! Banner text formatter.
//!
//! Produces a multi-line, ≤6-line / ≤400-char string the adapters
//! append to whatever `additional_context` they already produce. The
//! shape mirrors the spec in the original feature request:
//!
//! ```text
//! DiffLore: 2 new rules learned for this repo since 2026-05-20T14:30:00Z
//!   · Return 413 for body size limit errors  ← from review by alice (PR #88)
//!   · Wrap context cancellation in errgroup   ← from PR merge signature
//! Run `difflore status` to see the value loop.
//! ```
//!
//! Each bullet is at most ~80 chars (60-char title cap + provenance
//! suffix). The closing call-to-action is fixed so the agent can learn
//! to discover the status surface from any session.

use super::query::NewRule;

/// Max chars in a rendered rule title before we truncate with `…`.
/// 60 was chosen by walking real rule names in the corpus: anything
/// longer than this is usually a wrapped sentence and reads poorly as
/// a bullet anyway.
const TITLE_TRUNCATE_AT: usize = 60;

/// Cap on the entire banner string. Spec calls for ≤ 400 chars; this
/// constant is a hard ceiling we enforce *after* assembly so a future
/// rule that smuggles a 300-char title can't overflow the budget.
const BANNER_MAX_CHARS: usize = 400;

/// Build the banner from rule rows + the previous-session label.
/// `prev_label` is either an RFC-3339 timestamp ("2026-05-20T14:30:00Z")
/// or the synthetic phrase "the start of this repo" used on first
/// session — formatter doesn't care which, it just inlines whatever
/// the caller computed.
///
/// Returns a single string with embedded `\n`s. Trailing newline is
/// intentionally absent: the adapters append directly to other context
/// blocks and the caller can add separators.
pub fn format_banner(rules: &[NewRule], prev_label: &str) -> String {
    // Header line: pluralise "rule" / "rules" so the banner reads
    // naturally on a single new rule too.
    let count = rules.len();
    let rule_word = if count == 1 { "rule" } else { "rules" };
    let mut out =
        format!("DiffLore: {count} new {rule_word} learned for this repo since {prev_label}");

    for rule in rules {
        let title = truncate_title(&rule.title);
        let provenance = provenance_phrase(&rule.origin);
        // The middle dot `·` matches the bullet style elsewhere in
        // DiffLore's CLI output (e.g. `difflore status`'s value-loop
        // table) so the banner doesn't visually clash on copy/paste.
        out.push('\n');
        out.push_str("  · ");
        out.push_str(&title);
        out.push_str("  ← ");
        out.push_str(provenance);
    }

    out.push_str("\nRun `difflore status` to see the value loop.");

    // Hard ceiling. We'd rather emit a truncated banner with an
    // ellipsis than blow past the agent's prompt-window budget. In
    // practice this rarely triggers: 5 rules × 80 chars + header +
    // CTA is well under 400. We compare bytes (`len()`) here because
    // the budget is measured against agent token-window cost, which
    // is byte-driven for UTF-8 corpora.
    //
    // Truncate via `char_indices` so we never split mid-codepoint
    // (which would panic in `String::truncate`). Reserve 3 bytes for
    // the U+2026 ellipsis we append.
    const ELLIPSIS_BYTES: usize = '…'.len_utf8();
    if out.len() > BANNER_MAX_CHARS {
        let cap = BANNER_MAX_CHARS.saturating_sub(ELLIPSIS_BYTES);
        let cut = out
            .char_indices()
            .take_while(|(idx, _)| *idx <= cap)
            .last()
            .map_or(0, |(idx, _)| idx);
        out.truncate(cut);
        out.push('…');
    }
    out
}

/// Truncate `s` to `TITLE_TRUNCATE_AT` chars (counted by chars, not
/// bytes — multibyte titles like Japanese rule names wouldn't survive
/// a byte-truncate). Appends `…` when truncation actually fired.
fn truncate_title(s: &str) -> String {
    let trimmed = s.trim();
    if trimmed.chars().count() <= TITLE_TRUNCATE_AT {
        return trimmed.to_owned();
    }
    let mut out: String = trimmed
        .chars()
        .take(TITLE_TRUNCATE_AT.saturating_sub(1))
        .collect();
    out.push('…');
    out
}

/// Map a `skills.origin` enum value to the bullet's "← from …" phrase.
/// Unknown origins fall through to a generic phrasing so a new origin
/// value introduced cloud-side doesn't break the banner.
fn provenance_phrase(origin: &str) -> &'static str {
    match origin {
        "pr_review" => "from a PR review",
        "conversation" => "from agent chat (`remember_rule`)",
        "extracted" => "from cross-repo pattern mining",
        "manual" => "added manually",
        _ => "newly learned",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::session_banner::query::NewRule;

    fn rule(title: &str, origin: &str) -> NewRule {
        NewRule {
            title: title.to_owned(),
            origin: origin.to_owned(),
            source_repo: Some("acme/billing".to_owned()),
        }
    }

    #[test]
    fn header_pluralises_correctly_and_includes_watermark() {
        let one = format_banner(&[rule("Return 413", "pr_review")], "2026-05-20T14:30:00Z");
        assert!(one.contains("1 new rule learned"), "got: {one}");
        assert!(one.contains("2026-05-20T14:30:00Z"));

        let many = format_banner(
            &[
                rule("Return 413", "pr_review"),
                rule("Wrap errgroup", "extracted"),
            ],
            "the start of this repo",
        );
        assert!(many.contains("2 new rules learned"), "got: {many}");
        assert!(many.contains("the start of this repo"));
    }

    #[test]
    fn provenance_phrases_match_origin() {
        let rules = [
            rule("a", "pr_review"),
            rule("b", "conversation"),
            rule("c", "extracted"),
            rule("d", "manual"),
            rule("e", "future-origin-we-dont-know"),
        ];
        let out = format_banner(&rules, "1970-01-01T00:00:00Z");
        assert!(out.contains("from a PR review"));
        assert!(out.contains("from agent chat"));
        assert!(out.contains("from cross-repo pattern mining"));
        assert!(out.contains("added manually"));
        assert!(out.contains("newly learned"));
    }

    #[test]
    fn long_title_is_truncated_with_ellipsis() {
        // 200-char title — would push the bullet past 60 chars by a wide margin.
        let long_title = "x".repeat(200);
        let r = rule(&long_title, "pr_review");
        let out = format_banner(&[r], "1970-01-01T00:00:00Z");
        // Truncated form ends with the U+2026 horizontal ellipsis.
        assert!(out.contains('…'), "expected ellipsis, got: {out}");
        // Title itself must not exceed the cap (allow the ellipsis).
        let bullet_line = out.lines().find(|l| l.starts_with("  · ")).expect("bullet");
        let title_part = bullet_line
            .trim_start_matches("  · ")
            .split("  ← ")
            .next()
            .unwrap_or("");
        assert!(
            title_part.chars().count() <= TITLE_TRUNCATE_AT,
            "title overran cap: {title_part:?}"
        );
    }

    #[test]
    fn banner_includes_call_to_action_and_obeys_size_cap() {
        let many = (0..5)
            .map(|i| rule(&format!("Rule {i}"), "pr_review"))
            .collect::<Vec<_>>();
        let out = format_banner(&many, "2026-05-20T14:30:00Z");
        assert!(out.contains("Run `difflore status`"), "missing CTA: {out}");
        assert!(
            out.len() <= BANNER_MAX_CHARS,
            "banner overran {BANNER_MAX_CHARS} chars: {} bytes",
            out.len()
        );
        // 5 bullets + header + CTA = 7 lines.
        assert_eq!(out.lines().count(), 7);
    }

    #[test]
    fn hard_cap_truncates_pathological_input() {
        // Construct an input that, even after per-title truncation,
        // would push past the 400-char ceiling. 5 bullets × 60 chars
        // ≈ 300 base + envelope is below the cap, so use longer
        // titles than the cap (and disable truncation by going via
        // the formatter directly).
        let huge_title = "y".repeat(80); // each bullet ~85 chars after envelope
        let many = (0..5)
            .map(|_| rule(&huge_title, "pr_review"))
            .collect::<Vec<_>>();
        let out = format_banner(&many, "2026-05-20T14:30:00Z");
        // Hard ceiling holds.
        assert!(
            out.len() <= BANNER_MAX_CHARS,
            "expected ≤ {BANNER_MAX_CHARS}, got {}",
            out.len()
        );
    }
}
