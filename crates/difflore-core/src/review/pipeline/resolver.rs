//! Hunk-aware line resolution for review issues.
//!
//! The review LLM returns an `existing_code` snippet and/or a claimed `line`,
//! but the claimed line is unreliable (models routinely emit diff-relative or
//! off-by-N numbers, or count from the hunk header). This module snaps the
//! issue to the exact new-file line range by matching against the parsed diff
//! hunks.
//!
//! Everything here is pure: it takes a unified-diff string plus a
//! [`ResolveTarget`] and returns the resolved `(start, end)` (1-based,
//! new-file line numbers), or `None` when no confident match exists (the
//! caller then keeps whatever the model claimed). It only ever improves a line
//! number, never regresses: when it can't match it returns `None`.

/// One physical line inside a hunk, tagged by which side(s) of the diff it
/// belongs to, paired with its absolute 1-based line number on that side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineKind {
    /// Present on both sides (a context line).
    Context,
    /// Only on the new side (an added line).
    Added,
    /// Only on the old side (a deleted line).
    Deleted,
}

/// A single hunk parsed from a unified diff, retaining absolute line
/// numbers so a matched snippet can be mapped back to new-file lines.
#[derive(Debug, Clone)]
pub struct DiffHunk {
    old_start: u32,
    new_start: u32,
    lines: Vec<HunkLine>,
}

#[derive(Debug, Clone)]
struct HunkLine {
    kind: LineKind,
    /// Content with the leading diff marker (`+`/`-`/` `) already stripped.
    content: String,
}

/// `(absolute_line_number, normalized_content)` for one side of a hunk.
struct IndexedLine {
    line_num: u32,
    content: String,
}

/// The minimal view of a review issue the resolver needs. Kept separate from
/// `ReviewIssueRecord` so the resolver stays a pure, easily-tested function.
#[derive(Debug, Clone, Default)]
pub struct ResolveTarget {
    /// The verbatim source snippet the model flagged, if any. The primary,
    /// highest-precision signal.
    pub snippet: Option<String>,
    /// The line number the model claimed. Used as a secondary signal to
    /// pick the enclosing hunk and snap to a real changed line.
    pub claimed_line: Option<i32>,
}

/// Parse the hunks for a *single file's* unified diff section.
///
/// Accepts either a full `diff --git` section or just the `@@ … @@` hunk
/// run; any leading file headers (`diff --git`, `index`, `---`, `+++`) are
/// skipped. Hunk headers that don't parse are ignored (their bodies are
/// dropped) rather than aborting the whole parse.
pub fn parse_hunks(diff_section: &str) -> Vec<DiffHunk> {
    let mut hunks: Vec<DiffHunk> = Vec::new();
    let mut current: Option<DiffHunk> = None;

    for raw in diff_section.lines() {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if line.starts_with("@@") {
            if let Some(h) = current.take() {
                hunks.push(h);
            }
            current = parse_hunk_header(line);
            continue;
        }
        let Some(h) = current.as_mut() else {
            // Lines before the first hunk header (file headers) are skipped.
            continue;
        };
        // "\ No newline at end of file" markers carry no content line.
        if line.starts_with('\\') {
            continue;
        }
        match line.as_bytes().first() {
            Some(b'+') => h.lines.push(HunkLine {
                kind: LineKind::Added,
                content: line[1..].to_owned(),
            }),
            Some(b'-') => h.lines.push(HunkLine {
                kind: LineKind::Deleted,
                content: line[1..].to_owned(),
            }),
            Some(b' ') => h.lines.push(HunkLine {
                kind: LineKind::Context,
                content: line[1..].to_owned(),
            }),
            // A bare empty line inside a hunk is a context line with empty
            // content (git emits these for blank context lines).
            None => h.lines.push(HunkLine {
                kind: LineKind::Context,
                content: String::new(),
            }),
            // Anything else (e.g. a stray header) ends the current hunk's
            // body but we keep scanning for the next `@@`.
            Some(_) => {}
        }
    }
    if let Some(h) = current.take() {
        hunks.push(h);
    }
    hunks
}

/// Parse `@@ -<oldStart>[,<oldLen>] +<newStart>[,<newLen>] @@ …` into an
/// empty [`DiffHunk`] seeded with the two start line numbers.
fn parse_hunk_header(header: &str) -> Option<DiffHunk> {
    // Body between the first "@@" and the second "@@".
    let inner = header.strip_prefix("@@")?;
    let end = inner.find("@@")?;
    let spec = inner[..end].trim();
    let mut parts = spec.split_whitespace();
    let old = parts.next()?.strip_prefix('-')?;
    let new = parts.next()?.strip_prefix('+')?;
    let old_start = old.split(',').next()?.parse::<u32>().ok()?;
    let new_start = new.split(',').next()?.parse::<u32>().ok()?;
    Some(DiffHunk {
        old_start,
        new_start,
        lines: Vec::new(),
    })
}

/// Resolve an issue to `(start, end)` 1-based new-file line numbers.
///
/// Strategy, in priority order:
/// 1. **Snippet match, new side** — match the normalized `snippet` against
///    a consecutive run of context+added lines → new-file line numbers.
/// 2. **Snippet match, old side** — fall back to context+deleted lines →
///    old-file line numbers (best available when the issue is about removed
///    code).
/// 3. **Claimed-line snap** — when there's no usable snippet but the model
///    gave a `claimed_line`, find the hunk whose new-side range contains
///    that line and snap to the nearest context/added line.
///
/// Returns `None` when nothing matches; callers then keep the model's
/// claimed line untouched (never a regression).
pub fn resolve_issue_lines(target: &ResolveTarget, hunks: &[DiffHunk]) -> Option<(i32, i32)> {
    if hunks.is_empty() {
        return None;
    }

    // 1 + 2: snippet matching (primary, highest precision).
    if let Some(snippet) = target.snippet.as_deref() {
        let targets = split_and_normalize(snippet);
        if !targets.is_empty() {
            // A non-unique snippet (e.g. a bare `}` or a repeated `log(x);`)
            // can match several positions. When the model also gave a plausible
            // line, use it to break the tie toward the nearest occurrence
            // instead of blindly taking the first — otherwise ON could move a
            // correctly-claimed line onto an earlier duplicate. With no claim,
            // first-match-wins is preserved exactly.
            let prefer = target
                .claimed_line
                .filter(|n| *n > 0)
                .and_then(|n| u32::try_from(n).ok());
            // New side first (added/context), then old side (deletions).
            for new_side in [true, false] {
                let mut best: Option<(u32, u32)> = None;
                for hunk in hunks {
                    let side = extract_side_lines(hunk, new_side);
                    let Some(cand) = match_consecutive(&side, &targets, prefer) else {
                        continue;
                    };
                    match prefer {
                        None => return Some((to_i32(cand.0), to_i32(cand.1))),
                        Some(claimed) => {
                            best = Some(best.map_or(cand, |prev| {
                                if cand.0.abs_diff(claimed) < prev.0.abs_diff(claimed) {
                                    cand
                                } else {
                                    prev
                                }
                            }));
                        }
                    }
                }
                if let Some((s, e)) = best {
                    return Some((to_i32(s), to_i32(e)));
                }
            }
        }
    }

    // 3: claimed-line snap (secondary).
    if let Some(claimed) = target.claimed_line.filter(|n| *n > 0) {
        if let Some(line) = snap_claimed_line(claimed, hunks) {
            return Some((line, line));
        }
    }

    None
}

/// Find the changed line nearest to `claimed` within the hunk whose new-side
/// span contains it. "Changed" means context or added (lines that exist in
/// the new file). When the claimed line falls inside a hunk but lands on a
/// position with no new-side line (i.e. only deletions there), snap to the
/// closest new-side line in that hunk.
fn snap_claimed_line(claimed: i32, hunks: &[DiffHunk]) -> Option<i32> {
    let claimed = u32::try_from(claimed).ok()?;
    // Collect every new-side line number across all hunks, then pick the
    // hunk that contains `claimed` and the closest new-side line in it.
    let mut best: Option<(u32, u32)> = None; // (distance, line)
    for hunk in hunks {
        let side = extract_side_lines(hunk, true);
        if side.is_empty() {
            continue;
        }
        let lo = side.first().map_or(0, |l| l.line_num);
        let hi = side.last().map_or(0, |l| l.line_num);
        // Only snap within a hunk that plausibly contains the claimed line
        // (allowing the model to be off by a little around the boundaries).
        if claimed + 2 < lo || claimed > hi + 2 {
            continue;
        }
        for l in &side {
            let dist = l.line_num.abs_diff(claimed);
            if best.is_none_or(|(bd, _)| dist < bd) {
                best = Some((dist, l.line_num));
            }
        }
    }
    best.map(|(_, line)| to_i32(line))
}

/// Extract one side of a hunk as `(absolute_line, normalized_content)`.
/// `new_side == true` → context + added (new-file numbers); else context +
/// deleted (old-file numbers).
fn extract_side_lines(hunk: &DiffHunk, new_side: bool) -> Vec<IndexedLine> {
    let mut out = Vec::new();
    let mut old_line = hunk.old_start;
    let mut new_line = hunk.new_start;
    for l in &hunk.lines {
        match l.kind {
            LineKind::Context => {
                let n = if new_side { new_line } else { old_line };
                out.push(IndexedLine {
                    line_num: n,
                    content: normalize_line(&l.content),
                });
                old_line += 1;
                new_line += 1;
            }
            LineKind::Added => {
                if new_side {
                    out.push(IndexedLine {
                        line_num: new_line,
                        content: normalize_line(&l.content),
                    });
                }
                new_line += 1;
            }
            LineKind::Deleted => {
                if !new_side {
                    out.push(IndexedLine {
                        line_num: old_line,
                        content: normalize_line(&l.content),
                    });
                }
                old_line += 1;
            }
        }
    }
    out
}

/// Scan `side` for a consecutive run matching every line in `targets`.
/// Returns the absolute `(start, end)` line numbers of the match.
fn match_consecutive(
    side: &[IndexedLine],
    targets: &[String],
    prefer_near: Option<u32>,
) -> Option<(u32, u32)> {
    if targets.is_empty() || side.len() < targets.len() {
        return None;
    }
    let last = side.len() - targets.len();
    let mut best: Option<(u32, u32)> = None;
    for i in 0..=last {
        if side[i..]
            .iter()
            .zip(targets.iter())
            .all(|(s, t)| &s.content == t)
        {
            let cand = (side[i].line_num, side[i + targets.len() - 1].line_num);
            match prefer_near {
                // No tie-breaker → first match wins (original behavior).
                None => return Some(cand),
                // Prefer the occurrence whose start is nearest the claimed line.
                Some(claimed) => {
                    best = Some(best.map_or(cand, |prev| {
                        if cand.0.abs_diff(claimed) < prev.0.abs_diff(claimed) {
                            cand
                        } else {
                            prev
                        }
                    }));
                }
            }
        }
    }
    best
}

/// Split code text into lines, normalizing each and dropping blanks.
fn split_and_normalize(code: &str) -> Vec<String> {
    code.lines()
        .map(normalize_line)
        .filter(|s| !s.is_empty())
        .collect()
}

/// Trim whitespace and strip a single leading diff marker (`+`/`-`), then
/// trim again.
fn normalize_line(s: &str) -> String {
    let t = s.trim();
    let t = t
        .strip_prefix('+')
        .or_else(|| t.strip_prefix('-'))
        .unwrap_or(t);
    t.trim().to_owned()
}

fn to_i32(n: u32) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
@@ -10,6 +10,7 @@ fn handler() {
     let cfg = load();
     let db = connect();
-    let token = req.token;
+    let token = req.token.clone();
+    validate(&token)?;
     run(token);
 }
";

    // A diff where the same single line (`log(x);`) is added in two places —
    // new-side lines 2 and 5 — so a bare-line snippet is ambiguous.
    const DUP_SNIPPET: &str = "\
@@ -1,3 +1,5 @@
 fn a() {
+    log(x);
 }
 fn b() {
+    log(x);
 }
";

    fn parsed() -> Vec<DiffHunk> {
        parse_hunks(SAMPLE)
    }

    #[test]
    fn parses_header_start_lines() {
        let h = parsed();
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].old_start, 10);
        assert_eq!(h[0].new_start, 10);
    }

    #[test]
    fn matches_added_line_to_new_file_number() {
        // new-side numbering: 10 ` let cfg`, 11 ` let db`, 12 `+ token.clone`,
        // 13 `+ validate`, 14 ` run`, 15 ` }`
        let target = ResolveTarget {
            snippet: Some("validate(&token)?;".to_owned()),
            claimed_line: None,
        };
        assert_eq!(resolve_issue_lines(&target, &parsed()), Some((13, 13)));
    }

    #[test]
    fn matches_context_line_after_additions() {
        let target = ResolveTarget {
            snippet: Some("run(token);".to_owned()),
            claimed_line: None,
        };
        assert_eq!(resolve_issue_lines(&target, &parsed()), Some((14, 14)));
    }

    #[test]
    fn matches_multi_line_consecutive_run() {
        let target = ResolveTarget {
            snippet: Some("let token = req.token.clone();\nvalidate(&token)?;".to_owned()),
            claimed_line: None,
        };
        // 12..=13 on the new side.
        assert_eq!(resolve_issue_lines(&target, &parsed()), Some((12, 13)));
    }

    #[test]
    fn falls_back_to_old_side_for_deleted_code() {
        // The deleted line only exists on the old side (old line 12).
        let target = ResolveTarget {
            snippet: Some("let token = req.token;".to_owned()),
            claimed_line: None,
        };
        assert_eq!(resolve_issue_lines(&target, &parsed()), Some((12, 12)));
    }

    #[test]
    fn normalizes_diff_markers_and_whitespace_in_snippet() {
        // Snippet copied straight from the diff, markers + indentation intact.
        let target = ResolveTarget {
            snippet: Some("+    validate(&token)?;".to_owned()),
            claimed_line: None,
        };
        assert_eq!(resolve_issue_lines(&target, &parsed()), Some((13, 13)));
    }

    #[test]
    fn no_match_returns_none() {
        let target = ResolveTarget {
            snippet: Some("this text is not in the diff at all".to_owned()),
            claimed_line: None,
        };
        assert_eq!(resolve_issue_lines(&target, &parsed()), None);
    }

    #[test]
    fn empty_hunks_returns_none() {
        let target = ResolveTarget {
            snippet: Some("anything".to_owned()),
            claimed_line: Some(5),
        };
        assert_eq!(resolve_issue_lines(&target, &[]), None);
    }

    #[test]
    fn snaps_claimed_line_when_no_snippet() {
        // Model claims line 13 (which is the `validate` added line) but gives
        // no snippet — snap should land exactly on a real new-side line.
        let target = ResolveTarget {
            snippet: None,
            claimed_line: Some(13),
        };
        assert_eq!(resolve_issue_lines(&target, &parsed()), Some((13, 13)));
    }

    #[test]
    fn snaps_slightly_off_claimed_line_to_nearest_changed_line() {
        // Model claims 16 (one past the hunk's last new-side line, 15) — the
        // boundary tolerance snaps it back to 15.
        let target = ResolveTarget {
            snippet: None,
            claimed_line: Some(16),
        };
        assert_eq!(resolve_issue_lines(&target, &parsed()), Some((15, 15)));
    }

    #[test]
    fn claimed_line_far_outside_any_hunk_returns_none() {
        let target = ResolveTarget {
            snippet: None,
            claimed_line: Some(900),
        };
        assert_eq!(resolve_issue_lines(&target, &parsed()), None);
    }

    #[test]
    fn snippet_wins_over_claimed_line() {
        // Snippet points at line 14, claimed_line lies at 12 — snippet wins.
        let target = ResolveTarget {
            snippet: Some("run(token);".to_owned()),
            claimed_line: Some(12),
        };
        assert_eq!(resolve_issue_lines(&target, &parsed()), Some((14, 14)));
    }

    #[test]
    fn ambiguous_snippet_disambiguates_to_claimed_line() {
        // "log(x);" matches new-side lines 2 and 5. A correct claimed line must
        // break the tie instead of blindly snapping to the first occurrence —
        // otherwise ON would move a correctly-claimed line onto the duplicate.
        let hunks = parse_hunks(DUP_SNIPPET);
        let near5 = ResolveTarget {
            snippet: Some("log(x);".to_owned()),
            claimed_line: Some(5),
        };
        assert_eq!(resolve_issue_lines(&near5, &hunks), Some((5, 5)));
        let near2 = ResolveTarget {
            snippet: Some("log(x);".to_owned()),
            claimed_line: Some(2),
        };
        assert_eq!(resolve_issue_lines(&near2, &hunks), Some((2, 2)));
        // No claim → first occurrence (line 2), preserving original behavior.
        let no_claim = ResolveTarget {
            snippet: Some("log(x);".to_owned()),
            claimed_line: None,
        };
        assert_eq!(resolve_issue_lines(&no_claim, &hunks), Some((2, 2)));
    }

    #[test]
    fn parses_multiple_hunks_independently() {
        let multi = "\
@@ -1,2 +1,3 @@
 alpha
+beta
 gamma
@@ -40,2 +41,3 @@
 delta
+epsilon
 zeta
";
        let hunks = parse_hunks(multi);
        assert_eq!(hunks.len(), 2);
        // `epsilon` is an addition in the second hunk: new_start 41 → line 42.
        let target = ResolveTarget {
            snippet: Some("epsilon".to_owned()),
            claimed_line: None,
        };
        assert_eq!(resolve_issue_lines(&target, &hunks), Some((42, 42)));
        // `beta` is in the first hunk: new_start 1 → line 2.
        let target2 = ResolveTarget {
            snippet: Some("beta".to_owned()),
            claimed_line: None,
        };
        assert_eq!(resolve_issue_lines(&target2, &hunks), Some((2, 2)));
    }

    #[test]
    fn skips_file_headers_before_first_hunk() {
        let with_headers = "\
diff --git a/src/x.rs b/src/x.rs
index e69de29..4b825dc 100644
--- a/src/x.rs
+++ b/src/x.rs
@@ -1,1 +1,2 @@
 keep
+added
";
        let hunks = parse_hunks(with_headers);
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].new_start, 1);
        let target = ResolveTarget {
            snippet: Some("added".to_owned()),
            claimed_line: None,
        };
        assert_eq!(resolve_issue_lines(&target, &hunks), Some((2, 2)));
    }
}
