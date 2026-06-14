use openapi_contract::sse::SseStream;
use openapi_contract::{ApiClient, ApiError, Method};
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use std::collections::HashMap;
use std::path::PathBuf;

/// Whether `DIFFLORE_DEBUG_CLOUD=1` is set. Off by default so degraded
/// cloud paths stay silent (each returns an empty/false sentinel); the
/// raw reqwest error would only confuse users who already see a friendlier
/// top-level message. Devs flip it on to trace transport issues.
fn cloud_debug_enabled() -> bool {
    crate::infra::env::debug_cloud()
}

static AUTH_POOL_CACHE: tokio::sync::Mutex<Option<HashMap<PathBuf, SqlitePool>>> =
    tokio::sync::Mutex::const_new(None);

const AUTH_TOKEN_KEY: &str = "token";
const AUTH_REFRESH_TOKEN_KEY: &str = "refresh_token";
/// Cloud origin a saved token / refresh-token was issued for.
const AUTH_HOST_KEY: &str = "token_host";
const CLI_CLIENT_ID: &str = "difflore-cli";
const PAST_VERDICT_RECALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);
const PAST_VERDICT_RETRY_DELAYS_MS: &[u64] = &[100, 300, 700];

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct TokenRefreshResponse {
    token: String,
    refresh_token: Option<String>,
}

/// Which attempt of [`CloudClient::send_with_refresh`] produced a
/// transport error.
#[derive(Clone, Copy)]
enum SendPhase {
    Initial,
    Retry,
}

/// Percent-encode characters that would break routing when an id is
/// interpolated into one URL path segment.
fn escape_path_id(s: &str) -> String {
    s.replace('%', "%25")
        .replace('/', "%2F")
        .replace('#', "%23")
        .replace('?', "%3F")
}

/// Replace every known auth token (≥8 chars) in a cloud-response body
/// before it is logged or surfaced, so a compromised cloud echoing the
/// bearer/refresh token back can't leak it to stderr or an error string.
/// Tokens under 8 chars are skipped to avoid redacting common substrings.
fn scrub_tokens_from_body(body: &str, tokens: &[Option<&str>]) -> String {
    let mut out = body.to_owned();
    for &token in tokens.iter().flatten() {
        if token.len() >= 8 {
            out = out.replace(token, "[REDACTED-TOKEN]");
        }
    }
    out
}

fn truncate_for_error(body: &str, max_chars: usize) -> String {
    if body.chars().count() <= max_chars {
        return body.to_owned();
    }
    body.chars().take(max_chars).collect()
}

/// Fingerprint of an upload that returned a non-2xx HTTP response, held
/// by [`OutboxFailure::Http`] so the outbox can write a greppable
/// `last_error` (e.g. `"401 Unauthorized: …"`).
///
/// `body_snippet` is the first ~200 chars of the body, whitespace
/// collapsed and known auth tokens scrubbed. It is written only into the
/// local SQLite `last_error` column, never to stdout/stderr, so a cloud
/// stack-trace echo cannot leak into foreground output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HttpFailure {
    pub status: u16,
    pub reason_phrase: String,
    pub body_snippet: String,
}

/// Outcome of a fire-and-forget POST whose dispatcher wants status +
/// body details. The two variants let the outbox distinguish "cloud
/// rejected this with 401" from "we never reached the cloud" — collapsing
/// them would hide stale-auth incidents in a generic transport bucket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OutboxFailure {
    Http(HttpFailure),
    /// Transport / DNS / TLS / timeout — the request never produced a
    /// status code. Payload is the `reqwest::Error` description.
    Transport(String),
}

impl OutboxFailure {
    /// Format the failure for the `cloud_outbox.last_error` column.
    ///
    /// * `Http(401, "Unauthorized", "{…}")` → `"401 Unauthorized: {…}"`
    /// * `Http(500, "Internal Server Error", "")` → `"500 Internal Server Error"`
    /// * `Transport("…connection refused…")` → `"transport: …connection refused…"`
    ///
    /// The shape is greppable: `last_error LIKE '4__ %'` matches every
    /// client-class HTTP failure and the leading status code parses easily.
    pub fn format_for_outbox_last_error(&self) -> String {
        match self {
            Self::Http(http) => {
                if http.body_snippet.is_empty() {
                    format!("{} {}", http.status, http.reason_phrase)
                } else {
                    format!(
                        "{} {}: {}",
                        http.status, http.reason_phrase, http.body_snippet
                    )
                }
            }
            // Transport-class sentinel, distinct from any
            // `{status_code} {reason}` shape so callers can tell them apart.
            Self::Transport(msg) => format!("transport: {msg}"),
        }
    }
}

/// Collapse all ASCII whitespace runs to a single space and truncate to
/// at most `max_chars` Unicode scalar values (never splitting a
/// codepoint). Keeps an HTTP body fragment on a single grep line in
/// `last_error`.
pub(crate) fn normalize_body_snippet(body: &str, max_chars: usize) -> String {
    let collapsed: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= max_chars {
        return collapsed;
    }
    collapsed.chars().take(max_chars).collect()
}

use crate::context::types::PastVerdict;
use crate::infra::crypto::{decrypt_secret, encrypt_secret};

use super::endpoints::pricing_url;

use crate::contract::{
    GetTrajectoryResponse, ImpactBannerDto, ImpactCoverageDto, ImpactFixScorecardDto,
    ImpactTopRulesDto, ImpactWeeklyDto, PastVerdictDto, RecallPastVerdictsRequest,
    RecordAcceptedEditRequest, RecordAcceptedEditResponse, RecordReviewMetricsRequest,
    SaveTrajectoryRequest, UploadImportedReviewsRequest,
};

#[derive(Clone)]
pub struct CloudClient {
    client: reqwest::Client,
    base_url: String,
    token: Option<String>,
}

impl Default for CloudClient {
    fn default() -> Self {
        Self::new()
    }
}

impl CloudClient {
    #[allow(clippy::panic)]
    // reason: reqwest client construction with a static timeout is unrecoverable for CLI startup.
    pub fn new() -> Self {
        let base_url = Self::resolve_cloud_url();
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .unwrap_or_else(|e| {
                    panic!("failed to build cloud HTTP client with 30s timeout: {e}")
                }),
            base_url,
            token: None,
        }
    }

    pub async fn create() -> Self {
        let mut client = Self::new();
        client.token = Self::load_token().await;
        client
    }

    pub fn resolve_cloud_url() -> String {
        super::endpoints::api_base()
    }

    fn auth_db_path() -> crate::Result<PathBuf> {
        // Route through `paths::data_home()` so `DIFFLORE_HOME` controls
        // tests and self-host setups.
        Ok(crate::infra::paths::data_home()?.join("cloud-auth.db"))
    }

    pub async fn auth_pool() -> crate::Result<SqlitePool> {
        let path = Self::auth_db_path()?;
        let mut guard = AUTH_POOL_CACHE.lock().await;
        let cache = guard.get_or_insert_with(HashMap::new);
        if let Some(pool) = cache.get(&path) {
            return Ok(pool.clone());
        }

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(crate::CoreError::Io)?;
            crate::infra::db::restrict_to_owner(parent, true);
        }

        let opts = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true);

        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .map_err(crate::CoreError::Database)?;

        // cloud-auth.db holds the (encrypted) auth token — restrict it to the
        // owner on Unix (Windows relies on the per-user profile ACL).
        crate::infra::db::restrict_sqlite_files(&path);

        // cloud-auth.db is a two-column token store; create its one table
        // idempotently.
        sqlx::query!(
            "CREATE TABLE IF NOT EXISTS auth (\
                key TEXT PRIMARY KEY NOT NULL, \
                value TEXT NOT NULL\
            )"
        )
        .execute(&pool)
        .await
        .map_err(|e| crate::CoreError::Internal(format!("auth table create failed: {e}")))?;

        cache.insert(path, pool.clone());
        Ok(pool)
    }

    pub async fn auth_pool_public() -> crate::Result<SqlitePool> {
        Self::auth_pool().await
    }

    async fn save_encrypted_auth_key(key: &str, value: &str) -> crate::Result<()> {
        let encrypted = encrypt_secret(value)?;
        let pool = Self::auth_pool().await?;
        sqlx::query("INSERT OR REPLACE INTO auth (key, value) VALUES (?1, ?2)")
            .bind(key)
            .bind(encrypted)
            .execute(&pool)
            .await
            .map_err(crate::CoreError::Database)?;
        Ok(())
    }

    async fn load_encrypted_auth_key(key: &str, quiet: bool) -> Option<String> {
        let pool = Self::auth_pool().await.ok()?;
        let raw: String = sqlx::query_scalar("SELECT value FROM auth WHERE key = ?1")
            .bind(key)
            .fetch_optional(&pool)
            .await
            .ok()??;

        match decrypt_secret(&raw) {
            Ok(plaintext) => Some(plaintext),
            Err(e) => {
                // `quiet` suppresses the warning for read-only diagnostics
                // (e.g. the embedder status probe, which the TUI polls every
                // 500ms) so a corrupt token doesn't spam stderr and corrupt the
                // interactive display. Real cloud/recall calls keep the warning.
                if !quiet {
                    eprintln!(
                        "Token storage could not be decrypted: {e}. \
                         DiffLore left the stored token untouched; set DIFFLORE_MASTER_KEY if this is CI, \
                         or run `difflore cloud logout` then `difflore cloud login` to replace it."
                    );
                }
                None
            }
        }
    }

    async fn delete_auth_key(key: &str) -> crate::Result<()> {
        let pool = Self::auth_pool().await.map_err(|e| e.to_string())?;
        sqlx::query("DELETE FROM auth WHERE key = ?1")
            .bind(key)
            .execute(&pool)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub async fn save_token(token: &str) -> crate::Result<()> {
        Self::save_encrypted_auth_key(AUTH_TOKEN_KEY, token).await?;
        let pool = Self::auth_pool().await.map_err(|e| e.to_string())?;
        sqlx::query!("DELETE FROM auth WHERE key = 'login_nonce'")
            .execute(&pool)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub async fn save_refresh_token(refresh_token: &str) -> crate::Result<()> {
        Self::save_encrypted_auth_key(AUTH_REFRESH_TOKEN_KEY, refresh_token).await
    }

    pub async fn save_login_tokens(token: &str, refresh_token: Option<&str>) -> crate::Result<()> {
        Self::save_token(token).await?;
        match refresh_token.map(str::trim).filter(|s| !s.is_empty()) {
            Some(refresh_token) => Self::save_refresh_token(refresh_token).await?,
            None => Self::delete_auth_key(AUTH_REFRESH_TOKEN_KEY).await?,
        }
        // Bind saved credentials to the host that issued them.
        Self::save_encrypted_auth_key(AUTH_HOST_KEY, &super::endpoints::api_origin()).await?;
        Ok(())
    }

    pub async fn load_token() -> Option<String> {
        // An explicit env token is the user's own intent for the current URL.
        if let Some(token) = crate::infra::env::non_empty(crate::infra::env::DIFFLORE_TOKEN) {
            return Some(token);
        }

        let token = Self::load_encrypted_auth_key(AUTH_TOKEN_KEY, false).await?;
        Self::saved_credential_host_matches_current(false)
            .await
            .then_some(token)
    }

    /// Like [`load_token`] but never writes a decrypt-failure warning to stderr.
    /// Use from read-only diagnostics / render loops (e.g. the embedder status
    /// probe) where a corrupt token must not spam the terminal.
    pub async fn load_token_quiet() -> Option<String> {
        if let Some(token) = crate::infra::env::non_empty(crate::infra::env::DIFFLORE_TOKEN) {
            return Some(token);
        }

        let token = Self::load_encrypted_auth_key(AUTH_TOKEN_KEY, true).await?;
        Self::saved_credential_host_matches_current(true)
            .await
            .then_some(token)
    }

    pub async fn load_refresh_token() -> Option<String> {
        let token = Self::load_encrypted_auth_key(AUTH_REFRESH_TOKEN_KEY, false).await?;
        Self::saved_credential_host_matches_current(false)
            .await
            .then_some(token)
    }

    /// Saved cloud credentials are attached only when the configured
    /// origin matches the stored issuing origin. Credentials without a
    /// stored host are assumed to belong to the default production origin.
    async fn saved_credential_host_matches_current(quiet: bool) -> bool {
        let current = super::endpoints::api_origin();
        match Self::load_encrypted_auth_key(AUTH_HOST_KEY, quiet).await {
            Some(stored) => stored == current,
            None => current == super::endpoints::default_api_origin(),
        }
    }

    pub async fn clear_token() -> crate::Result<()> {
        Self::delete_auth_key(AUTH_TOKEN_KEY).await?;
        Self::delete_auth_key(AUTH_REFRESH_TOKEN_KEY).await?;
        Self::delete_auth_key(AUTH_HOST_KEY).await
    }

    pub async fn refresh_saved_token() -> Option<String> {
        let refresh_token = Self::load_refresh_token().await?;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .ok()?;
        let url = format!(
            "{}/token/refresh",
            Self::resolve_cloud_url().trim_end_matches('/')
        );
        let resp = match client
            .post(url)
            .header("content-type", "application/json")
            .json(&serde_json::json!({
                "clientId": CLI_CLIENT_ID,
                "refreshToken": refresh_token,
            }))
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(e) => {
                if cloud_debug_enabled() {
                    eprintln!("[cloud-client] token refresh network error: {e}");
                }
                return None;
            }
        };
        let status = resp.status();
        if !status.is_success() {
            if cloud_debug_enabled() {
                let raw = resp.text().await.unwrap_or_default();
                // The /token/refresh body could echo the refreshToken we sent.
                let body = scrub_tokens_from_body(&raw, &[Some(refresh_token.as_str())]);
                eprintln!(
                    "[cloud-client] token refresh returned {status}: {}",
                    truncate_for_error(&body, 500)
                );
            }
            return None;
        }
        let body = match resp.json::<TokenRefreshResponse>().await {
            Ok(body) => body,
            Err(e) => {
                if cloud_debug_enabled() {
                    eprintln!("[cloud-client] token refresh decode error: {e}");
                }
                return None;
            }
        };
        if Self::save_login_tokens(&body.token, body.refresh_token.as_deref())
            .await
            .is_err()
        {
            return None;
        }
        Some(body.token)
    }

    /// Shared auth-refresh-retry path for every authenticated request.
    ///
    /// `build` produces the request: `None` for the initial attempt (the
    /// caller embeds its own token, which may be absent) and
    /// `Some(&refreshed_token)` for the single retry that happens iff the
    /// first response is `401 Unauthorized` *and* `refresh_saved_token()`
    /// succeeds. Exactly one refresh + one retry, never a loop.
    ///
    /// Errors carry the [`SendPhase`] so callers can distinguish initial
    /// send failures from retry failures.
    async fn send_with_refresh<F>(
        build: F,
    ) -> crate::Result<reqwest::Response, (SendPhase, reqwest::Error)>
    where
        F: Fn(Option<&str>) -> reqwest::RequestBuilder,
    {
        let resp = build(None)
            .send()
            .await
            .map_err(|e| (SendPhase::Initial, e))?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED
            && let Some(refreshed_token) = Self::refresh_saved_token().await
        {
            return build(Some(&refreshed_token))
                .send()
                .await
                .map_err(|e| (SendPhase::Retry, e));
        }
        Ok(resp)
    }

    pub const fn is_logged_in(&self) -> bool {
        self.token.is_some()
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Call the cloud review-memory endpoint for semantically similar
    /// past verdicts. Deliberately fault-tolerant — a failing recall must
    /// NEVER block a review:
    /// * On HTTP 403 (no team entitlement, or a capacity/permission limit)
    ///   returns `Ok(vec![])` so the user proceeds with a normal review.
    /// * On network / decode errors, logs and returns `Ok(vec![])`.
    pub async fn recall_past_verdicts(
        &self,
        req: RecallPastVerdictsRequest,
    ) -> Result<Vec<PastVerdict>, crate::CoreError> {
        if !self.is_logged_in() {
            return Ok(Vec::new());
        }

        let url = format!("{}/reviews/recall-past-verdicts", self.base_url);
        // Recall needs its own transient-failure handling: a 45s timeout
        // plus backoff retry (see `send_recall_past_verdicts`), which the
        // shared `send_with_refresh` does not provide. This site keeps its
        // dedicated send path but still does the standard 401 refresh-retry.
        let mut resp = match self
            .send_recall_past_verdicts(&url, self.token.as_deref(), &req)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                if cloud_debug_enabled() {
                    eprintln!(
                        "[cloud-client] recall_past_verdicts network error after {} attempts: {e}",
                        PAST_VERDICT_RETRY_DELAYS_MS.len() + 1
                    );
                }
                return Ok(Vec::new());
            }
        };

        let mut status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED
            && let Some(refreshed_token) = Self::refresh_saved_token().await
        {
            match self
                .send_recall_past_verdicts(&url, Some(&refreshed_token), &req)
                .await
            {
                Ok(r) => {
                    resp = r;
                    status = resp.status();
                }
                Err(e) => {
                    if cloud_debug_enabled() {
                        eprintln!(
                            "[cloud-client] recall_past_verdicts retry error after token refresh: {e}"
                        );
                    }
                    return Ok(Vec::new());
                }
            }
        }
        if status == reqwest::StatusCode::FORBIDDEN {
            // Plan limit or missing team feature — graceful no-op.
            if cloud_debug_enabled() {
                static NOTIFIED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
                NOTIFIED.get_or_init(|| {
                    eprintln!(
                        "[difflore] Past-verdict recall skipped: this request needs more review-memory capacity or team scope. \
                         Personal Cloud Free recall still works for capped memory; see pricing at {} to expand team-wide recall.",
                        pricing_url()
                    );
                });
            }
            return Ok(Vec::new());
        }
        if status == reqwest::StatusCode::UNAUTHORIZED {
            // The server returns 401 here even for valid logged-in team
            // sessions (a cloud-side authz scope gap, not a client bug),
            // so recall stays fail-safe (empty). Diagnostic is deduped once
            // per process so commands aren't spammed with a 401.
            if cloud_debug_enabled() {
                static NOTIFIED_401: std::sync::OnceLock<()> = std::sync::OnceLock::new();
                NOTIFIED_401.get_or_init(|| {
                    eprintln!(
                        "[difflore] Past-verdict recall unauthorized (401). \
                         Continuing with local rules. Set DIFFLORE_DEBUG_CLOUD=1 \
                         for transport details, or run `difflore cloud status` \
                         to verify your session."
                    );
                });
            }
            return Ok(Vec::new());
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            if cloud_debug_enabled() {
                static NOTIFIED_OTHER: std::sync::OnceLock<()> = std::sync::OnceLock::new();
                NOTIFIED_OTHER.get_or_init(|| {
                    eprintln!(
                        "[difflore] Past-verdict recall unavailable ({status}). \
                         Continuing with local rules. Cloud response: {body}"
                    );
                });
            }
            return Ok(Vec::new());
        }

        let dtos: Vec<PastVerdictDto> = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                if cloud_debug_enabled() {
                    eprintln!("[cloud-client] recall_past_verdicts decode error: {e}");
                }
                return Ok(Vec::new());
            }
        };

        Ok(dtos
            .into_iter()
            .map(|d| PastVerdict {
                extraction_id: d.extraction_id,
                code_snippet: d.code_snippet,
                issue_text: d.issue_text,
                status: d.status,
                reason: d.reason,
                similarity: d.similarity,
                created_at: d.created_at,
                signature: d.signature,
                source_pr_number: d.source_pr_number,
                source_pr_title: d.source_pr_title,
                source_pr_url: d.source_pr_url,
            })
            .collect())
    }

    fn recall_past_verdicts_request(
        &self,
        url: &str,
        token: Option<&str>,
    ) -> reqwest::RequestBuilder {
        let mut builder = self
            .client
            .post(url)
            .timeout(PAST_VERDICT_RECALL_TIMEOUT)
            .header("content-type", "application/json");
        if let Some(token) = token {
            builder = builder.header("Authorization", format!("Bearer {token}"));
        }
        builder
    }

    async fn send_recall_past_verdicts(
        &self,
        url: &str,
        token: Option<&str>,
        req: &RecallPastVerdictsRequest,
    ) -> Result<reqwest::Response, reqwest::Error> {
        // Retry with backoff: one attempt per configured delay, then a final
        // attempt whose result (Ok or Err) is returned directly.
        for &delay_ms in PAST_VERDICT_RETRY_DELAYS_MS {
            match self
                .recall_past_verdicts_request(url, token)
                .json(req)
                .send()
                .await
            {
                Ok(resp) => return Ok(resp),
                Err(_) => {
                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                }
            }
        }
        self.recall_past_verdicts_request(url, token)
            .json(req)
            .send()
            .await
    }

    /// Shared helper for the fire-and-forget POST endpoints below. Returns
    /// `true` iff the server accepted the payload (2xx). Transient failure
    /// must NOT bubble up and kill the review/fix pipeline; most callers
    /// ignore the bool.
    async fn post_fire_and_forget<T: serde::Serialize>(
        &self,
        path: &str,
        body: &T,
        endpoint_label: &'static str,
    ) -> bool {
        match self
            .post_fire_and_forget_result(path, body, endpoint_label)
            .await
        {
            Ok(()) => true,
            Err(e) => {
                if cloud_debug_enabled() {
                    eprintln!("[cloud-client] {e}");
                }
                false
            }
        }
    }

    async fn post_fire_and_forget_result<T: serde::Serialize>(
        &self,
        path: &str,
        body: &T,
        endpoint_label: &'static str,
    ) -> crate::Result<()> {
        if !self.is_logged_in() {
            return Err(format!("{endpoint_label} skipped: not logged in").into());
        }

        let url = format!("{}{}", self.base_url, path);
        let resp = Self::send_with_refresh(|refreshed| {
            let mut builder = self
                .client
                .post(&url)
                .header("content-type", "application/json");
            match refreshed {
                Some(token) => {
                    builder = builder.header("Authorization", format!("Bearer {token}"));
                }
                None => {
                    if let Some(ref token) = self.token {
                        builder = builder.header("Authorization", format!("Bearer {token}"));
                    }
                }
            }
            builder.json(body)
        })
        .await
        .map_err(|(phase, e)| match phase {
            SendPhase::Initial => format!("{endpoint_label} network error: {e}"),
            SendPhase::Retry => format!("{endpoint_label} retry network error: {e}"),
        })?;

        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }

        let body = self
            .scrub_response_body(&resp.text().await.unwrap_or_default())
            .await;
        Err(format!(
            "{endpoint_label} returned {status}: {}",
            truncate_for_error(&body, 500)
        )
        .into())
    }

    /// Like [`post_fire_and_forget_result`] but surfaces the HTTP status +
    /// body fragment as structured data so the outbox can write a greppable
    /// `last_error`.
    ///
    /// Returns `Ok(())` on 2xx, `Err(OutboxFailure::Http(_))` for any
    /// non-2xx (including 401 after refresh+retry is exhausted), and
    /// `Err(OutboxFailure::Transport(_))` when the request never produced a
    /// status. Logs nothing to stdout/stderr — the response body lands only
    /// in the returned struct.
    async fn post_fire_and_forget_outcome<T: serde::Serialize>(
        &self,
        path: &str,
        body: &T,
        endpoint_label: &'static str,
    ) -> crate::Result<(), OutboxFailure> {
        if !self.is_logged_in() {
            // "Not logged in" is a transport-class failure: no HTTP status
            // to attach, same remediation shape as other unreachable states.
            return Err(OutboxFailure::Transport(format!(
                "{endpoint_label} skipped: not logged in"
            )));
        }

        let url = format!("{}{}", self.base_url, path);
        let resp = Self::send_with_refresh(|refreshed| {
            let mut builder = self
                .client
                .post(&url)
                .header("content-type", "application/json");
            match refreshed {
                Some(token) => {
                    builder = builder.header("Authorization", format!("Bearer {token}"));
                }
                None => {
                    if let Some(ref token) = self.token {
                        builder = builder.header("Authorization", format!("Bearer {token}"));
                    }
                }
            }
            builder.json(body)
        })
        .await
        .map_err(|(phase, e)| {
            let label = match phase {
                SendPhase::Initial => "initial",
                SendPhase::Retry => "retry",
            };
            OutboxFailure::Transport(format!("{endpoint_label} {label}: {e}"))
        })?;

        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }

        let status_code = status.as_u16();
        // `canonical_reason()` is None for unassigned status numbers; fall
        // back to the numeric form so `last_error` is never an empty phrase.
        let reason_phrase = status
            .canonical_reason()
            .map_or_else(|| status_code.to_string(), str::to_owned);
        let body_text = self
            .scrub_response_body(&resp.text().await.unwrap_or_default())
            .await;
        let body_snippet = normalize_body_snippet(&body_text, 200);
        Err(OutboxFailure::Http(HttpFailure {
            status: status_code,
            reason_phrase,
            body_snippet,
        }))
    }

    /// Outbox-friendly [`save_trajectory`]: returns [`OutboxFailure`]
    /// instead of a `bool` so the queue's `last_error` can carry the
    /// upstream status + body fragment.
    pub(crate) async fn save_trajectory_outcome(
        &self,
        pr_review_id: &str,
        steps: serde_json::Value,
    ) -> crate::Result<(), OutboxFailure> {
        let req = SaveTrajectoryRequest { steps };
        let path = format!("/reviews/{}/trajectory", escape_path_id(pr_review_id));
        self.post_fire_and_forget_outcome(&path, &req, "save_trajectory")
            .await
    }

    /// Outbox-friendly wrapper around [`record_review_metrics`].
    pub(crate) async fn record_review_metrics_outcome(
        &self,
        review_id: &str,
        req: RecordReviewMetricsRequest,
    ) -> crate::Result<(), OutboxFailure> {
        let path = format!("/reviews/{}/metrics", escape_path_id(review_id));
        self.post_fire_and_forget_outcome(&path, &req, "record_review_metrics")
            .await
    }

    /// Outbox-friendly wrapper around [`track_mcp_query`].
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn track_mcp_query_outcome(
        &self,
        file: &str,
        intent: Option<&str>,
        rules_injected: usize,
        strict_match_count: usize,
        rule_titles: Vec<String>,
        rule_ids: Vec<String>,
        client_label: Option<&str>,
        repo_full_name: Option<&str>,
    ) -> crate::Result<(), OutboxFailure> {
        let titles: Vec<String> = rule_titles.into_iter().take(10).collect();
        let ids: Vec<String> = rule_ids.into_iter().take(10).collect();
        let body = serde_json::json!({
            "file": file,
            "intent": intent,
            "rulesInjected": rules_injected,
            "strictMatchCount": strict_match_count,
            "ruleTitles": titles,
            "ruleIds": ids,
            "client": client_label.unwrap_or("mcp-server"),
            "repoFullName": repo_full_name,
        });
        self.post_fire_and_forget_outcome("/dashboard/mcp-query", &body, "track_mcp_query")
            .await
    }

    /// Outbox-friendly wrapper around [`upload_imported_reviews`].
    pub(crate) async fn upload_imported_reviews_outcome(
        &self,
        req: &UploadImportedReviewsRequest,
    ) -> crate::Result<(), OutboxFailure> {
        self.post_fire_and_forget_outcome("/reviews/import", req, "upload_imported_reviews")
            .await
    }

    /// Outbox-friendly wrapper around [`post_observations`].
    pub(crate) async fn post_observations_outcome(
        &self,
        batch: &[crate::contract::Observation],
    ) -> crate::Result<(), OutboxFailure> {
        self.post_fire_and_forget_outcome("/cloud/observations", &batch, "post_observations")
            .await
    }

    /// Outbox-friendly wrapper for locally mined candidate rules.
    pub(crate) async fn post_session_mined_candidate_outcome(
        &self,
        candidate: &crate::cloud::session_mined::SessionMinedCandidate,
    ) -> crate::Result<(), OutboxFailure> {
        self.post_fire_and_forget_outcome(
            "/cloud/session-mined-candidates",
            candidate,
            "post_session_mined_candidate",
        )
        .await
    }

    /// POST `/reviews/{id}/metrics`: upload token usage + estimated cost +
    /// duration for one review run. All request fields are optional (`None`
    /// leaves that column alone). Fire-and-forget — a failed upload must
    /// NEVER block a review; production callers ignore the bool.
    pub async fn record_review_metrics(
        &self,
        review_id: &str,
        req: RecordReviewMetricsRequest,
    ) -> bool {
        let path = format!("/reviews/{}/metrics", escape_path_id(review_id));
        self.post_fire_and_forget(&path, &req, "record_review_metrics")
            .await
    }

    /// POST `/reviews/{prReviewId}/trajectory`: send the serialized output
    /// of `TrajectoryBuilder::into_json()`. Fire-and-forget — a missing
    /// trajectory is never a review blocker.
    pub async fn save_trajectory(&self, pr_review_id: &str, steps: serde_json::Value) -> bool {
        let req = SaveTrajectoryRequest { steps };
        let path = format!("/reviews/{}/trajectory", escape_path_id(pr_review_id));
        self.post_fire_and_forget(&path, &req, "save_trajectory")
            .await
    }

    /// GET `/reviews/{prReviewId}/trajectory` for the `difflore trajectory`
    /// renderer. Unlike recall this is **not** fail-safe-to-empty — the
    /// caller wants to know *why* a fetch failed, so the [`get_json`] error
    /// string is surfaced verbatim:
    ///
    /// * `"not_logged_in"` when no cloud token is present, and
    /// * `"[get_trajectory] returned 4xx: …"` for HTTP failures.
    ///
    /// A review with no persisted trajectory comes back as a zero-UUID
    /// placeholder with `steps: []` (not a 404), so an empty `steps` vec is
    /// the "nothing recorded yet" signal.
    pub async fn get_trajectory(&self, pr_review_id: &str) -> crate::Result<GetTrajectoryResponse> {
        let path = format!("/reviews/{}/trajectory", escape_path_id(pr_review_id));
        self.get_json(&path, "get_trajectory").await
    }

    /// POST `/dashboard/mcp-query` — live agent-activity telemetry, sent
    /// each time the MCP server answers a rule-search tool call. Fire-and-
    /// forget — a missing telemetry hit must never block the MCP response
    /// (agents time out on slow MCP tools).
    #[allow(clippy::too_many_arguments)]
    pub async fn track_mcp_query(
        &self,
        file: &str,
        intent: Option<&str>,
        rules_injected: usize,
        strict_match_count: usize,
        rule_titles: Vec<String>,
        rule_ids: Vec<String>,
        client_label: Option<&str>,
        repo_full_name: Option<&str>,
    ) -> bool {
        // Cap titles/ids at 10 — the server rejects more.
        let titles: Vec<String> = rule_titles.into_iter().take(10).collect();
        let ids: Vec<String> = rule_ids.into_iter().take(10).collect();
        let body = serde_json::json!({
            "file": file,
            "intent": intent,
            "rulesInjected": rules_injected,
            "strictMatchCount": strict_match_count,
            "ruleTitles": titles,
            "ruleIds": ids,
            "client": client_label.unwrap_or("mcp-server"),
            "repoFullName": repo_full_name,
        });
        self.post_fire_and_forget("/dashboard/mcp-query", &body, "track_mcp_query")
            .await
    }

    /// POST `/accepted-edits`, called when the user locally accepts an edit.
    /// The cloud inserts a `fix_acceptances` row that the `rule-promoter`
    /// worker later aggregates into candidate rules. Fire-and-forget — must
    /// never block the local accept UX.
    pub async fn record_accepted_edit(&self, req: RecordAcceptedEditRequest) -> bool {
        self.record_accepted_edit_response(req).await.map_or_else(
            |e| {
                if cloud_debug_enabled() {
                    eprintln!("[cloud-client] {e}");
                }
                false
            },
            |response| response.acceptance_recorded,
        )
    }

    /// POST `/accepted-edits` and return the cloud attribution details:
    /// whether the accepted edit was also linked to a team-scoped
    /// `fix_outcome` observation. That distinction matters for Impact
    /// evidence — rule-linked observations prove the fix path reused memory.
    pub async fn record_accepted_edit_response(
        &self,
        req: RecordAcceptedEditRequest,
    ) -> crate::Result<RecordAcceptedEditResponse> {
        self.post_json("/accepted-edits", &req, "record_accepted_edit")
            .await
    }

    /// POST `/reviews/import`: upload locally-imported PR review comments
    /// for team-wide recall and analytics. Fire-and-forget — must never
    /// block the local import pipeline.
    pub async fn upload_imported_reviews(&self, req: &UploadImportedReviewsRequest) -> bool {
        self.post_fire_and_forget("/reviews/import", req, "upload_imported_reviews")
            .await
    }

    pub async fn post_observations(&self, batch: &[crate::contract::Observation]) -> bool {
        self.post_fire_and_forget("/cloud/observations", &batch, "post_observations")
            .await
    }

    pub async fn post_observation_events(
        &self,
        batch: &[super::observations::ObservationEvent],
    ) -> bool {
        self.post_fire_and_forget("/cloud/observations", &batch, "post_observation_events")
            .await
    }

    pub async fn post_observation_events_result(
        &self,
        batch: &[super::observations::ObservationEvent],
    ) -> crate::Result<()> {
        self.post_fire_and_forget_result("/cloud/observations", &batch, "post_observation_events")
            .await
    }

    /// Scrub the construction-time bearer, the current saved bearer (possibly
    /// refreshed mid-call), and the refresh token from a response body before
    /// it is surfaced in an error. Async because it reads the latest saved
    /// tokens; only hit on rare non-2xx error paths.
    async fn scrub_response_body(&self, body: &str) -> String {
        let saved = Self::load_token_quiet().await;
        let refresh = Self::load_refresh_token().await;
        scrub_tokens_from_body(
            body,
            &[self.token.as_deref(), saved.as_deref(), refresh.as_deref()],
        )
    }

    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        label: &'static str,
    ) -> crate::Result<T> {
        if !self.is_logged_in() {
            return Err("not_logged_in".to_owned().into());
        }
        let url = format!("{}{}", self.base_url, path);
        let resp = Self::send_with_refresh(|refreshed| {
            let mut builder = self.client.get(&url);
            match refreshed {
                Some(token) => {
                    builder = builder.header("Authorization", format!("Bearer {token}"));
                }
                None => {
                    if let Some(ref token) = self.token {
                        builder = builder.header("Authorization", format!("Bearer {token}"));
                    }
                }
            }
            builder
        })
        .await
        .map_err(|(phase, e)| match phase {
            SendPhase::Initial => format!("[{label}] network error: {e}"),
            SendPhase::Retry => format!("[{label}] retry network error: {e}"),
        })?;
        let status = resp.status();
        if !status.is_success() {
            let body = self
                .scrub_response_body(&resp.text().await.unwrap_or_default())
                .await;
            return Err(format!(
                "[{label}] returned {status}: {}",
                truncate_for_error(&body, 500)
            )
            .into());
        }
        resp.json::<T>()
            .await
            .map_err(|e| format!("[{label}] decode error: {e}").into())
    }

    /// POST helper that returns the decoded body. Used by interactive
    /// endpoints (knowledge corpora) where the user wants the answer, not
    /// just success/failure.
    async fn post_json<B: serde::Serialize, R: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
        label: &'static str,
    ) -> crate::Result<R> {
        if !self.is_logged_in() {
            return Err("not_logged_in".to_owned().into());
        }
        let url = format!("{}{}", self.base_url, path);
        let resp = Self::send_with_refresh(|refreshed| {
            let mut builder = self
                .client
                .post(&url)
                .header("content-type", "application/json");
            match refreshed {
                Some(token) => {
                    builder = builder.header("Authorization", format!("Bearer {token}"));
                }
                None => {
                    if let Some(ref token) = self.token {
                        builder = builder.header("Authorization", format!("Bearer {token}"));
                    }
                }
            }
            builder.json(body)
        })
        .await
        .map_err(|(phase, e)| match phase {
            SendPhase::Initial => format!("[{label}] network error: {e}"),
            SendPhase::Retry => format!("[{label}] retry network error: {e}"),
        })?;
        let status = resp.status();
        if !status.is_success() {
            let body = self
                .scrub_response_body(&resp.text().await.unwrap_or_default())
                .await;
            return Err(format!(
                "[{label}] returned {status}: {}",
                truncate_for_error(&body, 500)
            )
            .into());
        }
        resp.json::<R>()
            .await
            .map_err(|e| format!("[{label}] decode error: {e}").into())
    }

    /// POST `/knowledge/corpus` — build a filtered snapshot of rules +
    /// extractions into a new corpus.
    pub async fn build_corpus(
        &self,
        req: &crate::contract::BuildCorpusRequest,
    ) -> crate::Result<crate::contract::BuildCorpusResult> {
        self.post_json("/knowledge/corpus", req, "build_corpus")
            .await
    }

    /// POST `/knowledge/corpus/{id}/prime` — allocate a session token
    /// and mark the corpus primed. Returns the session token + ISO ts.
    pub async fn prime_corpus(
        &self,
        corpus_id: &str,
    ) -> crate::Result<crate::contract::PrimeCorpusResult> {
        let path = format!("/knowledge/corpus/{corpus_id}/prime");
        self.post_json(&path, &serde_json::json!({}), "prime_corpus")
            .await
    }

    /// POST `/knowledge/corpus/{id}/query` — ask the corpus a question.
    /// Returns answer + citations. Errors `LlmNotConfigured` if the
    /// caller has no `llmApiKey` configured cloud-side (BYOK gate).
    pub async fn query_corpus(
        &self,
        corpus_id: &str,
        question: &str,
    ) -> crate::Result<crate::contract::QueryCorpusResult> {
        let path = format!("/knowledge/corpus/{corpus_id}/query");
        let body = crate::contract::QueryCorpusRequest {
            question: question.to_owned(),
        };
        self.post_json(&path, &body, "query_corpus").await
    }

    /// GET `/knowledge/corpora` — list this team's corpora with item
    /// counts and prime/query timestamps.
    pub async fn list_corpora(&self) -> crate::Result<Vec<crate::contract::CorpusSummary>> {
        self.get_json("/knowledge/corpora", "list_corpora").await
    }

    /// GET `/impact/banner` — past verdicts recalled into reviews this week.
    pub async fn get_impact_banner(&self) -> crate::Result<ImpactBannerDto> {
        self.get_json("/impact/banner", "impact_banner").await
    }

    /// GET `/impact/weekly` — last 12 weeks of rules / verdicts / fixes.
    pub async fn get_impact_weekly(&self) -> crate::Result<ImpactWeeklyDto> {
        self.get_json("/impact/weekly", "impact_weekly").await
    }

    /// GET `/impact/top-rules` — top 5 candidate rules across user's teams.
    pub async fn get_impact_top_rules(&self) -> crate::Result<ImpactTopRulesDto> {
        self.get_json("/impact/top-rules", "impact_top_rules").await
    }

    /// GET `/impact/coverage` — repos / PRs / files covered by extractions.
    pub async fn get_impact_coverage(&self) -> crate::Result<ImpactCoverageDto> {
        self.get_json("/impact/coverage", "impact_coverage").await
    }

    /// GET `/impact/fix-scorecard` — last 30d fix acceptance rate + trend.
    pub async fn get_impact_fix_scorecard(&self) -> crate::Result<ImpactFixScorecardDto> {
        self.get_json("/impact/fix-scorecard", "impact_fix_scorecard")
            .await
    }
}

impl ApiClient for CloudClient {
    fn request(
        &self,
        method: Method,
        path: &str,
        query: Option<&str>,
        body: Option<String>,
    ) -> impl Future<Output = Result<reqwest::Response, ApiError>> + Send {
        let mut url = format!("{}{}", self.base_url, path);
        if let Some(qs) = query {
            url.push('?');
            url.push_str(qs);
        }
        let reqwest_method = method.as_reqwest();
        let client = self.client.clone();
        let token = self.token.clone();
        async move {
            Self::send_with_refresh(|refreshed| {
                let mut req = client.request(reqwest_method.clone(), &url);
                match refreshed {
                    // Retry attempt: force the freshly-refreshed bearer.
                    Some(refreshed_token) => {
                        req = req.header("Authorization", format!("Bearer {refreshed_token}"));
                    }
                    // Initial attempt: use the stored token if present.
                    None => {
                        if let Some(ref token) = token {
                            req = req.header("Authorization", format!("Bearer {token}"));
                        }
                    }
                }
                if let Some(ref b) = body {
                    req = req
                        .header("content-type", "application/json")
                        .body(b.clone());
                }
                req
            })
            .await
            .map_err(|(_phase, e)| ApiError::from(e))
        }
    }

    fn request_stream(
        &self,
        method: Method,
        path: &str,
        query: Option<&str>,
    ) -> impl Future<Output = Result<SseStream, ApiError>> + Send {
        let mut url = format!("{}{}", self.base_url, path);
        if let Some(qs) = query {
            url.push('?');
            url.push_str(qs);
        }
        let mut req = self.client.request(method.as_reqwest(), &url);
        if let Some(ref token) = self.token {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        async move {
            let resp = req.send().await.map_err(ApiError::from)?;
            let stream = resp.bytes_stream();
            Ok(SseStream::new(Box::pin(stream)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CloudClient, scrub_tokens_from_body};

    #[test]
    fn scrub_tokens_redacts_every_known_token_over_8_chars() {
        let body = "401: bearer=secret-access-12345 refresh=secret-refresh-67890 short=abc";
        let scrubbed = scrub_tokens_from_body(
            body,
            &[
                Some("secret-access-12345"),
                Some("secret-refresh-67890"),
                Some("abc"),
                None,
            ],
        );
        assert!(
            !scrubbed.contains("secret-access-12345"),
            "bearer must be redacted"
        );
        assert!(
            !scrubbed.contains("secret-refresh-67890"),
            "refresh must be redacted"
        );
        assert!(scrubbed.contains("[REDACTED-TOKEN]"));
        // Tokens under 8 chars are left alone (false-positive guard).
        assert!(scrubbed.contains("short=abc"));
    }

    #[tokio::test]
    async fn auth_pool_public_reopens_same_token_store() {
        let _home = crate::infra::db::shared_test_home();
        let pool = CloudClient::auth_pool_public()
            .await
            .expect("auth pool opens");
        sqlx::query("INSERT OR REPLACE INTO auth (key, value) VALUES (?1, ?2)")
            .bind("cache-test")
            .bind("cached")
            .execute(&pool)
            .await
            .expect("insert auth row");

        let cached = CloudClient::auth_pool_public()
            .await
            .expect("auth pool reopens");
        let value: String = sqlx::query_scalar("SELECT value FROM auth WHERE key = ?1")
            .bind("cache-test")
            .fetch_one(&cached)
            .await
            .expect("read auth row");
        assert_eq!(value, "cached");
    }
}
