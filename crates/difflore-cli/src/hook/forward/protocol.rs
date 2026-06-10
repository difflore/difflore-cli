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

/// Local-socket endpoint shared by server, async client, and blocking shim.
/// Honours `DIFFLORE_HOME` (via the core data-home resolver) so tests and
/// sandboxes redirect the socket together with the rest of the data dir.
pub fn endpoint() -> Result<std::path::PathBuf, String> {
    Ok(difflore_core::infra::paths::data_home()?.join("hook-forward.sock"))
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

/// Synchronous socket round-trip for the shim binary: write the request line,
/// read the (length-capped) response. Kept blocking so the shim needs no
/// runtime when the warm path is available.
pub fn ipc_roundtrip_blocking(request_line: &str) -> Result<String, String> {
    let path = endpoint()?;
    let name = path
        .to_fs_name::<GenericFilePath>()
        .map_err(|e| e.to_string())?;
    let mut stream = BlockingStream::connect(name).map_err(|e| e.to_string())?;
    stream
        .write_all(request_line.as_bytes())
        .map_err(|e| e.to_string())?;
    stream.flush().map_err(|e| e.to_string())?;
    let mut response = String::new();
    stream
        .take(MAX_IPC_BYTES)
        .read_to_string(&mut response)
        .map_err(|e| e.to_string())?;
    if response.trim().is_empty() {
        return Err("hook forwarder returned an empty response".to_owned());
    }
    Ok(response)
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
        assert_eq!(decode_response_line(&ok_line).unwrap(), "{\"context\":\"x\"}");

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
}
