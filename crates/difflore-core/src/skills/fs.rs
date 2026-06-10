use std::path::PathBuf;

use crate::paths;

pub fn skills_base_dir() -> Result<PathBuf, String> {
    Ok(paths::data_home()?.join("skills"))
}

pub fn ensure_skill_dirs() -> Result<(), String> {
    let base = skills_base_dir()?;
    for source in &["github", "local", "cloud", "team"] {
        std::fs::create_dir_all(base.join(source))
            .map_err(|e| format!("failed to create skill directory: {e}"))?;
    }
    Ok(())
}

pub fn get_engine_skills_dir(engine: &str) -> Option<PathBuf> {
    // When DIFFLORE_HOME is set (see db.rs), point engine dirs into that
    // sandbox so integration tests don't create real ~/.claude/skills
    // symlinks in the user's dotfiles.
    let home = if let Some(custom) = crate::env::difflore_home() {
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

pub fn sync_engine_link(
    source: &str,
    directory: &str,
    engine: &str,
    enabled: bool,
) -> std::io::Result<()> {
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
