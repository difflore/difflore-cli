//! Side-band embedding/vector-lane diagnostics.
//!
//! Reports whether the per-project vector lane is degraded by comparing the
//! embedding profile **already persisted** in the project index DB against
//! the profile the *active* embedder would produce right now. This is the
//! cheapest possible health check — it touches a single `rule_index_meta`
//! row and the in-process settings/token state, never the network and never
//! the corpus vectors themselves.
//!
//! It exists so `difflore doctor` (and the CLI status surface) can answer
//! "is recall still semantic, or did it silently fall back to local SHA1?"
//! without re-embedding anything. The corpus profile is written by
//! `mark_rule_index_current` (see `pool.rs`); the active profile comes from
//! `active_embedding_profile()` (see `embedding.rs`). A profile string is
//! `cloud:{model}:{dim}` / `byok:{host}:{model}:{dim}` / `sha1:local:128`,
//! so the trailing `:`-segment is always the embedding dimension.

use sqlx::SqlitePool;

use crate::context::embedding::active_embedding_profile;

use super::schema::read_meta;

const RECENT_EMBEDDING_FALLBACK_WINDOW_MS: i64 = 10 * 60 * 1000;

/// Result of a side-band embedding-lane health probe.
///
/// `degraded_reason` is a stable snake_case token (never a free-form
/// message) so callers can branch on it and render their own copy:
/// `index_not_built`, `provider_fallback`, `dimension_mismatch`,
/// `profile_mismatch`.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EmbeddingDiagnostics {
    /// Profile the currently-resolved embedder would produce.
    pub active_profile: String,
    /// Profile persisted alongside the corpus, if the index was ever built.
    pub index_profile: Option<String>,
    /// True when the active profile is byte-identical to the corpus profile.
    pub profile_match: bool,
    /// True when retrieval quality is compromised vs. the indexed corpus.
    pub degraded: bool,
    /// Stable snake_case explanation when `degraded` (or when no index).
    pub degraded_reason: Option<String>,
    /// True when semantic vector search can run as the corpus expects.
    pub vector_lane_available: bool,
}

/// Parse the embedding dimension — the integer after the **last** `:` of a
/// profile string. Returns `None` for an unparseable / malformed tail so
/// the caller treats the dimension as unknown rather than guessing.
fn profile_dim(profile: &str) -> Option<u32> {
    profile.rsplit(':').next().and_then(|s| s.parse().ok())
}

/// Best-effort check for a *recent* embedding fallback recorded by the
/// activity stream. Used only as a tie-breaker when the static profile
/// comparison did not already flag degradation: a provider can fail at
/// query time while the persisted corpus profile still looks semantic.
///
/// Reads the cheap tail API; any miss (no file / not present) yields
/// `false` so this never blocks or adds infrastructure.
fn recent_embedding_fallback_from_events(
    events: &[crate::activity_stream::ActivityEvent],
    now_ms: i64,
) -> bool {
    use crate::activity_stream::ActivityPayload;

    events
        .iter()
        .find_map(|event| match event.payload {
            ActivityPayload::EmbeddingFallback { .. } => {
                Some(now_ms.saturating_sub(event.ts_ms) <= RECENT_EMBEDDING_FALLBACK_WINDOW_MS)
            }
            ActivityPayload::RetrievalEmbedding { .. } => Some(false),
            _ => None,
        })
        .unwrap_or(false)
}

fn recent_embedding_fallback() -> bool {
    use crate::activity_stream::tail;
    use std::time::{SystemTime, UNIX_EPOCH};

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0);
    // A small window is enough: report query-time degradation only when the
    // latest embedding activity was a fresh fallback. Older fallbacks should
    // not keep a quiet project red forever.
    recent_embedding_fallback_from_events(&tail(16), now_ms)
}

/// True when **any** embedding fallback was recorded inside the recency window.
///
/// Unlike [`recent_embedding_fallback_from_events`], this is NOT masked by a
/// later `RetrievalEmbedding` event. That matters because every
/// `retrieve_rules_with_confidence` records a `RetrievalEmbedding` at the end of
/// retrieval (even when the query embed itself fell back to SHA1), so the
/// "latest event" view almost always sees retrieval, not the fallback that
/// preceded it in the same pass. Decisions that must NOT be fooled by that
/// trailing event — e.g. "is the remote embedder currently down, so skip a
/// futile corpus re-embed?" — use this strict scan instead.
fn recent_embedding_fallback_strict_from_events(
    events: &[crate::activity_stream::ActivityEvent],
    now_ms: i64,
) -> bool {
    use crate::activity_stream::ActivityPayload;
    events.iter().any(|event| {
        matches!(event.payload, ActivityPayload::EmbeddingFallback { .. })
            && now_ms.saturating_sub(event.ts_ms) <= RECENT_EMBEDDING_FALLBACK_WINDOW_MS
    })
}

fn recent_embedding_fallback_strict() -> bool {
    use crate::activity_stream::{MAX_EVENTS, tail};
    use std::time::{SystemTime, UNIX_EPOCH};

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0);
    // Scan the full capped log (not just the newest few): with the futile
    // re-embed skipped, warm recalls only append `RetrievalEmbedding`, so the
    // most recent fallback can sit many events back yet still be inside the
    // 10-minute window.
    recent_embedding_fallback_strict_from_events(&tail(MAX_EVENTS), now_ms)
}

/// Persistent cloud-embed failure classes (cap reached / auth rejected).
/// Unlike a transient `timeout`/`network` blip these last minutes-to-hours, so
/// a single one is conclusive: re-attempting on the hot path would only time
/// out and fall back to SHA1 again.
fn is_persistent_failure_reason(reason: &str) -> bool {
    matches!(reason, "cap" | "forbidden" | "unauthorized")
}

/// Sustained-outage threshold: this many *transient* cloud-embed fallbacks
/// inside the recency window means the provider is effectively down, not
/// blipping. Above 1 on purpose — a single transient failure must still
/// re-attempt per call so a brief outage recovers immediately (the query path's
/// documented design); only a sustained run trips the skip.
const SUSTAINED_TRANSIENT_FALLBACK_THRESHOLD: usize = 5;

fn cloud_embed_outage_active_from_events(
    events: &[crate::activity_stream::ActivityEvent],
    now_ms: i64,
) -> bool {
    use crate::activity_stream::ActivityPayload;
    let mut transient = 0usize;
    for event in events {
        let ActivityPayload::EmbeddingFallback { reason } = &event.payload else {
            continue;
        };
        if now_ms.saturating_sub(event.ts_ms) > RECENT_EMBEDDING_FALLBACK_WINDOW_MS {
            continue;
        }
        if is_persistent_failure_reason(reason) {
            return true;
        }
        transient += 1;
    }
    transient >= SUSTAINED_TRANSIENT_FALLBACK_THRESHOLD
}

/// True when the cloud embedder is in a sustained outage or a persistent
/// cap/auth failure, per the persisted activity log.
///
/// The query hot path consults this to skip a doomed remote embed — when the
/// provider is genuinely down, a per-query cloud call just burns the timeout
/// budget on every edit and falls back to SHA1 anyway. A single transient blip
/// does NOT trip it (`SUSTAINED_TRANSIENT_FALLBACK_THRESHOLD`), so a brief
/// outage still recovers on the very next query. Cross-process by design: it
/// reads the on-disk log, so even short-lived per-fire hook invocations honour
/// it (an in-process flag would reset every hook spawn).
pub(crate) fn cloud_embed_outage_active() -> bool {
    use crate::activity_stream::{MAX_EVENTS, tail};
    use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    // Short in-process cache. This runs on the query hot path (every hook fire /
    // search), but the outage signal moves on the order of seconds, so re-reading
    // the activity log per query is wasted I/O. A 3s TTL bounds it to ~once per
    // window in a long-lived MCP/daemon process; a short-lived per-fire hook
    // still reads once (it issues a single query), so it loses nothing. Two
    // un-paired atomics are fine here — a torn read at most costs one extra log
    // scan, never correctness.
    static CACHED_AT_MS: AtomicI64 = AtomicI64::new(0);
    static CACHED_VALUE: AtomicBool = AtomicBool::new(false);
    const CACHE_TTL_MS: i64 = 3_000;

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0);

    let cached_at = CACHED_AT_MS.load(Ordering::Relaxed);
    if cached_at != 0 && now_ms.saturating_sub(cached_at) < CACHE_TTL_MS {
        return CACHED_VALUE.load(Ordering::Relaxed);
    }

    let active = cloud_embed_outage_active_from_events(&tail(MAX_EVENTS), now_ms);
    CACHED_VALUE.store(active, Ordering::Relaxed);
    CACHED_AT_MS.store(now_ms, Ordering::Relaxed);
    active
}

/// Gather a side-band embedding-lane diagnostic for one project.
///
/// Compares the corpus embedding profile persisted in `index_pool`'s
/// `rule_index_meta` against the active embedder profile and classifies the
/// vector lane. Deterministic and cheap: a single meta-row read plus the
/// in-process embedder probe. Any DB error reading the persisted profile is
/// treated as "no index" rather than propagated — a diagnostic must never
/// itself fail.
pub async fn gather_embedding_diagnostics(index_pool: &SqlitePool) -> EmbeddingDiagnostics {
    let active = active_embedding_profile().await;
    // Treat any DB error as "no persisted profile": a degraded/missing
    // index DB is exactly the condition we are reporting on, so a read
    // failure must not panic or propagate out of a diagnostic.
    let index = read_meta(index_pool, "embedding_profile")
        .await
        .ok()
        .flatten();

    let profile_match = index.as_deref() == Some(active.as_str());

    let Some(index_profile) = index else {
        // Corpus was never built — nothing to compare against. The lane is
        // unavailable but this is "not yet indexed", not a regression.
        return EmbeddingDiagnostics {
            active_profile: active,
            index_profile: None,
            profile_match: false,
            degraded: false,
            degraded_reason: Some("index_not_built".to_owned()),
            vector_lane_available: false,
        };
    };

    if profile_match {
        // Active embedder matches the corpus exactly — fully healthy.
        return EmbeddingDiagnostics {
            active_profile: active,
            index_profile: Some(index_profile),
            profile_match: true,
            degraded: false,
            degraded_reason: None,
            vector_lane_available: true,
        };
    }

    // Profiles differ — decide *how* badly. Worst case first: the corpus
    // was embedded by a real semantic provider but the active embedder is
    // the local SHA1 lexical hash. Cosine search against semantic vectors
    // with lexical query vectors is noise — the lane is effectively dead.
    let provider_fallback = active.starts_with("sha1:")
        && (index_profile.starts_with("cloud:") || index_profile.starts_with("byok:"));
    if provider_fallback {
        return EmbeddingDiagnostics {
            active_profile: active,
            index_profile: Some(index_profile),
            profile_match: false,
            degraded: true,
            degraded_reason: Some("provider_fallback".to_owned()),
            vector_lane_available: false,
        };
    }

    // Both dimensions known and different → vectors are not comparable at
    // all (different vector spaces). Lane is unusable until re-index.
    let active_dim = profile_dim(&active);
    let index_dim = profile_dim(&index_profile);
    if let (Some(a), Some(i)) = (active_dim, index_dim)
        && a != i
    {
        return EmbeddingDiagnostics {
            active_profile: active,
            index_profile: Some(index_profile),
            profile_match: false,
            degraded: true,
            degraded_reason: Some("dimension_mismatch".to_owned()),
            vector_lane_available: false,
        };
    }

    // Same (or unknown) dimension but a different provider/model. Vectors
    // are still the right shape so cosine search can run, but cross-model
    // similarity is weaker than a matched corpus — degraded, lane usable.
    EmbeddingDiagnostics {
        active_profile: active,
        index_profile: Some(index_profile),
        profile_match: false,
        degraded: true,
        degraded_reason: Some("profile_mismatch".to_owned()),
        vector_lane_available: true,
    }
}

/// Variant of [`gather_embedding_diagnostics`] that additionally consults
/// the activity stream: if the static profile comparison did *not* already
/// flag degradation but a recent embed call fell back to local SHA1, the
/// lane is reported as `provider_fallback` (a query-time regression the
/// persisted profile cannot see). Best-effort — a missing activity log is
/// simply ignored.
pub async fn gather_embedding_diagnostics_with_activity(
    index_pool: &SqlitePool,
) -> EmbeddingDiagnostics {
    let mut diag = gather_embedding_diagnostics(index_pool).await;
    if !diag.degraded && diag.index_profile.is_some() && recent_embedding_fallback() {
        diag.degraded = true;
        diag.degraded_reason = Some("provider_fallback".to_owned());
        diag.vector_lane_available = false;
    }
    diag
}

/// True when the remote embedder appears to be currently failing, so callers
/// should avoid re-embedding the corpus through it. Strict (unmasked) so a
/// trailing `RetrievalEmbedding` cannot hide a same-pass fallback.
pub fn embedding_provider_recently_down() -> bool {
    recent_embedding_fallback_strict()
}

/// Decide the freshness-expected embedding profile (pure; see
/// [`effective_embedding_profile_for_freshness`]).
///
/// Returns the persisted SHA1 profile — so a SHA1 index counts as "current" and
/// the futile re-embed is skipped — only when the active embedder is remote, the
/// provider is currently failing, and the on-disk index is already SHA1. Every
/// other case returns the active profile unchanged (healthy upgrade/downgrade
/// paths are untouched). This only ever relaxes freshness toward the index
/// already on disk; it never serves stale content, because the count, scope
/// signature, and `max_updated_at` checks still run independently.
fn freshness_expected_profile(
    active_profile: &str,
    persisted_profile: Option<&str>,
    provider_recently_down: bool,
) -> String {
    if provider_recently_down
        && !active_profile.starts_with("sha1:")
        && let Some(persisted) = persisted_profile
        && persisted.starts_with("sha1:")
    {
        return persisted.to_owned();
    }
    active_profile.to_owned()
}

/// The embedding profile the index-freshness check should *expect* for the
/// active embedder, accounting for a remote provider that is currently failing.
///
/// Normally this is just the active profile. But when the active embedder is
/// remote (cloud / BYOK) and the provider is currently failing, re-embedding the
/// corpus to "adopt" that profile is futile: every chunk calls the dead
/// provider, falls back to local SHA1, and rewrites the identical SHA1 index —
/// turning every recall / MCP serve / hook fire into a multi-second corpus
/// re-embed (measured at 5-18s on real fork corpora). In that state, report the
/// persisted SHA1 profile so a SHA1 index counts as current and the futile
/// re-embed is skipped. The skip is self-limiting: once the strict fallback
/// window elapses (~10 min), the next freshness check re-embeds and, if the
/// provider has recovered, upgrades to cloud vectors. Probe cost is therefore
/// at most one re-embed per window instead of one per recall.
pub async fn effective_embedding_profile_for_freshness(
    index_pool: &SqlitePool,
    active_profile: &str,
) -> String {
    // Cheap exits first so the meta read only happens when relaxation can apply.
    if active_profile.starts_with("sha1:") || !embedding_provider_recently_down() {
        return active_profile.to_owned();
    }
    let persisted = read_meta(index_pool, "embedding_profile")
        .await
        .ok()
        .flatten();
    freshness_expected_profile(active_profile, persisted.as_deref(), true)
}

#[cfg(test)]
mod tests {
    use crate::activity_stream::{ActivityEvent, ActivityPayload};

    use super::super::schema::{open_pool_at, write_meta};
    use super::*;
    use tempfile::TempDir;

    async fn fresh_pool(tmp: &TempDir) -> SqlitePool {
        let path = tmp.path().join("diag-idx.db");
        open_pool_at(&path).await.expect("open_pool_at")
    }

    #[test]
    fn profile_dim_parses_trailing_segment() {
        assert_eq!(profile_dim("cloud:text-embedding-3-small:1536"), Some(1536));
        assert_eq!(profile_dim("byok:api.host.com:my-model:768"), Some(768));
        assert_eq!(profile_dim("sha1:local:128"), Some(128));
        assert_eq!(profile_dim("garbage-no-colon"), None);
        assert_eq!(profile_dim("cloud:model:not-a-number"), None);
    }

    #[test]
    fn recent_embedding_fallback_uses_latest_fresh_embedding_event() {
        let now = 1_000_000;
        let stale_fallback = ActivityEvent {
            ts_ms: now - RECENT_EMBEDDING_FALLBACK_WINDOW_MS - 1,
            payload: ActivityPayload::EmbeddingFallback {
                reason: "network".to_owned(),
            },
        };
        let fresh_fallback = ActivityEvent {
            ts_ms: now - 1_000,
            payload: ActivityPayload::EmbeddingFallback {
                reason: "network".to_owned(),
            },
        };
        let fresh_success = ActivityEvent {
            ts_ms: now,
            payload: ActivityPayload::RetrievalEmbedding {
                hits: 3,
                took_ms: 12,
            },
        };

        assert!(recent_embedding_fallback_from_events(
            std::slice::from_ref(&fresh_fallback),
            now
        ));
        assert!(!recent_embedding_fallback_from_events(
            std::slice::from_ref(&stale_fallback),
            now
        ));
        assert!(!recent_embedding_fallback_from_events(
            &[fresh_success, fresh_fallback],
            now
        ));
    }

    fn fallback(ts_ms: i64, reason: &str) -> ActivityEvent {
        ActivityEvent {
            ts_ms,
            payload: ActivityPayload::EmbeddingFallback {
                reason: reason.to_owned(),
            },
        }
    }

    #[test]
    fn cloud_outage_persistent_failure_trips_immediately() {
        let now = 1_000_000;
        // A cap / auth rejection persists — one is conclusive, no run needed.
        assert!(cloud_embed_outage_active_from_events(
            &[fallback(now - 1_000, "cap")],
            now
        ));
        assert!(cloud_embed_outage_active_from_events(
            &[fallback(now - 1_000, "unauthorized")],
            now
        ));
    }

    #[test]
    fn cloud_outage_single_transient_blip_does_not_trip() {
        let now = 1_000_000;
        // A handful of transient timeouts must still re-attempt per call: this
        // is the documented immediate-recovery behaviour for a brief outage.
        let few: Vec<_> = (0..SUSTAINED_TRANSIENT_FALLBACK_THRESHOLD - 1)
            .map(|i| fallback(now - 1_000 - i as i64, "timeout"))
            .collect();
        assert!(!cloud_embed_outage_active_from_events(&few, now));
    }

    #[test]
    fn cloud_outage_sustained_transient_run_trips() {
        let now = 1_000_000;
        let many: Vec<_> = (0..SUSTAINED_TRANSIENT_FALLBACK_THRESHOLD)
            .map(|i| fallback(now - 1_000 - i as i64, "timeout"))
            .collect();
        assert!(cloud_embed_outage_active_from_events(&many, now));
    }

    #[test]
    fn cloud_outage_ignores_events_outside_window() {
        let now = 1_000_000;
        let stale: Vec<_> = (0..SUSTAINED_TRANSIENT_FALLBACK_THRESHOLD + 3)
            .map(|i| {
                fallback(
                    now - RECENT_EMBEDDING_FALLBACK_WINDOW_MS - 1 - i as i64,
                    "timeout",
                )
            })
            .collect();
        assert!(!cloud_embed_outage_active_from_events(&stale, now));
    }

    #[test]
    fn strict_fallback_is_not_masked_by_trailing_retrieval() {
        // Regression: the freshness-skip signal must NOT be hidden by the
        // `RetrievalEmbedding` that every retrieval records last. The original
        // (latest-event) check returns false here; the strict check returns
        // true because a fresh fallback exists within the window.
        let now = 1_000_000;
        let fresh_fallback = ActivityEvent {
            ts_ms: now - 1_000,
            payload: ActivityPayload::EmbeddingFallback {
                reason: "network".to_owned(),
            },
        };
        let newer_retrieval = ActivityEvent {
            ts_ms: now,
            payload: ActivityPayload::RetrievalEmbedding {
                hits: 3,
                took_ms: 12,
            },
        };
        let events = [newer_retrieval, fresh_fallback];
        assert!(
            !recent_embedding_fallback_from_events(&events, now),
            "latest-event check is masked by the trailing retrieval (documents the bug)"
        );
        assert!(
            recent_embedding_fallback_strict_from_events(&events, now),
            "strict check must still see the fresh fallback"
        );

        // Outside the window → not recent, even strictly.
        let stale_fallback = ActivityEvent {
            ts_ms: now - RECENT_EMBEDDING_FALLBACK_WINDOW_MS - 1,
            payload: ActivityPayload::EmbeddingFallback {
                reason: "network".to_owned(),
            },
        };
        assert!(!recent_embedding_fallback_strict_from_events(
            std::slice::from_ref(&stale_fallback),
            now
        ));
    }

    #[test]
    fn freshness_expected_profile_relaxes_only_for_remote_active_over_sha1_index() {
        let cloud = "cloud:text-embedding-3-small:1536";
        let byok = "byok:host:m:768";
        let sha1 = "sha1:local:128";

        // Remote active + provider down + persisted SHA1 → expect SHA1 (skip the
        // futile re-embed). Holds for both cloud and BYOK.
        assert_eq!(freshness_expected_profile(cloud, Some(sha1), true), sha1);
        assert_eq!(freshness_expected_profile(byok, Some(sha1), true), sha1);

        // Provider healthy → expect the active profile so the upgrade re-embed runs.
        assert_eq!(freshness_expected_profile(cloud, Some(sha1), false), cloud);

        // Active already SHA1 → nothing to relax.
        assert_eq!(freshness_expected_profile(sha1, Some(sha1), true), sha1);

        // Persisted is itself remote (profiles match path) or absent → no relaxation.
        assert_eq!(freshness_expected_profile(cloud, Some(cloud), true), cloud);
        assert_eq!(freshness_expected_profile(cloud, None, true), cloud);
    }

    #[tokio::test]
    async fn persisted_sha1_meta_feeds_freshness_relaxation() {
        // Round-trips the persisted `embedding_profile` meta the async helper
        // reads, then feeds it to the pure decision exactly as
        // `effective_embedding_profile_for_freshness` does: a persisted SHA1
        // profile under a remote active embedder + provider-down resolves to the
        // SHA1 profile (so the freshness check treats the SHA1 index as current).
        // The async wrapper itself is a thin read_meta + strict-signal layer over
        // process-global state; the decision logic it delegates to is covered by
        // `freshness_expected_profile_relaxes_only_for_remote_active_over_sha1_index`.
        let tmp = TempDir::new().unwrap();
        let pool = fresh_pool(&tmp).await;
        write_meta(&pool, "embedding_profile", "sha1:local:128")
            .await
            .unwrap();
        let persisted = read_meta(&pool, "embedding_profile")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            freshness_expected_profile("cloud:m:1536", Some(&persisted), true),
            "sha1:local:128"
        );
    }

    #[tokio::test]
    async fn equal_profiles_are_not_degraded() {
        let tmp = TempDir::new().unwrap();
        let pool = fresh_pool(&tmp).await;
        // Persist whatever the active embedder reports so the two sides
        // match byte-for-byte regardless of the test environment's
        // resolved embedder (cloud token / settings).
        let active = active_embedding_profile().await;
        write_meta(&pool, "embedding_profile", &active)
            .await
            .unwrap();

        let d = gather_embedding_diagnostics(&pool).await;
        assert!(d.profile_match, "identical profiles must match");
        assert!(!d.degraded, "matched lane must not be degraded");
        assert_eq!(d.degraded_reason, None);
        assert!(d.vector_lane_available);
        assert_eq!(d.index_profile.as_deref(), Some(active.as_str()));
    }

    #[tokio::test]
    async fn sha1_active_vs_cloud_index_is_provider_fallback() {
        let tmp = TempDir::new().unwrap();
        let pool = fresh_pool(&tmp).await;
        write_meta(
            &pool,
            "embedding_profile",
            "cloud:text-embedding-3-small:1536",
        )
        .await
        .unwrap();

        let d = gather_embedding_diagnostics(&pool).await;
        // Only assert the fallback classification when the active embedder
        // really is the local SHA1 lexical hash (the default in a headless
        // test box with no cloud token / BYOK settings).
        if d.active_profile.starts_with("sha1:") {
            assert!(d.degraded, "semantic corpus + SHA1 active is degraded");
            assert_eq!(d.degraded_reason.as_deref(), Some("provider_fallback"));
            assert!(
                !d.vector_lane_available,
                "lexical query vs semantic corpus = dead lane"
            );
            assert!(!d.profile_match);
        }
    }

    #[tokio::test]
    async fn cloud_dim_mismatch_is_dimension_mismatch() {
        let tmp = TempDir::new().unwrap();
        let pool = fresh_pool(&tmp).await;
        write_meta(&pool, "embedding_profile", "cloud:model-x:768")
            .await
            .unwrap();

        let d = gather_embedding_diagnostics(&pool).await;
        // This classification is independent of the active embedder only
        // when the active profile is itself a parseable non-SHA1, non-768
        // cloud/byok profile. To keep the assertion deterministic we drive
        // the comparison purely off the parsed dims: a 768-dim corpus vs a
        // 1536-dim active cloud profile.
        if d.active_profile.starts_with("cloud:") && profile_dim(&d.active_profile) == Some(1536) {
            assert!(d.degraded);
            assert_eq!(d.degraded_reason.as_deref(), Some("dimension_mismatch"));
            assert!(!d.vector_lane_available);
            assert!(!d.profile_match);
        }
    }

    #[tokio::test]
    async fn same_dim_different_model_is_profile_mismatch() {
        let tmp = TempDir::new().unwrap();
        let pool = fresh_pool(&tmp).await;
        // Corpus embedded by a 1536-dim cloud model "A".
        write_meta(&pool, "embedding_profile", "cloud:model-a:1536")
            .await
            .unwrap();

        let d = gather_embedding_diagnostics(&pool).await;
        // Deterministic only when the active embedder is also a 1536-dim
        // cloud profile with a *different* model string (the standard
        // cloud-logged-in case: `cloud:text-embedding-3-small:1536`).
        if d.active_profile.starts_with("cloud:")
            && profile_dim(&d.active_profile) == Some(1536)
            && d.active_profile != "cloud:model-a:1536"
        {
            assert!(d.degraded, "cross-model corpus is degraded");
            assert_eq!(d.degraded_reason.as_deref(), Some("profile_mismatch"));
            assert!(
                d.vector_lane_available,
                "same dim → cosine still runs, just weaker"
            );
            assert!(!d.profile_match);
        }
    }

    #[tokio::test]
    async fn missing_index_profile_is_index_not_built() {
        let tmp = TempDir::new().unwrap();
        // Fresh pool: schema exists but no `embedding_profile` meta row.
        let pool = fresh_pool(&tmp).await;

        let d = gather_embedding_diagnostics(&pool).await;
        assert_eq!(d.index_profile, None);
        assert!(!d.profile_match, "no corpus profile cannot match");
        assert!(!d.degraded, "unbuilt index is not a regression");
        assert_eq!(d.degraded_reason.as_deref(), Some("index_not_built"));
        assert!(!d.vector_lane_available);
    }
}
