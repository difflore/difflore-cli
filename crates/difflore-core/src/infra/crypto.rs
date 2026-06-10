use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit},
};
use rand::RngExt;
use sha2::{Digest, Sha256};
use std::sync::OnceLock;

static MASTER_KEY: OnceLock<Result<[u8; 32], String>> = OnceLock::new();

const KEYRING_SERVICE: &str = "difflore";
const KEYRING_USER: &str = "master-key-v2";

/// Retrieve or create a random master key stored in the OS credential store.
/// Falls back to a path-derived local key if keyring is unavailable outside CI.
fn get_or_create_master_key() -> Result<[u8; 32], String> {
    MASTER_KEY.get_or_init(|| {
        // Env override — primarily for testing on platforms where the OS
        // keyring is broken (Windows Credential Manager rejecting the
        // Generic credential scope, CI sandboxes without a keyring, etc).
        // Accepts 64-char hex (32 bytes).
        if let Some(hex) = crate::env::master_key_hex() {
            if let Ok(bytes) = from_hex(hex.trim())
                && bytes.len() == 32 {
                    let mut key = [0u8; 32];
                    key.copy_from_slice(&bytes);
                    return Ok(key);
                }
            eprintln!(
                "warning: DIFFLORE_MASTER_KEY is not a valid 64-character hex key; using the local keyring instead."
            );
        }

        match try_keyring_key() {
            Ok(key) => Ok(key),
            Err(err) => {
                // On CI (no user keyring, ephemeral FS), the path-derived
                // fallback key is unsafe: secrets encrypted with it are
                // unrecoverable on the next run (different HOME, different
                // hostname).  Force the user to supply DIFFLORE_MASTER_KEY
                // explicitly so they know state won't persist.
                if is_ci_environment() {
                    return Err(format!(
                        "OS keyring unavailable ({err}) and running on CI. \
                         Set DIFFLORE_MASTER_KEY=<64-char-hex> to persist encrypted state; \
                         refusing local fallback key derivation because it produces unrecoverable secrets on CI."
                    ));
                }
                eprintln!(
                    "warning: OS keyring unavailable; DiffLore will use local fallback encryption for stored secrets."
                );
                if crate::env::debug_providers() {
                    eprintln!("[crypto] keyring unavailable: {err}");
                }
                Ok(derive_local_fallback_key())
            }
        }
    }).clone()
}

/// True on common CI hosts, where the silent fallback key is refused because
/// encrypted state wouldn't survive the next run.
fn is_ci_environment() -> bool {
    const CI_ENV_FLAGS: &[&str] = &[
        "CI",
        "GITHUB_ACTIONS",
        "GITLAB_CI",
        "CIRCLECI",
        "BUILDKITE",
        "JENKINS_URL",
        "TRAVIS",
        "TEAMCITY_VERSION",
        "CODEBUILD_BUILD_ID",
    ];
    CI_ENV_FLAGS.iter().any(|k| crate::env::truthy(k))
}

fn try_keyring_key() -> Result<[u8; 32], String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)
        .map_err(|e| format!("keyring entry error: {e}"))?;

    match entry.get_password() {
        Ok(hex) => {
            if let Ok(bytes) = from_hex(&hex) {
                if bytes.len() == 32 {
                    let mut key = [0u8; 32];
                    key.copy_from_slice(&bytes);
                    return Ok(key);
                }
                if crate::env::debug_providers() {
                    eprintln!(
                        "[crypto] keyring: decoded bytes len={} (expected 32)",
                        bytes.len()
                    );
                }
            } else if crate::env::debug_providers() {
                eprintln!("[crypto] keyring: hex decode failed");
            }
        }
        Err(e) => {
            if crate::env::debug_providers() {
                eprintln!("[crypto] keyring get_password failed: {e}");
            }
        }
    }

    let mut key = [0u8; 32];
    rand::rng().fill(&mut key);
    let hex = to_hex(&key);
    entry
        .set_password(&hex)
        .map_err(|e| format!("keyring set error: {e}"))?;
    Ok(key)
}

/// Local fallback key derivation for machines without an OS keyring.
fn derive_local_fallback_key() -> [u8; 32] {
    let anchor = dirs::home_dir().map_or_else(
        || "difflore-fallback".to_owned(),
        |p| p.to_string_lossy().to_string(),
    );
    let mut hasher = Sha256::new();
    hasher.update(anchor.as_bytes());
    hasher.update(b"difflore-cloud-encryption-key-v1");
    hasher.finalize().into()
}

fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut acc, b| {
            let _ = write!(&mut acc, "{b:02x}");
            acc
        })
}

/// SHA-256 a byte slice as the prefixed digest string `"sha256:<lowercase-hex>"`.
/// Used by the MCP install manifest to hash the exact rendered config block, so
/// a later `agents update` can tell "unchanged since DiffLore wrote it" (safe to
/// upgrade) from "the human edited it" (must not clobber). Pure — never touches
/// files, the keyring, or repo state.
#[must_use]
pub fn sha256_block_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest: [u8; 32] = hasher.finalize().into();
    format!("sha256:{}", to_hex(&digest))
}

fn from_hex(hex: &str) -> Result<Vec<u8>, String> {
    if !hex.len().is_multiple_of(2) {
        return Err("odd-length hex string".into());
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).map_err(|e| e.to_string()))
        .collect()
}

fn try_decrypt_with_key(
    key_bytes: &[u8; 32],
    nonce_bytes: &[u8],
    ciphertext: &[u8],
) -> Result<Vec<u8>, ()> {
    let key = aes_gcm::Key::<Aes256Gcm>::from_slice(key_bytes);
    let cipher = Aes256Gcm::new(key);
    let nonce = Nonce::from_slice(nonce_bytes);
    cipher.decrypt(nonce, ciphertext).map_err(|_| ())
}

pub fn encrypt_secret(plaintext: &str) -> Result<String, String> {
    let key_bytes = get_or_create_master_key()?;
    let key = aes_gcm::Key::<Aes256Gcm>::from_slice(&key_bytes);
    let cipher = Aes256Gcm::new(key);

    let mut nonce_bytes = [0u8; 12];
    rand::rng().fill(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, plaintext.as_bytes())
        .map_err(|e| format!("encryption failed: {e}"))?;

    let mut combined = nonce_bytes.to_vec();
    combined.extend_from_slice(&ciphertext);
    Ok(to_hex(&combined))
}

/// Which key successfully decrypted a stored secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecryptOrigin {
    CurrentKey,
}

/// Decrypt a stored secret, also reporting which key generation
/// succeeded.
pub fn decrypt_secret_with_origin(hex_data: &str) -> Result<(String, DecryptOrigin), String> {
    let combined = from_hex(hex_data)?;
    if combined.len() < 13 {
        return Err("ciphertext too short".into());
    }
    let (nonce_bytes, ciphertext) = combined.split_at(12);
    let master_key = get_or_create_master_key()?;

    let plaintext = try_decrypt_with_key(&master_key, nonce_bytes, ciphertext)
        .map_err(|()| "decryption failed with current key".to_owned())?;
    String::from_utf8(plaintext)
        .map(|s| (s, DecryptOrigin::CurrentKey))
        .map_err(|e| format!("invalid utf8: {e}"))
}

/// Decrypt a stored secret. Thin wrapper over
/// [`decrypt_secret_with_origin`] that discards the key-generation
/// signal.
pub fn decrypt_secret(hex_data: &str) -> Result<String, String> {
    decrypt_secret_with_origin(hex_data).map(|(plaintext, _origin)| plaintext)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_codec_round_trip_and_invariants() {
        // Round-trip every byte value, asserting both the encoding shape
        // (lowercase pairs) and decoder tolerance for mixed case.
        let data: Vec<u8> = (0u8..=255).collect();
        let hex = to_hex(&data);
        assert_eq!(hex.len(), data.len() * 2);
        assert!(
            hex.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert_eq!(from_hex(&hex).unwrap(), data);

        // Targeted spot checks for empty / short / mixed-case inputs.
        assert_eq!(to_hex(&[]), "");
        assert_eq!(from_hex("").unwrap(), Vec::<u8>::new());
        assert_eq!(from_hex("DEADBEEF").unwrap(), vec![0xde, 0xad, 0xbe, 0xef]);

        // Reject odd-length / non-hex input.
        let err = from_hex("abc").unwrap_err();
        assert!(err.contains("odd-length"), "unexpected error: {err}");
        assert!(from_hex("zz").is_err());
        assert!(from_hex("gh").is_err());
    }

    #[test]
    fn sha256_block_hex_is_prefixed_stable_and_input_sensitive() {
        // Known-answer vector: SHA-256("") — anchors the prefix + canonical hex.
        assert_eq!(
            sha256_block_hex(b""),
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        // Deterministic for identical input (the whole point: same render →
        // same hash → "unchanged since DiffLore wrote it").
        let a = sha256_block_hex(br#"{"command":"difflore","args":["mcp-server"]}"#);
        let b = sha256_block_hex(br#"{"command":"difflore","args":["mcp-server"]}"#);
        assert_eq!(a, b);
        assert!(a.starts_with("sha256:"));
        // A single-byte edit must change the digest (no clobber-on-edit).
        assert_ne!(
            a,
            sha256_block_hex(br#"{"command":"difflore","args":["mcp-server2"]}"#)
        );
    }

    #[test]
    fn decrypt_secret_rejects_odd_length_hex_before_touching_keyring() {
        // Odd-length hex fails inside from_hex, which runs before any keyring access.
        let err = decrypt_secret("abc").unwrap_err();
        assert!(err.contains("odd-length"), "unexpected error: {err}");
    }

    #[test]
    fn decrypt_secret_rejects_too_short_ciphertext() {
        // 4 hex chars → 2 bytes, below the fast-path sanity floor (12-byte
        // nonce + at least one ciphertext byte = 13). Real AES-GCM payloads
        // also carry a 16-byte tag, so anything genuinely decryptable is
        // ≥ 28 bytes — but we let aes_gcm reject those itself; the early
        // 13-byte gate exists only to fail before the keyring is touched.
        let err = decrypt_secret("abcd").unwrap_err();
        assert!(err.contains("too short"), "unexpected error: {err}");
    }
}
