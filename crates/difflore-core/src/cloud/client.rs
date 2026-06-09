use openapi_contract::sse::SseStream;
use openapi_contract::{ApiClient, ApiError, Method};
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use std::collections::HashMap;
use std::path::PathBuf;

/// Cloud-client debug noise (network errors on degraded paths) is gated
/// behind `DIFFLORE_DEBUG_CLOUD=1`. The default is silent: when cloud is
/// unavailable, every degraded endpoint here returns an empty/false sentinel
/// and the caller carries on — printing the raw reqwest error to stderr just
/// confuses users who are already getting a friendlier message from
/// `format_cloud_err` at the top-level command. Devs flip the env var on to
/// trace transport-layer issues.
fn cloud_debug_enabled() -> bool {
    crate::env::debug_cloud()
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
    /// The first send, before any token refresh.
    Initial,
    /// The single post-refresh retry send.
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

/// Scrub every known auth token (≥8 chars) from a cloud-response body before it
/// is logged or surfaced in an error. A hostile / compromised cloud could echo
/// the bearer OR refresh token back inside an error body; without this the CLI
/// would faithfully print the token to stderr / a returned error string. Tokens
/// under 8 chars are skipped (false positives on common substrings; real tokens
/// are always longer).
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

/// Distilled fingerprint of an upload that returned a non-2xx HTTP
/// response. Held by [`OutboxFailure::Http`] so the outbox can write
/// a human-greppable `last_error` (e.g. `"401 Unauthorized: …"`)
/// instead of the historic — and useless — `"upload returned
/// non-2xx"`.
///
/// `body_snippet` is the first ~200 chars of the response body with
/// all runs of whitespace collapsed to a single space. The body is
/// stored only inside this struct and only ever written into the
/// local SQLite `last_error` column — it is never logged to
/// stdout/stderr — so a stack-trace echo from the cloud cannot leak
/// into a user's foreground output.
///
/// The response body is opaque to the client and could echo a token or
/// other sensitive blob. Known auth tokens are scrubbed before the snippet is
/// stored. Treat `last_error` as locally trusted diagnostic data, not as
/// something to forward to third parties without review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HttpFailure {
    pub status: u16,
    pub reason_phrase: String,
    pub body_snippet: String,
}

/// Outcome of a fire-and-forget POST whose dispatcher wants the
/// status + body details (not just success/failure). Two flavours so
/// the outbox can distinguish "the cloud rejected this with 401"
/// from "we never reached the cloud" — collapsing the two used to
/// hide stale-auth incidents inside a generic transport-error
/// bucket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OutboxFailure {
    Http(HttpFailure),
    /// Transport / DNS / TLS / timeout — the request never produced
    /// a status code. The payload is the lossy `reqwest::Error`
    /// description (already includes the underlying `kind`).
    Transport(String),
}

impl OutboxFailure {
    /// Format the failure into the spec-mandated shape for the
    /// `cloud_outbox.last_error` column:
    ///
    /// * `Http(401, "Unauthorized", "{\"error\":\"…\"}")` →
    ///   `"401 Unauthorized: {\"error\":\"…\"}"`
    /// * `Http(500, "Internal Server Error", "")` →
    ///   `"500 Internal Server Error"`
    /// * `Transport("…connection refused…")` →
    ///   `"transport: …connection refused…"`
    ///
    /// The shape is deliberately greppable: `last_error LIKE '4__ %'`
    /// finds every client-class HTTP failure across the queue, and
    /// the leading status code parses with a one-line `awk`.
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
            // Transport-class sentinel — distinct from any
            // `{status_code} {reason}` shape so `classify_upload_issue`
            // and grep can tell them apart at a glance.
            Self::Transport(msg) => format!("transport: {msg}"),
        }
    }
}

/// Collapse all runs of ASCII whitespace (including tabs / CR / LF)
/// to a single space and truncate to at most `max_chars` Unicode
/// scalar values, never splitting a UTF-8 codepoint. Returns an
/// owned `String` because SQLite stores TEXT as UTF-8 and
/// `last_error` is bounded by `outbox_core::truncate` downstream
/// anyway. Used for HTTP body fragments embedded in `last_error` so
/// a JSON error blob with embedded newlines still fits on a single
/// grep line.
pub(crate) fn normalize_body_snippet(body: &str, max_chars: usize) -> String {
    let collapsed: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= max_chars {
        return collapsed;
    }
    collapsed.chars().take(max_chars).collect()
}

use crate::context::types::PastVerdict;
use crate::crypto::{decrypt_secret, encrypt_secret};

use super::endpoints::pricing_url;

use super::api_types::{
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

    fn auth_db_path() -> Result<PathBuf, String> {
        // Route through `paths::data_home()` so `DIFFLORE_HOME` controls
        // tests and self-host setups.
        Ok(crate::paths::data_home()?.join("cloud-auth.db"))
    }

    pub async fn auth_pool() -> Result<SqlitePool, String> {
        let path = Self::auth_db_path()?;
        let mut guard = AUTH_POOL_CACHE.lock().await;
        let cache = guard.get_or_insert_with(HashMap::new);
        if let Some(pool) = cache.get(&path) {
            return Ok(pool.clone());
        }

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            crate::infra::db::restrict_to_owner(parent, true);
        }

        let opts = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true);

        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .map_err(|e| e.to_string())?;

        // cloud-auth.db holds the (encrypted) auth token — restrict it to the
        // owner on Unix (Windows relies on the per-user profile ACL).
        crate::infra::db::restrict_sqlite_files(&path);

        // cloud-auth.db is a two-column token store; create only the
        // table it needs, idempotently and order-independently.
        sqlx::query!(
            "CREATE TABLE IF NOT EXISTS auth (\
                key TEXT PRIMARY KEY NOT NULL, \
                value TEXT NOT NULL\
            )"
        )
        .execute(&pool)
        .await
        .map_err(|e| format!("auth table create failed: {e}"))?;

        cache.insert(path, pool.clone());
        Ok(pool)
    }

    pub async fn auth_pool_public() -> Result<SqlitePool, String> {
        Self::auth_pool().await
    }

    async fn save_encrypted_auth_key(key: &str, value: &str) -> Result<(), String> {
        let encrypted = encrypt_secret(value)?;
        let pool = Self::auth_pool().await?;
        sqlx::query("INSERT OR REPLACE INTO auth (key, value) VALUES (?1, ?2)")
            .bind(key)
            .bind(encrypted)
            .execute(&pool)
            .await
            .map_err(|e| e.to_string())?;
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

    async fn delete_auth_key(key: &str) -> Result<(), String> {
        let pool = Self::auth_pool().await?;
        sqlx::query("DELETE FROM auth WHERE key = ?1")
            .bind(key)
            .execute(&pool)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub async fn save_token(token: &str) -> Result<(), String> {
        Self::save_encrypted_auth_key(AUTH_TOKEN_KEY, token).await?;
        let pool = Self::auth_pool().await?;
        sqlx::query!("DELETE FROM auth WHERE key = 'login_nonce'")
            .execute(&pool)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub async fn save_refresh_token(refresh_token: &str) -> Result<(), String> {
        Self::save_encrypted_auth_key(AUTH_REFRESH_TOKEN_KEY, refresh_token).await
    }

    pub async fn save_login_tokens(token: &str, refresh_token: Option<&str>) -> Result<(), String> {
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
        // Environment variable override — useful when OS keyring is unreliable.
        // An explicit env token is the user's own intent for the current URL, so
        // Env tokens are explicit user intent for the current URL.
        if let Some(token) = crate::env::non_empty(crate::env::DIFFLORE_TOKEN) {
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
        if let Some(token) = crate::env::non_empty(crate::env::DIFFLORE_TOKEN) {
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

    pub async fn clear_token() -> Result<(), String> {
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

    /// Single source of truth for the auth-refresh-retry dance shared by
    /// every authenticated request path in this file.
    ///
    /// `build` is invoked to produce the request: with `None` for the
    /// initial attempt (the caller embeds its own token, which may be
    /// absent) and with `Some(&refreshed_token)` for the single retry that
    /// happens iff the first response is `401 Unauthorized` *and*
    /// `refresh_saved_token()` succeeds. Behaviour is exactly one
    /// refresh + one retry — no loop — so the security-sensitive 401
    /// handling lives in one place instead of being copy-pasted across
    /// `recall_past_verdicts`, `post_fire_and_forget_result`, `get_json`,
    /// `post_json`, and `ApiClient::request`.
    ///
    /// Errors carry the [`SendPhase`] so callers can distinguish initial
    /// send failures from retry failures.
    async fn send_with_refresh<F>(
        build: F,
    ) -> Result<reqwest::Response, (SendPhase, reqwest::Error)>
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
    /// past verdicts.
    ///
    /// This call is deliberately fault-tolerant:
    /// * On HTTP 403 (e.g. team-scope recall without the team entitlement,
    ///   or a capacity/permission limit) — returns `Ok(vec![])` so users
    ///   proceed with a normal review. Personal review-memory recall is
    ///   available on Cloud Free; paid plans expand capacity and team scope.
    /// * On network / decode errors — logs and returns `Ok(vec![])`; a
    ///   failing recall must NEVER block a review.
    ///
    /// The endpoint is hand-wired with `reqwest` rather than routed through
    /// `openapi_contract::api!` because the request/response types carry
    /// extra fields and derive traits beyond what the generated code provides.
    pub async fn recall_past_verdicts(
        &self,
        req: RecallPastVerdictsRequest,
    ) -> Result<Vec<PastVerdict>, crate::CoreError> {
        if !self.is_logged_in() {
            return Ok(Vec::new());
        }

        let url = format!("{}/reviews/recall-past-verdicts", self.base_url);
        // Past-verdict recall is the one best-effort endpoint that needs its
        // own transient-failure handling: a 45s timeout plus a short backoff
        // retry (see `send_recall_past_verdicts`). The shared
        // `send_with_refresh` helper used by the other endpoints does not
        // apply a timeout or retry network errors, so this site deliberately
        // keeps its dedicated send path while still performing the standard
        // one-shot 401 token-refresh retry below.
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
            // plan_limit_exceeded or missing team feature — graceful no-op.
            // Surface a single informational line (not an eprintln!'d error)
            // per process so users understand *why* recall returned empty
            // without turning every review into a warning spam.
            static NOTIFIED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
            NOTIFIED.get_or_init(|| {
                eprintln!(
                    "[difflore] Past-verdict recall skipped: this request needs more review-memory capacity or team scope. \
                     Personal Cloud Free recall still works for capped memory; see pricing at {} to expand team-wide recall.",
                    pricing_url()
                );
            });
            return Ok(Vec::new());
        }
        if status == reqwest::StatusCode::UNAUTHORIZED {
            // KNOWN: cloud-side auth-scope gap on /reviews/recall-past-verdicts (see difflore-cloud route authz); fail-safe empty
            //
            // The server returns 401 here for valid logged-in team
            // sessions (notably the background reachability probe from
            // `ensure_ready`). This is a cloud-side authz scope gap, not
            // a client bug — recall stays fail-safe (empty) so a review
            // is never blocked. We no longer SILENTLY swallow it: emit a
            // diagnostic through the same `cloud_debug_enabled()` gate
            // every other degraded path in this file uses, deduped once
            // per process so `doctor`/`plan`/`recall` aren't spammed with
            // a scary-looking 401.
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
            // Gate the verbose, scary-looking error behind debug so unrelated
            // CLI commands (doctor, plan, etc.) don't spam users with a
            // probe-failure message that has no bearing on their request.
            // The first failure per process still surfaces a faded one-liner
            // so genuine outages remain discoverable.
            static NOTIFIED_OTHER: std::sync::OnceLock<()> = std::sync::OnceLock::new();
            NOTIFIED_OTHER.get_or_init(|| {
                if cloud_debug_enabled() {
                    eprintln!(
                        "[difflore] Past-verdict recall unavailable ({status}). \
                         Continuing with local rules. Cloud response: {body}"
                    );
                } else {
                    eprintln!(
                        "[difflore] Past-verdict recall unavailable ({status}); \
                         continuing with local rules. Set DIFFLORE_DEBUG_CLOUD=1 for details."
                    );
                }
            });
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
        // attempt whose result (Ok or Err) is returned directly — so there is no
        // stored-then-unwrapped "last error" to reason about.
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
    /// `true` iff the server accepted the payload (2xx). All of our upload
    /// endpoints are fire-and-forget — transient failure must NOT bubble
    /// up and kill the review/fix pipeline. Callers that want to know
    /// success can inspect the bool; most ignore it.
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
    ) -> Result<(), String> {
        if !self.is_logged_in() {
            return Err(format!("{endpoint_label} skipped: not logged in"));
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
        ))
    }

    /// Sibling of [`post_fire_and_forget_result`] that surfaces the
    /// HTTP status + body fragment as structured data so the outbox
    /// can write a greppable `last_error`.
    ///
    /// Returns `Ok(())` on 2xx, `Err(OutboxFailure::Http(_))` for
    /// any non-2xx response (including 401 after the standard
    /// one-shot refresh+retry has been exhausted), and
    /// `Err(OutboxFailure::Transport(_))` when the request never
    /// produced a status — transport / DNS / TLS / timeout.
    ///
    /// Intentionally **does not** log anything to stdout/stderr: the
    /// response body lands only in the returned struct, which the
    /// caller writes into the local SQLite `last_error` column. The
    /// fire-and-forget `bool` API stays unchanged for non-outbox callers.
    async fn post_fire_and_forget_outcome<T: serde::Serialize>(
        &self,
        path: &str,
        body: &T,
        endpoint_label: &'static str,
    ) -> Result<(), OutboxFailure> {
        if !self.is_logged_in() {
            // Pre-flight "not logged in" surfaces as a transport-
            // class failure: there is no HTTP status to attach, and
            // the remediation ("run `difflore cloud login`") is the
            // same shape as other unreachable-cloud states.
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
        // `canonical_reason()` is None only for unassigned status
        // numbers; fall back to the numeric form so `last_error` is
        // never an empty phrase.
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

    /// Outbox-friendly wrapper around [`save_trajectory`]: same
    /// payload + endpoint, but returns the rich [`OutboxFailure`]
    /// instead of a `bool` so the queue's `last_error` can carry the
    /// upstream status + body fragment.
    pub(crate) async fn save_trajectory_outcome(
        &self,
        pr_review_id: &str,
        steps: serde_json::Value,
    ) -> Result<(), OutboxFailure> {
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
    ) -> Result<(), OutboxFailure> {
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
    ) -> Result<(), OutboxFailure> {
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
    ) -> Result<(), OutboxFailure> {
        self.post_fire_and_forget_outcome("/reviews/import", req, "upload_imported_reviews")
            .await
    }

    /// Outbox-friendly wrapper around [`post_observations`].
    pub(crate) async fn post_observations_outcome(
        &self,
        batch: &[super::api_types::Observation],
    ) -> Result<(), OutboxFailure> {
        self.post_fire_and_forget_outcome("/cloud/observations", &batch, "post_observations")
            .await
    }

    /// POST `/reviews/{id}/metrics`.
    ///
    /// Uploads token usage + estimated cost + wall-clock duration for a
    /// single review run. All fields on the request are optional; pass
    /// `None` for anything the engine doesn't have and the server leaves
    /// that column alone.
    ///
    /// This is fire-and-forget — a failed metrics upload must NEVER block
    /// a review. Returns `true` on 2xx so unit tests can assert the happy
    /// path, but every production caller should ignore the bool.
    pub async fn record_review_metrics(
        &self,
        review_id: &str,
        req: RecordReviewMetricsRequest,
    ) -> bool {
        let path = format!("/reviews/{}/metrics", escape_path_id(review_id));
        self.post_fire_and_forget(&path, &req, "record_review_metrics")
            .await
    }

    /// POST `/reviews/{prReviewId}/trajectory`.
    ///
    /// Takes the serialized output of `TrajectoryBuilder::into_json()` and
    /// hands it to the cloud side, which parses it with a Zod discriminated
    /// union that mirrors the Rust enum in `review_trajectory.rs` exactly.
    /// Fire-and-forget — a missing trajectory is never a review blocker.
    pub async fn save_trajectory(&self, pr_review_id: &str, steps: serde_json::Value) -> bool {
        let req = SaveTrajectoryRequest { steps };
        let path = format!("/reviews/{}/trajectory", escape_path_id(pr_review_id));
        self.post_fire_and_forget(&path, &req, "save_trajectory")
            .await
    }

    /// GET `/reviews/{prReviewId}/trajectory` for the
    /// `difflore trajectory <review-id>` CLI renderer.
    ///
    /// Sibling of [`save_trajectory`]: it fetches the recorded decision
    /// trail for one review and decodes it back into the canonical
    /// [`crate::review_trajectory::TrajectoryStep`] enum. Unlike the recall
    /// path this is **not** fail-safe-to-empty — the caller (an explicit,
    /// user-invoked `trajectory` command) wants to know *why* a fetch
    /// failed (not logged in, plan-gated, review not found, network down)
    /// so it can print an actionable message. We therefore surface the
    /// underlying error string from [`get_json`] verbatim:
    ///
    /// * `"not_logged_in"` when no cloud token is present, and
    /// * `"[get_trajectory] returned 4xx: …"` for plan-limit / not-found /
    ///   other HTTP failures.
    ///
    /// The cloud returns a zero-UUID placeholder with `steps: []` (not a
    /// 404) when a review exists but has no persisted trajectory, so an
    /// `Ok` with an empty `steps` vec is the "nothing recorded yet" signal
    /// the renderer handles gracefully.
    pub async fn get_trajectory(
        &self,
        pr_review_id: &str,
    ) -> Result<GetTrajectoryResponse, String> {
        let path = format!("/reviews/{}/trajectory", escape_path_id(pr_review_id));
        self.get_json(&path, "get_trajectory").await
    }

    /// POST `/dashboard/mcp-query` — live agent-activity telemetry.
    ///
    /// Called every time the MCP server answers a canonical rule-search tool
    /// invocation. The cloud side appends a row to `metric_events` and the
    /// dashboard's "Recent agent activity" card polls for new entries. Fire-
    /// and-forget — a missing telemetry hit must never block the MCP response
    /// (agents time out on slow MCP tools).
    ///
    /// Kept flat to avoid churn in the outbox payload schema and callers.
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
        // Keep the payload small: cap titles/ids at 10 (server rejects >10).
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

    /// POST `/accepted-edits`.
    ///
    /// Called when the user locally accepts an edit (IDE / CLI). The cloud
    /// side inserts a `fix_acceptances` row which the `rule-promoter`
    /// worker later aggregates into candidate rules. Fire-and-forget: a
    /// failed acceptance POST must never block the local accept UX.
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

    /// POST `/accepted-edits` and return the cloud attribution details.
    ///
    /// The response tells the CLI whether the raw accepted edit was also
    /// linked to a team-scoped `fix_outcome` observation. That distinction
    /// matters for Impact evidence: raw accepted rows prove usage, but
    /// rule-linked observations prove the live fix path reused review memory.
    pub async fn record_accepted_edit_response(
        &self,
        req: RecordAcceptedEditRequest,
    ) -> Result<RecordAcceptedEditResponse, String> {
        self.post_json("/accepted-edits", &req, "record_accepted_edit")
            .await
    }

    /// POST `/reviews/import` — GitHub PR History Import.
    ///
    /// Uploads locally-imported PR review comments to the cloud for
    /// team-wide recall and analytics. Fire-and-forget — a failed upload
    /// must never block the local import pipeline.
    pub async fn upload_imported_reviews(&self, req: &UploadImportedReviewsRequest) -> bool {
        self.post_fire_and_forget("/reviews/import", req, "upload_imported_reviews")
            .await
    }

    pub async fn post_observations(&self, batch: &[super::api_types::Observation]) -> bool {
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
    ) -> Result<(), String> {
        self.post_fire_and_forget_result("/cloud/observations", &batch, "post_observation_events")
            .await
    }

    /// Scrub the construction-time bearer, the current saved bearer (possibly
    /// refreshed mid-call), AND the refresh token from a response body before
    /// it is surfaced in an error. Async because it reads the latest saved
    /// tokens — only hit on rare non-2xx error paths.
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
    ) -> Result<T, String> {
        if !self.is_logged_in() {
            return Err("not_logged_in".to_owned());
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
            ));
        }
        resp.json::<T>()
            .await
            .map_err(|e| format!("[{label}] decode error: {e}"))
    }

    /// POST helper that, unlike `post_fire_and_forget`, returns the
    /// decoded body. Used by interactive endpoints (knowledge corpora,
    /// future synchronous CLI flows) where the user wants the answer,
    /// not just success/failure.
    async fn post_json<B: serde::Serialize, R: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
        label: &'static str,
    ) -> Result<R, String> {
        if !self.is_logged_in() {
            return Err("not_logged_in".to_owned());
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
            ));
        }
        resp.json::<R>()
            .await
            .map_err(|e| format!("[{label}] decode error: {e}"))
    }

    /// POST `/knowledge/corpus` — build a filtered snapshot of rules +
    /// extractions into a new corpus. Cloud spec §3.16.
    pub async fn build_corpus(
        &self,
        req: &super::api_types::BuildCorpusRequest,
    ) -> Result<super::api_types::BuildCorpusResult, String> {
        self.post_json("/knowledge/corpus", req, "build_corpus")
            .await
    }

    /// POST `/knowledge/corpus/{id}/prime` — allocate a session token
    /// and mark the corpus primed. Returns the session token + ISO ts.
    pub async fn prime_corpus(
        &self,
        corpus_id: &str,
    ) -> Result<super::api_types::PrimeCorpusResult, String> {
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
    ) -> Result<super::api_types::QueryCorpusResult, String> {
        let path = format!("/knowledge/corpus/{corpus_id}/query");
        let body = super::api_types::QueryCorpusRequest {
            question: question.to_owned(),
        };
        self.post_json(&path, &body, "query_corpus").await
    }

    /// GET `/knowledge/corpora` — list this team's corpora with item
    /// counts and prime/query timestamps.
    pub async fn list_corpora(&self) -> Result<Vec<super::api_types::CorpusSummary>, String> {
        self.get_json("/knowledge/corpora", "list_corpora").await
    }

    /// GET `/impact/banner` — past verdicts recalled into reviews this week.
    pub async fn get_impact_banner(&self) -> Result<ImpactBannerDto, String> {
        self.get_json("/impact/banner", "impact_banner").await
    }

    /// GET `/impact/weekly` — last 12 weeks of rules / verdicts / fixes.
    pub async fn get_impact_weekly(&self) -> Result<ImpactWeeklyDto, String> {
        self.get_json("/impact/weekly", "impact_weekly").await
    }

    /// GET `/impact/top-rules` — top 5 candidate rules across user's teams.
    pub async fn get_impact_top_rules(&self) -> Result<ImpactTopRulesDto, String> {
        self.get_json("/impact/top-rules", "impact_top_rules").await
    }

    /// GET `/impact/coverage` — repos / PRs / files covered by extractions.
    pub async fn get_impact_coverage(&self) -> Result<ImpactCoverageDto, String> {
        self.get_json("/impact/coverage", "impact_coverage").await
    }

    /// GET `/impact/fix-scorecard` — last 30d fix acceptance rate + trend.
    pub async fn get_impact_fix_scorecard(&self) -> Result<ImpactFixScorecardDto, String> {
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
        let _home = crate::db::shared_test_home();
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
