//! Registry transport for rule packs: fetch `index.json` and a pack's
//! `pack.json` over HTTPS (short timeout + small redirect cap) or from a
//! `file://` path for tests / air-gapped install. No DiffLore Cloud
//! dependency — install is a pure GET of public content.

use std::time::Duration;

use crate::packs::manifest::{PackIndex, PackManifest, manifest_sha256};

/// Raw GitHub content of the registry repo's default branch. The `--registry`
/// CLI flag overrides this with a fork, a private mirror, or a `file://` path.
pub const DEFAULT_PACK_REGISTRY: &str =
    "https://raw.githubusercontent.com/difflore/rule-packs/main";

/// HTTP request timeout. Short, matching the cloud client's posture — a hung
/// registry must not stall `packs list`.
const FETCH_TIMEOUT: Duration = Duration::from_secs(20);

/// Cap redirects so a malicious registry can't bounce the client around.
const MAX_REDIRECTS: usize = 4;

#[derive(Debug)]
pub enum PackFetchError {
    /// The registry base URL or a derived path was malformed.
    BadUrl(String),
    /// Could not build the HTTP client or reach the registry.
    Transport(String),
    /// The registry returned a non-success HTTP status.
    Status { url: String, status: u16 },
    /// A `file://` registry path could not be read.
    Io(String),
    /// The fetched bytes did not parse as the expected JSON shape.
    Parse(String),
    /// The fetched manifest's `sha256` did not match the index pin.
    IntegrityMismatch { expected: String, actual: String },
}

impl std::fmt::Display for PackFetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadUrl(m) => write!(f, "invalid registry URL: {m}"),
            Self::Transport(m) => write!(f, "could not reach registry: {m}"),
            Self::Status { url, status } => {
                write!(f, "registry returned HTTP {status} for {url}")
            }
            Self::Io(m) => write!(f, "could not read local registry path: {m}"),
            Self::Parse(m) => write!(f, "registry payload did not parse: {m}"),
            Self::IntegrityMismatch { expected, actual } => write!(
                f,
                "pack manifest failed integrity check (sha256 expected {expected}, got {actual}) \
                 — refusing to install"
            ),
        }
    }
}

impl std::error::Error for PackFetchError {}

/// Whether the registry base points at a local `file://` path rather than
/// an HTTP(S) endpoint. The live fetch path inlines this check; this named
/// predicate documents the contract.
#[allow(dead_code)]
fn is_file_registry(base: &str) -> bool {
    base.starts_with("file://")
}

/// Join a registry base URL with a relative path, normalising the single slash
/// between them. Works for both HTTP bases and `file://` bases.
fn join_url(base: &str, rel: &str) -> String {
    format!(
        "{}/{}",
        base.trim_end_matches('/'),
        rel.trim_start_matches('/')
    )
}

/// Read raw bytes from either an HTTP(S) URL or a `file://` path.
async fn get_bytes(url: &str) -> Result<Vec<u8>, PackFetchError> {
    if let Some(path) = url.strip_prefix("file://") {
        // Tolerate the Windows `file:///C:/...` shape: strip a single leading
        // slash that precedes a drive letter so `C:/...` round-trips.
        let path = path
            .strip_prefix('/')
            .filter(|p| p.as_bytes().get(1) == Some(&b':'))
            .unwrap_or(path);
        return tokio::fs::read(path)
            .await
            .map_err(|e| PackFetchError::Io(format!("{path}: {e}")));
    }

    let client = reqwest::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .redirect(reqwest::redirect::Policy::limited(MAX_REDIRECTS))
        .build()
        .map_err(|e| PackFetchError::Transport(format!("could not build HTTP client: {e}")))?;
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| PackFetchError::Transport(e.to_string()))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(PackFetchError::Status {
            url: url.to_owned(),
            status: status.as_u16(),
        });
    }
    resp.bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|e| PackFetchError::Transport(e.to_string()))
}

/// Fetch and parse the registry `index.json`.
pub async fn fetch_index(registry_base: &str) -> Result<PackIndex, PackFetchError> {
    let base = registry_base.trim();
    if base.is_empty() {
        return Err(PackFetchError::BadUrl("empty registry base".to_owned()));
    }
    let url = join_url(base, "index.json");
    let bytes = get_bytes(&url).await?;
    serde_json::from_slice(&bytes).map_err(|e| PackFetchError::Parse(format!("index.json: {e}")))
}

/// Fetch a pack `pack.json`, verify its `sha256` against the index pin, and
/// parse it. `manifest_rel` is the index-declared path; `expected_sha256` is the
/// pin. Refuses to return a manifest whose bytes don't match the pin.
pub async fn fetch_manifest(
    registry_base: &str,
    manifest_rel: &str,
    expected_sha256: &str,
) -> Result<PackManifest, PackFetchError> {
    let base = registry_base.trim();
    if base.is_empty() {
        return Err(PackFetchError::BadUrl("empty registry base".to_owned()));
    }
    let url = join_url(base, manifest_rel);
    let bytes = get_bytes(&url).await?;

    // Supply-chain guard: recompute over the fetched bytes and refuse on
    // mismatch BEFORE parsing, so a tampered manifest never reaches install.
    let actual = manifest_sha256(&bytes);
    let expected = expected_sha256.trim().to_ascii_lowercase();
    if !expected.is_empty() && actual != expected {
        return Err(PackFetchError::IntegrityMismatch { expected, actual });
    }

    serde_json::from_slice(&bytes)
        .map_err(|e| PackFetchError::Parse(format!("{manifest_rel}: {e}")))
}

/// Whether a `--registry` override points at the first-party default. Callers
/// use this to demote a `maintainer.verified` badge to "verified (custom
/// registry)" so the trust signal is never misleading.
#[must_use]
pub fn is_default_registry(registry_base: &str) -> bool {
    registry_base.trim().trim_end_matches('/') == DEFAULT_PACK_REGISTRY
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_url_normalises_slashes() {
        assert_eq!(
            join_url("https://example.com/reg/", "/index.json"),
            "https://example.com/reg/index.json"
        );
        assert_eq!(
            join_url("https://example.com/reg", "packs/a/pack.json"),
            "https://example.com/reg/packs/a/pack.json"
        );
    }

    #[test]
    fn detects_file_and_default_registries() {
        assert!(is_file_registry("file:///tmp/reg"));
        assert!(!is_file_registry("https://example.com"));
        assert!(is_default_registry(DEFAULT_PACK_REGISTRY));
        assert!(is_default_registry(&format!("{DEFAULT_PACK_REGISTRY}/")));
        assert!(!is_default_registry("https://example.com/fork"));
    }

    #[tokio::test]
    async fn file_registry_round_trips_index() {
        let dir = tempfile::tempdir().expect("tempdir");
        let index_path = dir.path().join("index.json");
        std::fs::write(
            &index_path,
            r#"{"schemaVersion":1,"packs":[{"id":"x/y","name":"Y","latest":"1.0.0","versions":{}}]}"#,
        )
        .expect("write");
        let base = format!("file://{}", dir.path().display());
        let index = fetch_index(&base).await.expect("fetch index");
        assert_eq!(index.packs.len(), 1);
        assert_eq!(index.packs[0].id, "x/y");
    }

    #[tokio::test]
    async fn manifest_integrity_mismatch_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let raw = r#"{"schemaVersion":1,"id":"x/y","name":"Y","version":"1.0.0","rules":[]}"#;
        std::fs::write(dir.path().join("pack.json"), raw).expect("write");
        let base = format!("file://{}", dir.path().display());
        // Wrong pin -> refused.
        let err = fetch_manifest(&base, "pack.json", "0000")
            .await
            .expect_err("should refuse");
        assert!(matches!(err, PackFetchError::IntegrityMismatch { .. }));
        // Correct pin -> parses.
        let good = manifest_sha256(raw.as_bytes());
        let manifest = fetch_manifest(&base, "pack.json", &good)
            .await
            .expect("fetch manifest");
        assert_eq!(manifest.id, "x/y");
    }
}
