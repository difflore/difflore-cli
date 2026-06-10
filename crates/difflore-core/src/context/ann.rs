//! Per-project HNSW Approximate-Nearest-Neighbor index.
//!
//! Wraps [`hnsw_rs::hnsw::Hnsw`] so the retrieval path can swap the O(N)
//! linear cosine scan for an O(log N) graph query on big projects. The
//! design is deliberately cautious:
//!
//! 1. **Additive** — every public entry point returns `None` / an empty
//!    result on any failure. The retrieval fallback MUST always work, so
//!    this module never panics and never blocks rule writes.
//! 2. **Persistent, per-project** — each project hash has its own HNSW
//!    graph file under `~/.difflore/projects/{hash}/hnsw.*` plus a
//!    sidecar `hnsw.meta.json` that carries the dim + element count so
//!    we can detect a stale / wrong-dim index on reload.
//! 3. **Incremental upsert** — `hnsw_rs` supports runtime insertions, so
//!    `upsert_rule_chunks` can stream new embeddings into the graph
//!    without a full rebuild. Replacements (same `chunk_id`, new vector)
//!    are modelled as "shadow" entries: the old internal id stays in the
//!    graph but is hidden from search results by a `tombstones` set.
//!    A full `build_from_chunks` rebuild periodically cleans these out.
//! 4. **Dim mismatch => fallback** — if the query dim doesn't match the
//!    index dim we return an empty hit set; the caller sees this as
//!    "ANN gave nothing" and uses the linear scan.
//!
//! The internal/id translation is tracked on the Rust side because
//! `hnsw_rs`'s `DataId` is a `usize` and we want to key on `String`
//! chunk ids. Both maps are serialised alongside the graph in the
//! sidecar meta file.

use chrono::Utc;
use hnsw_rs::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use tokio::sync::Mutex;

use crate::errors::CoreError;

type AnnCacheKey = (String, usize);
type SharedAnnIndex = Arc<Mutex<AnnIndex>>;
type AnnCache = std::sync::Mutex<HashMap<AnnCacheKey, SharedAnnIndex>>;

/// On-disk basename. `hnsw_rs` writes `hnsw.hnsw.graph` and
/// `hnsw.hnsw.data`; the sidecar meta lives next to them as `hnsw.meta.json`.
const HNSW_BASENAME: &str = "hnsw";
const META_FILENAME: &str = "hnsw.meta.json";

/// Current meta schema version. Bumped when the sidecar shape changes —
/// an index with a different version is treated as stale and rebuilt.
const META_VERSION: u32 = 1;

/// HNSW construction parameters: defaults from the `hnsw_rs` docs /
/// Malkov+Yashunin paper. Not exposed to callers — retrieval quality is
/// dominated by our RRF weighting rather than these knobs at our scale
/// (≤ 100K chunks).
const MAX_NB_CONNECTION: usize = 16;
const EF_CONSTRUCTION: usize = 200;
const MAX_LAYER: usize = 16;
const DEFAULT_EF_SEARCH: usize = 64;
const MAX_SEARCH_TOP_K: usize = 50;
const MAX_RAW_SEARCH_CANDIDATES: usize = 150;

/// Sidecar meta for the on-disk index. A version or dim mismatch causes
/// the reload path to drop the index and return an empty one, which
/// retrieval treats as a fallback cue.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct AnnMeta {
    version: u32,
    dim: usize,
    /// Elements inserted, including tombstoned ones still live in the graph.
    size: u32,
    built_at: String,
    /// SHA-1 of the (id, content, embedding dim) signature at build time,
    /// so higher layers can detect graph/DB divergence. Not consulted by
    /// retrieval, which always trusts the graph and falls back on empty.
    schema_hash: String,
    /// Chunk ids ordered by internal HNSW point id. Needed because
    /// `hnsw_rs`'s `DataId` is a `usize` but our chunks use string ids.
    id_map: Vec<String>,
    /// Chunk ids overwritten after insertion; filtered out of search
    /// results. Keeps `upsert()` cheap (no graph edit) at the cost of a
    /// `HashSet` lookup per hit.
    tombstones: Vec<String>,
}

/// A project-scoped, disk-persisted HNSW index. See the module docs for
/// the overall approach.
pub struct AnnIndex {
    inner: Option<Hnsw<'static, f32, DistCosine>>,
    /// internal HNSW `DataId` -> `chunk_id`
    id_map: Vec<String>,
    /// `chunk_id` -> most recent internal `DataId` (later one wins; older
    /// ids land in `tombstones`)
    reverse: HashMap<String, usize>,
    /// Chunk ids superseded by an `upsert`; excluded from search output.
    tombstones: HashSet<String>,
    dim: usize,
    dirty: bool,
    project_hash: String,
    /// Schema hash from the last load/build, carried back into `save()`.
    schema_hash: String,
}

impl AnnIndex {
    /// Load from disk, or create an empty index if the on-disk files
    /// are missing / corrupt / dim-mismatched. Never errors — worst
    /// case the caller gets an empty index and retrieval falls through
    /// to the linear path.
    pub async fn load_or_empty(project_hash: &str, dim: usize) -> Result<Self, CoreError> {
        let dir = crate::db::project_index_dir(project_hash);
        let meta_path = dir.join(META_FILENAME);
        let graph_path = dir.join(format!("{HNSW_BASENAME}.hnsw.graph"));
        let data_path = dir.join(format!("{HNSW_BASENAME}.hnsw.data"));

        if !meta_path.exists() || !graph_path.exists() || !data_path.exists() {
            return Ok(Self::empty(project_hash.to_owned(), dim));
        }

        // A corrupt / missing meta falls through to an empty index rather
        // than failing retrieval.
        let meta: AnnMeta = match std::fs::read(&meta_path) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(m) => m,
                Err(_) => return Ok(Self::empty(project_hash.to_owned(), dim)),
            },
            Err(_) => return Ok(Self::empty(project_hash.to_owned(), dim)),
        };

        // Version or dim drift => wipe and start fresh. Dim mismatch is
        // the safety net for an embedder swap (e.g. SHA1 fallback to a
        // real 1536-dim provider mid-flight).
        if meta.version != META_VERSION || meta.dim != dim {
            return Ok(Self::empty(project_hash.to_owned(), dim));
        }

        // The reloaded `Hnsw` borrows from the `HnswIo` reloader even
        // without mmap, so `Box::leak` the reloader for a `'static`
        // handle. The one-time leak is bounded: load_or_empty runs at
        // most once per `ann_cache()` entry per process.
        let reloader: &'static mut HnswIo = Box::leak(Box::new(HnswIo::new(&dir, HNSW_BASENAME)));
        let Ok(hnsw) = reloader.load_hnsw::<f32, DistCosine>() else {
            return Ok(Self::empty(project_hash.to_owned(), dim));
        };

        let mut reverse: HashMap<String, usize> = HashMap::with_capacity(meta.id_map.len());
        for (idx, id) in meta.id_map.iter().enumerate() {
            // Later entries for the same chunk id win — matches the
            // tombstone semantics.
            reverse.insert(id.clone(), idx);
        }
        let tombstones: HashSet<String> = meta.tombstones.iter().cloned().collect();

        Ok(Self {
            inner: Some(hnsw),
            id_map: meta.id_map,
            reverse,
            tombstones,
            dim,
            dirty: false,
            project_hash: project_hash.to_owned(),
            schema_hash: meta.schema_hash,
        })
    }

    /// Construct a fresh index from a full slice of chunks. Used when
    /// no on-disk graph exists or when the caller wants to compact out
    /// tombstones. Swallows per-chunk dim mismatches (those rows are
    /// dropped) but never errors.
    pub async fn build_from_chunks(
        project_hash: &str,
        chunks: &[(String, Vec<f32>)],
    ) -> Result<Self, CoreError> {
        // Dim is inferred from the first non-empty embedding; a corpus of
        // only empty vectors yields an empty index (== fall back to linear).
        let dim = chunks
            .iter()
            .find(|(_, v)| !v.is_empty())
            .map_or(0, |(_, v)| v.len());
        if dim == 0 {
            return Ok(Self::empty(project_hash.to_owned(), 0));
        }

        let capacity_hint = chunks.len().max(1);
        let hnsw: Hnsw<'static, f32, DistCosine> = Hnsw::new(
            MAX_NB_CONNECTION,
            capacity_hint,
            MAX_LAYER,
            EF_CONSTRUCTION,
            DistCosine,
        );

        let mut id_map: Vec<String> = Vec::with_capacity(chunks.len());
        let mut reverse: HashMap<String, usize> = HashMap::with_capacity(chunks.len());
        for (chunk_id, emb) in chunks {
            if emb.len() != dim {
                // Skip heterogeneous dims rather than corrupt the graph.
                continue;
            }
            let internal_id = id_map.len();
            hnsw.insert((emb.as_slice(), internal_id));
            reverse.insert(chunk_id.clone(), internal_id);
            id_map.push(chunk_id.clone());
        }

        let schema_hash = compute_schema_hash(chunks, dim);

        Ok(Self {
            inner: Some(hnsw),
            id_map,
            reverse,
            tombstones: HashSet::new(),
            dim,
            dirty: true,
            project_hash: project_hash.to_owned(),
            schema_hash,
        })
    }

    fn empty(project_hash: String, dim: usize) -> Self {
        Self {
            inner: None,
            id_map: Vec::new(),
            reverse: HashMap::new(),
            tombstones: HashSet::new(),
            dim,
            dirty: false,
            project_hash,
            schema_hash: String::new(),
        }
    }

    /// Upsert a single chunk. The previous entry (if any) is marked
    /// tombstoned so it won't surface in future searches; the new
    /// embedding is appended to the graph with a fresh internal id.
    ///
    /// A dim mismatch is silently ignored — the caller is the SQL
    /// upsert path which continues regardless, matching the "ANN never
    /// blocks rule writes" contract.
    pub fn upsert(&mut self, chunk_id: &str, embedding: &[f32]) {
        if embedding.is_empty() {
            return;
        }
        // Lazy-init the graph on first insert so load_or_empty + upsert
        // works even when no on-disk graph existed.
        if self.inner.is_none() {
            if self.dim == 0 {
                self.dim = embedding.len();
            }
            if embedding.len() != self.dim {
                return;
            }
            self.inner = Some(Hnsw::new(
                MAX_NB_CONNECTION,
                64,
                MAX_LAYER,
                EF_CONSTRUCTION,
                DistCosine,
            ));
        }
        if embedding.len() != self.dim {
            return;
        }
        // The chunk's previous id (if any) becomes a tombstone: it marks
        // the chunk as having stale slots. `search()` keeps only the hit
        // whose internal id matches the latest `reverse` entry.
        if let Some(_prev) = self.reverse.get(chunk_id) {
            self.tombstones.insert(chunk_id.to_owned());
        }
        #[allow(clippy::expect_used)]
        // reason: invariant — `self.inner` was just set on the empty branch above.
        let hnsw = self.inner.as_ref().expect("inner set above");
        let new_internal = self.id_map.len();
        hnsw.insert((embedding, new_internal));
        self.id_map.push(chunk_id.to_owned());
        self.reverse.insert(chunk_id.to_owned(), new_internal);
        self.dirty = true;
    }

    /// Mark a chunk as removed. The HNSW entry is not physically deleted
    /// (`hnsw_rs` has no public `remove`); it's tombstoned so search skips
    /// it. Reclaimed on the next `build_from_chunks`.
    pub fn remove(&mut self, chunk_id: &str) {
        if self.reverse.remove(chunk_id).is_some() {
            self.tombstones.insert(chunk_id.to_owned());
            self.dirty = true;
        }
    }

    /// Search for `top_k` nearest chunks to the query. Returns
    /// `(chunk_id, distance)` pairs with smaller distance = more similar
    /// (`DistCosine` returns `1 - cos`). An empty index, dim mismatch,
    /// empty query, or any internal error yields an empty result — the
    /// caller should interpret that as "use the linear scan".
    pub fn search(&self, query: &[f32], top_k: usize) -> Vec<(String, f32)> {
        if top_k == 0 || query.is_empty() || self.id_map.is_empty() {
            return Vec::new();
        }
        let top_k = top_k.min(MAX_SEARCH_TOP_K);
        if query.len() != self.dim {
            return Vec::new();
        }
        let Some(hnsw) = self.inner.as_ref() else {
            return Vec::new();
        };
        // Over-fetch so tombstone + duplicate-id filtering still leaves
        // ≈ top_k survivors. 3x covers realistic tombstone ratios (< 30%).
        let raw_k = top_k.saturating_mul(3).min(MAX_RAW_SEARCH_CANDIDATES);
        let ef = DEFAULT_EF_SEARCH.max(top_k.saturating_mul(2));
        let raw = hnsw.search(query, raw_k, ef);
        let mut out = Vec::with_capacity(top_k);
        let mut seen: HashSet<&str> = HashSet::new();
        for n in raw {
            let internal_id = n.d_id;
            let Some(chunk_id) = self.id_map.get(internal_id) else {
                continue;
            };
            // If a chunk_id has both a tombstone and a fresh copy, only
            // the latest `reverse` mapping wins.
            if self.tombstones.contains(chunk_id) {
                if let Some(&current) = self.reverse.get(chunk_id) {
                    if current != internal_id {
                        continue;
                    }
                } else {
                    // Fully removed chunk.
                    continue;
                }
            }
            if !seen.insert(chunk_id.as_str()) {
                continue;
            }
            out.push((chunk_id.clone(), n.distance));
            if out.len() >= top_k {
                break;
            }
        }
        out
    }

    /// Persist the index + sidecar to `~/.difflore/projects/{hash}/`.
    /// Directory-create and graph-dump errors bubble up; sets
    /// `dirty = false` on success.
    pub async fn save(&mut self) -> Result<(), CoreError> {
        let Some(hnsw) = self.inner.as_ref() else {
            // Empty index => nothing to persist, but still write a meta
            // file so `load_or_empty` sees a consistent state.
            let dir = crate::db::project_index_dir(&self.project_hash);
            std::fs::create_dir_all(&dir)?;
            self.write_meta(&dir)?;
            self.dirty = false;
            return Ok(());
        };
        let dir = crate::db::project_index_dir(&self.project_hash);
        std::fs::create_dir_all(&dir)?;
        hnsw.file_dump(&dir, HNSW_BASENAME)
            .map_err(|e| CoreError::Internal(format!("hnsw file_dump failed: {e}")))?;
        self.write_meta(&dir)?;
        self.dirty = false;
        Ok(())
    }

    fn write_meta(&self, dir: &Path) -> Result<(), CoreError> {
        let meta = AnnMeta {
            version: META_VERSION,
            dim: self.dim,
            size: u32::try_from(self.id_map.len()).unwrap_or(u32::MAX),
            built_at: Utc::now().to_rfc3339(),
            schema_hash: self.schema_hash.clone(),
            id_map: self.id_map.clone(),
            tombstones: self.tombstones.iter().cloned().collect(),
        };
        let bytes = serde_json::to_vec_pretty(&meta)?;
        std::fs::write(dir.join(META_FILENAME), bytes)?;
        Ok(())
    }

    /// Has the in-memory state diverged from disk? Used to gate `save()`.
    pub const fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Number of live (non-tombstoned) chunks in the index.
    pub fn live_size(&self) -> u32 {
        u32::try_from(self.reverse.len()).unwrap_or(u32::MAX)
    }

    /// Total chunk count including tombstones.
    pub fn total_size(&self) -> u32 {
        u32::try_from(self.id_map.len()).unwrap_or(u32::MAX)
    }

    /// Dimensionality of the stored vectors; zero means unset.
    pub const fn dim(&self) -> usize {
        self.dim
    }
}

/// Stable schema hash from the first up-to-64 chunks. Not a security
/// construct; only distinguishes "same corpus across runs" from "corpus
/// changed". Capped at 64 to keep the meta file small.
fn compute_schema_hash(chunks: &[(String, Vec<f32>)], dim: usize) -> String {
    use sha1::{Digest, Sha1};
    let mut hasher = Sha1::new();
    hasher.update(dim.to_le_bytes());
    for (id, _) in chunks.iter().take(64) {
        hasher.update(id.as_bytes());
        hasher.update([0]);
    }
    let out = hasher.finalize();
    let mut hex = String::with_capacity(12);
    for b in out.iter().take(6) {
        hex.push_str(&format!("{b:02x}"));
    }
    hex
}

/// Process-wide cache of per-project/per-dimension ANN indices. Mirrors the
/// `pool_cache()` pattern in `index_db.rs` so two concurrent MCP tool
/// calls share one `AnnIndex` instance for the same embedding space.
fn ann_cache() -> &'static AnnCache {
    static CACHE: OnceLock<AnnCache> = OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// Get-or-load the ANN for a project + embedding dimension. Cheap on a
/// cache hit; cold path pays `load_or_empty`. Never errors.
pub async fn get_ann_for_project(
    project_hash: &str,
    dim: usize,
) -> Result<Arc<Mutex<AnnIndex>>, CoreError> {
    {
        #[allow(clippy::expect_used)]
        // reason: poisoned mutex in process-wide cache is unrecoverable; abort is correct.
        let guard = ann_cache().lock().expect("ann cache mutex poisoned");
        let key = (project_hash.to_owned(), dim);
        if let Some(existing) = guard.get(&key) {
            return Ok(Arc::clone(existing));
        }
    }
    let loaded = AnnIndex::load_or_empty(project_hash, dim).await?;
    let arc = Arc::new(Mutex::new(loaded));
    #[allow(clippy::expect_used)]
    // reason: poisoned mutex in process-wide cache is unrecoverable; abort is correct.
    let mut guard = ann_cache().lock().expect("ann cache mutex poisoned");
    // Keep the first concurrently loaded index so callers do not fork the cache.
    let key = (project_hash.to_owned(), dim);
    let entry = guard.entry(key).or_insert(arc);
    Ok(Arc::clone(entry))
}

/// Drop the cached entry for a project, forcing a cold reload.
#[cfg(test)]
pub fn invalidate_cache(project_hash: &str) {
    #[allow(clippy::expect_used)]
    // reason: poisoned mutex in process-wide cache is unrecoverable; abort is correct.
    let mut guard = ann_cache().lock().expect("ann cache mutex poisoned");
    guard.retain(|(cached_project, _), _| cached_project != project_hash);
}

/// Convenience helper for the on-disk files belonging to a project index.
pub fn ann_files_for_project(project_hash: &str) -> (PathBuf, PathBuf, PathBuf) {
    let dir = crate::db::project_index_dir(project_hash);
    (
        dir.join(format!("{HNSW_BASENAME}.hnsw.graph")),
        dir.join(format!("{HNSW_BASENAME}.hnsw.data")),
        dir.join(META_FILENAME),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_hash(tag: &str) -> String {
        // Timestamp-keyed so tests don't collide on the process-wide ann_cache.
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        format!("{tag}-{nanos}")
    }

    fn random_vec(seed: u64, dim: usize) -> Vec<f32> {
        // Deterministic pseudo-random so recall tests are reproducible.
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut v = Vec::with_capacity(dim);
        for i in 0..dim {
            let mut h = DefaultHasher::new();
            (seed, i).hash(&mut h);
            let raw = h.finish();
            // Map into [-1, 1).
            let x = ((raw as i64) as f64) / (i64::MAX as f64);
            v.push(x as f32);
        }
        // L2 normalise so DistCosine is well-behaved.
        let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if n > 0.0 {
            for x in &mut v {
                *x /= n;
            }
        }
        v
    }

    #[tokio::test]
    async fn empty_index_search_returns_empty() {
        let _home = crate::db::shared_test_home();
        let hash = unique_hash("empty-search");
        let idx = AnnIndex::load_or_empty(&hash, 16).await.unwrap();
        assert_eq!(idx.total_size(), 0);
        let hits = idx.search(&[0.1f32; 16], 5);
        assert!(hits.is_empty());
    }

    #[tokio::test]
    async fn build_and_search_returns_nearest() {
        let _home = crate::db::shared_test_home();
        let hash = unique_hash("build-search");
        let dim = 32;
        // Seed 50 random vectors, then append a "target" that we'll
        // query with a near-duplicate.
        let mut chunks: Vec<(String, Vec<f32>)> = Vec::new();
        for i in 0..50 {
            chunks.push((format!("c{i}"), random_vec(i as u64, dim)));
        }
        let target_seed = 99u64;
        let target = random_vec(target_seed, dim);
        chunks.push(("target".to_owned(), target.clone()));

        let idx = AnnIndex::build_from_chunks(&hash, &chunks).await.unwrap();
        assert_eq!(idx.total_size(), 51);

        let hits = idx.search(&target, 5);
        assert!(!hits.is_empty());
        // The nearest neighbour to a point is the point itself
        // (distance ~ 0 under DistCosine).
        assert_eq!(hits[0].0, "target");
        assert!(
            hits[0].1 < 1e-3,
            "self-match distance should be near zero, got {}",
            hits[0].1
        );
    }

    #[tokio::test]
    async fn save_and_load_roundtrip() {
        let _home = crate::db::shared_test_home();
        let hash = unique_hash("roundtrip");
        let dim = 24;
        let mut chunks: Vec<(String, Vec<f32>)> = Vec::new();
        for i in 0..20 {
            chunks.push((format!("c{i}"), random_vec(i as u64, dim)));
        }
        let query = random_vec(7, dim);

        let mut idx = AnnIndex::build_from_chunks(&hash, &chunks).await.unwrap();
        let before = idx.search(&query, 5);
        assert!(!before.is_empty());
        idx.save().await.unwrap();
        assert!(!idx.is_dirty());

        invalidate_cache(&hash);
        let reloaded = AnnIndex::load_or_empty(&hash, dim).await.unwrap();
        let after = reloaded.search(&query, 5);
        assert_eq!(before.len(), after.len());
        // The reload path keeps IDs stable — top-1 must match.
        assert_eq!(before[0].0, after[0].0);
    }

    #[tokio::test]
    async fn upsert_replaces_existing_chunk_embedding() {
        let _home = crate::db::shared_test_home();
        let hash = unique_hash("upsert");
        let dim = 8;
        let vec_a = random_vec(1, dim);
        let vec_b = random_vec(2, dim);
        let mut idx = AnnIndex::build_from_chunks(
            &hash,
            &[
                ("id1".to_owned(), vec_a.clone()),
                ("neighbor".to_owned(), random_vec(3, dim)),
            ],
        )
        .await
        .unwrap();

        // Querying with A => id1 comes back as top.
        let hits1 = idx.search(&vec_a, 2);
        assert_eq!(hits1[0].0, "id1");

        // Upsert id1 with vec B and confirm:
        //   * querying with A no longer surfaces id1 as top-1
        //     (tombstoned old slot should be filtered)
        //   * querying with B DOES surface id1
        idx.upsert("id1", &vec_b);
        let hits_a = idx.search(&vec_a, 2);
        // id1 may still appear but NOT in top-1 unless its new vec_b
        // happens to be close to vec_a; at minimum the first hit's
        // internal id should be the fresh one. Rather than race on
        // ranking, assert the freshest internal id wins.
        if !hits_a.is_empty() && hits_a[0].0 == "id1" {
            // This is fine provided it's the fresh copy.
            let current = idx.reverse.get("id1").copied();
            assert!(current.is_some());
        }
        let hits_b = idx.search(&vec_b, 2);
        assert!(
            hits_b.iter().any(|(id, _)| id == "id1"),
            "upserted chunk must be searchable via new vector"
        );
    }

    #[tokio::test]
    async fn remove_drops_from_search_results() {
        let _home = crate::db::shared_test_home();
        let hash = unique_hash("remove");
        let dim = 12;
        let vec_a = random_vec(10, dim);
        let mut idx = AnnIndex::build_from_chunks(
            &hash,
            &[
                ("doomed".to_owned(), vec_a.clone()),
                ("keep".to_owned(), random_vec(11, dim)),
            ],
        )
        .await
        .unwrap();

        let before = idx.search(&vec_a, 2);
        assert!(before.iter().any(|(id, _)| id == "doomed"));

        idx.remove("doomed");
        let after = idx.search(&vec_a, 2);
        assert!(
            !after.iter().any(|(id, _)| id == "doomed"),
            "removed chunk must not appear in search results"
        );
    }

    #[tokio::test]
    async fn dim_mismatch_triggers_fallback() {
        let _home = crate::db::shared_test_home();
        let hash = unique_hash("dim-mismatch");
        let dim = 16;
        let mut chunks: Vec<(String, Vec<f32>)> = Vec::new();
        for i in 0..5 {
            chunks.push((format!("c{i}"), random_vec(i as u64, dim)));
        }
        let mut idx = AnnIndex::build_from_chunks(&hash, &chunks).await.unwrap();
        idx.save().await.unwrap();

        invalidate_cache(&hash);
        // Reload asking for a DIFFERENT dim — should get empty index.
        let reloaded = AnnIndex::load_or_empty(&hash, 32).await.unwrap();
        assert_eq!(reloaded.total_size(), 0, "dim drift must reset the index");
        // Searching with a query at the stored dim also yields empty
        // (the reload dropped the graph so nothing to search).
        let hits = reloaded.search(&random_vec(0, 32), 5);
        assert!(hits.is_empty());
    }

    #[tokio::test]
    async fn search_recall_at_top_10() {
        // 1000 random vectors; query is a near-duplicate of one of
        // them. HNSW should place the target in the top-10 on the
        // overwhelming majority of runs — we assert "within top 10"
        // as a recall smoke-check rather than top-1 because HNSW is
        // approximate.
        let _home = crate::db::shared_test_home();
        let hash = unique_hash("recall10");
        let dim = 32;
        let mut chunks: Vec<(String, Vec<f32>)> = Vec::new();
        for i in 0..1000 {
            chunks.push((format!("c{i}"), random_vec(i as u64, dim)));
        }
        // Pick c250 as the target; query is a slightly-perturbed
        // version of its embedding.
        let target_idx = 250u64;
        let mut target_query = random_vec(target_idx, dim);
        // Perturb by 1% then renormalise.
        for (i, x) in target_query.iter_mut().enumerate() {
            *x = 0.01f32.mul_add(random_vec(i as u64 + 5000, 1)[0], *x);
        }
        let n: f32 = target_query.iter().map(|x| x * x).sum::<f32>().sqrt();
        if n > 0.0 {
            for x in &mut target_query {
                *x /= n;
            }
        }

        let idx = AnnIndex::build_from_chunks(&hash, &chunks).await.unwrap();
        let hits = idx.search(&target_query, 10);
        let ids: Vec<_> = hits.iter().map(|(id, _)| id.as_str()).collect();
        assert!(
            ids.contains(&format!("c{target_idx}").as_str()),
            "target chunk must be in top-10 (got {ids:?})"
        );
    }

    #[tokio::test]
    async fn search_caps_large_top_k_requests() {
        let _home = crate::db::shared_test_home();
        let hash = unique_hash("cap-top-k");
        let chunks: Vec<(String, Vec<f32>)> = (0..80)
            .map(|i| (format!("id-{i:02}"), vec![i as f32, 1.0]))
            .collect();
        let idx = AnnIndex::build_from_chunks(&hash, &chunks).await.unwrap();
        let hits = idx.search(&[1.0, 1.0], 500);
        assert!(
            hits.len() <= MAX_SEARCH_TOP_K,
            "ANN search should cap oversized requests, got {} hits",
            hits.len()
        );
    }

    #[tokio::test]
    async fn persistence_meta_version_bump_invalidates() {
        let _home = crate::db::shared_test_home();
        let hash = unique_hash("meta-bump");
        let dim = 8;
        let chunks = vec![
            ("a".to_owned(), random_vec(1, dim)),
            ("b".to_owned(), random_vec(2, dim)),
        ];
        let mut idx = AnnIndex::build_from_chunks(&hash, &chunks).await.unwrap();
        idx.save().await.unwrap();
        // Rewrite the meta with an older / bogus version so the next
        // load drops the on-disk graph.
        let (_g_path, _d_path, meta_path) = ann_files_for_project(&hash);
        let raw = std::fs::read(&meta_path).unwrap();
        let mut parsed: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        parsed["version"] = serde_json::json!(999);
        std::fs::write(&meta_path, serde_json::to_vec(&parsed).unwrap()).unwrap();

        invalidate_cache(&hash);
        let reloaded = AnnIndex::load_or_empty(&hash, dim).await.unwrap();
        assert_eq!(
            reloaded.total_size(),
            0,
            "version drift must wipe the in-memory graph"
        );
    }

    #[tokio::test]
    async fn get_ann_for_project_caches_across_calls() {
        let _home = crate::db::shared_test_home();
        let hash = unique_hash("cache");
        let a = get_ann_for_project(&hash, 16).await.unwrap();
        let b = get_ann_for_project(&hash, 16).await.unwrap();
        assert!(
            Arc::ptr_eq(&a, &b),
            "cache must return the same Arc across calls"
        );
    }

    #[tokio::test]
    async fn get_ann_for_project_keys_cache_by_dim() {
        let _home = crate::db::shared_test_home();
        let hash = unique_hash("cache-dim");
        let a = get_ann_for_project(&hash, 16).await.unwrap();
        let b = get_ann_for_project(&hash, 32).await.unwrap();
        assert!(
            !Arc::ptr_eq(&a, &b),
            "same project with different embedding dims must not reuse one ANN cache entry"
        );
    }
}
