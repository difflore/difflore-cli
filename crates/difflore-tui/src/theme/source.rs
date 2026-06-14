//! Theme source: `~/.difflore/config.toml` IO with an mtime-debounced cache.
//!
//! The draw path asks for the active theme dozens of times per frame, so the
//! config file is re-read only when the cache TTL lapses *and* the file's
//! signature (path, mtime, size) actually changed.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use super::Theme;

const THEME_CACHE_TTL: Duration = Duration::from_millis(250);

static THEME_CACHE: OnceLock<Mutex<ThemeCache>> = OnceLock::new();

#[derive(Clone, Debug, PartialEq, Eq)]
struct ConfigSignature {
    path: Option<PathBuf>,
    modified: Option<SystemTime>,
    len: Option<u64>,
}

impl ConfigSignature {
    fn read() -> Self {
        let path = difflore_core::infra::paths::config_file().ok();
        let metadata = path.as_ref().and_then(|p| std::fs::metadata(p).ok());
        Self {
            path,
            modified: metadata.as_ref().and_then(|m| m.modified().ok()),
            len: metadata.map(|m| m.len()),
        }
    }
}

#[derive(Clone, Debug)]
struct ThemeCache {
    checked_at: Instant,
    signature: ConfigSignature,
    theme: Theme,
}

impl ThemeCache {
    fn fresh(now: Instant, signature: ConfigSignature) -> Self {
        Self {
            checked_at: now,
            signature,
            theme: load_current_theme(),
        }
    }

    fn is_fresh(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.checked_at) < THEME_CACHE_TTL
    }
}

pub(super) fn cached_current_theme() -> Theme {
    let now = Instant::now();
    let cache =
        THEME_CACHE.get_or_init(|| Mutex::new(ThemeCache::fresh(now, ConfigSignature::read())));
    let Ok(mut cache) = cache.lock() else {
        return load_current_theme();
    };

    if cache.is_fresh(now) {
        return cache.theme;
    }

    let signature = ConfigSignature::read();
    if signature == cache.signature {
        cache.checked_at = now;
        return cache.theme;
    }

    *cache = ThemeCache::fresh(now, signature);
    cache.theme
}

fn load_current_theme() -> Theme {
    match difflore_core::infra::config::load().theme {
        difflore_core::infra::config::ThemeMode::Light => Theme::LIGHT,
        difflore_core::infra::config::ThemeMode::Dark => Theme::DARK,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_cache_is_fresh_inside_ttl() {
        let now = Instant::now();
        let cache = ThemeCache::fresh(
            now,
            ConfigSignature {
                path: None,
                modified: None,
                len: None,
            },
        );

        assert!(cache.is_fresh(now + Duration::from_millis(1)));
        assert!(!cache.is_fresh(now + THEME_CACHE_TTL + Duration::from_millis(1)));
    }
}
