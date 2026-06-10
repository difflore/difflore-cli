use async_trait::async_trait;

use crate::contract::RecallPastVerdictsRequest;
use crate::cloud::client::CloudClient;
use crate::context::types::{PastVerdict, PastVerdictScope};
use crate::error::CoreError;

/// Async seam so tests can substitute a fake cloud recall without hitting the
/// network. Implemented for `CloudClient` so real call sites use it directly.
#[async_trait]
pub trait PastVerdictRecaller: Send + Sync {
    async fn recall(&self, req: RecallPastVerdictsRequest) -> Result<Vec<PastVerdict>, CoreError>;
}

#[async_trait]
impl PastVerdictRecaller for CloudClient {
    async fn recall(&self, req: RecallPastVerdictsRequest) -> Result<Vec<PastVerdict>, CoreError> {
        self.recall_past_verdicts(req).await
    }
}

/// Retrieve past verdicts for the current review chunk.
///
/// A failing recall must never block a review: errors are logged and
/// downgraded to an empty `Vec`. Gating is handled upstack; this is the pure
/// retrieval step.
pub async fn retrieve_past_verdicts<R: PastVerdictRecaller + ?Sized>(
    cloud: &R,
    chunk_embedding: &[f32],
    repo_id: Option<&str>,
    scope: PastVerdictScope,
    k: u32,
    target_file: Option<&str>,
) -> Vec<PastVerdict> {
    retrieve_past_verdicts_with_team(cloud, chunk_embedding, repo_id, scope, k, target_file, None)
        .await
}

/// Embedding-based recall variant with optional team scope metadata.
pub async fn retrieve_past_verdicts_with_team<R: PastVerdictRecaller + ?Sized>(
    cloud: &R,
    chunk_embedding: &[f32],
    repo_id: Option<&str>,
    scope: PastVerdictScope,
    k: u32,
    target_file: Option<&str>,
    team_id: Option<&str>,
) -> Vec<PastVerdict> {
    let req = RecallPastVerdictsRequest {
        embedding: chunk_embedding.to_vec(),
        query_text: None,
        repo_id: repo_id.map(ToOwned::to_owned),
        scope: scope.as_str().to_owned(),
        team_id: team_id.map(ToOwned::to_owned),
        k,
        target_file: target_file.map(ToOwned::to_owned),
    };
    match cloud.recall(req).await {
        Ok(v) => v,
        Err(e) => {
            if crate::infra::env::debug_cloud() {
                eprintln!("[retrieve_past_verdicts] recall failed: {e:?}");
            }
            Vec::new()
        }
    }
}

/// Merge multiple groups of past-verdict recalls into one ranked list.
///
/// De-dupes by `extraction_id` (higher-similarity copy wins), sorts descending
/// by similarity, and truncates to `limit`.
pub fn merge_past_verdicts(
    groups: impl IntoIterator<Item = Vec<PastVerdict>>,
    limit: usize,
) -> Vec<PastVerdict> {
    let mut by_id: std::collections::HashMap<String, PastVerdict> =
        std::collections::HashMap::new();
    for group in groups {
        for verdict in group {
            match by_id.get(&verdict.extraction_id) {
                Some(existing) if existing.similarity >= verdict.similarity => {}
                _ => {
                    by_id.insert(verdict.extraction_id.clone(), verdict);
                }
            }
        }
    }

    let mut merged: Vec<_> = by_id.into_values().collect();
    merged.sort_by(|a, b| {
        b.similarity
            .total_cmp(&a.similarity)
            .then_with(|| a.extraction_id.cmp(&b.extraction_id))
    });
    merged.truncate(limit);
    merged
}

/// Text-based recall variant. The server embeds the query itself, avoiding the
/// client/server algorithm and dimensionality drift that affects the
/// `chunk_embedding` path when the client lacks a 1536-dim embedder.
pub async fn retrieve_past_verdicts_by_text<R: PastVerdictRecaller + ?Sized>(
    cloud: &R,
    query_text: &str,
    repo_id: Option<&str>,
    scope: PastVerdictScope,
    k: u32,
    target_file: Option<&str>,
) -> Vec<PastVerdict> {
    retrieve_past_verdicts_by_text_with_team(
        cloud,
        query_text,
        repo_id,
        scope,
        k,
        target_file,
        None,
    )
    .await
}

/// Text-based recall variant with optional team scope metadata.
pub async fn retrieve_past_verdicts_by_text_with_team<R: PastVerdictRecaller + ?Sized>(
    cloud: &R,
    query_text: &str,
    repo_id: Option<&str>,
    scope: PastVerdictScope,
    k: u32,
    target_file: Option<&str>,
    team_id: Option<&str>,
) -> Vec<PastVerdict> {
    let req = RecallPastVerdictsRequest {
        embedding: Vec::new(),
        query_text: Some(query_text.to_owned()),
        repo_id: repo_id.map(ToOwned::to_owned),
        scope: scope.as_str().to_owned(),
        team_id: team_id.map(ToOwned::to_owned),
        k,
        target_file: target_file.map(ToOwned::to_owned),
    };
    match cloud.recall(req).await {
        Ok(v) => v,
        Err(e) => {
            if crate::infra::env::debug_cloud() {
                eprintln!("[retrieve_past_verdicts_by_text] recall failed: {e:?}");
            }
            Vec::new()
        }
    }
}
