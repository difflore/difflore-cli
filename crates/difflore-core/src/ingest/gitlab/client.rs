//! Minimal GitLab REST v4 client for review import.
//!
//! Deliberately not `glab`-based: self-managed enterprise instances often
//! forbid installing extra CLIs, while a PAT + plain HTTPS is universally
//! allowed. TLS uses the workspace-default reqwest backend (same as the
//! cloud client and `difflore auth gitlab --check`), so a private CA must be
//! trusted at the OS level — there is no insecure-skip option.
//!
//! Retry policy mirrors the GitHub importer's skeleton
//! (`ingest::github::sleep_before_graphql_retry`): up to 4 attempts on 429 /
//! 5xx with 5s/10s/20s/40s exponential backoff, except a `Retry-After`
//! header (GitLab sends one on 429) overrides the computed delay. Pagination
//! follows the `x-next-page` response header.

use std::time::Duration;

use serde::de::DeserializeOwned;

use crate::error::CoreError;

use super::schema::{Discussion, MergeRequest};

const GITLAB_API_TIMEOUT_SECS: u64 = 45;
const MAX_ATTEMPTS: usize = 4;
/// MR list page size. Modest because every listed MR costs a follow-up
/// discussions request anyway.
const MR_PAGE_SIZE: usize = 50;
const DISCUSSIONS_PAGE_SIZE: usize = 100;
/// Upper bound honored for `Retry-After`: respect the server's pacing but
/// never let one header park the CLI for minutes.
const RETRY_AFTER_CAP_SECS: u64 = 120;

pub(super) struct GitlabClient {
    http: reqwest::Client,
    /// `https://{host}` — host may carry a port for self-managed instances.
    base: String,
    token: String,
}

impl GitlabClient {
    pub(super) fn new(host: &str, token: &str) -> crate::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(GITLAB_API_TIMEOUT_SECS))
            .build()
            .map_err(|e| CoreError::Internal(format!("failed to build GitLab HTTP client: {e}")))?;
        Ok(Self {
            http,
            base: format!("https://{host}"),
            token: token.to_owned(),
        })
    }

    /// List merged MRs, newest-updated first, up to `max` items.
    /// `updated_after` is an ISO8601 timestamp pushed server-side.
    pub(super) async fn list_merged_merge_requests(
        &self,
        project_path: &str,
        updated_after: Option<&str>,
        max: usize,
    ) -> crate::Result<Vec<MergeRequest>> {
        let project = encode_project_path(project_path);
        let mut path = format!(
            "/api/v4/projects/{project}/merge_requests?state=merged&order_by=updated_at&sort=desc&per_page={MR_PAGE_SIZE}"
        );
        if let Some(updated_after) = updated_after {
            path.push_str(&format!("&updated_after={updated_after}"));
        }
        self.get_paged(&path, Some(max)).await
    }

    /// Fetch one MR by IID. `Ok(None)` when GitLab answers 404 — for private
    /// projects without access GitLab also returns 404 (not 403), so a
    /// missing IID and a permission gap are indistinguishable here; the
    /// caller reports both as "missing/inaccessible".
    pub(super) async fn get_merge_request(
        &self,
        project_path: &str,
        iid: i32,
    ) -> crate::Result<Option<MergeRequest>> {
        let project = encode_project_path(project_path);
        let path = format!("/api/v4/projects/{project}/merge_requests/{iid}");
        let response = self.get_with_retry(&path).await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        read_json::<MergeRequest>(response, &path).await.map(Some)
    }

    /// All discussions (threads) of an MR, oldest-first, following
    /// `x-next-page` until exhausted.
    pub(super) async fn list_discussions(
        &self,
        project_path: &str,
        iid: i64,
    ) -> crate::Result<Vec<Discussion>> {
        let project = encode_project_path(project_path);
        let path = format!(
            "/api/v4/projects/{project}/merge_requests/{iid}/discussions?per_page={DISCUSSIONS_PAGE_SIZE}"
        );
        self.get_paged(&path, None).await
    }

    /// Preflight: confirm the project is visible to this token before any
    /// import work. Maps straight onto `GET /api/v4/projects/:id`.
    pub(super) async fn check_project_access(&self, project_path: &str) -> crate::Result<()> {
        let project = encode_project_path(project_path);
        let path = format!("/api/v4/projects/{project}");
        let response = self.get_with_retry(&path).await?;
        let status = response.status();
        if status.is_success() {
            Ok(())
        } else {
            Err(CoreError::Internal(status_error(status, &path)))
        }
    }

    /// Paginated GET returning the concatenated items. Stops early once
    /// `max_items` is reached.
    async fn get_paged<T: DeserializeOwned>(
        &self,
        path_and_query: &str,
        max_items: Option<usize>,
    ) -> crate::Result<Vec<T>> {
        let mut out: Vec<T> = Vec::new();
        let mut page: u64 = 1;
        loop {
            let url_path = format!("{path_and_query}&page={page}");
            let response = self.get_with_retry(&url_path).await?;
            let status = response.status();
            if !status.is_success() {
                return Err(CoreError::Internal(status_error(status, path_and_query)));
            }
            let next_page = next_page_number(response.headers().get("x-next-page"));
            let items: Vec<T> = read_json(response, path_and_query).await?;
            out.extend(items);
            if let Some(max) = max_items
                && out.len() >= max
            {
                out.truncate(max);
                break;
            }
            match next_page {
                Some(next) => page = next,
                None => break,
            }
        }
        Ok(out)
    }

    /// One GET with the retry policy applied. Returns the final response for
    /// BOTH success and non-retryable error statuses (callers branch on the
    /// status, e.g. 404 → "missing MR"); `Err` is reserved for transport
    /// failures and exhausted retries.
    async fn get_with_retry(&self, url_path: &str) -> crate::Result<reqwest::Response> {
        let mut last_retryable: Option<String> = None;
        for attempt in 0..MAX_ATTEMPTS {
            let result = self
                .http
                .get(format!("{}{url_path}", self.base))
                .header("PRIVATE-TOKEN", &self.token)
                .send()
                .await;
            let response = match result {
                Ok(response) => response,
                Err(e) => {
                    let message = format!("GitLab API request failed for GET {url_path}: {e}");
                    // Timeouts are the one transport failure worth retrying;
                    // TLS/DNS/connect errors won't fix themselves in 40s and
                    // the CLI maps them to actionable recovery text instead.
                    if e.is_timeout() && attempt + 1 < MAX_ATTEMPTS {
                        last_retryable = Some(message);
                        tokio::time::sleep(retry_delay(attempt, None)).await;
                        continue;
                    }
                    return Err(CoreError::Internal(message));
                }
            };
            let status = response.status();
            if !is_retryable_status(status) {
                return Ok(response);
            }
            let message = status_error(status, url_path);
            if attempt + 1 < MAX_ATTEMPTS {
                let retry_after = parse_retry_after(response.headers().get("retry-after"));
                last_retryable = Some(message);
                tokio::time::sleep(retry_delay(attempt, retry_after)).await;
                continue;
            }
            return Err(CoreError::Internal(message));
        }
        Err(CoreError::Internal(last_retryable.unwrap_or_else(|| {
            format!("GitLab API request failed for GET {url_path}")
        })))
    }
}

/// Read and deserialize a success response body, with a truncated-body error
/// on parse failure so triage stays possible without dumping megabytes.
async fn read_json<T: DeserializeOwned>(
    response: reqwest::Response,
    url_path: &str,
) -> crate::Result<T> {
    let status = response.status();
    if !status.is_success() {
        return Err(CoreError::Internal(status_error(status, url_path)));
    }
    let body = response
        .text()
        .await
        .map_err(|e| CoreError::Internal(format!("GitLab API read failed for {url_path}: {e}")))?;
    serde_json::from_str(&body).map_err(|e| {
        CoreError::Internal(format!(
            "Failed to parse GitLab response for {url_path}: {e}: {}",
            truncate_chars(&body, 200)
        ))
    })
}

/// Stable error string for an HTTP error status. The CLI error mapper keys
/// on the `HTTP {status}` fragment (401/403/404/429/5xx), so keep the shape
/// stable.
fn status_error(status: reqwest::StatusCode, url_path: &str) -> String {
    format!("GitLab API error: HTTP {status} for GET {url_path}")
}

fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    status.is_server_error() || status.as_u16() == 429
}

/// Backoff before retry `attempt` (0-based): a server-provided `Retry-After`
/// wins (capped), otherwise 5s, 10s, 20s, 40s — the GitHub importer's ladder.
fn retry_delay(attempt: usize, retry_after_secs: Option<u64>) -> Duration {
    let secs = retry_after_secs.map_or_else(
        || 5_u64 * (1_u64 << attempt.min(3)),
        |secs| secs.min(RETRY_AFTER_CAP_SECS),
    );
    Duration::from_secs(secs)
}

/// Parse a `Retry-After` header value. Only the delta-seconds form is
/// honored; the HTTP-date form falls back to the exponential ladder.
fn parse_retry_after(value: Option<&reqwest::header::HeaderValue>) -> Option<u64> {
    value?.to_str().ok()?.trim().parse().ok()
}

/// Parse GitLab's `x-next-page` pagination header. GitLab sends an empty
/// value (or omits the header) on the last page.
fn next_page_number(value: Option<&reqwest::header::HeaderValue>) -> Option<u64> {
    let raw = value?.to_str().ok()?.trim();
    if raw.is_empty() {
        return None;
    }
    raw.parse().ok()
}

/// URL-encode a full namespace path for the `/projects/:id` placeholder.
/// [`crate::ingest::provider::validate_gitlab_project_path`] restricts
/// segments to URL-unreserved characters (`A-Za-z0-9._-`), so only the `/`
/// separators need escaping.
pub(super) fn encode_project_path(path: &str) -> String {
    path.replace('/', "%2F")
}

/// UTF-8-safe truncation (same contract as the GitHub importer's helper):
/// `&s[..N]` would panic mid-codepoint on multi-byte bodies.
fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(value: &str) -> reqwest::header::HeaderValue {
        reqwest::header::HeaderValue::from_str(value).expect("valid header value")
    }

    #[test]
    fn project_paths_encode_only_the_separators() {
        assert_eq!(encode_project_path("group/project"), "group%2Fproject");
        assert_eq!(
            encode_project_path("group/sub/my_pro-ject.rs"),
            "group%2Fsub%2Fmy_pro-ject.rs"
        );
    }

    #[test]
    fn retry_delay_uses_exponential_ladder_when_no_retry_after() {
        assert_eq!(retry_delay(0, None), Duration::from_secs(5));
        assert_eq!(retry_delay(1, None), Duration::from_secs(10));
        assert_eq!(retry_delay(2, None), Duration::from_secs(20));
        assert_eq!(retry_delay(3, None), Duration::from_secs(40));
        // Attempt index past the ladder stays at the cap.
        assert_eq!(retry_delay(9, None), Duration::from_secs(40));
    }

    #[test]
    fn retry_after_overrides_the_ladder_but_is_capped() {
        assert_eq!(retry_delay(0, Some(17)), Duration::from_secs(17));
        // A hostile/huge Retry-After cannot park the CLI for minutes.
        assert_eq!(
            retry_delay(0, Some(3600)),
            Duration::from_secs(RETRY_AFTER_CAP_SECS)
        );
    }

    #[test]
    fn retry_after_parses_delta_seconds_and_ignores_http_dates() {
        assert_eq!(parse_retry_after(Some(&header("30"))), Some(30));
        assert_eq!(parse_retry_after(Some(&header(" 5 "))), Some(5));
        // HTTP-date form → None → exponential ladder takes over.
        assert_eq!(
            parse_retry_after(Some(&header("Wed, 21 Oct 2026 07:28:00 GMT"))),
            None
        );
        assert_eq!(parse_retry_after(None), None);
    }

    #[test]
    fn next_page_header_empty_means_last_page() {
        assert_eq!(next_page_number(Some(&header("2"))), Some(2));
        assert_eq!(next_page_number(Some(&header(""))), None);
        assert_eq!(next_page_number(None), None);
    }

    #[test]
    fn retryable_statuses_are_429_and_5xx_only() {
        use reqwest::StatusCode;
        assert!(is_retryable_status(StatusCode::TOO_MANY_REQUESTS));
        assert!(is_retryable_status(StatusCode::BAD_GATEWAY));
        assert!(is_retryable_status(StatusCode::SERVICE_UNAVAILABLE));
        // Auth/permission/missing must fail fast — retrying cannot help and
        // GitLab signals "no access to private project" as 404.
        assert!(!is_retryable_status(StatusCode::UNAUTHORIZED));
        assert!(!is_retryable_status(StatusCode::FORBIDDEN));
        assert!(!is_retryable_status(StatusCode::NOT_FOUND));
        assert!(!is_retryable_status(StatusCode::OK));
    }

    #[test]
    fn status_errors_carry_the_http_code_for_the_cli_mapper() {
        let message = status_error(
            reqwest::StatusCode::NOT_FOUND,
            "/api/v4/projects/group%2Fproject",
        );
        assert!(message.contains("HTTP 404"), "got: {message}");
        assert!(message.contains("/api/v4/projects/"), "got: {message}");
    }
}
