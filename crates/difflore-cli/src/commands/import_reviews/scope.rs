use difflore_core::review_store::{ReviewCommentRecord, ReviewItemWithComments};

use crate::support::file_ext::{is_review_file_extension, is_source_code_extension};

pub(super) fn is_import_review_noise_line(line: &str) -> bool {
    let trimmed = line
        .trim()
        .trim_start_matches(['>', '-', '*'])
        .trim()
        .trim_matches('`')
        .trim();
    let lower = trimmed.to_ascii_lowercase();
    lower == "[!caution]"
        || lower.contains("some comments are outside the diff")
        || lower.starts_with("outside diff range comment")
        || lower.starts_with("outside diff range comments")
        || lower.starts_with("review table")
        || lower.starts_with("review details")
        || is_plus_more_scope_marker(trimmed)
        || is_review_table_wrapper_line(trimmed)
}

fn is_plus_more_scope_marker(value: &str) -> bool {
    let trimmed = value
        .trim()
        .trim_matches('|')
        .trim()
        .trim_end_matches(['.', ',', ';', ':'])
        .trim();
    let Some(rest) = trimmed.strip_prefix('+') else {
        return false;
    };
    let rest = rest.trim_start();
    let digit_count = rest.chars().take_while(char::is_ascii_digit).count();
    if digit_count == 0 {
        return false;
    }
    rest[digit_count..].trim_start().starts_with("more")
}

pub(super) fn is_review_table_wrapper_line(line: &str) -> bool {
    let trimmed = line.trim();
    if !trimmed.starts_with('|') {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    [
        "reviewable files",
        "files reviewed",
        "files changed",
        "additional comments",
        "outside diff",
        "comments outside the diff",
        "committable suggestions",
        "sequence diagrams",
        "review profile",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

pub(super) fn is_high_leverage_scope_path(path: &str) -> bool {
    let lower = path.replace('\\', "/").to_ascii_lowercase();
    let file = lower.rsplit('/').next().unwrap_or(lower.as_str());
    lower.starts_with(".github/workflows/") || is_manifest_filename(file)
}

fn is_manifest_filename(file_name: &str) -> bool {
    let lower = file_name.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "go.mod"
            | "go.sum"
            | "cargo.toml"
            | "cargo.lock"
            | "package.json"
            | "package-lock.json"
            | "pnpm-lock.yaml"
            | "yarn.lock"
            | "bun.lockb"
            | "uv.lock"
            | "poetry.lock"
            | "requirements.txt"
            | "dockerfile"
    )
}

fn source_scope_extension(path: &str) -> Option<String> {
    let normalized = path.trim().trim_start_matches("./").replace('\\', "/");
    let (_, ext_raw) = normalized.rsplit_once('.')?;
    if ext_raw.is_empty() || ext_raw.contains('/') || ext_raw.contains('\\') {
        return None;
    }
    let ext = ext_raw.to_ascii_lowercase();
    is_review_file_extension(&ext).then_some(ext)
}

fn has_source_scope_anchor(paths: &[String]) -> bool {
    paths
        .iter()
        .any(|path| !is_high_leverage_scope_path(path) && source_scope_extension(path).is_some())
}

fn drop_manifest_scope_when_source_anchor_exists(mut paths: Vec<String>) -> Vec<String> {
    if has_source_scope_anchor(&paths) {
        paths.retain(|path| !is_high_leverage_scope_path(path));
    }
    paths
}

pub(super) fn file_pattern_from_path(path: &str) -> Option<String> {
    file_patterns_from_path(path).into_iter().next()
}

pub(super) fn repo_wide_file_pattern_from_path(path: &str) -> Option<String> {
    let trimmed = path.trim().trim_start_matches("./");
    if trimmed.is_empty() || trimmed.contains(' ') {
        return None;
    }
    let normalized = trimmed.replace('\\', "/");
    if !normalized.contains('.') {
        return None;
    }
    let (_, ext_raw) = normalized.rsplit_once('.')?;
    if ext_raw.is_empty() || ext_raw.contains('/') || ext_raw.contains('\\') {
        return None;
    }
    let ext = ext_raw.to_ascii_lowercase();
    if !is_source_code_extension(&ext) {
        return None;
    }
    Some(format!("**/*.{ext}"))
}

/// Derive narrow and, for common monorepo layouts, sibling-broadening
/// file patterns from a comment-anchor path.
pub(super) fn file_patterns_from_path(path: &str) -> Vec<String> {
    let trimmed = path.trim().trim_start_matches("./");
    if trimmed.is_empty() || trimmed.contains(' ') {
        return Vec::new();
    }
    let normalized = trimmed.replace('\\', "/");
    let file_name = normalized.rsplit('/').next().unwrap_or(normalized.as_str());
    let exact_manifest = is_manifest_filename(file_name);
    if exact_manifest {
        let pat = if let Some((dir, _)) = normalized.rsplit_once('/') {
            format!("{dir}/**/{file_name}")
        } else {
            format!("**/{file_name}")
        };
        return vec![pat];
    }
    if !normalized.contains('.') {
        return Vec::new();
    }
    let Some((_, ext_raw)) = normalized.rsplit_once('.') else {
        return Vec::new();
    };
    if ext_raw.is_empty() || ext_raw.contains('/') || ext_raw.contains('\\') {
        return Vec::new();
    }
    let ext = ext_raw.to_ascii_lowercase();
    if !is_review_file_extension(&ext) {
        return Vec::new();
    }
    let dir = normalized.rsplit_once('/').map_or("**", |(dir, _)| dir);
    if ext == "md" && dir == "**" {
        return vec![format!("**/{file_name}")];
    }
    let narrow = if dir == "**" {
        format!("**/*.{ext}")
    } else {
        format!("{dir}/**/*.{ext}")
    };
    let mut out = vec![narrow];
    // Monorepo broadening for sibling packages with the same shape.
    for prefix in ["packages/", "apps/", "crates/", "pkg/", "examples/"] {
        if let Some(rest) = normalized.strip_prefix(prefix)
            && rest.contains('/')
        {
            // `packages/router-core/src/foo.ts` -> `packages/**/*.ts`
            let broad = format!("{prefix}**/*.{ext}");
            if !out.contains(&broad) {
                out.push(broad);
            }
            break;
        }
    }
    out
}

fn normalize_review_file_path(value: &str) -> Option<String> {
    let trimmed = value
        .trim()
        .trim_matches('`')
        .trim_matches('"')
        .trim_matches('\'')
        .trim()
        .trim_start_matches("./")
        .trim_end_matches([',', '.', ';', ':'])
        .replace('\\', "/");
    if trimmed.is_empty()
        || trimmed.contains(char::is_whitespace)
        || is_plus_more_scope_marker(&trimmed)
        || is_import_review_noise_line(&trimmed)
        || trimmed.starts_with("http://")
        || trimmed.starts_with("https://")
        || trimmed.contains('<')
        || trimmed.contains('>')
        || trimmed == "File"
        || trimmed.chars().all(|ch| ch == '-')
    {
        return None;
    }
    file_pattern_from_path(&trimmed).map(|_| trimmed)
}

fn backticked_segments(line: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut rest = line;
    while let Some(start) = rest.find('`') {
        rest = &rest[start + 1..];
        let Some(end) = rest.find('`') else {
            break;
        };
        segments.push(rest[..end].to_owned());
        rest = &rest[end + 1..];
    }
    segments
}

fn review_file_paths_from_content(content: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut push_path = |value: &str| {
        if let Some(path) = normalize_review_file_path(value)
            && (path.contains('/') || is_high_leverage_scope_path(&path))
            && seen.insert(path.clone())
        {
            paths.push(path);
        }
    };
    for line in content.lines() {
        if is_import_review_noise_line(line) {
            continue;
        }
        for segment in backticked_segments(line) {
            push_path(&segment);
        }
        let trimmed = line.trim();
        if trimmed.starts_with('|') {
            for cell in trimmed.split('|') {
                push_path(cell);
            }
        }
    }
    paths
}

pub(super) fn candidate_scope_paths(
    item: &ReviewItemWithComments,
    comment: &ReviewCommentRecord,
) -> Vec<String> {
    let mut paths = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut push_path = |value: &str| {
        if let Some(path) = normalize_review_file_path(value)
            && seen.insert(path.clone())
        {
            paths.push(path);
        }
    };
    if let Some(path) = comment_file_path(comment) {
        push_path(&path);
    }
    push_path(&item.item.file_path);
    for path in review_file_paths_from_content(&comment.content) {
        push_path(&path);
    }
    drop_manifest_scope_when_source_anchor_exists(paths)
}

fn comment_file_path(comment: &ReviewCommentRecord) -> Option<String> {
    let metadata = comment.metadata.as_deref()?;
    let value: serde_json::Value = serde_json::from_str(metadata).ok()?;
    value
        .get("filePath")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use difflore_core::review_store::ReviewItemRecord;

    fn review_item(file_path: &str) -> ReviewItemWithComments {
        ReviewItemWithComments {
            item: ReviewItemRecord {
                id: "item-1".to_owned(),
                session_id: None,
                project_id: None,
                file_path: file_path.to_owned(),
                diff_content: String::new(),
                status: "open".to_owned(),
                source: "github".to_owned(),
                source_kind: "pull_request".to_owned(),
                external_review_id: Some("1".to_owned()),
                repo_full_name: Some("owner/repo".to_owned()),
                pr_number: Some(1),
                author: Some("reviewer".to_owned()),
                synced_at: None,
                metadata: None,
                created_at: "2026-06-15 00:00:00".to_owned(),
                reviewed_at: None,
            },
            comments: Vec::new(),
        }
    }

    fn review_comment(content: &str) -> ReviewCommentRecord {
        ReviewCommentRecord {
            id: "comment-1".to_owned(),
            review_item_id: "item-1".to_owned(),
            external_comment_id: Some("c1".to_owned()),
            line_number: None,
            content: content.to_owned(),
            author: Some("reviewer".to_owned()),
            comment_url: Some("https://example.test/review".to_owned()),
            thread_id: None,
            metadata: None,
            created_at: "2026-06-15 00:00:00".to_owned(),
        }
    }

    #[test]
    fn candidate_scope_paths_drop_manifest_when_source_anchor_exists() {
        let item = review_item("package.json");
        let comment = review_comment(
            "Use the base media component in `src/components/developers/Hero.tsx`; \
             package.json changed only because the PR touched dependencies.",
        );

        let paths = candidate_scope_paths(&item, &comment);

        assert_eq!(paths, vec!["src/components/developers/Hero.tsx"]);
    }

    #[test]
    fn candidate_scope_paths_keep_manifest_without_source_anchor() {
        let item = review_item("package.json");
        let comment = review_comment("Keep dependency versions aligned in `package.json`.");

        let paths = candidate_scope_paths(&item, &comment);

        assert_eq!(paths, vec!["package.json"]);
    }
}
