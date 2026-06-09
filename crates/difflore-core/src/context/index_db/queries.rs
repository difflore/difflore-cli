use sqlx::sqlite::SqlitePool;

use crate::context::embedding::{
    EMBEDDING_DIM, active_embedding_profile, embed_text, embed_texts_async_with_timeout,
};
use crate::context::rule_source::RuleDocument;
use crate::errors::CoreError;

use super::schema::{
    IndexedRuleChunk, QueryFilter, blob_to_embedding, embedding_to_blob, read_meta, write_meta,
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
    // 2026-04-27 perf: pre-load existing rows once so we can short-
    // circuit unchanged rules without re-embedding or hitting the SQL
    // hot path. The hook path runs upsert_rule_chunks on EVERY
    // PreToolUse:Read/Edit/Write — a 3,921-rule cloud corpus would
    // otherwise pay 3,921 embedding calls + 3,921 INSERT OR UPDATE
    // statements per fire. Existing-row signature
    // (content, file_patterns, language, repo_scope) — all four must
    // match to skip; a metadata change still forces a row update so
    // the SQL filter sees the new value.
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

    let active_embedding_profile = active_embedding_profile().await;
    let indexed_embedding_profile = read_meta(pool, "embedding_profile").await?;
    let force_reembed =
        indexed_embedding_profile.as_deref() != Some(active_embedding_profile.as_str());

    // Collect (id, embedding) pairs so we can push them into the ANN
    // index after the SQL transaction commits. We cannot write the ANN
    // BEFORE the commit because a DB rollback would leave the graph
    // out of sync; we also cannot write INSIDE the transaction because
    // ann.upsert + ann.save are not transactional. Post-commit is the
    // natural resync point.
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
            // No-op: row matches in content + all filtered metadata,
            // and the active embedding profile matches the stored index
            // profile. Skip both — saves an embedding call and a SQL
            // UPDATE. If the profile changed (e.g. local SHA1 -> cloud
            // semantic), force a re-embed even when content is unchanged.
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

    let embed_inputs: Vec<String> = rows_to_embed
        .iter()
        .map(|(_, _, content, _, _, _)| content.clone())
        .collect();
    let embed_rule_ids: Vec<String> = rows_to_embed
        .iter()
        .map(|(_, skill_id, _, _, _, _)| skill_id.clone())
        .collect();
    let embedded_rows =
        embed_texts_async_with_timeout(&embed_inputs, Some(&embed_rule_ids), embedding_timeout)
            .await;
    let active_profile_is_semantic = active_embedding_profile.starts_with("cloud:")
        || active_embedding_profile.starts_with("byok:");
    let mut actual_embedding_profile = active_embedding_profile.clone();
    if active_profile_is_semantic && embedded_rows.iter().any(|row| !row.semantic) {
        actual_embedding_profile = format!("sha1:local:{EMBEDDING_DIM}");
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

    let embedded_rows = if actual_embedding_profile == active_embedding_profile {
        embedded_rows
    } else {
        rows_to_embed
            .iter()
            .map(
                |(_, _, content, _, _, _)| crate::context::embedding::EmbeddedText {
                    vector: embed_text(content),
                    semantic: false,
                },
            )
            .collect()
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

    // 2026-04-25: prune chunks whose skill_id is NOT in the input set.
    // Without this, deleted rules' chunks linger forever and pollute
    // recall — a process-advice rule we removed from rules_cloud was
    // still surfacing here because the chunk row stayed indefinitely.
    // Callers that pass all current skills (the MCP path does) get
    // free orphan cleanup; callers that pass a partial slice should use
    // a narrower write path instead of calling this function.
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
    // swallowed because retrieval has a linear fallback. Empty upserts stay free.
    if !ann_updates.is_empty() {
        let dim = ann_updates[0].1.len();
        let project_hash = crate::db::project_hash_from_root(&crate::db::current_project_root());
        match crate::context::ann::get_ann_for_project(&project_hash, dim).await {
            Ok(ann_arc) => {
                let mut ann_guard = ann_arc.lock().await;
                for (id, emb) in &ann_updates {
                    ann_guard.upsert(id, emb);
                }
                if let Err(e) = ann_guard.save().await {
                    eprintln!("[upsert_rule_chunks] ann save failed: {e}");
                }
            }
            Err(e) => {
                eprintln!("[upsert_rule_chunks] ann cache lookup failed: {e}");
            }
        }
    }

    Ok(RuleChunksUpsertOutcome {
        count,
        embedding_profile: actual_embedding_profile,
    })
}

/// Build a fully self-contained rule index in `pool` for an ephemeral,
/// throwaway corpus (the `difflore try` demo). Assumes a fresh pool.
///
/// Differs from [`upsert_rule_chunks`] in three deliberate ways, all so the
/// demo is instant, deterministic, and never touches the user's real data:
/// - Embeds with the local SHA1 lexical embedder only — no provider call and
///   no dependency on the user's cloud/BYOK config. This is exactly the
///   zero-setup recall path a brand-new user gets, so what the demo shows is
///   honest, and it can't hang on a slow or misconfigured cloud embedder.
/// - Writes NO ANN graph. The ANN write in `upsert_rule_chunks` is keyed to
///   the process CWD's project hash, so reusing it would scribble demo vectors
///   into whatever real repo the user happens to be standing in
///   (`~/.difflore/projects/{hash}/hnsw.*`). Retrieval over this pool must pass
///   `ann_enabled = false`; the linear scan is trivial for a small demo corpus.
/// - Skips orphan pruning (the pool is fresh, so there is nothing to prune).
pub async fn upsert_rule_chunks_isolated(
    pool: &SqlitePool,
    rules: &[RuleDocument],
) -> Result<usize, CoreError> {
    let mut tx = pool.begin().await?;
    for rule in rules {
        let id = format!("rule-{}", rule.skill_id);
        let blob = embedding_to_blob(&embed_text(&rule.content));
        // Non-macro `query` (not `query!`) so this isolated path carries no
        // offline-cache dependency; the binding shape mirrors the canonical
        // upsert above.
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
    tx.commit().await?;

    // Self-describe the index as the local lexical profile so the retrieval
    // path's `embed_query_aligned_to_index` embeds the query with SHA1 too
    // (matching dimensions), restoring the hash + FTS hybrid lane.
    write_meta(
        pool,
        "embedding_profile",
        &format!("sha1:local:{EMBEDDING_DIM}"),
    )
    .await?;

    Ok(rules.len())
}

/// Load chunks from the index DB applying the metadata pre-filter at the
/// SQL layer. `filter.language` matches exact equality when set;
/// `filter.repo_scope` matches exact equality when set. NULL repo scope is
/// unattributed metadata and must not be widened into another repo at runtime.
///
/// When `filter.is_empty()` this returns the current per-project index
/// without widening through retired repo metadata.
pub async fn query_rule_chunks(
    pool: &SqlitePool,
    filter: &QueryFilter,
) -> Result<Vec<IndexedRuleChunk>, CoreError> {
    // The WHERE clause uses the "param IS NULL OR column = param" idiom
    // so we can bind a fixed parameter list regardless of which filters
    // are active. Unlike language, repo scope is intentionally exact:
    // no cross-project/runtime global rules.
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
        out.push(IndexedRuleChunk {
            id: r.id,
            skill_id: r.skill_id,
            content: r.content,
            embedding: blob_to_embedding(&r.embedding.unwrap_or_default())?,
            file_patterns: r.file_patterns,
            language: r.language,
            repo_scope: r.repo_scope,
        });
    }
    Ok(out)
}

/// FTS5 keyword search. Returns `(chunk_id, rank)` pairs ordered by BM25
/// rank (smaller = better in `SQLite` FTS5). `top_k` caps the result size
/// BEFORE the metadata filter is applied — we fetch `top_k * 4` raw
/// candidates from FTS then post-filter in Rust so rules that fail the
/// metadata check don't eat into the keyword budget.
///
/// A malformed / empty query string yields an empty result rather than
/// an error: the retrieval path treats FTS as best-effort (an embedding
/// fallback always exists).
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
    // `deprecated_xyzzy_handler` break into discoverable tokens that match
    // what the porter/unicode61 tokenizer stored at index time. Matches how
    // most hybrid-search tutorials preprocess BM25 input.
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
            eprintln!("[fts_search] query failed ({e}); returning empty hit set");
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
