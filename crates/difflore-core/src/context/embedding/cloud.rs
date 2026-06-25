use async_trait::async_trait;
use std::time::Duration;

use crate::error::CoreError;

use super::{
    EMBEDDING_RETRY_DELAYS_MS, Embedder, embedding_http_client, parse_embedding_vector,
    retryable_embedding_status,
};

/// Cloud-managed embedder. POSTs `{ texts: [..] }` to the cloud API's
/// `/api/embeddings` endpoint, authenticating with the user's CLI session token
/// (the same `cloud-auth.db` row as `cloud::client::CloudClient`).
///
/// The Free-tier path: the cloud forwards to its own embedding provider, so
/// users need no local OpenAI key. Failures (network / 401 / 5xx) bubble up as
/// `CoreError::Internal` so the caller can fall back to local SHA1 after retry.
pub struct CloudEmbedder {
    base_url: String,
    token: String,
    client: reqwest::Client,
}

impl CloudEmbedder {
    pub fn new(base_url: String, token: String) -> Self {
        Self {
            base_url,
            token,
            client: embedding_http_client(),
        }
    }

    pub(crate) fn endpoint(&self) -> String {
        format!("{}/embeddings", self.base_url.trim_end_matches('/'))
    }

    async fn post_embedding(
        &self,
        token: &str,
        body: &serde_json::Value,
    ) -> Result<reqwest::Response, CoreError> {
        self.client
            .post(self.endpoint())
            .bearer_auth(token)
            .json(body)
            .send()
            .await
            .map_err(|e| CoreError::Internal(format!("cloud embedding request failed: {e}")))
    }

    async fn post_embedding_with_transport_retry(
        &self,
        token: &str,
        body: &serde_json::Value,
    ) -> Result<reqwest::Response, CoreError> {
        let mut last_error = String::new();
        for attempt in 0..=EMBEDDING_RETRY_DELAYS_MS.len() {
            match self.post_embedding(token, body).await {
                Ok(resp) => return Ok(resp),
                Err(error) => {
                    last_error = error.to_string();
                    if let Some(delay_ms) = EMBEDDING_RETRY_DELAYS_MS.get(attempt) {
                        tokio::time::sleep(Duration::from_millis(*delay_ms)).await;
                    }
                }
            }
        }
        Err(CoreError::Internal(format!(
            "cloud embedding request failed after {} transport attempts: {last_error}",
            EMBEDDING_RETRY_DELAYS_MS.len() + 1
        )))
    }
}

#[async_trait]
impl Embedder for CloudEmbedder {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, CoreError> {
        let single = vec![text.to_owned()];
        let mut vectors = self.embed_batch(&single, None).await?;
        return vectors.pop().ok_or_else(|| {
            CoreError::Internal("cloud embedding response missing first vector".into())
        });
    }

    async fn embed_batch(
        &self,
        texts: &[String],
        rule_ids: Option<&[String]>,
    ) -> Result<Vec<Vec<f32>>, CoreError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let body = serde_json::json!({ "texts": texts });
        let body = if let Some(rule_ids) = rule_ids {
            let mut value = body;
            value["rule_ids"] = serde_json::json!(rule_ids);
            value
        } else {
            body
        };
        let mut active_token = self.token.clone();
        let mut resp = self
            .post_embedding_with_transport_retry(&active_token, &body)
            .await?;

        let mut status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED
            && let Some(refreshed_token) =
                crate::cloud::client::CloudClient::refresh_saved_token().await
        {
            active_token = refreshed_token;
            resp = self
                .post_embedding_with_transport_retry(&active_token, &body)
                .await?;
            status = resp.status();
        }
        for delay_ms in EMBEDDING_RETRY_DELAYS_MS {
            if !retryable_embedding_status(status) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(*delay_ms)).await;
            resp = self
                .post_embedding_with_transport_retry(&active_token, &body)
                .await?;
            status = resp.status();
        }
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            // 409 `embed_cap_reached` is the Free-tier rule cap. Surfaced as a
            // typed error (not `Internal`) so the caller can fall back to
            // lexical retrieval for this call and record a doctor activity event.
            if status.as_u16() == 409
                && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&body_text)
                && parsed.get("code").and_then(|c| c.as_str()) == Some("embed_cap_reached")
            {
                let cap = u32::try_from(
                    parsed
                        .get("cap")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0),
                )
                .unwrap_or(u32::MAX);
                let used = u32::try_from(
                    parsed
                        .get("used")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0),
                )
                .unwrap_or(u32::MAX);
                crate::observability::activity_stream::record(
                    crate::observability::activity_stream::ActivityPayload::EmbedCapReached {
                        cap,
                        used,
                    },
                );
                return Err(CoreError::EmbedCapReached { cap, used });
            }
            return Err(CoreError::Internal(format!(
                "cloud embedding endpoint returned {status}; semantic recall will fall back to file-pattern and keyword matching"
            )));
        }

        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| CoreError::Internal(format!("cloud embedding decode error: {e}")))?;

        let vectors = json
            .get("vectors")
            .and_then(|v| v.as_array())
            .ok_or_else(|| CoreError::Internal("cloud embedding response missing vectors".into()))?
            .iter()
            .map(|vector| {
                vector
                    .as_array()
                    .ok_or_else(|| {
                        CoreError::Internal("cloud embedding vector is not an array".into())
                    })
                    .and_then(|items| parse_embedding_vector(items, "cloud embedding vector"))
            })
            .collect::<Result<Vec<Vec<f32>>, CoreError>>()?;
        if vectors.len() != texts.len() {
            return Err(CoreError::Internal(format!(
                "cloud embedding response length mismatch: expected {}, got {}",
                texts.len(),
                vectors.len()
            )));
        }
        Ok(vectors)
    }

    fn dim(&self) -> usize {
        0
    }
}
