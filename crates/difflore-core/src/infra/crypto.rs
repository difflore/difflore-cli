use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit},
};
use rand::RngExt;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::OnceLock,
};

static MASTER_KEY: OnceLock<[u8; 32]> = OnceLock::new();

const KEYRING_SERVICE: &str = "difflore";
const KEYRING_USER: &str = "master-key-v2";
const KEYSEED_FILE: &str = "keyseed";
const KEYSEED_BYTES: usize = 32;
const LOCAL_FALLBACK_CONTEXT: &[u8] = b"difflore-local-fallback-master-key-v2";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyseedStatus {
    Present {
        path: PathBuf,
        permissions_ok: Option<bool>,
    },
    Missing {
        path: PathBuf,
    },
    Invalid {
        path: PathBuf,
        error: String,
        permissions_ok: Option<bool>,
    },
    Unreadable {
        path: PathBuf,
        error: String,
        permissions_ok: Option<bool>,
    },
    Unavailable {
        error: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MasterKeyStorageStatus {
    EnvOverride,
    KeyringReady,
    KeyringWillCreate,
    KeyringInvalid(String),
    LocalFallback {
        keyring_error: String,
        keyseed: KeyseedStatus,
    },
    CiRequiresExplicitKey {
        keyring_error: String,
    },
}

/// Retrieve or create a random master key stored in the OS credential store.
/// Falls back to a locally persisted random seed if keyring is unavailable
/// outside CI.
fn get_or_create_master_key() -> crate::Result<[u8; 32]> {
    if let Some(key) = MASTER_KEY.get() {
        return Ok(*key);
    }

    // Env override — primarily for testing on platforms where the OS
    // keyring is broken (Windows Credential Manager rejecting the
    // Generic credential scope, CI sandboxes without a keyring, etc).
    // Accepts 64-char hex (32 bytes).
    let key = if let Some(hex) = crate::infra::env::master_key_hex() {
        if let Ok(bytes) = from_hex(hex.trim())
            && bytes.len() == 32
        {
            let mut key = [0u8; 32];
            key.copy_from_slice(&bytes);
            key
        } else {
            eprintln!(
                "warning: DIFFLORE_MASTER_KEY is not a valid 64-character hex key; using the local keyring instead."
            );
            master_key_from_keyring_result(try_keyring_key())?
        }
    } else {
        master_key_from_keyring_result(try_keyring_key())?
    };

    let _ = MASTER_KEY.set(key);
    Ok(*MASTER_KEY.get().unwrap_or(&key))
}

pub fn probe_master_key_storage() -> MasterKeyStorageStatus {
    if let Some(hex) = crate::infra::env::master_key_hex()
        && parse_32_byte_hex(hex.trim()).is_ok()
    {
        return MasterKeyStorageStatus::EnvOverride;
    }

    let entry = match keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER) {
        Ok(entry) => entry,
        Err(e) => {
            let keyring_error = format!("keyring entry error: {e}");
            return fallback_storage_status(keyring_error);
        }
    };
    match entry.get_password() {
        Ok(hex) => match parse_32_byte_hex(&hex) {
            Ok(_) => MasterKeyStorageStatus::KeyringReady,
            Err(e) => MasterKeyStorageStatus::KeyringInvalid(e.to_string()),
        },
        Err(keyring::Error::NoEntry) => MasterKeyStorageStatus::KeyringWillCreate,
        Err(e) => fallback_storage_status(e.to_string()),
    }
}

pub fn local_fallback_keyseed_status() -> KeyseedStatus {
    match local_keyseed_path() {
        Ok(path) => probe_keyseed_path(&path),
        Err(error) => KeyseedStatus::Unavailable {
            error: error.to_string(),
        },
    }
}

fn fallback_storage_status(keyring_error: String) -> MasterKeyStorageStatus {
    if is_ci_environment() {
        MasterKeyStorageStatus::CiRequiresExplicitKey { keyring_error }
    } else {
        MasterKeyStorageStatus::LocalFallback {
            keyring_error,
            keyseed: local_fallback_keyseed_status(),
        }
    }
}

fn master_key_from_keyring_result(
    keyring_result: crate::Result<[u8; 32]>,
) -> crate::Result<[u8; 32]> {
    match keyring_result {
        Ok(key) => Ok(key),
        Err(err) => local_fallback_key_for_keyring_error(&err.to_string()),
    }
}

fn local_fallback_key_for_keyring_error(err: &str) -> crate::Result<[u8; 32]> {
    // On CI (no user keyring, ephemeral FS), even a keyseed-backed local
    // fallback can disappear between runs. Force the user to supply
    // DIFFLORE_MASTER_KEY explicitly so they know state will persist.
    if is_ci_environment() {
        return Err(format!(
            "OS keyring unavailable ({err}) and running on CI. \
             Set DIFFLORE_MASTER_KEY=<64-char-hex> to persist encrypted state; \
             refusing local fallback key derivation because CI storage is often ephemeral."
        )
        .into());
    }
    let key = derive_local_fallback_key().map_err(|seed_err| {
        format!("OS keyring unavailable ({err}) and local fallback keyseed unavailable: {seed_err}")
    })?;
    eprintln!(
        "warning: OS keyring unavailable; DiffLore will use local fallback encryption backed by ~/.difflore/keyseed for stored secrets."
    );
    if crate::infra::env::debug_providers() {
        eprintln!("[crypto] keyring unavailable: {err}");
    }
    Ok(key)
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
    CI_ENV_FLAGS.iter().any(|k| crate::infra::env::truthy(k))
}

fn try_keyring_key() -> crate::Result<[u8; 32]> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)
        .map_err(|e| format!("keyring entry error: {e}"))?;

    match entry.get_password() {
        Ok(hex) => {
            if let Ok(key) = parse_32_byte_hex(&hex) {
                return Ok(key);
            }
            if let Ok(bytes) = from_hex(&hex) {
                if crate::infra::env::debug_providers() {
                    eprintln!(
                        "[crypto] keyring: decoded bytes len={} (expected 32)",
                        bytes.len()
                    );
                }
            } else if crate::infra::env::debug_providers() {
                eprintln!("[crypto] keyring: hex decode failed");
            }
        }
        Err(e) => {
            if crate::infra::env::debug_providers() {
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
fn derive_local_fallback_key() -> crate::Result<[u8; 32]> {
    let seed = load_or_create_local_keyseed()?;
    Ok(derive_local_fallback_key_from_seed(
        &seed,
        &local_fallback_context(),
    ))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LocalFallbackContext {
    home_anchor: String,
    machine_id: Option<String>,
}

fn local_fallback_context() -> LocalFallbackContext {
    let anchor = dirs::home_dir().map_or_else(
        || "difflore-fallback".to_owned(),
        |p| p.to_string_lossy().to_string(),
    );
    LocalFallbackContext {
        home_anchor: anchor,
        machine_id: read_machine_id(),
    }
}

fn derive_local_fallback_key_from_seed(
    seed: &[u8; KEYSEED_BYTES],
    context: &LocalFallbackContext,
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(LOCAL_FALLBACK_CONTEXT);
    hasher.update(seed);
    hasher.update(context.home_anchor.as_bytes());
    if let Some(machine_id) = context.machine_id.as_deref() {
        hasher.update(b"\0machine-id\0");
        hasher.update(machine_id.as_bytes());
    }
    hasher.finalize().into()
}

fn read_machine_id() -> Option<String> {
    ["/etc/machine-id", "/var/lib/dbus/machine-id"]
        .iter()
        .filter_map(|path| fs::read_to_string(path).ok())
        .map(|raw| raw.trim().to_owned())
        .find(|id| !id.is_empty())
}

fn local_keyseed_path() -> crate::Result<PathBuf> {
    Ok(crate::infra::paths::config_home()?.join(KEYSEED_FILE))
}

fn load_or_create_local_keyseed() -> crate::Result<[u8; KEYSEED_BYTES]> {
    load_or_create_local_keyseed_at(&local_keyseed_path()?)
}

fn load_or_create_local_keyseed_at(path: &Path) -> crate::Result<[u8; KEYSEED_BYTES]> {
    match read_existing_keyseed(path) {
        Ok(seed) => return Ok(seed),
        Err(KeyseedReadError::NotFound) => {}
        Err(KeyseedReadError::Invalid(e) | KeyseedReadError::Io(e)) => return Err(e.into()),
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| {
            format!(
                "could not create keyseed directory {}: {e}",
                parent.display()
            )
        })?;
        crate::infra::db::restrict_to_owner(parent, true);
    }

    let mut seed = [0u8; KEYSEED_BYTES];
    rand::rng().fill(&mut seed);
    match write_new_keyseed(path, &seed) {
        Ok(()) => Ok(seed),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            let seed = read_existing_keyseed(path).map_err(KeyseedReadError::into_message)?;
            crate::infra::db::restrict_to_owner(path, false);
            Ok(seed)
        }
        Err(e) => Err(format!("could not create keyseed {}: {e}", path.display()).into()),
    }
}

fn write_new_keyseed(path: &Path, seed: &[u8; KEYSEED_BYTES]) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(to_hex(seed).as_bytes())?;
    file.sync_all()?;
    crate::infra::db::restrict_to_owner(path, false);
    Ok(())
}

enum KeyseedReadError {
    NotFound,
    Invalid(String),
    Io(String),
}

impl KeyseedReadError {
    fn into_message(self) -> String {
        match self {
            Self::NotFound => "keyseed not found".to_owned(),
            Self::Invalid(e) | Self::Io(e) => e,
        }
    }
}

fn read_existing_keyseed(path: &Path) -> Result<[u8; KEYSEED_BYTES], KeyseedReadError> {
    crate::infra::db::restrict_to_owner(path, false);
    match fs::read_to_string(path) {
        Ok(raw) => parse_keyseed_hex(&raw).map_err(|e| {
            KeyseedReadError::Invalid(format!("invalid keyseed {}: {e}", path.display()))
        }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Err(KeyseedReadError::NotFound),
        Err(e) => Err(KeyseedReadError::Io(format!(
            "could not read keyseed {}: {e}",
            path.display()
        ))),
    }
}

fn parse_keyseed_hex(raw: &str) -> crate::Result<[u8; KEYSEED_BYTES]> {
    let trimmed = raw.trim();
    if trimmed.len() != KEYSEED_BYTES * 2 {
        return Err(format!(
            "expected {} lowercase hex characters, got {}",
            KEYSEED_BYTES * 2,
            trimmed.len()
        )
        .into());
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    {
        return Err("expected lowercase hex keyseed".to_owned().into());
    }
    parse_32_byte_hex(trimmed)
}

fn probe_keyseed_path(path: &Path) -> KeyseedStatus {
    let permissions_ok = keyseed_permissions_ok(path);
    match fs::read_to_string(path) {
        Ok(raw) => match parse_keyseed_hex(&raw) {
            Ok(_) => KeyseedStatus::Present {
                path: path.to_path_buf(),
                permissions_ok,
            },
            Err(error) => KeyseedStatus::Invalid {
                path: path.to_path_buf(),
                error: error.to_string(),
                permissions_ok,
            },
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => KeyseedStatus::Missing {
            path: path.to_path_buf(),
        },
        Err(e) => KeyseedStatus::Unreadable {
            path: path.to_path_buf(),
            error: e.to_string(),
            permissions_ok,
        },
    }
}

#[cfg(unix)]
fn keyseed_permissions_ok(path: &Path) -> Option<bool> {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path)
        .ok()
        .map(|meta| meta.permissions().mode() & 0o777 == 0o600)
}

#[cfg(not(unix))]
fn keyseed_permissions_ok(_path: &Path) -> Option<bool> {
    None
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

fn from_hex(hex: &str) -> crate::Result<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return Err("odd-length hex string".into());
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&hex[i..i + 2], 16)
                .map_err(|e| crate::CoreError::Internal(e.to_string()))
        })
        .collect()
}

fn parse_32_byte_hex(hex: &str) -> crate::Result<[u8; 32]> {
    let bytes = from_hex(hex)?;
    if bytes.len() != 32 {
        return Err(format!("decoded {} bytes, expected 32", bytes.len()).into());
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&bytes);
    Ok(key)
}

fn try_decrypt_with_key(
    key_bytes: &[u8; 32],
    nonce_bytes: &[u8],
    ciphertext: &[u8],
) -> crate::Result<Vec<u8>, ()> {
    let key = aes_gcm::Key::<Aes256Gcm>::from_slice(key_bytes);
    let cipher = Aes256Gcm::new(key);
    let nonce = Nonce::from_slice(nonce_bytes);
    cipher.decrypt(nonce, ciphertext).map_err(|_| ())
}

pub fn encrypt_secret(plaintext: &str) -> crate::Result<String> {
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
pub fn decrypt_secret_with_origin(hex_data: &str) -> crate::Result<(String, DecryptOrigin)> {
    let combined = from_hex(hex_data)?;
    if combined.len() < 13 {
        return Err("ciphertext too short".into());
    }
    let (nonce_bytes, ciphertext) = combined.split_at(12);
    let master_key = get_or_create_master_key()?;

    let plaintext = try_decrypt_with_key(&master_key, nonce_bytes, ciphertext)
        .map_err(|()| "decryption failed with current key".to_owned())?;
    Ok(String::from_utf8(plaintext)
        .map(|s| (s, DecryptOrigin::CurrentKey))
        .map_err(|e| format!("invalid utf8: {e}"))?)
}

/// Decrypt a stored secret. Thin wrapper over
/// [`decrypt_secret_with_origin`] that discards the key-generation
/// signal.
pub fn decrypt_secret(hex_data: &str) -> crate::Result<String> {
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
        let err = from_hex("abc").unwrap_err().to_string();
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
        let err = decrypt_secret("abc").unwrap_err().to_string();
        assert!(err.contains("odd-length"), "unexpected error: {err}");
    }

    #[test]
    fn decrypt_secret_rejects_too_short_ciphertext() {
        // 4 hex chars → 2 bytes, below the fast-path sanity floor (12-byte
        // nonce + at least one ciphertext byte = 13). Real AES-GCM payloads
        // also carry a 16-byte tag, so anything genuinely decryptable is
        // ≥ 28 bytes — but we let aes_gcm reject those itself; the early
        // 13-byte gate exists only to fail before the keyring is touched.
        let err = decrypt_secret("abcd").unwrap_err().to_string();
        assert!(err.contains("too short"), "unexpected error: {err}");
    }

    #[test]
    fn local_fallback_key_changes_with_seed_even_for_same_home() {
        let context = LocalFallbackContext {
            home_anchor: "/home/alice".to_owned(),
            machine_id: Some("machine-a".to_owned()),
        };
        let mut seed_a = [0u8; KEYSEED_BYTES];
        seed_a[0] = 1;
        let mut seed_b = [0u8; KEYSEED_BYTES];
        seed_b[0] = 2;

        assert_ne!(
            derive_local_fallback_key_from_seed(&seed_a, &context),
            derive_local_fallback_key_from_seed(&seed_b, &context)
        );
    }

    #[test]
    fn local_fallback_key_is_stable_for_same_seed_and_context() {
        let context = LocalFallbackContext {
            home_anchor: "/home/alice".to_owned(),
            machine_id: None,
        };
        let seed = [7u8; KEYSEED_BYTES];

        assert_eq!(
            derive_local_fallback_key_from_seed(&seed, &context),
            derive_local_fallback_key_from_seed(&seed, &context)
        );
    }

    #[test]
    fn keyseed_creation_writes_lowercase_hex_and_owner_only_permissions() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(KEYSEED_FILE);

        let seed = load_or_create_local_keyseed_at(&path).expect("create keyseed");
        let raw = fs::read_to_string(&path).expect("read keyseed");

        assert_eq!(raw.len(), KEYSEED_BYTES * 2);
        assert!(
            raw.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "keyseed must be lowercase hex: {raw}"
        );
        assert_eq!(parse_keyseed_hex(&raw).unwrap(), seed);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "keyseed should be 0600");
        }
    }

    #[test]
    fn existing_keyseed_is_read_stably() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(KEYSEED_FILE);
        let seed = [9u8; KEYSEED_BYTES];
        fs::write(&path, to_hex(&seed)).unwrap();

        assert_eq!(load_or_create_local_keyseed_at(&path).unwrap(), seed);
        assert_eq!(load_or_create_local_keyseed_at(&path).unwrap(), seed);
    }

    #[test]
    fn invalid_keyseed_format_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(KEYSEED_FILE);
        fs::write(&path, "abc").unwrap();

        let err = load_or_create_local_keyseed_at(&path)
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid keyseed"), "unexpected error: {err}");
        assert!(err.contains("expected 64"), "unexpected error: {err}");
    }

    #[test]
    fn keyring_success_returns_key_without_fallback() {
        let key = [42u8; 32];

        assert_eq!(master_key_from_keyring_result(Ok(key)).unwrap(), key);
    }
}
