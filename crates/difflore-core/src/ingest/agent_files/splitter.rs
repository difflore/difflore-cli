//! Split cross-vendor agent memory files into DiffLore memory entries.

use std::collections::HashMap;
use std::path::Path;

use super::MemoryDoc;

const MAX_TITLE_CHARS: usize = 96;
const MAX_BODY_CHARS: usize = 12_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentFileMemoryKind {
    ReviewRule,
    SoftPreference,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentFileMemoryEntry {
    pub title: String,
    pub body: String,
    pub kind: AgentFileMemoryKind,
    pub category: Option<String>,
    pub file_patterns: Vec<String>,
    pub source_id: &'static str,
    pub path: std::path::PathBuf,
}

#[derive(Debug, Clone, Default)]
struct Frontmatter {
    values: HashMap<String, String>,
    file_patterns: Vec<String>,
}

#[derive(Debug, Clone)]
struct SplitCandidate {
    title: String,
    body: String,
}

pub fn split_memory_doc(doc: &MemoryDoc) -> Vec<AgentFileMemoryEntry> {
    if is_memory_md(&doc.path) {
        return Vec::new();
    }

    let content = strip_difflore_context(&doc.content);
    let (frontmatter, body) = split_frontmatter(&content);
    let claude_type = frontmatter
        .values
        .get("metadata.type")
        .or_else(|| frontmatter.values.get("type"))
        .map(|value| value.trim().to_ascii_lowercase());
    if claude_type.as_deref() == Some("reference") {
        return Vec::new();
    }

    let forced_kind = match claude_type.as_deref() {
        Some("feedback") => Some((AgentFileMemoryKind::ReviewRule, None)),
        Some("user") => Some((
            AgentFileMemoryKind::SoftPreference,
            Some("user_preference".to_owned()),
        )),
        Some("project") => Some((
            AgentFileMemoryKind::SoftPreference,
            Some("project_context".to_owned()),
        )),
        _ => None,
    };

    split_freeform(body)
        .into_iter()
        .filter_map(|candidate| {
            let body = clamp_body(clean_body(&candidate.body));
            if !is_meaningful_body(&body) {
                return None;
            }
            let title = clean_title(&candidate.title)
                .filter(|title| !title.is_empty())
                .unwrap_or_else(|| title_from_body(&body));
            // Conservative default for freeform agent files: project docs and
            // editor rule files often contain broad reference/context sections.
            // Unless frontmatter explicitly says `type: user` or `type:
            // project`, route entries through pending review so onboarding
            // cannot silently activate noisy rules.
            let (kind, category) = forced_kind
                .clone()
                .unwrap_or((AgentFileMemoryKind::ReviewRule, None));

            Some(AgentFileMemoryEntry {
                title,
                body,
                kind,
                category,
                file_patterns: frontmatter.file_patterns.clone(),
                source_id: doc.source_id,
                path: doc.path.clone(),
            })
        })
        .collect()
}

fn is_memory_md(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("MEMORY.md"))
}

fn strip_difflore_context(input: &str) -> String {
    let mut remaining = input.to_owned();
    loop {
        let lower = remaining.to_ascii_lowercase();
        let Some(start) = lower.find("<difflore-context>") else {
            return remaining;
        };
        let search_from = start + "<difflore-context>".len();
        let Some(rel_end) = lower[search_from..].find("</difflore-context>") else {
            remaining.truncate(start);
            return remaining;
        };
        let end = search_from + rel_end + "</difflore-context>".len();
        remaining.replace_range(start..end, "\n");
    }
}

fn split_frontmatter(input: &str) -> (Frontmatter, &str) {
    if !input.starts_with("---") {
        return (Frontmatter::default(), input);
    }
    let mut offset = 0usize;
    let mut lines = input.lines();
    let Some(first) = lines.next() else {
        return (Frontmatter::default(), input);
    };
    offset += first.len() + 1;
    if first.trim() != "---" {
        return (Frontmatter::default(), input);
    }

    let mut raw = String::new();
    for line in lines {
        let line_len = line.len() + 1;
        if line.trim() == "---" {
            offset += line_len;
            return (parse_frontmatter(&raw), input.get(offset..).unwrap_or(""));
        }
        raw.push_str(line);
        raw.push('\n');
        offset += line_len;
    }
    (Frontmatter::default(), input)
}

fn parse_frontmatter(raw: &str) -> Frontmatter {
    let mut values = HashMap::new();
    let mut file_patterns = Vec::new();
    let mut section: Option<String> = None;

    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let is_nested = line.starts_with(' ') || line.starts_with('\t');
        if !is_nested && trimmed.ends_with(':') && !trimmed.contains(": ") {
            section = Some(trimmed.trim_end_matches(':').to_ascii_lowercase());
            continue;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let key = if is_nested {
            section
                .as_ref()
                .map(|section| format!("{section}.{key}"))
                .unwrap_or(key)
        } else {
            section = None;
            key
        };
        let value = value.trim().trim_matches('"').trim_matches('\'').to_owned();
        if matches!(
            key.as_str(),
            "globs" | "glob" | "file_patterns" | "file-patterns"
        ) {
            file_patterns.extend(parse_list_value(&value));
        }
        values.insert(key, value);
    }

    file_patterns.sort();
    file_patterns.dedup();
    Frontmatter {
        values,
        file_patterns,
    }
}

fn parse_list_value(value: &str) -> Vec<String> {
    let trimmed = value.trim().trim_start_matches('[').trim_end_matches(']');
    trimmed
        .split(',')
        .map(|part| part.trim().trim_matches('"').trim_matches('\''))
        .filter(|part| !part.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn split_freeform(body: &str) -> Vec<SplitCandidate> {
    let body = body.trim();
    if body.is_empty() {
        return Vec::new();
    }

    let heading_sections = split_heading_sections(body);
    if !heading_sections.is_empty() {
        return heading_sections;
    }

    let bullet_sections = split_bullets(body);
    if !bullet_sections.is_empty() {
        return bullet_sections;
    }

    vec![SplitCandidate {
        title: title_from_body(body),
        body: body.to_owned(),
    }]
}

fn split_heading_sections(body: &str) -> Vec<SplitCandidate> {
    let mut sections = Vec::new();
    let mut current_title: Option<String> = None;
    let mut current_body = Vec::new();

    for line in body.lines() {
        if let Some(title) = heading_text(line) {
            flush_section(&mut sections, &mut current_title, &mut current_body);
            current_title = Some(title.to_owned());
            continue;
        }
        if current_title.is_some() {
            current_body.push(line);
        }
    }
    flush_section(&mut sections, &mut current_title, &mut current_body);
    sections
}

fn flush_section(
    sections: &mut Vec<SplitCandidate>,
    current_title: &mut Option<String>,
    current_body: &mut Vec<&str>,
) {
    let Some(title) = current_title.take() else {
        current_body.clear();
        return;
    };
    let body = current_body.join("\n");
    current_body.clear();
    if is_meaningful_body(&body) {
        sections.push(SplitCandidate { title, body });
    }
}

fn heading_text(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    let hashes = trimmed.chars().take_while(|c| *c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    trimmed.get(hashes..)?.trim().strip_prefix(' ').or_else(|| {
        let value = trimmed.get(hashes..)?.trim();
        (!value.is_empty()).then_some(value)
    })
}

fn split_bullets(body: &str) -> Vec<SplitCandidate> {
    let mut bullets = Vec::new();
    let mut current = String::new();

    for line in body.lines() {
        if let Some(text) = bullet_text(line) {
            flush_bullet(&mut bullets, &mut current);
            current.push_str(text);
        } else if !current.is_empty() && !line.trim().is_empty() {
            current.push(' ');
            current.push_str(line.trim());
        }
    }
    flush_bullet(&mut bullets, &mut current);
    bullets
}

fn flush_bullet(bullets: &mut Vec<SplitCandidate>, current: &mut String) {
    let body = current.trim();
    if is_meaningful_body(body) {
        bullets.push(SplitCandidate {
            title: title_from_body(body),
            body: body.to_owned(),
        });
    }
    current.clear();
}

fn bullet_text(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    for marker in ["- ", "* ", "+ "] {
        if let Some(rest) = trimmed.strip_prefix(marker) {
            return Some(rest.trim());
        }
    }
    let dot = trimmed.find('.')?;
    if dot == 0 || dot > 3 {
        return None;
    }
    trimmed[..dot]
        .chars()
        .all(|c| c.is_ascii_digit())
        .then(|| trimmed[dot + 1..].trim())
        .filter(|rest| !rest.is_empty())
}

fn clean_body(body: &str) -> String {
    body.lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned()
}

fn clamp_body(body: String) -> String {
    if body.chars().count() <= MAX_BODY_CHARS {
        return body;
    }
    let mut truncated = body.chars().take(MAX_BODY_CHARS).collect::<String>();
    truncated.push_str("\n\n[Truncated from agent file.]");
    truncated
}

fn clean_title(title: &str) -> Option<String> {
    let mut title = title.trim();
    while let Some(rest) = title.strip_prefix('#') {
        title = rest.trim_start();
    }
    title = title
        .trim_start_matches(['-', '*', '+'])
        .trim_start()
        .trim_matches('`')
        .trim();
    if title.is_empty() {
        return None;
    }
    Some(limit_chars(title, MAX_TITLE_CHARS))
}

fn title_from_body(body: &str) -> String {
    let first = body
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("Agent file memory");
    let first = first.split(['.', '!', '?']).next().unwrap_or(first).trim();
    clean_title(first).unwrap_or_else(|| "Agent file memory".to_owned())
}

fn limit_chars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_owned();
    }
    value.chars().take(max_chars).collect::<String>()
}

fn is_meaningful_body(body: &str) -> bool {
    body.split_whitespace().count() >= 3
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn doc(source_id: &'static str, path: &str, content: &str) -> MemoryDoc {
        MemoryDoc {
            source_id,
            path: PathBuf::from(path),
            content: content.to_owned(),
            modified_at: None,
        }
    }

    #[test]
    fn claude_feedback_frontmatter_becomes_review_rule() {
        let entries = split_memory_doc(&doc(
            "claude-code-memory",
            "review.md",
            "---\nmetadata:\n  type: feedback\nglobs: [src/**/*.rs]\n---\nNever unwrap in production request paths.",
        ));

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, AgentFileMemoryKind::ReviewRule);
        assert_eq!(entries[0].file_patterns, vec!["src/**/*.rs"]);
    }

    #[test]
    fn claude_user_frontmatter_becomes_soft_preference() {
        let entries = split_memory_doc(&doc(
            "claude-code-memory",
            "user.md",
            "---\ntype: user\n---\nThe user prefers concise final answers.",
        ));

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, AgentFileMemoryKind::SoftPreference);
        assert_eq!(entries[0].category.as_deref(), Some("user_preference"));
    }

    #[test]
    fn reference_and_memory_md_are_skipped() {
        assert!(
            split_memory_doc(&doc("claude-code-memory", "MEMORY.md", "Never ship x")).is_empty()
        );
        assert!(
            split_memory_doc(&doc(
                "claude-code-memory",
                "reference.md",
                "---\ntype: reference\n---\nReference docs only.",
            ))
            .is_empty()
        );
    }

    #[test]
    fn strips_difflore_context_blocks_before_splitting() {
        let entries = split_memory_doc(&doc(
            "agents-md",
            "AGENTS.md",
            "<difflore-context>\nold generated context\n</difflore-context>\nAlways run cargo fmt before tests.",
        ));

        assert_eq!(entries.len(), 1);
        assert!(!entries[0].body.contains("generated context"));
        assert_eq!(entries[0].kind, AgentFileMemoryKind::ReviewRule);
    }

    #[test]
    fn headings_and_bullets_split_into_individual_entries() {
        let heading_entries = split_memory_doc(&doc(
            "agents-md",
            "AGENTS.md",
            "# Tests\nAlways run focused tests first.\n\n# Style\nKeep UI copy in English.",
        ));
        assert_eq!(heading_entries.len(), 2);
        assert_eq!(heading_entries[0].title, "Tests");

        let bullet_entries = split_memory_doc(&doc(
            "gemini-md",
            "GEMINI.md",
            "- Use `rg` before slower search tools.\n- The repo uses pnpm for frontend work.",
        ));
        assert_eq!(bullet_entries.len(), 2);
        assert_eq!(bullet_entries[0].kind, AgentFileMemoryKind::ReviewRule);
        assert_eq!(bullet_entries[1].kind, AgentFileMemoryKind::ReviewRule);
    }
}
