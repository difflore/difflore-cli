//! Shareable starter rule-pack marketplace.
//!
//! A rule pack is a curated, attributed starter set a brand-new team can install
//! on day-0 to close the cold-start recall gap. The registry is a public GitHub
//! repo exposing an `index.json` catalog plus per-pack `pack.json` manifests;
//! install is a plain HTTPS GET of public content with a `sha256` supply-chain
//! pin, so it works logged-out and offline-after-cache.
//!
//! Installed pack rules are suggestions, not ratified memory. They:
//!   - carry `origin = 'pack'` and a synthetic `source_repo = "pack:<id>"` that
//!     can never match a real git remote, so the runtime scope gate confines
//!     them to the `crossRepoStarter` suggestion-only fallback;
//!   - start at `confidence_score = 0.55`, below `manual` (0.7) and
//!     `conversation` (0.6), so they never start at parity with earned memory;
//!   - carry no fabricated metrics — `cited_count` / `trust_rate` reflect this
//!     team's observed behavior and start at 0.
//!
//! Pack rule bodies are rendered through the shared
//! [`crate::context::rule_render::render_code_spec`] so an installed pack rule is
//! byte-for-byte indistinguishable in body from a mined rule — only its `origin`
//! / tags / `source_repo` / confidence differ.

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

/// The `origin` value stamped on every installed pack rule. Downstream
/// consumers key off it to render a "from a starter pack" badge and to exclude
/// pack rules from earned-memory metrics or evals. The `idx_skills_origin`
/// index makes `WHERE origin = 'pack'` cheap.
pub const PACK_ORIGIN: &str = "pack";

/// Base confidence for an installed pack rule. Deliberately below `manual`
/// (0.7) and `conversation` (0.6) so pack suggestions never start at parity
/// with earned judgment. `confidence_from_tags` may refine via `severity:`,
/// but the install floor stays here.
pub const PACK_CONFIDENCE: f64 = 0.55;

/// Reserved synthetic-`source_repo` namespace prefix. A `pack:` value can never
/// match a real `owner/repo` git remote — this is the isolation key that keeps
/// a pack rule reachable only via the cross-repo starter fallback.
pub const PACK_SOURCE_REPO_PREFIX: &str = "pack:";

/// Build the synthetic `source_repo` for a pack id, e.g.
/// `difflore/go-http-safety` -> `pack:difflore/go-http-safety`.
#[must_use]
pub fn pack_source_repo(pack_id: &str) -> String {
    format!("{PACK_SOURCE_REPO_PREFIX}{}", pack_id.trim())
}

/// `pack:<id>@<version>` install-identity tag. `packs list --installed` groups
/// installed rows on it; `packs install` treats a row already carrying it as
/// idempotent.
#[must_use]
pub fn pack_version_tag(pack_id: &str, version: &str) -> String {
    format!("pack:{}@{}", pack_id.trim(), version.trim())
}

/// `pack-rule:<ruleId>` per-rule identity tag — the key a version supersede
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
        // The `pack:` prefix guarantees a pack rule can never match a git
        // remote: the derived scope cannot equal any real repo scope.
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
