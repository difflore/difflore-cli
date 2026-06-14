use sqlx::sqlite::SqlitePool;

use crate::context::embedding::{
    EMBEDDING_DIM, active_embedding_profile, embed_text, embed_texts_async_with_timeout,
};
use crate::context::rule_source::RuleDocument;
use crate::error::CoreError;

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

    let active_embedding_profile = active_embedding_profile().await;
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
    tx.commit().await?;

    // Self-describe as the local lexical profile so the retrieval path
    // embeds the query with SHA1 too (matching dimensions), restoring the
    // hash + FTS hybrid lane.
    write_meta(
        pool,
        "embedding_profile",
        &format!("sha1:local:{EMBEDDING_DIM}"),
    )
    .await?;

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
