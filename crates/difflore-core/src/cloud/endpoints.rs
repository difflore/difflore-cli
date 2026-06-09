//! Single source of truth for cloud-side URLs.
//!
//! Runtime cloud API calls and browser deep links route through this
//! module. `DIFFLORE_CLOUD_URL` overrides the production API base.
//!
//! Display-only marketing labels stay outside this runtime URL table.

/// Cloud API base used when cloud URL overrides are unset.
///
/// Production host; override with `DIFFLORE_CLOUD_URL`.
pub const DEFAULT_API_BASE: &str = "https://difflore.dev/api";

/// Canonical environment variable that overrides the default base. Honoured by
/// every helper in this module.
pub const ENV_CLOUD_URL: &str = crate::env::DIFFLORE_CLOUD_URL;

/// Legacy spelling accepted as a compatibility fallback.
pub const LEGACY_ENV_CLOUD_URL: &str = crate::env::DIFF_LORE_CLOUD_URL;

/// Read the configured API base, falling back to `DEFAULT_API_BASE`.
/// Empty values are treated as unset so empty env vars do not silently break
/// clients.
pub fn api_base() -> String {
    env_api_base(ENV_CLOUD_URL)
        .or_else(|| env_api_base(LEGACY_ENV_CLOUD_URL))
        .unwrap_or_else(|| DEFAULT_API_BASE.to_owned())
}

fn env_api_base(name: &str) -> Option<String> {
    crate::env::var(name)
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

/// Browser origin derived from the API base. A trailing `/api` is
/// stripped so deep links target public web routes.
pub fn web_origin() -> String {
    web_origin_from(&api_base())
}

/// Pure variant exposed for testing — derive a web origin from any
/// API-base string.
pub fn web_origin_from(api_base: &str) -> String {
    let trimmed = api_base.trim_end_matches('/');
    trimmed
        .strip_suffix("/api")
        .map_or_else(|| trimmed.to_owned(), ToOwned::to_owned)
}

/// Scheme + authority (`scheme://host[:port]`) used to bind saved auth
/// tokens to the origin that issued them.
pub fn api_origin() -> String {
    origin_of(&api_base())
}

/// Default origin assumed for saved credentials without a stored host.
pub fn default_api_origin() -> String {
    origin_of(DEFAULT_API_BASE)
}

/// `scheme://host[:port]` from any URL, dropping the path. Keeps the
/// scheme so credentials never cross `http`/`https` boundaries.
pub fn origin_of(url: &str) -> String {
    let trimmed = url.trim();
    let Some((scheme, rest)) = trimmed.split_once("://") else {
        return trimmed.to_owned();
    };
    let authority = rest.split('/').next().unwrap_or(rest);
    // Scheme and host are case-insensitive; lowercasing avoids false
    // mismatches without merging distinct hosts.
    format!(
        "{}://{}",
        scheme.to_ascii_lowercase(),
        authority.to_ascii_lowercase()
    )
}

/// Build a full URL to a web page on the cloud origin. `path` may
/// include a leading `/`; both forms produce the same result.
pub fn web_link(path: &str) -> String {
    web_link_from(&api_base(), path)
}

/// Pure variant exposed for testing.
pub fn web_link_from(api_base: &str, path: &str) -> String {
    let base = web_origin_from(api_base);
    let path = path.trim_start_matches('/');
    if path.is_empty() {
        base
    } else {
        format!("{base}/{path}")
    }
}

/// Bare host (and port, if non-default) for display strings.
pub fn web_host_display() -> String {
    let origin = web_origin();
    origin
        .strip_prefix("https://")
        .or_else(|| origin.strip_prefix("http://"))
        .unwrap_or(&origin)
        .to_owned()
}

/// Cloud pricing page on the configured web origin.
pub fn pricing_url() -> String {
    web_link("pricing")
}

// ── GitHub repository ────────────────────────────────────────────────
//
// Central `owner/name` slug used by GitHub links and release metadata.

/// `owner/name` slug for the project's canonical GitHub repository.
pub const GITHUB_REPO: &str = "difflore/difflore-cli";

/// `https://github.com/<owner>/<name>`.
pub fn github_repo_url() -> String {
    format!("https://github.com/{GITHUB_REPO}")
}

/// `…/issues` — used in bug-report footers and the 5xx error help
/// pointer.
pub fn github_issues_url() -> String {
    format!("https://github.com/{GITHUB_REPO}/issues")
}

/// `…/releases/tag/v{version}` — emitted by the upgrade checker so
/// users can read release notes before pulling.
pub fn github_release_tag_url(version: &str) -> String {
    format!("https://github.com/{GITHUB_REPO}/releases/tag/v{version}")
}

// ── Device registration ──────────────────────────────────────────────

use openapi_contract::api;

use super::api_types::RegisterDeviceResult;
use super::client::CloudClient;

pub async fn register(
    client: &CloudClient,
    name: &str,
    platform: &str,
) -> crate::Result<RegisterDeviceResult> {
    let payload = serde_json::json!({ "name": name, "platform": platform });
    let device: RegisterDeviceResult = api!(POST "/auth/devices", body = &payload)
        .fetch(client)
        .await?;
    Ok(device)
}

pub const fn detect_platform() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    }
}

pub fn detect_hostname() -> String {
    for key in ["COMPUTERNAME", "HOSTNAME", "HOST"] {
        if let Some(name) = crate::env::var(key) {
            return name;
        }
    }
    // Unix fallback for shells that do not export a hostname.
    if !cfg!(target_os = "windows")
        && let Ok(out) = std::process::Command::new("hostname").output()
        && out.status.success()
    {
        let name = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        if !name.is_empty() {
            return name;
        }
    }
    "unknown-device".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_origin_strips_trailing_api_segment() {
        assert_eq!(
            web_origin_from("https://difflore.dev/api"),
            "https://difflore.dev"
        );
        assert_eq!(
            web_origin_from("https://difflore.dev/api/"),
            "https://difflore.dev"
        );
    }

    #[test]
    fn web_origin_passes_through_when_no_api_suffix() {
        assert_eq!(
            web_origin_from("https://difflore.dev"),
            "https://difflore.dev"
        );
    }

    #[test]
    fn origin_of_keeps_scheme_and_authority_drops_path() {
        assert_eq!(
            origin_of("https://difflore.dev/api"),
            "https://difflore.dev"
        );
        assert_eq!(
            origin_of("https://difflore.dev/api/"),
            "https://difflore.dev"
        );
        assert_eq!(
            origin_of("http://127.0.0.1:3017/api"),
            "http://127.0.0.1:3017"
        );
        assert_eq!(
            origin_of("https://staging.example.com:8443/api/v1"),
            "https://staging.example.com:8443"
        );
        assert_eq!(
            origin_of(" https://difflore.dev/api "),
            "https://difflore.dev"
        );
    }

    #[test]
    fn origin_of_distinguishes_scheme_and_host_so_tokens_cannot_cross() {
        // Saved credentials must not cross scheme or host boundaries.
        assert_ne!(
            origin_of("https://difflore.dev/api"),
            origin_of("http://difflore.dev/api")
        );
        assert_ne!(
            origin_of("https://difflore.dev/api"),
            origin_of("https://attacker.example/api")
        );
    }

    #[test]
    fn origin_of_is_case_insensitive_on_scheme_and_host() {
        // A benign case difference must not lock a user out of their own token.
        assert_eq!(
            origin_of("HTTPS://Difflore.DEV/api"),
            origin_of("https://difflore.dev/api"),
        );
        assert_eq!(
            origin_of("HTTPS://Difflore.DEV/api"),
            "https://difflore.dev"
        );
    }

    #[test]
    fn default_api_origin_is_the_production_host() {
        assert_eq!(default_api_origin(), "https://difflore.dev");
    }

    #[test]
    fn web_link_handles_leading_slash_either_way() {
        assert_eq!(
            web_link_from("https://difflore.dev/api", "/pricing"),
            "https://difflore.dev/pricing"
        );
        assert_eq!(
            web_link_from("https://difflore.dev/api", "pricing"),
            "https://difflore.dev/pricing"
        );
        assert_eq!(
            web_link_from("https://difflore.dev/api", ""),
            "https://difflore.dev"
        );
    }

    #[test]
    fn api_base_honors_canonical_cloud_url_env() {
        temp_env::with_vars(
            [
                (ENV_CLOUD_URL, Some(" http://127.0.0.1:3017/api ")),
                (LEGACY_ENV_CLOUD_URL, None),
            ],
            || {
                assert_eq!(api_base(), "http://127.0.0.1:3017/api");
            },
        );
    }

    #[test]
    fn api_base_honors_legacy_cloud_url_env() {
        temp_env::with_vars(
            [
                (ENV_CLOUD_URL, None),
                (LEGACY_ENV_CLOUD_URL, Some(" http://127.0.0.1:3018/api ")),
            ],
            || {
                assert_eq!(api_base(), "http://127.0.0.1:3018/api");
            },
        );
    }

    #[test]
    fn canonical_cloud_url_env_wins_over_legacy() {
        temp_env::with_vars(
            [
                (ENV_CLOUD_URL, Some("http://127.0.0.1:3017/api")),
                (LEGACY_ENV_CLOUD_URL, Some("http://127.0.0.1:3018/api")),
            ],
            || {
                assert_eq!(api_base(), "http://127.0.0.1:3017/api");
            },
        );
    }
}
