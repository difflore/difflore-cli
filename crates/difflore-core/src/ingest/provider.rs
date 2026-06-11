//! Review-provider identity and provider-aware remote detection.
//!
//! Foundation for multi-VCS review import. Detection is deliberately
//! conservative: `github.com` maps to GitHub, `gitlab.com` (or a host the
//! user explicitly configured for GitLab, e.g. via `difflore auth gitlab
//! --host`) maps to GitLab, and every other host returns `None` so callers
//! must require an explicit `--provider` flag instead of guessing — a wrong
//! guess would import review history from the wrong API surface.

/// Which VCS hosting provider a repo's review history is imported from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReviewProvider {
    Github,
    Gitlab,
}

impl ReviewProvider {
    /// Stable lowercase identifier, used for storage keys and `--provider`
    /// flag values.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Github => "github",
            Self::Gitlab => "gitlab",
        }
    }
}

impl std::fmt::Display for ReviewProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Extract the host (with port, when present) from a git remote URL.
///
/// Accepts the remote forms git itself accepts for hosted providers:
///   `https://host/path`, `http://host/path`,
///   `ssh://git@host/path`, `ssh://host/path`,
///   `git@host:path` (scp-like).
///
/// The host is lowercased; userinfo (`git@`) is stripped. Returns `None`
/// for local paths and anything without a recognizable host.
#[must_use]
pub fn remote_url_host(url: &str) -> Option<String> {
    let url = url.trim();

    let after_scheme = if let Some(rest) = url.strip_prefix("https://") {
        Some(rest)
    } else if let Some(rest) = url.strip_prefix("http://") {
        Some(rest)
    } else {
        url.strip_prefix("ssh://")
    };

    let Some(authority_and_path) = after_scheme else {
        // scp-like `user@host:path`. Require the `@` so a Windows drive path
        // (`C:\repo`) or a relative path with a colon never parses as a host.
        let (userinfo, rest) = url.split_once('@')?;
        if userinfo.is_empty() || userinfo.contains('/') {
            return None;
        }
        let (host, _path) = rest.split_once(':')?;
        return normalize_host(host);
    };

    let authority = authority_and_path
        .split('/')
        .next()
        .unwrap_or(authority_and_path);
    let host = authority.rsplit('@').next().unwrap_or(authority);
    normalize_host(host)
}

/// Lowercase and sanity-check a host[:port] candidate.
fn normalize_host(host: &str) -> Option<String> {
    let host = host.trim().trim_end_matches('.');
    if host.is_empty() {
        return None;
    }
    let (name, port) = host
        .split_once(':')
        .map_or((host, None), |(name, port)| (name, Some(port)));
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
    {
        return None;
    }
    if let Some(port) = port
        && (port.is_empty() || !port.chars().all(|c| c.is_ascii_digit()))
    {
        return None;
    }
    Some(host.to_ascii_lowercase())
}

/// Map a remote host to its review provider.
///
/// * `github.com` → [`ReviewProvider::Github`]
/// * `gitlab.com` or any host in `configured_gitlab_hosts` →
///   [`ReviewProvider::Gitlab`]
/// * anything else → `None` — the caller must require an explicit
///   `--provider`; we never guess a self-managed host's provider.
#[must_use]
pub fn provider_for_remote_host(
    host: &str,
    configured_gitlab_hosts: &[String],
) -> Option<ReviewProvider> {
    let host = host.trim().to_ascii_lowercase();
    if host == "github.com" {
        return Some(ReviewProvider::Github);
    }
    if host == "gitlab.com"
        || configured_gitlab_hosts
            .iter()
            .any(|configured| configured.trim().eq_ignore_ascii_case(&host))
    {
        return Some(ReviewProvider::Gitlab);
    }
    None
}

/// Provider detection straight from a git remote URL. `None` when the URL has
/// no recognizable host or the host is not attributable to a known provider.
#[must_use]
pub fn detect_provider_from_remote_url(
    url: &str,
    configured_gitlab_hosts: &[String],
) -> Option<ReviewProvider> {
    let host = remote_url_host(url)?;
    provider_for_remote_host(&host, configured_gitlab_hosts)
}

/// Validate a GitLab project path (`group/project` or
/// `group/subgroup/.../project`).
///
/// Unlike GitHub's strict two-segment `owner/repo`, GitLab namespaces nest, so
/// any depth ≥ 2 is accepted. Each segment is restricted to ASCII
/// alphanumerics plus `.`, `_`, `-` — the same whitelist as the GitHub
/// validator — so the path is safe to embed in API URLs without further
/// escaping (the caller still percent-encodes the `/` separators for the
/// GitLab REST API).
pub fn validate_gitlab_project_path(path: &str) -> Result<(), String> {
    let segments: Vec<&str> = path.split('/').collect();
    if segments.len() < 2 {
        return Err(format!(
            "invalid GitLab project path {path:?}: expected group/project (subgroups allowed, e.g. group/subgroup/project)"
        ));
    }
    if let Some(last) = segments.last()
        && last.to_ascii_lowercase().ends_with(".git")
    {
        // GitLab project paths never end in `.git`; this is almost always a
        // pasted clone URL, so name the fix instead of a generic rejection.
        return Err(format!(
            "invalid GitLab project path {path:?}: drop the trailing .git suffix"
        ));
    }
    for segment in &segments {
        if segment.is_empty() {
            return Err(format!(
                "invalid GitLab project path {path:?}: empty path segment"
            ));
        }
        if *segment == "." || *segment == ".." {
            return Err(format!(
                "invalid GitLab project path {path:?}: segment {segment:?} is not allowed"
            ));
        }
        if !segment
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
        {
            return Err(format!(
                "invalid GitLab project path {path:?}: segment {segment:?} contains disallowed characters"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_provider_identifiers_are_stable() {
        assert_eq!(ReviewProvider::Github.as_str(), "github");
        assert_eq!(ReviewProvider::Gitlab.as_str(), "gitlab");
        assert_eq!(ReviewProvider::Gitlab.to_string(), "gitlab");
    }

    #[test]
    fn remote_url_host_parses_https_ssh_and_scp_forms() {
        let cases: &[(&str, Option<&str>)] = &[
            ("https://github.com/owner/repo.git", Some("github.com")),
            ("http://gitlab.com/group/project", Some("gitlab.com")),
            ("ssh://git@gitlab.com/group/project.git", Some("gitlab.com")),
            (
                "ssh://gitlab.corp.example/group/project",
                Some("gitlab.corp.example"),
            ),
            ("git@github.com:owner/repo.git", Some("github.com")),
            (
                "git@gitlab.corp.example:group/sub/project.git",
                Some("gitlab.corp.example"),
            ),
            // Hosts keep their port (self-managed instances).
            (
                "https://gitlab.corp.example:8443/group/project",
                Some("gitlab.corp.example:8443"),
            ),
            // Case is normalized.
            ("https://GitLab.COM/group/project", Some("gitlab.com")),
            // Non-host remotes never parse.
            ("/srv/git/repo.git", None),
            ("C:\\repos\\widget", None),
            ("../relative/path", None),
            ("", None),
        ];
        for (url, expected) in cases {
            assert_eq!(remote_url_host(url).as_deref(), *expected, "url: {url}");
        }
    }

    #[test]
    fn known_hosts_map_to_their_provider() {
        assert_eq!(
            provider_for_remote_host("github.com", &[]),
            Some(ReviewProvider::Github)
        );
        assert_eq!(
            provider_for_remote_host("gitlab.com", &[]),
            Some(ReviewProvider::Gitlab)
        );
        // Case-insensitive on both sides.
        assert_eq!(
            provider_for_remote_host("GITHUB.COM", &[]),
            Some(ReviewProvider::Github)
        );
    }

    #[test]
    fn configured_gitlab_hosts_extend_detection_to_self_managed() {
        let configured = vec!["gitlab.corp.example".to_owned()];
        assert_eq!(
            provider_for_remote_host("gitlab.corp.example", &configured),
            Some(ReviewProvider::Gitlab)
        );
        assert_eq!(
            provider_for_remote_host("GitLab.Corp.Example", &configured),
            Some(ReviewProvider::Gitlab)
        );
    }

    #[test]
    fn unknown_hosts_return_none_instead_of_guessing() {
        // A self-managed GitLab host that was never configured must NOT be
        // guessed from its name — the caller asks for an explicit --provider.
        assert_eq!(provider_for_remote_host("gitlab.corp.example", &[]), None);
        assert_eq!(provider_for_remote_host("bitbucket.org", &[]), None);
        assert_eq!(provider_for_remote_host("git.sr.ht", &[]), None);
    }

    #[test]
    fn detect_provider_from_remote_url_combines_parse_and_mapping() {
        assert_eq!(
            detect_provider_from_remote_url("git@github.com:tokio-rs/tokio.git", &[]),
            Some(ReviewProvider::Github)
        );
        assert_eq!(
            detect_provider_from_remote_url("https://gitlab.com/group/sub/project.git", &[]),
            Some(ReviewProvider::Gitlab)
        );
        assert_eq!(
            detect_provider_from_remote_url(
                "https://gitlab.corp.example/group/project.git",
                &["gitlab.corp.example".to_owned()],
            ),
            Some(ReviewProvider::Gitlab)
        );
        assert_eq!(
            detect_provider_from_remote_url("https://gitea.corp.example/o/r.git", &[]),
            None
        );
        assert_eq!(
            detect_provider_from_remote_url("/srv/git/repo.git", &[]),
            None
        );
    }

    #[test]
    fn gitlab_project_paths_allow_multi_level_namespaces() {
        assert!(validate_gitlab_project_path("group/project").is_ok());
        assert!(validate_gitlab_project_path("group/subgroup/project").is_ok());
        assert!(validate_gitlab_project_path("a/b/c/d/e").is_ok());
        assert!(validate_gitlab_project_path("my-group/my_project.rs").is_ok());
    }

    #[test]
    fn gitlab_project_paths_reject_malformed_input() {
        // Single segment: a namespace alone is not a project.
        assert!(validate_gitlab_project_path("project").is_err());
        // Empty segments (leading/trailing/double slash).
        assert!(validate_gitlab_project_path("/group/project").is_err());
        assert!(validate_gitlab_project_path("group/project/").is_err());
        assert!(validate_gitlab_project_path("group//project").is_err());
        // Path traversal lookalikes.
        assert!(validate_gitlab_project_path("group/..").is_err());
        assert!(validate_gitlab_project_path("./project").is_err());
        // Shell/URL metacharacters.
        assert!(validate_gitlab_project_path("group/proj ect").is_err());
        assert!(validate_gitlab_project_path("group/proj?ect").is_err());
        assert!(validate_gitlab_project_path("group/proj#ect").is_err());
        // Pasted clone-URL suffix gets an actionable message.
        let err = validate_gitlab_project_path("group/project.git").unwrap_err();
        assert!(err.contains(".git"), "unexpected error: {err}");
    }
}
