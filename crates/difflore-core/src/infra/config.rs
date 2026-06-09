use std::path::Path;

use crate::infra::paths;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ThemeMode {
    #[default]
    Dark,
    Light,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiffloreConfig {
    pub theme: ThemeMode,
}

/// Read `<config_home>/config.toml`. Missing or unreadable files yield
/// `DiffloreConfig::default()`. We deliberately avoid pulling in serde
/// + toml just for one key; `parse_kv_pairs` understands the small
///   `key = "value"` subset we ship.
pub fn load() -> DiffloreConfig {
    let Ok(path) = paths::config_file() else {
        return DiffloreConfig::default();
    };
    load_from_path(&path)
}

/// Load a config from an explicit path. Returns default on any I/O or
/// parse failure (including missing file). Exposed for tests that need
/// to point at a specific tempdir without racing on the shared
/// `DIFFLORE_HOME`.
pub fn load_from_path(path: &Path) -> DiffloreConfig {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return DiffloreConfig::default();
    };
    let mut cfg = DiffloreConfig::default();
    for (key, value) in parse_kv_pairs(&raw) {
        if key == "theme" {
            cfg.theme = match value.as_str() {
                "light" => ThemeMode::Light,
                "dark" => ThemeMode::Dark,
                _ => ThemeMode::default(),
            };
        }
    }
    cfg
}

fn parse_kv_pairs(src: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in src.lines() {
        let line = line.trim_start();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(eq) = line.find('=') else {
            continue;
        };
        let key = line[..eq].trim().to_owned();
        let rest = line[eq + 1..].trim_start();
        let Some(rest) = rest.strip_prefix('"') else {
            continue;
        };
        let Some(end) = rest.find('"') else {
            continue;
        };
        out.push((key, rest[..end].to_owned()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_cfg(contents: &str) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("config.toml"), contents).unwrap();
        tmp
    }

    #[test]
    fn load_from_path_returns_default_when_file_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = load_from_path(&tmp.path().join("does-not-exist.toml"));
        assert_eq!(cfg, DiffloreConfig::default());
        assert_eq!(cfg.theme, ThemeMode::Dark);
    }

    #[test]
    fn load_from_path_parses_theme_light() {
        let tmp = write_cfg(r#"theme = "light""#);
        assert_eq!(
            load_from_path(&tmp.path().join("config.toml")).theme,
            ThemeMode::Light
        );
    }

    #[test]
    fn load_from_path_parses_theme_dark() {
        let tmp = write_cfg(r#"theme = "dark""#);
        assert_eq!(
            load_from_path(&tmp.path().join("config.toml")).theme,
            ThemeMode::Dark
        );
    }

    #[test]
    fn load_from_path_malformed_theme_falls_back_to_default() {
        // No quotes around `bogus` -> parser skips the line entirely.
        let tmp = write_cfg("theme = bogus\n");
        assert_eq!(
            load_from_path(&tmp.path().join("config.toml")).theme,
            ThemeMode::Dark
        );
    }

    #[test]
    fn load_from_path_tolerates_comments_and_extra_keys() {
        let tmp = write_cfg("# leading comment\ntheme = \"light\"\nfoo = \"bar\"\n");
        assert_eq!(
            load_from_path(&tmp.path().join("config.toml")).theme,
            ThemeMode::Light
        );
    }

    #[test]
    fn load_returns_default_when_file_missing_in_data_home() {
        // The shared test home doesn't include a config.toml unless
        // some other test has written one. Treat missing as default
        // — that's the production guarantee we care about.
        // (We don't write to the shared home here to avoid racing
        //  with parallel tests.)
        let _ = load(); // must not panic
    }

    #[test]
    fn unrecognised_theme_value_falls_back_to_default() {
        let tmp = write_cfg(r#"theme = "neon""#);
        assert_eq!(
            load_from_path(&tmp.path().join("config.toml")).theme,
            ThemeMode::Dark
        );
    }
}
