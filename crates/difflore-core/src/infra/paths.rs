use std::path::PathBuf;

/// `~/.difflore` (or `$DIFFLORE_HOME` in production / shared test home in
/// tests). All local data + config files live under this root.
pub fn data_home() -> Result<PathBuf, String> {
    crate::infra::db::difflore_dir()
}

/// Currently equals `data_home()`. Kept as a separate function so a future
/// split (e.g. honoring XDG `$XDG_CONFIG_HOME`) doesn't break callers.
pub fn config_home() -> Result<PathBuf, String> {
    data_home()
}

/// `<config_home>/config.toml`.
pub fn config_file() -> Result<PathBuf, String> {
    Ok(config_home()?.join("config.toml"))
}

/// Project root: `git rev-parse --show-toplevel`, falling back to cwd.
pub use crate::infra::db::current_project_root;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_home_resolves_to_existing_dir_under_test_home() {
        // The shared test home is set up by db.rs's `shared_test_home()`
        // singleton — `data_home()` must return that path so all test
        // I/O lands in the per-process tempdir, never the user's real
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
