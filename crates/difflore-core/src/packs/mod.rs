//! Shareable starter rule-pack marketplace (roadmap item ①).
//!
//! A *rule pack* is a curated, attributed starter set a brand-new team can
//! install on day-0 to close the cold-start recall gap (`difflore import-reviews`
//! needs `gh` auth + PR history; packs need neither). The registry is a plain
//! public GitHub repo exposing an `index.json` catalog plus per-pack `pack.json`
//! manifests — install is a pure HTTPS GET of public content with a `sha256`
//! supply-chain pin, so it works logged-out and offline-after-cache.
//!
//! ## Honesty / moat guardrails (non-negotiable, see roadmap §1)
//!
//! Installed pack rules are **suggestions, not ratified memory**. They:
//!   - carry `origin = 'pack'` (the authoritative "installed, not mined here"
//!     marker) and a synthetic `source_repo = "pack:<id>"` that can never match
//!     a real git remote — so the runtime scope gate confines them to the
//!     `crossRepoStarter` suggestion-only fallback automatically (no new
//!     privileged retrieval path);
//!   - start at `confidence_score = 0.55`, below `manual` (0.7) and
//!     `conversation` (0.6), so they never start at parity with earned memory;
//!   - carry **no fabricated metrics** — `cited_count` / `trust_rate` reflect
//!     *this team's* observed behavior and start at 0.
//!
//! ## Rule body format (dependency on item ⑥)
//!
//! Pack rule bodies are rendered through item ⑥'s public, DB-free renderer
//! [`crate::context::rule_render::render_code_spec`] so an installed pack rule is
//! byte-for-byte indistinguishable *in body* from a mined rule — only its
//! `origin` / tags / `source_repo` / confidence differ. We do NOT re-implement
//! rendering here.

mod install;
mod manifest;
mod registry;

pub use install::{InstallPackOutcome, InstalledPackRule, install_pack};
pub use manifest::{
    PackIndex, PackIndexEntry, PackIndexVersion, PackMaintainer, PackManifest, PackProvenance,
    PackRule, PackRuleExamples, PackRuleProvenance, PackTarget, manifest_sha256,
};
pub use registry::{
    DEFAULT_PACK_REGISTRY, PackFetchError, fetch_index, fetch_manifest, is_default_registry,
};

/// The `origin` value stamped on every installed pack rule. The single
/// strongest provenance marker; downstream consumers key off it to render a
/// "from a starter pack" badge and to exclude pack rules from any "your team's
/// earned memory" metric or eval. The local `idx_skills_origin` index makes
/// `WHERE origin = 'pack'` cheap.
pub const PACK_ORIGIN: &str = "pack";

/// Base confidence for an installed pack rule. Deliberately below `manual`
/// (0.7) and `conversation` (0.6): pack rules are suggestions and must not
/// start at parity with the team's own earned judgment. `confidence_from_tags`
/// may refine via `severity:` but the install floor stays here.
pub const PACK_CONFIDENCE: f64 = 0.55;

/// Reserved synthetic-`source_repo` namespace prefix. A `pack:` value can never
/// match a real `owner/repo` git remote, which is the isolation key (roadmap
/// §4.2): a pack rule can only ever reach the cross-repo starter fallback.
pub const PACK_SOURCE_REPO_PREFIX: &str = "pack:";

/// Build the synthetic `source_repo` for a pack id (e.g. `difflore/go-http-safety`
/// -> `pack:difflore/go-http-safety`).
#[must_use]
pub fn pack_source_repo(pack_id: &str) -> String {
    format!("{PACK_SOURCE_REPO_PREFIX}{}", pack_id.trim())
}

/// `pack:<id>@<version>` install-identity tag. `packs list --installed` groups
/// locally-installed rows on this tag; `packs install` treats a row already
/// carrying it as idempotent.
#[must_use]
pub fn pack_version_tag(pack_id: &str, version: &str) -> String {
    format!("pack:{}@{}", pack_id.trim(), version.trim())
}

/// `pack-rule:<ruleId>` per-rule identity tag — the lever a version supersede
/// deletes/replaces on, independent of the `@version` suffix.
#[must_use]
pub fn pack_rule_tag(rule_id: &str) -> String {
    format!("pack-rule:{}", rule_id.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthetic_source_repo_is_never_a_real_owner_repo() {
        // The `pack:` prefix is what guarantees a pack rule can never match a
        // git remote — `repo_scope_from_source_repo` would derive a scope that
        // no real `repo_scopes_for_search_rules` value can equal.
        assert_eq!(
            pack_source_repo("difflore/go-http-safety"),
            "pack:difflore/go-http-safety"
        );
    }

    #[test]
    fn version_and_rule_tags_are_stable() {
        assert_eq!(
            pack_version_tag("difflore/go-http-safety", "1.2.0"),
            "pack:difflore/go-http-safety@1.2.0"
        );
        assert_eq!(
            pack_rule_tag("go-http-safety/413-body-limit"),
            "pack-rule:go-http-safety/413-body-limit"
        );
    }
}
