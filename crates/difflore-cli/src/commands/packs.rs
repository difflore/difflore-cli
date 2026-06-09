//! `difflore packs ...` — the shareable starter rule-pack marketplace surface
//! over `difflore_core::packs`: fetch the registry catalog / a pack manifest,
//! verify its `sha256`, and install suggestion-only starter rules.
//!
//! All commands honor `--json` (house convention) and a `--registry <URL>`
//! override (default: `difflore_core::packs::DEFAULT_PACK_REGISTRY`; supports a
//! `file://` path for tests / air-gapped install). Network is required only for
//! `list` / `show` / `install` / `publish`; `installed` / `uninstall` are local.

use colored::Colorize;
use serde_json::json;

use difflore_core::packs::{
    self, DEFAULT_PACK_REGISTRY, PackFetchError, PackIndex, PackManifest, fetch_index,
    fetch_manifest, install_pack,
};

use crate::commands::util::{exit_err, json_compact_or};
use crate::runtime::{CommandContext, OutputMode};
use crate::style;

/// Resolve the effective registry base: the `--registry` override or the
/// first-party default.
fn registry_base(registry: Option<String>) -> String {
    registry
        .map(|r| r.trim().to_owned())
        .filter(|r| !r.is_empty())
        .unwrap_or_else(|| DEFAULT_PACK_REGISTRY.to_owned())
}

/// Whether a custom (non-default) registry is in use. When so, a
/// `maintainer.verified` badge is rendered as `verified (custom registry)` so
/// the trust signal is never misleading. Delegates to the core's canonical
/// `is_default_registry` so the default-detection rule lives in one place.
fn is_custom_registry(base: &str) -> bool {
    !packs::is_default_registry(base)
}

/// Split `<packId>[@<version>]` into its id and optional version.
fn split_pack_ref(pack_ref: &str) -> (String, Option<String>) {
    match pack_ref.rsplit_once('@') {
        Some((id, version)) if !id.is_empty() && !version.is_empty() => {
            (id.to_owned(), Some(version.to_owned()))
        }
        _ => (pack_ref.to_owned(), None),
    }
}

fn fetch_err(json: bool, scope: &str, err: &PackFetchError) -> ! {
    let message = format!("{scope}: {err}");
    if json {
        println!("{}", json_compact_or(&json!({ "error": message }), "{}"));
        std::process::exit(1);
    }
    eprintln!("{} {message}", style::err("error:"));
    eprintln!(
        "  {} {}",
        style::emerald(style::sym::TIP),
        style::pewter(
            "the rule-pack registry is unavailable; retry later, or point at a catalog with --registry <url>"
        ),
    );
    std::process::exit(1);
}

/// `difflore packs list` — fetch the registry catalog and print each pack's
/// id, name, target languages/frameworks, latest version, verified badge, and
/// rule count. `--installed` lists locally-installed packs instead (no network).
pub(crate) async fn handle_list(registry: Option<String>, installed: bool, json: bool) {
    if installed {
        handle_installed(json).await;
        return;
    }

    let base = registry_base(registry);
    let custom = is_custom_registry(&base);
    let index = match fetch_index(&base).await {
        Ok(i) => i,
        Err(e) => fetch_err(json, "fetch index", &e),
    };

    if json {
        println!("{}", json_compact_or(&index, "{}"));
        return;
    }

    if index.packs.is_empty() {
        println!("No packs found in registry {}.", style::pewter(&base));
        return;
    }

    println!("Available rule packs ({}):\n", style::pewter(&base));
    for pack in &index.packs {
        print_index_entry(pack, custom);
    }
    println!(
        "\n{} install one with {}",
        style::emerald(style::sym::TIP),
        style::cmd("difflore packs install <packId>"),
    );
}

fn print_index_entry(entry: &packs::PackIndexEntry, custom_registry: bool) {
    let badge = match &entry.maintainer {
        Some(m) if m.verified && custom_registry => " [verified (custom registry)]".to_owned(),
        Some(m) if m.verified => " [verified]".to_owned(),
        _ => String::new(),
    };
    let langs = entry
        .target
        .as_ref()
        .map(|t| t.languages.join(", "))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "—".to_owned());
    let rule_count = entry
        .resolve_version(None)
        .and_then(|(_, v)| v.rule_count)
        .map_or_else(|| "?".to_owned(), |n| n.to_string());
    println!(
        "  {} {}{}",
        style::ident(&entry.id),
        entry.name,
        badge.as_str().dimmed()
    );
    println!(
        "    v{}  ·  {}  ·  {} rules",
        entry.latest, langs, rule_count
    );
}

/// `difflore packs show <packId>` — fetch the manifest and render rule titles +
/// provenance + target globs + license. Read-only, network only.
pub(crate) async fn handle_show(pack_ref: String, registry: Option<String>, json: bool) {
    let base = registry_base(registry);
    let (pack_id, requested_version) = split_pack_ref(&pack_ref);
    let manifest = resolve_manifest(&base, &pack_id, requested_version.as_deref(), json).await;

    if json {
        println!("{}", json_compact_or(&manifest, "{}"));
        return;
    }

    println!(
        "{} {} v{}",
        style::ident(&manifest.id),
        manifest.name,
        manifest.version
    );
    if let Some(desc) = manifest.description.as_deref().filter(|d| !d.is_empty()) {
        println!("  {desc}");
    }
    if let Some(target) = &manifest.target {
        if !target.languages.is_empty() {
            println!("  languages: {}", target.languages.join(", "));
        }
        if !target.frameworks.is_empty() {
            println!("  frameworks: {}", target.frameworks.join(", "));
        }
        if !target.file_globs.is_empty() {
            println!("  globs: {}", target.file_globs.join(", "));
        }
    }
    if let Some(license) = manifest.license.as_deref().filter(|l| !l.is_empty()) {
        println!("  license: {license}");
    }
    if let Some(prov) = &manifest.provenance {
        let summary = prov.summary.as_deref().unwrap_or("");
        println!("  provenance: {} — {summary}", prov.kind);
    }
    println!("\n  rules ({}):", manifest.rules.len());
    for rule in &manifest.rules {
        let sev = rule
            .severity
            .as_deref()
            .map(|s| format!(" [{s}]"))
            .unwrap_or_default();
        println!(
            "    {} {}{}",
            style::sym::BULLET,
            rule.title,
            sev.as_str().dimmed()
        );
    }
}

/// `difflore packs install <packId>[@version]` — fetch + verify + install the
/// pack's rules as suggestion-only starter memory. `--dry-run` previews the
/// rows without writing; `--yes` skips the (currently informational) prompt.
pub(crate) async fn handle_install(
    pack_ref: String,
    registry: Option<String>,
    dry_run: bool,
    _yes: bool,
    json: bool,
) {
    let base = registry_base(registry);
    let (pack_id, requested_version) = split_pack_ref(&pack_ref);
    let manifest = resolve_manifest(&base, &pack_id, requested_version.as_deref(), json).await;

    let ctx = CommandContext::new(OutputMode::from_json_flag(json)).await;
    let outcome = match install_pack(&ctx.db, &manifest, dry_run).await {
        Ok(o) => o,
        Err(e) => {
            let message = format!("install failed: {e}");
            if json {
                println!("{}", json_compact_or(&json!({ "error": message }), "{}"));
            }
            exit_err(&message);
        }
    };

    if json {
        let rules: Vec<_> = outcome
            .rules
            .iter()
            .map(|r| {
                json!({
                    "skillId": r.skill_id,
                    "packRuleId": r.pack_rule_id,
                    "title": r.title,
                    "filePatterns": r.file_patterns,
                    "tags": r.tags,
                    "origin": r.origin,
                    "sourceRepo": r.source_repo,
                    "confidence": r.confidence,
                    "hasExample": r.has_example,
                })
            })
            .collect();
        println!(
            "{}",
            json_compact_or(
                &json!({
                    "packId": outcome.pack_id,
                    "version": outcome.pack_version,
                    "dryRun": outcome.dry_run,
                    "ruleCount": outcome.rules.len(),
                    "superseded": outcome.superseded_rule_ids,
                    "rules": rules,
                }),
                "{}"
            )
        );
        return;
    }

    if outcome.dry_run {
        println!(
            "{} dry run — would install {} rule(s) from {} v{}:",
            style::emerald(style::sym::TIP),
            outcome.rules.len(),
            style::ident(&outcome.pack_id),
            outcome.pack_version,
        );
        for r in &outcome.rules {
            println!(
                "    {} {}\n      id={}  origin={}  source_repo={}  confidence={:.2}\n      globs=[{}]  tags=[{}]",
                style::sym::BULLET,
                r.title,
                r.skill_id,
                r.origin,
                r.source_repo,
                r.confidence,
                r.file_patterns.join(", "),
                r.tags.join(", "),
            );
        }
        println!(
            "\n  re-run without {} to write these rows.",
            style::cmd("--dry-run")
        );
        return;
    }

    println!(
        "{} Installed {} starter suggestion(s) from {} v{}.",
        style::ok(style::sym::OK),
        outcome.rules.len(),
        style::ident(&outcome.pack_id),
        outcome.pack_version,
    );
    if !outcome.superseded_rule_ids.is_empty() {
        println!(
            "  superseded {} older pack row(s) from a previous version.",
            outcome.superseded_rule_ids.len()
        );
    }
    println!(
        "  These show up only until this repo has its own memory. Run {} to learn from YOUR team's PRs.",
        style::cmd("difflore import-reviews")
    );
}

/// `difflore packs installed` (alias `packs list --installed`) — list locally
/// installed packs, grouped by `pack:<id>@<version>` tag. No network.
pub(crate) async fn handle_installed(json: bool) {
    let ctx = CommandContext::new(OutputMode::from_json_flag(json)).await;
    let groups = match query_installed_packs(&ctx.db).await {
        Ok(g) => g,
        Err(e) => {
            let message = format!("could not read installed packs: {e}");
            if json {
                println!("{}", json_compact_or(&json!({ "error": message }), "{}"));
            }
            exit_err(&message);
        }
    };

    if json {
        let payload: Vec<_> = groups
            .iter()
            .map(|g| json!({ "pack": g.version_tag, "ruleCount": g.rule_count }))
            .collect();
        println!(
            "{}",
            json_compact_or(&json!({ "installed": payload }), "{}")
        );
        return;
    }

    if groups.is_empty() {
        println!(
            "No installed packs. Browse with {} or install with {}.",
            style::cmd("difflore packs list"),
            style::cmd("difflore packs install <packId>"),
        );
        return;
    }

    println!("Installed packs:");
    for g in &groups {
        println!(
            "  {} {}  ·  {} rule(s)",
            style::sym::BULLET,
            style::ident(&g.version_tag),
            g.rule_count
        );
    }
}

/// `difflore packs uninstall <packId>` — delete all pack-origin rows (+ SKILL.md
/// dirs) whose tags reference `pack:<id>` (any version). Bumps store freshness
/// by removing rows, so the cross-repo starter index rebuilds on next recall.
pub(crate) async fn handle_uninstall(pack_id: String, _yes: bool, json: bool) {
    let ctx = CommandContext::new(OutputMode::from_json_flag(json)).await;
    let (pack_id, _) = split_pack_ref(&pack_id);
    let removed = match uninstall_pack_rows(&ctx.db, &pack_id).await {
        Ok(r) => r,
        Err(e) => {
            let message = format!("uninstall failed: {e}");
            if json {
                println!("{}", json_compact_or(&json!({ "error": message }), "{}"));
            }
            exit_err(&message);
        }
    };

    if json {
        println!(
            "{}",
            json_compact_or(
                &json!({ "packId": pack_id, "removed": removed.len(), "skillIds": removed }),
                "{}"
            )
        );
        return;
    }

    if removed.is_empty() {
        println!(
            "No installed rules found for pack {}.",
            style::ident(&pack_id)
        );
        return;
    }
    println!(
        "{} Uninstalled {} rule(s) from pack {}.",
        style::ok(style::sym::OK),
        removed.len(),
        style::ident(&pack_id),
    );
}

/// `difflore packs publish` validates a local pack manifest, then prints the
/// validated pack plus the manual PR step against the registry repo.
pub(crate) async fn handle_publish(path: String, _registry: Option<String>, json: bool) {
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            let message = format!("could not read pack manifest at {path}: {e}");
            if json {
                println!("{}", json_compact_or(&json!({ "error": message }), "{}"));
            }
            exit_err(&message);
        }
    };
    let manifest: PackManifest = match serde_json::from_slice(&bytes) {
        Ok(m) => m,
        Err(e) => {
            let message = format!("{path} is not a valid pack.json: {e}");
            if json {
                println!("{}", json_compact_or(&json!({ "error": message }), "{}"));
            }
            exit_err(&message);
        }
    };

    // Validate isolation: published packs must not carry private `owner/repo`
    // provenance. Local installs re-namespace `source_repo` to `pack:<id>`.
    let sha = packs::manifest_sha256(&bytes);

    if json {
        println!(
            "{}",
            json_compact_or(
                &json!({
                    "validated": true,
                    "packId": manifest.id,
                    "version": manifest.version,
                    "ruleCount": manifest.rules.len(),
                    "sha256": sha,
                    "nextStep": "Open a PR adding this pack to the registry repo (difflore/rule-packs).",
                }),
                "{}"
            )
        );
        return;
    }

    println!(
        "{} Validated pack {} v{} ({} rule(s)).",
        style::ok(style::sym::OK),
        style::ident(&manifest.id),
        manifest.version,
        manifest.rules.len(),
    );
    println!("  sha256: {sha}");
    println!(
        "\n{} Publishing is PR-based (human-reviewed). Next:",
        style::emerald(style::sym::TIP)
    );
    println!(
        "    1. Add this pack under packs/{}/ in the registry repo.",
        manifest.id
    );
    println!(
        "    2. Pin its sha256 in index.json: {}",
        style::pewter(&sha)
    );
    println!("    3. Open a PR against difflore/rule-packs for review.");
}

/// Resolve a manifest from the registry: fetch the index, look up the pack +
/// version, then fetch + verify the manifest. Exits with a clear error on any
/// failure.
async fn resolve_manifest(
    base: &str,
    pack_id: &str,
    requested_version: Option<&str>,
    json: bool,
) -> PackManifest {
    let index: PackIndex = match fetch_index(base).await {
        Ok(i) => i,
        Err(e) => fetch_err(json, "fetch index", &e),
    };
    let Some(entry) = index.find(pack_id) else {
        let message = format!(
            "pack '{pack_id}' not found in registry {base}. Run `difflore packs list` to see available packs."
        );
        if json {
            println!("{}", json_compact_or(&json!({ "error": message }), "{}"));
        }
        exit_err(&message);
    };
    let Some((version, version_row)) = entry.resolve_version(requested_version) else {
        let message = format!(
            "pack '{pack_id}' has no version '{}'",
            requested_version.unwrap_or("<latest>")
        );
        if json {
            println!("{}", json_compact_or(&json!({ "error": message }), "{}"));
        }
        exit_err(&message);
    };
    match fetch_manifest(base, &version_row.manifest, &version_row.sha256).await {
        Ok(m) => m,
        Err(e) => fetch_err(json, &format!("fetch pack {pack_id}@{version}"), &e),
    }
}

// ── Local store queries (runtime-checked, no offline sqlx cache needed) ──

struct InstalledGroup {
    version_tag: String,
    rule_count: usize,
}

/// Group locally-installed pack rows by their `pack:<id>@<version>` tag.
async fn query_installed_packs(
    db: &difflore_core::SqlitePool,
) -> Result<Vec<InstalledGroup>, difflore_core::CoreError> {
    let tags_rows: Vec<(String,)> =
        sqlx::query_as("SELECT tags FROM skills WHERE origin = ?1 AND status = 'active'")
            .bind(packs::PACK_ORIGIN)
            .fetch_all(db)
            .await?;
    let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for (tags_json,) in tags_rows {
        if let Ok(tags) = serde_json::from_str::<Vec<String>>(&tags_json) {
            // The `pack:<id>@<version>` tag is the install-identity group key.
            if let Some(version_tag) = tags
                .into_iter()
                .find(|t| t.starts_with("pack:") && t.contains('@'))
            {
                *counts.entry(version_tag).or_insert(0) += 1;
            }
        }
    }
    Ok(counts
        .into_iter()
        .map(|(version_tag, rule_count)| InstalledGroup {
            version_tag,
            rule_count,
        })
        .collect())
}

/// Delete every pack-origin row (and its examples) whose tags reference
/// `pack:<id>` for any version, returning the removed skill ids. Best-effort
/// SKILL.md dir cleanup mirrors the install path.
async fn uninstall_pack_rows(
    db: &difflore_core::SqlitePool,
    pack_id: &str,
) -> Result<Vec<String>, difflore_core::CoreError> {
    // Match the version-tag prefix so `pack:<id>@*` (and not a different pack
    // that merely shares an id prefix) is removed. The trailing `@` anchors the
    // boundary between id and version.
    let needle = format!("pack:{}@", pack_id.trim());
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT id, tags FROM skills WHERE origin = ?1")
            .bind(packs::PACK_ORIGIN)
            .fetch_all(db)
            .await?;
    let mut to_remove: Vec<String> = Vec::new();
    for (id, tags_json) in rows {
        if let Ok(tags) = serde_json::from_str::<Vec<String>>(&tags_json) {
            if tags.iter().any(|t| t.starts_with(&needle)) {
                to_remove.push(id);
            }
        }
    }

    if to_remove.is_empty() {
        return Ok(to_remove);
    }

    let base_dir = difflore_core::skill_fs::skills_base_dir()
        .map(|p| p.join("pack"))
        .ok();

    let mut tx = db.begin().await?;
    for id in &to_remove {
        sqlx::query("DELETE FROM rule_examples WHERE skill_id = ?1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM skills WHERE id = ?1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;

    if let Some(base) = base_dir {
        for id in &to_remove {
            let _ = std::fs::remove_dir_all(base.join(id));
            let _ = difflore_core::skill_fs::sync_engine_link("pack", id, "claude", false);
        }
    }

    Ok(to_remove)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_pack_ref_parses_version() {
        assert_eq!(
            split_pack_ref("difflore/go-http-safety@1.2.0"),
            (
                "difflore/go-http-safety".to_owned(),
                Some("1.2.0".to_owned())
            )
        );
        assert_eq!(
            split_pack_ref("difflore/go-http-safety"),
            ("difflore/go-http-safety".to_owned(), None)
        );
    }

    #[test]
    fn registry_base_falls_back_to_default() {
        assert_eq!(registry_base(None), DEFAULT_PACK_REGISTRY);
        assert_eq!(registry_base(Some("  ".to_owned())), DEFAULT_PACK_REGISTRY);
        assert_eq!(
            registry_base(Some("https://example.com/fork".to_owned())),
            "https://example.com/fork"
        );
    }

    #[test]
    fn custom_registry_detection() {
        assert!(!is_custom_registry(DEFAULT_PACK_REGISTRY));
        assert!(is_custom_registry("https://example.com/fork"));
    }
}
