use std::process::Command;

/// Map raw `gh` CLI / GitHub API error strings into actionable hints
/// for `difflore import-reviews`. The import path shells out to the
/// GitHub CLI (`gh api graphql ...`); errors arrive as
/// `"gh api graphql error: <stderr>"` or `"GraphQL errors: <msg>"`.
/// Same shape as `format_cloud_err`: substring match, raw retained on
/// the unrecognised path so triage info isn't lost.
pub(crate) fn format_github_import_err(label: &str, e: &str) -> String {
    let lower = e.to_ascii_lowercase();
    if e.contains("GitHub CLI (gh) is not installed") || lower.contains("gh: command not found") {
        return format!(
            "{label}: GitHub CLI (`gh`) not installed.\n  Install it: https://cli.github.com — then `gh auth login` and retry.\n  Recovery: run `difflore import-reviews --dry-run` after `gh` is ready to preview the import first."
        );
    }
    if lower.contains("bad credentials")
        || lower.contains("requires authentication")
        || lower.contains("could not authenticate")
    {
        return format!(
            "{label}: GitHub CLI auth missing or expired.\n  Run `gh auth login` (or `gh auth refresh`), then retry.\n  Recovery: no local data was changed before GitHub auth succeeded; retry with `difflore import-reviews --dry-run` to preview.\n\n  raw: {e}"
        );
    }
    if lower.contains("could not resolve to a repository")
        || lower.contains("not found")
        || lower.contains("404")
    {
        return format!(
            "{label}: repo not found or no access.\n  Verify with `gh repo view <owner>/<repo>`. Private repos need `repo` scope on your token.\n\n  raw: {e}"
        );
    }
    if lower.contains("resource not accessible")
        || lower.contains("forbidden")
        || lower.contains("403")
    {
        return format!(
            "{label}: GitHub rejected the request (403). Token likely lacks the `repo` scope.\n  Re-auth with `gh auth refresh -s repo,read:org`, then retry.\n\n  raw: {e}"
        );
    }
    if lower.contains("rate limit") || lower.contains("429") {
        return format!(
            "{label}: GitHub rate limit hit. Wait an hour, or use `gh auth login` for a higher authenticated quota.\n  Recovery: retry with a smaller window (`--max-prs 20` or `--since YYYY-MM-DD`) and keep `--dry-run` for the first check.\n\n  raw: {e}"
        );
    }
    if lower.contains("http 502")
        || lower.contains("http 503")
        || lower.contains("http 504")
        || lower.contains("bad gateway")
        || lower.contains("service unavailable")
        || lower.contains("gateway timeout")
        || lower.contains("timed out")
        || lower.contains("timeout")
        || lower.contains("connection reset")
        || lower.contains("temporarily unavailable")
        || lower.contains("something went wrong")
    {
        return format!(
            "{label}: GitHub returned a transient error after retrying.\n  Recovery: rerun the same command, or shrink the window (`--max-prs 20` or `--since YYYY-MM-DD`) if GitHub is unstable.\n\n  raw: {e}"
        );
    }
    // Network / timeout / generic fallback all share shape with the
    // cloud path — delegate to the core helper. Domain-specific GitHub
    // hints live above; the core layer is product-agnostic.
    difflore_core::origins::format_api_error(label, e)
}

pub(super) fn verify_source_repo_access(source_repo: &str) -> Result<(), String> {
    let gh = which::which("gh")
        .map_err(|e| format!("GitHub CLI (gh) is not installed or not on PATH: {e}"))?;
    let output = Command::new(gh)
        .args(["repo", "view", source_repo, "--json", "nameWithOwner"])
        .output()
        .map_err(|e| format!("GitHub CLI (gh) is not installed or not on PATH: {e}"))?;

    if output.status.success() {
        return Ok(());
    }

    Err(gh_repo_view_failure_detail(
        source_repo,
        &output.status.to_string(),
        &output.stdout,
        &output.stderr,
    ))
}

pub(super) fn gh_repo_view_failure_detail(
    source_repo: &str,
    status: &str,
    stdout: &[u8],
    stderr: &[u8],
) -> String {
    significant_gh_failure_text(stderr)
        .or_else(|| significant_gh_failure_text(stdout))
        .unwrap_or_else(|| format!("gh repo view {source_repo} failed with status {status}"))
}

fn significant_gh_failure_text(raw: &[u8]) -> Option<String> {
    let lines = String::from_utf8_lossy(raw)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| !is_gh_warning_line(line))
        .filter(|line| !looks_like_gh_json_payload(line))
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

fn is_gh_warning_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.starts_with("warning:")
        || lower.starts_with("warn:")
        || lower.starts_with("notice:")
        || lower.starts_with("! warning:")
}

fn looks_like_gh_json_payload(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with('{') || trimmed.starts_with('[')
}
