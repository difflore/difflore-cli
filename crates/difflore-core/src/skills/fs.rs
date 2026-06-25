use std::path::PathBuf;

use crate::infra::paths;

pub fn skills_base_dir() -> crate::Result<PathBuf> {
    Ok(paths::data_home()?.join("skills"))
}

pub fn ensure_skill_dirs() -> crate::Result<()> {
    let base = skills_base_dir()?;
    for source in &["github", "local", "cloud", "team"] {
        std::fs::create_dir_all(base.join(source)).map_err(|e| {
            crate::CoreError::internal(format!("failed to create skill directory: {e}"))
        })?;
    }
    Ok(())
}

pub fn get_engine_skills_dir(engine: &str) -> Option<PathBuf> {
    // When DIFFLORE_HOME is set (see db.rs), point engine dirs into that
    // sandbox so integration tests don't create real ~/.claude/skills
    // symlinks in the user's dotfiles.
    let home = if let Some(custom) = crate::infra::env::difflore_home() {
        PathBuf::from(custom)
    } else {
        dirs::home_dir()?
    };
    match engine {
        "codex" => Some(home.join(".codex").join("skills")),
        "claude" => Some(home.join(".claude").join("skills")),
        "gemini" => Some(home.join(".gemini").join("skills")),
        "cursor" => Some(home.join(".cursor").join("skills")),
        _ => None,
    }
}

pub fn skill_type_allows_engine_link(skill_type: &str) -> bool {
    skill_type.trim().eq_ignore_ascii_case("skill")
}

/// A skill `source`/`directory` must be a single, ordinary path component.
/// These values are joined into both `~/.difflore/skills/...` and the engine
/// skills dirs (`~/.claude/skills/...`); a value containing `..`, a path
/// separator, or an absolute/prefix component would let the join escape the
/// skills root and plant (or delete) a symlink/directory elsewhere. Cloud/team
/// sync already slugifies via `cloud_rule_directory_name`, so this is
/// defense-in-depth for any other caller (e.g. GitHub skill import).
pub(crate) fn is_safe_skill_component(value: &str) -> bool {
    use std::path::{Component, Path};
    if value.is_empty() || value.contains(['/', '\\']) {
        return false;
    }
    let mut comps = Path::new(value).components();
    matches!(
        (comps.next(), comps.next()),
        (Some(Component::Normal(_)), None)
    )
}

/// Lower-case, path-traversal-safe slug derived from a human-supplied name.
///
/// Folds every character that isn't ASCII alphanumeric or `_` to `-`, then
/// collapses runs of `-` and trims leading/trailing ones. The result contains
/// only `[a-z0-9_-]`, so it can never introduce a path separator, `..`, or an
/// absolute/prefix component when joined under the skills root. Returns `None`
/// when the input slugs to the empty string, leaving the caller to raise the
/// error variant appropriate to its surface (creation vs. capture).
pub(crate) fn safe_slug(name: &str) -> Option<String> {
    let slug: String = name
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    (!slug.is_empty()).then_some(slug)
}

pub fn sync_engine_link(
    source: &str,
    directory: &str,
    engine: &str,
    enabled: bool,
) -> std::io::Result<()> {
    if !is_safe_skill_component(source) || !is_safe_skill_component(directory) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "refusing to sync skill link for unsafe path component (source={source:?}, directory={directory:?})"
            ),
        ));
    }
    let skill_dir = skills_base_dir()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::NotFound, e))?
        .join(source)
        .join(directory);
    let Some(engine_dir) = get_engine_skills_dir(engine) else {
        return Ok(());
    };
    let _ = std::fs::create_dir_all(&engine_dir);
    let link_path = engine_dir.join(directory);

    if enabled {
        if !skill_dir.exists() {
            return Ok(());
        }
        match link_entry_kind(&link_path)? {
            Some(LinkEntryKind::ManagedLink) => return Ok(()),
            Some(LinkEntryKind::Other) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    format!(
                        "cannot enable skill link because a non-symlink entry exists at {}",
                        link_path.display()
                    ),
                ));
            }
            None => {}
        }
        create_skill_link(&skill_dir, &link_path).or_else(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists
                && matches!(
                    link_entry_kind(&link_path)?,
                    Some(LinkEntryKind::ManagedLink)
                )
            {
                Ok(())
            } else {
                Err(e)
            }
        })?;
    } else {
        match link_entry_kind(&link_path)? {
            Some(LinkEntryKind::ManagedLink) => remove_link_entry(&link_path)?,
            Some(LinkEntryKind::Other) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    format!(
                        "cannot disable skill link because a non-symlink entry exists at {}",
                        link_path.display()
                    ),
                ));
            }
            None => {}
        }
    }
    Ok(())
}

pub fn purge_review_standard_engine_links(engine: &str, dry_run: bool) -> std::io::Result<usize> {
    let Some(engine_dir) = get_engine_skills_dir(engine) else {
        return Ok(0);
    };
    if !engine_dir.exists() {
        return Ok(0);
    }

    let mut removed = 0;
    for entry in std::fs::read_dir(engine_dir)? {
        let path = entry?.path();
        if !matches!(link_entry_kind(&path)?, Some(LinkEntryKind::ManagedLink)) {
            continue;
        }
        if !link_points_to_review_standard(&path) {
            continue;
        }
        if !dry_run {
            remove_link_entry(&path)?;
        }
        removed += 1;
    }
    Ok(removed)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinkEntryKind {
    ManagedLink,
    Other,
}

fn link_entry_kind(path: &std::path::Path) -> std::io::Result<Option<LinkEntryKind>> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            if is_link_like(&meta) {
                Ok(Some(LinkEntryKind::ManagedLink))
            } else {
                Ok(Some(LinkEntryKind::Other))
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn is_link_like(meta: &std::fs::Metadata) -> bool {
    if meta.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn create_skill_link(
    skill_dir: &std::path::Path,
    link_path: &std::path::Path,
) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(skill_dir, link_path)
    }
    #[cfg(windows)]
    {
        if skill_dir.is_dir() {
            std::os::windows::fs::symlink_dir(skill_dir, link_path)
        } else {
            std::os::windows::fs::symlink_file(skill_dir, link_path)
        }
    }
}

fn remove_link_entry(path: &std::path::Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(file_err) => match std::fs::remove_dir(path) {
            Ok(()) => Ok(()),
            Err(_) => Err(file_err),
        },
    }
}

fn link_points_to_review_standard(path: &std::path::Path) -> bool {
    let Ok(markdown) = std::fs::read_to_string(path.join("SKILL.md")) else {
        return false;
    };
    let mut lines = markdown.lines();
    if lines.next().map(str::trim) != Some("---") {
        return false;
    }
    for line in lines {
        let line = line.trim();
        if line == "---" {
            break;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case("type")
            && value.trim().eq_ignore_ascii_case("review_standard")
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod component_safety_tests {
    use super::is_safe_skill_component;

    #[test]
    fn accepts_ordinary_components() {
        for ok in [
            "local",
            "cloud",
            "rule-abc123",
            "conv_review_aabbccdd",
            "a.b",
        ] {
            assert!(is_safe_skill_component(ok), "{ok:?} should be accepted");
        }
    }

    #[test]
    fn rejects_traversal_separators_and_absolute() {
        for bad in [
            "",
            "..",
            ".",
            "a/b",
            "a\\b",
            "../escape",
            "/abs",
            "../../etc",
            "x/..",
            "C:\\windows",
        ] {
            assert!(!is_safe_skill_component(bad), "{bad:?} must be rejected");
        }
    }
}
