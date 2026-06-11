//! `difflore auth gitlab` — store / verify / remove a GitLab PAT.
//!
//! Three modes, one per flag:
//! * default — read a token (piped stdin or env) and store it encrypted,
//!   keyed by host, via the core PAT storage.
//! * `--check` — resolve the token the importer would use (env → storage)
//!   and verify it against `GET https://{host}/api/v4/user`.
//! * `--remove` — delete the stored token for the host.

use difflore_core::ingest::gitlab::auth as gitlab_auth;

use crate::style;
use crate::support::util::exit_code;

const CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);

pub(crate) async fn handle_gitlab(host: String, check: bool, remove: bool) {
    if let Err(e) = run(host, check, remove).await {
        eprintln!("{} {e}", style::err(style::sym::ERR));
        exit_code(1);
    }
}

async fn run(host_input: String, check: bool, remove: bool) -> Result<(), String> {
    let host = gitlab_auth::normalize_gitlab_host(&host_input)?;
    if remove {
        return run_remove(&host).await;
    }
    if check {
        return run_check(&host).await;
    }
    run_store(&host).await
}

async fn run_remove(host: &str) -> Result<(), String> {
    if gitlab_auth::remove_pat(host).await? {
        println!(
            "{} Removed the stored GitLab token for {host}.",
            style::ok(style::sym::OK)
        );
    } else {
        println!(
            "{} No GitLab token was stored for {host}; nothing to remove.",
            style::pewter(style::sym::BULLET)
        );
    }
    if difflore_core::infra::env::non_empty(difflore_core::infra::env::DIFFLORE_GITLAB_TOKEN)
        .is_some()
        || difflore_core::infra::env::non_empty(difflore_core::infra::env::GITLAB_TOKEN).is_some()
    {
        println!(
            "  {} A GitLab token env var is still set in this shell and will keep being used.",
            style::amber("note:")
        );
    }
    Ok(())
}

async fn run_store(host: &str) -> Result<(), String> {
    let (token, source) = read_token_for_store().ok_or_else(|| missing_token_message(host))?;
    gitlab_auth::save_pat(host, &token).await?;
    println!(
        "{} GitLab token for {host} saved (encrypted at rest).",
        style::ok(style::sym::OK)
    );
    println!("  source    {}", style::pewter(source));
    println!();
    println!(
        "  {} verify it: {}",
        style::emerald(style::sym::TIP),
        style::cmd(&check_command_hint(host)),
    );
    Ok(())
}

/// Token input for the store flow: env (explicit user intent for this shell)
/// first, then piped stdin. Never a `--token` flag and never an interactive
/// prompt — both leak into shell history / terminal scrollback too easily.
fn read_token_for_store() -> Option<(String, &'static str)> {
    if let Some(token) =
        difflore_core::infra::env::non_empty(difflore_core::infra::env::DIFFLORE_GITLAB_TOKEN)
    {
        return Some((token.trim().to_owned(), "DIFFLORE_GITLAB_TOKEN env var"));
    }
    if let Some(token) =
        difflore_core::infra::env::non_empty(difflore_core::infra::env::GITLAB_TOKEN)
    {
        return Some((token.trim().to_owned(), "GITLAB_TOKEN env var"));
    }
    use std::io::{IsTerminal, Read};
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        let mut buf = String::new();
        if stdin.lock().read_to_string(&mut buf).is_ok() {
            let trimmed = buf.trim();
            if !trimmed.is_empty() {
                return Some((trimmed.to_owned(), "piped stdin"));
            }
        }
    }
    None
}

fn missing_token_message(host: &str) -> String {
    format!(
        "GitLab token required. Supply via one of:\n  \
         1. echo \"<TOKEN>\" | difflore auth gitlab --host {host}  (recommended; stays out of shell history)\n  \
         2. DIFFLORE_GITLAB_TOKEN env var\n  \
         3. GITLAB_TOKEN env var\n\n  \
         Create one with the read_api scope at {}",
        pat_settings_url(host)
    )
}

async fn run_check(host: &str) -> Result<(), String> {
    let Some((token, source)) = gitlab_auth::resolve_token(host).await else {
        return Err(format!(
            "No GitLab token found for {host}.\n  \
             Store one first: echo \"<TOKEN>\" | difflore auth gitlab --host {host}\n  \
             Or set DIFFLORE_GITLAB_TOKEN / GITLAB_TOKEN in this shell."
        ));
    };

    let client = reqwest::Client::builder()
        .timeout(CHECK_TIMEOUT)
        .build()
        .map_err(|e| format!("Could not build the HTTP client for the check: {e}"))?;
    let url = check_url(host);
    let response = client
        .get(&url)
        .header("PRIVATE-TOKEN", token)
        .send()
        .await
        .map_err(|e| network_error_message(host, &e))?;

    let status = response.status();
    if !status.is_success() {
        return Err(check_status_error(host, status));
    }

    let username = response
        .json::<GitlabUser>()
        .await
        .ok()
        .and_then(|user| user.username);
    println!(
        "{} GitLab token verified against {host}.",
        style::ok(style::sym::OK)
    );
    if let Some(username) = username.as_deref() {
        println!("  user      {}", style::pewter(&format!("@{username}")));
    }
    println!("  source    {}", style::pewter(source.describe()));
    Ok(())
}

#[derive(Debug, serde::Deserialize)]
struct GitlabUser {
    username: Option<String>,
}

fn check_url(host: &str) -> String {
    format!("https://{host}/api/v4/user")
}

fn pat_settings_url(host: &str) -> String {
    format!("https://{host}/-/user_settings/personal_access_tokens")
}

fn check_command_hint(host: &str) -> String {
    if host == gitlab_auth::DEFAULT_GITLAB_HOST {
        "difflore auth gitlab --check".to_owned()
    } else {
        format!("difflore auth gitlab --check --host {host}")
    }
}

fn check_status_error(host: &str, status: reqwest::StatusCode) -> String {
    match status {
        reqwest::StatusCode::UNAUTHORIZED => format!(
            "GitLab rejected the token (HTTP 401 Unauthorized).\n  \
             The token is invalid, expired, revoked, or missing the read_api scope.\n  \
             Mint a new one with read_api at {}\n  \
             then re-store it: echo \"<TOKEN>\" | difflore auth gitlab --host {host}",
            pat_settings_url(host)
        ),
        reqwest::StatusCode::FORBIDDEN => format!(
            "GitLab refused the request (HTTP 403 Forbidden).\n  \
             The token authenticated but is not allowed to call /api/v4/user — \
             check the instance's IP allowlist / admin token policies, or mint a \
             fresh personal access token with the read_api scope at {}",
            pat_settings_url(host)
        ),
        other => format!(
            "GitLab returned an unexpected status for GET {} (HTTP {other}).\n  \
             If {host} sits behind a proxy or SSO page, confirm the API path is \
             reachable from this machine (curl {} works as a quick probe).",
            check_url(host),
            check_url(host),
        ),
    }
}

fn network_error_message(host: &str, e: &reqwest::Error) -> String {
    let detail = e.to_string();
    let lower = detail.to_ascii_lowercase();
    if e.is_timeout() {
        return format!(
            "Could not reach {host} within {}s.\n  \
             Check VPN/proxy access to the instance, then retry. Self-managed \
             instances often require being on the corporate network.\n\n  raw: {detail}",
            CHECK_TIMEOUT.as_secs()
        );
    }
    if lower.contains("certificate") || lower.contains("tls") || lower.contains("ssl") {
        return format!(
            "TLS handshake with {host} failed.\n  \
             Self-managed instances with a private CA need that CA trusted at the \
             OS level (difflore uses the platform certificate verifier; there is \
             no insecure-skip option).\n\n  raw: {detail}"
        );
    }
    format!(
        "Could not reach {host}.\n  \
         Check the host spelling (--host), DNS, and VPN/proxy access, then retry.\n\n  raw: {detail}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_url_targets_the_v4_user_endpoint() {
        assert_eq!(check_url("gitlab.com"), "https://gitlab.com/api/v4/user");
        assert_eq!(
            check_url("gitlab.corp.example:8443"),
            "https://gitlab.corp.example:8443/api/v4/user"
        );
    }

    #[test]
    fn unauthorized_error_names_read_api_scope_and_recovery() {
        let message = check_status_error("gitlab.com", reqwest::StatusCode::UNAUTHORIZED);
        assert!(message.contains("401"));
        assert!(message.contains("read_api"));
        assert!(message.contains("personal_access_tokens"));
        assert!(message.contains("difflore auth gitlab --host gitlab.com"));
    }

    #[test]
    fn forbidden_and_unexpected_statuses_stay_actionable() {
        let forbidden = check_status_error("gitlab.corp.example", reqwest::StatusCode::FORBIDDEN);
        assert!(forbidden.contains("403"));
        assert!(forbidden.contains("read_api"));

        let teapot = check_status_error("gitlab.corp.example", reqwest::StatusCode::IM_A_TEAPOT);
        assert!(teapot.contains("418"));
        assert!(teapot.contains("https://gitlab.corp.example/api/v4/user"));
    }

    #[test]
    fn missing_token_message_lists_stdin_and_env_paths() {
        let message = missing_token_message("gitlab.corp.example");
        assert!(
            message.contains("echo \"<TOKEN>\" | difflore auth gitlab --host gitlab.corp.example")
        );
        assert!(message.contains("DIFFLORE_GITLAB_TOKEN"));
        assert!(message.contains("GITLAB_TOKEN"));
        assert!(message.contains("read_api"));
    }

    #[test]
    fn check_hint_omits_default_host_flag() {
        assert_eq!(
            check_command_hint("gitlab.com"),
            "difflore auth gitlab --check"
        );
        assert_eq!(
            check_command_hint("gitlab.corp.example"),
            "difflore auth gitlab --check --host gitlab.corp.example"
        );
    }
}
