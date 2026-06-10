use async_trait::async_trait;

use crate::errors::CoreError;

use super::{EMBEDDING_DIM, Embedder, embed_text};

/// Hash-based fallback embedder. Deterministic, offline, and fast. Not
/// semantically meaningful but keeps retrieval working without network or
/// model configuration.
pub struct Sha1Embedder;

impl Sha1Embedder {
    pub const fn new() -> Self {
        Self
    }
}

impl Default for Sha1Embedder {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Embedder for Sha1Embedder {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, CoreError> {
        Ok(embed_text(text))
    }

    async fn embed_batch(
        &self,
        texts: &[String],
        _rule_ids: Option<&[String]>,
    ) -> Result<Vec<Vec<f32>>, CoreError> {
        Ok(texts.iter().map(|text| embed_text(text)).collect())
    }

    fn dim(&self) -> usize {
        EMBEDDING_DIM
    }

    fn is_semantic(&self) -> bool {
        // SHA1 bag-of-words is deterministic but carries no semantic signal, so
        // hybrid retrieval shifts RRF weight toward the FTS baseline.
        false
    }
}
