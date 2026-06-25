use crate::error::InternalResultExt as _;
use openapi_contract::sse::SseStream;
use openapi_contract::{ApiClient, ApiError, ApiRequest, Method, api};
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use std::collections::HashMap;
use std::fmt::Write as _;
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
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                let _ = write!(&mut out, "%{byte:02X}");
            }
        }
    }
    out
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

fn format_structured_api_error(label: &str, status: u16, body: &str) -> String {
    let raw = format!("returned {status}: {body}");
    crate::domain::origins::format_api_error_from_status(label, &raw, status)
        .unwrap_or_else(|| format!("{label} returned {status}: {body}"))
}

fn build_api_url(base_url: &str, path: &str, query: Option<&str>) -> String {
    let mut url = format!("{}{}", base_url.trim_end_matches('/'), path);
    if let Some(qs) = query {
        url.push('?');
        url.push_str(qs);
    }
    url
}

#[allow(clippy::too_many_arguments)]
fn mcp_query_body(
    file: &str,
    intent: Option<&str>,
    rules_injected: usize,
    strict_match_count: usize,
    rule_titles: &[String],
    rule_ids: &[String],
    client_label: Option<&str>,
    repo_full_name: Option<&str>,
) -> serde_json::Value {
    let mut body = serde_json::json!({
        "file": file,
        "intent": intent,
        "rulesInjected": rules_injected,
        "strictMatchCount": strict_match_count,
        "ruleTitles": rule_titles,
        "ruleIds": rule_ids,
        "client": client_label.unwrap_or("mcp-server"),
    });
    if let Some(repo) = repo_full_name
        .map(str::trim)
        .filter(|repo| !repo.is_empty())
    {
        if let Some(obj) = body.as_object_mut() {
            obj.insert(
                "repoFullName".to_owned(),
                serde_json::Value::String(repo.to_owned()),
            );
        }
    }
    body
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

use crate::contract::{
    GetTrajectoryResponse, ImpactBannerDto, ImpactCoverageDto, ImpactFixScorecardDto,
    ImpactTopRulesDto, ImpactWeeklyDto, ObservationIngestResult, PastVerdictDto,
    RecallPastVerdictsRequest, RecordAcceptedEditRequest, RecordAcceptedEditResponse,
    RecordReviewMetricsRequest, SaveTrajectoryRequest, SessionMinedCandidateIngestResult,
    UploadImportedReviewsRequest,
};

#[derive(Clone)]
pub struct CloudClient {
    client: reqwest::Client,
    base_url: String,
    token: Option<String>,
}

struct TokenRefreshClient {
    client: reqwest::Client,
    base_url: String,
}

impl ApiClient for TokenRefreshClient {
    fn request(
        &self,
        method: Method,
        path: &str,
        query: Option<&str>,
        body: Option<String>,
    ) -> impl Future<Output = Result<reqwest::Response, ApiError>> + Send {
        let url = build_api_url(&self.base_url, path, query);
        let client = self.client.clone();
        async move {
            let mut req = client.request(method.as_reqwest(), &url);
            if let Some(body) = body {
                req = req.header("content-type", "application/json").body(body);
            }
            req.send().await.map_err(ApiError::from)
        }
    }

    fn request_stream(
        &self,
        method: Method,
        path: &str,
        query: Option<&str>,
    ) -> impl Future<Output = Result<SseStream, ApiError>> + Send {
        let url = build_api_url(&self.base_url, path, query);
        let client = self.client.clone();
        async move {
            let resp = client
                .request(method.as_reqwest(), &url)
                .send()
                .await
                .map_err(ApiError::from)?;
            Ok(SseStream::new(Box::pin(resp.bytes_stream())))
        }
    }
}

fn format_token_refresh_error(error: ApiError, refresh_token: &str) -> String {
    match error {
        ApiError::Http(e) => format!("network error: {e}"),
        ApiError::Serialization(e) => format!("decode error: {e}"),
        ApiError::Api { status, message } => {
            let body = scrub_tokens_from_body(&message, &[Some(refresh_token)]);
            format!("returned {status}: {}", truncate_for_error(&body, 500))
        }
        ApiError::Defined {
            status,
            code,
            message,
        } => {
            let body =
                scrub_tokens_from_body(&format!("{code}: {message}"), &[Some(refresh_token)]);
            format!("returned {status}: {}", truncate_for_error(&body, 500))
        }
    }
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
                // `quiet` suppresses the warning for read-only diagnostics so a
                // corrupt token doesn't spam stderr. Real cloud/recall calls
                // keep the warning.
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
        let pool = Self::auth_pool().await.internal()?;
        sqlx::query("DELETE FROM auth WHERE key = ?1")
            .bind(key)
            .execute(&pool)
            .await
            .internal()?;
        Ok(())
    }

    pub async fn save_token(token: &str) -> crate::Result<()> {
        Self::save_encrypted_auth_key(AUTH_TOKEN_KEY, token).await?;
        let pool = Self::auth_pool().await.internal()?;
        sqlx::query!("DELETE FROM auth WHERE key = 'login_nonce'")
            .execute(&pool)
            .await
            .internal()?;
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

    fn explicit_env_token() -> Option<String> {
        Self::resolve_explicit_env_token(crate::infra::env::non_empty)
    }

    fn resolve_explicit_env_token(
        mut lookup: impl FnMut(&str) -> Option<String>,
    ) -> Option<String> {
        lookup(crate::infra::env::DIFFLORE_CLOUD_TOKEN)
            .map(|token| token.trim().to_owned())
            .filter(|token| !token.is_empty())
    }

    pub async fn load_token() -> Option<String> {
        // An explicit env token is the user's own intent for the current URL.
        if let Some(token) = Self::explicit_env_token() {
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
        if let Some(token) = Self::explicit_env_token() {
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
        let payload = serde_json::json!({
            "clientId": CLI_CLIENT_ID,
            "refreshToken": refresh_token,
        });
        let refresh_client = TokenRefreshClient {
            client,
            base_url: Self::resolve_cloud_url(),
        };
        let body_value: serde_json::Value = match api!(POST "/token/refresh", body = &payload)
            .fetch(&refresh_client)
            .await
        {
            Ok(body) => body,
            Err(e) => {
                if cloud_debug_enabled() {
                    eprintln!(
                        "[cloud-client] token refresh error: {}",
                        format_token_refresh_error(e, &refresh_token)
                    );
                }
                return None;
            }
        };
        let body = match serde_json::from_value::<TokenRefreshResponse>(body_value) {
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

        let dtos: Vec<PastVerdictDto> = match self
            .fetch_api_json(
                api!(POST "/reviews/recall-past-verdicts", body = &req),
                "recall_past_verdicts",
            )
            .await
        {
            Ok(dtos) => dtos,
            Err(e) => {
                if cloud_debug_enabled() {
                    eprintln!(
                        "[difflore] Past-verdict recall unavailable. Continuing with local rules. {e}"
                    );
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

    /// Shared helper for the fire-and-forget POST endpoints below. Returns
    /// `true` iff the server accepted the payload (2xx). Transient failure
    /// must NOT bubble up and kill the review/fix pipeline; most callers
    /// ignore the bool.
    async fn post_fire_and_forget<T: serde::de::DeserializeOwned>(
        &self,
        request: ApiRequest<T>,
        endpoint_label: &'static str,
    ) -> bool {
        match self
            .post_fire_and_forget_result(request, endpoint_label)
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

    async fn post_fire_and_forget_result<T: serde::de::DeserializeOwned>(
        &self,
        request: ApiRequest<T>,
        endpoint_label: &'static str,
    ) -> crate::Result<()> {
        if !self.is_logged_in() {
            return Err(crate::CoreError::Auth(format!(
                "{endpoint_label} skipped: not logged in"
            )));
        }

        match request.fetch(self).await {
            Ok(_) => Ok(()),
            Err(e) => Err(crate::CoreError::internal(
                self.format_api_error(endpoint_label, e).await,
            )),
        }
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
    async fn post_fire_and_forget_outcome<T: serde::de::DeserializeOwned>(
        &self,
        request: ApiRequest<T>,
        endpoint_label: &'static str,
    ) -> crate::Result<(), OutboxFailure> {
        if !self.is_logged_in() {
            // "Not logged in" is a transport-class failure: no HTTP status
            // to attach, same remediation shape as other unreachable states.
            return Err(OutboxFailure::Transport(format!(
                "{endpoint_label} skipped: not logged in"
            )));
        }

        match request.fetch(self).await {
            Ok(_) => Ok(()),
            Err(e) => Err(self.outbox_failure_from_api_error(endpoint_label, e).await),
        }
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
        let pr_review_id = escape_path_id(pr_review_id);
        self.post_fire_and_forget_outcome(
            api!(POST "/reviews/{prReviewId}/trajectory", prReviewId = &pr_review_id, body = &req),
            "save_trajectory",
        )
        .await
    }

    /// Outbox-friendly wrapper around [`record_review_metrics`].
    pub(crate) async fn record_review_metrics_outcome(
        &self,
        review_id: &str,
        req: RecordReviewMetricsRequest,
    ) -> crate::Result<(), OutboxFailure> {
        let review_id = escape_path_id(review_id);
        self.post_fire_and_forget_outcome(
            api!(POST "/reviews/{id}/metrics", id = &review_id, body = &req),
            "record_review_metrics",
        )
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
        let body = mcp_query_body(
            file,
            intent,
            rules_injected,
            strict_match_count,
            &titles,
            &ids,
            client_label,
            repo_full_name,
        );
        self.post_fire_and_forget_outcome(
            api!(POST "/dashboard/mcp-query", body = &body),
            "track_mcp_query",
        )
        .await
    }

    /// Outbox-friendly wrapper around [`upload_imported_reviews`].
    pub(crate) async fn upload_imported_reviews_outcome(
        &self,
        req: &UploadImportedReviewsRequest,
    ) -> crate::Result<(), OutboxFailure> {
        self.post_fire_and_forget_outcome(
            api!(POST "/reviews/import", body = req),
            "upload_imported_reviews",
        )
        .await
    }

    /// Outbox-friendly wrapper around [`post_observations`].
    pub(crate) async fn post_observations_outcome(
        &self,
        batch: &[crate::contract::Observation],
    ) -> crate::Result<(), OutboxFailure> {
        self.post_fire_and_forget_outcome(
            api!(POST "/cloud/observations", body = &batch),
            "post_observations",
        )
        .await
    }

    /// Outbox-friendly wrapper for locally mined candidate rules.
    pub(crate) async fn post_session_mined_candidate_outcome(
        &self,
        candidate: &crate::cloud::session_mined::SessionMinedCandidate,
    ) -> crate::Result<(), OutboxFailure> {
        let batch = std::slice::from_ref(candidate);
        self.post_fire_and_forget_outcome(
            api!(POST "/cloud/session-mined-candidates", body = &batch),
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
        let review_id = escape_path_id(review_id);
        self.post_fire_and_forget(
            api!(POST "/reviews/{id}/metrics", id = &review_id, body = &req),
            "record_review_metrics",
        )
        .await
    }

    /// POST `/reviews/{prReviewId}/trajectory`: send the serialized output
    /// of `TrajectoryBuilder::into_json()`. Fire-and-forget — a missing
    /// trajectory is never a review blocker.
    pub async fn save_trajectory(&self, pr_review_id: &str, steps: serde_json::Value) -> bool {
        let req = SaveTrajectoryRequest { steps };
        let pr_review_id = escape_path_id(pr_review_id);
        self.post_fire_and_forget(
            api!(POST "/reviews/{prReviewId}/trajectory", prReviewId = &pr_review_id, body = &req),
            "save_trajectory",
        )
        .await
    }

    /// GET `/reviews/{prReviewId}/trajectory` for the `difflore trajectory`
    /// renderer. Unlike recall this is **not** fail-safe-to-empty — the
    /// caller wants to know *why* a fetch failed, so the typed OpenAPI
    /// request error string is surfaced verbatim:
    ///
    /// * `"not_logged_in"` when no cloud token is present, and
    /// * `"[get_trajectory] returned 4xx: …"` for HTTP failures.
    ///
    /// A review with no persisted trajectory comes back as a zero-UUID
    /// placeholder with `steps: []` (not a 404), so an empty `steps` vec is
    /// the "nothing recorded yet" signal.
    pub async fn get_trajectory(&self, pr_review_id: &str) -> crate::Result<GetTrajectoryResponse> {
        let pr_review_id = escape_path_id(pr_review_id);
        self.fetch_logged_in_api_json(
            api!(GET "/reviews/{prReviewId}/trajectory", prReviewId = &pr_review_id),
            "get_trajectory",
        )
        .await
    }

    /// POST `/accepted-edits` and return the cloud attribution details:
    /// whether the accepted edit was also linked to a team-scoped
    /// `fix_outcome` observation. That distinction matters for Impact
    /// evidence — rule-linked observations prove the fix path reused memory.
    pub async fn record_accepted_edit_response(
        &self,
        req: RecordAcceptedEditRequest,
    ) -> crate::Result<RecordAcceptedEditResponse> {
        self.fetch_logged_in_api_json(
            api!(POST "/accepted-edits", body = &req),
            "record_accepted_edit",
        )
        .await
    }

    /// POST `/reviews/import`: upload locally-imported PR review comments
    /// for team-wide recall and analytics. Fire-and-forget — must never
    /// block the local import pipeline.
    pub async fn upload_imported_reviews(&self, req: &UploadImportedReviewsRequest) -> bool {
        self.post_fire_and_forget(
            api!(POST "/reviews/import", body = req),
            "upload_imported_reviews",
        )
        .await
    }

    /// POST `/cloud/observations` for a batch of observation events, surfacing
    /// the HTTP status + body fragment as a structured [`OutboxFailure`] so the
    /// observation emitter can classify the failure (permanent vs. transient
    /// vs. rate limited) and decide how to reschedule instead of blindly
    /// abandoning.
    pub(crate) async fn post_observation_events_outcome(
        &self,
        batch: &[super::observations::ObservationEvent],
    ) -> crate::Result<(), OutboxFailure> {
        self.post_fire_and_forget_outcome(
            api!(POST "/cloud/observations", body = &batch),
            "post_observation_events",
        )
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

    async fn format_api_error(&self, label: &'static str, error: ApiError) -> String {
        match error {
            ApiError::Http(e) => format!("{label} network error: {e}"),
            ApiError::Serialization(e) => format!("{label} decode error: {e}"),
            ApiError::Api { status, message } => {
                let body = self.scrub_response_body(&message).await;
                format_structured_api_error(label, status, &truncate_for_error(&body, 500))
            }
            ApiError::Defined {
                status,
                code,
                message,
            } => {
                let body = self
                    .scrub_response_body(&format!("{code}: {message}"))
                    .await;
                format_structured_api_error(label, status, &truncate_for_error(&body, 500))
            }
        }
    }

    async fn outbox_failure_from_api_error(
        &self,
        label: &'static str,
        error: ApiError,
    ) -> OutboxFailure {
        match error {
            ApiError::Http(e) => OutboxFailure::Transport(format!("{label}: {e}")),
            ApiError::Serialization(e) => OutboxFailure::Transport(format!("{label}: {e}")),
            ApiError::Api { status, message } => self.outbox_http_failure(status, &message).await,
            ApiError::Defined {
                status,
                code,
                message,
            } => {
                self.outbox_http_failure(status, &format!("{code}: {message}"))
                    .await
            }
        }
    }

    async fn outbox_http_failure(&self, status: u16, body: &str) -> OutboxFailure {
        let reason_phrase = reqwest::StatusCode::from_u16(status)
            .ok()
            .and_then(|status| status.canonical_reason().map(str::to_owned))
            .unwrap_or_else(|| status.to_string());
        let body_text = self.scrub_response_body(body).await;
        let body_snippet = normalize_body_snippet(&body_text, 200);
        OutboxFailure::Http(HttpFailure {
            status,
            reason_phrase,
            body_snippet,
        })
    }

    pub(crate) async fn fetch_api_json<T, R>(
        &self,
        request: ApiRequest<T>,
        label: &'static str,
    ) -> crate::Result<R>
    where
        T: serde::de::DeserializeOwned + serde::Serialize,
        R: serde::de::DeserializeOwned,
    {
        let response = match request.fetch(self).await {
            Ok(response) => response,
            Err(e) => {
                return Err(crate::CoreError::internal(
                    self.format_api_error(label, e).await,
                ));
            }
        };
        let value = serde_json::to_value(response).map_err(|e| {
            crate::CoreError::internal(format!("[{label}] encode generated response error: {e}"))
        })?;
        serde_json::from_value::<R>(value)
            .map_err(|e| crate::CoreError::internal(format!("[{label}] decode error: {e}")))
    }

    async fn fetch_logged_in_api_json<T, R>(
        &self,
        request: ApiRequest<T>,
        label: &'static str,
    ) -> crate::Result<R>
    where
        T: serde::de::DeserializeOwned + serde::Serialize,
        R: serde::de::DeserializeOwned,
    {
        if !self.is_logged_in() {
            return Err(crate::CoreError::Auth("not_logged_in".to_owned()));
        }
        self.fetch_api_json(request, label).await
    }

    /// GET `/impact/banner` — past verdicts recalled into reviews this week.
    pub async fn get_impact_banner(&self) -> crate::Result<ImpactBannerDto> {
        self.fetch_logged_in_api_json(api!(GET "/impact/banner"), "impact_banner")
            .await
    }

    /// GET `/impact/weekly` — last 12 weeks of rules / verdicts / fixes.
    pub async fn get_impact_weekly(&self) -> crate::Result<ImpactWeeklyDto> {
        self.fetch_logged_in_api_json(api!(GET "/impact/weekly"), "impact_weekly")
            .await
    }

    /// GET `/impact/top-rules` — top 5 candidate rules across user's teams.
    pub async fn get_impact_top_rules(&self) -> crate::Result<ImpactTopRulesDto> {
        self.fetch_logged_in_api_json(api!(GET "/impact/top-rules"), "impact_top_rules")
            .await
    }

    /// GET `/impact/coverage` — repos / PRs / files covered by extractions.
    pub async fn get_impact_coverage(&self) -> crate::Result<ImpactCoverageDto> {
        self.fetch_logged_in_api_json(api!(GET "/impact/coverage"), "impact_coverage")
            .await
    }

    /// GET `/impact/fix-scorecard` — last 30d fix acceptance rate + trend.
    pub async fn get_impact_fix_scorecard(&self) -> crate::Result<ImpactFixScorecardDto> {
        self.fetch_logged_in_api_json(api!(GET "/impact/fix-scorecard"), "impact_fix_scorecard")
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
        let url = build_api_url(&self.base_url, path, query);
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
        let url = build_api_url(&self.base_url, path, query);
        let reqwest_method = method.as_reqwest();
        let client = self.client.clone();
        let token = self.token.clone();
        async move {
            let resp = Self::send_with_refresh(|refreshed| {
                let mut req = client.request(reqwest_method.clone(), &url);
                match refreshed {
                    Some(refreshed_token) => {
                        req = req.header("Authorization", format!("Bearer {refreshed_token}"));
                    }
                    None => {
                        if let Some(ref token) = token {
                            req = req.header("Authorization", format!("Bearer {token}"));
                        }
                    }
                }
                req
            })
            .await
            .map_err(|(_phase, e)| ApiError::from(e))?;
            let stream = resp.bytes_stream();
            Ok(SseStream::new(Box::pin(stream)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CloudClient, escape_path_id, format_structured_api_error, mcp_query_body,
        scrub_tokens_from_body,
    };

    #[test]
    fn explicit_env_token_uses_cloud_token_name() {
        let mut keys = Vec::new();
        let token = CloudClient::resolve_explicit_env_token(|key| {
            keys.push(key.to_owned());
            if key == crate::infra::env::DIFFLORE_CLOUD_TOKEN {
                Some(" cloud-token ".to_owned())
            } else {
                None
            }
        });

        assert_eq!(token.as_deref(), Some("cloud-token"));
        assert_eq!(keys, vec![crate::infra::env::DIFFLORE_CLOUD_TOKEN]);
    }

    #[test]
    fn explicit_env_token_ignores_empty_cloud_token() {
        let token = CloudClient::resolve_explicit_env_token(|key| {
            if key == crate::infra::env::DIFFLORE_CLOUD_TOKEN {
                Some(" ".to_owned())
            } else {
                None
            }
        });

        assert_eq!(token, None);
    }

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

    #[test]
    fn escape_path_id_percent_encodes_path_segment_reserved_bytes() {
        assert_eq!(escape_path_id("abc-._~XYZ09"), "abc-._~XYZ09");
        assert_eq!(
            escape_path_id("a/b c?x#%:\u{2603}"),
            "a%2Fb%20c%3Fx%23%25%3A%E2%98%83"
        );
    }

    #[test]
    fn structured_api_error_formats_by_status_before_string_matching() {
        let unauthorized = format_structured_api_error("rules_sync", 401, "token revoked");
        assert!(unauthorized.contains("session expired"));
        assert!(unauthorized.contains("returned 401: token revoked"));

        let app_code = format_structured_api_error("rules_sync", 400, "API error 50001");
        assert_eq!(app_code, "rules_sync returned 400: API error 50001");
    }

    #[test]
    fn cloud_client_openapi_paths_are_not_sent_as_raw_strings() {
        let source = include_str!("client.rs");
        let forbidden = [
            (
                "direct ApiClient::request with a Method path",
                concat!("request", "(Method::"),
            ),
            (
                "raw fire-and-forget path",
                concat!("post_fire_and_forget", "(\"/"),
            ),
            ("legacy GET helper", concat!("get_", "json(")),
            ("legacy POST helper", concat!("post_", "json(")),
            (
                "raw reviews path formatting",
                concat!("format!", "(\"/reviews"),
            ),
            (
                "raw knowledge path formatting",
                concat!("format!", "(\"/knowledge"),
            ),
            ("raw cloud path formatting", concat!("format!", "(\"/cloud")),
            (
                "raw dashboard path formatting",
                concat!("format!", "(\"/dashboard"),
            ),
        ];

        for (label, pattern) in forbidden {
            assert!(
                !source.contains(pattern),
                "{label} bypasses openapi_contract::api!: {pattern}"
            );
        }
    }

    #[test]
    fn session_mined_candidate_upload_uses_cloud_batch_contract() {
        let source = include_str!("client.rs");
        let bare_object_call = concat!(
            r#"api!(POST "/cloud/session-mined-candidates", body = "#,
            "candidate)"
        );
        assert!(
            source.contains("let batch = std::slice::from_ref(candidate);"),
            "session-mined uploads must wrap the single outbox item in the cloud batch payload"
        );
        assert!(
            source.contains(r#"api!(POST "/cloud/session-mined-candidates", body = &batch)"#),
            "session-mined cloud endpoint accepts an array batch, not a bare candidate object"
        );
        assert!(
            !source.contains(bare_object_call),
            "a bare candidate object is rejected by the cloud schema as invalid_batch"
        );
    }

    #[test]
    fn mcp_query_body_omits_missing_repo_full_name() {
        let body = mcp_query_body(
            "unknown",
            Some("self-check"),
            0,
            0,
            &[],
            &[],
            Some("mcp-server-search"),
            None,
        );

        assert_eq!(body["client"], "mcp-server-search");
        assert!(
            body.as_object()
                .is_some_and(|obj| !obj.contains_key("repoFullName")),
            "cloud schema accepts omitted repoFullName but rejects null"
        );
    }

    #[test]
    fn mcp_query_body_keeps_present_repo_full_name() {
        let body = mcp_query_body(
            "src/lib.rs",
            None,
            1,
            1,
            &["Use typed routes".to_owned()],
            &["rule-1".to_owned()],
            None,
            Some(" Acme/Widgets "),
        );

        assert_eq!(body["client"], "mcp-server");
        assert_eq!(body["repoFullName"], "Acme/Widgets");
    }

    #[test]
    fn cloud_openapi_call_sites_use_contract_macro() {
        fn collect_rs_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).expect("read source directory") {
                let path = entry.expect("read source directory entry").path();
                if path.is_dir() {
                    collect_rs_files(&path, out);
                } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
                    out.push(path);
                }
            }
        }

        let mut files = Vec::new();
        collect_rs_files(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut files,
        );

        let forbidden = [
            (
                "direct ApiClient::request with a Method path",
                concat!("request", "(Method::"),
            ),
            (
                "raw fire-and-forget path",
                concat!("post_fire_and_forget", "(\"/"),
            ),
            (
                "raw fire-and-forget result path",
                concat!("post_fire_and_forget_result", "(\"/"),
            ),
            (
                "raw fire-and-forget outcome path",
                concat!("post_fire_and_forget_outcome", "(\"/"),
            ),
            ("legacy GET helper", concat!("get_", "json(")),
            ("legacy POST helper", concat!("post_", "json(")),
            (
                "raw reviews path formatting",
                concat!("format!", "(\"/reviews"),
            ),
            (
                "raw knowledge path formatting",
                concat!("format!", "(\"/knowledge"),
            ),
            (
                "raw impact path formatting",
                concat!("format!", "(\"/impact"),
            ),
            ("raw cloud path formatting", concat!("format!", "(\"/cloud")),
            (
                "raw dashboard path formatting",
                concat!("format!", "(\"/dashboard"),
            ),
            ("raw rules path formatting", concat!("format!", "(\"/rules")),
            ("raw sync path formatting", concat!("format!", "(\"/sync")),
            ("raw auth path formatting", concat!("format!", "(\"/auth")),
            (
                "raw billing path formatting",
                concat!("format!", "(\"/billing"),
            ),
            ("raw teams path formatting", concat!("format!", "(\"/teams")),
            ("raw token path formatting", concat!("format!", "(\"/token")),
        ];

        let mut failures = Vec::new();
        for file in files {
            let source = std::fs::read_to_string(&file).expect("read Rust source");
            for (label, pattern) in forbidden {
                if source.contains(pattern) {
                    failures.push(format!("{}: {label}: {pattern}", file.display()));
                }
            }
        }

        assert!(
            failures.is_empty(),
            "OpenAPI-covered cloud calls must go through openapi_contract::api!: {failures:#?}"
        );
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
