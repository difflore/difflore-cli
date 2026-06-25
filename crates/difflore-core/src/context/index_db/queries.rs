use sqlx::{Row, sqlite::SqlitePool};

use crate::context::embedding::{
    EMBEDDING_DIM, EmbeddedText, active_embedding_profile, embed_text,
    embed_texts_async_with_timeout, local_embedding_profile,
};
use crate::context::rule_source::RuleDocument;
use crate::error::CoreError;

use super::schema::{
    IndexedRuleChunk, QueryFilter, blob_to_embedding, embedding_to_blob, read_meta,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleChunksUpsertOutcome {
    pub count: usize,
    pub embedding_profile: String,
}

pub async fn upsert_rule_chunks(
    pool: &SqlitePool,
    rules: &[RuleDocument],
) -> Result<usize, CoreError> {
    Ok(upsert_rule_chunks_with_profile(pool, rules).await?.count)
}

pub async fn upsert_rule_chunks_with_profile(
    pool: &SqlitePool,
    rules: &[RuleDocument],
) -> Result<RuleChunksUpsertOutcome, CoreError> {
    upsert_rule_chunks_with_profile_and_timeout(pool, rules, None).await
}

pub async fn upsert_rule_chunks_with_profile_and_timeout(
    pool: &SqlitePool,
    rules: &[RuleDocument],
    embedding_timeout: Option<std::time::Duration>,
) -> Result<RuleChunksUpsertOutcome, CoreError> {
    upsert_rule_chunks_with_mode(
        pool,
        rules,
        UpsertEmbeddingMode::Active { embedding_timeout },
    )
    .await
}

pub async fn upsert_rule_chunks_with_local_profile(
    pool: &SqlitePool,
    rules: &[RuleDocument],
) -> Result<RuleChunksUpsertOutcome, CoreError> {
    upsert_rule_chunks_with_mode(pool, rules, UpsertEmbeddingMode::Local).await
}

#[derive(Clone, Copy)]
enum UpsertEmbeddingMode {
    Active {
        embedding_timeout: Option<std::time::Duration>,
    },
    Local,
}

async fn upsert_rule_chunks_with_mode(
    pool: &SqlitePool,
    rules: &[RuleDocument],
    mode: UpsertEmbeddingMode,
) -> Result<RuleChunksUpsertOutcome, CoreError> {
    // Pre-load existing rows once to short-circuit unchanged rules without
    // re-embedding. The hook path runs this on EVERY PreToolUse
    // Read/Edit/Write, so a large corpus would otherwise pay one embedding
    // call + one upsert per rule per fire. All four signature fields
    // (content, file_patterns, language, repo_scope) must match to skip.
    use std::collections::HashMap;
    type Sig = (String, Option<String>, Option<String>, Option<String>);
    type PendingUpsert = (
        String,
        String,
        String,
        Vec<u8>,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    type RowToEmbed = (
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    let existing_rows = sqlx::query!(
        r#"SELECT id as "id!: String", content as "content!: String",
                  file_patterns, language, repo_scope
           FROM rule_chunks"#
    )
    .fetch_all(pool)
    .await?;
    let existing: HashMap<String, Sig> = existing_rows
        .into_iter()
        .map(|r| (r.id, (r.content, r.file_patterns, r.language, r.repo_scope)))
        .collect();
    let fts_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM rule_chunks_fts")
        .fetch_one(pool)
        .await?;
    let rebuild_fts = usize::try_from(fts_count).ok() != Some(existing.len());

    let active_embedding_profile = match mode {
        UpsertEmbeddingMode::Active { .. } => active_embedding_profile().await,
        UpsertEmbeddingMode::Local => local_embedding_profile(),
    };
    let indexed_embedding_profile = read_meta(pool, "embedding_profile").await?;
    let force_reembed =
        indexed_embedding_profile.as_deref() != Some(active_embedding_profile.as_str());

    // Collect (id, embedding) pairs to push into the ANN index AFTER the
    // SQL transaction commits: writing before commit risks a rollback
    // desyncing the graph, and ann.upsert/ann.save aren't transactional, so
    // post-commit is the only safe resync point.
    let mut ann_updates: Vec<(String, Vec<f32>)> = Vec::with_capacity(rules.len());
    let mut rows_to_embed: Vec<RowToEmbed> = Vec::new();

    let mut count = 0;
    for rule in rules {
        let id = format!("rule-{}", rule.skill_id);
        let want: Sig = (
            rule.content.clone(),
            rule.file_patterns.clone(),
            rule.language.clone(),
            rule.repo_scope.clone(),
        );
        if !force_reembed && existing.get(&id) == Some(&want) {
            // Row matches content + all filtered metadata and the active
            // embedding profile matches the stored one, so skip the
            // embedding call and the UPDATE. A profile change (e.g. local
            // SHA1 -> cloud semantic) forces a re-embed even when unchanged.
            count += 1;
            continue;
        }

        rows_to_embed.push((
            id,
            rule.skill_id.clone(),
            rule.content.clone(),
            rule.file_patterns.clone(),
            rule.language.clone(),
            rule.repo_scope.clone(),
        ));
        count += 1;
    }

    let mut actual_embedding_profile = active_embedding_profile.clone();
    let embedded_rows: Vec<EmbeddedText> = match mode {
        UpsertEmbeddingMode::Local => rows_to_embed
            .iter()
            .map(|(_, _, content, _, _, _)| EmbeddedText {
                vector: embed_text(content),
                semantic: false,
            })
            .collect(),
        UpsertEmbeddingMode::Active { embedding_timeout } => {
            let embed_inputs: Vec<String> = rows_to_embed
                .iter()
                .map(|(_, _, content, _, _, _)| content.clone())
                .collect();
            let embed_rule_ids: Vec<String> = rows_to_embed
                .iter()
                .map(|(_, skill_id, _, _, _, _)| skill_id.clone())
                .collect();
            let embedded_rows = embed_texts_async_with_timeout(
                &embed_inputs,
                Some(&embed_rule_ids),
                embedding_timeout,
            )
            .await;
            let active_profile_is_semantic = active_embedding_profile.starts_with("cloud:")
                || active_embedding_profile.starts_with("byok:");
            if active_profile_is_semantic && embedded_rows.iter().any(|row| !row.semantic) {
                actual_embedding_profile = local_embedding_profile();
                rows_to_embed = rules
                    .iter()
                    .map(|rule| {
                        (
                            format!("rule-{}", rule.skill_id),
                            rule.skill_id.clone(),
                            rule.content.clone(),
                            rule.file_patterns.clone(),
                            rule.language.clone(),
                            rule.repo_scope.clone(),
                        )
                    })
                    .collect();
            }

            if actual_embedding_profile == active_embedding_profile {
                embedded_rows
            } else {
                rows_to_embed
                    .iter()
                    .map(|(_, _, content, _, _, _)| EmbeddedText {
                        vector: embed_text(content),
                        semantic: false,
                    })
                    .collect()
            }
        }
    };

    let mut pending_upserts: Vec<PendingUpsert> = Vec::with_capacity(rows_to_embed.len());
    for ((id, skill_id, content, file_patterns, language, repo_scope), embedded) in
        rows_to_embed.into_iter().zip(embedded_rows)
    {
        let emb = embedded.vector;
        let blob = embedding_to_blob(&emb);
        pending_upserts.push((
            id.clone(),
            skill_id,
            content,
            blob,
            file_patterns,
            language,
            repo_scope,
        ));
        ann_updates.push((id, emb));
    }

    let mut tx = pool.begin().await?;
    for (id, skill_id, content, blob, file_patterns, language, repo_scope) in pending_upserts {
        sqlx::query!(
            "INSERT INTO rule_chunks (id, skill_id, content, embedding, file_patterns, language, repo_scope)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET
                content = excluded.content,
                embedding = excluded.embedding,
                file_patterns = excluded.file_patterns,
                language = excluded.language,
                repo_scope = excluded.repo_scope",
            id,
            skill_id,
            content,
            blob,
            file_patterns,
            language,
            repo_scope,
        )
        .execute(&mut *tx)
        .await?;
    }

    // Prune chunks whose skill_id is NOT in the input set, or deleted
    // rules' chunks linger forever and pollute recall. Callers that pass all
    // current skills (the MCP path) get free orphan cleanup; callers passing
    // a partial slice should use a narrower write path instead.
    let valid_ids: Vec<String> = rules.iter().map(|r| r.skill_id.clone()).collect();
    let ids_json = serde_json::to_string(&valid_ids)
        .map_err(|e| CoreError::Internal(format!("serialize valid_ids: {e}")))?;
    sqlx::query!(
        "DELETE FROM rule_chunks WHERE skill_id NOT IN (SELECT value FROM json_each(?1))",
        ids_json
    )
    .execute(&mut *tx)
    .await?;
    if rebuild_fts {
        sqlx::query("DELETE FROM rule_chunks_fts")
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO rule_chunks_fts(chunk_id, content) \
             SELECT id, content FROM rule_chunks",
        )
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;

    // Incrementally update the per-project ANN graph. Errors are logged and
    // swallowed because retrieval has a linear fallback.
    if !ann_updates.is_empty() {
        let dim = ann_updates[0].1.len();
        let project_hash =
            crate::infra::db::project_hash_from_root(&crate::infra::db::current_project_root());
        match crate::context::ann::get_ann_for_project(&project_hash, dim).await {
            Ok(ann_arc) => {
                let mut ann_guard = ann_arc.lock().await;
                for (id, emb) in &ann_updates {
                    ann_guard.upsert(id, emb);
                }
                if ann_guard.needs_compaction() {
                    match compact_ann_from_rule_chunks(pool, &project_hash).await {
                        Ok(compacted) => {
                            *ann_guard = compacted;
                        }
                        Err(e) => {
                            if crate::infra::env::debug_telemetry() {
                                eprintln!("[upsert_rule_chunks] ann compaction failed: {e}");
                            }
                        }
                    }
                }
                if let Err(e) = ann_guard.save().await {
                    if crate::infra::env::debug_telemetry() {
                        eprintln!("[upsert_rule_chunks] ann save failed: {e}");
                    }
                }
            }
            Err(e) => {
                if crate::infra::env::debug_telemetry() {
                    eprintln!("[upsert_rule_chunks] ann cache lookup failed: {e}");
                }
            }
        }
    }

    Ok(RuleChunksUpsertOutcome {
        count,
        embedding_profile: actual_embedding_profile,
    })
}

async fn compact_ann_from_rule_chunks(
    pool: &SqlitePool,
    project_hash: &str,
) -> Result<crate::context::ann::AnnIndex, CoreError> {
    let rows = sqlx::query("SELECT id, embedding FROM rule_chunks ORDER BY id")
        .fetch_all(pool)
        .await?;
    let mut chunks = Vec::with_capacity(rows.len());
    for row in rows {
        let id: String = row.try_get("id")?;
        let embedding: Option<Vec<u8>> = row.try_get("embedding")?;
        let Some(embedding) = embedding else {
            continue;
        };
        let vector = blob_to_embedding(&embedding)?;
        if !vector.is_empty() {
            chunks.push((id, vector));
        }
    }
    crate::context::ann::AnnIndex::build_from_chunks(project_hash, &chunks).await
}

/// Build a self-contained rule index in a fresh `pool` for the ephemeral
/// `difflore try` demo corpus. Differs from [`upsert_rule_chunks`] so the
/// demo is instant, deterministic, and never touches real data:
/// - Embeds with the local SHA1 lexical embedder only — no provider call,
///   no cloud/BYOK dependency, and can't hang on a slow embedder.
/// - Writes NO ANN graph: the ANN write is keyed to the CWD's project hash,
///   so reusing it would scribble demo vectors into the user's real repo.
///   Retrieval over this pool must pass `ann_enabled = false`.
/// - Skips orphan pruning (the pool is fresh).
pub async fn upsert_rule_chunks_isolated(
    pool: &SqlitePool,
    rules: &[RuleDocument],
) -> Result<usize, CoreError> {
    let mut tx = pool.begin().await?;
    for rule in rules {
        let id = format!("rule-{}", rule.skill_id);
        let blob = embedding_to_blob(&embed_text(&rule.content));
        // Non-macro `query` so this isolated path carries no offline-cache
        // dependency; binding shape mirrors the canonical upsert above.
        sqlx::query(
            "INSERT INTO rule_chunks (id, skill_id, content, embedding, file_patterns, language, repo_scope)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET
                content = excluded.content,
                embedding = excluded.embedding,
                file_patterns = excluded.file_patterns,
                language = excluded.language,
                repo_scope = excluded.repo_scope",
        )
        .bind(&id)
        .bind(&rule.skill_id)
        .bind(&rule.content)
        .bind(&blob)
        .bind(&rule.file_patterns)
        .bind(&rule.language)
        .bind(&rule.repo_scope)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query("DELETE FROM rule_chunks_fts")
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO rule_chunks_fts(chunk_id, content) SELECT id, content FROM rule_chunks",
    )
    .execute(&mut *tx)
    .await?;
    // Self-describe as the local lexical profile in the same transaction as
    // the isolated corpus rebuild, so readers never see fresh chunks with a
    // missing/stale embedding profile.
    sqlx::query(
        "INSERT INTO rule_index_meta (key, value)
         VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind("embedding_profile")
    .bind(format!("sha1:local:{EMBEDDING_DIM}"))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok(rules.len())
}

/// Load chunks from the index DB, applying the metadata pre-filter in SQL.
/// `language` and `repo_scope` match on exact equality when set. A NULL
/// repo scope is unattributed and must not be widened into another repo.
pub async fn query_rule_chunks(
    pool: &SqlitePool,
    filter: &QueryFilter,
) -> Result<Vec<IndexedRuleChunk>, CoreError> {
    // The "param IS NULL OR column = param" idiom binds a fixed parameter
    // list regardless of which filters are active. Repo scope is exact (no
    // cross-project global rules), unlike language which also matches NULL.
    let language = filter.language.as_deref();
    let repo_scope = filter.repo_scope.as_deref();
    let rows = sqlx::query!(
        r"SELECT id, skill_id, content, embedding,
                  file_patterns, language, repo_scope
           FROM rule_chunks
           WHERE (?1 IS NULL OR language = ?1 OR language IS NULL)
           AND   (?2 IS NULL OR repo_scope = ?2)",
        language,
        repo_scope,
    )
    .fetch_all(pool)
    .await?;

    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let Some(embedding_blob) = r.embedding else {
            if crate::infra::env::debug_telemetry() {
                eprintln!(
                    "[query_rule_chunks] skipping chunk `{}` with NULL embedding",
                    r.id
                );
            }
            continue;
        };
        out.push(IndexedRuleChunk {
            id: r.id,
            skill_id: r.skill_id,
            content: r.content,
            embedding: blob_to_embedding(&embedding_blob)?,
            file_patterns: r.file_patterns,
            language: r.language,
            repo_scope: r.repo_scope,
        });
    }
    Ok(out)
}

/// Same metadata pre-filter as [`query_rule_chunks`] but skips the `embedding`
/// column entirely. The ANN path ranks against the on-disk HNSW graph and never
/// reads `IndexedRuleChunk::embedding`, so parsing N x dim x 4 bytes of blob
/// into discarded `Vec<f32>` is pure waste on that (default) latency-critical
/// path. Returned chunks carry an empty `embedding`; callers that need vectors
/// (the linear cosine fallback) must use [`query_rule_chunks`] instead.
///
/// Chunks with a NULL embedding are still dropped to keep the active set
/// identical to the full query — the ANN graph only contains embedded chunks.
pub async fn query_rule_chunks_no_embeddings(
    pool: &SqlitePool,
    filter: &QueryFilter,
) -> Result<Vec<IndexedRuleChunk>, CoreError> {
    let language = filter.language.as_deref();
    let repo_scope = filter.repo_scope.as_deref();
    // Runtime (non-macro) query: the `embedding IS NOT NULL` presence probe is
    // only needed here, and avoiding the `sqlx::query!` macro keeps the offline
    // `.sqlx` cache free of a one-off expression column. Behaviour matches the
    // macro path — the same parameter idiom and NULL-embedding skip.
    let rows = sqlx::query(
        r"SELECT id, skill_id, content,
                  file_patterns, language, repo_scope,
                  (embedding IS NOT NULL) AS has_embedding
           FROM rule_chunks
           WHERE (?1 IS NULL OR language = ?1 OR language IS NULL)
           AND   (?2 IS NULL OR repo_scope = ?2)",
    )
    .bind(language)
    .bind(repo_scope)
    .fetch_all(pool)
    .await?;

    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let has_embedding: bool = r.try_get("has_embedding")?;
        let id: String = r.try_get("id")?;
        if !has_embedding {
            if crate::infra::env::debug_telemetry() {
                eprintln!(
                    "[query_rule_chunks_no_embeddings] skipping chunk `{id}` with NULL embedding",
                );
            }
            continue;
        }
        out.push(IndexedRuleChunk {
            id,
            skill_id: r.try_get("skill_id")?,
            content: r.try_get("content")?,
            embedding: Vec::new(),
            file_patterns: r.try_get("file_patterns")?,
            language: r.try_get("language")?,
            repo_scope: r.try_get("repo_scope")?,
        });
    }
    Ok(out)
}

/// FTS5 keyword search returning `(chunk_id, rank)` pairs ordered by BM25
/// rank (smaller = better). Fetches `top_k * 4` raw candidates then
/// post-filters metadata in Rust so filtered-out rows don't eat into the
/// keyword budget. A malformed/empty query yields an empty result rather
/// than an error — FTS is best-effort with an embedding fallback.
pub async fn fts_search(
    pool: &SqlitePool,
    query: &str,
    filter: &QueryFilter,
    top_k: usize,
) -> Result<Vec<(String, f64)>, CoreError> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }

    // Sanitise the query: FTS5 treats `:` / `-` / `"` as operators. Split
    // on any non-alphanumeric boundary (including `_`) so identifiers like
    // `deprecated_xyzzy_handler` break into tokens matching what the
    // porter/unicode61 tokenizer stored at index time.
    let terms: Vec<String> = trimmed
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        // Wrap each term in double quotes so FTS5 treats it as a literal
        // phrase. Without this, tokens like `NOT` / `OR` / `AND` / `NEAR`
        // (FTS5 reserved keywords, case-sensitive) blow up the query
        // with `fts5: syntax error near "NOT"` and the entire keyword
        // path silently returns zero hits — killing retrieval whenever
        // a diff happens to contain those words (very common in code).
        .map(|w| format!("\"{w}\""))
        .collect();
    if terms.is_empty() {
        return Ok(Vec::new());
    }
    let fts_query = terms.join(" OR ");

    // Fetch more than top_k raw because the metadata filter may discard
    // hits; we want to fill the budget whenever possible.
    let raw_limit = (top_k.saturating_mul(4)).max(top_k) as i64;

    let language = filter.language.as_deref();
    let repo_scope = filter.repo_scope.as_deref();
    let rows = match sqlx::query!(
        r#"SELECT f.chunk_id AS "chunk_id!: String", f.rank AS "rank: f64"
           FROM rule_chunks_fts f
           JOIN rule_chunks c ON c.id = f.chunk_id
           WHERE rule_chunks_fts MATCH ?1
           AND (?2 IS NULL OR c.language = ?2 OR c.language IS NULL)
           AND (?3 IS NULL OR c.repo_scope = ?3)
           ORDER BY f.rank
           LIMIT ?4"#,
        fts_query,
        language,
        repo_scope,
        raw_limit,
    )
    .fetch_all(pool)
    .await
    {
        Ok(r) => r,
        Err(e) => {
            // FTS5 can raise syntax errors on unexpected query shapes.
            // We downgrade to "no hits" rather than failing retrieval —
            // the embedding path is always available as fallback.
            if crate::infra::env::debug_telemetry() {
                eprintln!("[fts_search] query failed ({e}); returning empty hit set");
            }
            return Ok(Vec::new());
        }
    };

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        out.push((row.chunk_id, row.rank.unwrap_or(0.0)));
    }
    out.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    out.truncate(top_k);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn compact_ann_from_rule_chunks_rebuilds_from_committed_rows() {
        let _home = crate::infra::db::shared_test_home();
        let tmp = TempDir::new().unwrap();
        let pool = super::super::schema::open_pool_at(&tmp.path().join("idx.db"))
            .await
            .unwrap();
        let alpha = embedding_to_blob(&[1.0, 0.0, 0.0, 0.0]);
        let beta = embedding_to_blob(&[0.0, 1.0, 0.0, 0.0]);
        sqlx::query(
            "INSERT INTO rule_chunks (id, skill_id, content, embedding)
             VALUES (?1, ?2, ?3, ?4), (?5, ?6, ?7, ?8)",
        )
        .bind("rule-alpha")
        .bind("alpha")
        .bind("alpha content")
        .bind(alpha)
        .bind("rule-beta")
        .bind("beta")
        .bind("beta content")
        .bind(beta)
        .execute(&pool)
        .await
        .unwrap();

        let idx = compact_ann_from_rule_chunks(&pool, "compact-test")
            .await
            .unwrap();

        assert_eq!(idx.live_size(), 2);
        assert_eq!(idx.total_size(), 2);
        assert!(!idx.needs_compaction());
        let hits = idx.search(&[1.0, 0.0, 0.0, 0.0], 1);
        assert_eq!(hits[0].0, "rule-alpha");
    }
}
