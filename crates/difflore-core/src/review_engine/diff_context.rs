//! Token-aware-ish packing for already collected PR diffs.
//!
//! This module deliberately starts after the existing PR fetch and
//! merge-base diff step. It never shells out, fetches refs, or decides which
//! commits belong to a PR; callers pass file records that were already
//! produced from a merge-base diff.

use std::cmp::Ordering;

/// Caller intent for ordering records before fitting them into a budget.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DiffContextMode {
    /// Prefer highly relevant files, then smaller files for broad review
    /// coverage.
    #[default]
    ReviewExtraction,
    /// Prefer highly relevant files, then files with more changed lines,
    /// while still fitting smaller records first when otherwise tied.
    FixPr,
}

/// Change kind for a diff record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffContextFileChange {
    Added,
    Modified,
    Renamed,
    Deleted,
}

impl DiffContextFileChange {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Modified => "modified",
            Self::Renamed => "renamed",
            Self::Deleted => "deleted",
        }
    }
}

/// One file-level diff record produced by an upstream merge-base diff.
#[derive(Debug, Clone, Copy)]
pub struct DiffContextFile<'a> {
    pub path: &'a str,
    pub patch: &'a str,
    /// Higher values are packed earlier. Use zero when no external
    /// relevance signal is available.
    pub relevance: u16,
    pub change: DiffContextFileChange,
}

impl<'a> DiffContextFile<'a> {
    pub const fn new(path: &'a str, patch: &'a str) -> Self {
        Self {
            path,
            patch,
            relevance: 0,
            change: DiffContextFileChange::Modified,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct DiffContextOptions {
    /// Maximum character count for the packed diff text. This is deliberately
    /// a character budget, not a real tokenizer.
    pub char_budget: Option<usize>,
    pub mode: DiffContextMode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackedDiffFile {
    pub path: String,
    pub change: DiffContextFileChange,
    pub relevance: u16,
    pub original_chars: usize,
    pub included_chars: usize,
    pub additions: usize,
    pub deletions: usize,
    pub truncated: bool,
}

/// Why a file appears in the summary list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffContextSummaryReason {
    DeletedFile,
    EmptyPatch,
    OmittedForBudget,
    TruncatedForBudget,
}

/// Summary for a deleted, omitted, or truncated file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffContextSummary {
    pub path: String,
    pub change: DiffContextFileChange,
    pub reason: DiffContextSummaryReason,
    pub original_chars: usize,
    pub included_chars: usize,
    pub additions: usize,
    pub deletions: usize,
    pub summary: String,
}

/// Packed diff text plus bookkeeping for files that were not fully included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackedDiffContext {
    pub text: String,
    pub included_files: Vec<PackedDiffFile>,
    pub summaries: Vec<DiffContextSummary>,
    pub char_budget: Option<usize>,
    pub packed_chars: usize,
    pub original_chars: usize,
}

/// Pack file-level diff records into a deterministic context block.
///
/// The algorithm is intentionally small:
/// 1. Sort records by mode-specific priority, using relevance and patch size.
/// 2. Include full file patches while they fit the optional character budget.
/// 3. When a high-priority file does not fit, include a compact patch made of
///    file/hunk headers, changed lines, and adjacent context lines.
/// 4. Return summaries for deleted, omitted, empty, and truncated records.
pub fn pack_diff_context(
    files: &[DiffContextFile<'_>],
    options: DiffContextOptions,
) -> PackedDiffContext {
    let mut ordered: Vec<(usize, &DiffContextFile<'_>)> = files.iter().enumerate().collect();
    ordered.sort_by(|(a_idx, a), (b_idx, b)| compare_files(a, *a_idx, b, *b_idx, options.mode));

    let mut text = String::new();
    let mut included_files = Vec::new();
    let mut summaries = Vec::new();
    let mut packed_chars = 0usize;
    let mut original_chars = 0usize;

    for (_idx, file) in ordered {
        let path = file.path.trim();
        let patch = file.patch.trim_end();
        let change = effective_change(file);
        let patch_chars = char_count(patch);
        let (additions, deletions) = count_changed_lines(patch);
        original_chars = original_chars.saturating_add(patch_chars);

        if path.is_empty() || patch.trim().is_empty() {
            summaries.push(build_summary(
                path,
                change,
                DiffContextSummaryReason::EmptyPatch,
                patch_chars,
                0,
                additions,
                deletions,
            ));
            continue;
        }

        if change == DiffContextFileChange::Deleted {
            summaries.push(build_summary(
                path,
                change,
                DiffContextSummaryReason::DeletedFile,
                patch_chars,
                0,
                additions,
                deletions,
            ));
            continue;
        }

        let section = render_file_section(path, patch);
        let section_chars = char_count(&section);
        if fits_budget(packed_chars, section_chars, options.char_budget) {
            text.push_str(&section);
            packed_chars = packed_chars.saturating_add(section_chars);
            included_files.push(PackedDiffFile {
                path: path.to_owned(),
                change,
                relevance: file.relevance,
                original_chars: patch_chars,
                included_chars: section_chars,
                additions,
                deletions,
                truncated: false,
            });
            continue;
        }

        let Some(char_budget) = options.char_budget else {
            continue;
        };
        let remaining = char_budget.saturating_sub(packed_chars);
        if let Some(compact_section) = render_compact_file_section(path, patch, remaining) {
            let compact_chars = char_count(&compact_section);
            text.push_str(&compact_section);
            packed_chars = packed_chars.saturating_add(compact_chars);
            included_files.push(PackedDiffFile {
                path: path.to_owned(),
                change,
                relevance: file.relevance,
                original_chars: patch_chars,
                included_chars: compact_chars,
                additions,
                deletions,
                truncated: true,
            });
            summaries.push(build_summary(
                path,
                change,
                DiffContextSummaryReason::TruncatedForBudget,
                patch_chars,
                compact_chars,
                additions,
                deletions,
            ));
        } else {
            summaries.push(build_summary(
                path,
                change,
                DiffContextSummaryReason::OmittedForBudget,
                patch_chars,
                0,
                additions,
                deletions,
            ));
        }
    }

    PackedDiffContext {
        text,
        included_files,
        summaries,
        char_budget: options.char_budget,
        packed_chars,
        original_chars,
    }
}

fn compare_files(
    a: &DiffContextFile<'_>,
    a_idx: usize,
    b: &DiffContextFile<'_>,
    b_idx: usize,
    mode: DiffContextMode,
) -> Ordering {
    let a_change = effective_change(a);
    let b_change = effective_change(b);
    let a_active_rank = active_rank(a_change);
    let b_active_rank = active_rank(b_change);
    let a_chars = char_count(a.patch.trim_end());
    let b_chars = char_count(b.patch.trim_end());
    let a_changed = changed_line_total(a.patch);
    let b_changed = changed_line_total(b.patch);
    let a_path = a.path.trim();
    let b_path = b.path.trim();

    match mode {
        DiffContextMode::ReviewExtraction => b
            .relevance
            .cmp(&a.relevance)
            .then_with(|| a_active_rank.cmp(&b_active_rank))
            .then_with(|| a_chars.cmp(&b_chars))
            .then_with(|| a_path.cmp(b_path))
            .then_with(|| a_idx.cmp(&b_idx)),
        DiffContextMode::FixPr => b
            .relevance
            .cmp(&a.relevance)
            .then_with(|| a_active_rank.cmp(&b_active_rank))
            .then_with(|| b_changed.cmp(&a_changed))
            .then_with(|| a_chars.cmp(&b_chars))
            .then_with(|| a_path.cmp(b_path))
            .then_with(|| a_idx.cmp(&b_idx)),
    }
}

const fn active_rank(change: DiffContextFileChange) -> u8 {
    match change {
        DiffContextFileChange::Deleted => 1,
        DiffContextFileChange::Added
        | DiffContextFileChange::Modified
        | DiffContextFileChange::Renamed => 0,
    }
}

fn effective_change(file: &DiffContextFile<'_>) -> DiffContextFileChange {
    if file.change == DiffContextFileChange::Deleted || patch_indicates_deleted_file(file.patch) {
        DiffContextFileChange::Deleted
    } else {
        file.change
    }
}

fn fits_budget(current_chars: usize, added_chars: usize, budget: Option<usize>) -> bool {
    budget.is_none_or(|limit| current_chars.saturating_add(added_chars) <= limit)
}

fn render_file_section(path: &str, patch: &str) -> String {
    let mut section = String::new();
    section.push_str("\n\n## File: ");
    section.push_str(path);
    section.push_str("\n\n```diff\n");
    section.push_str(patch.trim_end());
    section.push_str("\n```\n");
    section
}

fn render_compact_file_section(path: &str, patch: &str, max_chars: usize) -> Option<String> {
    const TRUNCATED_MARKER: &str = "... [diff context truncated]\n";
    let prefix = format!("\n\n## File: {path}\n\n```diff\n");
    let suffix = "```\n";
    let separator = "\n";
    let overhead = char_count(&prefix)
        .saturating_add(char_count(separator))
        .saturating_add(char_count(TRUNCATED_MARKER))
        .saturating_add(char_count(suffix));
    if max_chars <= overhead {
        return None;
    }

    let patch_budget = max_chars.saturating_sub(overhead);
    let compact_patch = compact_patch_lines(patch, patch_budget);
    if compact_patch.trim().is_empty() {
        return None;
    }

    let mut section = prefix;
    section.push_str(compact_patch.trim_end());
    section.push_str(separator);
    section.push_str(TRUNCATED_MARKER);
    section.push_str(suffix);

    (char_count(&section) <= max_chars).then_some(section)
}

fn compact_patch_lines(patch: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }

    let lines: Vec<&str> = patch.trim_end().lines().collect();
    if lines.is_empty() {
        return String::new();
    }

    let mut keep = vec![false; lines.len()];
    for (idx, line) in lines.iter().enumerate() {
        if is_key_patch_line(line) {
            keep[idx] = true;
            if idx > 0 && is_context_line(lines[idx - 1]) {
                keep[idx - 1] = true;
            }
            if idx + 1 < lines.len() && is_context_line(lines[idx + 1]) {
                keep[idx + 1] = true;
            }
        }
    }

    if !keep.iter().any(|keep_line| *keep_line) {
        return take_chars(patch.trim(), max_chars);
    }

    let mut out = String::new();
    let mut out_chars = 0usize;
    let mut skipped = false;
    let mut included_any = false;

    for (idx, line) in lines.iter().enumerate() {
        if !keep[idx] {
            skipped = true;
            continue;
        }

        if skipped && included_any && try_push_line(&mut out, &mut out_chars, "...", max_chars) {
            skipped = false;
        }

        if try_push_line(&mut out, &mut out_chars, line, max_chars) {
            included_any = true;
            continue;
        }

        if !included_any {
            push_partial_line(&mut out, &mut out_chars, line, max_chars);
        }
        break;
    }

    out.trim_end().to_owned()
}

fn try_push_line(out: &mut String, out_chars: &mut usize, line: &str, max_chars: usize) -> bool {
    let needed = char_count(line).saturating_add(1);
    if out_chars.saturating_add(needed) > max_chars {
        return false;
    }
    out.push_str(line);
    out.push('\n');
    *out_chars = out_chars.saturating_add(needed);
    true
}

fn push_partial_line(out: &mut String, out_chars: &mut usize, line: &str, max_chars: usize) {
    let remaining = max_chars.saturating_sub(*out_chars);
    if remaining == 0 {
        return;
    }
    let line_part = if remaining > 1 {
        take_chars(line, remaining - 1)
    } else {
        String::new()
    };
    out.push_str(&line_part);
    if remaining > 1 {
        out.push('\n');
    }
    *out_chars = max_chars;
}

fn is_key_patch_line(line: &str) -> bool {
    line.starts_with("diff --git ")
        || line.starts_with("index ")
        || line.starts_with("old mode ")
        || line.starts_with("new mode ")
        || line.starts_with("new file mode ")
        || line.starts_with("deleted file mode ")
        || line.starts_with("similarity index ")
        || line.starts_with("rename from ")
        || line.starts_with("rename to ")
        || line.starts_with("--- ")
        || line.starts_with("+++ ")
        || line.starts_with("@@ ")
        || line.starts_with("Binary files ")
        || is_changed_line(line)
}

fn is_context_line(line: &str) -> bool {
    line.starts_with(' ')
}

fn is_changed_line(line: &str) -> bool {
    (line.starts_with('+') && !line.starts_with("+++"))
        || (line.starts_with('-') && !line.starts_with("---"))
}

fn count_changed_lines(patch: &str) -> (usize, usize) {
    let mut additions = 0usize;
    let mut deletions = 0usize;
    for line in patch.lines() {
        if line.starts_with('+') && !line.starts_with("+++") {
            additions = additions.saturating_add(1);
        } else if line.starts_with('-') && !line.starts_with("---") {
            deletions = deletions.saturating_add(1);
        }
    }
    (additions, deletions)
}

fn changed_line_total(patch: &str) -> usize {
    let (additions, deletions) = count_changed_lines(patch);
    additions.saturating_add(deletions)
}

fn patch_indicates_deleted_file(patch: &str) -> bool {
    patch
        .lines()
        .any(|line| line.trim() == "+++ /dev/null" || line.starts_with("deleted file mode "))
}

fn build_summary(
    path: &str,
    change: DiffContextFileChange,
    reason: DiffContextSummaryReason,
    original_chars: usize,
    included_chars: usize,
    additions: usize,
    deletions: usize,
) -> DiffContextSummary {
    let reason_text = match reason {
        DiffContextSummaryReason::DeletedFile => "summarized because the file was deleted",
        DiffContextSummaryReason::EmptyPatch => "omitted because the patch was empty",
        DiffContextSummaryReason::OmittedForBudget => {
            "deferred because the char budget was exhausted"
        }
        DiffContextSummaryReason::TruncatedForBudget => {
            "partially included with key patch context because the full patch exceeded budget"
        }
    };
    let summary = format!(
        "{} ({}, +{}, -{}, {} chars): {}",
        path,
        change.as_str(),
        additions,
        deletions,
        original_chars,
        reason_text
    );

    DiffContextSummary {
        path: path.to_owned(),
        change,
        reason,
        original_chars,
        included_chars,
        additions,
        deletions,
        summary,
    }
}

fn char_count(s: &str) -> usize {
    s.chars().count()
}

fn take_chars(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}
