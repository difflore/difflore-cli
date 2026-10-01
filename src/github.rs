use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

const CACHE_TTL_SECS: u64 = 24 * 60 * 60;
const MAX_BODY_CHARS: usize = 700;
const MIN_BODY_CHARS: usize = 15;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repo {
    pub owner: String,
    pub name: String,
}

impl std::fmt::Display for Repo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.owner, self.name)
    }
}

impl Repo {
    pub fn parse(input: &str) -> Option<Self> {
        let s = input.trim().trim_end_matches('/');
        let s = s.strip_suffix(".git").unwrap_or(s);
        let s = s
            .strip_prefix("https://github.com/")
            .or_else(|| s.strip_prefix("http://github.com/"))
            .or_else(|| s.strip_prefix("git@github.com:"))
            .or_else(|| s.strip_prefix("ssh://git@github.com/"))
            .or_else(|| s.strip_prefix("github.com/"))
            .unwrap_or(s);
        let mut parts = s.split('/');
        let owner = parts.next()?.trim();
        let name = parts.next()?.trim();
        if owner.is_empty() || name.is_empty() || owner.contains(':') {
            return None;
        }
        Some(Self {
            owner: owner.to_owned(),
            name: name.to_owned(),
        })
    }

    pub fn from_git_remote() -> Result<Self> {
        let out = Command::new("git")
            .args(["remote", "get-url", "origin"])
            .output()
            .context("could not run git")?;
        if !out.status.success() {
            bail!("no repo given and this directory has no `origin` remote");
        }
        let url = String::from_utf8_lossy(&out.stdout);
        Self::parse(&url).with_context(|| format!("`origin` is not a GitHub repo: {}", url.trim()))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Comment {
    pub pr: u64,
    pub author: String,
    pub path: String,
    pub line: String,
    pub body: String,
    pub url: String,
    pub created_at: String,
}

#[derive(Deserialize)]
struct RawComment {
    body: String,
    path: Option<String>,
    html_url: String,
    pull_request_url: String,
    in_reply_to_id: Option<u64>,
    author_association: String,
    user: Option<RawUser>,
    diff_hunk: Option<String>,
    created_at: String,
}

#[derive(Deserialize)]
struct RawUser {
    login: String,
    #[serde(rename = "type")]
    kind: String,
}

pub struct FetchOptions {
    pub limit: usize,
    pub all_reviewers: bool,
    pub refresh: bool,
}

#[derive(Serialize, Deserialize)]
struct Cache {
    fetched_at: u64,
    limit: usize,
    all_reviewers: bool,
    comments: Vec<Comment>,
}

pub fn fetch_comments(
    repo: &Repo,
    opts: &FetchOptions,
    progress: &dyn Fn(&str),
) -> Result<Vec<Comment>> {
    let cache_path = cache_path(repo);
    if !opts.refresh
        && let Some(cached) = read_cache(cache_path.as_ref(), opts)
    {
        progress(&format!(
            "Using {} cached review comments (--refresh to refetch)",
            cached.len()
        ));
        return Ok(cached);
    }

    let token = github_token();
    if token.is_none() {
        progress(
            "No GitHub token found (GITHUB_TOKEN or `gh auth login`); using the 60 requests/hour anonymous limit",
        );
    }

    let max_pages = (opts.limit / 100 + 1) * 4;
    let mut kept = Vec::new();
    let mut seen = 0usize;
    for page in 1..=max_pages {
        let url = format!(
            "https://api.github.com/repos/{}/{}/pulls/comments?sort=created&direction=desc&per_page=100&page={page}",
            repo.owner, repo.name
        );
        let batch = get_page(&url, token.as_deref())
            .with_context(|| format!("GitHub request failed for {repo}"))?;
        if batch.is_empty() {
            break;
        }
        seen += batch.len();
        kept.extend(
            batch
                .into_iter()
                .filter_map(|c| keep(c, opts.all_reviewers)),
        );
        progress(&format!(
            "Fetched {seen} review comments, kept {}",
            kept.len()
        ));
        if kept.len() >= opts.limit {
            kept.truncate(opts.limit);
            break;
        }
    }

    if let Some(path) = cache_path {
        write_cache(&path, opts, &kept);
    }
    Ok(kept)
}

fn get_page(url: &str, token: Option<&str>) -> Result<Vec<RawComment>> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        match get_page_once(url, token) {
            Err(e) if attempt < 3 && is_transient(&e) => {
                std::thread::sleep(std::time::Duration::from_secs(2 * attempt));
            }
            other => return other,
        }
    }
}

fn is_transient(e: &anyhow::Error) -> bool {
    !matches!(
        e.downcast_ref::<ureq::Error>(),
        Some(ureq::Error::StatusCode(code)) if *code < 500
    ) && !e.to_string().contains("rate limit")
        && !e.to_string().contains("repo not found")
}

fn get_page_once(url: &str, token: Option<&str>) -> Result<Vec<RawComment>> {
    let mut req = ureq::get(url)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "difflore-cli")
        .header("X-GitHub-Api-Version", "2022-11-28");
    if let Some(token) = token {
        req = req.header("Authorization", &format!("Bearer {token}"));
    }
    match req.call() {
        Ok(mut resp) => Ok(resp.body_mut().read_json()?),
        Err(ureq::Error::StatusCode(404)) => {
            bail!("repo not found, or it is private and no token with access was found")
        }
        Err(ureq::Error::StatusCode(403 | 429)) => {
            bail!("GitHub rate limit hit; set GITHUB_TOKEN or run `gh auth login`")
        }
        Err(e) => Err(e.into()),
    }
}

fn keep(c: RawComment, all_reviewers: bool) -> Option<Comment> {
    if c.in_reply_to_id.is_some() {
        return None;
    }
    let user = c.user?;
    if user.kind == "Bot" || user.login.ends_with("[bot]") {
        return None;
    }
    if !all_reviewers
        && !matches!(
            c.author_association.as_str(),
            "OWNER" | "MEMBER" | "COLLABORATOR"
        )
    {
        return None;
    }
    let body = clean_body(&c.body);
    if body.chars().count() < MIN_BODY_CHARS {
        return None;
    }
    let pr = c.pull_request_url.rsplit('/').next()?.parse().ok()?;
    let line = c
        .diff_hunk
        .as_deref()
        .and_then(|h| h.lines().last())
        .map(|l| l.chars().take(160).collect::<String>())
        .unwrap_or_default();
    Some(Comment {
        pr,
        author: user.login,
        path: c.path.unwrap_or_default(),
        line,
        body,
        url: c.html_url,
        created_at: c.created_at,
    })
}

fn clean_body(body: &str) -> String {
    let collapsed = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() > MAX_BODY_CHARS {
        let mut s: String = collapsed.chars().take(MAX_BODY_CHARS).collect();
        s.push('…');
        s
    } else {
        collapsed
    }
}

fn github_token() -> Option<String> {
    for var in ["GITHUB_TOKEN", "GH_TOKEN"] {
        if let Ok(v) = std::env::var(var)
            && !v.trim().is_empty()
        {
            return Some(v.trim().to_owned());
        }
    }
    let out = Command::new("gh").args(["auth", "token"]).output().ok()?;
    let token = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (out.status.success() && !token.is_empty()).then_some(token)
}

fn cache_path(repo: &Repo) -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    Some(
        base.join("difflore")
            .join(format!("{}__{}.json", repo.owner, repo.name)),
    )
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn read_cache(path: Option<&PathBuf>, opts: &FetchOptions) -> Option<Vec<Comment>> {
    let raw = std::fs::read_to_string(path?).ok()?;
    let cache: Cache = serde_json::from_str(&raw).ok()?;
    let fresh = now().saturating_sub(cache.fetched_at) < CACHE_TTL_SECS;
    (fresh && cache.limit == opts.limit && cache.all_reviewers == opts.all_reviewers)
        .then_some(cache.comments)
}

fn write_cache(path: &PathBuf, opts: &FetchOptions, comments: &[Comment]) {
    let cache = Cache {
        fetched_at: now(),
        limit: opts.limit,
        all_reviewers: opts.all_reviewers,
        comments: comments.to_vec(),
    };
    if let Some(dir) = path.parent()
        && std::fs::create_dir_all(dir).is_ok()
        && let Ok(json) = serde_json::to_string(&cache)
    {
        let _ = std::fs::write(path, json);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_repo_forms() {
        let want = Some(Repo {
            owner: "rust-lang".into(),
            name: "cargo".into(),
        });
        for input in [
            "rust-lang/cargo",
            "https://github.com/rust-lang/cargo",
            "https://github.com/rust-lang/cargo.git\n",
            "git@github.com:rust-lang/cargo.git",
            "github.com/rust-lang/cargo/",
        ] {
            assert_eq!(Repo::parse(input), want, "{input}");
        }
        assert_eq!(Repo::parse("cargo"), None);
    }

    fn raw(assoc: &str, login: &str, kind: &str, reply: Option<u64>, body: &str) -> RawComment {
        RawComment {
            body: body.into(),
            path: Some("src/lib.rs".into()),
            html_url: "https://github.com/o/r/pull/7#discussion_r1".into(),
            pull_request_url: "https://api.github.com/repos/o/r/pulls/7".into(),
            in_reply_to_id: reply,
            author_association: assoc.into(),
            user: Some(RawUser {
                login: login.into(),
                kind: kind.into(),
            }),
            diff_hunk: Some("@@ -1 +1 @@\n+let x = foo();".into()),
            created_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    #[test]
    fn keeps_only_top_level_maintainer_comments() {
        let body = "Please return a Result here instead of panicking.";
        let kept = keep(raw("MEMBER", "alice", "User", None, body), false).expect("kept");
        assert_eq!(kept.pr, 7);
        assert_eq!(kept.line, "+let x = foo();");

        assert!(keep(raw("MEMBER", "alice", "User", Some(1), body), false).is_none());
        assert!(keep(raw("MEMBER", "ci[bot]", "Bot", None, body), false).is_none());
        assert!(keep(raw("CONTRIBUTOR", "bob", "User", None, body), false).is_none());
        assert!(keep(raw("CONTRIBUTOR", "bob", "User", None, body), true).is_some());
        assert!(keep(raw("OWNER", "alice", "User", None, "nit: typo"), false).is_none());
    }
}
