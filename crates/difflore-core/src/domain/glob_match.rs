//! Shared file-pattern glob matcher.
//!
//! Two call sites share the "does this path satisfy a rule's JSON-encoded
//! `file_patterns` glob list?" logic but want **opposite** error handling,
//! expressed via [`GlobErrorPolicy`]:
//!
//! * Rule retrieval over-recalls: a corrupt blob must NOT silently drop a
//!   rule — surfacing a maybe-irrelevant rule beats losing real signal.
//! * Observation attribution drops: a corrupt blob can't prove the rule
//!   applies, so the safe call is to NOT credit it.

use globset::{Glob, GlobSetBuilder};

/// What to return when the pattern blob can't be turned into a usable
/// glob set (malformed JSON, no parseable globs, or `GlobSet::build`
/// failure). Absent / empty / `[]` patterns are *not* errors — those are
/// "universal rule" and always match regardless of policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlobErrorPolicy {
    /// Over-recall: treat an unusable pattern blob as "matches". Used by
    /// rule retrieval so a corrupt blob never costs us recall.
    OverRecall,
    /// Drop: treat an unusable pattern blob as "does not match". Used by
    /// observation attribution so we never credit a rule we can't prove
    /// applies.
    Drop,
}

impl GlobErrorPolicy {
    #[inline]
    const fn verdict(self) -> bool {
        match self {
            Self::OverRecall => true,
            Self::Drop => false,
        }
    }
}

/// Decide whether `path` is in scope for a rule whose `patterns_json` is
/// a JSON array of glob strings (e.g. `["src/**/*.rs", "**/*.toml"]`).
///
/// Returns `true` when:
/// * `patterns_json` is `None`, blank, or parses to an empty list
///   (universal rule — always in scope), or
/// * any glob in the list matches the normalised `path`.
///
/// On a recoverable failure (malformed JSON, zero parseable globs, or a
/// `GlobSet` build error) the result is governed by `on_error` so the
/// two call sites keep their intentional opposite behaviour.
///
/// `path` is normalised before matching: a leading `/` is stripped and
/// `\` is rewritten to `/` so Windows-style paths agree with
/// forward-slash globs.
pub fn glob_match(patterns_json: Option<&str>, path: &str, on_error: GlobErrorPolicy) -> bool {
    let Some(raw) = patterns_json.map(str::trim).filter(|s| !s.is_empty()) else {
        return true;
    };
    let patterns: Vec<String> = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => return on_error.verdict(),
    };
    if patterns.is_empty() {
        return true;
    }

    let mut builder = GlobSetBuilder::new();
    let mut added = false;
    for pattern in &patterns {
        if let Ok(glob) = Glob::new(pattern.trim()) {
            builder.add(glob);
            added = true;
        }
    }
    if !added {
        return on_error.verdict();
    }
    let Ok(set) = builder.build() else {
        return on_error.verdict();
    };

    // Normalise: drop a leading slash and convert backslashes so
    // Windows paths agree with Unix-style globs.
    let normalised = path.trim_start_matches('/').replace('\\', "/");
    set.is_match(&normalised)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_or_empty_is_universal_under_either_policy() {
        for policy in [GlobErrorPolicy::OverRecall, GlobErrorPolicy::Drop] {
            assert!(glob_match(None, "src/lib.rs", policy));
            assert!(glob_match(Some(""), "src/lib.rs", policy));
            assert!(glob_match(Some("   "), "src/lib.rs", policy));
            assert!(glob_match(Some("[]"), "src/lib.rs", policy));
        }
    }

    #[test]
    fn glob_match_basic_and_path_normalisation() {
        for policy in [GlobErrorPolicy::OverRecall, GlobErrorPolicy::Drop] {
            assert!(glob_match(
                Some(r#"["**/*.rs"]"#),
                "tokio/src/io/uring.rs",
                policy
            ));
            assert!(!glob_match(
                Some(r#"["**/*.rs"]"#),
                ".github/workflows/ci.yml",
                policy
            ));
            assert!(glob_match(
                Some(r#"["tokio/src/io/**"]"#),
                "tokio/src/io/uring.rs",
                policy
            ));
            assert!(!glob_match(
                Some(r#"["tokio/src/io/**"]"#),
                "tokio/src/runtime/mod.rs",
                policy
            ));
            // Backslash + leading-slash normalisation.
            assert!(glob_match(
                Some(r#"["tokio/src/io/**"]"#),
                "tokio\\src\\io\\uring.rs",
                policy
            ));
            assert!(glob_match(
                Some(r#"["tokio/src/io/**"]"#),
                "/tokio/src/io/uring.rs",
                policy
            ));
        }
    }

    #[test]
    fn malformed_blob_follows_policy() {
        // Malformed JSON.
        assert!(glob_match(
            Some("not-json"),
            "any/path.rs",
            GlobErrorPolicy::OverRecall
        ));
        assert!(!glob_match(
            Some("not-json"),
            "any/path.rs",
            GlobErrorPolicy::Drop
        ));
        // JSON object, not the expected array.
        assert!(glob_match(
            Some("{}"),
            "any/path.rs",
            GlobErrorPolicy::OverRecall
        ));
        assert!(!glob_match(
            Some("{}"),
            "any/path.rs",
            GlobErrorPolicy::Drop
        ));
    }
}
