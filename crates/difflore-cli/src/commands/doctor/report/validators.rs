use super::formatters::lang_short_label;

/// Self-recall@K: query each sampled rule by the first 8 significant words
/// of its own description and check whether it returns in top-K. Since the
/// query distils the rule's own text this is an optimistic upper bound, not
/// real-world recall — a local sanity check that index/embedder/rerank are
/// wired up, not a benchmark. Real paraphrase recall needs separate
/// task-query evaluation.
pub(crate) async fn self_recall_section(pool: &difflore_core::SqlitePool, s: &mut String) {
    use difflore_core::context::{index_db, retrieval, rule_source};

    sw!(s, "\n## Self-recall sanity check\n");

    let rules_db = match difflore_core::skills::list_review_standards(pool).await {
        Ok(r) => r,
        Err(e) => {
            sw!(s, "- ✗ failed to load rules: {e}");
            return;
        }
    };
    if rules_db.len() < 5 {
        sw!(
            s,
            "- (skip — only {} rule(s); need ≥5 to measure)",
            rules_db.len()
        );
        return;
    }

    let rule_docs = match rule_source::load_rules_from_db(pool).await {
        Ok(r) => r,
        Err(e) => {
            sw!(s, "- ✗ failed to load rule documents: {e}");
            return;
        }
    };

    // Build the measurement index in a throwaway temp dir with the local
    // SHA1 embedder: deterministic, offline, never writes into the real
    // repo's per-project index, and can't stall on cloud-embed timeouts.
    // Same setup `difflore eval` uses, so the two report the same number.
    let tmp = match tempfile::tempdir() {
        Ok(t) => t,
        Err(e) => {
            sw!(s, "- (skip — temp index unavailable: {e})");
            return;
        }
    };
    let index_pool = match index_db::open_index_pool_at(&tmp.path().join("self-recall.db")).await {
        Ok(p) => p,
        Err(e) => {
            sw!(s, "- (skip — context index unavailable: {e})");
            return;
        }
    };
    if let Err(e) = index_db::upsert_rule_chunks_isolated(&index_pool, &rule_docs).await {
        sw!(s, "- ✗ failed to build measurement index: {e}");
        return;
    }

    // Deterministic stride sampling: same N across runs when the corpus is
    // unchanged, evenly spread over `installed_at DESC` order.
    const N_TARGET: usize = 20;
    const SELF_RECALL_EMBEDDING_TIMEOUT: std::time::Duration =
        std::time::Duration::from_millis(2500);
    let step = rules_db.len().div_ceil(N_TARGET).max(1);
    let samples: Vec<_> = rules_db.iter().step_by(step).take(N_TARGET).collect();

    const STOP: &[&str] = &[
        "the", "a", "an", "and", "or", "of", "to", "for", "in", "on", "at", "by", "with", "when",
        "use", "using", "as", "is", "are", "be", "this", "that", "from", "into", "do", "not",
        "should", "must", "via", "than", "then", "but", "if", "else",
    ];
    let truncate_query = |desc: &str| -> String {
        let mut out: Vec<&str> = Vec::new();
        for w in desc.split_whitespace() {
            let trimmed = w.trim_matches(|c: char| !c.is_alphanumeric());
            if trimmed.is_empty() {
                continue;
            }
            if STOP.contains(&trimmed.to_ascii_lowercase().as_str()) {
                continue;
            }
            out.push(w);
            if out.len() >= 8 {
                break;
            }
        }
        out.join(" ")
    };

    let lang_by_id: std::collections::HashMap<&str, Option<&str>> = rule_docs
        .iter()
        .map(|d| (d.skill_id.as_str(), d.language.as_deref()))
        .collect();

    let mut tested = 0usize;
    let mut hits_at_1 = 0usize;
    let mut hits_at_5 = 0usize;
    // Sum of 1/rank (1-based) over hits; divided by `tested` below for MRR.
    let mut reciprocal_rank_sum = 0.0f64;
    // Per-language (samples, @1 hits, @5 hits, reciprocal-rank sum).
    let mut per_lang: std::collections::BTreeMap<String, (usize, usize, usize, f64)> =
        std::collections::BTreeMap::new();
    for rule in &samples {
        let query = truncate_query(rule.description.trim());
        if query.is_empty() {
            continue;
        }
        tested += 1;
        let lang_key = lang_by_id
            .get(rule.id.as_str())
            .copied()
            .flatten()
            .map_or_else(|| "(unknown)".to_owned(), str::to_owned);
        let entry = per_lang.entry(lang_key).or_insert((0, 0, 0, 0.0));
        entry.0 += 1;
        // Measure the same reranked path that recall/fix/MCP/hook use
        // (`retrieve_rules_for_search`), not raw
        // `retrieve_rules_with_confidence`: the raw path skips the lexical
        // re-rank and badly understates @1 (≈35% raw vs ≈85% reranked).
        let Ok(scored) = retrieval::retrieve_rules_for_search(
            &index_pool,
            retrieval::RuleSearchRetrievalOptions {
                query: &query,
                lexical_query: &query,
                top_k: 5,
                confidence_map: None,
                age_days_map: None,
                target_scope: None,
                repo_scopes: &[],
                ann_enabled: false,
                embedding_timeout: Some(SELF_RECALL_EMBEDDING_TIMEOUT),
                cold_start_retry: false,
                adaptive_prune: false,
            },
        )
        .await
        else {
            continue;
        };
        if let Some(pos) = scored.iter().position(|sc| sc.skill_id == rule.id) {
            hits_at_5 += 1;
            entry.2 += 1;
            let reciprocal_rank = 1.0 / (pos as f64 + 1.0);
            reciprocal_rank_sum += reciprocal_rank;
            entry.3 += reciprocal_rank;
            if pos == 0 {
                hits_at_1 += 1;
                entry.1 += 1;
            }
        }
    }

    if tested == 0 {
        sw!(
            s,
            "- (skip — sampled rules all had empty descriptions; no signal possible)"
        );
        return;
    }

    let pct5 = (hits_at_5 as f64 / tested as f64) * 100.0;
    let pct1 = (hits_at_1 as f64 / tested as f64) * 100.0;
    let mark5 = if pct5 >= 80.0 {
        "✓"
    } else if pct5 >= 50.0 {
        "⚠"
    } else {
        "✗"
    };
    let mark1 = if pct1 >= 50.0 {
        "✓"
    } else if pct1 >= 25.0 {
        "⚠"
    } else {
        "✗"
    };
    sw!(
        s,
        "- {mark5} self-recall@5: {hits_at_5}/{tested} ({pct5:.1}%)"
    );
    sw!(
        s,
        "- {mark1} self-recall@1: {hits_at_1}/{tested} ({pct1:.1}%)"
    );
    let mrr = self_recall_mrr(reciprocal_rank_sum, tested);
    let markm = self_recall_mrr_mark(mrr);
    sw!(s, "- {markm} self-recall MRR: {mrr:.3}");
    sw!(
        s,
        "  (measured through the reranked search path — the one recall/fix/MCP/hook use)"
    );
    sw!(
        s,
        "  (query = first 8 significant words of each rule's OWN description → an optimistic UPPER BOUND, NOT real-world recall)"
    );
    sw!(
        s,
        "  (real-world paraphrase recall needs separate task-query evaluation)"
    );
    sw!(
        s,
        "  (interpretation: recall sanity only; inspect search/recall `why:` + `source:` lines for precision)"
    );

    // Skip the per-language breakdown for single-language repos.
    let known_langs_count = per_lang
        .iter()
        .filter(|(k, _)| k.as_str() != "(unknown)")
        .count();
    if tested >= 6 && known_langs_count >= 2 {
        let mut entries: Vec<(String, (usize, usize, usize, f64))> =
            per_lang.iter().map(|(k, v)| (k.clone(), *v)).collect();
        entries.sort_by(|a, b| b.1.0.cmp(&a.1.0).then_with(|| a.0.cmp(&b.0)));
        let (top, rest) = if entries.len() > 4 {
            entries.split_at(4)
        } else {
            (entries.as_slice(), &[][..])
        };
        sw!(s, "  by language:");
        for (lang, (n, h1, h5, rr)) in top {
            let label = lang_short_label(lang);
            let lang_mrr = self_recall_mrr(*rr, *n);
            sw!(
                s,
                "  - {label}: @1 {h1}/{n} · @5 {h5}/{n} · MRR {lang_mrr:.2}"
            );
        }
        if !rest.is_empty() {
            let (n, h1, h5, rr) = rest.iter().fold((0, 0, 0, 0.0), |acc, (_, t)| {
                (acc.0 + t.0, acc.1 + t.1, acc.2 + t.2, acc.3 + t.3)
            });
            let other_mrr = self_recall_mrr(rr, n);
            sw!(
                s,
                "  - other: @1 {h1}/{n} · @5 {h5}/{n} · MRR {other_mrr:.2}"
            );
        }
    }
    if pct5 < 80.0 {
        sw!(
            s,
            "  ⚠ low self-recall@5 — retrieval (not corpus) is likely the bottleneck. \
             Current mode may be lexical-only or missing real embeddings; \
             `difflore cloud login` or `difflore embeddings setup` can enable semantic embeddings. \
             Re-run this section to verify the lift on your own corpus."
        );
    } else if pct1 < 50.0 {
        sw!(
            s,
            "  ⚠ self-recall@1 lags @5 — the right rule is in the top-5 but ranks below near-misses. \
             Semantic embeddings usually improve ranking quality, but the measured number above is the source of truth. \
             `difflore cloud login` to enable, or `difflore embeddings setup` for BYOK."
        );
    }
}

/// Mean reciprocal rank over the `tested` samples. Dividing by `tested`
/// (not the hit count) lets misses drag MRR down. Returns 0.0 when nothing
/// was tested.
fn self_recall_mrr(reciprocal_rank_sum: f64, tested: usize) -> f64 {
    if tested == 0 {
        0.0
    } else {
        reciprocal_rank_sum / tested as f64
    }
}

/// Health mark for the MRR line. `≥ 0.7` is the healthy ranking-quality
/// target (the right rule usually at rank 1–2); `0.5–0.7` is marginal.
fn self_recall_mrr_mark(mrr: f64) -> &'static str {
    if mrr >= 0.7 {
        "✓"
    } else if mrr >= 0.5 {
        "⚠"
    } else {
        "✗"
    }
}

// Counts rules with empty file_patterns, a recall-killing signature.
pub(super) async fn corpus_health_subsection(pool: &difflore_core::SqlitePool, s: &mut String) {
    sw!(s, "\n## Local memory\n");
    match difflore_core::infra::db::corpus_health(pool).await {
        Ok(h) => {
            sw!(s, "- total rules: {}", h.total);
            if !h.by_origin.is_empty() {
                sw!(s, "- by origin:");
                for (origin, n) in &h.by_origin {
                    sw!(s, "  - {origin}: {n}");
                }
            }
            if !h.by_source_repo.is_empty() {
                sw!(s, "- top source_repo (10):");
                for (repo, n) in &h.by_source_repo {
                    sw!(s, "  - {repo}: {n}");
                }
            }
            let mark = if h.empty_file_patterns == 0 {
                "✓"
            } else {
                "⚠"
            };
            sw!(
                s,
                "- rules with empty file_patterns: {mark} {}",
                h.empty_file_patterns,
            );
        }
        Err(e) => {
            sw!(s, "- local memory probe failed: {e}");
        }
    }
}

// The SHA1 fallback embedder produces sims in 0.005–0.02 (looks broken
// even when ranking is correct), so surface the active mode to explain
// weak numbers.
pub(super) async fn embedder_status_subsection(s: &mut String) {
    embedder_status_subsection_for(
        s,
        &difflore_core::context::embedding::probe_active_embedder().await,
    );
}

fn embedder_status_subsection_for(
    s: &mut String,
    embedder: &difflore_core::context::embedding::ActiveEmbedderKind,
) {
    use difflore_core::context::embedding::ActiveEmbedderKind;

    match embedder {
        ActiveEmbedderKind::Cloud { model, dim } => {
            sw!(
                s,
                "- embedder: ✓ cloud-managed configured ({model}, {dim} dims)"
            );
            sw!(
                s,
                "  (configured mode follows DiffLore's cloud-first embedding priority. \
                 Startup health, the Embedding section, and Memory pipeline events are the \
                 source of truth for current cloud reachability, caps, and fallback.)"
            );
        }
        ActiveEmbedderKind::Byok {
            provider_host,
            model,
            dim,
        } => {
            sw!(
                s,
                "- embedder: ✓ BYOK configured ({provider_host}, {model}, {dim} dims; key redacted)"
            );
            sw!(
                s,
                "  (configured mode follows DiffLore's embedding priority. \
                 The Embedding section and Memory pipeline events show recent provider \
                 failures or local fallback.)"
            );
        }
        ActiveEmbedderKind::Sha1 => {
            sw!(s, "- embedder: · local-lexical");
            sw!(
                s,
                "  (offline hybrid: local hash + FTS5 BM25. This is deterministic and local, \
                 but less semantic than cloud-managed or BYOK embeddings. \
                 Use the Self-recall section above to measure this corpus; \
                 `difflore cloud login` or `difflore embeddings setup` can enable semantic embeddings.)"
            );
        }
    }
}

// The active embedder is what new embeds would use; the index DB records
// the profile rules were actually embedded under. A mismatch silently
// disables the vector lane and forces FTS-only retrieval, so surface both
// to attribute weak recall to a stale index rather than the corpus.
pub(super) async fn embedding_profile_match_subsection(s: &mut String) {
    use difflore_core::context::{gather_embedding_diagnostics_with_activity, index_db};

    let index_pool = match index_db::get_pool_for_cwd().await {
        Ok(p) => p,
        Err(e) => {
            sw!(
                s,
                "- index embedding profile: (skip — context index unavailable: {e})"
            );
            return;
        }
    };
    let diag = gather_embedding_diagnostics_with_activity(&index_pool).await;
    embedding_profile_match_subsection_for(s, &diag);
}

fn embedding_profile_match_subsection_for(
    s: &mut String,
    diag: &difflore_core::context::EmbeddingDiagnostics,
) {
    let index_profile = diag
        .index_profile
        .as_deref()
        .unwrap_or("(none — no rules embedded yet)");
    let match_mark = if diag.profile_match { "✓" } else { "⚠" };
    sw!(s, "- active embedding profile: `{}`", diag.active_profile);
    sw!(s, "- index embedding profile: `{index_profile}`");
    sw!(
        s,
        "- profile match: {match_mark} {}",
        if diag.profile_match {
            "yes (index embedded under the active profile)"
        } else {
            "no (index was embedded under a different profile)"
        }
    );
    if diag.degraded {
        let reason = diag
            .degraded_reason
            .as_deref()
            .unwrap_or("embedding profile mismatch");
        if diag.vector_lane_available && reason == "profile_mismatch" {
            sw!(
                s,
                "- ⚠ WARNING: vector lane degraded ({reason}) — semantic search can run, but active/index profiles differ so recall may be weaker"
            );
        } else {
            let detail = match reason {
                "provider_fallback" => "semantic provider fell back to local lexical embeddings",
                "dimension_mismatch" => "active and indexed vector dimensions are not comparable",
                _ => "retrieval is FTS-only (BM25 lexical, no semantic vectors)",
            };
            sw!(
                s,
                "- ✗ ERROR: vector lane unavailable ({reason}) — {detail}"
            );
        }
        sw!(
            s,
            "  → re-embed under the active profile to restore the vector lane: run `difflore embeddings rebuild` (force-rebuild, recovers a same-count inconsistency) or `difflore recall --diff` / open an editor with a memory-wired agent (lazy, freshness-gated re-index); confirm the active profile first via the Embedding section above"
        );
    } else if !diag.vector_lane_available {
        sw!(
            s,
            "- · vector lane unavailable — retrieval is FTS-only (no semantic vectors for this profile)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{
        embedder_status_subsection_for, embedding_profile_match_subsection_for, self_recall_mrr,
        self_recall_mrr_mark,
    };
    use difflore_core::context::EmbeddingDiagnostics;
    use difflore_core::context::embedding::ActiveEmbedderKind;

    #[test]
    fn self_recall_mrr_handles_empty_and_known_sums() {
        assert!(self_recall_mrr(0.0, 0).abs() < 1e-9);
        assert!((self_recall_mrr(3.0, 3) - 1.0).abs() < 1e-9);
        // Ranks 1, 2, and a miss over 3 samples → (1 + 0.5 + 0) / 3 = 0.5.
        assert!((self_recall_mrr(1.5, 3) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn self_recall_mrr_mark_thresholds_match_target() {
        assert_eq!(self_recall_mrr_mark(1.0), "✓");
        assert_eq!(self_recall_mrr_mark(0.7), "✓");
        assert_eq!(self_recall_mrr_mark(0.69), "⚠");
        assert_eq!(self_recall_mrr_mark(0.5), "⚠");
        assert_eq!(self_recall_mrr_mark(0.49), "✗");
        assert_eq!(self_recall_mrr_mark(0.0), "✗");
    }

    #[test]
    fn embedder_status_reports_cloud_configured_without_claiming_reachability() {
        let mut out = String::new();
        embedder_status_subsection_for(
            &mut out,
            &ActiveEmbedderKind::Cloud {
                model: "text-embedding-3-small".to_owned(),
                dim: 1536,
            },
        );

        assert!(out.contains("cloud-managed configured"), "{out}");
        assert!(
            out.contains("source of truth for current cloud reachability"),
            "{out}"
        );
    }

    #[test]
    fn embedder_status_reports_byok_host_with_redacted_key() {
        let mut out = String::new();
        embedder_status_subsection_for(
            &mut out,
            &ActiveEmbedderKind::Byok {
                provider_host: "embed.example.com".to_owned(),
                model: "custom-embed".to_owned(),
                dim: 768,
            },
        );

        assert!(out.contains("BYOK configured"), "{out}");
        assert!(out.contains("embed.example.com"), "{out}");
        assert!(out.contains("key redacted"), "{out}");
    }

    #[test]
    fn embedder_status_reports_local_lexical_fallback_copy() {
        let mut out = String::new();
        embedder_status_subsection_for(&mut out, &ActiveEmbedderKind::Sha1);

        assert!(out.contains("local-lexical"), "{out}");
        assert!(out.contains("difflore cloud login"), "{out}");
    }

    #[test]
    fn embedding_profile_match_reports_aligned_index_without_warning() {
        let mut out = String::new();
        embedding_profile_match_subsection_for(
            &mut out,
            &EmbeddingDiagnostics {
                active_profile: "cloud:text-embedding-3-small".to_owned(),
                index_profile: Some("cloud:text-embedding-3-small".to_owned()),
                profile_match: true,
                degraded: false,
                degraded_reason: None,
                vector_lane_available: true,
            },
        );

        assert!(
            out.contains("active embedding profile: `cloud:text-embedding-3-small`"),
            "{out}"
        );
        assert!(out.contains("profile match: ✓ yes"), "{out}");
        assert!(!out.contains("WARNING"), "{out}");
    }

    #[test]
    fn embedding_profile_match_errors_when_degraded_to_fts_only() {
        let mut out = String::new();
        embedding_profile_match_subsection_for(
            &mut out,
            &EmbeddingDiagnostics {
                active_profile: "cloud:text-embedding-3-small".to_owned(),
                index_profile: Some("local:sha1".to_owned()),
                profile_match: false,
                degraded: true,
                degraded_reason: Some("dimension_mismatch".to_owned()),
                vector_lane_available: false,
            },
        );

        assert!(out.contains("profile match: ⚠ no"), "{out}");
        assert!(out.contains("ERROR: vector lane unavailable"), "{out}");
        assert!(out.contains("dimension_mismatch"), "{out}");
        assert!(
            out.contains("active and indexed vector dimensions are not comparable"),
            "{out}"
        );
    }

    #[test]
    fn embedding_profile_match_warns_when_semantic_lane_is_degraded_but_available() {
        let mut out = String::new();
        embedding_profile_match_subsection_for(
            &mut out,
            &EmbeddingDiagnostics {
                active_profile: "cloud:text-embedding-3-small:1536".to_owned(),
                index_profile: Some("cloud:older-model:1536".to_owned()),
                profile_match: false,
                degraded: true,
                degraded_reason: Some("profile_mismatch".to_owned()),
                vector_lane_available: true,
            },
        );

        assert!(out.contains("WARNING: vector lane degraded"), "{out}");
        assert!(out.contains("profile_mismatch"), "{out}");
        assert!(out.contains("semantic search can run"), "{out}");
        assert!(!out.contains("FTS-only"), "{out}");
    }

    #[test]
    fn embedding_profile_match_errors_on_provider_fallback() {
        let mut out = String::new();
        embedding_profile_match_subsection_for(
            &mut out,
            &EmbeddingDiagnostics {
                active_profile: "sha1:local:128".to_owned(),
                index_profile: Some("cloud:text-embedding-3-small:1536".to_owned()),
                profile_match: false,
                degraded: true,
                degraded_reason: Some("provider_fallback".to_owned()),
                vector_lane_available: false,
            },
        );

        assert!(out.contains("ERROR: vector lane unavailable"), "{out}");
        assert!(out.contains("provider_fallback"), "{out}");
        assert!(
            out.contains("semantic provider fell back to local lexical embeddings"),
            "{out}"
        );
    }
}
