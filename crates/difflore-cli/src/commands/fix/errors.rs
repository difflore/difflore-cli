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
            "{label}: fix needs a logged-in provider.\n  Run `claude /login`, or choose another provider with `difflore providers setup`."
        );
    }
    if lower.contains("failed to spawn git")
        || lower.contains("could not spawn git")
        || (lower.contains("git") && lower.contains("no such file or directory"))
        || (lower.contains("git") && lower.contains("the system cannot find the file specified"))
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
    }
}
