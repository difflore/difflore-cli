//! Compare an agent's stated changes against the actual git diff.
//!
//! Catches the failure mode where an agent claims "I edited files X, Y, Z"
//! but `git diff` shows it never wrote one or more of them.
//!
//! Pure and side-effect free: caller supplies the claim text (an assistant
//! message) and the actual changed-file set (from `git diff --name-only`),
//! and decides where to surface the warning.

use std::collections::BTreeSet;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Agent claimed file edits but the diff is empty.
    Hallucination,
    /// Agent claimed at least one file that doesn't appear in the diff.
    PartialMismatch,
}

#[derive(Debug, Clone)]
pub struct Finding {
    pub severity: Severity,
    /// Files the agent's text mentioned as touched, normalised to forward
    /// slashes and stripped of leading `./`.
    pub claimed: Vec<String>,
    /// Files actually present in the diff.
    pub actual: Vec<String>,
    /// `claimed - actual` — the gap to surface to the user.
    pub missing_from_diff: Vec<String>,
}

impl Finding {
    /// One-line message for a CLI footer or hook output.
    ///
    /// Returns the unfiltered internal view — every claimed/missing path,
    /// including ones noisy for a non-engineer (SQL migrations, lockfiles,
    /// generated snapshots). Use [`Finding::summary_for_user`] for end users.
    pub fn summary(&self) -> String {
        match self.severity {
            Severity::Hallucination => format!(
                "agent claimed to edit {} file(s), git diff shows none — see [{}]",
                self.claimed.len(),
                truncate_list(&self.claimed, 3),
            ),
            Severity::PartialMismatch => format!(
                "agent claimed [{}] but diff is missing [{}]",
                truncate_list(&self.claimed, 3),
                truncate_list(&self.missing_from_diff, 3),
            ),
        }
    }

    /// User-facing one-liner for hooks / footers. Filters out files noisy for
    /// a non-engineer (SQL migrations, lockfiles, drizzle/`meta/*.json`
    /// snapshots, sqlx caches) and uses descriptive rather than accusatory
    /// wording. Returns `None` when nothing meaningful remains after filtering,
    /// in which case the caller suppresses the warning.
    pub fn summary_for_user(&self) -> Option<String> {
        let filtered_missing: Vec<&String> = self
            .missing_from_diff
            .iter()
            .filter(|p| !is_low_signal_for_user(p))
            .collect();
        if filtered_missing.is_empty() {
            return None;
        }
        let filtered_claimed: Vec<&String> = self
            .claimed
            .iter()
            .filter(|p| !is_low_signal_for_user(p))
            .collect();
        let claimed_for_display: Vec<String> = if filtered_claimed.is_empty() {
            self.claimed.clone()
        } else {
            filtered_claimed.into_iter().cloned().collect()
        };
        let missing_for_display: Vec<String> = filtered_missing.into_iter().cloned().collect();
        Some(match self.severity {
            Severity::Hallucination => format!(
                "agent described {} file edit(s) but the diff is empty — likely a worktree or staged-elsewhere situation. Mentioned: [{}]",
                claimed_for_display.len(),
                truncate_list(&claimed_for_display, 3),
            ),
            Severity::PartialMismatch => format!(
                "agent referenced [{}] in its description that aren't in the diff yet — usually fine if the work landed in a worktree",
                truncate_list(&missing_for_display, 3),
            ),
        })
    }
}

/// True if `claim` names a file class noisy for a non-engineer reader.
/// Generated artefacts, migrations, lockfiles, and offline query caches are
/// real files but their absence-from-diff carries no actionable signal for a
/// product user. Filtered before user-facing surfaces; kept in the internal
/// `missing_from_diff` for engineering callers.
fn is_low_signal_for_user(claim: &str) -> bool {
    let lower = claim.to_ascii_lowercase();
    // Lockfiles and offline caches.
    if lower.ends_with(".lock")
        || lower.ends_with("-lock.yaml")
        || lower.ends_with("-lock.json")
        || lower.ends_with("pnpm-lock.yaml")
        || lower.ends_with("yarn.lock")
        || lower.ends_with("package-lock.json")
        || lower.ends_with("cargo.lock")
        || lower.ends_with("poetry.lock")
        || lower.ends_with("uv.lock")
    {
        return true;
    }
    // SQL migrations — generated files in numbered subpaths users don't read.
    if lower.ends_with(".sql") {
        return true;
    }
    // Drizzle / migration snapshot JSONs.
    if lower.contains("/meta/") && lower.ends_with(".json") {
        return true;
    }
    // sqlx offline query cache, at repo root or nested under a crate.
    if lower.starts_with(".sqlx/") || lower.contains("/.sqlx/") {
        return true;
    }
    // Snapshot artefacts (insta, jest snapshots, etc.).
    if lower.ends_with(".snap") || lower.contains(".snap.") {
        return true;
    }
    false
}

/// Run the comparison. `actual` is the set from `git diff --name-only <base>`
/// (or equivalent); an empty set means the agent edited nothing.
/// `expected_hint`, when known (e.g. a PR's `Files changed` table), filters
/// unrecognised bare-filename mentions in `claim_text` to reduce false positives.
pub fn validate(
    claim_text: &str,
    actual: &[impl AsRef<Path>],
    expected_hint: &[impl AsRef<Path>],
) -> Option<Finding> {
    let actual: BTreeSet<String> = actual.iter().map(|p| normalise(p.as_ref())).collect();
    let expected: BTreeSet<String> = expected_hint
        .iter()
        .map(|p| normalise(p.as_ref()))
        .collect();

    let raw_claims = extract_claimed_paths(claim_text);
    let mut claimed: BTreeSet<String> = BTreeSet::new();

    for c in raw_claims {
        // Out-of-tree claims (home/system absolute paths) can never appear in
        // `git diff --name-only`; the validator only catches repo-edit
        // hallucinations, not global config edits.
        if is_out_of_tree(&c) {
            continue;
        }
        // Build artefacts (.exe, .dll, .so, …) are produced/copied by shell
        // commands, never edited as source, so they never appear in a diff.
        if is_binary_artifact(&c) {
            continue;
        }
        if actual.contains(&c) || expected.contains(&c) {
            claimed.insert(c);
            continue;
        }
        // Suffix match: model often writes "view.go" for the canonical
        // "pkg/cmd/run/view/view.go". Accept the longer form when the suffix
        // matches an actual or expected entry.
        let mut matched = false;
        for ref_path in actual.iter().chain(expected.iter()) {
            if ref_path == &c || ref_path.ends_with(&format!("/{c}")) {
                claimed.insert(ref_path.clone());
                matched = true;
                break;
            }
        }
        if matched {
            continue;
        }
        // Trust unmatched mentions that look like a real filename (contain a
        // `/`, or have a recognised extension). Hallucinations often cite by
        // basename in markdown bold (`**go.mod**`), so dropping all bare names
        // would miss them.
        if c.contains('/') || looks_like_filename(&c) {
            claimed.insert(c);
        }
    }

    if claimed.is_empty() {
        return None;
    }

    let missing: Vec<String> = claimed.difference(&actual).cloned().collect();
    if missing.is_empty() {
        return None;
    }

    let severity = if actual.is_empty() {
        Severity::Hallucination
    } else {
        Severity::PartialMismatch
    };

    Some(Finding {
        severity,
        claimed: claimed.into_iter().collect(),
        actual: actual.into_iter().collect(),
        missing_from_diff: missing,
    })
}

/// True if `claim` is an absolute path outside any working-tree — a home
/// dotfile, system config, or Windows drive-letter absolute. `git diff
/// --name-only` only emits repo-relative paths, so these can't be validated
/// and would otherwise always be flagged "missing from diff".
fn is_out_of_tree(claim: &str) -> bool {
    if claim.starts_with("~/")
        || claim.starts_with("$HOME")
        || claim.starts_with("/home/")
        || claim.starts_with("/Users/")
        || claim.starts_with("/etc/")
        || claim.starts_with("/usr/")
        || claim.starts_with("/var/")
    {
        return true;
    }
    // git-bash / MSYS form on Windows: /c/Users/foo/...
    let lower = claim.to_ascii_lowercase();
    if lower.starts_with("/c/users/")
        || lower.starts_with("/d/users/")
        || lower.starts_with("/e/users/")
    {
        return true;
    }
    // Windows drive-letter absolute: `C:/...` or `C:\...`.
    let bytes = claim.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'/' || bytes[2] == b'\\')
}

/// True if `claim` ends in a build-artefact extension (compiled binary,
/// shared library, archive, debug symbol). These are outputs and never appear
/// in `git diff` as content edits.
fn is_binary_artifact(claim: &str) -> bool {
    let stripped = strip_quotes(claim).trim_end_matches(&[',', '.', ';', ':'][..]);
    let Some((_, ext)) = stripped.rsplit_once('.') else {
        return false;
    };
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "exe"
            | "dll"
            | "so"
            | "dylib"
            | "a"
            | "o"
            | "obj"
            | "lib"
            | "pdb"
            | "wasm"
            | "class"
            | "jar"
            | "pyc"
            | "pyo"
    )
}

/// True if `s` has the shape of a filename: a name part, a `.`, then a 1–6
/// char ASCII-alphanumeric extension. Gates which unmatched bare claims
/// survive (vs. e.g. "config" or "test", which have no dot).
fn looks_like_filename(s: &str) -> bool {
    let Some((name, ext)) = s.rsplit_once('.') else {
        return false;
    };
    !name.is_empty()
        && !ext.is_empty()
        && ext.len() <= 6
        && ext.chars().all(|c| c.is_ascii_alphanumeric())
}

fn normalise(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    s.trim_start_matches("./").to_owned()
}

/// Extract path-like tokens from prose.
///
/// Recognises three forms:
///   1. quoted/back-ticked/bolded paths with extension: `path/to/file.ext`
///   2. quoted/back-ticked bare filenames with extension: `go.mod`
///   3. unquoted well-known config files: `go.mod`, `package.json`, etc.
fn extract_claimed_paths(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();

    // Pass 1: explicit paths (contain `/`).
    for word in tokenise(text) {
        // Shell-style `name='...'` / `name="..."` — peel off the lvalue so
        // `alias difflore='C:/.../foo.exe'` extracts the path, not the lvalue.
        let body = match word.find('=') {
            Some(eq) if matches!(word.as_bytes().get(eq + 1), Some(b'\'' | b'"')) => {
                &word[eq + 1..]
            }
            _ => word,
        };
        if body.contains('/') && has_extension(body) {
            let trimmed = body.trim_end_matches(&[',', '.', ';', ':'][..]);
            out.push(strip_quotes(trimmed).to_owned());
        }
    }
    // Pass 2: quoted bare filenames with a *known* extension. Requiring a
    // whitelisted extension (not any 1–6-char alphanumeric suffix) keeps
    // quoted dotted identifiers like `pack.rule_context.title` from being
    // mistaken for filenames. Quote style is irrelevant.
    for (_delim, slice) in iter_quoted_runs(text) {
        for word in slice.split_whitespace() {
            if has_known_file_ext(word) && !word.contains('/') {
                let cleaned =
                    strip_quotes(word.trim_end_matches(&[',', '.', ';', ':'][..])).to_owned();
                if !cleaned.is_empty() {
                    out.push(cleaned);
                }
            }
        }
    }
    // Pass 3: well-known unquoted config-file mentions.
    for token in [
        "go.mod",
        "go.sum",
        "package.json",
        "pnpm-lock.yaml",
        "Cargo.toml",
        "Cargo.lock",
        "tsconfig.json",
        "Makefile",
    ] {
        if text.contains(token) {
            out.push(token.to_owned());
        }
    }

    out
}

fn tokenise(text: &str) -> Vec<&str> {
    text.split(|c: char| c.is_whitespace() || matches!(c, '(' | ')' | ',' | ';'))
        .filter(|s| !s.is_empty())
        .collect()
}

fn has_extension(word: &str) -> bool {
    let stripped = strip_quotes(word.trim_end_matches(&[',', '.', ';', ':'][..]));
    let last_dot = stripped.rfind('.').map(|i| &stripped[i + 1..]);
    matches!(last_dot, Some(ext) if !ext.is_empty()
        && ext.len() <= 6
        && ext.chars().all(|c| c.is_ascii_alphanumeric()))
}

/// Like `has_extension` but only matches a known file extension. Used in
/// Pass 2 (bare quoted filenames), where there's no `/` to disambiguate a
/// real path from a dotted identifier (`pack.rule_context.title`). A false
/// negative is cheap: Pass 1 still catches any path with a `/`.
fn has_known_file_ext(word: &str) -> bool {
    const KNOWN: &[&str] = &[
        // code
        "rs",
        "ts",
        "tsx",
        "js",
        "jsx",
        "mjs",
        "cjs",
        "py",
        "go",
        "java",
        "kt",
        "rb",
        "php",
        "c",
        "h",
        "cpp",
        "hpp",
        "cc",
        "hh",
        "cs",
        "swift",
        "scala",
        "sh",
        "bash",
        "zsh",
        "fish",
        "ps1",
        "sql",
        "lua",
        "vim",
        "nu",
        "exs",
        "ex",
        "erl",
        // data / config
        "json",
        "yaml",
        "yml",
        "toml",
        "xml",
        "csv",
        "tsv",
        "ini",
        "conf",
        "env",
        "lock",
        "mod",
        "sum",
        "properties",
        // docs
        "md",
        "mdx",
        "txt",
        "rst",
        "adoc",
        // web
        "html",
        "htm",
        "css",
        "scss",
        "sass",
        "less",
        "vue",
        "svelte",
        // shell/build
        "Makefile",
        "Dockerfile",
        "gradle",
        "bzl",
        "bazel",
        // image (rare but seen in PR descriptions)
        "png",
        "jpg",
        "jpeg",
        "svg",
        "gif",
        "webp",
        "ico",
    ];
    let stripped = strip_quotes(word.trim_end_matches(&[',', '.', ';', ':'][..]));
    let Some(idx) = stripped.rfind('.') else {
        return false;
    };
    let ext = &stripped[idx + 1..];
    KNOWN.iter().any(|&k| k.eq_ignore_ascii_case(ext))
}

fn strip_quotes(word: &str) -> &str {
    word.trim_matches(|c: char| matches!(c, '`' | '*' | '"' | '\''))
}

/// Yield quoted spans `(delim_char, contents)` for back-tick / bold-star /
/// quote-pair markers. Bold uses `**…**` (two stars on each side).
fn iter_quoted_runs(text: &str) -> Vec<(char, &str)> {
    let mut out = Vec::new();
    for delim in ['`', '"', '\''] {
        let mut start: Option<usize> = None;
        for (i, c) in text.char_indices() {
            if c == delim {
                if let Some(s) = start.take() {
                    if i > s + 1 {
                        out.push((delim, &text[s + 1..i]));
                    }
                } else {
                    start = Some(i);
                }
            }
        }
    }
    // Bold: split on `**` and treat alternating segments as quoted.
    let parts: Vec<&str> = text.split("**").collect();
    for (i, part) in parts.iter().enumerate() {
        if i % 2 == 1 && !part.is_empty() {
            out.push(('*', part));
        }
    }
    out
}

fn truncate_list(items: &[String], max: usize) -> String {
    if items.len() <= max {
        items.join(", ")
    } else {
        let head: Vec<&str> = items.iter().take(max).map(String::as_str).collect();
        format!("{}, +{} more", head.join(", "), items.len() - max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn paths(items: &[&str]) -> Vec<PathBuf> {
        items.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn flags_full_hallucination_when_diff_empty_and_claims_match_expected() {
        // Real fixture: gin/#4580/difflore-haiku — claimed go.mod + workflows, edited 0.
        let claim = r"
        - **golang.org/x/sys**: v0.41.0 -> v0.45.0
        - **gin.yml**: bumped golangci-lint to v2.11
        - **trivy-scan.yml**: bumped action to 0.35.0
        All changes have been applied to go.mod and go.sum.
        ";
        let actual: Vec<PathBuf> = vec![];
        let expected = paths(&[
            ".github/workflows/gin.yml",
            ".github/workflows/trivy-scan.yml",
            "go.mod",
            "go.sum",
        ]);
        let f = validate(claim, &actual, &expected).expect("should flag");
        assert_eq!(f.severity, Severity::Hallucination);
        assert!(f.missing_from_diff.contains(&"go.mod".to_owned()));
        assert!(f.missing_from_diff.iter().any(|p| p.ends_with("gin.yml")));
    }

    #[test]
    fn flags_partial_mismatch_when_one_claimed_file_missing() {
        // Real fixture: cli/#13272/bare-haiku — wrote .go files, claimed but
        // didn't write the txtar acceptance test.
        let claim = r"
        I modified `pkg/cmd/run/view/view.go`, added tests in
        `pkg/cmd/run/view/view_test.go`, and created the integration test
        at `acceptance/testdata/workflow/run-view-log-escape-sequences.txtar`.
        ";
        let actual = paths(&["pkg/cmd/run/view/view.go", "pkg/cmd/run/view/view_test.go"]);
        let expected: Vec<PathBuf> = vec![];
        let f = validate(claim, &actual, &expected).expect("should flag");
        assert_eq!(f.severity, Severity::PartialMismatch);
        assert_eq!(f.missing_from_diff.len(), 1);
        assert!(f.missing_from_diff[0].ends_with("escape-sequences.txtar"));
    }

    #[test]
    fn no_finding_when_diff_matches_claims() {
        let claim = "Updated `view.go` and added a test in `view_test.go`.";
        let actual = paths(&["pkg/cmd/run/view/view.go", "pkg/cmd/run/view/view_test.go"]);
        let expected: Vec<PathBuf> = vec![];
        assert!(validate(claim, &actual, &expected).is_none());
    }

    #[test]
    fn no_finding_when_no_paths_in_claim() {
        let claim = "I looked at the code and decided no changes are needed.";
        let actual: Vec<PathBuf> = vec![];
        let expected: Vec<PathBuf> = vec![];
        assert!(validate(claim, &actual, &expected).is_none());
    }

    #[test]
    fn suffix_match_lifts_bare_filename_to_full_path() {
        // Model wrote "view.go" inline; canonical is "pkg/cmd/run/view/view.go".
        let claim = "Updated `view.go` for the security fix.";
        let actual: Vec<PathBuf> = vec![];
        let expected = paths(&["pkg/cmd/run/view/view.go"]);
        let f = validate(claim, &actual, &expected).expect("should flag");
        assert_eq!(f.severity, Severity::Hallucination);
        assert_eq!(f.claimed, vec!["pkg/cmd/run/view/view.go"]);
    }

    #[test]
    fn drops_common_noun_false_positives() {
        let claim = "Reviewed the test suite and the config file. No edits.";
        let actual: Vec<PathBuf> = vec![];
        let expected: Vec<PathBuf> = vec![];
        // "test" / "config" without extension or quotes shouldn't fire.
        assert!(validate(claim, &actual, &expected).is_none());
    }

    #[test]
    fn drops_quoted_code_field_paths() {
        // Real-world false positive: Rust field paths quoted in PR
        // descriptions / commit messages get caught by the bare-
        // filename pass because `title` looked like an extension to
        // the loose `has_extension` heuristic. We now require a known
        // file extension in Pass 2; field paths must not flag.
        let claim = "Audit displays `pack.rule_context.title` from `r.title` even when fetched from skills DB.";
        let actual: Vec<PathBuf> = vec![];
        let expected: Vec<PathBuf> = vec![];
        assert!(
            validate(claim, &actual, &expected).is_none(),
            "code field paths in backticks must not be claimed-path candidates"
        );
    }

    #[test]
    fn summary_text_is_present_and_short() {
        let claim = "Edited `pkg/foo/bar.go` and `lib.rs`.";
        let actual: Vec<PathBuf> = vec![];
        let expected = paths(&["pkg/foo/bar.go", "lib.rs"]);
        let f = validate(claim, &actual, &expected).expect("flag");
        let s = f.summary();
        assert!(s.contains("git diff shows none"));
        assert!(s.len() < 200);
    }

    #[test]
    fn handles_store_295_autofix_yml_pattern() {
        // Regression: claimed three workflow files, but only two appear in the
        // actual diff.
        let claim = "I updated `.github/workflows/autofix.yml`, \
                     `.github/workflows/pr.yml`, and \
                     `.github/workflows/release.yml` to bump the checkout action \
                     and apply the casing fix. All three workflow files are now \
                     consistent with TanStack/config conventions.";
        let actual = paths(&[".github/workflows/pr.yml", ".github/workflows/release.yml"]);
        let expected = paths(&[
            ".github/workflows/autofix.yml",
            ".github/workflows/pr.yml",
            ".github/workflows/release.yml",
        ]);
        let f = validate(claim, &actual, &expected).expect("should flag");
        assert_eq!(f.severity, Severity::PartialMismatch);
        assert!(
            f.missing_from_diff
                .iter()
                .any(|p| p.ends_with("autofix.yml"))
        );
    }

    #[test]
    fn handles_bold_basename_real_world_case() {
        // Real fixture (gin/#4580/difflore-haiku log): markdown bold with
        // bare basenames is the dominant claim shape. Validator must
        // catch these even when neither expected_hint nor actual carries
        // the canonical full path.
        let claim = "## Summary of Changes\n\
                     - **gin.yml**: Upgraded golangci-lint from v2.9 to v2.11\n\
                     - **trivy-scan.yml**: bumped\n\
                     ### Go Dependencies Updates (go.mod & go.sum)\n\
                     - **goccy/go-json**: v0.10.5 → v0.11.0";
        let actual: Vec<PathBuf> = Vec::new();
        let expected: Vec<PathBuf> = Vec::new();
        let f = validate(claim, &actual, &expected).expect("real-world fixture should flag");
        assert_eq!(f.severity, Severity::Hallucination);
        assert!(
            f.claimed.iter().any(|c| c == "go.mod"),
            "expected go.mod claim from bare-name extraction, got: {:?}",
            f.claimed
        );
        assert!(
            f.claimed
                .iter()
                .any(|c| c.ends_with("gin.yml") || c == "gin.yml"),
            "expected gin.yml claim from bold extraction, got: {:?}",
            f.claimed
        );
    }

    #[test]
    fn skips_home_dotfile_claim() {
        // Real fixture: agent edited `~/.zshrc` to add a shell alias. The
        // file is outside any git repo, so `git diff` is empty even though
        // the edit succeeded — must not flag.
        let claim = "Wrote `~/.zshrc`";
        let actual: Vec<PathBuf> = vec![];
        let expected: Vec<PathBuf> = vec![];
        assert!(validate(claim, &actual, &expected).is_none());
    }

    #[test]
    fn skips_windows_absolute_path_claim() {
        // Same scenario via the Windows drive-letter form. The CLI path
        // `C:/Users/alice/.../difflore.exe` showed up in the alias body
        // and got picked up by Pass 1 (contains `/` + `.exe` extension).
        let claim = "alias difflore='C:/Users/alice/projects/difflore/target/release/difflore.exe'";
        let actual: Vec<PathBuf> = vec![];
        let expected: Vec<PathBuf> = vec![];
        assert!(validate(claim, &actual, &expected).is_none());
    }

    #[test]
    fn skips_bare_binary_artefact_mention() {
        // Real fixture: agent described a `cp` step copying
        // `difflore-hook.exe` between bin dirs. The .exe is built, never
        // edited as source — must not flag as a missing-from-diff claim.
        let claim = "Copied `difflore-hook.exe` into ~/bin/";
        let actual: Vec<PathBuf> = vec![];
        let expected: Vec<PathBuf> = vec![];
        assert!(validate(claim, &actual, &expected).is_none());
    }

    #[test]
    fn skips_other_build_artefact_extensions() {
        for word in ["foo.dll", "libthing.so", "core.o", "app.wasm", "Bar.class"] {
            let claim = format!("rebuilt `{word}`");
            assert!(
                validate(&claim, &Vec::<PathBuf>::new(), &Vec::<PathBuf>::new()).is_none(),
                "should skip binary artefact: {word}"
            );
        }
    }

    #[test]
    fn skips_unix_home_absolute_path_claim() {
        let claim = "Updated `/home/alice/.config/foo.toml` for the workaround.";
        let actual: Vec<PathBuf> = vec![];
        let expected: Vec<PathBuf> = vec![];
        assert!(validate(claim, &actual, &expected).is_none());
    }

    #[test]
    fn still_flags_in_tree_hallucination_alongside_out_of_tree_claim() {
        // Mixed claim: legitimate repo edit hallucination + a benign
        // home-config edit. Validator must still fire on the repo path.
        let claim = "Updated `~/.zshrc` and `src/lib.rs`.";
        let actual: Vec<PathBuf> = vec![];
        let expected = paths(&["src/lib.rs"]);
        let f = validate(claim, &actual, &expected).expect("should still flag in-tree miss");
        assert_eq!(f.severity, Severity::Hallucination);
        assert_eq!(f.claimed, vec!["src/lib.rs"]);
        assert!(!f.claimed.iter().any(|c| c.contains(".zshrc")));
    }

    #[test]
    fn handles_router_changeset_pattern() {
        // Real fixture: router/#7265 — agent claimed a TanStack changeset
        // file alongside the real source edits; never wrote the changeset.
        // A PR shipped without it would fail the upstream changesets check.
        let claim = "Added `.changeset/nine-years-grab.md` documenting the fix \
                     and updated `packages/solid-router/src/useMatch.tsx` plus \
                     `packages/solid-router/tests/loaders.test.tsx`.";
        let actual = paths(&[
            "packages/solid-router/src/useMatch.tsx",
            "packages/solid-router/tests/loaders.test.tsx",
        ]);
        let expected: Vec<PathBuf> = vec![];
        let f = validate(claim, &actual, &expected).expect("should flag");
        assert_eq!(f.severity, Severity::PartialMismatch);
        assert!(
            f.missing_from_diff
                .iter()
                .any(|p| p.contains("nine-years-grab.md"))
        );
    }

    #[test]
    fn user_summary_filters_low_signal_misses() {
        // Real-world: agent worked in a worktree and described a migration
        // + a code file. Only the migration is "missing from diff" because
        // the worktree's branch isn't merged yet. Migrations alone are not
        // a user-facing concern — the warning should suppress entirely.
        let f = Finding {
            severity: Severity::PartialMismatch,
            claimed: vec![
                "drizzle/0013_rule_trust_supersedes.sql".to_owned(),
                "src/orpc/rules.ts".to_owned(),
            ],
            actual: vec!["src/orpc/rules.ts".to_owned()],
            missing_from_diff: vec!["drizzle/0013_rule_trust_supersedes.sql".to_owned()],
        };
        assert!(
            f.summary_for_user().is_none(),
            "SQL-only misses must suppress the user-facing warning"
        );
    }

    #[test]
    fn user_summary_keeps_genuine_code_misses() {
        let f = Finding {
            severity: Severity::PartialMismatch,
            claimed: vec![
                "src/orpc/rules.ts".to_owned(),
                "src/orpc/billing.ts".to_owned(),
            ],
            actual: vec!["src/orpc/rules.ts".to_owned()],
            missing_from_diff: vec!["src/orpc/billing.ts".to_owned()],
        };
        let s = f.summary_for_user().expect("genuine miss must surface");
        assert!(s.contains("billing.ts"), "got: {s}");
        assert!(
            !s.contains("claimed") || !s.contains("but diff is missing"),
            "wording must be softened away from the accusatory original"
        );
    }

    #[test]
    fn user_summary_drops_lockfile_and_meta_snapshot() {
        let f = Finding {
            severity: Severity::PartialMismatch,
            claimed: vec![
                "Cargo.lock".to_owned(),
                "drizzle/meta/0013_snapshot.json".to_owned(),
                "pnpm-lock.yaml".to_owned(),
                ".sqlx/query-abc.json".to_owned(),
                "src/lib.rs".to_owned(),
            ],
            actual: vec!["src/lib.rs".to_owned()],
            missing_from_diff: vec![
                "Cargo.lock".to_owned(),
                "drizzle/meta/0013_snapshot.json".to_owned(),
                "pnpm-lock.yaml".to_owned(),
                ".sqlx/query-abc.json".to_owned(),
            ],
        };
        assert!(
            f.summary_for_user().is_none(),
            "lockfile + meta snapshot + sqlx cache only must suppress"
        );
    }

    #[test]
    fn is_low_signal_table() {
        for s in [
            "drizzle/0013_rule_trust_supersedes.sql",
            "Cargo.lock",
            "pnpm-lock.yaml",
            "package-lock.json",
            "drizzle/meta/0011_snapshot.json",
            ".sqlx/query-82aab2.json",
            "test.snap",
            "snapshots/foo.snap.new",
        ] {
            assert!(is_low_signal_for_user(s), "{s} should be low-signal");
        }
        for s in [
            "src/orpc/rules.ts",
            "crates/difflore-core/src/lib.rs",
            ".github/workflows/pr.yml",
            "README.md",
        ] {
            assert!(!is_low_signal_for_user(s), "{s} should NOT be low-signal");
        }
    }
}
