//! Wire protocol between the `difflore-hook` shim binary and the hook
//! forwarder. This is the protocol's only definition: the shim's blocking
//! client (`bin/difflore-hook.rs`) and the in-process async client/server
//! ([`super`]) both speak exactly these shapes over the local socket, so the
//! two sides cannot drift apart.
//!
//! Transport is one NDJSON line per direction over a cross-platform local
//! socket (`interprocess` maps the same path to a Unix-domain socket on Unix
//! and a named-pipe equivalent on Windows).

use std::io::{BufRead as _, Write as _};

use interprocess::local_socket::Stream as BlockingStream;
use interprocess::local_socket::traits::Stream as _;
#[cfg(not(windows))]
use interprocess::local_socket::{GenericFilePath, ToFsName as _};
#[cfg(windows)]
use interprocess::local_socket::{GenericNamespaced, ToNsName as _};
use serde::{Deserialize, Serialize};

/// Env var selecting the forward mode (`auto` / `always` / `never`).
pub const ENV: &str = difflore_core::infra::env::DIFFLORE_HOOK_FORWARD;

/// Wire protocol version between the shim and the warm hook daemon.
pub const PROTOCOL_VERSION: u16 = 1;

/// Binary version expected on both sides of the hook-forward IPC.
pub const BINARY_VERSION: &str = env!("CARGO_PKG_VERSION");

const INCOMPATIBLE_FORWARDER_PREFIX: &str = "incompatible hook forwarder protocol";

/// Hard cap on a single request or response payload. A hostile or runaway
/// producer must not be able to OOM either side of the socket.
pub const MAX_IPC_BYTES: u64 = 16 * 1024 * 1024;

/// The neutral "do nothing" hook output every supported client accepts.
pub const NOOP_OUTPUT: &str = r#"{"continue":true}"#;

/// Version guard carried by every request and response. It lets a freshly
/// upgraded shim reject a still-running daemon from an older install before it
/// trusts the daemon's hook output.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProtocolGuard {
    pub protocol_version: u16,
    pub binary_version: String,
}

impl ProtocolGuard {
    #[must_use]
    pub fn current() -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            binary_version: BINARY_VERSION.to_owned(),
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.protocol_version != PROTOCOL_VERSION {
            return Err(incompatible_forwarder_error(format!(
                "expected protocol {}, got {}",
                PROTOCOL_VERSION, self.protocol_version
            )));
        }
        if self.binary_version != BINARY_VERSION {
            return Err(incompatible_forwarder_error(format!(
                "expected binary version {}, got {}",
                BINARY_VERSION, self.binary_version
            )));
        }
        Ok(())
    }
}

/// Actual hook payload nested under the version guard. Nesting is intentional:
/// older daemons do not understand this shape and will reject it before doing
/// hook work, instead of silently ignoring an added top-level field.
#[derive(Debug, Serialize, Deserialize)]
pub struct RequestPayload {
    pub client: String,
    pub raw: String,
}

/// One forwarded hook invocation: the client adapter to use plus the raw
/// stdin payload the IDE handed the hook.
#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    pub protocol: ProtocolGuard,
    pub payload: RequestPayload,
}

/// Forwarder reply. `ok == true` carries the exact hook stdout in `output`;
/// `ok == false` carries a human-readable `error` and the caller falls back
/// to in-process dispatch.
#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<ProtocolGuard>,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    #[must_use]
    pub fn ok(output: String) -> Self {
        Self {
            protocol: Some(ProtocolGuard::current()),
            ok: true,
            output: Some(output),
            error: None,
        }
    }

    #[must_use]
    pub fn error(error: impl Into<String>) -> Self {
        Self {
            protocol: Some(ProtocolGuard::current()),
            ok: false,
            output: None,
            error: Some(error.into()),
        }
    }
}

#[must_use]
pub fn incompatible_forwarder_error(detail: impl AsRef<str>) -> String {
    format!("{INCOMPATIBLE_FORWARDER_PREFIX}: {}", detail.as_ref())
}

#[must_use]
pub fn is_incompatible_forwarder_error(error: &str) -> bool {
    error.contains(INCOMPATIBLE_FORWARDER_PREFIX)
}

/// Forward-mode knob read from [`ENV`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Auto,
    Always,
    Never,
}

impl Mode {
    #[must_use]
    pub fn from_env() -> Self {
        let value = difflore_core::infra::env::var(ENV);
        Self::from_env_value(value.as_deref())
    }

    fn from_env_value(value: Option<&str>) -> Self {
        let Some(value) = value else {
            return Self::default_for_platform();
        };
        match value.to_ascii_lowercase().as_str() {
            "always" => Self::Always,
            "auto" => Self::Auto,
            "never" | "off" | "0" | "false" => Self::Never,
            _ => Self::default_for_platform(),
        }
    }

    const fn default_for_platform() -> Self {
        Self::Auto
    }
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Auto => write!(f, "auto"),
            Self::Always => write!(f, "always"),
            Self::Never => write!(f, "never"),
        }
    }
}

/// Conservative read timeout (ms) for the shim's blocking round-trip. A warm
/// daemon answers in single-digit milliseconds; this only bounds the *bad*
/// case where a daemon is alive (socket connectable) but its handler is wedged,
/// so the shim falls back in-process instead of stalling the user's edit.
pub const BLOCKING_READ_TIMEOUT_MS: u64 = 3_000;

/// Local-socket endpoint for a specific project hash. Each repo and binary
/// version gets its own socket so a freshly upgraded shim never talks to a
/// still-running older daemon. The file lives in the data-home *root* (not
/// under `projects/{hash}/`) to keep the Unix `sun_path` short (~104-byte
/// limit). On Windows the returned path is a logical endpoint; the basename is
/// mapped to a local named pipe via [`GenericNamespaced`]. Honours
/// `DIFFLORE_HOME` via the core data-home resolver so tests and sandboxes
/// redirect the socket together with the rest of the data dir.
pub fn endpoint_for_hash(project_hash: &str) -> Result<std::path::PathBuf, String> {
    Ok(difflore_core::infra::paths::data_home()
        .map_err(|e| e.to_string())?
        .join(endpoint_file_name(project_hash)))
}

fn endpoint_file_name(project_hash: &str) -> String {
    format!(
        "hook-forward-{project_hash}-p{}-b{}.sock",
        PROTOCOL_VERSION,
        endpoint_safe_binary_version()
    )
}

fn endpoint_safe_binary_version() -> String {
    BINARY_VERSION
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect()
}

/// Endpoint for the project containing the current working directory. Shim and
/// server both derive the hash from the same `current_project_root` →
/// `project_hash_from_root` pipeline, so they agree on the socket name without
/// putting `cwd` on the wire.
pub fn endpoint_for_current_project() -> Result<std::path::PathBuf, String> {
    endpoint_for_hash(&current_project_hash())
}

/// Stable per-project hash for the current working directory. Same derivation
/// the daemon receives via `--project-hash`, so the shim's connect target and
/// the daemon's frozen index pool refer to the same project.
#[must_use]
pub fn current_project_hash() -> String {
    let root = difflore_core::infra::db::current_project_root();
    difflore_core::infra::db::project_hash_from_root(&root)
}

/// Encode a request as the one NDJSON line the server expects.
pub fn encode_request_line(client: &str, raw: &str) -> Result<String, String> {
    let req = Request {
        protocol: ProtocolGuard::current(),
        payload: RequestPayload {
            client: client.to_owned(),
            raw: raw.to_owned(),
        },
    };
    serde_json::to_string(&req)
        .map(|line| line + "\n")
        .map_err(|e| e.to_string())
}

/// Decode one NDJSON response line into the hook output, or the forwarder's
/// error message for the caller to handle (typically: fall back in-process).
pub fn decode_response_line(line: &str) -> Result<String, String> {
    let response: Response = serde_json::from_str(line.trim()).map_err(|e| e.to_string())?;
    match response.protocol {
        Some(protocol) => protocol.validate()?,
        None => return Err(incompatible_forwarder_error("missing protocol guard")),
    }
    if response.ok {
        Ok(response.output.unwrap_or_else(|| NOOP_OUTPUT.to_owned()))
    } else {
        Err(response
            .error
            .unwrap_or_else(|| "hook forwarder returned an unknown error".to_owned()))
    }
}

/// Remove the current project's socket path so an incompatible live daemon can
/// be replaced by a fresh one. Best-effort: on Unix an already-bound old daemon
/// keeps its unlinked listener until it idles out; future shims hit the new
/// socket. On Windows / missing paths this is simply a no-op.
pub fn remove_current_project_socket_best_effort() {
    if let Ok(path) = endpoint_for_current_project() {
        let _ = std::fs::remove_file(path);
    }
}

/// Connect (blocking) to a daemon serving `project_hash`, returning the live
/// stream. Errors carry the OS [`io::ErrorKind`] so the single-instance probe
/// can distinguish "no socket file" (`NotFound`) from "file present, nobody
/// listening" (`ConnectionRefused`) — both mean "safe to (re)bind", but the
/// distinction is useful in traces and tests.
pub fn connect_blocking_for_hash(project_hash: &str) -> std::io::Result<BlockingStream> {
    let path = endpoint_for_hash(project_hash).map_err(std::io::Error::other)?;
    let name = socket_name_from_endpoint(&path)?;
    BlockingStream::connect(name)
}

#[cfg(windows)]
pub(super) fn socket_name_from_endpoint(
    endpoint: &std::path::Path,
) -> std::io::Result<interprocess::local_socket::Name<'_>> {
    let Some(file_name) = endpoint.file_name() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "hook endpoint is missing a pipe name",
        ));
    };
    file_name
        .to_os_string()
        .to_ns_name::<GenericNamespaced>()
        .map_err(|e| std::io::Error::other(e.to_string()))
}

#[cfg(not(windows))]
pub(super) fn socket_name_from_endpoint(
    endpoint: &std::path::Path,
) -> std::io::Result<interprocess::local_socket::Name<'_>> {
    endpoint
        .to_fs_name::<GenericFilePath>()
        .map_err(|e| std::io::Error::other(e.to_string()))
}

fn read_response_line_blocking<R: std::io::Read>(reader: R) -> Result<String, String> {
    let mut response = String::new();
    let bytes_read = std::io::BufReader::new(reader.take(MAX_IPC_BYTES + 1))
        .read_line(&mut response)
        .map_err(|e| e.to_string())?;

    if bytes_read == 0 || response.trim().is_empty() {
        return Err("hook forwarder returned an empty response".to_owned());
    }

    if response.len() as u64 > MAX_IPC_BYTES {
        return Err(format!(
            "hook forwarder response exceeded {MAX_IPC_BYTES} bytes"
        ));
    }

    Ok(response)
}

/// Synchronous socket round-trip for the shim binary: connect to the daemon
/// serving the current project, write the request line, read one
/// length-capped NDJSON response line. Kept blocking so the shim needs no
/// runtime when the warm path is available.
///
/// The read is bounded by [`BLOCKING_READ_TIMEOUT_MS`] via a watchdog thread:
/// the blocking trait offers no per-fd read timeout, and a wedged daemon
/// (connectable socket, stuck handler) must not stall the user's edit. On
/// timeout we return `Err` so the caller falls back in-process; the orphaned
/// reader thread is harmless since the shim process exits immediately after.
pub fn ipc_roundtrip_blocking(request_line: &str) -> Result<String, String> {
    let hash = current_project_hash();
    let mut stream = connect_blocking_for_hash(&hash).map_err(|e| e.to_string())?;
    stream
        .write_all(request_line.as_bytes())
        .map_err(|e| e.to_string())?;
    stream.flush().map_err(|e| e.to_string())?;

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = read_response_line_blocking(stream);
        // Receiver may already be gone (timeout fired); ignore the send error.
        let _ = tx.send(result);
    });

    match rx.recv_timeout(std::time::Duration::from_millis(BLOCKING_READ_TIMEOUT_MS)) {
        Ok(Ok(response)) => Ok(response),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("hook forwarder read timed out".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_round_trips_ok_and_error_shapes() {
        // The shim and the forwarder must agree on the exact wire shape; pin
        // both branches of the decode here.
        let ok_line =
            serde_json::to_string(&Response::ok("{\"context\":\"x\"}".to_owned())).unwrap();
        assert_eq!(
            decode_response_line(&ok_line).unwrap(),
            "{\"context\":\"x\"}"
        );

        let err_line = serde_json::to_string(&Response::error("boom")).unwrap();
        assert_eq!(decode_response_line(&err_line).unwrap_err(), "boom");
    }

    #[test]
    fn ok_response_without_output_degrades_to_noop() {
        // A forwarder that replies ok with no payload must still hand the
        // client a valid hook output, not an empty string.
        let line = serde_json::to_string(&Response {
            protocol: Some(ProtocolGuard::current()),
            ok: true,
            output: None,
            error: None,
        })
        .unwrap();
        assert_eq!(decode_response_line(&line).unwrap(), NOOP_OUTPUT);
    }

    #[test]
    fn request_line_is_single_line_ndjson() {
        let line = encode_request_line("claude-code", "{\"hook\":\"x\"}").unwrap();
        assert!(line.ends_with('\n'));
        assert_eq!(line.trim().lines().count(), 1);
        let decoded: Request = serde_json::from_str(line.trim()).unwrap();
        decoded.protocol.validate().unwrap();
        assert_eq!(decoded.payload.client, "claude-code");
        assert_eq!(decoded.payload.raw, "{\"hook\":\"x\"}");
    }

    #[test]
    fn forward_mode_default_is_platform_safe() {
        assert_eq!(Mode::from_env_value(None), Mode::Auto);
        assert_eq!(Mode::from_env_value(Some("")), Mode::Auto);
        assert_eq!(Mode::from_env_value(Some("unexpected")), Mode::Auto);
    }

    #[test]
    fn forward_mode_explicit_auto_overrides_platform_default() {
        assert_eq!(Mode::from_env_value(Some("auto")), Mode::Auto);
        assert_eq!(Mode::from_env_value(Some("always")), Mode::Always);
        assert_eq!(Mode::from_env_value(Some("never")), Mode::Never);
    }

    #[test]
    fn blocking_response_reader_returns_after_one_ndjson_line_without_eof() {
        struct OneLineNoEof {
            bytes: Option<&'static [u8]>,
        }

        impl std::io::Read for OneLineNoEof {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let Some(bytes) = self.bytes.take() else {
                    panic!("reader waited for EOF after receiving a complete line");
                };
                let len = bytes.len().min(buf.len());
                buf[..len].copy_from_slice(&bytes[..len]);
                Ok(len)
            }
        }

        let response = read_response_line_blocking(OneLineNoEof {
            bytes: Some(
                b"{\"protocol\":{\"protocol_version\":1,\"binary_version\":\"test\"},\"ok\":true}\n",
            ),
        })
        .unwrap();
        assert_eq!(
            response,
            "{\"protocol\":{\"protocol_version\":1,\"binary_version\":\"test\"},\"ok\":true}\n"
        );
    }

    #[test]
    fn old_response_without_protocol_guard_is_rejected() {
        let err = decode_response_line(r#"{"ok":true,"output":"{}"}"#).unwrap_err();
        assert!(is_incompatible_forwarder_error(&err), "got: {err}");
    }

    #[test]
    fn mismatched_binary_version_is_rejected() {
        let line = serde_json::to_string(&Response {
            protocol: Some(ProtocolGuard {
                protocol_version: PROTOCOL_VERSION,
                binary_version: "0.0.0-old".to_owned(),
            }),
            ok: true,
            output: Some("{}".to_owned()),
            error: None,
        })
        .unwrap();
        let err = decode_response_line(&line).unwrap_err();
        assert!(is_incompatible_forwarder_error(&err), "got: {err}");
    }

    #[cfg(windows)]
    #[test]
    fn windows_endpoint_maps_to_named_pipe_not_filesystem_path() {
        let endpoint = endpoint_for_hash("aaaaaaaaaaaa").expect("endpoint");
        socket_name_from_endpoint(&endpoint)
            .expect("Windows endpoint basename should map to a named pipe name");
    }

    #[test]
    fn endpoint_for_hash_is_per_project_under_data_home_root() {
        // Distinct hashes must map to distinct socket files so a daemon for one
        // repo can never bind the path another repo's shim connects to.
        let a = endpoint_for_hash("aaaaaaaaaaaa").expect("endpoint a");
        let b = endpoint_for_hash("bbbbbbbbbbbb").expect("endpoint b");
        assert_ne!(a, b, "different hashes must not collide on one socket");

        // The hash and version guard appear in the file name, and the socket
        // lives directly in the data-home root (not under projects/{hash}/) to
        // keep sun_path short.
        let name_a = a.file_name().and_then(|n| n.to_str()).expect("file name a");
        assert_eq!(
            name_a,
            format!(
                "hook-forward-aaaaaaaaaaaa-p{}-b{}.sock",
                PROTOCOL_VERSION,
                endpoint_safe_binary_version()
            )
        );
        let root = difflore_core::infra::paths::data_home().expect("data home");
        assert_eq!(a.parent(), Some(root.as_path()));

        // The hash is a flat hex token, so the file name carries no path
        // separator that could escape the data-home root.
        assert!(!name_a.contains('/'));
        assert!(!name_a.contains('\\'));
    }

    #[test]
    fn current_project_endpoint_matches_explicit_hash() {
        // The shim resolves the endpoint via `current_project_hash`; the daemon
        // is launched with that same hash on the command line. Pin that the two
        // derivations land on the same socket so they cannot drift.
        //
        // Compare the hash-derived *file name* rather than the full path: the
        // data-home parent is read from `DIFFLORE_HOME`, which sibling tests
        // mutate via `OnceLock` setup running on other threads, so a full-path
        // comparison races on the env between the two `data_home()` reads. The
        // project hash is derived from `cwd` (never mutated) and is the
        // invariant that must hold here; the parent-under-data-home property is
        // covered by `endpoint_for_hash_is_per_project_under_data_home_root`.
        let derived = endpoint_for_current_project().expect("current endpoint");
        let explicit = endpoint_for_hash(&current_project_hash()).expect("explicit endpoint");
        assert_eq!(derived.file_name(), explicit.file_name());
    }
}
