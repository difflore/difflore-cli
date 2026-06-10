//! Device registration, GitHub OAuth/token exchange, and the
//! browser/token login dispatch internals for `difflore cloud login`.
//!
//! The browser callback server itself lives in [`super::login`]; this
//! module wires the higher-level login flow (token-flag vs. browser vs.
//! GitHub CLI), persists/saves the cloud tokens, and records the
//! per-device registration state surfaced by `difflore cloud status`.
//!
//! The public entry points (`handle_login_dispatch`, `try_login_dispatch`,
//! `handle_logout`) stay in [`super`]; they call into the helpers here.

use std::path::PathBuf;

use crate::commands::cloud::login as cloud_login;
use crate::commands::providers::resolve_secret_input;
use crate::style;

const DEVICE_REGISTRATION_FILE: &str = "cloud-device-registration.json";
const CLI_CLIENT_ID: &str = "difflore-cli";
const GITHUB_CLI_TOKEN_PATH: &str = "/cli-token/github";

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct DeviceRegistrationState {
    state: String,
    host: String,
    platform: String,
    device_id: Option<String>,
    message: Option<String>,
    updated_at: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CloudGithubLoginResponse {
    token: String,
    refresh_token: Option<String>,
}

fn device_registration_state_path() -> Result<PathBuf, String> {
    Ok(difflore_core::infra::paths::data_home()?.join(DEVICE_REGISTRATION_FILE))
}

pub(super) fn load_device_registration_state() -> Option<DeviceRegistrationState> {
    let path = device_registration_state_path().ok()?;
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn save_device_registration_state(state: &DeviceRegistrationState) -> Result<(), String> {
    let path = device_registration_state_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let raw = serde_json::to_string_pretty(state).map_err(|e| e.to_string())?;
    std::fs::write(path, raw).map_err(|e| e.to_string())
}

pub(super) fn clear_device_registration_state() {
    if let Ok(path) = device_registration_state_path() {
        let _ = std::fs::remove_file(path);
    }
}

fn registered_device_state(host: &str, platform: &str, device_id: &str) -> DeviceRegistrationState {
    DeviceRegistrationState {
        state: "registered".to_owned(),
        host: host.to_owned(),
        platform: platform.to_owned(),
        device_id: Some(device_id.to_owned()),
        message: None,
        updated_at: chrono::Utc::now().to_rfc3339(),
    }
}

fn failed_device_state(host: &str, platform: &str, message: &str) -> DeviceRegistrationState {
    DeviceRegistrationState {
        state: "failed".to_owned(),
        host: host.to_owned(),
        platform: platform.to_owned(),
        device_id: None,
        message: Some(message.to_owned()),
        updated_at: chrono::Utc::now().to_rfc3339(),
    }
}

pub(super) fn device_registration_value(
    state: Option<&DeviceRegistrationState>,
) -> serde_json::Value {
    match state {
        Some(state) => serde_json::to_value(state).unwrap_or_else(|_| {
            serde_json::json!({
                "state": "unknown",
                "action": "difflore cloud login",
            })
        }),
        None => serde_json::json!({
            "state": "unknown",
            "action": "difflore cloud login",
        }),
    }
}

fn device_registration_status_line(state: Option<&DeviceRegistrationState>) -> String {
    match state {
        Some(state) if state.state == "registered" => {
            if let Some(id) = state.device_id.as_deref() {
                format!("registered ({}, id={id})", state.host)
            } else {
                format!("registered ({})", state.host)
            }
        }
        Some(state) if state.state == "failed" => {
            let detail = state
                .message
                .as_deref()
                .filter(|message| !message.trim().is_empty())
                .unwrap_or("last registration attempt failed");
            format!("registration failed: {detail}; retry: difflore cloud login")
        }
        _ => "registration unknown; retry: difflore cloud login".to_owned(),
    }
}

pub(super) fn print_device_registration_status(state: Option<&DeviceRegistrationState>) {
    let line = device_registration_status_line(state);
    let rendered = if state.is_some_and(|s| s.state == "failed") {
        style::amber(&line)
    } else {
        style::pewter(&line)
    };
    println!("  device    {rendered}");
}

pub(super) async fn try_login_dispatch_with_github(
    token_flag: Option<String>,
    force_browser: bool,
    github: bool,
) -> Result<(), String> {
    use std::io::IsTerminal;

    if github {
        return try_handle_github_login().await;
    }

    let has_flag = token_flag.as_ref().is_some_and(|s| !s.trim().is_empty());
    let has_env = difflore_core::infra::env::var(difflore_core::infra::env::DIFFLORE_CLOUD_TOKEN)
        .is_some_and(|v| !v.trim().is_empty());
    let stdin_piped = !std::io::stdin().is_terminal();
    let stdout_tty = std::io::stdout().is_terminal();

    let prefer_browser = force_browser && !has_flag && !has_env;

    if !prefer_browser && (has_flag || has_env || stdin_piped) {
        let resolved = resolve_secret_input(
            token_flag,
            "DIFFLORE_CLOUD_TOKEN",
            "cloud token",
            "difflore cloud login",
        );
        return try_handle_login(&resolved).await;
    }

    if let Some(reason) =
        browser_login_blocker(BrowserLoginSignals::detect(stdout_tty), force_browser)
    {
        return Err(browser_login_fallback_message(reason));
    }

    let api_base = difflore_core::cloud::client::CloudClient::resolve_cloud_url();
    println!(
        "{} {}",
        style::emerald(style::sym::TIP),
        style::ok("Starting browser-based cloud login")
    );
    // Spinner ticks while the blocking browser-callback worker runs. We
    // race a tick interval against the join handle so the animation stays
    // smooth without polling overhead between ticks.
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let worker_cancel = std::sync::Arc::clone(&cancel);
    let mut join = tokio::task::spawn_blocking(move || {
        cloud_login::run_browser_login_with_cancel(&api_base, &worker_cancel)
    });
    let spinner = style::Spinner::new("waiting on browser callback");
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(80));
    let result = loop {
        tokio::select! {
            _ = tick.tick() => {
                spinner.tick();
            }
            signal = tokio::signal::ctrl_c() => {
                cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                match signal {
                    Ok(()) => {
                        break join.await.unwrap_or_else(|e| Err(format!("Login worker panicked after cancellation: {e}")))
                            .and(Err("Login cancelled.".to_owned()));
                    }
                    Err(e) => {
                        break Err(format!("Failed to listen for Ctrl+C: {e}"));
                    }
                }
            }
            r = &mut join => {
                break r.unwrap_or_else(|e| Err(format!("Login worker panicked: {e}")));
            }
        }
    };

    match result {
        Ok(res) => {
            spinner.finish_ok("authorization received");
            try_handle_login_with_refresh(&res.token, res.refresh_token.as_deref()).await?;
        }
        Err(e) => {
            spinner.finish_err("browser login failed");
            return Err(e);
        }
    }
    Ok(())
}

async fn try_handle_github_login() -> Result<(), String> {
    println!(
        "{} {}",
        style::emerald(style::sym::TIP),
        style::ok("Using GitHub CLI authentication")
    );
    let github_token = read_github_cli_token().await?;
    let cloud_tokens = exchange_github_token_for_cloud_tokens(&github_token).await?;
    try_handle_login_with_refresh(&cloud_tokens.token, cloud_tokens.refresh_token.as_deref()).await
}

async fn read_github_cli_token() -> Result<String, String> {
    let gh = which::which("gh").map_err(|_| {
        github_login_recovery_message(
            "GitHub CLI (`gh`) was not found on PATH. Install it from https://cli.github.com.",
        )
    })?;
    let output = tokio::process::Command::new(gh)
        .args(["auth", "token"])
        .output()
        .await
        .map_err(|_| {
            github_login_recovery_message("Could not read GitHub CLI auth with `gh auth token`.")
        })?;

    if !output.status.success() {
        return Err(github_login_recovery_message(
            "GitHub CLI auth is missing, expired, or not usable for token exchange.",
        ));
    }

    let token = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if token.is_empty() {
        return Err(github_login_recovery_message(
            "`gh auth token` returned an empty token.",
        ));
    }
    Ok(token)
}

async fn exchange_github_token_for_cloud_tokens(
    github_token: &str,
) -> Result<CloudGithubLoginResponse, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("Could not build cloud HTTP client: {e}"))?;
    let url = github_cli_token_url(&difflore_core::cloud::client::CloudClient::resolve_cloud_url());
    let resp = client
        .post(url)
        .header("content-type", "application/json")
        .json(&serde_json::json!({
            "clientId": CLI_CLIENT_ID,
            "githubToken": github_token,
        }))
        .send()
        .await
        .map_err(|e| {
            github_login_recovery_message(&format!(
                "Could not reach DiffLore Cloud for GitHub token exchange: {e}"
            ))
        })?;

    let status = resp.status();
    if !status.is_success() {
        return Err(github_login_exchange_status_message(status));
    }

    resp.json::<CloudGithubLoginResponse>().await.map_err(|e| {
        github_login_recovery_message(&format!(
            "DiffLore Cloud returned an unreadable GitHub login response: {e}"
        ))
    })
}

fn github_cli_token_url(api_base: &str) -> String {
    format!(
        "{}{}",
        api_base.trim_end_matches('/'),
        GITHUB_CLI_TOKEN_PATH
    )
}

fn github_login_exchange_status_message(status: reqwest::StatusCode) -> String {
    let detail = match status {
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN => {
            "DiffLore Cloud could not accept the GitHub CLI auth token."
        }
        reqwest::StatusCode::NOT_FOUND => {
            "DiffLore Cloud does not expose GitHub CLI token exchange at this API URL."
        }
        _ => "DiffLore Cloud rejected the GitHub CLI token exchange.",
    };
    github_login_recovery_message(&format!("{detail} (HTTP {status})"))
}

fn github_login_recovery_message(detail: &str) -> String {
    format!(
        "{detail}\n\n  \
         Run `gh auth login`, then retry `difflore cloud login --github`.\n  \
         If GitHub is already connected in your browser, run `difflore cloud login` \
         and link your GitHub account from DiffLore Cloud first."
    )
}

async fn try_handle_login(token: &str) -> Result<(), String> {
    try_handle_login_with_refresh(token, None).await
}

async fn try_handle_login_with_refresh(
    token: &str,
    refresh_token: Option<&str>,
) -> Result<(), String> {
    let trimmed = token.trim();
    if trimmed.is_empty() {
        return Err("Token is empty.".to_owned());
    }
    let prior_token = difflore_core::cloud::client::CloudClient::load_token().await;
    let prior_refresh_token = difflore_core::cloud::client::CloudClient::load_refresh_token().await;

    if let Err(e) =
        difflore_core::cloud::client::CloudClient::save_login_tokens(trimmed, refresh_token).await
    {
        return Err(format!("Failed to save token: {e}"));
    }

    let client = difflore_core::cloud::client::CloudClient::create().await;
    let status = difflore_core::cloud::sync::fetch_cloud_status(&client).await;
    if !status.logged_in {
        let restore_result = match prior_token {
            Some(prev) => {
                difflore_core::cloud::client::CloudClient::save_login_tokens(
                    &prev,
                    prior_refresh_token.as_deref(),
                )
                .await
            }
            None => difflore_core::cloud::client::CloudClient::clear_token().await,
        };
        if let Err(e) = restore_result {
            eprintln!(
                "  {} could not restore previous token state after rejection: {e}",
                style::warn("warning:")
            );
        }
        // Cloud has no manual mint UI; tokens are only issued through
        // the browser handshake (/cli-auth). Direct the user to re-run
        // login rather than pointing at a non-existent settings page.
        return Err(
            "Token rejected by cloud (auth probe failed).\n\n  \
             Mint a fresh one by re-running `difflore cloud login` (browser flow).\n  \
             If the cloud is unreachable, retry later - local CLI features keep working offline.\n  \
             Your previous login (if any) was preserved."
                .to_owned(),
        );
    }

    println!("{} Cloud token saved.", style::ok(style::sym::OK));
    if let Some(email) = status.email.as_deref() {
        println!("  Logged in as: {}", style::pewter(email));
    }
    if let Some(plan) = status.plan.as_deref() {
        println!("  Plan:         {}", style::pewter(plan));
    }
    if let Some(team) = status.team_name.as_deref() {
        println!("  Team:         {}", style::pewter(team));
    }
    super::team::print_accepted_fix_proof_readiness(status.logged_in, status.team_name.as_deref());

    let host = difflore_core::cloud::endpoints::detect_hostname();
    let platform = difflore_core::cloud::endpoints::detect_platform();
    match difflore_core::cloud::endpoints::register(&client, &host, platform).await {
        Ok(d) => {
            println!(
                "  Device:       {} ({}, id={})",
                d.name,
                d.platform,
                style::pewter(&d.id)
            );
            if let Err(e) = save_device_registration_state(&registered_device_state(
                &d.name,
                &d.platform,
                &d.id,
            )) {
                eprintln!(
                    "  {} could not save device registration status: {e}",
                    style::warn("warning:")
                );
            }
        }
        Err(e) => {
            let message = e.to_string();
            eprintln!(
                "  {} device registration failed (login still ok): {message}",
                style::warn("warning:")
            );
            eprintln!(
                "    Retry later with {}. {} will show registration as failed until it succeeds.",
                style::cmd("difflore cloud login"),
                style::cmd("difflore cloud status"),
            );
            if let Err(save_err) =
                save_device_registration_state(&failed_device_state(&host, platform, &message))
            {
                eprintln!(
                    "  {} could not save device registration failure status: {save_err}",
                    style::warn("warning:")
                );
            }
        }
    }

    if let Ok(emitter) =
        difflore_core::cloud::observations::ObservationEmitter::open_default().await
    {
        let _ = emitter.retry_pending_uploads_now().await;
        if let Ok((attempted, confirmed)) = emitter.flush_to_cloud(&client).await
            && attempted > 0
        {
            println!(
                "  Observations: {} uploaded, {} queued",
                style::pewter(&confirmed.to_string()),
                attempted.saturating_sub(confirmed),
            );
        }
    }
    if let Some(line) =
        super::agent_usage_pending_upload_line(super::load_agent_usage_summary().await.as_ref())
    {
        println!("  Activity uploads: {}", style::pewter(&line));
    }

    println!();
    // Surface both onboarding paths: `cloud sync` for users joining an
    // existing team, `import-reviews` for a first device on a fresh team
    // that has nothing to sync yet.
    println!("  {} What's next:", style::emerald(style::sym::TIP));
    println!(
        "    {}                {}",
        style::cmd("difflore cloud sync"),
        style::pewter("joining a team - pull existing memories"),
    );
    println!(
        "    {}      {}",
        style::cmd("difflore import-reviews --upload"),
        style::pewter("first device - turn PR review history into memories"),
    );
    println!(
        "    {}                {}",
        style::cmd("difflore init --check"),
        style::pewter("see your full readiness state"),
    );
    Ok(())
}

pub(super) fn print_browser_login_recovery() {
    // There is no manual token-mint UI; retry browser login or paste an
    // existing token.
    eprintln!(
        "  Most browser-login failures are transient. Retry first:\n  \
         \n  \
         1. Re-run: difflore cloud login\n  \
         2. If your browser didn't open, ensure no firewall is blocking 127.0.0.1\n  \
         3. If you already have a token (e.g. CI secret):\n  \
            difflore cloud login --token <TOKEN>\n  \
            echo \"<TOKEN>\" | difflore cloud login\n  \
            DIFFLORE_CLOUD_TOKEN=<TOKEN> difflore cloud login"
    );
}

pub(super) fn print_github_login_recovery() {
    eprintln!(
        "  GitHub login recovery:\n  \
         \n  \
         1. Run: gh auth login\n  \
         2. Retry: difflore cloud login --github\n  \
         3. Or run: difflore cloud login and link your GitHub account in the browser"
    );
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BrowserLoginSignals {
    stdout_is_tty: bool,
    ssh_session: bool,
    graphical_forwarding: bool,
}

impl BrowserLoginSignals {
    fn detect(stdout_is_tty: bool) -> Self {
        let ssh_session = env_nonempty("SSH_CONNECTION") || env_nonempty("SSH_TTY");
        let graphical_forwarding = env_nonempty("DISPLAY")
            || env_nonempty("WAYLAND_DISPLAY")
            || env_nonempty("MIR_SOCKET");
        Self {
            stdout_is_tty,
            ssh_session,
            graphical_forwarding,
        }
    }
}

fn env_nonempty(key: &str) -> bool {
    difflore_core::infra::env::var(key).is_some_and(|value| !value.trim().is_empty())
}

const fn browser_login_blocker(
    signals: BrowserLoginSignals,
    force_browser: bool,
) -> Option<&'static str> {
    if !signals.stdout_is_tty && !force_browser {
        return Some("browser login needs an interactive terminal");
    }
    if signals.ssh_session && !signals.graphical_forwarding {
        return Some("browser login cannot complete from a headless SSH session");
    }
    None
}

fn browser_login_fallback_message(reason: &str) -> String {
    format!(
        "{reason}.\n\n  \
         The browser flow redirects to a localhost callback on the same machine that runs \
         `difflore`. In SSH/headless sessions that callback usually lands on the wrong \
         device and times out.\n\n  \
         Use one of these token paths instead:\n  \
           difflore cloud login --token <TOKEN>\n  \
           echo \"<TOKEN>\" | difflore cloud login\n  \
           DIFFLORE_CLOUD_TOKEN=<TOKEN> difflore cloud login\n\n  \
         Or run `difflore cloud login` from a terminal on the machine with the browser."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_login_blocks_headless_ssh_before_localhost_callback() {
        let reason = browser_login_blocker(
            BrowserLoginSignals {
                stdout_is_tty: true,
                ssh_session: true,
                graphical_forwarding: false,
            },
            false,
        )
        .expect("headless ssh should not start browser callback");

        let message = browser_login_fallback_message(reason);
        assert!(message.contains("headless SSH"));
        assert!(message.contains("localhost callback"));
        assert!(message.contains("difflore cloud login --token <TOKEN>"));
    }

    #[test]
    fn browser_login_allows_interactive_local_or_forwarded_sessions() {
        assert_eq!(
            browser_login_blocker(
                BrowserLoginSignals {
                    stdout_is_tty: true,
                    ssh_session: false,
                    graphical_forwarding: false,
                },
                false,
            ),
            None
        );
        assert_eq!(
            browser_login_blocker(
                BrowserLoginSignals {
                    stdout_is_tty: true,
                    ssh_session: true,
                    graphical_forwarding: true,
                },
                false,
            ),
            None
        );
        assert!(
            browser_login_blocker(
                BrowserLoginSignals {
                    stdout_is_tty: false,
                    ssh_session: false,
                    graphical_forwarding: false,
                },
                false,
            )
            .is_some()
        );
        assert_eq!(
            browser_login_blocker(
                BrowserLoginSignals {
                    stdout_is_tty: false,
                    ssh_session: false,
                    graphical_forwarding: false,
                },
                true,
            ),
            None,
            "--browser should print the auth URL and wait even from non-TTY local shells"
        );
    }

    #[test]
    fn github_cli_token_url_uses_new_cloud_endpoint() {
        assert_eq!(
            github_cli_token_url("https://difflore.dev/api"),
            "https://difflore.dev/api/cli-token/github"
        );
        assert_eq!(
            github_cli_token_url("https://difflore.dev/api/"),
            "https://difflore.dev/api/cli-token/github"
        );
    }

    #[test]
    fn github_login_response_reads_refresh_token() {
        let body: CloudGithubLoginResponse =
            serde_json::from_str(r#"{"token":"difflore-token","refreshToken":"difflore-refresh"}"#)
                .expect("github login response decodes");

        assert_eq!(body.token, "difflore-token");
        assert_eq!(body.refresh_token.as_deref(), Some("difflore-refresh"));
    }

    #[test]
    fn github_login_recovery_points_to_gh_auth_or_browser_link() {
        let message = github_login_recovery_message("GitHub auth missing");

        assert!(message.contains("gh auth login"));
        assert!(message.contains("difflore cloud login --github"));
        assert!(message.contains("difflore cloud login"));
        assert!(message.contains("link your GitHub account"));
    }

    #[test]
    fn github_login_status_error_omits_token_paths_and_suggests_recovery() {
        let message = github_login_exchange_status_message(reqwest::StatusCode::UNAUTHORIZED);

        assert!(message.contains("HTTP 401 Unauthorized"));
        assert!(message.contains("gh auth login"));
        assert!(!message.contains("--token <TOKEN>"));
        assert!(!message.contains("DIFFLORE_CLOUD_TOKEN"));
    }

    #[test]
    fn device_registration_line_does_not_claim_unknown_as_registered() {
        assert_eq!(
            device_registration_status_line(None),
            "registration unknown; retry: difflore cloud login"
        );

        let failed = failed_device_state("workstation", "windows", "API error 500");
        let line = device_registration_status_line(Some(&failed));
        assert!(line.contains("registration failed"));
        assert!(line.contains("retry: difflore cloud login"));
    }

    #[test]
    fn device_registration_json_surfaces_failed_retry_state() {
        let failed = failed_device_state("workstation", "windows", "connection refused");
        let value = device_registration_value(Some(&failed));

        assert_eq!(value["state"], "failed");
        assert_eq!(value["host"], "workstation");
        assert_eq!(value["platform"], "windows");
        assert_eq!(value["message"], "connection refused");
    }
}
