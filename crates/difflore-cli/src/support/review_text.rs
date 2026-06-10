//! Shared cleanup for AI-reviewer prose (CodeRabbit & friends) so noisy
//! emphasis/banner markup never reaches user-visible surfaces.
//!
//! Two layers, applied in order:
//!  - `strip_review_markdown_noise`: drops emoji, markdown emphasis runs
//!    (`**`, `__`, `_`, `*`), and leading severity banners.
//!  - `clean_display_title`: also drops the `Review:` ingest prefix when
//!    nothing useful remains after the colon, and trims residual delimiters.
//!
//! Both are idempotent so they can run at both ingest time (`import_reviews`)
//! and display time (`search` / `recall`) without double-stripping good text.

const SEVERITY_BANNERS: &[&str] = &[
    // Longer phrases first so the loop consumes the maximal prefix
    // before falling through to a shorter variant.
    "actionable comments posted",
    "actionable comments",
    "nitpick comments",
    "nitpick comment",
    "duplicate comments",
    "duplicate comment",
    "potential issue",
    "minor",
    "major",
    "critical",
    "blocker",
    "suggestion",
];

/// Banners stripped ONLY at display time — kept visible during ingestion so
/// the high-signal gate can reject PR-overview bot summaries before rule
/// extraction.
const DISPLAY_ONLY_BANNERS: &[&str] = &[
    "## pull request overview",
    "## pull-request overview",
    "pull request overview",
    "pr overview",
    "## summary",
    "## changes",
    "## what changed",
];

const LEADING_DELIMITERS: &[char] = &[' ', '|', '·', ':', ',', '-', '–', '—', '/', '#', '>'];

/// Drop emoji, markdown emphasis markers, and leading severity banners.
/// Idempotent.
pub(crate) fn strip_review_markdown_noise(input: &str) -> String {
    // 1. Drop emoji and variation/zero-width selectors that don't help
    //    retrieval and look noisy in plain-text contexts.
    let no_emoji: String = input
        .chars()
        .filter(|ch| {
            let code = *ch as u32;
            !matches!(
                code,
                0x2600..=0x27BF | 0x1F300..=0x1FAFF | 0xFE00..=0xFE0F | 0x200D
            )
        })
        .collect();

    // 2. Strip emphasis markers. Multi-char markers first to avoid
    //    leaving asymmetric residue.
    let mut tmp = no_emoji;
    for marker in ["**", "__"] {
        tmp = tmp.replace(marker, " ");
    }
    let cleaned: String = tmp
        .chars()
        .map(|ch| if matches!(ch, '_' | '*') { ' ' } else { ch })
        .collect();

    // 3. Walk leading severity banners + delimiters until we hit content.
    let mut head = cleaned;
    loop {
        let trimmed = head.trim_start_matches(LEADING_DELIMITERS).to_owned();
        let lower = trimmed.to_ascii_lowercase();
        let mut matched = false;
        for banner in SEVERITY_BANNERS {
            if lower.starts_with(banner) {
                trimmed[banner.len()..].clone_into(&mut head);
                matched = true;
                break;
            }
        }
        if matched {
            continue;
        }
        return trimmed.trim().to_owned();
    }
}

/// Display-time cleanup for rule titles. Strips noise then re-frames the
/// `Review: ...` ingest prefix when stripping leaves it empty or trivial.
/// Returns the input verbatim if cleanup would produce a title shorter
/// than the fallback — we never want to make a title *worse* by cleanup.
pub(crate) fn clean_display_title(title: &str, fallback: &str) -> String {
    let trimmed = title.trim();
    if trimmed.is_empty() {
        return fallback.to_owned();
    }

    // If the title was minted by `import-reviews` it begins with "Review: ";
    // strip that prefix before cleaning so banners/emphasis right after the
    // colon are reachable, then re-add the prefix when there's content left.
    let (had_prefix, body) = match trimmed.strip_prefix("Review:") {
        Some(rest) => (true, rest.trim_start()),
        None => (false, trimmed),
    };

    let cleaned = strip_review_markdown_noise(body);
    let cleaned = strip_display_only_banners(&cleaned);
    let cleaned = strip_count_residue(&cleaned);
    if cleaned.trim().is_empty() {
        // Stripping consumed the whole title — fall back rather than
        // surface an empty cell.
        return fallback.to_owned();
    }

    if had_prefix {
        format!("Review: {cleaned}")
    } else {
        cleaned
    }
}

/// Strip leading count residue left behind by AI-reviewer headers
/// like `Actionable comments posted: 9** [` once the banner is gone.
/// Consumes a leading run of digits / asterisks / brackets / parens /
/// HTML `details`/`summary` tags so the rendered title starts at the
/// first real content character.
fn strip_count_residue(input: &str) -> String {
    let residue_chars: &[char] = &[
        ' ', ':', '*', '(', ')', '[', ']', '#', '>', '<', '/', '|', '·', ',', '-',
    ];
    let mut head = input.to_owned();
    let mut prev = head.len() + 1;
    while head.len() < prev {
        prev = head.len();
        head = head.trim_start_matches(residue_chars).to_owned();
        head = head
            .trim_start_matches(|c: char| c.is_ascii_digit())
            .to_owned();
        for tag in ["details>", "summary>", "/details>", "/summary>"] {
            if head.to_ascii_lowercase().starts_with(tag) {
                head = head[tag.len()..].to_owned();
            }
        }
    }
    head.trim().to_owned()
}

/// Strip leading PR-overview / Summary section markers (display-only;
/// ingest keeps them for the high-signal gate). Also consumes count residue
/// from headers like `Actionable comments posted: 9** [` or
/// `Nitpick comments (5)` so the title starts at the first real sentence.
fn strip_display_only_banners(input: &str) -> String {
    let mut head = input.to_owned();
    loop {
        let trimmed = head.trim_start_matches(LEADING_DELIMITERS).to_owned();
        let lower = trimmed.to_ascii_lowercase();
        let mut matched = false;
        for banner in DISPLAY_ONLY_BANNERS {
            if lower.starts_with(banner) {
                trimmed[banner.len()..].clone_into(&mut head);
                matched = true;
                break;
            }
        }
        if !matched {
            head = trimmed;
            break;
        }
    }
    // Consume residue from "<banner>: <count>**" / "<banner>(<count>)" /
    // "<banner> <details><summary>…" shapes that survive the prefix
    // strip. Keep going while we're seeing only delimiters, digits,
    // asterisks, parens, brackets, or a `details`/`summary` HTML tag.
    let residue_chars: &[char] = &[
        ' ', ':', '*', '(', ')', '[', ']', '#', '>', '<', '/', '|', '·', ',', '-',
    ];
    let mut prev = head.len() + 1;
    while head.len() < prev {
        prev = head.len();
        head = head.trim_start_matches(residue_chars).to_owned();
        head = head
            .trim_start_matches(|c: char| c.is_ascii_digit())
            .to_owned();
        // HTML <details>/<summary> tag remnants from CodeRabbit blocks.
        for tag in ["details>", "summary>", "/details>", "/summary>"] {
            let lower = head.to_ascii_lowercase();
            if lower.starts_with(tag) {
                head = head[tag.len()..].to_owned();
            }
        }
    }
    head.trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_drops_severity_banners_and_emphasis() {
        let raw = "_⚠️ Potential issue_ | _🟡 Minor_ Wait for the async submit \
                   path before asserting state.";
        let out = strip_review_markdown_noise(raw);
        assert!(!out.contains('_'), "underscores remain: {out}");
        assert!(!out.contains('⚠'), "emoji remain: {out}");
        assert!(
            !out.to_ascii_lowercase().contains("potential issue"),
            "banner: {out}"
        );
        assert!(out.starts_with("Wait for the async submit"));
    }

    #[test]
    fn strip_keeps_real_prose_and_inline_code() {
        let raw = "**Use** `errors.Is` rather than `==` when comparing wrapped errors.";
        let out = strip_review_markdown_noise(raw);
        assert!(out.contains("Use"));
        assert!(out.contains("errors.Is"));
        assert!(!out.contains('*'));
    }

    #[test]
    fn strip_is_idempotent() {
        let raw = "_⚠️ Potential issue_ Use immutable view";
        let once = strip_review_markdown_noise(raw);
        let twice = strip_review_markdown_noise(&once);
        assert_eq!(once, twice);
    }

    #[test]
    fn clean_display_title_reframes_review_prefix() {
        let title = "Review: _⚠️ Potential issue_ | _🟡 Minor_ Wait for the async submit path";
        let cleaned = clean_display_title(title, "rule-id");
        assert_eq!(cleaned, "Review: Wait for the async submit path");
    }

    #[test]
    fn clean_display_title_passes_through_clean_titles() {
        let title = "Use errors.Is for wrapped error comparison";
        let cleaned = clean_display_title(title, "rule-id");
        assert_eq!(cleaned, title);
    }

    #[test]
    fn clean_display_title_falls_back_when_cleanup_empties_body() {
        let title = "Review: _⚠️_";
        let cleaned = clean_display_title(title, "rule-7");
        assert_eq!(cleaned, "rule-7");
    }

    #[test]
    fn clean_display_title_handles_empty_input() {
        assert_eq!(clean_display_title("", "fallback"), "fallback");
        assert_eq!(clean_display_title("   ", "fallback"), "fallback");
    }

    #[test]
    fn clean_display_title_drops_pr_overview_boilerplate() {
        // PR-overview banner is intentionally NOT stripped by
        // strip_review_markdown_noise (ingest time keeps the marker so
        // the high-signal gate can reject these bot summaries). Display
        // layer owns this scrub via clean_display_title.
        let raw =
            "## Pull request overview Adds a new bind-everything capability to Gin's binding flow.";
        let out = clean_display_title(raw, "rule-id");
        assert!(
            !out.to_ascii_lowercase().contains("pull request overview"),
            "display-time banner not stripped: {out}"
        );
        assert!(out.starts_with("Adds a new bind-everything"), "got: {out}");
    }

    #[test]
    fn clean_display_title_strips_pr_overview_review_prefix() {
        let raw = "Review: ## Pull request overview This PR refactors the buffer allocation logic.";
        let out = clean_display_title(raw, "rule-id");
        assert_eq!(
            out, "Review: This PR refactors the buffer allocation logic.",
            "got: {out}"
        );
    }
}
