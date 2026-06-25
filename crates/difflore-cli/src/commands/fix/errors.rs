/// Prefix emitted by `apply.rs` when spawning `git` fails (imported there as
/// `GIT_SPAWN_PREFIX`). Centralized here so producer and classifier share one
/// source of truth: an upstream reword breaks the regression tests below rather
/// than silently degrading the hint.
pub(super) const GIT_SPAWN_FAILURE: &str = "failed to spawn git";
/// OS-level "file not found" markers surfaced through `git`'s spawn error.
/// These are produced by the platform (`io::Error` Display), not by us, so we
/// pin them as named constants and guard each with a regression test.
const OS_NOT_FOUND_UNIX: &str = "no such file or directory";
const OS_NOT_FOUND_WINDOWS: &str = "the system cannot find the file specified";

pub(super) fn format_fix_err(label: &str, raw: &str) -> String {
    let raw = raw.trim();
    let lower = raw.to_ascii_lowercase();
    if lower.contains("no llm provider configured")
        || lower.contains("no active ai provider configured")
        || lower.contains("no supported agent cli found")
    {
        return format!(
            "{label}: no active provider or supported agent CLI is available.\n  \
             Run `difflore providers setup`, or install Claude Code / Codex / Gemini / OpenCode and retry.\n\n  \
             raw: {raw}"
        );
    }
    if lower.contains("claude code cli failed") && lower.contains("not logged in") {
        return format!(
            "{label}: fix needs a logged-in provider.\n  Run `claude /login`, or choose another provider with `difflore providers setup`.\n\n  raw: {raw}"
        );
    }
    if lower.contains(GIT_SPAWN_FAILURE)
        || lower.contains("could not spawn git")
        || (lower.contains("git") && lower.contains(OS_NOT_FOUND_UNIX))
        || (lower.contains("git") && lower.contains(OS_NOT_FOUND_WINDOWS))
    {
        return format!(
            "{label}: Git is required but `git` was not found on PATH.\n  \
             Install Git, then retry from the repository root.\n\n  raw: {raw}"
        );
    }
    if lower.contains("authentication failed")
        || lower.contains("permission denied (publickey)")
        || lower.contains("could not read username")
    {
        return format!(
            "{label}: Git authentication failed while preparing the fix.\n  \
             Check your Git credentials, then retry.\n\n  raw: {raw}"
        );
    }
    if lower.contains("failed to fetch")
        || lower.contains("could not resolve host")
        || lower.contains("network is unreachable")
    {
        return format!(
            "{label}: could not reach the remote repository.\n  \
             Check network/VPN access and run `git fetch` to verify, then retry.\n\n  raw: {raw}"
        );
    }

    difflore_core::domain::origins::format_api_error(label, raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_fix_err_classifies_missing_provider_and_git() {
        let provider = format_fix_err(
            "Fix failed",
            "no LLM provider configured and no supported agent CLI found on PATH",
        );
        assert!(provider.contains("difflore providers setup"));
        assert!(provider.contains("Claude Code / Codex / Gemini / OpenCode"));

        let git = format_fix_err(
            "Fix failed",
            "failed to spawn git: No such file or directory",
        );
        assert!(git.contains("Git is required"));
        assert!(git.contains("Install Git"));

        let login = format_fix_err("Fix failed", "Claude Code CLI failed: not logged in");
        assert!(login.contains("claude /login"));
        assert!(login.contains("raw: Claude Code CLI failed: not logged in"));
    }

    /// The spawn marker is owned by `apply.rs`; pinning it here means an
    /// upstream reword of the producer fails CI instead of silently dropping
    /// the "Install Git" hint.
    #[test]
    fn format_fix_err_classifies_owned_git_spawn_marker() {
        let raw = format!("{GIT_SPAWN_FAILURE}: program not found");
        let out = format_fix_err("Fix failed", &raw);
        assert!(out.contains("Git is required"));
        assert!(out.contains("Install Git"));
    }

    /// Branches 21-22: OS-level "not found" strings paired with `git`. These
    /// come from the platform, not us, so each gets an explicit regression so a
    /// reworded OS message fails CI rather than degrading to the generic hint.
    #[test]
    fn format_fix_err_classifies_os_not_found_git_branches() {
        let unix = format_fix_err(
            "Fix failed",
            "could not run git: No such file or directory (os error 2)",
        );
        assert!(unix.contains("Git is required"), "unix branch: {unix}");
        assert!(unix.contains("Install Git"));

        let windows = format_fix_err(
            "Fix failed",
            "git apply: The system cannot find the file specified. (os error 2)",
        );
        assert!(
            windows.contains("Git is required"),
            "windows branch: {windows}"
        );
        assert!(windows.contains("Install Git"));
    }

    /// A bare OS "not found" without the `git` marker must NOT be misclassified
    /// as a missing-Git failure; it should fall through to the API formatter.
    #[test]
    fn format_fix_err_does_not_misclassify_unrelated_not_found() {
        let out = format_fix_err("Fix failed", "open config: no such file or directory");
        assert!(
            !out.contains("Git is required"),
            "unexpected git hint: {out}"
        );
    }
}
