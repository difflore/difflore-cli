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

use std::borrow::Cow;

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

/// Outcome of turning a rule's `patterns_json` blob into a matchable
/// glob set, shared by the single-path and changeset matchers so the
/// universal/error semantics cannot drift between them.
enum BuiltPatterns {
    /// Absent / blank / `[]`: universal rule, matches every path.
    Universal,
    /// A usable glob set to match paths against.
    Set(globset::GlobSet),
    /// Malformed JSON, zero parseable globs, or `GlobSet::build` failure;
    /// the caller resolves this through its [`GlobErrorPolicy`].
    Unusable,
}

/// Parse + compile a JSON-encoded glob list (e.g. `["src/**/*.rs"]`).
fn build_globset(patterns_json: Option<&str>) -> BuiltPatterns {
    let Some(raw) = patterns_json.map(str::trim).filter(|s| !s.is_empty()) else {
        return BuiltPatterns::Universal;
    };
    let patterns: Vec<String> = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => return BuiltPatterns::Unusable,
    };
    if patterns.is_empty() {
        return BuiltPatterns::Universal;
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
        return BuiltPatterns::Unusable;
    }
    match builder.build() {
        Ok(set) => BuiltPatterns::Set(set),
        Err(_) => BuiltPatterns::Unusable,
    }
}

/// Normalise a path for glob matching: drop a leading slash and convert
/// backslashes so Windows-style paths agree with forward-slash globs.
fn normalise_path(path: &str) -> Cow<'_, str> {
    let trimmed = path.trim_start_matches('/');
    if trimmed.contains('\\') {
        Cow::Owned(trimmed.replace('\\', "/"))
    } else {
        Cow::Borrowed(trimmed)
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
    match build_globset(patterns_json) {
        BuiltPatterns::Universal => true,
        BuiltPatterns::Unusable => on_error.verdict(),
        BuiltPatterns::Set(set) => {
            let path = normalise_path(path);
            set.is_match(path.as_ref())
        }
    }
}

/// Changeset variant of [`glob_match`]: decide whether ANY of `paths`
/// (typically `git diff --name-only` output) is in scope for a rule's
/// `patterns_json` glob list.
///
/// Semantics mirror the single-path matcher exactly:
/// * absent / blank / `[]` patterns are a universal rule — `true`
///   regardless of `paths` (even an empty changeset);
/// * an unusable pattern blob resolves through `on_error`, same as
///   [`glob_match`], so retrieval keeps over-recalling and attribution
///   keeps dropping;
/// * otherwise `true` iff at least one normalised path matches at least
///   one glob. An empty `paths` slice therefore matches nothing —
///   pattern-scoped rules need a changed file to prove they apply.
///
/// The glob set is compiled once and reused across all paths.
pub fn glob_match_changeset(
    patterns_json: Option<&str>,
    paths: &[String],
    on_error: GlobErrorPolicy,
) -> bool {
    match build_globset(patterns_json) {
        BuiltPatterns::Universal => true,
        BuiltPatterns::Unusable => on_error.verdict(),
        BuiltPatterns::Set(set) => paths.iter().any(|path| {
            let path = normalise_path(path);
            set.is_match(path.as_ref())
        }),
    }
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
    fn normalise_path_borrows_when_no_rewrite_is_needed() {
        assert!(matches!(normalise_path("src/lib.rs"), Cow::Borrowed(_)));
        assert_eq!(normalise_path("/src/lib.rs").as_ref(), "src/lib.rs");
        assert!(matches!(
            normalise_path("src\\lib.rs"),
            Cow::Owned(ref path) if path == "src/lib.rs"
        ));
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

    // ── Changeset matcher ────────────────────────────────────────────
    //
    // Synthetic changesets modelled on the motivating case: a cross-cutting
    // rule whose `filePatterns` carries BOTH sides of a coupled change
    // (schema file + migrations directory), recalled for `recall --diff` /
    // `fix` when ANY changed file lands in scope.

    /// The cross-cutting rule's pattern blob: schema changes must ship with
    /// a migration, so both sides are listed.
    const SCHEMA_MIGRATION_GLOBS: &str = r#"["db/schema/**", "migrations/**/*.sql"]"#;

    fn changeset(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|p| (*p).to_owned()).collect()
    }

    #[test]
    fn changeset_both_sides_of_coupled_change_hit() {
        // Schema + migration both touched: the canonical compliant diff.
        let diff = changeset(&["db/schema/users.sql", "migrations/0042/add_email.sql"]);
        for policy in [GlobErrorPolicy::OverRecall, GlobErrorPolicy::Drop] {
            assert!(glob_match_changeset(
                Some(SCHEMA_MIGRATION_GLOBS),
                &diff,
                policy
            ));
        }
    }

    #[test]
    fn changeset_single_side_hit_recalls_rule() {
        // Only the schema side changed (the violation the rule exists to
        // catch): ANY-path semantics must still surface the rule, even when
        // the rest of the diff is unrelated.
        let diff = changeset(&["src/api/handler.ts", "db/schema/users.sql"]);
        for policy in [GlobErrorPolicy::OverRecall, GlobErrorPolicy::Drop] {
            assert!(glob_match_changeset(
                Some(SCHEMA_MIGRATION_GLOBS),
                &diff,
                policy
            ));
        }
        // Migration-only diffs hit via the second glob.
        let migration_only = changeset(&["migrations/0042/add_email.sql"]);
        assert!(glob_match_changeset(
            Some(SCHEMA_MIGRATION_GLOBS),
            &migration_only,
            GlobErrorPolicy::OverRecall
        ));
    }

    #[test]
    fn changeset_no_path_in_scope_is_no_match() {
        let diff = changeset(&["src/api/handler.ts", "README.md"]);
        for policy in [GlobErrorPolicy::OverRecall, GlobErrorPolicy::Drop] {
            assert!(!glob_match_changeset(
                Some(SCHEMA_MIGRATION_GLOBS),
                &diff,
                policy
            ));
        }
    }

    #[test]
    fn changeset_empty_or_absent_patterns_is_universal() {
        let diff = changeset(&["src/lib.rs"]);
        for policy in [GlobErrorPolicy::OverRecall, GlobErrorPolicy::Drop] {
            assert!(glob_match_changeset(None, &diff, policy));
            assert!(glob_match_changeset(Some(""), &diff, policy));
            assert!(glob_match_changeset(Some("[]"), &diff, policy));
            // Universal rules match even an empty changeset, mirroring the
            // single-path matcher's "always in scope" contract.
            assert!(glob_match_changeset(Some("[]"), &[], policy));
        }
    }

    #[test]
    fn changeset_empty_paths_never_proves_a_scoped_rule() {
        // A pattern-scoped rule needs at least one changed file to apply;
        // an empty changeset matches nothing under either policy.
        for policy in [GlobErrorPolicy::OverRecall, GlobErrorPolicy::Drop] {
            assert!(!glob_match_changeset(
                Some(SCHEMA_MIGRATION_GLOBS),
                &[],
                policy
            ));
        }
    }

    #[test]
    fn changeset_malformed_blob_follows_policy() {
        let diff = changeset(&["db/schema/users.sql"]);
        // Malformed JSON / wrong JSON shape: over-recall keeps the rule,
        // drop refuses to credit it — identical to the single-path matcher.
        for blob in ["not-json", "{}"] {
            assert!(glob_match_changeset(
                Some(blob),
                &diff,
                GlobErrorPolicy::OverRecall
            ));
            assert!(!glob_match_changeset(
                Some(blob),
                &diff,
                GlobErrorPolicy::Drop
            ));
        }
    }

    #[test]
    fn changeset_normalises_windows_and_leading_slash_paths() {
        let diff = changeset(&["db\\schema\\users.sql"]);
        assert!(glob_match_changeset(
            Some(SCHEMA_MIGRATION_GLOBS),
            &diff,
            GlobErrorPolicy::Drop
        ));
        let rooted = changeset(&["/migrations/0001/init.sql"]);
        assert!(glob_match_changeset(
            Some(SCHEMA_MIGRATION_GLOBS),
            &rooted,
            GlobErrorPolicy::Drop
        ));
    }

    #[test]
    fn changeset_agrees_with_single_path_matcher_per_path() {
        // ANY-semantics sanity: the changeset verdict equals the OR of the
        // per-path verdicts, for both policies.
        let paths = changeset(&[
            "src/api/handler.ts",
            "db/schema/users.sql",
            ".github/workflows/ci.yml",
        ]);
        for policy in [GlobErrorPolicy::OverRecall, GlobErrorPolicy::Drop] {
            let per_path_any = paths
                .iter()
                .any(|p| glob_match(Some(SCHEMA_MIGRATION_GLOBS), p, policy));
            assert_eq!(
                glob_match_changeset(Some(SCHEMA_MIGRATION_GLOBS), &paths, policy),
                per_path_any,
            );
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
