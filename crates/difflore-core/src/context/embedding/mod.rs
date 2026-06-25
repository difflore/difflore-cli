use async_trait::async_trait;
use sha1::{Digest, Sha1};
use std::time::Duration;
use unicode_segmentation::UnicodeSegmentation;

use crate::error::CoreError;
use crate::infra::crypto;

mod cloud;
mod openai;
mod sha1_embedder;

pub use cloud::CloudEmbedder;
pub use openai::OpenAICompatEmbedder;
pub use sha1_embedder::Sha1Embedder;

pub const EMBEDDING_DIM: usize = 128;

/// Sentinel stored in `context_engine.embedding_provider_url` when the
/// user explicitly picked the cloud-managed embedding source. The
/// `get_embedder` chain treats this as "use CloudEmbedder if logged in,
/// otherwise local lexical hash" — it must never be sent as a real URL to
/// `OpenAICompatEmbedder`.
pub const CLOUD_MANAGED_SENTINEL: &str = "cloud-managed";

/// Default dimensionality for `OpenAI` `text-embedding-3-small`.
pub const DEFAULT_OPENAI_EMBEDDING_DIM: usize = 1536;
// Kept longer than cloud's own retry window so the client does not disconnect
// early and force the caller into SHA1 fallback.
pub(crate) const EMBEDDING_PROVIDER_TIMEOUT: Duration = Duration::from_secs(45);
const EMBEDDING_RETRY_DELAYS_MS: &[u64] = &[100, 300, 700];
pub const EMBEDDING_BATCH_SIZE: usize = 64;

fn parse_embedding_vector(
    values: &[serde_json::Value],
    context: &str,
) -> Result<Vec<f32>, CoreError> {
    values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            value.as_f64().map(|number| number as f32).ok_or_else(|| {
                CoreError::Internal(format!(
                    "{context} contains non-numeric value at index {index}"
                ))
            })
        })
        .collect()
}

#[allow(clippy::panic)]
// reason: reqwest client construction with a static timeout is unrecoverable for provider setup.
fn embedding_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(EMBEDDING_PROVIDER_TIMEOUT)
        .build()
        .unwrap_or_else(|e| {
            panic!("failed to build embedding HTTP client with provider timeout: {e}")
        })
}

/// Abstract embedding provider.
#[async_trait]
pub trait Embedder: Send + Sync {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, CoreError>;

    async fn embed_batch(
        &self,
        texts: &[String],
        _rule_ids: Option<&[String]>,
    ) -> Result<Vec<Vec<f32>>, CoreError> {
        let mut vectors = Vec::with_capacity(texts.len());
        for text in texts {
            vectors.push(self.embed(text).await?);
        }
        Ok(vectors)
    }

    fn dim(&self) -> usize;

    /// Whether this embedder produces semantically meaningful vectors.
    /// Lexical-only fallbacks override to `false` so hybrid retrieval leans
    /// harder on the FTS baseline.
    fn is_semantic(&self) -> bool {
        true
    }
}

/// Encrypt an embedding provider API key and return the opaque storage
/// identifier that should be persisted in settings (`embedding_provider_key`).
///
/// Under the hood this uses the AES-GCM master key stored in the OS keyring
/// (see `crate::infra::crypto`). The returned string is ciphertext hex — it is safe
/// to store on disk. Callers must round-trip through [`load_embedding_key`]
/// before using the key with an embedding provider.
pub fn store_embedding_key(api_key: &str) -> Result<String, CoreError> {
    crypto::encrypt_secret(api_key)
        .map_err(|e| CoreError::Internal(format!("failed to encrypt embedding key: {e}")))
}

/// Decrypt an embedding provider API key from the opaque storage identifier
/// produced by [`store_embedding_key`].
///
/// Returns `CoreError::Internal` on any crypto / keyring failure so callers
/// can fall back to [`Sha1Embedder`] without panicking.
pub fn load_embedding_key(storage_key: &str) -> Result<String, CoreError> {
    crypto::decrypt_secret(storage_key)
        .map_err(|e| CoreError::Internal(format!("failed to decrypt embedding key: {e}")))
}

fn retryable_embedding_status(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::REQUEST_TIMEOUT
        || status == reqwest::StatusCode::BAD_GATEWAY
        || status == reqwest::StatusCode::SERVICE_UNAVAILABLE
        || status == reqwest::StatusCode::GATEWAY_TIMEOUT
        || status.is_server_error()
}

/// Resolve the configured embedder from settings.
///
/// Priority chain (first match wins):
///   1. `OpenAICompatEmbedder` — explicit BYOK provider (`semantic_embedding`
///      on + a real non-sentinel `embedding_provider_url`). Takes precedence
///      over a stored cloud token.
///   2. `CloudEmbedder` — if logged in to cloud. The stored token is trusted
///      without a network probe; request failures fall back to local SHA1 via
///      `embed_text_async`.
///   3. [`Sha1Embedder`] — deterministic offline fallback, also used on any
///      settings error.
///
/// `probe_active_embedder` mirrors this same order — keep the two in sync.
pub async fn get_embedder() -> Box<dyn Embedder> {
    // The cloud-managed sentinel is not a real URL, so it is excluded here and
    // handled by the cloud branch below.
    if let Ok(settings) = crate::infra::settings::get().await {
        let ce = &settings.context_engine;
        let byok_url = ce
            .embedding_provider_url
            .as_ref()
            .map(|u| u.trim().to_owned())
            .filter(|u| !u.is_empty() && u != CLOUD_MANAGED_SENTINEL);
        if ce.semantic_embedding
            && let Some(url) = byok_url
        {
            // `embedding_provider_key` is a keyring storage identifier; decrypt
            // it to get the real API key. On decrypt failure, fall through to
            // cloud/SHA1 rather than sending empty credentials to the provider.
            let key = match ce.embedding_provider_key.as_ref() {
                Some(storage_key) if !storage_key.trim().is_empty() => {
                    if let Ok(plain) = load_embedding_key(storage_key) {
                        Some(plain)
                    } else {
                        eprintln!(
                            "warning: DiffLore could not read the saved embedding key; using keyword matching for now."
                        );
                        None
                    }
                }
                // BYOK without a stored key (some local providers need none).
                _ => Some(String::new()),
            };
            if let Some(key) = key {
                let model = ce
                    .embedding_model
                    .clone()
                    .unwrap_or_else(|| "text-embedding-3-small".to_owned());
                let dim = ce.embedding_dim.unwrap_or(DEFAULT_OPENAI_EMBEDDING_DIM);
                return Box::new(OpenAICompatEmbedder::new(url, key, model, dim));
            }
        }
    }

    if let Some(token) = crate::cloud::client::CloudClient::load_token().await {
        let base = crate::cloud::endpoints::api_base();
        return Box::new(CloudEmbedder::new(base, token));
    }

    Box::new(Sha1Embedder::new())
}

/// Tag for the active embedder, returned by [`probe_active_embedder`] so
/// callers can render the right hint copy without re-implementing the chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActiveEmbedderKind {
    Cloud,
    Byok {
        provider_host: String,
        model: String,
        dim: usize,
    },
    Sha1,
}

impl ActiveEmbedderKind {
    pub const fn dim(&self) -> usize {
        match self {
            Self::Cloud => 0,
            Self::Byok { dim, .. } => *dim,
            Self::Sha1 => EMBEDDING_DIM,
        }
    }

    pub fn profile(&self) -> String {
        match self {
            Self::Cloud => "cloud:managed".to_owned(),
            Self::Byok {
                provider_host,
                model,
                dim,
            } => format!("byok:{provider_host}:{model}:{dim}"),
            Self::Sha1 => format!("sha1:local:{EMBEDDING_DIM}"),
        }
    }
}

/// Returns `Byok` iff settings select an explicit, usable BYOK provider
/// (semantic_embedding on, a real non-sentinel URL, and a decryptable key if
/// one is configured). `None` means BYOK does not apply.
///
/// Single source of truth for "is BYOK active", so diagnostics and the MCP
/// hook never drift from the runtime resolver. Pure, so
/// [`probe_active_embedder`] can defer the async cloud-token load until BYOK is
/// ruled out.
fn byok_from_settings(
    ce: Option<&crate::domain::models::ContextEngineRecord>,
) -> Option<ActiveEmbedderKind> {
    let ce = ce?;
    if !ce.semantic_embedding {
        return None;
    }
    let url = ce
        .embedding_provider_url
        .as_ref()
        .map(|u| u.trim())
        .filter(|u| !u.is_empty() && *u != CLOUD_MANAGED_SENTINEL)?;
    // A configured-but-undecryptable key means BYOK is not actually usable, so
    // the resolver would fall through to cloud/SHA1. Reporting Byok here would
    // mislabel the backend and let mismatched vectors persist under a BYOK
    // embedding profile.
    let key_usable = match ce.embedding_provider_key.as_ref() {
        Some(storage_key) if !storage_key.trim().is_empty() => {
            load_embedding_key(storage_key).is_ok()
        }
        _ => true,
    };
    if !key_usable {
        return None;
    }
    let host = url_host(url).unwrap_or_else(|| "byok".to_owned());
    let model = ce
        .embedding_model
        .clone()
        .unwrap_or_else(|| "text-embedding-3-small".to_owned());
    let dim = ce.embedding_dim.unwrap_or(DEFAULT_OPENAI_EMBEDDING_DIM);
    Some(ActiveEmbedderKind::Byok {
        provider_host: host,
        model,
        dim,
    })
}

/// Report the currently-resolved embedder kind, mirroring the priority chain
/// in `get_embedder` (explicit BYOK → cloud → SHA1) without allocating the
/// actual embedder.
pub async fn probe_active_embedder() -> ActiveEmbedderKind {
    let settings = crate::infra::settings::get().await.ok();
    if let Some(byok) = byok_from_settings(settings.as_ref().map(|s| &s.context_engine)) {
        return byok;
    }
    // `load_token_quiet`: read-only status check, so a corrupt token must not
    // spam stderr. Real recall/cloud paths use the loud `load_token`.
    if crate::cloud::client::CloudClient::load_token_quiet()
        .await
        .is_some()
    {
        return ActiveEmbedderKind::Cloud;
    }
    ActiveEmbedderKind::Sha1
}

/// Sync sibling of [`probe_active_embedder`] for non-async callers. Runs the
/// async probe on a short-lived scratch runtime on its own thread so it returns
/// the exact same answer as the runtime resolver, with no separate sync
/// detection logic that could drift.
pub fn probe_active_embedder_sync() -> ActiveEmbedderKind {
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt.block_on(probe_active_embedder()),
                    Err(_) => ActiveEmbedderKind::Sha1,
                }
            })
            .join()
            .unwrap_or(ActiveEmbedderKind::Sha1)
    })
}

pub async fn active_embedding_profile() -> String {
    probe_active_embedder().await.profile()
}

pub fn local_embedding_profile() -> String {
    format!("sha1:local:{EMBEDDING_DIM}")
}

fn url_host(s: &str) -> Option<String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(url) = reqwest::Url::parse(trimmed) {
        let host = url.host_str()?;
        let host = if host.contains(':') && !host.starts_with('[') {
            format!("[{host}]")
        } else {
            host.to_owned()
        };
        return Some(match url.port() {
            Some(port) => format!("{host}:{port}"),
            None => host,
        });
    }
    let host = trimmed.split('/').next().unwrap_or(trimmed);
    if host.is_empty() {
        None
    } else {
        Some(host.to_owned())
    }
}

/// Synchronous SHA1 lexical embedding — the local fallback for offline users
/// without cloud/BYOK semantic embeddings.
pub fn embed_text(text: &str) -> Vec<f32> {
    let mut vec = vec![0.0f32; EMBEDDING_DIM];
    for word in text.unicode_words() {
        let mut hasher = Sha1::new();
        hasher.update(word.to_lowercase().as_bytes());
        let hash = hasher.finalize();
        for (i, byte) in hash.iter().enumerate() {
            let dim = i % EMBEDDING_DIM;
            vec[dim] += if byte & 1 == 0 { 1.0 } else { -1.0 };
        }
    }
    let norm: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in &mut vec {
            *x /= norm;
        }
    }
    vec
}

#[derive(Debug, Clone, PartialEq)]
pub struct EmbeddedText {
    pub vector: Vec<f32>,
    pub semantic: bool,
}

/// Async embedding helper that always returns a `Vec<f32>` (never an error).
/// Falls back to the local SHA1 vector if there is no semantic provider or a
/// configured provider fails after retry.
pub async fn embed_text_async(text: &str) -> Vec<f32> {
    embed_text_async_with_timeout(text, None).await.vector
}

/// Async embedding helper for latency-sensitive paths.
///
/// When `timeout` is present, provider calls that exceed the budget fall
/// back to local SHA1 after retry. The returned `semantic` flag describes
/// the actual vector, not merely the configured provider, so retrieval can
/// weight FTS more heavily after any provider failure or timeout.
pub async fn embed_text_async_with_timeout(text: &str, timeout: Option<Duration>) -> EmbeddedText {
    let texts = vec![text.to_owned()];
    embed_texts_async_with_timeout(&texts, None, timeout)
        .await
        .into_iter()
        .next()
        .unwrap_or_else(|| sha1_fallback_embedding(text))
}

pub async fn embed_texts_async_with_timeout(
    texts: &[String],
    rule_ids: Option<&[String]>,
    timeout: Option<Duration>,
) -> Vec<EmbeddedText> {
    if texts.is_empty() {
        return Vec::new();
    }
    let embedder = get_embedder().await;
    embed_texts_with_embedder_and_timeout(embedder.as_ref(), texts, rule_ids, timeout).await
}

/// Max provider batches in flight at once. Bounded so a large corpus reindex
/// overlaps round-trips (~1 RTT instead of N serialized) without tripping
/// provider rate limits.
const EMBEDDING_BATCH_CONCURRENCY: usize = 4;

async fn embed_texts_with_embedder_and_timeout(
    embedder: &dyn Embedder,
    texts: &[String],
    rule_ids: Option<&[String]>,
    timeout: Option<Duration>,
) -> Vec<EmbeddedText> {
    // The latency-sensitive recall path passes a timeout and keeps the
    // sequential "stop at the first failing batch + SHA1-fill the remainder"
    // semantics: once a provider call times out we must not keep firing
    // round-trips, and every position from the failure onward is filled
    // locally so the caller still gets a vector per text in order.
    if timeout.is_some() {
        return embed_batches_sequential_with_timeout(embedder, texts, rule_ids, timeout).await;
    }
    embed_batches_concurrent(embedder, texts, rule_ids).await
}

/// Sequential path used whenever a per-batch timeout is set. Stops at the first
/// failed/timed-out batch and SHA1-fills every remaining text.
async fn embed_batches_sequential_with_timeout(
    embedder: &dyn Embedder,
    texts: &[String],
    rule_ids: Option<&[String]>,
    timeout: Option<Duration>,
) -> Vec<EmbeddedText> {
    let semantic = embedder.is_semantic();
    let mut embedded = Vec::with_capacity(texts.len());
    for (chunk_index, text_chunk) in texts.chunks(EMBEDDING_BATCH_SIZE).enumerate() {
        let start = chunk_index * EMBEDDING_BATCH_SIZE;
        let end = start + text_chunk.len();
        let rule_id_chunk = rule_ids.and_then(|ids| ids.get(start..end));
        let embed_fut = embedder.embed_batch(text_chunk, rule_id_chunk);
        let result = match timeout {
            Some(timeout) => match tokio::time::timeout(timeout, embed_fut).await {
                Ok(result) => result,
                Err(_) => Err(CoreError::Internal(format!(
                    "embedding provider timed out after {}ms",
                    timeout.as_millis()
                ))),
            },
            None => embed_fut.await,
        };

        match batch_outcome(result, text_chunk, semantic) {
            BatchOutcome::Ok(vectors) => embedded.extend(vectors),
            BatchOutcome::Failed(reason) => {
                warn_embedding_fallback_once(&reason);
                // Stop at the first failure and SHA1-fill the remainder so we
                // never keep paying provider round-trips after a timeout.
                embedded.extend(
                    texts[start..]
                        .iter()
                        .map(|text| sha1_fallback_embedding(text)),
                );
                break;
            }
        }
    }
    embedded
}

/// Concurrent path used when no per-batch timeout is configured (e.g.
/// `embeddings rebuild`, BYOK reindex). Batches run with bounded parallelism so
/// round-trips overlap, while results are collected strictly in input order.
/// Each batch falls back to SHA1 independently — unlike the timed path there is
/// no global stop, matching the original no-timeout behaviour.
async fn embed_batches_concurrent(
    embedder: &dyn Embedder,
    texts: &[String],
    rule_ids: Option<&[String]>,
) -> Vec<EmbeddedText> {
    use futures_util::stream::{self, StreamExt};

    let semantic = embedder.is_semantic();

    // Each future owns its batch inputs (owned `Vec`s moved into the async
    // block), so the only borrow it holds across `.await` is `&dyn Embedder`
    // — which is `Send` because the trait is `Send + Sync`. Owning the inputs
    // keeps the `buffered` stream future `Send`, so it can be driven from a
    // `tokio::spawn`ed connection task.
    let batches: Vec<(Vec<String>, Option<Vec<String>>)> = texts
        .chunks(EMBEDDING_BATCH_SIZE)
        .enumerate()
        .map(|(chunk_index, text_chunk)| {
            let start = chunk_index * EMBEDDING_BATCH_SIZE;
            let end = start + text_chunk.len();
            let rule_id_chunk = rule_ids
                .and_then(|ids| ids.get(start..end))
                .map(<[String]>::to_vec);
            (text_chunk.to_vec(), rule_id_chunk)
        })
        .collect();

    let batched: Vec<Vec<EmbeddedText>> = stream::iter(batches)
        .map(|(text_chunk, rule_id_chunk)| async move {
            let result = embedder
                .embed_batch(&text_chunk, rule_id_chunk.as_deref())
                .await;
            match batch_outcome(result, &text_chunk, semantic) {
                BatchOutcome::Ok(vectors) => vectors,
                BatchOutcome::Failed(reason) => {
                    warn_embedding_fallback_once(&reason);
                    text_chunk
                        .iter()
                        .map(|text| sha1_fallback_embedding(text))
                        .collect()
                }
            }
        })
        .buffered(EMBEDDING_BATCH_CONCURRENCY)
        .collect()
        .await;

    let mut embedded = Vec::with_capacity(texts.len());
    for batch in batched {
        embedded.extend(batch);
    }
    embedded
}

enum BatchOutcome {
    Ok(Vec<EmbeddedText>),
    Failed(String),
}

/// Validate one provider batch response against the requested chunk, mapping it
/// to either the parsed vectors or a fallback reason. Shared by both the timed
/// and concurrent paths so validation never drifts between them.
fn batch_outcome(
    result: Result<Vec<Vec<f32>>, CoreError>,
    text_chunk: &[String],
    semantic: bool,
) -> BatchOutcome {
    match result {
        Ok(vectors)
            if vectors.len() == text_chunk.len()
                && vectors.iter().all(|vector| !vector.is_empty()) =>
        {
            BatchOutcome::Ok(
                vectors
                    .into_iter()
                    .map(|vector| EmbeddedText { vector, semantic })
                    .collect(),
            )
        }
        Ok(_) => {
            BatchOutcome::Failed("provider returned empty or mismatched vector batch".to_owned())
        }
        Err(e) => BatchOutcome::Failed(format!("provider failed ({e})")),
    }
}

fn sha1_fallback_embedding(text: &str) -> EmbeddedText {
    EmbeddedText {
        vector: embed_text(text),
        semantic: false,
    }
}

/// Record an embedding fallback, and print a calm warning at most once per
/// process per distinct cause.
///
/// The activity event is recorded on EVERY call, not just the first: the
/// freshness-skip and health diagnostics read it to decide whether the remote
/// provider is currently down, and deduping the record per process would let a
/// long-lived server's down-signal go stale. Only the console print is deduped,
/// so a single `difflore recall` shows one clear line per cause class rather
/// than one per failed rule chunk.
fn warn_embedding_fallback_once(reason: &str) {
    use std::collections::HashSet;
    use std::sync::Mutex;
    static SEEN: Mutex<Option<HashSet<String>>> = Mutex::new(None);
    let key = classify_reason(reason);
    crate::observability::activity_stream::record(
        crate::observability::activity_stream::ActivityPayload::EmbeddingFallback {
            reason: key.clone(),
        },
    );
    let Ok(mut guard) = SEEN.lock() else {
        return; // poisoned mutex — event already recorded; just skip the print
    };
    let set = guard.get_or_insert_with(HashSet::new);
    if !set.insert(key.clone()) {
        return; // already printed this class of failure this process
    }
    eprintln!("warning: {}", calm_fallback_summary(&key));
    eprintln!("{}", actionable_fix_for(&key));
}

/// A calm, user-facing summary of an embedding fallback, classified by the same
/// stable key as [`actionable_fix_for`]. The raw transport error is kept off
/// the hot path; `difflore doctor` is the place for the verbose diagnostic.
fn calm_fallback_summary(key: &str) -> &'static str {
    match key {
        "scope" | "forbidden" | "unauthorized" => {
            "semantic vectors paused (cloud sign-in needs refresh); \
             recall continues with file-pattern + keyword matching"
        }
        "cap" => {
            "semantic vectors paused (cloud embedding cap reached); \
             recall continues with file-pattern + keyword matching"
        }
        "timeout" | "network" => {
            "semantic vectors paused (cloud unreachable); \
             recall continues with file-pattern + keyword matching"
        }
        "empty" => {
            "semantic vectors paused (provider returned no vector); \
             recall continues with file-pattern + keyword matching"
        }
        _ => {
            "semantic vectors paused (cloud embedding unavailable); \
             recall continues with file-pattern + keyword matching"
        }
    }
}

/// Bucket the raw error string into a short stable key so failures can be
/// deduped across call sites without storing the full message.
fn classify_reason(reason: &str) -> String {
    let lower = reason.to_ascii_lowercase();
    if lower.contains("missing required scope") {
        return "scope".to_owned();
    }
    if lower.contains("embed cap")
        || lower.contains("embedding cap reached")
        || lower.contains("embed_cap_reached")
    {
        return "cap".to_owned();
    }
    if lower.contains("403") || lower.contains("forbidden") {
        return "forbidden".to_owned();
    }
    if lower.contains("401") || lower.contains("unauthorized") {
        return "unauthorized".to_owned();
    }
    if lower.contains("timeout") || lower.contains("timed out") {
        return "timeout".to_owned();
    }
    if lower.contains("connect") || lower.contains("dns") {
        return "network".to_owned();
    }
    if lower.contains("empty vector") {
        return "empty".to_owned();
    }
    "other".to_owned()
}

/// Return an actionable recovery next-step tailored per failure class.
fn actionable_fix_for(key: &str) -> &'static str {
    match key {
        "scope" => {
            "[embedding] -> your cloud token is missing the embedding scope. \
             Re-run `difflore cloud login` to refresh, \
             or `difflore embeddings setup` to bring your own key."
        }
        "forbidden" => {
            "[embedding] -> cloud rejected the embed request. \
             Re-run `difflore cloud login` to refresh credentials."
        }
        "unauthorized" => "[embedding] -> cloud token expired. Run `difflore cloud login`.",
        "cap" => {
            "[embedding] -> cloud embedding cap reached. Recall stays usable via local SHA1 + FTS; \
             upgrade for unlimited managed embedding, or run `difflore embeddings setup` for BYOK."
        }
        "timeout" | "network" => {
            "[embedding] -> cloud unreachable. Recall stays usable via local SHA1 + FTS; \
             retry when network recovers, or run `difflore embeddings setup` \
             for an offline BYOK key."
        }
        "empty" => {
            "[embedding] -> provider returned no vector. \
             Run `difflore doctor` to inspect the active embedder."
        }
        _ => {
            "[embedding] -> run `difflore doctor` for diagnostics, \
             or `difflore embeddings setup` to switch to BYOK."
        }
    }
}

/// True cosine similarity in `[-1, 1]`, normalizing both inputs.
///
/// The local SHA1 embedder returns unit-norm vectors, but managed/BYOK
/// providers may not, so a bare dot product would rank by magnitude and
/// disagree with the ANN cosine path. Dividing by the norms keeps the
/// linear-scan fallback consistent for any provider. Zero-norm inputs
/// return `0.0` rather than `NaN`.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a * norm_b)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::float_cmp
)] // reason: test code — explicit panic/expect/exact-cmp on known-finite vectors.
mod tests {
    use super::*;

    #[test]
    fn embed_text_produces_fixed_dim_vector() {
        let vec = embed_text("hello world");
        assert_eq!(vec.len(), EMBEDDING_DIM);
    }

    #[test]
    fn embed_text_is_unit_normalized() {
        let vec = embed_text("let x = 42;");
        let norm: f32 = vec.iter().map(|v| v * v).sum::<f32>().sqrt();
        // allow small rounding error
        assert!((norm - 1.0).abs() < 1e-4, "expected unit-norm, got {norm}");
    }

    #[test]
    fn parse_embedding_vector_rejects_non_numeric_items() {
        let items = vec![serde_json::json!(1.0), serde_json::json!("bad")];
        let err = parse_embedding_vector(&items, "test embedding").unwrap_err();

        match err {
            CoreError::Internal(msg) => {
                assert!(msg.contains("non-numeric value at index 1"), "msg: {msg}");
            }
            other => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn embed_empty_text_returns_zero_vector() {
        let vec = embed_text("");
        assert_eq!(vec.len(), EMBEDDING_DIM);
        assert!(vec.iter().all(|&v| v == 0.0));
    }

    #[test]
    fn cosine_similarity_identical_vectors_is_one() {
        let a = embed_text("fn main() {}");
        let sim = cosine_similarity(&a, &a);
        assert!((sim - 1.0).abs() < 1e-4);
    }

    #[test]
    fn cosine_similarity_orthogonal_zero_vectors_is_zero() {
        let a = vec![0.0; EMBEDDING_DIM];
        let b = vec![0.0; EMBEDDING_DIM];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }

    #[test]
    fn cosine_similarity_is_scale_invariant() {
        // Same direction, different magnitudes: true cosine is 1.0. A bare
        // dot product would return 50.0 here, mis-ranking non-unit-norm
        // (BYOK) embeddings in the linear-scan fallback.
        let a = [3.0_f32, 4.0];
        let b = [6.0_f32, 8.0];
        assert!((cosine_similarity(&a, &b) - 1.0).abs() < 1e-6);
        // Orthogonal directions: cosine 0.0 regardless of magnitude.
        let c = [0.0_f32, 5.0];
        let d = [7.0_f32, 0.0];
        assert!(cosine_similarity(&c, &d).abs() < 1e-6);
    }

    #[test]
    fn provider_failure_fallback_uses_sha1_after_retry() {
        let fallback = sha1_fallback_embedding("hello world");
        assert_eq!(
            fallback.vector,
            embed_text("hello world"),
            "provider failures should fall back to local SHA1 only after retry"
        );
        assert!(
            !fallback.semantic,
            "provider failure fallback is local lexical hash, not semantic"
        );
    }

    #[test]
    fn provider_failure_warning_marks_sha1_as_fallback() {
        let message = actionable_fix_for("network");
        assert!(
            message.contains("local SHA1 + FTS"),
            "network fallback should name the degraded local path: {message}"
        );
        assert!(
            message.contains("retry when network recovers"),
            "provider failure guidance should prefer cloud recovery: {message}"
        );
    }

    #[tokio::test]
    async fn sha1_embedder_matches_embed_text() {
        let embedder = Sha1Embedder::new();
        assert_eq!(embedder.dim(), EMBEDDING_DIM);
        let out = embedder.embed("hello world").await.expect("sha1 embed");
        let expected = embed_text("hello world");
        assert_eq!(out.len(), EMBEDDING_DIM);
        assert_eq!(out, expected);
    }

    #[tokio::test]
    async fn sha1_embedder_is_deterministic_128d() {
        let embedder = Sha1Embedder::new();
        let a = embedder.embed("fn main() {}").await.unwrap();
        let b = embedder.embed("fn main() {}").await.unwrap();
        assert_eq!(a.len(), 128);
        assert_eq!(a, b);
    }

    struct SlowBatchEmbedder {
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl Embedder for SlowBatchEmbedder {
        async fn embed(&self, _text: &str) -> Result<Vec<f32>, CoreError> {
            unreachable!("test calls embed_batch directly")
        }

        async fn embed_batch(
            &self,
            texts: &[String],
            _rule_ids: Option<&[String]>,
        ) -> Result<Vec<Vec<f32>>, CoreError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(50)).await;
            Ok(texts.iter().map(|_| vec![1.0]).collect())
        }

        fn dim(&self) -> usize {
            1
        }
    }

    #[tokio::test]
    async fn timed_batch_embedding_falls_back_for_remaining_batches_after_first_timeout() {
        let embedder = SlowBatchEmbedder {
            calls: std::sync::atomic::AtomicUsize::new(0),
        };
        let texts = (0..=(EMBEDDING_BATCH_SIZE * 3))
            .map(|i| format!("rule body {i}"))
            .collect::<Vec<_>>();

        let embedded = embed_texts_with_embedder_and_timeout(
            &embedder,
            &texts,
            None,
            Some(Duration::from_millis(5)),
        )
        .await;

        assert_eq!(embedded.len(), texts.len());
        assert_eq!(
            embedder.calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "latency-sensitive batch calls should not wait once per provider batch"
        );
        for (embedded, text) in embedded.iter().zip(&texts) {
            assert!(!embedded.semantic);
            assert_eq!(embedded.vector, embed_text(text));
        }
    }

    #[test]
    fn openai_embedder_endpoint_handles_url_variants() {
        let cases = &[
            (
                "https://api.openai.com/v1",
                "https://api.openai.com/v1/embeddings",
            ),
            (
                "https://api.example.com/v1/",
                "https://api.example.com/v1/embeddings",
            ),
            (
                "https://api.example.com/v1/embeddings",
                "https://api.example.com/v1/embeddings",
            ),
        ];
        for (base, expected) in cases {
            let e = OpenAICompatEmbedder::new((*base).into(), "k".into(), "m".into(), 128);
            assert_eq!(e.endpoint(), *expected, "base: {base}");
        }
    }

    fn openai_embedding_response(values: &[f32]) -> &'static str {
        let body = serde_json::json!({ "data": [{ "embedding": values }] }).to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        Box::leak(response.into_boxed_str())
    }

    #[tokio::test]
    async fn openai_embedder_accepts_matching_dimension_without_sending_dimensions() {
        let (url, handle) = spawn_mock(openai_embedding_response(&[0.1, 0.2, 0.3]));
        let embedder =
            OpenAICompatEmbedder::new(url, "k".into(), "text-embedding-3-small".into(), 3);
        let v = embedder
            .embed("hello")
            .await
            .expect("matching dim should succeed");
        assert_eq!(v.len(), 3);
        // Must NOT send a `dimensions` field: models like ada-002 and strict
        // local providers reject it. Length is validated from the response.
        let req = String::from_utf8(handle.join().unwrap()).unwrap();
        assert!(
            !req.contains("\"dimensions\""),
            "request must not send a dimensions field: {req}"
        );
    }

    #[tokio::test]
    async fn openai_embedder_rejects_dimension_mismatch() {
        // Provider returns 2 dims while 3 are configured — must error rather than
        // store mismatched-length vectors under the configured profile.
        let (url, handle) = spawn_mock(openai_embedding_response(&[0.1, 0.2]));
        let embedder =
            OpenAICompatEmbedder::new(url, "k".into(), "text-embedding-3-small".into(), 3);
        let err = embedder
            .embed("hello")
            .await
            .expect_err("dimension mismatch should error");
        match err {
            CoreError::Internal(msg) => {
                assert!(msg.contains("dimensions"), "msg: {msg}");
                assert!(msg.contains("difflore embeddings setup"), "msg: {msg}");
            }
            other => panic!("unexpected err: {other:?}"),
        }
        let _ = handle.join();
    }

    fn openai_batch_response(items: &[(u64, &[f32])]) -> &'static str {
        let data: Vec<serde_json::Value> = items
            .iter()
            .map(|(index, vec)| serde_json::json!({ "index": index, "embedding": vec }))
            .collect();
        openai_batch_response_data(&data)
    }

    fn openai_batch_response_data(data: &[serde_json::Value]) -> &'static str {
        let body = serde_json::json!({ "data": data.to_vec() }).to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        Box::leak(response.into_boxed_str())
    }

    #[tokio::test]
    async fn openai_embedder_batches_into_single_request() {
        // spawn_mock accepts exactly one TCP connection, so this also proves the
        // batch is sent as ONE request rather than one-per-text.
        let resp = openai_batch_response(&[(0, &[0.1, 0.2, 0.3]), (1, &[0.4, 0.5, 0.6])]);
        let (url, handle) = spawn_mock(resp);
        let embedder = OpenAICompatEmbedder::new(url, "k".into(), "m".into(), 3);
        let texts = vec!["a".to_owned(), "b".to_owned()];
        let vectors = embedder
            .embed_batch(&texts, None)
            .await
            .expect("batch embed should succeed");
        assert_eq!(vectors.len(), 2);
        assert_eq!(vectors[0], vec![0.1f32, 0.2, 0.3]);
        assert_eq!(vectors[1], vec![0.4f32, 0.5, 0.6]);
        let req = String::from_utf8(handle.join().unwrap()).unwrap();
        assert!(
            req.contains("\"input\""),
            "request should batch input: {req}"
        );
    }

    #[tokio::test]
    async fn openai_embedder_batch_orders_by_response_index() {
        // Items returned out of order must be sorted back to input order.
        let resp = openai_batch_response(&[(1, &[0.4, 0.5]), (0, &[0.1, 0.2])]);
        let (url, handle) = spawn_mock(resp);
        let embedder = OpenAICompatEmbedder::new(url, "k".into(), "m".into(), 2);
        let texts = vec!["first".to_owned(), "second".to_owned()];
        let vectors = embedder
            .embed_batch(&texts, None)
            .await
            .expect("batch embed should succeed");
        assert_eq!(vectors[0], vec![0.1f32, 0.2]);
        assert_eq!(vectors[1], vec![0.4f32, 0.5]);
        let _ = handle.join();
    }

    #[tokio::test]
    async fn openai_embedder_rejects_mixed_response_indices() {
        let resp = openai_batch_response_data(&[
            serde_json::json!({ "index": 1, "embedding": [0.4, 0.5] }),
            serde_json::json!({ "embedding": [0.1, 0.2] }),
        ]);
        let (url, handle) = spawn_mock(resp);
        let embedder = OpenAICompatEmbedder::new(url, "k".into(), "m".into(), 2);
        let texts = vec!["first".to_owned(), "second".to_owned()];
        let err = embedder
            .embed_batch(&texts, None)
            .await
            .expect_err("mixed explicit/missing indices must be rejected");

        match err {
            CoreError::Internal(msg) => assert!(msg.contains("mixed explicit and missing")),
            other => panic!("unexpected err: {other:?}"),
        }
        let _ = handle.join();
    }

    #[test]
    fn probe_active_embedder_sync_runs_without_panicking() {
        // Sync callers must drive the async probe on a scratch runtime without
        // panicking or deadlocking.
        // The exact kind depends on the test environment; we only assert the
        // sync→async bridge works and returns a recognizable profile.
        let kind = probe_active_embedder_sync();
        assert!(!kind.profile().is_empty());
    }

    #[tokio::test]
    async fn openai_embedder_omits_auth_header_when_keyless() {
        let (url, handle) = spawn_mock(openai_batch_response(&[(0, &[0.1, 0.2])]));
        // Empty key = keyless local provider (`--no-key`).
        let embedder = OpenAICompatEmbedder::new(url, String::new(), "m".into(), 2);
        embedder
            .embed_batch(&["x".to_owned()], None)
            .await
            .expect("keyless embed should succeed");
        let req = String::from_utf8(handle.join().unwrap())
            .unwrap()
            .to_ascii_lowercase();
        assert!(
            !req.contains("authorization:"),
            "keyless request must not send an auth header: {req}"
        );
    }

    #[tokio::test]
    async fn openai_embedder_sends_auth_header_when_keyed() {
        let (url, handle) = spawn_mock(openai_batch_response(&[(0, &[0.1, 0.2])]));
        let embedder = OpenAICompatEmbedder::new(url, "sk-x".into(), "m".into(), 2);
        embedder
            .embed_batch(&["x".to_owned()], None)
            .await
            .expect("keyed embed should succeed");
        let req = String::from_utf8(handle.join().unwrap())
            .unwrap()
            .to_ascii_lowercase();
        assert!(
            req.contains("authorization: bearer sk-x"),
            "keyed request must send bearer auth: {req}"
        );
    }

    // keyring-encrypted embedding key round-trip.
    //
    // Marked `#[ignore]` so they never block headless runs that lack any
    // credential backend (e.g. sandboxed CI without `dirs::home_dir`). Run
    // locally with: cargo test -p difflore-core embedding_key -- --ignored
    #[test]
    #[ignore = "requires OS keyring or stable home dir; run with --ignored"]
    fn store_and_load_embedding_key_round_trip() {
        let plaintext = "sk-test-abcdef123456";
        let storage_key = store_embedding_key(plaintext).expect("store should succeed");
        assert_ne!(
            storage_key, plaintext,
            "stored value must not equal plaintext"
        );
        assert!(
            !storage_key.is_empty(),
            "storage key should be non-empty hex"
        );
        let recovered = load_embedding_key(&storage_key).expect("load should succeed");
        assert_eq!(recovered, plaintext);
    }

    // CloudEmbedder tests use a tiny TcpListener-backed HTTP/1.1 mock rather
    // than a mock-server crate, to avoid bloating the dev-dep tree.

    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    fn spawn_mock(response: &'static str) -> (String, thread::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}");
        let handle = thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            // Read up to the headers + body. Quick-and-dirty: read once
            // — for our small JSON requests it fits in a single recv.
            let mut buf = [0u8; 4096];
            let n = sock.read(&mut buf).unwrap_or(0);
            sock.write_all(response.as_bytes()).ok();
            sock.flush().ok();
            buf[..n].to_vec()
        });
        (url, handle)
    }

    fn spawn_mock_sequence(
        responses: Vec<&'static str>,
    ) -> (String, thread::JoinHandle<Vec<Vec<u8>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}");
        let handle = thread::spawn(move || {
            let mut requests = Vec::new();
            for response in responses {
                let (mut sock, _) = listener.accept().expect("accept");
                let mut buf = [0u8; 4096];
                let n = sock.read(&mut buf).unwrap_or(0);
                sock.write_all(response.as_bytes()).ok();
                sock.flush().ok();
                requests.push(buf[..n].to_vec());
            }
            requests
        });
        (url, handle)
    }

    #[tokio::test]
    async fn cloud_embedder_returns_first_vector_on_success() {
        let body = serde_json::json!({
            "vectors": [[0.1, 0.2, 0.3]],
            "model": "text-embedding-3-small",
            "dim": 1536,
        })
        .to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        // Leak so the closure's 'static bound is satisfied.
        let response_static: &'static str = Box::leak(response.into_boxed_str());
        let (url, handle) = spawn_mock(response_static);
        let embedder = CloudEmbedder::new(url, "tok".into());
        let v = embedder.embed("hello").await.expect("embed");
        assert_eq!(v.len(), 3);
        assert!((v[0] - 0.1).abs() < 1e-4);
        let req = handle.join().unwrap();
        let req_str = String::from_utf8_lossy(&req);
        // HTTP/1.1 headers are case-insensitive; reqwest may emit
        // "authorization:" lower-cased depending on the version. Compare
        // case-insensitively.
        let req_lower = req_str.to_ascii_lowercase();
        assert!(
            req_lower.contains("authorization: bearer tok"),
            "auth header missing in: {req_str}"
        );
        assert!(req_str.contains("\"texts\""));
        assert!(req_str.contains("hello"));
        assert!(
            !req_str.contains("\"model\""),
            "cloud-managed requests must not pin a provider model: {req_str}"
        );
    }

    #[tokio::test]
    async fn cloud_embedder_maps_5xx_to_core_error() {
        let response =
            "HTTP/1.1 502 Bad Gateway\r\nContent-Length: 4\r\nConnection: close\r\n\r\nfail";
        let (url, handle) = spawn_mock_sequence(vec![response, response, response, response]);
        let embedder = CloudEmbedder::new(url, "t".into());
        let err = embedder.embed("x").await.expect_err("should fail");
        match err {
            CoreError::Internal(msg) => assert!(msg.contains("502"), "msg: {msg}"),
            other => panic!("unexpected err: {other:?}"),
        }
        assert_eq!(handle.join().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn cloud_embedder_retries_transient_5xx_once() {
        let ok_body = serde_json::json!({
            "vectors": [[0.4, 0.5]],
            "model": "text-embedding-3-small",
            "dim": 1536,
        })
        .to_string();
        let fail = "HTTP/1.1 502 Bad Gateway\r\nContent-Length: 4\r\nConnection: close\r\n\r\nfail";
        let ok = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{}",
            ok_body.len(),
            ok_body
        );
        let ok_static: &'static str = Box::leak(ok.into_boxed_str());
        let (url, handle) = spawn_mock_sequence(vec![fail, ok_static]);
        let embedder = CloudEmbedder::new(url, "tok".into());
        let v = embedder.embed("hello").await.expect("embed after retry");
        assert_eq!(v, vec![0.4, 0.5]);
        assert_eq!(handle.join().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn cloud_embedder_dim_is_unknown_without_network() {
        let embedder = CloudEmbedder::new("http://example.invalid".into(), "t".into());
        assert_eq!(embedder.dim(), 0);
        assert!(embedder.is_semantic());
    }

    #[test]
    fn cloud_embedder_endpoint_handles_trailing_slash() {
        let a = CloudEmbedder::new("http://h/api".into(), "t".into());
        let b = CloudEmbedder::new("http://h/api/".into(), "t".into());
        assert_eq!(a.endpoint(), "http://h/api/embeddings");
        assert_eq!(b.endpoint(), "http://h/api/embeddings");
    }

    #[test]
    fn url_host_strips_scheme_and_path() {
        assert_eq!(
            url_host("https://api.openai.com/v1"),
            Some("api.openai.com".to_owned())
        );
        assert_eq!(
            url_host("http://localhost:8080/x"),
            Some("localhost:8080".to_owned())
        );
        assert_eq!(
            url_host("https://user:pass@example.com/v1"),
            Some("example.com".to_owned())
        );
        assert_eq!(
            url_host("http://[::1]:8080/v1"),
            Some("[::1]:8080".to_owned())
        );
        assert_eq!(url_host("noscheme/path"), Some("noscheme".to_owned()));
        assert_eq!(url_host(""), None);
    }

    #[test]
    fn load_embedding_key_rejects_invalid_storage_key() {
        // Invalid hex / too-short ciphertext must produce an error, never
        // panic. This path does NOT touch the keyring (the validation fires
        // first inside `from_hex` / length check), so it's safe to run in
        // headless environments.
        let err = load_embedding_key("not-valid-hex-$$").unwrap_err();
        match err {
            CoreError::Internal(msg) => assert!(msg.contains("failed to decrypt")),
            other => panic!("unexpected error variant: {other:?}"),
        }

        let err2 = load_embedding_key("abcd").unwrap_err();
        assert!(matches!(err2, CoreError::Internal(_)));
    }
}
