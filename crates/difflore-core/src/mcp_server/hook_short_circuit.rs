//! In-process empty-rate cache for post-edit rule recall.
//!
//! Empty-prone post-edit extensions can skip retrieval once their recent
//! empty rate is high enough, reducing latency and upload backlog.
//!
//! It keeps a small per-extension ring buffer. Short-circuited calls are
//! not recorded, so a fresh process or improved corpus can recover naturally.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// Trailing window size. Each extension keeps the last `WINDOW` outcomes.
/// Large enough to resist one-off outliers, small enough to recover quickly.
pub(crate) const WINDOW: usize = 50;

/// Minimum number of observations in the window before the heuristic can
/// fire. Avoids short-circuiting on a cold cache where 1/1 empties would
/// look like 100%.
pub(crate) const MIN_OBSERVATIONS: usize = 10;

/// Empty-rate threshold (inclusive) above which we short-circuit.
pub(crate) const EMPTY_RATE_THRESHOLD: f64 = 0.9;

/// Ring buffer of the last `WINDOW` boolean outcomes (true = was_empty)
/// for a single file extension. Stores the count of empties separately
/// so the verdict check is O(1).
#[derive(Debug)]
struct WindowStats {
    /// Filled positions; saturates at `WINDOW`.
    len: usize,
    /// Write head; wraps mod `WINDOW`.
    cursor: usize,
    /// Boolean ring (true = served empty). The std lib does not
    /// implement `Default` for `[T; N]` past N=32, so we hand-roll a
    /// `Default` impl below rather than `#[derive]`.
    slots: [bool; WINDOW],
    /// Count of `true` entries currently in `slots[..len]`. Maintained
    /// in lockstep with `record` so `empty_rate` is a single division.
    empties: usize,
}

impl Default for WindowStats {
    fn default() -> Self {
        Self {
            len: 0,
            cursor: 0,
            slots: [false; WINDOW],
            empties: 0,
        }
    }
}

impl WindowStats {
    const fn record(&mut self, was_empty: bool) {
        if self.len == WINDOW {
            // Evict the oldest entry (the one we're about to overwrite).
            if self.slots[self.cursor] {
                self.empties = self.empties.saturating_sub(1);
            }
        } else {
            self.len += 1;
        }
        self.slots[self.cursor] = was_empty;
        if was_empty {
            self.empties += 1;
        }
        self.cursor = (self.cursor + 1) % WINDOW;
    }

    fn should_short_circuit(&self) -> bool {
        if self.len < MIN_OBSERVATIONS {
            return false;
        }
        // len > 0 once MIN_OBSERVATIONS is satisfied; safe to divide.
        let rate = self.empties as f64 / self.len as f64;
        rate >= EMPTY_RATE_THRESHOLD
    }
}

/// In-process cache. One entry per lowercase extension (e.g. `.ts`).
/// Files without an extension share the sentinel key `""`.
#[derive(Debug, Default)]
pub(crate) struct ShortCircuitCache {
    by_ext: Mutex<HashMap<String, WindowStats>>,
}

impl ShortCircuitCache {
    /// Read the current verdict for `ext`. Returns `true` when the
    /// caller should skip retrieval and synthesise an empty response.
    /// Does NOT update the cache — short-circuited paths intentionally
    /// leave the window unchanged so the extension can recover.
    pub(crate) fn should_short_circuit(&self, ext: &str) -> bool {
        let guard = self
            .by_ext
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard
            .get(ext)
            .is_some_and(WindowStats::should_short_circuit)
    }

    /// Record the outcome of a real (non-short-circuited) recall call.
    pub(crate) fn record(&self, ext: &str, was_empty: bool) {
        let mut guard = self
            .by_ext
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.entry(ext.to_owned()).or_default().record(was_empty);
    }

    /// Test-only: snapshot (`empties`, `len`) for an extension.
    #[cfg(test)]
    pub(crate) fn snapshot(&self, ext: &str) -> (usize, usize) {
        let guard = self
            .by_ext
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.get(ext).map_or((0, 0), |w| (w.empties, w.len))
    }
}

/// Process-wide cache handle.
///
/// The `fetch_relevant_rules_for_hook` entrypoint is a free function
/// invoked from both the stdio MCP server (which has an [`super::McpState`])
/// and the hook-runtime dispatch path (which does not). A per-process
/// `OnceLock` lets both surfaces share one cache without threading a
/// new handle through every caller.
pub(crate) fn global_cache() -> &'static ShortCircuitCache {
    static CACHE: OnceLock<ShortCircuitCache> = OnceLock::new();
    CACHE.get_or_init(ShortCircuitCache::default)
}

/// Normalise a path / file string into a lowercase extension key
/// (e.g. `"src/foo/bar.TS"` → `".ts"`). Returns `""` for files with
/// no extension so we still bucket them coherently.
#[must_use]
pub(crate) fn extension_key(file: &str) -> String {
    let trimmed = file.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    // Match on the basename only so a directory like `.git/HEAD` does
    // not produce a leading-dot "extension" of the dotdir.
    let basename = trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed);
    match basename.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() => {
            format!(".{}", ext.to_ascii_lowercase())
        }
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_key_normalises_case_and_skips_dotfiles() {
        assert_eq!(extension_key("foo.TS"), ".ts");
        assert_eq!(extension_key("a/b/c.Md"), ".md");
        assert_eq!(extension_key("weird/path\\file.JSON"), ".json");
        // Pure dotfile (no stem) → no extension bucket.
        assert_eq!(extension_key(".gitignore"), "");
        // No extension at all.
        assert_eq!(extension_key("Makefile"), "");
        // Empty / whitespace.
        assert_eq!(extension_key(""), "");
        assert_eq!(extension_key("   "), "");
    }

    #[test]
    fn cache_below_threshold_does_not_short_circuit_even_when_all_empty() {
        // Spec test #1: 9 calls all empty (one short of MIN_OBSERVATIONS)
        // must not trigger the heuristic.
        let cache = ShortCircuitCache::default();
        for _ in 0..9 {
            cache.record(".ts", true);
        }
        assert!(
            !cache.should_short_circuit(".ts"),
            "must wait for MIN_OBSERVATIONS before firing"
        );
        assert_eq!(cache.snapshot(".ts"), (9, 9));
    }

    #[test]
    fn cache_at_threshold_with_ninety_percent_empty_short_circuits() {
        // Spec test #2: 9 empties + 1 hit = exactly the threshold.
        let cache = ShortCircuitCache::default();
        for _ in 0..9 {
            cache.record(".ts", true);
        }
        cache.record(".ts", false);
        assert!(
            cache.should_short_circuit(".ts"),
            "10 observations at 90% empty must short-circuit"
        );
    }

    #[test]
    fn short_circuit_check_does_not_mutate_window() {
        // Spec test #3: short-circuiting must NOT inflate the empty count,
        // otherwise the extension can never recover after a corpus refresh.
        let cache = ShortCircuitCache::default();
        for _ in 0..9 {
            cache.record(".ts", true);
        }
        cache.record(".ts", false);
        let before = cache.snapshot(".ts");
        for _ in 0..5 {
            assert!(cache.should_short_circuit(".ts"));
        }
        let after = cache.snapshot(".ts");
        assert_eq!(
            before, after,
            "short-circuit verdicts must leave the trailing window unchanged"
        );
    }

    #[test]
    fn window_evicts_oldest_when_full() {
        // Once the ring fills, an old `true` slot getting overwritten by a
        // fresh `false` should decrement the empties counter.
        let cache = ShortCircuitCache::default();
        for _ in 0..WINDOW {
            cache.record(".ts", true);
        }
        assert_eq!(cache.snapshot(".ts"), (WINDOW, WINDOW));
        // Overwrite all WINDOW slots with non-empties.
        for _ in 0..WINDOW {
            cache.record(".ts", false);
        }
        assert_eq!(cache.snapshot(".ts"), (0, WINDOW));
        assert!(
            !cache.should_short_circuit(".ts"),
            "fresh window of all-non-empty must NOT short-circuit"
        );
    }

    #[test]
    fn off_mode_disables_short_circuit_even_when_heuristic_would_fire() {
        // Spec test #4: when the operator sets
        // `DIFFLORE_HOOK_SHORT_CIRCUIT=off`, the heuristic must NOT
        // suppress retrieval, even with a fully-saturated empty window.
        // We mirror the wiring in `fetch_relevant_rules_for_hook` so the
        // unit test does not require spinning up SQLite / the full hook.
        use crate::env::HookShortCircuitMode;

        let cache = ShortCircuitCache::default();
        for _ in 0..10 {
            cache.record(".ts", true);
        }
        assert!(
            cache.should_short_circuit(".ts"),
            "precondition: heuristic should be tripped"
        );

        // The hook call site combines the env mode with the cache
        // verdict. `Off` must veto a tripped heuristic.
        let mode = HookShortCircuitMode::Off;
        let would_short_circuit = match mode {
            HookShortCircuitMode::Off => false,
            HookShortCircuitMode::Force => true,
            HookShortCircuitMode::Auto => cache.should_short_circuit(".ts"),
        };
        assert!(
            !would_short_circuit,
            "DIFFLORE_HOOK_SHORT_CIRCUIT=off must override the heuristic"
        );

        // Sanity: Force mode forces short-circuit on a virgin cache,
        // and Auto agrees with the bare heuristic.
        let virgin = ShortCircuitCache::default();
        let force_decision = match HookShortCircuitMode::Force {
            HookShortCircuitMode::Off => false,
            HookShortCircuitMode::Force => true,
            HookShortCircuitMode::Auto => virgin.should_short_circuit(".ts"),
        };
        assert!(force_decision, "Force mode must always short-circuit");
        let auto_virgin = match HookShortCircuitMode::Auto {
            HookShortCircuitMode::Off => false,
            HookShortCircuitMode::Force => true,
            HookShortCircuitMode::Auto => virgin.should_short_circuit(".ts"),
        };
        assert!(!auto_virgin, "Auto mode must not fire on an empty cache");
    }

    #[test]
    fn different_extensions_are_tracked_independently() {
        let cache = ShortCircuitCache::default();
        for _ in 0..10 {
            cache.record(".md", true);
        }
        for _ in 0..10 {
            cache.record(".go", false);
        }
        assert!(cache.should_short_circuit(".md"));
        assert!(!cache.should_short_circuit(".go"));
        assert!(!cache.should_short_circuit(".unseen"));
    }
}
