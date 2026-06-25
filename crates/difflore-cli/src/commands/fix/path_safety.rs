//! Path-safety guards for the `fix` apply path.
//!
//! An issue's `file` and the LLM-generated diff are semi-trusted input — they
//! can originate from shared cloud rules or a crafted repo — so before we READ
//! a file and feed it to the model, or WRITE an LLM patch with `git apply`, we
//! constrain every path to the repository: no absolute paths, no `..`
//! traversal, no symlink escape, and a generated diff may only touch the single
//! file it was produced for. Without this, an issue `file` like
//! `../../../etc/passwd` would be read and exfiltrated to the provider, and one
//! accepted issue's patch could mutate any file in (or outside) the repo.

use std::path::{Component, Path, PathBuf};

/// Resolve `candidate` (an issue's `file`) against `repo_root`, rejecting any
/// path that is absolute, traverses out via `..`, or — when it already exists —
/// canonicalizes outside the repository (symlink escape). Returns the safe
/// absolute path to read.
pub(super) fn repo_relative_path(repo_root: &Path, candidate: &str) -> Result<PathBuf, String> {
    for comp in Path::new(candidate).components() {
        match comp {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir => {
                return Err(format!(
                    "refusing to touch '{candidate}': path escapes the repository via '..'"
                ));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(format!(
                    "refusing to touch '{candidate}': absolute paths are not repo-relative"
                ));
            }
        }
    }
    let joined = repo_root.join(candidate);
    // Symlink-escape guard (best effort; only when the path already resolves).
    if let Ok(canon) = joined.canonicalize() {
        let root = repo_root.canonicalize().map_err(|_| {
            format!("refusing to touch '{candidate}': repository root could not be canonicalized")
        })?;
        if !canon.starts_with(&root) {
            return Err(format!(
                "refusing to touch '{candidate}': resolves outside the repository"
            ));
        }
    }
    Ok(joined)
}

/// Verify a generated unified diff only modifies `expected_file_path`. Rejects a
/// diff that targets a different file, an absolute / `..` path, `/dev/null` (a
/// create or delete), or a rename/copy — so one accepted issue's patch can never
/// mutate an unrelated (or out-of-repo) file.
pub(super) fn validate_diff_targets(diff: &str, expected_file_path: &str) -> Result<(), String> {
    let expected = canonical_rel(expected_file_path);
    let reject = |why: String| -> Result<(), String> {
        Err(format!(
            "refusing to apply patch for '{expected_file_path}': {why}"
        ))
    };
    let mut saw_old = false;
    let mut saw_new = false;
    for raw_line in diff.lines() {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);

        // Reject git-format / extended / binary headers outright. `git apply`
        // acts on these (mode change, create/delete, rename/copy, binary patch)
        // WITHOUT a matching `--- `/`+++ ` line, so they can mutate another path
        // that a `--- `/`+++ ` scanner never sees. A single-file in-place text
        // edit — the only thing the patch prompt asks for — needs none of them.
        if line.starts_with("diff --git ")
            || line.starts_with("index ")
            || line.starts_with("old mode")
            || line.starts_with("new mode")
            || line.starts_with("new file")
            || line.starts_with("deleted file")
            || line.starts_with("rename ")
            || line.starts_with("copy ")
            || line.starts_with("similarity ")
            || line.starts_with("dissimilarity ")
            || line.starts_with("GIT binary patch")
            || line.starts_with("Binary files ")
        {
            return reject(
                "diff uses a git-extended/binary header; only a plain unified single-file edit is allowed".to_owned(),
            );
        }

        let (marker, want_prefix) = if line.starts_with("--- ") {
            ("---", "a/")
        } else if line.starts_with("+++ ") {
            ("+++", "b/")
        } else {
            continue;
        };
        // Drop only a trailing tab-separated timestamp — NOT spaces, which git
        // treats as significant filename bytes.
        let path = line[4..].split('\t').next().unwrap_or(&line[4..]);
        if path == "/dev/null" {
            return reject("diff creates or deletes a file".to_owned());
        }
        // Require the canonical `a/` (old) / `b/` (new) prefix so the forced
        // `git apply -p1` strips exactly that prefix, leaving the path we
        // validated. An unprefixed `--- src/foo.rs` would otherwise pass a
        // naive `== expected` check yet land on `foo.rs` after -p1.
        let Some(target) = path.strip_prefix(want_prefix) else {
            return reject(format!(
                "diff header '{marker} {path}' lacks the required '{want_prefix}' prefix"
            ));
        };
        if !is_safe_repo_relative(target) {
            return reject(format!("diff targets an unsafe path '{target}'"));
        }
        if canonical_rel(target) != expected {
            return reject(format!("diff targets a different file '{target}'"));
        }
        if marker == "---" {
            saw_old = true;
        } else {
            saw_new = true;
        }
    }
    if !saw_old || !saw_new {
        return reject(
            "diff has no complete `--- a/<file>` + `+++ b/<file>` header for the expected file"
                .to_owned(),
        );
    }
    Ok(())
}

/// A diff target is repo-safe iff every path component is a plain name or `.` —
/// no `..` traversal, no absolute root, and no Windows drive/UNC prefix.
fn is_safe_repo_relative(path: &str) -> bool {
    let normalized = path.replace('\\', "/");
    Path::new(&normalized)
        .components()
        .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
}

/// Lexical relative-path identity: forward-slashed, dropping `.` segments. Only
/// used for equality after `..`/absolute have already been rejected, so the
/// `ParentDir`/root cases never reach here.
fn canonical_rel(path: &str) -> String {
    Path::new(&path.replace('\\', "/"))
        .components()
        .filter_map(|c| match c {
            Component::Normal(seg) => Some(seg.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_relative_rejects_traversal_and_absolute() {
        let root = Path::new("/repo");
        assert!(repo_relative_path(root, "../../etc/passwd").is_err());
        assert!(repo_relative_path(root, "src/../../secret").is_err());
        assert!(repo_relative_path(root, "/etc/passwd").is_err());
        // A normal repo-relative path is accepted and joined under the root.
        let ok = repo_relative_path(root, "src/foo.rs").expect("relative path ok");
        assert_eq!(ok, Path::new("/repo/src/foo.rs"));
        // A leading `./` is harmless.
        assert!(repo_relative_path(root, "./src/foo.rs").is_ok());
    }

    #[test]
    fn validate_diff_accepts_a_single_file_modification() {
        let diff = "--- a/src/foo.rs\n+++ b/src/foo.rs\n@@ -1 +1 @@\n-old\n+new\n";
        assert!(validate_diff_targets(diff, "src/foo.rs").is_ok());
        // Path-prefix and `./` normalization still match.
        assert!(validate_diff_targets(diff, "./src/foo.rs").is_ok());
    }

    #[test]
    fn validate_diff_rejects_a_different_target_file() {
        let diff = "--- a/src/foo.rs\n+++ b/src/evil.rs\n@@ -1 +1 @@\n-old\n+new\n";
        assert!(validate_diff_targets(diff, "src/foo.rs").is_err());
    }

    #[test]
    fn validate_diff_rejects_traversal_absolute_devnull_and_rename() {
        let traversal = "--- a/src/foo.rs\n+++ b/../../etc/cron.d/x\n@@ -1 +1 @@\n-a\n+b\n";
        assert!(validate_diff_targets(traversal, "src/foo.rs").is_err());

        let absolute = "--- a/src/foo.rs\n+++ /etc/passwd\n@@ -1 +1 @@\n-a\n+b\n";
        assert!(validate_diff_targets(absolute, "src/foo.rs").is_err());

        let create = "--- /dev/null\n+++ b/src/foo.rs\n@@ -0,0 +1 @@\n+new\n";
        assert!(validate_diff_targets(create, "src/foo.rs").is_err());

        let rename =
            "diff --git a/src/foo.rs b/src/bar.rs\nrename from src/foo.rs\nrename to src/bar.rs\n";
        assert!(validate_diff_targets(rename, "src/foo.rs").is_err());
    }

    #[test]
    fn validate_diff_rejects_unprefixed_headers_that_p1_would_misroute() {
        // No a//b/ prefix passes a naive `== expected` check, but `git apply -p1`
        // strips the first component and would land on a different file.
        let diff = "--- src/foo.rs\n+++ src/foo.rs\n@@ -1 +1 @@\n-a\n+b\n";
        assert!(validate_diff_targets(diff, "src/foo.rs").is_err());
    }

    #[test]
    fn validate_diff_rejects_git_extended_and_smuggled_sections() {
        // A new-file / binary git section can touch another path with no
        // `---`/`+++` line a naive scanner would catch.
        let new_file = "diff --git a/evil b/evil\nnew file mode 100644\nindex 0000000..abc\n--- /dev/null\n+++ b/evil\n@@ -0,0 +1 @@\n+x\n";
        assert!(validate_diff_targets(new_file, "src/foo.rs").is_err());

        let binary = "diff --git a/x b/x\nindex 0..1 100644\nGIT binary patch\nliteral 4\n";
        assert!(validate_diff_targets(binary, "src/foo.rs").is_err());

        // A valid expected-file patch with a second git section smuggled after it.
        let smuggled = "--- a/src/foo.rs\n+++ b/src/foo.rs\n@@ -1 +1 @@\n-a\n+b\ndiff --git a/secret b/secret\ndeleted file mode 100644\n";
        assert!(validate_diff_targets(smuggled, "src/foo.rs").is_err());
    }
}
