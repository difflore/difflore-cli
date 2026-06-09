//! Query-embedding alignment for hybrid rule retrieval.
//!
//! Split out of `rules.rs`: the logic that decides HOW to embed the recall
//! query so the resulting vector can actually match the persisted index —
//! including the cold-start retry and the sustained-outage fast path. The
//! orchestration in `rules.rs` calls [`embed_query_aligned_to_index`] via
//! `super::query_embed::…`.

use crate::context::embedding::{EmbeddedText, embed_text, embed_text_async_with_timeout};
use crate::context::index_db;
use sqlx::SqlitePool;
use std::time::Duration;

/// Embed the recall query in a profile the persisted index can actually match.
///
/// When the project index's vector lane is dead relative to the active embedder
/// (`vector_lane_available == false` — e.g. a local SHA1 index while the active
/// embedder is cloud/BYOK), embedding the query with the active remote provider
/// is futile: the resulting vector lives in a different space (usually a
/// different dimension) than every indexed chunk, so the cosine pass below drops
/// them all (`query_emb.len() != c.embedding.len()`) and recall silently
/// degrades to FTS-only — *worse* than the local-hash + FTS hybrid the SHA1
/// index supports. It also forces a doomed network round-trip and prints
/// provider-failure warnings on every recall, contradicting the `status` line
/// that already reports the lane as paused.
///
/// In that state, embed locally with SHA1 instead: it matches the SHA1 index
/// dimension (restoring the hybrid lane), makes no network call, and stays
/// quiet. The same applies when there is no usable lane at all — a not-yet-built
/// or empty index returns nothing regardless of the query vector, so a remote
/// embed there is pure latency (the no-data / fresh-repo recall otherwise hangs
/// ~2.5s on a doomed cloud call before reporting "no memories").
///
/// This keys off the STATIC lane check only, deliberately. A *cloud-profiled*
/// index whose provider is briefly down (`profile_match == true`) still issues a
/// remote query embed that times out and falls back to SHA1 per-call — a bounded
/// per-query cost while genuinely down, and immediate recovery to full semantic
/// ranking the moment the provider returns. Gating this on the strict
/// recent-fallback window instead would force SHA1 (FTS-only ranking) for the
/// whole window on an otherwise-healthy lane after a single transient blip, so
/// the build-path freshness skip — not the query path — owns that window.
///
/// Cold-start recovery (`cold_start_retry`): the *first* query after an idle
/// period embeds against a cloud provider whose upstream model connection is
/// cold. That first remote embed routinely exceeds the latency-sensitive base
/// budget (e.g. the local cloud dialling OpenAI for the first time); with a bare
/// ~2.5s timeout the call is aborted and recall falls back to SHA1 even though
/// the lane is healthy — the user sees "semantic vectors paused" on a cold call
/// and full semantic ranking only on the warm follow-up. The user-initiated
/// `recall`/`search` path fixes this in two parts, gated on a healthy lane (we
/// only reach the remote embed when `vector_lane_available && !outage`). First,
/// it raises the initial attempt's ceiling to the cold-absorbing budget
/// ([`COLD_RETRY_EMBEDDING_TIMEOUT`]) so a cold round-trip completes in one shot
/// instead of being aborted at ~2.5s and re-paid; a warm query is unaffected,
/// since the budget is a ceiling and it still returns the instant the provider
/// responds. Second, if that attempt still falls back, it retries once to ride
/// out a transient transport flap (a dropped cold connection) before settling
/// for SHA1, which stays the last resort used only after both fail.
///
/// `cold_start_retry` is OFF for the latency-critical hook (800ms) and MCP
/// (1500ms) paths: those deliberately fast-degrade to lexical so a cold provider
/// never blocks the agent's edit hook or tool call. Only the human-waiting CLI
/// path opts in, where a few extra seconds to keep semantic ranking is the right
/// trade.
pub(super) async fn embed_query_aligned_to_index(
    index_pool: &SqlitePool,
    query: &str,
    timeout: Option<Duration>,
    cold_start_retry: bool,
) -> EmbeddedText {
    let diag = index_db::gather_embedding_diagnostics(index_pool).await;
    if !diag.vector_lane_available {
        return EmbeddedText {
            vector: embed_text(query),
            semantic: false,
        };
    }
    // When the cloud embedder is in a sustained outage or a persistent cap/auth
    // failure (per the persisted activity log), skip the doomed per-query remote
    // embed: it would just exhaust the timeout budget on every edit and fall
    // back to SHA1 regardless. A single transient blip does NOT trip this, so
    // the documented immediate-recovery-on-next-query behaviour is preserved for
    // brief outages; only a genuine sustained outage routes straight to SHA1.
    if index_db::cloud_embed_outage_active() {
        return EmbeddedText {
            vector: embed_text(query),
            semantic: false,
        };
    }
    // The interactive `recall` path (`cold_start_retry`) raises the FIRST
    // attempt's ceiling to the cold-absorbing budget rather than aborting a
    // healthy-but-cold embed at the latency-sensitive base budget and re-paying
    // the connection cost on a retry. A warm query still returns the instant the
    // provider responds (the budget is a ceiling, not a floor), so this adds no
    // latency to the common fast path; a cold round-trip completes in one shot.
    let attempt_timeout = if cold_start_retry {
        Some(timeout.map_or(COLD_RETRY_EMBEDDING_TIMEOUT, |base| {
            base.max(COLD_RETRY_EMBEDDING_TIMEOUT)
        }))
    } else {
        timeout
    };
    let embedded = embed_text_async_with_timeout(query, attempt_timeout).await;
    // Healthy lane (checked above) but the attempt still fell back to a
    // non-semantic vector — under these gates that is a timeout / transport
    // failure, not a deliberate lexical result. On the opted-in CLI path, retry
    // once to ride out a transient transport flap (a dropped cold connection)
    // before settling for SHA1. A genuine connection refusal fails fast, so the
    // retry does not lengthen the truly-offline case; SHA1 stays the last resort.
    if cold_start_retry && !embedded.semantic {
        return embed_text_async_with_timeout(query, Some(COLD_RETRY_EMBEDDING_TIMEOUT)).await;
    }
    embedded
}

/// Per-query budget for the single cold-start retry in
/// [`embed_query_aligned_to_index`]. The first remote embed after an idle period
/// pays the provider's cold-connection cost (e.g. the local cloud dialling its
/// upstream model), which routinely overruns the ~2.5s interactive base budget
/// the base attempt aborts on. The retry budget is sized to clear a genuinely
/// cold single-text OpenAI embed in one shot while staying well under the 45s
/// embedding HTTP-client cap, and is bounded so a truly unreachable provider
/// still terminates one recall in seconds: the on-disk outage gate
/// (`cloud_embed_outage_active`) trips after a short run of transient failures
/// and routes subsequent queries straight to SHA1, so this larger budget is only
/// ever paid on the FIRST cold call, never repeatedly.
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
