//! Serde types for the pack registry `index.json` catalog and per-pack
//! `pack.json` manifest (roadmap §3, §6). The manifest pins only pack-level
//! metadata + attribution and treats each rule's renderable content through
//! item ⑥'s canonical body shape — it does not invent a second body format.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The registry catalog fetched on `packs list` / `packs install`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackIndex {
    pub schema_version: u32,
    #[serde(default)]
    pub generated_at: Option<String>,
    #[serde(default)]
    pub packs: Vec<PackIndexEntry>,
}

impl PackIndex {
    /// Look up a catalog entry by registry-unique pack id.
    #[must_use]
    pub fn find(&self, pack_id: &str) -> Option<&PackIndexEntry> {
        let needle = pack_id.trim();
        self.packs.iter().find(|p| p.id == needle)
    }
}

/// One pack's catalog row. Carries the per-version manifest path + `sha256`
/// pin used to verify the fetched manifest (supply-chain guard).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackIndexEntry {
    pub id: String,
    pub name: String,
    /// Default version when `packs install <id>` omits `@version`.
    pub latest: String,
    /// `version -> {manifest path, sha256, ruleCount}`.
    #[serde(default)]
    pub versions: std::collections::BTreeMap<String, PackIndexVersion>,
    #[serde(default)]
    pub target: Option<PackTarget>,
    #[serde(default)]
    pub maintainer: Option<PackMaintainer>,
    #[serde(default)]
    pub license: Option<String>,
}

impl PackIndexEntry {
    /// Resolve a requested version (or `latest` when `None`) to its catalog
    /// version row. Returns the resolved version string alongside it.
    #[must_use]
    pub fn resolve_version(&self, requested: Option<&str>) -> Option<(String, &PackIndexVersion)> {
        let version = requested.map_or_else(|| self.latest.clone(), ToOwned::to_owned);
        self.versions.get(&version).map(|v| (version, v))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackIndexVersion {
    /// Path to the `pack.json`, relative to the registry root.
    pub manifest: String,
    /// Hex `sha256` over the fetched manifest bytes — verified on install.
    pub sha256: String,
    #[serde(default)]
    pub rule_count: Option<u32>,
}

/// The per-pack `pack.json` manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackManifest {
    pub schema_version: u32,
    /// Registry-unique `<namespace>/<slug>`.
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub target: Option<PackTarget>,
    #[serde(default)]
    pub maintainer: Option<PackMaintainer>,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub provenance: Option<PackProvenance>,
    #[serde(default)]
    pub rules: Vec<PackRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackTarget {
    /// Drives the language tag + default file globs. The first entry becomes
    /// the `RuleDocument.language` tag.
    #[serde(default)]
    pub languages: Vec<String>,
    /// Informational; surfaced in `packs list` / `packs show`.
    #[serde(default)]
    pub frameworks: Vec<String>,
    /// Pack-level default globs; a rule's own `fileGlobs` override these.
    #[serde(default)]
    pub file_globs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackMaintainer {
    pub name: String,
    #[serde(default)]
    pub url: Option<String>,
    /// Set ONLY by the registry owner for first-party packs. A custom
    /// `--registry` must render this as `verified (custom registry)` so the
    /// trust badge is never misleading.
    #[serde(default)]
    pub verified: bool,
}

/// Pack-level provenance default. `kind` is the honesty contract (roadmap §3.3):
/// `curated` | `mined` | `imported`. No `kind` may carry trust/acceptance
/// numbers into the installing team's store.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackProvenance {
    pub kind: String,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub sources: Vec<PackProvenanceSource>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackProvenanceSource {
    pub label: String,
    #[serde(default)]
    pub url: Option<String>,
}

/// One rule inside a manifest. `body` is the item-⑥-shaped renderable content;
/// `examples` map to a `rule_examples` row when both sides are present.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackRule {
    /// Pack-namespaced rule id (e.g. `go-http-safety/413-body-limit`).
    pub id: String,
    pub title: String,
    /// `info` | `warning` | `error`. Becomes a `severity:<level>` tag so the
    /// rule participates in the existing severity weighting honestly.
    #[serde(default)]
    pub severity: Option<String>,
    /// Overrides `target.fileGlobs`. The strict-cascade gate that keeps a Go
    /// pack rule off a `.py` edit.
    #[serde(default)]
    pub file_globs: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    /// The rule body prose. Authored in the same `Rule:` / first-sentence
    /// directive shape the item-⑥ renderer parses, so the rendered code-spec is
    /// identical to a mined rule's.
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub examples: Option<PackRuleExamples>,
    /// Per-rule provenance, overrides the pack-level default.
    #[serde(default)]
    pub provenance: Option<PackRuleProvenance>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackRuleExamples {
    #[serde(default)]
    pub bad: Option<String>,
    #[serde(default)]
    pub good: Option<String>,
    /// Optional reviewer-style note; flows to `rule_examples.description`.
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackRuleProvenance {
    pub kind: String,
    #[serde(default)]
    pub attribution: Option<String>,
    #[serde(default)]
    pub source_url: Option<String>,
}

/// Hex `sha256` over the raw manifest bytes, used as the supply-chain integrity
/// check. The index pins this value; install recomputes it over the fetched
/// bytes and refuses on mismatch.
#[must_use]
pub fn manifest_sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_INDEX: &str = r#"{
        "schemaVersion": 1,
        "generatedAt": "2026-06-01T00:00:00Z",
        "packs": [
            {
                "id": "difflore/go-http-safety",
                "name": "Go HTTP handler safety",
                "latest": "1.0.0",
                "versions": {
                    "1.0.0": {
                        "manifest": "packs/difflore/go-http-safety/pack.json",
                        "sha256": "deadbeef",
                        "ruleCount": 6
                    }
                },
                "target": { "languages": ["go"], "frameworks": ["net/http"] },
                "maintainer": { "name": "DiffLore", "verified": true },
                "license": "CC-BY-4.0"
            }
        ]
    }"#;

    #[test]
    fn index_round_trips_and_finds_entry() {
        let index: PackIndex = serde_json::from_str(SAMPLE_INDEX).expect("parse index");
        assert_eq!(index.schema_version, 1);
        let entry = index.find("difflore/go-http-safety").expect("entry");
        assert_eq!(entry.name, "Go HTTP handler safety");
        assert_eq!(entry.latest, "1.0.0");
        let (resolved, version) = entry.resolve_version(None).expect("latest");
        assert_eq!(resolved, "1.0.0");
        assert_eq!(version.sha256, "deadbeef");
        assert_eq!(version.rule_count, Some(6));
    }

    #[test]
    fn resolve_version_pins_explicit_request() {
        let index: PackIndex = serde_json::from_str(SAMPLE_INDEX).expect("parse index");
        let entry = index.find("difflore/go-http-safety").expect("entry");
        assert!(entry.resolve_version(Some("9.9.9")).is_none());
        assert!(entry.resolve_version(Some("1.0.0")).is_some());
    }

    #[test]
    fn manifest_sha256_is_deterministic_hex() {
        let a = manifest_sha256(b"hello");
        let b = manifest_sha256(b"hello");
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
        assert_ne!(a, manifest_sha256(b"world"));
    }

    #[test]
    fn manifest_parses_minimal_pack() {
        let raw = r#"{
            "schemaVersion": 1,
            "id": "difflore/go-http-safety",
            "name": "Go HTTP handler safety",
            "version": "1.0.0",
            "target": { "languages": ["go"], "fileGlobs": ["**/*.go"] },
            "provenance": { "kind": "curated" },
            "rules": [
                {
                    "id": "go-http-safety/413-body-limit",
                    "title": "Return 413 when a request body exceeds the size limit",
                    "severity": "error",
                    "body": "Enforce a maximum request body size.",
                    "examples": { "bad": "x", "good": "y" }
                }
            ]
        }"#;
        let manifest: PackManifest = serde_json::from_str(raw).expect("parse manifest");
        assert_eq!(manifest.id, "difflore/go-http-safety");
        assert_eq!(manifest.rules.len(), 1);
        let rule = &manifest.rules[0];
        assert_eq!(rule.severity.as_deref(), Some("error"));
        assert_eq!(
            manifest.target.as_ref().unwrap().file_globs,
            vec!["**/*.go".to_owned()]
        );
    }
}
