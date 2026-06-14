use std::path::PathBuf;

/// Root for all local data and config files: `~/.difflore`, overridden by
/// `$DIFFLORE_HOME` in production or the shared test home in tests.
pub fn data_home() -> crate::Result<PathBuf> {
    crate::infra::db::difflore_dir()
}

/// Currently equals `data_home()`. Separate so a future split (e.g. honoring
/// XDG `$XDG_CONFIG_HOME`) won't break callers.
pub fn config_home() -> crate::Result<PathBuf> {
    data_home()
}

pub fn config_file() -> crate::Result<PathBuf> {
    Ok(config_home()?.join("config.toml"))
}

/// Project root: `git rev-parse --show-toplevel`, falling back to cwd.
pub use crate::infra::db::current_project_root;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_home_resolves_to_existing_dir_under_test_home() {
        // data_home() must resolve to the per-process test tempdir (from
        // db.rs's shared_test_home() singleton), never the user's real
        // ~/.difflore.
        let home = data_home().expect("data_home should resolve in tests");
        assert!(home.is_absolute(), "expected absolute path, got {home:?}");
    }

    #[test]
    fn config_home_equals_data_home() {
        assert_eq!(config_home().unwrap(), data_home().unwrap());
    }

    #[test]
    fn config_file_is_config_toml_under_config_home() {
        let cfg = config_file().unwrap();
        assert_eq!(cfg.file_name().unwrap(), "config.toml");
        assert!(cfg.starts_with(config_home().unwrap()));
    }

    #[test]
    fn current_project_root_returns_path() {
        // Smoke test — must not panic even outside a git checkout.
        let p = current_project_root();
        assert!(!p.as_os_str().is_empty());
    }
}
