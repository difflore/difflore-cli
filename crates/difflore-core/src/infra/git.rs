use std::collections::HashMap;
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::domain::models::{
    DiffContentRecord, DiffHunkRecord, GitBranchRecord, GitBranchesInput, GitCommitInput,
    GitDiffInput, GitFileStatusRecord, GitStatusInput, GitStatusRecord,
};
use crate::error::CoreError;

fn run_git(project_path: &str, args: &[&str]) -> crate::Result<String> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(project_path)
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CoreError::Internal(format!("git error: {stderr}")));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn run_git_args(project_path: &str, args: &[String]) -> crate::Result<String> {
    let output = std::process::Command::new("git")
        .args(args.iter().map(String::as_str))
        .current_dir(project_path)
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CoreError::Internal(format!("git error: {stderr}")));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn parse_ahead_behind(line: &str) -> (i32, i32) {
    let mut ahead = 0i32;
    let mut behind = 0i32;
    if let Some(idx) = line.find("[ahead ") {
        let rest = &line[idx + 7..];
        let num: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if let Ok(n) = num.parse() {
            ahead = n;
        }
    }
    if let Some(idx) = line.find("behind ") {
        let rest = &line[idx + 7..];
        let num: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if let Ok(n) = num.parse() {
            behind = n;
        }
    }
    (ahead, behind)
}

fn parse_branch_from_status_line(line: &str) -> Option<String> {
    let rest = line.strip_prefix("## ")?;
    if rest.is_empty() {
        return None;
    }
    let branch_part = rest.split_once("...").map_or(rest, |(a, _)| a);
    let branch_part = branch_part
        .split_once('[')
        .map_or(branch_part, |(a, _)| a.trim())
        .trim();
    if branch_part.is_empty() {
        None
    } else {
        Some(branch_part.to_owned())
    }
}

fn merge_numstat(project_path: &str) -> crate::Result<HashMap<String, (i32, i32)>> {
    let mut m: HashMap<String, (i32, i32)> = HashMap::new();
    for out in [
        run_git(project_path, &["diff", "--numstat"])?,
        run_git(project_path, &["diff", "--cached", "--numstat"])?,
    ] {
        for line in out.lines() {
            if line.is_empty() {
                continue;
            }
            let mut parts = line.splitn(3, '\t');
            let add = parts.next();
            let del = parts.next();
            let path = parts.next();
            if let (Some(a), Some(d), Some(p)) = (add, del, path) {
                let adds = if a == "-" { 0 } else { a.parse().unwrap_or(0) };
                let dels = if d == "-" { 0 } else { d.parse().unwrap_or(0) };
                let e = m.entry(p.to_owned()).or_insert((0, 0));
                e.0 += adds;
                e.1 += dels;
            }
        }
    }
    Ok(m)
}

fn parse_git_diff(output: &str) -> Vec<DiffContentRecord> {
    if output.trim().is_empty() {
        return vec![];
    }
    let sections: Vec<String> = output
        .split("\ndiff --git ")
        .enumerate()
        .map(|(i, s)| {
            if i == 0 {
                s.to_owned()
            } else {
                format!("diff --git {s}")
            }
        })
        .filter(|s| !s.trim().is_empty())
        .collect();

    if sections.len() == 1 && !sections[0].starts_with("diff --git ") {
        return vec![];
    }

    let mut files = Vec::new();
    for section in sections {
        let first_line = section.lines().next().unwrap_or("");
        let file_path = parse_b_path_from_diff_git(first_line).unwrap_or_default();
        if file_path.is_empty() {
            continue;
        }
        let hunks = parse_hunks(&section);
        if hunks.is_empty() && is_binary_diff_section(&section) {
            continue;
        }
        files.push(DiffContentRecord { file_path, hunks });
    }
    files
}

fn parse_b_path_from_diff_git(first_line: &str) -> Option<String> {
    let rest = first_line.strip_prefix("diff --git ")?;
    let (_, rest) = parse_diff_path_token(rest)?;
    let (b_path, _) = parse_diff_path_token(rest.trim_start())?;
    b_path.strip_prefix("b/").map(ToOwned::to_owned)
}

fn parse_diff_path_token(input: &str) -> Option<(String, &str)> {
    let input = input.trim_start();
    if let Some(rest) = input.strip_prefix('"') {
        return parse_quoted_diff_path(rest);
    }
    let split = input.find(char::is_whitespace).unwrap_or(input.len());
    if split == 0 {
        return None;
    }
    Some((input[..split].to_owned(), &input[split..]))
}

fn parse_quoted_diff_path(input: &str) -> Option<(String, &str)> {
    let mut out = String::new();
    let mut chars = input.char_indices().peekable();
    while let Some((idx, ch)) = chars.next() {
        match ch {
            '"' => {
                let rest = &input[idx + ch.len_utf8()..];
                return Some((out, rest));
            }
            '\\' => {
                let (_, escaped) = chars.next()?;
                match escaped {
                    'n' => out.push('\n'),
                    'r' => out.push('\r'),
                    't' => out.push('\t'),
                    '\\' | '"' => out.push(escaped),
                    '0'..='7' => {
                        let mut value = escaped.to_digit(8)?;
                        for _ in 0..2 {
                            let Some((_, next)) = chars.peek().copied() else {
                                break;
                            };
                            let Some(digit) = next.to_digit(8) else {
                                break;
                            };
                            value = value * 8 + digit;
                            let _ = chars.next();
                        }
                        out.push(char::from_u32(value).unwrap_or('\u{FFFD}'));
                    }
                    other => out.push(other),
                }
            }
            other => out.push(other),
        }
    }
    None
}

fn is_binary_diff_section(section: &str) -> bool {
    section
        .lines()
        .any(|line| line.starts_with("Binary files ") || line == "GIT binary patch")
}

fn parse_hunks(section: &str) -> Vec<DiffHunkRecord> {
    let mut hunks = Vec::new();
    let mut in_hunk = false;
    let mut header = String::new();
    let mut body = String::new();
    for line in section.lines() {
        if line.starts_with("@@") {
            if in_hunk {
                hunks.push(DiffHunkRecord {
                    header: std::mem::take(&mut header),
                    body: std::mem::take(&mut body),
                });
            }
            line.clone_into(&mut header);
            in_hunk = true;
        } else if in_hunk {
            body.push_str(line);
            body.push('\n');
        }
    }
    if in_hunk {
        hunks.push(DiffHunkRecord { header, body });
    }
    hunks
}

pub async fn status(input: GitStatusInput) -> crate::Result<GitStatusRecord> {
    let out = run_git(&input.project_path, &["status", "--porcelain", "-b"])?;
    let mut branch: Option<String> = None;
    let mut ahead = 0i32;
    let mut behind = 0i32;
    let mut files: Vec<GitFileStatusRecord> = Vec::new();
    let stats = merge_numstat(&input.project_path)?;

    for line in out.lines() {
        if line.starts_with("## ") {
            branch = parse_branch_from_status_line(line);
            (ahead, behind) = parse_ahead_behind(line);
            continue;
        }
        if line.len() < 3 {
            continue;
        }
        let status = line[..2].to_string();
        let path = line[3..].trim().to_owned();
        if path.is_empty() {
            continue;
        }
        let (adds, dels) = stats.get(&path).copied().unwrap_or((0, 0));
        files.push(GitFileStatusRecord {
            path,
            status,
            additions: adds,
            deletions: dels,
        });
    }

    Ok(GitStatusRecord {
        branch,
        ahead,
        behind,
        files,
    })
}

pub async fn branches(input: GitBranchesInput) -> crate::Result<Vec<GitBranchRecord>> {
    let out = run_git(&input.project_path, &["branch", "-a"])?;
    let mut rows = Vec::new();
    for line in out.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let current = line.starts_with('*');
        let name = line.trim_start_matches('*').trim().to_owned();
        if name.is_empty() || name.contains(" -> ") {
            continue;
        }
        let remote = if name.starts_with("remotes/") {
            name.strip_prefix("remotes/").map(|s| {
                s.split_once('/')
                    .map_or_else(|| s.to_owned(), |(r, _)| r.to_owned())
            })
        } else {
            None
        };
        rows.push(GitBranchRecord {
            name,
            current,
            remote,
        });
    }
    Ok(rows)
}

/// Reject a revision/ref that git could misparse as an OPTION (argument
/// injection). Values are passed as argv (no shell injection), but git parses a
/// leading-`-` arg as a flag, so a cloud-/PR-supplied ref like `--upload-pack=…`
/// could smuggle a dangerous git flag. Legitimate revisions never begin with
/// `-` and never contain control characters, so refusing those closes the vector.
pub fn reject_option_like_revision(value: &str, what: &str) -> crate::Result<()> {
    if value.starts_with('-') {
        return Err(CoreError::Validation(format!(
            "refusing to pass {what} '{value}' to git: a leading '-' would be parsed as an option (possible argument injection)"
        )));
    }
    if value.chars().any(char::is_control) {
        return Err(CoreError::Validation(format!(
            "refusing to pass {what} to git: contains control characters"
        )));
    }
    Ok(())
}

pub async fn diff(input: GitDiffInput) -> crate::Result<Vec<DiffContentRecord>> {
    let mut args: Vec<String> = vec!["diff".into(), "--no-color".into()];
    if input.staged.unwrap_or(false) {
        args.push("--cached".into());
    }
    // Validate the user/cloud-supplied revisions before handing them to git so
    // an option-looking ref can't be parsed as a flag.
    if let Some(ref a) = input.ref1 {
        reject_option_like_revision(a, "diff revision")?;
        args.push(a.clone());
    }
    if let Some(ref b) = input.ref2 {
        reject_option_like_revision(b, "diff revision")?;
        args.push(b.clone());
    }
    let output = run_git_args(&input.project_path, &args)?;
    Ok(parse_git_diff(&output))
}

/// NOTE: git:changed events are driven by frontend mutation invalidation
/// (useGitCommit / useGitPush onSettled), not emitted from the backend.
pub async fn commit(input: GitCommitInput) -> crate::Result<()> {
    match &input.files {
        Some(files) if !files.is_empty() => {
            let mut args = vec!["add", "--"];
            let file_refs: Vec<&str> = files.iter().map(String::as_str).collect();
            args.extend(file_refs);
            run_git(&input.project_path, &args)?;
        }
        _ => {
            return Err(CoreError::Validation(
                "No files specified for commit. Please select files to stage explicitly.".into(),
            ));
        }
    }

    let now_nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let message_file = std::env::temp_dir().join(format!(
        "difflore-git-commit-message-{}-{now_nanos}.txt",
        std::process::id()
    ));
    fs::write(&message_file, &input.message).map_err(|e| {
        CoreError::Internal(format!("failed to write temporary commit message: {e}"))
    })?;

    let commit_args = vec![
        "commit".to_owned(),
        "-F".to_owned(),
        message_file.to_string_lossy().to_string(),
    ];
    let commit_result = run_git_args(&input.project_path, &commit_args);
    let _ = fs::remove_file(&message_file);
    commit_result?;
    Ok(())
}

/// Parse a GitHub remote URL into `owner/repo`.
///
/// Accepts HTTPS and SSH forms:
///   `https://github.com/owner/repo(.git)?`
///   `git@github.com:owner/repo(.git)?`
///   `ssh://git@github.com/owner/repo(.git)?`
pub fn parse_github_remote_url(url: &str) -> Option<String> {
    let url = url.trim().trim_end_matches('/');
    let stripped = if let Some(rest) = url.strip_prefix("https://github.com/") {
        rest
    } else if let Some(rest) = url.strip_prefix("git@github.com:") {
        rest
    } else {
        url.strip_prefix("ssh://git@github.com/")?
    };
    let stripped = stripped.strip_suffix(".git").unwrap_or(stripped);
    let parts: Vec<&str> = stripped.split('/').collect();
    if parts.len() == 2 && !parts[0].is_empty() && !parts[1].is_empty() {
        // GitHub treats owner/repo names case-insensitively, while our local
        // rule index compares repo scopes as strings. Normalize once at the
        // boundary so forks with mixed-case upstream remotes still recall
        // rules imported from lower-case cloud `source_repo` values.
        Some(format!("{}/{}", parts[0], parts[1]).to_ascii_lowercase())
    } else {
        None
    }
}

fn normalize_repo_scope_segments(
    value: &str,
    min_segments: usize,
    max_segments: Option<usize>,
) -> Option<String> {
    let value = value.trim().trim_end_matches('/').trim_end_matches(".git");
    let parts: Vec<&str> = value.split('/').map(str::trim).collect();
    if parts.len() < min_segments || max_segments.is_some_and(|max| parts.len() > max) {
        return None;
    }
    if parts.iter().any(|part| {
        part.is_empty()
            || *part == "."
            || *part == ".."
            || !part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
    }) {
        return None;
    }
    Some(parts.join("/").to_ascii_lowercase())
}

fn hosted_remote_parts(url: &str) -> Option<(&str, &str)> {
    let url = url.trim().trim_end_matches('/');
    if let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
    {
        let (host, path) = rest.split_once('/')?;
        return Some((host, path));
    }
    if let Some(rest) = url.strip_prefix("ssh://git@") {
        let (host, path) = rest.split_once('/')?;
        return Some((host, path));
    }
    if let Some(rest) = url.strip_prefix("git@") {
        let (host, path) = rest.split_once(':')?;
        return Some((host, path));
    }
    None
}

fn gitlab_host_allowed(host: &str, configured_gitlab_hosts: &[String]) -> bool {
    let Ok(host) = crate::ingest::gitlab::auth::normalize_gitlab_host(host) else {
        return false;
    };
    host == crate::ingest::gitlab::auth::DEFAULT_GITLAB_HOST
        || configured_gitlab_hosts.iter().any(|configured| {
            crate::ingest::gitlab::auth::normalize_gitlab_host(configured)
                .is_ok_and(|configured| configured == host)
        })
}

/// Canonicalize a GitLab repo namespace with the host dimension preserved.
///
/// GitHub keeps the legacy `owner/repo` scope. GitLab must not: `gitlab.com`
/// and self-managed instances can share the same namespace path, so their
/// canonical key is `host/group/project`.
pub fn canonical_gitlab_repo_scope(host: &str, repo_path: &str) -> Option<String> {
    let host = crate::ingest::gitlab::auth::normalize_gitlab_host(host).ok()?;

    let repo_path = repo_path
        .trim()
        .trim_end_matches('/')
        .trim_end_matches(".git");
    if let Some((first, rest)) = repo_path.split_once('/')
        && first.contains('.')
    {
        let embedded_host = crate::ingest::gitlab::auth::normalize_gitlab_host(first).ok()?;
        if embedded_host != host {
            return None;
        }
        let path = normalize_repo_scope_segments(rest, 2, None)?;
        if path.split('/').any(|segment| segment == "-") {
            return None;
        }
        return Some(format!("{host}/{path}"));
    }

    let normalized = normalize_repo_scope_segments(repo_path, 2, None)?;
    if normalized.split('/').any(|segment| segment == "-") {
        return None;
    }
    Some(format!("{host}/{normalized}"))
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RepoScope(String);

impl RepoScope {
    pub fn github(repo_full_name: &str) -> Option<Self> {
        normalize_repo_scope_segments(repo_full_name, 2, Some(2)).map(Self)
    }

    pub fn gitlab(host: &str, repo_path: &str) -> Option<Self> {
        canonical_gitlab_repo_scope(host, repo_path).map(Self)
    }

    pub fn canonical(value: &str) -> Option<Self> {
        normalize_canonical_repo_scope(value).map(Self)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl std::fmt::Display for RepoScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl AsRef<str> for RepoScope {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

pub fn canonical_source_repo(
    provider: crate::ingest::provider::ReviewProvider,
    provider_host: Option<&str>,
    repo_full_name: &str,
) -> Option<RepoScope> {
    match provider {
        crate::ingest::provider::ReviewProvider::Github => RepoScope::github(repo_full_name),
        crate::ingest::provider::ReviewProvider::Gitlab => {
            RepoScope::gitlab(provider_host?, repo_full_name)
        }
    }
}

/// Parse a supported hosted git remote URL into DiffLore's repo scope.
///
/// GitHub remotes keep the historical two-segment `owner/repo` contract.
/// GitLab remotes preserve the host dimension (`host/group/project`) so rules
/// from GitHub, gitlab.com, and self-managed GitLab instances cannot collide.
#[deprecated(note = "use parse_repo_remote_url_with_gitlab_hosts; pass configured GitLab hosts")]
pub fn parse_repo_remote_url(url: &str) -> Option<String> {
    parse_repo_remote_url_with_gitlab_hosts(url, &[])
}

pub fn parse_repo_remote_url_with_gitlab_hosts(
    url: &str,
    configured_gitlab_hosts: &[String],
) -> Option<String> {
    if let Some(repo) = parse_github_remote_url(url) {
        return Some(repo);
    }
    let (host, path) = hosted_remote_parts(url)?;
    if host.eq_ignore_ascii_case("github.com") {
        return None;
    }
    if !gitlab_host_allowed(host, configured_gitlab_hosts) {
        return None;
    }
    canonical_gitlab_repo_scope(host, path)
}

/// Normalize a provider-neutral repo scope string or supported git remote URL.
#[deprecated(note = "use normalize_repo_scope_with_gitlab_hosts; pass configured GitLab hosts")]
pub fn normalize_repo_scope(value: &str) -> Option<String> {
    normalize_repo_scope_with_gitlab_hosts(value, &[])
}

pub fn normalize_repo_scope_with_gitlab_hosts(
    value: &str,
    configured_gitlab_hosts: &[String],
) -> Option<String> {
    if let Some(repo) = parse_repo_remote_url_with_gitlab_hosts(value, configured_gitlab_hosts) {
        return Some(repo);
    }
    let value = value.trim().trim_end_matches('/').trim_end_matches(".git");
    if let Some(repo) = normalize_repo_scope_segments(value, 2, Some(2)) {
        return Some(repo);
    }
    let (host, path) = value.split_once('/')?;
    let host = crate::ingest::gitlab::auth::normalize_gitlab_host(host).ok()?;
    if !host.contains('.') || !gitlab_host_allowed(&host, configured_gitlab_hosts) {
        return None;
    }
    canonical_gitlab_repo_scope(&host, path)
}

/// Normalize a repo-scope key that already came from DiffLore storage or a
/// prior trusted detection step.
///
/// This intentionally differs from [`normalize_repo_scope`]: it accepts
/// host-prefixed canonical GitLab keys for self-managed instances without
/// consulting the auth DB. We only use it after the host dimension has already
/// been materialized, never to guess a provider from an arbitrary remote URL.
pub fn normalize_canonical_repo_scope(value: &str) -> Option<String> {
    let value = value.trim().trim_end_matches('/').trim_end_matches(".git");
    if let Some(normalized) = normalize_repo_scope_segments(value, 2, Some(2)) {
        let (owner, _) = normalized.split_once('/')?;
        return (!owner.contains('.')).then_some(normalized);
    }

    let (host, path) = value.split_once('/')?;
    let host = crate::ingest::gitlab::auth::normalize_gitlab_host(host).ok()?;
    if !host.contains('.') || host == "github.com" {
        return None;
    }
    let path = normalize_repo_scope_segments(path, 2, None)?;
    if path.split('/').any(|segment| segment == "-") {
        return None;
    }
    Some(format!("{host}/{path}"))
}

/// Normalize a GitHub `owner/repo` string or supported GitHub remote URL.
///
/// Runtime memory scoping accepts explicit MCP `repo_full_name` values as
/// well as local git remote URLs. Keeping both paths on one normalizer avoids
/// accidental global recall when an agent passes `repo_full_name` but the MCP
/// server's cwd is not the edited repository.
pub fn normalize_github_repo_full_name(value: &str) -> Option<String> {
    if let Some(repo) = parse_github_remote_url(value) {
        return Some(repo);
    }
    normalize_repo_scope_segments(value, 2, Some(2))
}

/// Best-effort repo-scope detection from local Git remotes.
///
/// Returns remotes in priority order, currently `origin` first (the repo
/// users can safely push/write outcomes to), then `upstream` as legacy
/// provenance metadata. Runtime rule recall uses the primary repo only;
/// upstream is not a cross-project widening signal.
#[deprecated(note = "use detect_repo_full_names_with_gitlab_hosts; pass configured GitLab hosts")]
pub fn detect_repo_full_names(project_path: &str) -> Vec<String> {
    detect_repo_full_names_with_gitlab_hosts(project_path, &[])
}

pub fn detect_repo_full_names_with_gitlab_hosts(
    project_path: &str,
    configured_gitlab_hosts: &[String],
) -> Vec<String> {
    let mut repos = Vec::new();
    for remote in ["origin", "upstream"] {
        let Ok(url) = run_git(project_path, &["remote", "get-url", remote]) else {
            continue;
        };
        let Some(repo) = parse_repo_remote_url_with_gitlab_hosts(&url, configured_gitlab_hosts)
        else {
            continue;
        };
        if !repos.iter().any(|existing| existing == &repo) {
            repos.push(repo);
        }
    }
    repos
}

pub fn detect_github_repo_full_names(project_path: &str) -> Vec<String> {
    let mut repos = Vec::new();
    for remote in ["origin", "upstream"] {
        let Ok(url) = run_git(project_path, &["remote", "get-url", remote]) else {
            continue;
        };
        let Some(repo) = parse_github_remote_url(&url) else {
            continue;
        };
        if !repos.iter().any(|existing| existing == &repo) {
            repos.push(repo);
        }
    }
    repos
}

/// Best-effort primary repo-scope detection from local Git remotes.
#[deprecated(note = "use detect_repo_full_names_with_gitlab_hosts; pass configured GitLab hosts")]
pub fn detect_repo_full_name(project_path: &str) -> Option<String> {
    detect_repo_full_names_with_gitlab_hosts(project_path, &[])
        .into_iter()
        .next()
}

/// Best-effort primary GitHub `owner/repo` detection from local Git remotes.
///
/// Returns `None` when not inside a git repo, when the `origin` remote is
/// missing, or when the remote URL doesn't parse as a GitHub URL. Accepts
/// both HTTPS and SSH forms:
///   `https://github.com/owner/repo(.git)?`
///   `git@github.com:owner/repo(.git)?`
///
/// Used by `run_review` to scope past-verdict recall to THIS repo's rules.
/// Non-fatal — callers return no repo-scoped recall when detection fails.
pub fn detect_github_repo_full_name(project_path: &str) -> Option<String> {
    detect_github_repo_full_names(project_path)
        .into_iter()
        .next()
}

#[cfg(test)]
mod detect_tests {
    use super::*;

    #[test]
    fn parses_supported_github_remote_urls() {
        assert_eq!(
            parse_github_remote_url("https://github.com/vitejs/vite.git").as_deref(),
            Some("vitejs/vite")
        );
        assert_eq!(
            parse_github_remote_url("git@github.com:tokio-rs/tokio.git").as_deref(),
            Some("tokio-rs/tokio")
        );
        assert_eq!(
            parse_github_remote_url("ssh://git@github.com/gin-gonic/gin").as_deref(),
            Some("gin-gonic/gin")
        );
        assert_eq!(
            parse_github_remote_url("https://github.com/TanStack/router.git").as_deref(),
            Some("tanstack/router")
        );
    }

    #[test]
    fn reject_option_like_revision_blocks_argument_injection() {
        // Option-looking revisions are refused (a real ref never starts with '-'
        // and never carries control characters), so a cloud-/PR-supplied ref
        // can't smuggle a git flag like `--upload-pack=…`.
        assert!(reject_option_like_revision("--upload-pack=evil", "ref").is_err());
        assert!(reject_option_like_revision("-foo", "ref").is_err());
        assert!(reject_option_like_revision("--output=/tmp/x", "ref").is_err());
        assert!(reject_option_like_revision("ref\nwith-newline", "ref").is_err());
        // Legitimate refs / SHAs / rev-expressions pass.
        assert!(reject_option_like_revision("HEAD", "ref").is_ok());
        assert!(reject_option_like_revision("main", "ref").is_ok());
        assert!(reject_option_like_revision("origin/feature-x", "ref").is_ok());
        assert!(reject_option_like_revision("HEAD~3", "ref").is_ok());
        // Also used to guard non-revision positional args (e.g. a clone URL).
        assert!(reject_option_like_revision("https://github.com/owner/repo.git", "url").is_ok());
        assert!(
            reject_option_like_revision("9ef0a85b2e2e4e2fbbbc02dd3bd0a57d12345678", "sha").is_ok()
        );
    }

    #[test]
    fn rejects_non_github_or_incomplete_remote_urls() {
        assert_eq!(parse_github_remote_url("https://gitlab.com/a/b.git"), None);
        assert_eq!(parse_github_remote_url("https://github.com/owner"), None);
        assert_eq!(parse_github_remote_url("git@github.com:owner/.git"), None);
    }

    #[test]
    fn parses_provider_neutral_remote_scopes() {
        assert_eq!(
            parse_repo_remote_url_with_gitlab_hosts("https://github.com/vitejs/vite.git", &[])
                .as_deref(),
            Some("vitejs/vite")
        );
        assert_eq!(
            parse_repo_remote_url_with_gitlab_hosts(
                "https://gitlab.com/group/sub/project.git",
                &[]
            )
            .as_deref(),
            Some("gitlab.com/group/sub/project")
        );
        assert_eq!(
            parse_repo_remote_url_with_gitlab_hosts("git@gitlab.com:Group/Sub/Project.git", &[])
                .as_deref(),
            Some("gitlab.com/group/sub/project")
        );
        assert_eq!(
            parse_repo_remote_url_with_gitlab_hosts(
                "ssh://git@gitlab.corp.example/platform/api.git",
                &[]
            )
            .as_deref(),
            None
        );
        assert_eq!(
            parse_repo_remote_url_with_gitlab_hosts(
                "ssh://git@gitlab.corp.example:8443/platform/api.git",
                &["gitlab.corp.example:8443".to_owned()]
            )
            .as_deref(),
            Some("gitlab.corp.example:8443/platform/api")
        );
        assert_eq!(
            parse_repo_remote_url_with_gitlab_hosts("https://bitbucket.org/acme/app.git", &[]),
            None
        );
        assert_eq!(
            parse_repo_remote_url_with_gitlab_hosts("https://github.com/owner/repo/extra.git", &[]),
            None
        );
        assert_eq!(
            parse_repo_remote_url_with_gitlab_hosts("https://example.com/owner", &[]),
            None
        );
        assert_eq!(
            parse_repo_remote_url_with_gitlab_hosts("https://example.com/owner/../repo", &[]),
            None
        );
    }

    #[test]
    fn normalizes_explicit_github_repo_full_names() {
        assert_eq!(
            normalize_github_repo_full_name("TanStack/router").as_deref(),
            Some("tanstack/router")
        );
        assert_eq!(
            normalize_github_repo_full_name("https://github.com/FastAPI/FastAPI.git").as_deref(),
            Some("fastapi/fastapi")
        );
        assert_eq!(normalize_github_repo_full_name("owner"), None);
        assert_eq!(
            normalize_github_repo_full_name("https://gitlab.com/a/b"),
            None
        );
    }

    #[test]
    fn normalizes_provider_neutral_repo_scopes() {
        assert_eq!(
            normalize_repo_scope_with_gitlab_hosts("GitLab.com/Group/Sub/Project", &[]).as_deref(),
            Some("gitlab.com/group/sub/project")
        );
        assert_eq!(
            normalize_repo_scope_with_gitlab_hosts("https://gitlab.com/Group/Sub/Project.git", &[])
                .as_deref(),
            Some("gitlab.com/group/sub/project")
        );
        assert_eq!(
            normalize_repo_scope_with_gitlab_hosts(
                "gitlab.corp.example:8443/Group/Sub/Project",
                &["gitlab.corp.example:8443".to_owned()]
            )
            .as_deref(),
            Some("gitlab.corp.example:8443/group/sub/project")
        );
        assert_eq!(
            normalize_repo_scope_with_gitlab_hosts("Group/Sub/Project", &[]).as_deref(),
            None
        );
        assert_eq!(
            normalize_repo_scope_with_gitlab_hosts("github.com/owner/repo", &[]).as_deref(),
            None
        );
        assert_eq!(
            normalize_repo_scope_with_gitlab_hosts(
                "https://gitlab.com/acme/app/-/merge_requests/1",
                &[]
            )
            .as_deref(),
            None
        );
        assert_eq!(
            normalize_repo_scope_with_gitlab_hosts("owner", &[]).as_deref(),
            None
        );
        assert_eq!(
            normalize_repo_scope_with_gitlab_hosts("owner/../repo", &[]).as_deref(),
            None
        );
    }

    #[test]
    fn normalizes_persisted_canonical_repo_scope_keys() {
        assert_eq!(
            normalize_canonical_repo_scope("Owner/Repo").as_deref(),
            Some("owner/repo")
        );
        assert_eq!(
            normalize_canonical_repo_scope("gitlab.corp.example:8443/Group/Sub/Project").as_deref(),
            Some("gitlab.corp.example:8443/group/sub/project")
        );
        assert_eq!(
            normalize_canonical_repo_scope("gitlab.com/Group/Project").as_deref(),
            Some("gitlab.com/group/project")
        );
        assert_eq!(
            normalize_canonical_repo_scope("github.com/owner/repo").as_deref(),
            None
        );
        assert_eq!(
            normalize_canonical_repo_scope("group/sub/project").as_deref(),
            None
        );
        assert_eq!(
            normalize_canonical_repo_scope("gitlab.com/acme/app/-/merge_requests/1").as_deref(),
            None
        );
    }

    #[test]
    fn repo_scope_newtype_canonicalizes_provider_source_repos() {
        assert_eq!(
            canonical_source_repo(
                crate::ingest::provider::ReviewProvider::Github,
                None,
                "Owner/Repo"
            )
            .as_ref()
            .map(RepoScope::as_str),
            Some("owner/repo")
        );
        assert_eq!(
            canonical_source_repo(
                crate::ingest::provider::ReviewProvider::Gitlab,
                Some("GitLab.Corp.Example:8443"),
                "Group/Sub/Project"
            )
            .as_ref()
            .map(RepoScope::as_str),
            Some("gitlab.corp.example:8443/group/sub/project")
        );
        assert!(
            canonical_source_repo(
                crate::ingest::provider::ReviewProvider::Gitlab,
                None,
                "Group/Sub/Project"
            )
            .is_none()
        );
    }

    #[test]
    fn unknown_host_returns_none() {
        assert_eq!(
            detect_github_repo_full_name("/nonexistent-path-for-sure"),
            None
        );
    }

    #[test]
    fn parses_quoted_diff_git_paths() {
        assert_eq!(
            parse_b_path_from_diff_git(
                "diff --git \"a/src/file with spaces.rs\" \"b/src/file with spaces.rs\""
            )
            .as_deref(),
            Some("src/file with spaces.rs")
        );
        assert_eq!(
            parse_b_path_from_diff_git(
                "diff --git \"a/src/quoted\\\"name.rs\" \"b/src/quoted\\\"name.rs\""
            )
            .as_deref(),
            Some("src/quoted\"name.rs")
        );
    }

    #[test]
    fn parse_git_diff_skips_binary_diff_without_hunks() {
        let diff = "diff --git a/logo.png b/logo.png\nindex 111..222 100644\nBinary files a/logo.png and b/logo.png differ\n";

        assert!(parse_git_diff(diff).is_empty());
    }
}
