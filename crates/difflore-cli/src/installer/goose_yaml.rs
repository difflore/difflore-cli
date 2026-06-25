//! Goose YAML config helpers — line-based string manipulation to avoid
//! pulling in a YAML parser. Mirrors claude-mem's `mergeGooseYamlConfig`.

use std::{fs, path::PathBuf};

use anyhow::bail;

use super::{InstallState, TargetStatus, common::MCP_SERVER_ARG};

/// Merge a `difflore` entry under the top-level `mcpServers:` block in a Goose
/// YAML config. Returns true if an existing `difflore:` entry was replaced.
pub(super) fn merge_goose_yaml_config(
    path: &PathBuf,
    bin: &str,
    dry_run: bool,
) -> Result<bool, String> {
    let existing = if path.exists() {
        fs::read_to_string(path).map_err(|e| format!("failed to read {}: {e}", path.display()))?
    } else {
        String::new()
    };

    let (new_content, replaced) = if existing.is_empty() {
        let entry_block = render_goose_block(bin);
        let header = format!("mcpServers:\n{entry_block}");
        (header, false)
    } else if yaml_has_difflore_under_mcp_servers(&existing) {
        (
            replace_goose_difflore_block(&existing, bin).map_err(|e| format!("{e:#}"))?,
            true,
        )
    } else if has_top_level_mcp_servers(&existing) {
        (
            insert_under_mcp_servers(&existing, bin).map_err(|e| format!("{e:#}"))?,
            false,
        )
    } else {
        let entry_block = render_goose_block(bin);
        let mut out = existing.trim_end().to_owned();
        out.push('\n');
        out.push_str("mcpServers:\n");
        out.push_str(&entry_block);
        (out, false)
    };

    if !dry_run {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
        }
        super::common::write_atomic(path, new_content.as_bytes())
            .map_err(|e| format!("failed to write {}: {e}", path.display()))?;
    }
    Ok(replaced)
}

/// Inverse of [`merge_goose_yaml_config`]: remove the `difflore:` block (and
/// its deeper-indented children) from under `mcpServers:`. If that leaves
/// `mcpServers:` empty, drop the now-orphaned header line too. Returns true if a
/// block was removed; missing file / no block is a no-op returning false.
pub(super) fn remove_goose_yaml_config(path: &PathBuf, dry_run: bool) -> Result<bool, String> {
    if !path.exists() {
        return Ok(false);
    }
    let existing =
        fs::read_to_string(path).map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    if !yaml_has_difflore_under_mcp_servers(&existing) {
        return Ok(false);
    }
    let stripped = remove_goose_difflore_block(&existing).map_err(|e| format!("{e:#}"))?;
    let new_content = drop_empty_mcp_servers_block(&stripped);
    if !dry_run {
        super::common::write_atomic(path, new_content.as_bytes())
            .map_err(|e| format!("failed to write {}: {e}", path.display()))?;
    }
    Ok(true)
}

/// Remove the first `difflore:` block under `mcpServers:` and its
/// deeper-indented children. Like [`replace_goose_difflore_block`] but emits no
/// replacement.
fn remove_goose_difflore_block(yaml: &str) -> anyhow::Result<String> {
    let mut out = String::new();
    let mut lines = yaml.split_inclusive('\n').peekable();
    let mut found = false;
    let mut in_mcp_servers = false;
    let mut child_indent: Option<usize> = None;
    while let Some(line) = lines.next() {
        if let Some(key) = top_level_key(line) {
            in_mcp_servers = key == "mcpServers" && top_level_block_key(line) == Some("mcpServers");
            child_indent = None;
            out.push_str(line);
            continue;
        }
        if in_mcp_servers && child_indent.is_none() {
            child_indent = mcp_child_indent_from_line(line);
        }
        if !found
            && in_mcp_servers
            && child_indent.is_some_and(|indent| is_indented_key(line, "difflore", indent))
        {
            let indent = child_indent.unwrap_or(2);
            found = true;
            while let Some(next) = lines.peek() {
                if next.trim().is_empty() || indent_of(next) > indent {
                    lines.next();
                } else {
                    break;
                }
            }
            continue;
        }
        out.push_str(line);
    }
    if !found {
        bail!("could not locate existing difflore block under mcpServers");
    }
    Ok(out)
}

/// If `mcpServers:` has no remaining child key, drop the header line so we
/// don't leave an orphaned section. Other top-level content is preserved.
fn drop_empty_mcp_servers_block(yaml: &str) -> String {
    let mut out = String::new();
    let mut lines = yaml.split_inclusive('\n');
    while let Some(line) = lines.next() {
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if indent_of(trimmed) == 0 && trimmed.trim_end() == "mcpServers:" {
            // Peek the next non-blank line: if it isn't a child (indent > 0),
            // the block is empty and we skip the header.
            let has_child = lines
                .clone()
                .find(|l| !l.trim().is_empty())
                .is_some_and(|l| indent_of(l) > 0);
            if !has_child {
                continue;
            }
        }
        out.push_str(line);
    }
    out
}

pub(super) fn yaml_escape_scalar(s: &str) -> String {
    // Only need to quote if the scalar contains chars YAML would interpret
    // (colons, leading/trailing whitespace, special tokens). Windows paths
    // embed backslashes and the drive colon `C:` — both need quoting.
    let needs_quote = s.contains(':')
        || s.contains('#')
        || s.contains('\\')
        || s.starts_with(' ')
        || s.ends_with(' ')
        || s.is_empty();
    if needs_quote {
        // Single-quoted YAML scalar: double embedded single quotes.
        format!("'{}'", s.replace('\'', "''"))
    } else {
        s.to_owned()
    }
}

fn yaml_has_difflore_under_mcp_servers(yaml: &str) -> bool {
    // Only a `difflore:` key that is an actual child of the TOP-LEVEL
    // `mcpServers:` section counts. A `difflore:` nested under some other
    // section must not be detected (and later clobbered).
    let mut in_mcp_servers = false;
    let mut child_indent: Option<usize> = None;
    for line in yaml.lines() {
        if let Some(key) = top_level_key(line) {
            in_mcp_servers = key == "mcpServers" && top_level_block_key(line) == Some("mcpServers");
            child_indent = None;
            continue;
        }
        if in_mcp_servers && child_indent.is_none() {
            child_indent = mcp_child_indent_from_line(line);
        }
        if in_mcp_servers
            && child_indent.is_some_and(|indent| is_indented_key(line, "difflore", indent))
        {
            return true;
        }
    }
    false
}

/// Replace an existing `difflore:` block under `mcpServers:` with
/// `replacement`. The block is the `difflore:` line at the section child
/// indentation plus
/// all following lines indented deeper.
fn replace_goose_difflore_block(yaml: &str, bin: &str) -> anyhow::Result<String> {
    let mut out = String::new();
    let mut lines = yaml.split_inclusive('\n').peekable();
    let mut found = false;
    let mut in_mcp_servers = false;
    let mut child_indent: Option<usize> = None;
    while let Some(line) = lines.next() {
        if let Some(key) = top_level_key(line) {
            in_mcp_servers = key == "mcpServers" && top_level_block_key(line) == Some("mcpServers");
            child_indent = None;
            out.push_str(line);
            continue;
        }
        if in_mcp_servers && child_indent.is_none() {
            child_indent = mcp_child_indent_from_line(line);
        }
        if !found
            && in_mcp_servers
            && child_indent.is_some_and(|indent| is_indented_key(line, "difflore", indent))
        {
            let indent = child_indent.unwrap_or(2);
            // Emit the replacement, then skip this line and its children.
            out.push_str(&render_goose_block_at_indent(bin, indent));
            found = true;
            while let Some(next) = lines.peek() {
                if next.trim().is_empty() || indent_of(next) > indent {
                    lines.next();
                } else {
                    break;
                }
            }
            continue;
        }
        out.push_str(line);
    }
    if !found {
        bail!("could not locate existing difflore block under mcpServers");
    }
    Ok(out)
}

/// Insert a new `  difflore:` block as the first child of the existing
/// `mcpServers:` section. Scoped to the TOP-LEVEL `mcpServers:` line, not the
/// first `mcpServers:` substring (which could match a comment or value).
fn insert_under_mcp_servers(yaml: &str, bin: &str) -> anyhow::Result<String> {
    let entry_block = render_goose_block_at_indent(bin, mcp_child_indent(yaml).unwrap_or(2));
    let mut offset = 0usize;
    for line in yaml.split_inclusive('\n') {
        if top_level_key(line) == Some("mcpServers") {
            if top_level_block_key(line) != Some("mcpServers") {
                bail!("mcpServers must be a block mapping, not an inline value");
            }
            let insertion = offset + line.len();
            let mut out = String::with_capacity(yaml.len() + entry_block.len());
            out.push_str(&yaml[..insertion]);
            out.push_str(&entry_block);
            out.push_str(&yaml[insertion..]);
            return Ok(out);
        }
        offset += line.len();
    }
    bail!("mcpServers: not found")
}

fn indent_of(line: &str) -> usize {
    line.chars().take_while(|c| *c == ' ').count()
}

fn is_indented_key(line: &str, key: &str, indent: usize) -> bool {
    let trimmed_end = line.trim_end_matches(['\n', '\r']);
    if indent_of(trimmed_end) != indent {
        return false;
    }
    let after_indent = &trimmed_end[indent..];
    // Must start with `<key>:` optionally followed by whitespace / comment.
    if !after_indent.starts_with(key) {
        return false;
    }
    let tail = &after_indent[key.len()..];
    tail.starts_with(':')
}

fn mcp_child_indent_from_line(line: &str) -> Option<usize> {
    if line.trim().is_empty() || line.trim_start().starts_with('#') {
        return None;
    }
    let indent = indent_of(line);
    (indent > 0).then_some(indent)
}

fn mcp_child_indent(yaml: &str) -> Option<usize> {
    let mut in_mcp_servers = false;
    for line in yaml.lines() {
        if let Some(key) = top_level_key(line) {
            in_mcp_servers = key == "mcpServers" && top_level_block_key(line) == Some("mcpServers");
            continue;
        }
        if in_mcp_servers && let Some(indent) = mcp_child_indent_from_line(line) {
            return Some(indent);
        }
    }
    None
}

/// The key of a top-level (indent-0) YAML mapping entry — e.g. `mcpServers` for
/// a `mcpServers:` line. Returns `None` for indented lines, blanks, comments,
/// and list items, i.e. anything that is NOT a section boundary. Used to scope
/// every `difflore:` operation to the children of the top-level `mcpServers:`
/// section, so an unrelated `difflore:` key nested under another section is
/// never detected, replaced, or removed.
fn top_level_key(line: &str) -> Option<&str> {
    top_level_key_and_tail(line).map(|(key, _)| key)
}

fn top_level_block_key(line: &str) -> Option<&str> {
    let (key, tail) = top_level_key_and_tail(line)?;
    let tail_without_comment = tail.split('#').next().unwrap_or("").trim();
    tail_without_comment.is_empty().then_some(key)
}

fn top_level_key_and_tail(line: &str) -> Option<(&str, &str)> {
    if indent_of(line) != 0 {
        return None;
    }
    let content = line.trim_end_matches(['\n', '\r']).trim_start();
    if content.is_empty() || content.starts_with('#') || content.starts_with('-') {
        return None;
    }
    let (key, tail) = content.split_once(':')?;
    let key = key.trim_end();
    (!key.is_empty()).then_some((key, tail))
}

fn has_top_level_mcp_servers(yaml: &str) -> bool {
    yaml.lines()
        .any(|line| top_level_key(line) == Some("mcpServers"))
}

pub(super) fn probe_goose_install(
    name: &'static str,
    path: &PathBuf,
    expected_command: &str,
) -> TargetStatus {
    if !path.exists() {
        return TargetStatus {
            name,
            detected: false,
            state: InstallState::NotInstalled,
            detail: Some(format!("{} not found", path.display())),
        };
    }
    let text = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            return TargetStatus {
                name,
                detected: true,
                state: InstallState::Unknown,
                detail: Some(format!("failed to read {}: {e}", path.display())),
            };
        }
    };
    if !yaml_has_difflore_under_mcp_servers(&text) {
        return TargetStatus {
            name,
            detected: true,
            state: InstallState::NotInstalled,
            detail: Some(format!("{} has no difflore block", path.display())),
        };
    }
    let difflore_block = difflore_block_lines(&text);
    let expected_command_line = format!("command: {}", yaml_escape_scalar(expected_command));
    let command_ok = difflore_block
        .iter()
        .any(|line| line.trim_start() == expected_command_line);
    let expected_arg_line = format!("- {MCP_SERVER_ARG}");
    let args_ok = difflore_block
        .iter()
        .any(|line| line.trim_start() == expected_arg_line);
    if command_ok && args_ok {
        return TargetStatus {
            name,
            detected: true,
            state: InstallState::Installed,
            detail: Some(path.display().to_string()),
        };
    }
    TargetStatus {
        name,
        detected: true,
        state: InstallState::Conflict,
        detail: Some(format!(
            "{}: difflore block exists but command/args drifted",
            path.display()
        )),
    }
}

// ── Rendered-block helpers ────────────────────────────────────────────────

/// The exact `  difflore:` YAML block DiffLore writes under `mcpServers:`,
/// byte-identical to the `entry_block` in [`merge_goose_yaml_config`]. This is
/// the string the install manifest hashes for a Goose target.
pub(super) fn render_goose_block(bin: &str) -> String {
    render_goose_block_at_indent(bin, 2)
}

fn render_goose_block_at_indent(bin: &str, indent: usize) -> String {
    let child = " ".repeat(indent);
    let nested = " ".repeat(indent + 2);
    let arg = " ".repeat(indent + 4);
    format!(
        "{child}difflore:\n{nested}command: {bin}\n{nested}args:\n{arg}- mcp-server\n",
        bin = yaml_escape_scalar(bin),
    )
}

/// Re-extract the on-disk `  difflore:` block (header + deeper-indented
/// children) for re-hashing, normalised to match [`render_goose_block`]: each
/// line trimmed of trailing CR/LF and re-joined with `\n`. The normalisation
/// keeps the hash stable across CRLF/LF line endings (e.g. a Windows-edited
/// config). Returns `None` when the file is missing/unreadable or has no block.
pub(super) fn extract_goose_block(path: &PathBuf) -> Option<String> {
    if !path.exists() {
        return None;
    }
    let text = fs::read_to_string(path).ok()?;
    if !yaml_has_difflore_under_mcp_servers(&text) {
        return None;
    }
    let lines = difflore_block_lines(&text);
    if lines.is_empty() {
        return None;
    }
    let mut out = String::new();
    for line in lines {
        out.push_str(line.trim_end_matches(['\n', '\r']));
        out.push('\n');
    }
    Some(out)
}

fn difflore_block_lines(yaml: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut in_block = false;
    let mut in_mcp_servers = false;
    let mut child_indent: Option<usize> = None;
    for line in yaml.lines() {
        if !in_block {
            if let Some(key) = top_level_key(line) {
                in_mcp_servers =
                    key == "mcpServers" && top_level_block_key(line) == Some("mcpServers");
                child_indent = None;
                continue;
            }
            if in_mcp_servers && child_indent.is_none() {
                child_indent = mcp_child_indent_from_line(line);
            }
            if in_mcp_servers
                && child_indent.is_some_and(|indent| is_indented_key(line, "difflore", indent))
            {
                in_block = true;
                lines.push(line);
            }
            continue;
        }
        if indent_of(line) <= child_indent.unwrap_or(2) && !line.trim().is_empty() {
            break;
        }
        lines.push(line);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const BIN: &str = "/tmp/fake/difflore";

    #[test]
    fn goose_install_handles_fresh_existing_block_and_missing_block() {
        // (initial yaml, "must contain after install" assertions)
        let cases: &[(Option<&str>, &[&str])] = &[
            // Missing file → fresh mcpServers block.
            (None, &["mcpServers:", "difflore:", "mcp-server"]),
            // mcpServers: already present with another entry → append, preserve.
            (
                Some("# prelude\nmcpServers:\n  other:\n    command: x\n    args:\n      - y\n"),
                &["other:", "difflore:"],
            ),
            // No mcpServers: section at all → append the whole block.
            (
                Some("gpt:\n  model: whatever\n"),
                &["gpt:", "mcpServers:", "difflore:"],
            ),
        ];
        for (initial, expected) in cases {
            let tmp = TempDir::new().unwrap();
            let path = tmp.path().join("config.yaml");
            if let Some(seed) = initial {
                fs::write(&path, seed).unwrap();
            }
            let existed = merge_goose_yaml_config(&path, BIN, false).unwrap();
            assert!(
                !existed,
                "fresh install should not report existed for {initial:?}"
            );
            let text = fs::read_to_string(&path).unwrap();
            for needle in *expected {
                assert!(
                    text.contains(needle),
                    "missing {needle:?} for case {initial:?}"
                );
            }
        }
    }

    #[test]
    fn goose_replaces_existing_difflore_block_on_reinstall() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.yaml");
        fs::write(
            &path,
            "mcpServers:\n  difflore:\n    command: /old/path\n    args:\n      - mcp-server\n  other:\n    command: x\n",
        )
        .unwrap();
        let existed = merge_goose_yaml_config(&path, BIN, false).unwrap();
        assert!(existed, "difflore was already there");
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("/old/path"), "old command must be gone");
        assert!(
            text.contains(BIN) || text.contains(&yaml_escape_scalar(BIN)),
            "new command must be present"
        );
        assert!(text.contains("other:"), "unrelated server must survive");
    }

    #[test]
    fn goose_preserves_existing_mcp_servers_child_indent() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.yaml");
        fs::write(
            &path,
            "mcpServers:\n    difflore:\n      command: /old/path\n      args:\n        - mcp-server\n    other:\n      command: x\n",
        )
        .unwrap();

        let existed = merge_goose_yaml_config(&path, BIN, false).unwrap();
        assert!(existed, "difflore was already there");

        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(
            text.matches("    difflore:").count(),
            1,
            "reinstall should replace, not duplicate: {text}"
        );
        assert!(
            !text.lines().any(|line| line == "  difflore:"),
            "replacement should use existing child indent: {text}"
        );
        assert!(!text.contains("/old/path"), "old command must be gone");
        assert!(matches!(
            probe_goose_install("Goose", &path, BIN).state,
            InstallState::Installed
        ));

        let extracted = extract_goose_block(&path).unwrap();
        assert!(
            extracted.starts_with("    difflore:\n      command:"),
            "extract should preserve the configured indent: {extracted}"
        );

        let removed = remove_goose_yaml_config(&path, false).unwrap();
        assert!(removed);
        let after = fs::read_to_string(&path).unwrap();
        assert!(
            !after.contains("difflore:"),
            "uninstall should remove the indented block: {after}"
        );
        assert!(after.contains("    other:"), "other server must survive");
    }

    #[test]
    fn goose_rejects_inline_mcp_servers_instead_of_corrupting_yaml() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.yaml");
        let initial = "mcpServers: {}\nother: ok\n";
        fs::write(&path, initial).unwrap();

        let err = merge_goose_yaml_config(&path, BIN, false).unwrap_err();

        assert!(
            err.contains("mcpServers must be a block mapping"),
            "unexpected error: {err}"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), initial);
    }

    #[test]
    fn goose_scopes_difflore_ops_to_mcp_servers_children() {
        // An unrelated top-level section ALSO has a `difflore:` key. Detect /
        // replace / remove must touch ONLY the one under `mcpServers:`.
        let unrelated = "extensions:\n  difflore:\n    note: not ours\n";
        let real =
            "mcpServers:\n  difflore:\n    command: /old/path\n    args:\n      - mcp-server\n";

        // (a) Only an unrelated `difflore:` → not detected as installed.
        assert!(!yaml_has_difflore_under_mcp_servers(unrelated));

        // (b) Both present → detected; reinstall replaces ONLY the mcpServers one
        // and preserves the unrelated block.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.yaml");
        fs::write(&path, format!("{unrelated}{real}")).unwrap();
        assert!(yaml_has_difflore_under_mcp_servers(
            &fs::read_to_string(&path).unwrap()
        ));
        let existed = merge_goose_yaml_config(&path, BIN, false).unwrap();
        assert!(existed, "the mcpServers difflore block should be detected");
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("/old/path"), "mcpServers command replaced");
        assert!(
            text.contains("note: not ours"),
            "the unrelated difflore block must survive replace"
        );

        // (c) Uninstall removes ONLY the mcpServers difflore block.
        let removed = remove_goose_yaml_config(&path, false).unwrap();
        assert!(removed);
        let after = fs::read_to_string(&path).unwrap();
        assert!(
            after.contains("note: not ours"),
            "the unrelated difflore block must survive uninstall"
        );
        assert!(after.contains("extensions:"), "unrelated section preserved");
        assert!(
            !after.contains("mcp-server"),
            "the mcpServers difflore block must be gone"
        );
    }

    #[test]
    fn goose_probe_requires_command_and_mcp_server_arg() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.yaml");
        fs::write(
            &path,
            "mcpServers:\n  difflore:\n    command: /tmp/fake/difflore\n    args: []\n",
        )
        .unwrap();

        let status = probe_goose_install("Goose", &path, BIN);
        assert_eq!(status.state, InstallState::Conflict);
        assert!(
            status
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("command/args drifted"))
        );

        fs::write(
            &path,
            "mcpServers:\n  difflore:\n    command: /tmp/fake/difflore\n    args:\n      - mcp-server\n",
        )
        .unwrap();
        let status = probe_goose_install("Goose", &path, BIN);
        assert_eq!(status.state, InstallState::Installed);
    }

    #[test]
    fn uninstall_removes_difflore_block_and_preserves_other_servers() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.yaml");
        // Seed with another server, then install difflore, then uninstall.
        fs::write(
            &path,
            "# prelude\nmcpServers:\n  other:\n    command: x\n    args:\n      - y\n",
        )
        .unwrap();
        merge_goose_yaml_config(&path, BIN, false).unwrap();
        let removed = remove_goose_yaml_config(&path, false).unwrap();
        assert!(removed, "uninstall must report removing the difflore block");

        let text = fs::read_to_string(&path).unwrap();
        assert!(
            !text.contains("difflore:"),
            "difflore block must be gone: {text}"
        );
        assert!(
            text.contains("other:"),
            "unrelated server clobbered: {text}"
        );
        assert!(
            text.contains("mcpServers:"),
            "section header still needed: {text}"
        );
        assert!(text.contains("# prelude"), "prelude lost: {text}");
    }

    #[test]
    fn uninstall_drops_empty_mcp_servers_header_on_round_trip() {
        // A config whose only mcpServers child was difflore should not be left
        // with an orphaned `mcpServers:` header.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.yaml");
        merge_goose_yaml_config(&path, BIN, false).unwrap(); // fresh install
        let removed = remove_goose_yaml_config(&path, false).unwrap();
        assert!(removed);
        let text = fs::read_to_string(&path).unwrap();
        assert!(
            !text.contains("difflore:"),
            "difflore must be gone: {text:?}"
        );
        assert!(
            !text.contains("mcpServers:"),
            "empty mcpServers header should be dropped: {text:?}"
        );
    }

    #[test]
    fn uninstall_goose_is_noop_when_no_block_or_missing_file() {
        let tmp = TempDir::new().unwrap();
        // Missing file.
        let absent = tmp.path().join("absent.yaml");
        assert!(!remove_goose_yaml_config(&absent, false).unwrap());
        assert!(!absent.exists());

        // File without a difflore block.
        let path = tmp.path().join("config.yaml");
        fs::write(&path, "gpt:\n  model: whatever\n").unwrap();
        assert!(!remove_goose_yaml_config(&path, false).unwrap());
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "gpt:\n  model: whatever\n"
        );
    }

    #[test]
    fn uninstall_goose_dry_run_does_not_write() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.yaml");
        merge_goose_yaml_config(&path, BIN, false).unwrap();
        let before = fs::read_to_string(&path).unwrap();
        let removed = remove_goose_yaml_config(&path, true).unwrap();
        assert!(removed, "dry-run reports it would remove");
        assert_eq!(fs::read_to_string(&path).unwrap(), before, "dry-run wrote");
    }

    #[test]
    fn yaml_escape_quotes_windows_paths() {
        // Windows drive colon must be quoted, else YAML parses `C` as a key.
        let q = yaml_escape_scalar(r"C:\Users\foo\difflore.exe");
        assert!(q.starts_with('\''));
        assert!(q.ends_with('\''));
        // Plain paths pass through.
        assert_eq!(yaml_escape_scalar("/usr/bin/difflore"), "/usr/bin/difflore");
    }
}
