//! Wire protocol between the `difflore-hook` shim binary and the hook
//! forwarder. This is the protocol's only definition: the shim's blocking
//! client (`bin/difflore-hook.rs`) and the in-process async client/server
//! ([`super`]) both speak exactly these shapes over the local socket, so the
//! two sides cannot drift apart.
//!
//! Transport is one NDJSON line per direction over a cross-platform local
//! socket (`interprocess` maps the same path to a Unix-domain socket on Unix
//! and a named-pipe equivalent on Windows).

use std::io::{Read as _, Write as _};

use interprocess::local_socket::traits::Stream as _;
use interprocess::local_socket::{GenericFilePath, Stream as BlockingStream, ToFsName as _};
use serde::{Deserialize, Serialize};

/// Env var selecting the forward mode (`auto` / `always` / `never`).
pub const ENV: &str = difflore_core::infra::env::DIFFLORE_HOOK_FORWARD;

/// Hard cap on a single request or response payload. A hostile or runaway
/// producer must not be able to OOM either side of the socket.
pub const MAX_IPC_BYTES: u64 = 16 * 1024 * 1024;

/// The neutral "do nothing" hook output every supported client accepts.
pub const NOOP_OUTPUT: &str = r#"{"continue":true}"#;

/// One forwarded hook invocation: the client adapter to use plus the raw
/// stdin payload the IDE handed the hook.
#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    pub client: String,
    pub raw: String,
}

/// Forwarder reply. `ok == true` carries the exact hook stdout in `output`;
/// `ok == false` carries a human-readable `error` and the caller falls back
/// to in-process dispatch.
#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
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
        match difflore_core::infra::env::var(ENV)
            .unwrap_or_else(|| "auto".to_owned())
            .to_ascii_lowercase()
            .as_str()
        {
            "always" => Self::Always,
            "never" | "off" | "0" | "false" => Self::Never,
            _ => Self::Auto,
        }
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

/// Local-socket endpoint for a specific project hash. Each repo gets its own
/// `hook-forward-<hash>.sock` so a warm daemon only ever serves the one project
/// whose per-project index pool it froze at launch — index pools cannot cross
/// repos. The file lives in the data-home *root* (not under `projects/{hash}/`)
/// to keep the Unix `sun_path` short (~104-byte limit). Honours `DIFFLORE_HOME`
/// via the core data-home resolver so tests and sandboxes redirect the socket
/// together with the rest of the data dir.
pub fn endpoint_for_hash(project_hash: &str) -> Result<std::path::PathBuf, String> {
    Ok(difflore_core::infra::paths::data_home()?.join(format!("hook-forward-{project_hash}.sock")))
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
        client: client.to_owned(),
        raw: raw.to_owned(),
    };
    serde_json::to_string(&req)
        .map(|line| line + "\n")
        .map_err(|e| e.to_string())
}

/// Decode one NDJSON response line into the hook output, or the forwarder's
/// error message for the caller to handle (typically: fall back in-process).
pub fn decode_response_line(line: &str) -> Result<String, String> {
    let response: Response = serde_json::from_str(line.trim()).map_err(|e| e.to_string())?;
    if response.ok {
        Ok(response.output.unwrap_or_else(|| NOOP_OUTPUT.to_owned()))
    } else {
        Err(response
            .error
            .unwrap_or_else(|| "hook forwarder returned an unknown error".to_owned()))
    }
}

/// Connect (blocking) to a daemon serving `project_hash`, returning the live
/// stream. Errors carry the OS [`io::ErrorKind`] so the single-instance probe
/// can distinguish "no socket file" (`NotFound`) from "file present, nobody
/// listening" (`ConnectionRefused`) — both mean "safe to (re)bind", but the
/// distinction is useful in traces and tests.
pub fn connect_blocking_for_hash(project_hash: &str) -> std::io::Result<BlockingStream> {
    let path = endpoint_for_hash(project_hash).map_err(std::io::Error::other)?;
    let name = path
        .to_fs_name::<GenericFilePath>()
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    BlockingStream::connect(name)
}

/// Synchronous socket round-trip for the shim binary: connect to the daemon
/// serving the current project, write the request line, read the
/// (length-capped) response. Kept blocking so the shim needs no runtime when
/// the warm path is available.
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
        let mut response = String::new();
        let result = stream
            .take(MAX_IPC_BYTES)
            .read_to_string(&mut response)
            .map(|_| response)
            .map_err(|e| e.to_string());
        // Receiver may already be gone (timeout fired); ignore the send error.
        let _ = tx.send(result);
    });

    match rx.recv_timeout(std::time::Duration::from_millis(BLOCKING_READ_TIMEOUT_MS)) {
        Ok(Ok(response)) => {
            if response.trim().is_empty() {
                return Err("hook forwarder returned an empty response".to_owned());
            }
            Ok(response)
        }
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
        let ok_line = serde_json::to_string(&Response {
            ok: true,
            output: Some("{\"context\":\"x\"}".to_owned()),
            error: None,
        })
        .unwrap();
        assert_eq!(
            decode_response_line(&ok_line).unwrap(),
            "{\"context\":\"x\"}"
        );

        let err_line = serde_json::to_string(&Response {
            ok: false,
            output: None,
            error: Some("boom".to_owned()),
        })
        .unwrap();
        assert_eq!(decode_response_line(&err_line).unwrap_err(), "boom");
    }

    #[test]
    fn ok_response_without_output_degrades_to_noop() {
        // A forwarder that replies ok with no payload must still hand the
        // client a valid hook output, not an empty string.
        assert_eq!(decode_response_line(r#"{"ok":true}"#).unwrap(), NOOP_OUTPUT);
    }

    #[test]
    fn request_line_is_single_line_ndjson() {
        let line = encode_request_line("claude-code", "{\"hook\":\"x\"}").unwrap();
        assert!(line.ends_with('\n'));
        assert_eq!(line.trim().lines().count(), 1);
        let decoded: Request = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(decoded.client, "claude-code");
    }

    #[test]
    fn endpoint_for_hash_is_per_project_under_data_home_root() {
        // Distinct hashes must map to distinct socket files so a daemon for one
        // repo can never bind the path another repo's shim connects to.
        let a = endpoint_for_hash("aaaaaaaaaaaa").expect("endpoint a");
        let b = endpoint_for_hash("bbbbbbbbbbbb").expect("endpoint b");
        assert_ne!(a, b, "different hashes must not collide on one socket");

        // The hash appears in the file name, and the socket lives directly in
        // the data-home root (not under projects/{hash}/) to keep sun_path short.
        let name_a = a.file_name().and_then(|n| n.to_str()).expect("file name a");
        assert_eq!(name_a, "hook-forward-aaaaaaaaaaaa.sock");
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
        let derived = endpoint_for_current_project().expect("current endpoint");
        let explicit = endpoint_for_hash(&current_project_hash()).expect("explicit endpoint");
        assert_eq!(derived, explicit);
    }
}
