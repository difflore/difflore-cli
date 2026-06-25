//! GitLab personal-access-token storage and resolution.
//!
//! PATs are encrypted with the existing [`crate::infra::crypto`] master-key
//! mechanism and stored in the same `auth` key/value table the cloud tokens
//! use, keyed per host (`gitlab_pat::{host}`) so one machine can hold tokens
//! for gitlab.com and any number of self-managed instances at once.
//!
//! Resolution order for consumers (import client, `--check`):
//! `DIFFLORE_GITLAB_TOKEN` env → `GITLAB_TOKEN` env → encrypted storage.
//! Env wins so CI and one-off shells never need to write to disk.

use crate::cloud::client::CloudClient;
use crate::error::{CoreError, InternalResultExt as _};
use crate::infra::crypto::{decrypt_secret, encrypt_secret};

/// Default host for `difflore auth gitlab` when `--host` is omitted.
pub const DEFAULT_GITLAB_HOST: &str = "gitlab.com";

const PAT_KEY_PREFIX: &str = "gitlab_pat::";

/// Where a resolved GitLab token came from. Surfaced in `--check` output so
/// users can tell which of env/storage is actually being used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitlabTokenSource {
    /// `DIFFLORE_GITLAB_TOKEN` environment variable.
    EnvDifflore,
    /// `GITLAB_TOKEN` environment variable.
    EnvGitlab,
    /// Encrypted `auth` storage written by `difflore auth gitlab`.
    Storage,
}

impl GitlabTokenSource {
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::EnvDifflore => "DIFFLORE_GITLAB_TOKEN env var",
            Self::EnvGitlab => "GITLAB_TOKEN env var",
            Self::Storage => "encrypted local storage",
        }
    }
}

/// Normalize a user-supplied `--host` value to a bare lowercase `host[:port]`.
///
/// Accepts a pasted origin (`https://gitlab.corp.example/`) as a convenience
/// but rejects anything with a path, credentials, or non-host characters so
/// the value is safe to interpolate into `https://{host}/api/v4/...` URLs.
pub fn normalize_gitlab_host(input: &str) -> crate::Result<String> {
    let trimmed = input.trim();
    let without_scheme = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
        .unwrap_or(trimmed);
    let without_slash = without_scheme.trim_end_matches('/');
    if without_slash.is_empty() {
        return Err(CoreError::Validation(
            "GitLab host is empty; pass --host like gitlab.example.com".to_owned(),
        ));
    }
    if without_slash.contains('/') || without_slash.contains('@') {
        return Err(CoreError::Validation(format!(
            "invalid GitLab host {input:?}: pass a bare host like gitlab.example.com (no path or credentials)"
        )));
    }
    let (name, port) = without_slash
        .split_once(':')
        .map_or((without_slash, None), |(name, port)| (name, Some(port)));
    let name = name.trim_end_matches('.');
    let host_chars_ok = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
    let port_ok = port.is_none_or(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()));
    if !host_chars_ok || !port_ok {
        return Err(CoreError::Validation(format!(
            "invalid GitLab host {input:?}: pass a bare host like gitlab.example.com (optionally :port)"
        )));
    }
    Ok(match port {
        Some(port) => format!("{name}:{port}"),
        None => name.to_owned(),
    }
    .to_ascii_lowercase())
}

/// Storage key for a host's PAT in the `auth` table.
#[must_use]
pub fn pat_storage_key(host: &str) -> String {
    format!("{PAT_KEY_PREFIX}{host}")
}

/// Encrypt and persist a PAT for `host`. Overwrites any previous token for
/// the same host.
pub async fn save_pat(host: &str, token: &str) -> crate::Result<()> {
    let trimmed = token.trim();
    if trimmed.is_empty() {
        return Err(CoreError::Validation("GitLab token is empty.".to_owned()));
    }
    let encrypted = encrypt_secret(trimmed)?;
    let pool = CloudClient::auth_pool_public().await.internal()?;
    let key = pat_storage_key(host);
    sqlx::query("INSERT OR REPLACE INTO auth (key, value) VALUES (?1, ?2)")
        .bind(&key)
        .bind(encrypted)
        .execute(&pool)
        .await
        .map_err(|e| CoreError::Internal(format!("could not save GitLab token: {e}")))?;
    Ok(())
}

/// Load the stored (decrypted) PAT for `host`, if any. Decryption failures
/// surface as `None` plus a stderr warning — same contract as the cloud
/// token loader.
pub async fn load_stored_pat(host: &str) -> Option<String> {
    let pool = match CloudClient::auth_pool_public().await {
        Ok(pool) => pool,
        Err(e) => {
            eprintln!("Could not open auth storage to load GitLab token for {host}: {e}");
            return None;
        }
    };
    let raw: String = match sqlx::query_scalar("SELECT value FROM auth WHERE key = ?1")
        .bind(pat_storage_key(host))
        .fetch_optional(&pool)
        .await
    {
        Ok(Some(raw)) => raw,
        Ok(None) => return None,
        Err(e) => {
            eprintln!("Could not load stored GitLab token for {host}: {e}");
            return None;
        }
    };
    match decrypt_secret(&raw) {
        Ok(token) => Some(token),
        Err(e) => {
            eprintln!(
                "Stored GitLab token for {host} could not be decrypted: {e}. \
                 Run `difflore auth gitlab --host {host}` again to replace it."
            );
            None
        }
    }
}

/// Delete the stored PAT for `host`. Returns `true` when a token existed.
pub async fn remove_pat(host: &str) -> crate::Result<bool> {
    let pool = CloudClient::auth_pool_public().await.internal()?;
    let result = sqlx::query("DELETE FROM auth WHERE key = ?1")
        .bind(pat_storage_key(host))
        .execute(&pool)
        .await
        .map_err(|e| CoreError::Internal(format!("could not remove GitLab token: {e}")))?;
    Ok(result.rows_affected() > 0)
}

/// Resolve the token to use for `host`:
/// `DIFFLORE_GITLAB_TOKEN` → `GITLAB_TOKEN` → encrypted storage.
pub async fn resolve_token(host: &str) -> Option<(String, GitlabTokenSource)> {
    if let Some(token) = crate::infra::env::non_empty(crate::infra::env::DIFFLORE_GITLAB_TOKEN) {
        return Some((token, GitlabTokenSource::EnvDifflore));
    }
    if let Some(token) = crate::infra::env::non_empty(crate::infra::env::GITLAB_TOKEN) {
        return Some((token, GitlabTokenSource::EnvGitlab));
    }
    load_stored_pat(host)
        .await
        .map(|token| (token, GitlabTokenSource::Storage))
}

/// Hosts that have a stored PAT — these count as "configured GitLab hosts"
/// for provider-aware remote detection
/// ([`crate::ingest::provider::provider_for_remote_host`]).
pub async fn configured_hosts() -> Vec<String> {
    let pool = match CloudClient::auth_pool_public().await {
        Ok(pool) => pool,
        Err(e) => {
            eprintln!("Could not open auth storage to list configured GitLab hosts: {e}");
            return Vec::new();
        }
    };
    let keys: Vec<String> = match sqlx::query_scalar(
        "SELECT key FROM auth WHERE key LIKE 'gitlab_pat::%' ORDER BY key",
    )
    .fetch_all(&pool)
    .await
    {
        Ok(keys) => keys,
        Err(e) => {
            eprintln!("Could not list configured GitLab hosts: {e}");
            return Vec::new();
        }
    };
    keys.iter()
        .filter_map(|key| key.strip_prefix(PAT_KEY_PREFIX))
        .filter(|host| !host.is_empty())
        .filter_map(|host| normalize_gitlab_host(host).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pat_storage_key_is_host_scoped() {
        assert_eq!(pat_storage_key("gitlab.com"), "gitlab_pat::gitlab.com");
        assert_eq!(
            pat_storage_key("gitlab.corp.example:8443"),
            "gitlab_pat::gitlab.corp.example:8443"
        );
    }

    #[test]
    fn normalize_gitlab_host_accepts_bare_hosts_and_pasted_origins() {
        let cases: &[(&str, &str)] = &[
            ("gitlab.com", "gitlab.com"),
            ("GitLab.COM", "gitlab.com"),
            (" gitlab.corp.example ", "gitlab.corp.example"),
            ("gitlab.com.", "gitlab.com"),
            ("https://gitlab.corp.example", "gitlab.corp.example"),
            ("https://gitlab.corp.example/", "gitlab.corp.example"),
            (
                "https://GitLab.Corp.Example.:8443/",
                "gitlab.corp.example:8443",
            ),
            (
                "http://gitlab.corp.example:8443",
                "gitlab.corp.example:8443",
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(
                normalize_gitlab_host(input).unwrap(),
                *expected,
                "input: {input}"
            );
        }
    }

    #[test]
    fn normalize_gitlab_host_rejects_paths_credentials_and_junk() {
        for bad in [
            "",
            "   ",
            "https://gitlab.com/group/project",
            "git@gitlab.com",
            "gitlab.com:port",
            "gitlab com",
            "https://",
        ] {
            assert!(
                normalize_gitlab_host(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn token_source_descriptions_name_the_actual_origin() {
        assert!(
            GitlabTokenSource::EnvDifflore
                .describe()
                .contains("DIFFLORE_GITLAB_TOKEN")
        );
        assert!(
            GitlabTokenSource::EnvGitlab
                .describe()
                .contains("GITLAB_TOKEN")
        );
        assert!(GitlabTokenSource::Storage.describe().contains("storage"));
    }
}
