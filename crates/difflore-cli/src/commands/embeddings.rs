//! Embedding configuration commands — `difflore embeddings status/setup/disable`.
//!
//! * `status`  — show which embedder is active and whether semantic recall is on.
//! * `setup`   — write BYOK (OpenAI-compatible) embedding credentials to settings.
//! * `disable` — revert to fast local keyword matching.

use colored::Colorize;
use difflore_core::context::embedding::{ActiveEmbedderKind, DEFAULT_OPENAI_EMBEDDING_DIM};

use crate::commands::providers::resolve_secret_input;
use crate::support::util::exit_err;
use crate::style;

const DEFAULT_PROVIDER_URL: &str = "https://api.openai.com/v1";
const DEFAULT_MODEL: &str = "text-embedding-3-small";
const DEFAULT_DIM: usize = DEFAULT_OPENAI_EMBEDDING_DIM;

// ── helpers ────────────────────────────────────────────────────────────────

/// Extract the host portion from a URL, for a short provider label.
fn provider_host_from_url(url: &str) -> String {
    let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let host = after_scheme.split('/').next().unwrap_or(after_scheme);
    if host.is_empty() {
        url.to_owned()
    } else {
        host.to_owned()
    }
}

// ── status ─────────────────────────────────────────────────────────────────

pub(crate) async fn handle_status(json: bool) {
    let kind = difflore_core::context::embedding::probe_active_embedder().await;
    let diagnostics = cwd_embedding_diagnostics().await;

    if json {
        let (embedder_tag, semantic, model, dim, provider_host) = match &kind {
            ActiveEmbedderKind::Cloud { model, dim } => {
                ("cloud", true, model.as_str(), *dim, "managed".to_owned())
            }
            ActiveEmbedderKind::Byok {
                provider_host,
                model,
                dim,
            } => ("byok", true, model.as_str(), *dim, provider_host.clone()),
            ActiveEmbedderKind::Sha1 => ("sha1", false, "", 128usize, String::new()),
        };
        let value = serde_json::json!({
            "embedder": embedder_tag,
            "semantic": semantic,
            "model": model,
            "dim": dim,
            "providerHost": provider_host,
            "currentRepoIndex": diagnostics.as_ref().map(|diag| serde_json::json!({
                "activeProfile": &diag.active_profile,
                "indexProfile": &diag.index_profile,
                "profileMatch": diag.profile_match,
                "degraded": diag.degraded,
                "degradedReason": &diag.degraded_reason,
                "vectorLaneAvailable": diag.vector_lane_available,
            })),
        });
        println!("{}", crate::support::util::json_or(&value, "{}"));
        return;
    }

    match &kind {
        ActiveEmbedderKind::Cloud { model, dim } => {
            println!("{} Embeddings", style::ok(style::sym::OK));
            println!();
            println!(
                "  {} {}",
                style::pewter("embedder:"),
                "cloud (DiffLore managed)".bold()
            );
            println!("  {} {model}", style::pewter("model:   "));
            println!("  {} {dim}", style::pewter("dim:     "));
            println!();
            println!(
                "  {} {}",
                style::emerald(style::sym::TIP),
                semantic_status_line(diagnostics.as_ref())
            );
        }
        ActiveEmbedderKind::Byok {
            provider_host,
            model,
            dim,
        } => {
            println!("{} Embeddings", style::ok(style::sym::OK));
            println!();
            println!(
                "  {} {}",
                style::pewter("embedder:"),
                "BYOK (bring-your-own-key)".bold()
            );
            println!("  {} {provider_host}", style::pewter("provider:"));
            println!("  {} {model}", style::pewter("model:   "));
            println!("  {} {dim}", style::pewter("dim:     "));
            println!();
            println!(
                "  {} {}",
                style::emerald(style::sym::TIP),
                semantic_status_line(diagnostics.as_ref())
            );
        }
        ActiveEmbedderKind::Sha1 => {
            println!("{} Embeddings", style::warn(style::sym::WARN));
            println!();
            println!(
                "  {} {}",
                style::pewter("semantic search:"),
                "off | using fast keyword matching".bold()
            );
            println!();
            println!(
                "  {} Recall still works; semantic search is optional.",
                style::amber(style::sym::WARN)
            );
            println!("    To improve match quality, choose one of:");
            println!(
                "      1. {}  (managed, no key required)",
                style::cmd("difflore cloud login")
            );
            println!(
                "      2. {}  (bring your own OpenAI-compatible key)",
                style::cmd("difflore embeddings setup")
            );
        }
    }

    print_cwd_embedding_diagnostics(diagnostics.as_ref());
}

const fn semantic_status_line(
    diag: Option<&difflore_core::context::EmbeddingDiagnostics>,
) -> &'static str {
    match diag {
        Some(diag) if diag.degraded || !diag.vector_lane_available => {
            "Semantic search is configured; current repo index status appears below."
        }
        _ => "Semantic search is active.",
    }
}

async fn cwd_embedding_diagnostics() -> Option<difflore_core::context::EmbeddingDiagnostics> {
    let index_pool = difflore_core::context::index_db::get_pool_for_cwd()
        .await
        .ok()?;
    Some(difflore_core::context::gather_embedding_diagnostics_with_activity(&index_pool).await)
}

fn print_cwd_embedding_diagnostics(diag: Option<&difflore_core::context::EmbeddingDiagnostics>) {
    let Some(diag) = diag else {
        return;
    };
    if diag.degraded {
        let reason = diag
            .degraded_reason
            .as_deref()
            .unwrap_or("embedding_profile_mismatch");
        let state = if diag.vector_lane_available {
            "semantic index needs attention"
        } else {
            "semantic index is paused"
        };
        let display_reason = match reason {
            "dimension_mismatch" | "profile_mismatch" | "embedding_profile_mismatch" => {
                "index needs a rebuild"
            }
            "missing_vectors" | "vector_lane_missing" => "index has not been built",
            _ => "index is out of date",
        };
        println!();
        println!(
            "  {} Current repo index: {state} ({display_reason}).",
            style::amber(style::sym::WARN)
        );
        println!("    Recall still works through file patterns and keyword matching.");
        if matches!(reason, "dimension_mismatch" | "profile_mismatch") {
            // Force-rebuild heals a same-count inconsistency that the
            // freshness-gated `recall --diff` would skip.
            println!(
                "    Rebuild this repo's semantic index: {} (or run {} to refresh lazily)",
                style::cmd("difflore embeddings rebuild"),
                style::cmd("difflore recall --diff")
            );
        } else {
            println!(
                "    Restore semantic embeddings: {} or {}",
                style::cmd("difflore cloud login"),
                style::cmd("difflore embeddings setup")
            );
        }
        println!(
            "    Full detail: {}",
            style::cmd("difflore doctor --report")
        );
    } else if !diag.vector_lane_available {
        println!();
        println!(
            "  {} Current repo index: semantic search is not built yet.",
            style::pewter(style::sym::BULLET)
        );
        println!(
            "    Build it with: {}",
            style::cmd("difflore recall --diff")
        );
    }
}

// ── setup ──────────────────────────────────────────────────────────────────

pub(crate) async fn handle_setup(
    provider_url: Option<String>,
    model: Option<String>,
    dim: Option<usize>,
    key: Option<String>,
    no_key: bool,
) {
    let url = provider_url
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_PROVIDER_URL.to_owned());
    let model = model
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_MODEL.to_owned());
    let dim = dim.unwrap_or(DEFAULT_DIM);

    // `--no-key` supports keyless local providers; otherwise resolve
    // the API key from flag / env var / piped stdin.
    let storage = if no_key {
        None
    } else {
        let raw_key = resolve_secret_input(
            key,
            "DIFFLORE_EMBEDDING_KEY",
            "Embedding provider API key",
            "difflore embeddings setup (use --no-key for a keyless local provider)",
        );
        Some(
            difflore_core::context::embedding::store_embedding_key(&raw_key)
                .unwrap_or_else(|e| exit_err(&format!("failed to encrypt embedding key: {e}"))),
        )
    };

    let mut settings = difflore_core::infra::settings::get()
        .await
        .unwrap_or_else(|e| exit_err(&format!("failed to load settings: {e}")));

    settings.context_engine.semantic_embedding = true;
    settings.context_engine.embedding_provider_url = Some(url.clone());
    settings.context_engine.embedding_provider_key = storage;
    settings.context_engine.embedding_model = Some(model.clone());
    settings.context_engine.embedding_dim = Some(dim);

    difflore_core::infra::settings::update(settings)
        .await
        .unwrap_or_else(|e| exit_err(&format!("failed to save settings: {e}")));

    let host = provider_host_from_url(&url);

    println!(
        "{} Embedding provider configured",
        style::ok(style::sym::OK)
    );
    println!();
    println!("  {} {host}", style::pewter("provider:"));
    println!("  {} {model}", style::pewter("model:   "));
    println!("  {} {dim}", style::pewter("dim:     "));
    println!();
    println!(
        "  {} The next {} or review will re-index using the new provider.",
        style::emerald(style::sym::TIP),
        style::cmd("difflore recall")
    );
    println!(
        "    Check status:   {}",
        style::cmd("difflore embeddings status")
    );
    println!(
        "    Test recall:    {}",
        style::cmd("difflore recall --diff")
    );
}

// ── disable ────────────────────────────────────────────────────────────────

pub(crate) async fn handle_disable() {
    let mut settings = difflore_core::infra::settings::get()
        .await
        .unwrap_or_else(|e| exit_err(&format!("failed to load settings: {e}")));

    settings.context_engine.semantic_embedding = false;

    difflore_core::infra::settings::update(settings)
        .await
        .unwrap_or_else(|e| exit_err(&format!("failed to save settings: {e}")));

    // Report the embedder actually active now: disabling BYOK while
    // logged into cloud falls back to cloud-managed embeddings, not SHA1.
    let kind = difflore_core::context::embedding::probe_active_embedder().await;
    if let ActiveEmbedderKind::Cloud { .. } = kind {
        println!("{} BYOK embeddings disabled", style::ok(style::sym::OK));
        println!();
        println!(
            "  You're logged in to cloud, so recall now uses cloud-managed semantic embeddings."
        );
        println!(
            "  To use only local keyword matching, also run {}.",
            style::cmd("difflore cloud logout")
        );
    } else {
        println!("{} Semantic search turned off", style::ok(style::sym::OK));
        println!();
        println!("  Recall still works with fast local keyword matching.");
        println!("  To re-enable:");
        println!("    Managed:  {}", style::cmd("difflore cloud login"));
        println!("    BYOK:     {}", style::cmd("difflore embeddings setup"));
    }
}

// ── rebuild ──────────────────────────────────────────────────────────────────

/// `difflore embeddings rebuild` — force-rebuild the current repo's
/// per-project semantic index. Re-embeds the in-scope corpus and prunes
/// out-of-scope/orphaned chunks, bypassing the freshness short-circuit
/// that recall/serve use. Safe to run anytime.
pub(crate) async fn handle_rebuild(json: bool) {
    let db = crate::support::util::init_db().await;

    // Detect repo scope like recall / the hook (origin + upstream, with
    // fork->source alias expansion). The per-project index is the scope
    // boundary, so an unscoped checkout has nothing to rebuild.
    let detected =
        difflore_core::infra::git::detect_github_repo_full_names(&crate::support::util::project_path());
    let repo_scopes = difflore_core::skills::expand_repo_scopes_with_source_aliases(&db, &detected)
        .await
        .unwrap_or(detected);

    if repo_scopes.is_empty() {
        if json {
            println!(
                "{}",
                crate::support::util::json_or(
                    &serde_json::json!({ "rebuilt": false, "reason": "no_repo_scope", "chunks": 0 }),
                    "{}",
                )
            );
        } else {
            println!(
                "{} No GitHub origin/upstream remote detected; the index is repo-scoped, so there is nothing to rebuild here.",
                style::warn(style::sym::WARN),
            );
            println!(
                "  Add a remote (or run inside a repo that has one): {}",
                style::cmd("git remote -v"),
            );
        }
        return;
    }

    let index_pool = match difflore_core::context::index_db::get_pool_for_cwd().await {
        Ok(pool) => pool,
        Err(error) => exit_err(&format!("failed to open local index DB: {error}")),
    };

    match difflore_core::context::orchestrator::rebuild_rules_index_for_repo_scopes(
        &db,
        &index_pool,
        &repo_scopes,
        Some(std::time::Duration::from_secs(30)),
    )
    .await
    {
        Ok(chunks) => {
            if json {
                println!(
                    "{}",
                    crate::support::util::json_or(
                        &serde_json::json!({
                            "rebuilt": true,
                            "repoScopes": repo_scopes,
                            "chunks": chunks,
                        }),
                        "{}",
                    )
                );
            } else {
                println!(
                    "{} Rebuilt the local index for {} ({} chunk{}).",
                    style::ok(style::sym::OK),
                    repo_scopes.join(", "),
                    chunks,
                    if chunks == 1 { "" } else { "s" },
                );
                println!("  The next recall or agent run uses the fresh index.");
            }
        }
        Err(error) => exit_err(&format!("index rebuild failed: {error}")),
    }
}

// ── tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_dim_matches_core_constant() {
        // Keep the local DEFAULT_DIM alias in sync with the core constant.
        assert_eq!(DEFAULT_DIM, DEFAULT_OPENAI_EMBEDDING_DIM);
    }

    #[test]
    fn provider_host_from_url_strips_scheme_and_path() {
        assert_eq!(
            provider_host_from_url("https://api.openai.com/v1"),
            "api.openai.com"
        );
        assert_eq!(
            provider_host_from_url("https://api.openai.com/v1/embeddings"),
            "api.openai.com"
        );
        assert_eq!(
            provider_host_from_url("http://localhost:8080/v1"),
            "localhost:8080"
        );
        assert_eq!(
            provider_host_from_url("https://together.xyz"),
            "together.xyz"
        );
    }

    #[test]
    fn provider_host_handles_no_scheme() {
        // URL without scheme — treat everything before first `/` as host.
        assert_eq!(
            provider_host_from_url("api.example.com/v1"),
            "api.example.com"
        );
    }

    #[test]
    fn default_provider_url_parses_to_known_host() {
        assert_eq!(
            provider_host_from_url(DEFAULT_PROVIDER_URL),
            "api.openai.com"
        );
    }
}
