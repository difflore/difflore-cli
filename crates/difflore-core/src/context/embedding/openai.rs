use async_trait::async_trait;

use crate::errors::CoreError;

use super::{Embedder, embedding_http_client};

/// OpenAI-compatible embedding provider.
///
/// Works with any backend that speaks the `OpenAI` `/embeddings` shape
/// (`OpenAI`, Azure `OpenAI`, Together, `DeepInfra`, etc.).
pub struct OpenAICompatEmbedder {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub dim: usize,
    client: reqwest::Client,
}

impl OpenAICompatEmbedder {
    pub fn new(base_url: String, api_key: String, model: String, dim: usize) -> Self {
        Self {
            base_url,
            api_key,
            model,
            dim,
            client: embedding_http_client(),
        }
    }

    pub(crate) fn endpoint(&self) -> String {
        let trimmed = self.base_url.trim_end_matches('/');
        if trimmed.ends_with("/embeddings") {
            trimmed.to_owned()
        } else {
            format!("{trimmed}/embeddings")
        }
    }

    /// Build a POST request, attaching `Authorization: Bearer` only when a key
    /// is configured. Keyless local providers (configured via
    /// `difflore embeddings setup --no-key`) can reject any auth header, so an
    /// empty key must send no header at all.
    fn authed_post(&self, url: &str) -> reqwest::RequestBuilder {
        let request = self.client.post(url);
        if self.api_key.is_empty() {
            request
        } else {
            request.bearer_auth(&self.api_key)
        }
    }
}

fn provider_status_error(status: reqwest::StatusCode) -> CoreError {
    CoreError::Internal(format!(
        "embedding provider returned {status}; check provider URL, model, dimensions, and API key"
    ))
}

#[async_trait]
impl Embedder for OpenAICompatEmbedder {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, CoreError> {
        let url = self.endpoint();
        // We deliberately do NOT send a `dimensions` parameter: many valid
        // OpenAI-compatible models (e.g. text-embedding-ada-002) and strict
        // local providers reject it, which would break configs whose `--dim`
        // already matches the model's native size. Instead we validate the
        // returned length below, so a mismatched `--dim` surfaces a clear error
        // rather than silently storing wrong-length vectors.
        let body = serde_json::json!({
            "model": self.model,
            "input": text,
        });

        let resp = self
            .authed_post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| CoreError::Internal(format!("embedding request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            return Err(provider_status_error(status));
        }

        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| CoreError::Internal(format!("embedding response parse error: {e}")))?;

        let vec = json
            .get("data")
            .and_then(|d| d.get(0))
            .and_then(|d| d.get("embedding"))
            .and_then(|e| e.as_array())
            .ok_or_else(|| {
                CoreError::Internal("embedding response missing data[0].embedding".into())
            })?
            .iter()
            .map(|v| v.as_f64().unwrap_or(0.0) as f32)
            .collect::<Vec<f32>>();

        // Refuse a length that disagrees with the configured profile rather than
        // storing mismatched-length vectors under a `byok:<host>:<model>:<dim>`
        // profile. The actionable message points at the fix.
        if vec.len() != self.dim {
            return Err(CoreError::Internal(format!(
                "embedding provider returned {} dimensions but {} are configured; \
                 re-run `difflore embeddings setup --dim {}` to match your provider/model",
                vec.len(),
                self.dim,
                vec.len()
            )));
        }

        Ok(vec)
    }

    async fn embed_batch(
        &self,
        texts: &[String],
        _rule_ids: Option<&[String]>,
    ) -> Result<Vec<Vec<f32>>, CoreError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        // OpenAI-compatible APIs accept batched `input`, which keeps BYOK indexing
        // inside the bounded recall/fix/MCP timeouts.
        let body = serde_json::json!({
            "model": self.model,
            "input": texts,
        });
        let resp = self
            .authed_post(&self.endpoint())
            .json(&body)
            .send()
            .await
            .map_err(|e| CoreError::Internal(format!("embedding request failed: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            return Err(provider_status_error(status));
        }
        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| CoreError::Internal(format!("embedding response parse error: {e}")))?;
        let data = json
            .get("data")
            .and_then(|d| d.as_array())
            .ok_or_else(|| CoreError::Internal("embedding response missing data array".into()))?;
        if data.len() != texts.len() {
            return Err(CoreError::Internal(format!(
                "embedding response length mismatch: expected {}, got {}",
                texts.len(),
                data.len()
            )));
        }
        // OpenAI returns each item with an `index`; order is normally preserved
        // but we sort defensively so vectors line up with the input texts.
        let mut indexed: Vec<(usize, Vec<f32>)> = Vec::with_capacity(data.len());
        for item in data {
            let index = item
                .get("index")
                .and_then(serde_json::Value::as_u64)
                .map_or(indexed.len(), |i| i as usize);
            let vector = item
                .get("embedding")
                .and_then(|e| e.as_array())
                .ok_or_else(|| {
                    CoreError::Internal("embedding response item missing embedding array".into())
                })?
                .iter()
                .map(|v| v.as_f64().unwrap_or(0.0) as f32)
                .collect::<Vec<f32>>();
            if vector.len() != self.dim {
                return Err(CoreError::Internal(format!(
                    "embedding provider returned {} dimensions but {} are configured; \
                     re-run `difflore embeddings setup --dim {}` to match your provider/model",
                    vector.len(),
                    self.dim,
                    vector.len()
                )));
            }
            indexed.push((index, vector));
        }
        indexed.sort_by_key(|(index, _)| *index);
        Ok(indexed.into_iter().map(|(_, vector)| vector).collect())
    }

    fn dim(&self) -> usize {
        self.dim
    }
}

#[cfg(test)]
mod tests {
    use super::provider_status_error;

    #[test]
    fn provider_status_error_does_not_echo_response_body() {
        let err = provider_status_error(reqwest::StatusCode::UNAUTHORIZED).to_string();

        assert!(err.contains("401"));
        assert!(err.contains("check provider URL"));
        assert!(!err.contains("Authorization"));
        assert!(!err.contains("sk-"));
    }
}
