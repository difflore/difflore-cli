//! Query-embedding alignment for hybrid rule retrieval: decides HOW to embed
//! the recall query so the resulting vector can match the persisted index,
//! including the cold-start retry and the sustained-outage fast path.

use crate::context::embedding::{EmbeddedText, embed_text, embed_text_async_with_timeout};
use crate::context::index_db;
use sqlx::SqlitePool;
use std::time::Duration;

/// Embed the recall query in a profile the persisted index can actually match.
///
/// When the index's vector lane is dead relative to the active embedder
/// (`vector_lane_available == false` — e.g. a SHA1 index while the embedder is
/// cloud/BYOK), a remote query embed is futile: the vector lives in a different
/// space than every indexed chunk, so the cosine pass drops them all and recall
/// degrades to FTS-only — worse than the SHA1 hash + FTS hybrid — while paying a
/// doomed round-trip. So embed locally with SHA1 instead, which matches the SHA1
/// index dimension, makes no network call, and stays quiet. The same applies
/// when there is no usable lane at all (a not-yet-built or empty index).
///
/// This keys off the STATIC lane check only, deliberately. A cloud-profiled
/// index whose provider is briefly down still issues a remote embed that times
/// out and falls back to SHA1 per-call: a bounded per-query cost, with immediate
/// recovery to semantic ranking the moment the provider returns.
///
/// `cold_start_retry`: the first query after an idle period hits a cold cloud
/// connection that routinely exceeds the ~2.5s base budget, so recall falls back
/// to SHA1 even on a healthy lane. The interactive `recall`/`search` path (gated
/// on a healthy lane) raises the first attempt's ceiling to
/// [`COLD_RETRY_EMBEDDING_TIMEOUT`] so a cold round-trip completes in one shot
/// (a warm query is unaffected — the budget is a ceiling), then retries once to
/// ride out a transient transport flap before settling for SHA1.
///
/// `cold_start_retry` is only for human-waiting CLI paths. Latency-critical
/// MCP/hook callers bypass this function with local query embeddings, so a cold
/// provider never sits in the agent's tool-call path.
/// If a caller reaches this function while the current project index is already
/// in that local-agent profile, keep the query local as well.
pub(super) async fn embed_query_aligned_to_index(
    index_pool: &SqlitePool,
    query: &str,
    timeout: Option<Duration>,
    cold_start_retry: bool,
) -> EmbeddedText {
    let diag = index_db::gather_embedding_diagnostics(index_pool).await;
    if !diag.vector_lane_available || diag.is_local_agent_index() {
        return EmbeddedText {
            vector: embed_text(query),
            semantic: false,
        };
    }
    // On a sustained outage or persistent cap/auth failure (per the activity
    // log), skip the doomed per-query remote embed that would exhaust the
    // timeout budget on every edit and fall back to SHA1 anyway. A single
    // transient blip does NOT trip this, preserving immediate recovery on the
    // next query for brief outages.
    if index_db::cloud_embed_outage_active() {
        return EmbeddedText {
            vector: embed_text(query),
            semantic: false,
        };
    }
    // On the interactive `recall` path, raise the first attempt's ceiling to
    // the cold-absorbing budget. A warm query still returns the instant the
    // provider responds (the budget is a ceiling), so the fast path is
    // unaffected; a cold round-trip completes in one shot.
    let attempt_timeout = if cold_start_retry {
        Some(timeout.map_or(COLD_RETRY_EMBEDDING_TIMEOUT, |base| {
            base.max(COLD_RETRY_EMBEDDING_TIMEOUT)
        }))
    } else {
        timeout
    };
    let embedded = embed_text_async_with_timeout(query, attempt_timeout).await;
    // Healthy lane but the attempt still fell back: under these gates that is a
    // timeout / transport failure, not a deliberate lexical result. Retry once
    // to ride out a transient flap. A genuine connection refusal fails fast, so
    // this does not lengthen the truly-offline case.
    if cold_start_retry && !embedded.semantic {
        return embed_text_async_with_timeout(query, Some(COLD_RETRY_EMBEDDING_TIMEOUT)).await;
    }
    embedded
}

/// Per-query budget for the single cold-start retry in
/// [`embed_query_aligned_to_index`]. Sized to clear a genuinely cold single-text
/// OpenAI embed in one shot while staying under the 45s embedding HTTP-client
/// cap. The outage gate (`cloud_embed_outage_active`) trips after a short run of
/// failures and routes subsequent queries to SHA1, so this larger budget is only
/// ever paid on the first cold call.
pub(super) const COLD_RETRY_EMBEDDING_TIMEOUT: Duration = Duration::from_secs(12);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cold_retry_embedding_timeout_absorbs_cold_call_within_client_cap() {
        // The cold-start retry budget must be (a) materially larger than the
        // ~2.5s interactive base budget the first attempt aborts on, so a cold
        // single-text OpenAI embed completes in one shot, and (b) safely under
        // the 45s embedding HTTP-client cap so the retry, not the socket,
        // governs the deadline.
        assert!(
            COLD_RETRY_EMBEDDING_TIMEOUT >= Duration::from_secs(10),
            "retry budget must clear a cold provider round-trip"
        );
        assert!(
            COLD_RETRY_EMBEDDING_TIMEOUT < crate::context::embedding::EMBEDDING_PROVIDER_TIMEOUT,
            "retry budget must stay under the embedding HTTP-client timeout"
        );
    }
}
