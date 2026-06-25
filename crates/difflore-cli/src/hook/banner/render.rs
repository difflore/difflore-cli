//! Banner text formatter.
//!
//! Produces a ≤6-line / ≤400-char string the adapters append to their
//! `additional_context`. Shape:
//!
//! ```text
//! DiffLore: 2 new rules learned for this repo since 2026-05-20T14:30:00Z
//!   - Return 413 for body size limit errors <- from a PR review
//!   - Wrap context cancellation in errgroup <- from cross-repo pattern mining
//! Run `difflore status` to inspect local memory.
//! ```
//!
//! Each bullet is ~80 chars (60-char title cap + provenance suffix).

use super::query::{MemoryPulseSummary, NewRule};

/// Max chars in a rendered rule title before truncating with `…`. Titles
/// longer than this are usually wrapped sentences that read poorly as a
/// bullet.
const TITLE_TRUNCATE_AT: usize = 60;

/// Hard ceiling on the whole banner, enforced after assembly so a single
/// oversized title can't overflow the budget.
const BANNER_MAX_CHARS: usize = 400;

/// Build the banner from rule rows + the previous-session `prev_label`
/// (an RFC-3339 timestamp or the synthetic "the start of this repo";
/// inlined as-is).
///
/// No trailing newline: adapters append directly to other context blocks
/// and add their own separators.
pub fn format_banner(rules: &[NewRule], prev_label: &str) -> String {
    format_banner_with_capture_paused(rules, prev_label, None, false)
}

pub fn format_banner_with_capture_paused(
    rules: &[NewRule],
    prev_label: &str,
    capture_paused_reason: Option<&str>,
    windows_forwarder_cold: bool,
) -> String {
    let count = rules.len();
    let rule_word = if count == 1 { "rule" } else { "rules" };
    let mut out =
        format!("DiffLore: {count} new {rule_word} learned for this repo since {prev_label}");

    for rule in rules {
        let title = truncate_title(&rule.title);
        let provenance = provenance_phrase(&rule.origin);
        out.push('\n');
        out.push_str("  - ");
        out.push_str(&title);
        out.push_str(" <- ");
        out.push_str(provenance);
    }

    out.push_str("\nRun `difflore status` to inspect local memory.");
    append_capture_paused_line(&mut out, capture_paused_reason);
    append_windows_forwarder_cold_line(&mut out, windows_forwarder_cold);

    enforce_banner_cap(out)
}

pub fn format_banner_with_memory_pulse(
    rules: &[NewRule],
    pulse: &MemoryPulseSummary,
    prev_label: &str,
    capture_paused_reason: Option<&str>,
    windows_forwarder_cold: bool,
) -> String {
    if pulse.is_empty() {
        return format_banner_with_capture_paused(
            rules,
            prev_label,
            capture_paused_reason,
            windows_forwarder_cold,
        );
    }

    let mut parts = vec![format!("+{} ready", rules.len())];
    if pulse.folded_away > 0 {
        parts.push(format!("{} folded away", pulse.folded_away));
    }
    if pulse.to_confirm > 0 {
        parts.push(format!("{} to confirm", pulse.to_confirm));
    }
    let mut out = format!("DiffLore memory: {} since {prev_label}", parts.join(" · "));

    if !rules.is_empty() {
        out.push('\n');
        out.push_str("Ready: ");
        out.push_str(&ready_titles_line(rules));
    }

    out.push_str("\nRun `difflore status` to inspect local memory.");
    append_capture_paused_line(&mut out, capture_paused_reason);
    append_windows_forwarder_cold_line(&mut out, windows_forwarder_cold);

    enforce_banner_cap(out)
}

pub fn format_capture_paused_banner(reason: &str) -> String {
    let mut out = "DiffLore: capture paused for session learning; recall still works.".to_owned();
    append_capture_paused_line(&mut out, Some(reason));
    enforce_banner_cap(out)
}

pub fn format_windows_forwarder_cold_banner() -> String {
    enforce_banner_cap(
        "DiffLore: Windows hook warm path was cold; using local fallback. MCP/self-warm should speed later hooks. Run `difflore doctor --report`.".to_owned(),
    )
}

fn append_capture_paused_line(out: &mut String, reason: Option<&str>) {
    let Some(reason) = reason.map(str::trim).filter(|s| !s.is_empty()) else {
        return;
    };
    out.push_str("\nCapture paused: ");
    out.push_str(&truncate_capture_reason(reason));
    out.push_str(". Run `difflore doctor`.");
}

fn append_windows_forwarder_cold_line(out: &mut String, enabled: bool) {
    if !enabled {
        return;
    }
    out.push_str("\nWindows hook warm path was cold; MCP/self-warm should speed later hooks.");
}

fn enforce_banner_cap(mut out: String) -> String {
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

fn truncate_capture_reason(reason: &str) -> String {
    const MAX: usize = 96;
    let reason = reason.lines().next().unwrap_or("").trim();
    if reason.chars().count() <= MAX {
        return reason.to_owned();
    }
    let mut out: String = reason.chars().take(MAX.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn ready_titles_line(rules: &[NewRule]) -> String {
    let mut titles = rules
        .iter()
        .take(2)
        .map(|rule| truncate_title(&rule.title))
        .collect::<Vec<_>>();
    if rules.len() > titles.len() {
        titles.push(format!("+{} more", rules.len() - titles.len()));
    }
    titles.join("; ")
}

/// Truncate `s` to `TITLE_TRUNCATE_AT` chars (counted by chars, not bytes,
/// so multibyte titles survive). Appends `…` when truncation fired.
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

/// Map a `skills.origin` value to the bullet's "<- from ..." phrase.
/// Unknown origins fall through to a generic phrasing so a new cloud-side
/// origin value doesn't break the banner.
fn provenance_phrase(origin: &str) -> &'static str {
    match origin {
        "pr_review" => "from a PR review",
        "autopilot" => "auto-enabled by DiffLore",
        "conversation" => "from agent chat (`remember_rule`)",
        "extracted" => "from cross-repo pattern mining",
        "manual" => "added manually",
        _ => "newly learned",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hook::banner::query::NewRule;

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
    fn memory_pulse_renders_result_narrative_when_folds_accompany_ready_rules() {
        let pulse = MemoryPulseSummary {
            folded_away: 4,
            to_confirm: 1,
        };
        let out = format_banner_with_memory_pulse(
            &[rule("Use npm run tauri dev", "autopilot")],
            &pulse,
            "2026-06-25T10:00:00Z",
            None,
            false,
        );

        assert!(out.contains("DiffLore memory: +1 ready · 4 folded away · 1 to confirm"));
        assert!(out.contains("Ready: Use npm run tauri dev"));
        assert!(out.lines().count() <= 3, "got: {out}");
    }

    #[test]
    fn memory_pulse_summary_does_not_render_for_folds_only() {
        let pulse = MemoryPulseSummary {
            folded_away: 4,
            to_confirm: 0,
        };

        assert!(!pulse.should_render(0));
        assert!(pulse.should_render(1));
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
        let bullet_line = out.lines().find(|l| l.starts_with("  - ")).expect("bullet");
        let title_part = bullet_line
            .trim_start_matches("  - ")
            .split(" <- ")
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
    fn capture_paused_banner_mentions_recall_and_doctor() {
        let out = format_capture_paused_banner("codex unauthorized for gate capture");

        assert!(out.contains("capture paused"), "got: {out}");
        assert!(out.contains("recall still works"), "got: {out}");
        assert!(out.contains("codex unauthorized"), "got: {out}");
        assert!(out.contains("difflore doctor"), "got: {out}");
        assert!(out.len() <= BANNER_MAX_CHARS);
    }

    #[test]
    fn learned_rules_banner_can_include_capture_paused_line() {
        let out = format_banner_with_capture_paused(
            &[rule("Return 413", "pr_review")],
            "2026-05-20T14:30:00Z",
            Some("claude-code forbidden"),
            false,
        );

        assert!(out.contains("1 new rule learned"), "got: {out}");
        assert!(out.contains("Capture paused:"), "got: {out}");
        assert!(out.contains("claude-code forbidden"), "got: {out}");
        assert!(out.len() <= BANNER_MAX_CHARS);
    }

    #[test]
    fn windows_forwarder_cold_banner_points_to_doctor_report() {
        let out = format_windows_forwarder_cold_banner();

        assert!(out.contains("warm path was cold"), "got: {out}");
        assert!(out.contains("local fallback"), "got: {out}");
        assert!(out.contains("difflore doctor --report"), "got: {out}");
        assert!(out.len() <= BANNER_MAX_CHARS);
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
