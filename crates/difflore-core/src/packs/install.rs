//! `install_pack` — write a fetched [`PackManifest`] into the local `skills`
//! store. Reuses the `remember.rs` INSERT-INTO-skills shape: a row + SKILL.md
//! on disk + an optional `rule_examples` pair, all in one transaction. Every
//! installed row carries `origin = 'pack'`, a synthetic
//! `source_repo = "pack:<id>"`, the mandatory pack tags, and
//! `confidence = 0.55` — the levers that confine pack rules to the
//! suggestion-only cross-repo starter fallback.
//!
//! The rule body is rendered through
//! [`crate::context::rule_render::render_code_spec`] so an installed pack rule
//! is byte-for-byte indistinguishable in body from a mined rule.

use sqlx::SqlitePool;

use crate::context::rule_render::{RuleRenderInput, render_code_spec};
use crate::context::rule_source::RuleExample;
use crate::errors::CoreError;
use crate::observability::privacy::{redact_secretish_tokens, strip_private_tagged_regions};
use crate::packs::manifest::{PackManifest, PackRule};
use crate::packs::{
    PACK_CONFIDENCE, PACK_ORIGIN, pack_rule_tag, pack_source_repo, pack_version_tag,
};

/// On-disk source bucket for pack SKILL.md dirs: `<data>/skills/pack/<slug>`.
/// Distinct from `local`/`cloud`/`team` so a pack rule's files never collide
/// with a mined rule's.
const PACK_SKILL_SOURCE: &str = "pack";

/// Defense-in-depth redaction: even though packs are public, run their
/// bodies/examples through the same redaction the ingest pipeline uses before
/// they touch disk/DB.
fn sanitize(input: &str) -> String {
    redact_secretish_tokens(&strip_private_tagged_regions(input))
}

/// Path-traversal-safe slug, mirroring `remember.rs`'s `create_local`
/// algorithm so the generated directory name is predictable and can't escape
/// the skills root.
fn slugify(value: &str) -> String {
    value
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

/// Short, deterministic hex suffix derived from the pack-namespaced rule id, so
/// the local `skills.id` is stable across re-installs (idempotency) yet can
/// never collide with `conv-*` / `local-*` ids or a cloud UUID (which
/// `looks_like_cloud_uuid` rejects — this id is not a UUID).
fn deterministic_suffix(pack_rule_id: &str) -> String {
    use std::fmt::Write as _;

    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(pack_rule_id.as_bytes());
    let digest = hasher.finalize();
    digest.iter().take(4).fold(String::new(), |mut acc, b| {
        let _ = write!(acc, "{b:02x}");
        acc
    })
}

/// Local `skills.id` for a pack rule: `pack-<packSlug>-<ruleSlug>-<8hex>`.
fn local_skill_id(pack_id: &str, pack_rule_id: &str) -> String {
    // The rule slug strips the redundant `<packSlug>/` namespace prefix the
    // manifest carries (e.g. `go-http-safety/413-body-limit`) so the id reads
    // cleanly; the deterministic suffix is hashed over the FULL pack-rule id so
    // distinctness is preserved even if two packs share a leaf slug.
    let pack_slug = slugify(pack_id);
    let rule_leaf = pack_rule_id.rsplit('/').next().unwrap_or(pack_rule_id);
    let rule_slug = slugify(rule_leaf);
    format!(
        "pack-{pack_slug}-{rule_slug}-{}",
        deterministic_suffix(pack_rule_id)
    )
}

/// Resolve the effective glob list for a rule: its own `fileGlobs` override the
/// pack-level `target.fileGlobs` default.
fn effective_globs(rule: &PackRule, manifest: &PackManifest) -> Vec<String> {
    let mut globs: Vec<String> = if rule.file_globs.is_empty() {
        manifest
            .target
            .as_ref()
            .map(|t| t.file_globs.clone())
            .unwrap_or_default()
    } else {
        rule.file_globs.clone()
    };
    globs.retain(|g| !g.trim().is_empty());
    globs
}

/// Assemble the mandatory tag set for an installed pack rule: `pack`,
/// `pack:<id>@<version>`, `pack-rule:<ruleId>`, the language tag,
/// `severity:<level>` (when present), plus the rule's own declared tags.
fn build_tags(rule: &PackRule, manifest: &PackManifest) -> Vec<String> {
    let mut tags: Vec<String> = Vec::new();
    tags.push(PACK_ORIGIN.to_owned());
    tags.push(pack_version_tag(&manifest.id, &manifest.version));
    tags.push(pack_rule_tag(&rule.id));

    if let Some(lang) = manifest
        .target
        .as_ref()
        .and_then(|t| t.languages.first())
        .map(|l| l.trim().to_ascii_lowercase())
        .filter(|l| !l.is_empty())
    {
        tags.push(lang);
    }

    if let Some(sev) = rule
        .severity
        .as_deref()
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
    {
        tags.push(format!("severity:{sev}"));
    }

    for tag in &rule.tags {
        let trimmed = tag.trim();
        if !trimmed.is_empty() {
            tags.push(trimmed.to_owned());
        }
    }

    // De-dup while preserving first-seen order.
    let mut seen = std::collections::HashSet::new();
    tags.retain(|t| seen.insert(t.clone()));
    tags
}

/// Render the rule body via the canonical renderer, constructing a
/// [`RuleRenderInput`] exactly as the MCP `get_rules` path does so a pack rule
/// renders identically to a mined rule. The optional example feeds the
/// renderer's Validation matrix + Cases sections.
fn render_body(
    skill_id: &str,
    rule: &PackRule,
    manifest: &PackManifest,
    globs: &[String],
    example: Option<&RuleExample>,
) -> String {
    let source_repo = pack_source_repo(&manifest.id);
    let description = rule
        .body
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .map(sanitize)
        .unwrap_or_default();
    let examples_slice = example.map(std::slice::from_ref);
    let input = RuleRenderInput {
        id: skill_id,
        name: rule.title.trim(),
        r#type: "review_standard",
        confidence: PACK_CONFIDENCE,
        origin: PACK_ORIGIN,
        source_repo: Some(source_repo.as_str()),
        file_patterns: globs,
        description: &description,
        // Packs carry no trigger/check_prompt; the renderer omits those
        // sections.
        trigger: None,
        check_prompt: None,
        examples: examples_slice,
    };
    render_code_spec(&input)
}

/// Build the optional example pair for a rule. Only present when BOTH sides are
/// non-empty after redaction — a one-sided example hurts few-shot quality
/// (mirrors `remember.rs`).
fn build_example(skill_id: &str, rule: &PackRule) -> Option<RuleExample> {
    let ex = rule.examples.as_ref()?;
    let bad = sanitize(ex.bad.as_deref().unwrap_or_default());
    let good = sanitize(ex.good.as_deref().unwrap_or_default());
    if bad.trim().is_empty() || good.trim().is_empty() {
        return None;
    }
    let description = ex
        .description
        .as_deref()
        .map(sanitize)
        .map(|d| d.trim().to_owned())
        .filter(|d| !d.is_empty());
    Some(RuleExample {
        id: format!(
            "example-pack-{}",
            crate::packs::manifest::manifest_sha256(skill_id.as_bytes())
        ),
        skill_id: skill_id.to_owned(),
        bad_code: bad.trim().to_owned(),
        good_code: good.trim().to_owned(),
        description,
        source: PACK_ORIGIN.to_owned(),
    })
}

/// SKILL.md markdown for a pack rule, so the on-disk file reads naturally and
/// the rendered code-spec body round-trips. Mirrors the frontmatter shape
/// `remember.rs` writes.
fn build_skill_md(rule: &PackRule, tags: &[String], body: &str) -> String {
    let mut md = String::new();
    md.push_str("---\n");
    md.push_str("type: review_standard\n");
    md.push_str("engines: [claude]\n");
    md.push_str(&format!("tags: [{}]\n", tags.join(", ")));
    md.push_str("origin: pack\n");
    md.push_str("---\n\n");
    md.push_str(&format!("# {}\n\n", rule.title.trim()));
    md.push_str(body);
    md.push('\n');
    md
}

/// One installed-rule summary, returned for `--dry-run` preview and the
/// install confirmation: id, globs, tags, origin, synthetic source_repo,
/// confidence.
#[derive(Debug, Clone)]
pub struct InstalledPackRule {
    pub skill_id: String,
    pub pack_rule_id: String,
    pub title: String,
    pub file_patterns: Vec<String>,
    pub tags: Vec<String>,
    pub origin: String,
    pub source_repo: String,
    pub confidence: f64,
    pub has_example: bool,
}

/// Result of an [`install_pack`] run.
#[derive(Debug, Clone)]
pub struct InstallPackOutcome {
    pub pack_id: String,
    pub pack_version: String,
    /// The rules that would be / were written.
    pub rules: Vec<InstalledPackRule>,
    /// Rows removed because a different version of the same `pack-rule:<id>` was
    /// already installed (version supersede). Empty for a fresh install and a
    /// `dry_run`.
    pub superseded_rule_ids: Vec<String>,
    /// True when nothing was written: either `dry_run`, or this exact
    /// `pack:<id>@<version>` was already fully installed (idempotent no-op).
    pub dry_run: bool,
}

/// Install (or dry-run preview) every rule in a fetched pack manifest.
///
/// Idempotency / supersede: a rule already present at this exact
/// `pack:<id>@<version>` is left untouched; a rule present at a *different*
/// version of the same pack is deleted and replaced, keyed on its
/// `pack-rule:<ruleId>` tag. All writes happen in one transaction.
pub async fn install_pack(
    db: &SqlitePool,
    manifest: &PackManifest,
    dry_run: bool,
) -> Result<InstallPackOutcome, CoreError> {
    let source_repo = pack_source_repo(&manifest.id);

    // Compute everything purely (no DB writes) so a dry-run is a faithful
    // preview of the real install.
    struct Prepared {
        skill_id: String,
        pack_rule_id: String,
        title: String,
        globs: Vec<String>,
        tags: Vec<String>,
        body: String,
        skill_md: String,
        example: Option<RuleExample>,
    }
    let mut prepared: Vec<Prepared> = Vec::with_capacity(manifest.rules.len());
    for rule in &manifest.rules {
        if rule.title.trim().is_empty() {
            return Err(CoreError::Validation(format!(
                "pack '{}' has a rule with an empty title (id '{}')",
                manifest.id, rule.id
            )));
        }
        let skill_id = local_skill_id(&manifest.id, &rule.id);
        let globs = effective_globs(rule, manifest);
        let tags = build_tags(rule, manifest);
        let example = build_example(&skill_id, rule);
        let body = render_body(&skill_id, rule, manifest, &globs, example.as_ref());
        let skill_md = build_skill_md(rule, &tags, &body);
        prepared.push(Prepared {
            skill_id,
            pack_rule_id: rule.id.clone(),
            title: rule.title.trim().to_owned(),
            globs,
            tags,
            body,
            skill_md,
            example,
        });
    }

    let installed: Vec<InstalledPackRule> = prepared
        .iter()
        .map(|p| InstalledPackRule {
            skill_id: p.skill_id.clone(),
            pack_rule_id: p.pack_rule_id.clone(),
            title: p.title.clone(),
            file_patterns: p.globs.clone(),
            tags: p.tags.clone(),
            origin: PACK_ORIGIN.to_owned(),
            source_repo: source_repo.clone(),
            confidence: PACK_CONFIDENCE,
            has_example: p.example.is_some(),
        })
        .collect();

    if dry_run {
        return Ok(InstallPackOutcome {
            pack_id: manifest.id.clone(),
            pack_version: manifest.version.clone(),
            rules: installed,
            superseded_rule_ids: Vec::new(),
            dry_run: true,
        });
    }

    // Resolve and confine the on-disk pack root once.
    let base_dir = crate::skill_fs::skills_base_dir()
        .map_err(CoreError::Internal)?
        .join(PACK_SKILL_SOURCE);
    std::fs::create_dir_all(&base_dir)
        .map_err(|e| CoreError::Internal(format!("failed to create pack skills dir: {e}")))?;
    let canonical_base = base_dir
        .canonicalize()
        .map_err(|e| CoreError::Internal(format!("failed to resolve pack skills dir: {e}")))?;

    let now_utc = chrono::Utc::now();
    let now = now_utc.format("%Y-%m-%d %H:%M:%S").to_string();
    let now_ms: i64 = now_utc.timestamp_millis();

    let mut tx = db.begin().await?;
    let mut superseded_rule_ids: Vec<String> = Vec::new();
    let mut written_dirs: Vec<std::path::PathBuf> = Vec::new();

    for p in &prepared {
        let version_tag = pack_version_tag(&manifest.id, &manifest.version);
        let rule_tag = pack_rule_tag(&p.pack_rule_id);

        // Idempotency: an identical row already installed at THIS exact
        // version is a no-op (skip it, leaving the existing row + examples).
        let already_at_version: Option<String> = sqlx::query_scalar(
            "SELECT id FROM skills WHERE origin = ?1 AND tags LIKE '%' || ?2 || '%' \
             AND tags LIKE '%' || ?3 || '%' LIMIT 1",
        )
        .bind(PACK_ORIGIN)
        .bind(&rule_tag)
        .bind(&version_tag)
        .fetch_optional(&mut *tx)
        .await?;
        if already_at_version.is_some() {
            continue;
        }

        // Version supersede: a DIFFERENT version of the same pack-rule is
        // delete-and-replaced. Match on the `pack-rule:<id>` tag (version
        // independent) but only among pack-origin rows.
        let stale_ids: Vec<String> = sqlx::query_scalar(
            "SELECT id FROM skills WHERE origin = ?1 AND tags LIKE '%' || ?2 || '%'",
        )
        .bind(PACK_ORIGIN)
        .bind(&rule_tag)
        .fetch_all(&mut *tx)
        .await?;
        for stale in &stale_ids {
            sqlx::query("DELETE FROM rule_examples WHERE skill_id = ?1")
                .bind(stale)
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM skills WHERE id = ?1")
                .bind(stale)
                .execute(&mut *tx)
                .await?;
            // Best-effort disk cleanup of the superseded row's dir.
            let stale_dir = base_dir.join(stale);
            let _ = std::fs::remove_dir_all(&stale_dir);
            superseded_rule_ids.push(stale.clone());
        }

        // Write SKILL.md, path-confined to the pack root.
        let skill_dir = base_dir.join(&p.skill_id);
        let skill_dir_for_check = canonical_base.join(&p.skill_id);
        if !skill_dir_for_check.starts_with(&canonical_base) {
            tx.rollback().await.ok();
            cleanup_dirs(&written_dirs);
            return Err(CoreError::Validation(
                "install_pack: invalid slug after sanitization".into(),
            ));
        }
        std::fs::create_dir_all(&skill_dir)
            .map_err(|e| CoreError::Internal(format!("failed to create skill directory: {e}")))?;
        let canonical_skill = skill_dir
            .canonicalize()
            .map_err(|e| CoreError::Internal(format!("failed to resolve skill directory: {e}")))?;
        if !canonical_skill.starts_with(&canonical_base) {
            tx.rollback().await.ok();
            cleanup_dirs(&written_dirs);
            return Err(CoreError::Validation("install_pack: path escape".into()));
        }
        std::fs::write(skill_dir.join("SKILL.md"), &p.skill_md)
            .map_err(|e| CoreError::Internal(format!("failed to write SKILL.md: {e}")))?;
        written_dirs.push(skill_dir.clone());

        let engines_json = serde_json::to_string(&["claude"])?;
        let tags_json = serde_json::to_string(&p.tags)?;
        let file_patterns_json: Option<String> = if p.globs.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&p.globs)?)
        };

        // Reuse the `remember.rs` INSERT-INTO-skills shape, swapping
        // source='pack', origin='pack', synthetic source_repo, and the 0.55
        // confidence floor. `enabled_for_claude = 1` mirrors the remember path.
        sqlx::query(
            "INSERT INTO skills
             (id, name, source, directory, version, description, type, engines, tags,
              trigger, check_prompt, file_patterns, source_repo, enabled_for_claude,
              confidence_score, installed_at, updated_at, origin, content_hash, hash_created_at)
             VALUES (?1, ?2, 'pack', ?3, ?4, ?5, 'review_standard', ?6, ?7,
                     NULL, NULL, ?8, ?9, 1, ?10, ?11, ?11, ?12, NULL, ?13)",
        )
        .bind(&p.skill_id)
        .bind(&p.title)
        .bind(&p.skill_id)
        .bind(&manifest.version)
        .bind(&p.body)
        .bind(&engines_json)
        .bind(&tags_json)
        .bind(file_patterns_json.as_deref())
        .bind(source_repo.as_str())
        .bind(PACK_CONFIDENCE)
        .bind(now.as_str())
        .bind(PACK_ORIGIN)
        .bind(now_ms)
        .execute(&mut *tx)
        .await?;

        if let Some(example) = &p.example {
            let ex_now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
            sqlx::query(
                "INSERT INTO rule_examples (id, skill_id, bad_code, good_code, description, source, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )
            .bind(&example.id)
            .bind(&example.skill_id)
            .bind(&example.bad_code)
            .bind(&example.good_code)
            .bind(example.description.as_deref())
            .bind(&example.source)
            .bind(&ex_now)
            .execute(&mut *tx)
            .await?;
        }
    }

    if let Err(e) = tx.commit().await {
        cleanup_dirs(&written_dirs);
        return Err(e.into());
    }

    // Keep the claude engine link consistent with `enabled_for_claude = 1`,
    // mirroring `remember.rs`. Best-effort: a link failure must not fail an
    // otherwise-committed install.
    for p in &prepared {
        if let Err(e) =
            crate::skill_fs::sync_engine_link(PACK_SKILL_SOURCE, &p.skill_id, "claude", true)
        {
            eprintln!(
                "warning: sync_engine_link failed for pack rule {}: {e}",
                p.skill_id
            );
        }
    }

    Ok(InstallPackOutcome {
        pack_id: manifest.id.clone(),
        pack_version: manifest.version.clone(),
        rules: installed,
        superseded_rule_ids,
        dry_run: false,
    })
}

/// Best-effort cleanup of partially-written SKILL.md dirs after a transaction
/// rollback, mirroring `remember.rs`'s `remove_dir_all` on insert failure.
fn cleanup_dirs(dirs: &[std::path::PathBuf]) {
    for dir in dirs {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packs::manifest::{PackRuleExamples, PackTarget};

    fn sample_manifest() -> PackManifest {
        PackManifest {
            schema_version: 1,
            id: "difflore/go-http-safety".to_owned(),
            name: "Go HTTP handler safety".to_owned(),
            version: "1.2.0".to_owned(),
            description: None,
            target: Some(PackTarget {
                languages: vec!["go".to_owned()],
                frameworks: vec!["net/http".to_owned()],
                file_globs: vec!["**/*.go".to_owned()],
            }),
            maintainer: None,
            license: None,
            provenance: None,
            rules: vec![PackRule {
                id: "go-http-safety/413-body-limit".to_owned(),
                title: "Return 413 when a request body exceeds the size limit".to_owned(),
                severity: Some("error".to_owned()),
                file_globs: vec![],
                tags: vec!["http".to_owned(), "security".to_owned()],
                body: Some(
                    "Reject oversized request bodies with HTTP 413 instead of \
                     reading them unbounded into memory."
                        .to_owned(),
                ),
                examples: Some(PackRuleExamples {
                    bad: Some("data, _ := io.ReadAll(r.Body)".to_owned()),
                    good: Some("r.Body = http.MaxBytesReader(w, r.Body, max)".to_owned()),
                    description: Some("reviewer flagged unbounded read".to_owned()),
                }),
                provenance: None,
            }],
        }
    }

    #[test]
    fn local_skill_id_is_namespaced_and_not_a_uuid() {
        let id = local_skill_id("difflore/go-http-safety", "go-http-safety/413-body-limit");
        assert!(id.starts_with("pack-difflore-go-http-safety-413-body-limit-"));
        // Not a UUID -> looks_like_cloud_uuid rejects it, so it's never
        // mistaken for a cloud-synced rule.
        assert!(!id.contains('/'));
        // Deterministic: same input -> same id (idempotency).
        assert_eq!(
            id,
            local_skill_id("difflore/go-http-safety", "go-http-safety/413-body-limit")
        );
    }

    #[test]
    fn mandatory_tags_present() {
        let manifest = sample_manifest();
        let tags = build_tags(&manifest.rules[0], &manifest);
        assert!(tags.contains(&"pack".to_owned()));
        assert!(tags.contains(&"pack:difflore/go-http-safety@1.2.0".to_owned()));
        assert!(tags.contains(&"pack-rule:go-http-safety/413-body-limit".to_owned()));
        assert!(tags.contains(&"go".to_owned()));
        assert!(tags.contains(&"severity:error".to_owned()));
        assert!(tags.contains(&"http".to_owned()));
    }

    #[test]
    fn rule_globs_override_pack_default() {
        let mut manifest = sample_manifest();
        manifest.rules[0].file_globs = vec!["internal/http/**/*.go".to_owned()];
        let globs = effective_globs(&manifest.rules[0], &manifest);
        assert_eq!(globs, vec!["internal/http/**/*.go".to_owned()]);
    }

    #[test]
    fn pack_default_globs_used_when_rule_has_none() {
        let manifest = sample_manifest();
        let globs = effective_globs(&manifest.rules[0], &manifest);
        assert_eq!(globs, vec!["**/*.go".to_owned()]);
    }

    #[test]
    fn body_renders_via_item_six_code_spec() {
        let manifest = sample_manifest();
        let rule = &manifest.rules[0];
        let globs = effective_globs(rule, &manifest);
        let skill_id = local_skill_id(&manifest.id, &rule.id);
        let example = build_example(&skill_id, rule);
        let body = render_body(&skill_id, rule, &manifest, &globs, example.as_ref());
        // Header / Scope / Contract / Cases come from item ⑥'s renderer.
        assert!(body.starts_with(&format!("## Rule {skill_id} -")));
        assert!(body.contains("Scope: **/*.go"));
        assert!(body.contains("Confidence: 0.55"));
        assert!(body.contains("Origin: pack"));
        assert!(body.contains("### Contract"));
        assert!(body.contains("### Cases"));
        // Curated rule with no "When X, Y" / 'pr_review' origin still gets a
        // Validation matrix from its example pair.
        assert!(body.contains("### Validation / Error matrix"));
    }

    #[test]
    fn example_requires_both_sides() {
        let manifest = sample_manifest();
        let rule = &manifest.rules[0];
        let skill_id = local_skill_id(&manifest.id, &rule.id);
        assert!(build_example(&skill_id, rule).is_some());

        let mut one_sided = rule.clone();
        one_sided.examples = Some(PackRuleExamples {
            bad: Some("x".to_owned()),
            good: None,
            description: None,
        });
        assert!(build_example(&skill_id, &one_sided).is_none());
    }
}
