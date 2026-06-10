use std::sync::Arc;
use std::time::Duration;

use interprocess::local_socket::tokio::prelude::*;
use interprocess::local_socket::{GenericFilePath, ListenerOptions, ToFsName};
use serde::{Deserialize, Serialize};
use tokio::time::timeout;

pub const ENV: &str = difflore_core::infra::env::DIFFLORE_HOOK_FORWARD;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Auto,
    Always,
    Never,
}

impl Mode {
    fn from_env() -> Self {
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

pub enum Attempt {
    Used(String),
    Unavailable { mode: Mode, error: String },
    Disabled,
}

#[derive(Clone)]
pub struct State {
    pub db: difflore_core::SqlitePool,
    pub index_pool: difflore_core::SqlitePool,
}

#[derive(Debug, Serialize, Deserialize)]
struct Request {
    client: String,
    raw: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct Response {
    ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

pub async fn try_forward(client: &str, raw: &str) -> Attempt {
    let mode = Mode::from_env();
    if mode == Mode::Never {
        return Attempt::Disabled;
    }
    let req = Request {
        client: client.to_owned(),
        raw: raw.to_owned(),
    };
    let fut = roundtrip(&req);
    match timeout(Duration::from_secs(5), fut).await {
        Ok(Ok(output)) => Attempt::Used(output),
        Ok(Err(error)) => Attempt::Unavailable {
            mode,
            error: error.to_string(),
        },
        Err(_) => Attempt::Unavailable {
            mode,
            error: "timed out connecting to hook forwarder".to_owned(),
        },
    }
}

async fn roundtrip(req: &Request) -> anyhow::Result<String> {
    let line = serde_json::to_string(req)?;
    let response_line = ipc_roundtrip(&(line + "\n")).await?;
    let response: Response = serde_json::from_str(response_line.trim())?;
    if response.ok {
        Ok(response
            .output
            .unwrap_or_else(|| "{\"continue\":true}".to_owned()))
    } else {
        Err(anyhow::anyhow!(
            "{}",
            response
                .error
                .unwrap_or_else(|| "hook forwarder returned an unknown error".to_owned())
        ))
    }
}

pub async fn run_server() -> anyhow::Result<()> {
    let db = difflore_core::infra::db::init_db()
        .await
        .map_err(anyhow::Error::msg)?;
    let index_pool = difflore_core::context::index_db::get_pool_for_cwd().await?;
    let state = Arc::new(State { db, index_pool });
    run_ipc_server(state).await
}

async fn handle_request(state: &State, line: &str) -> Response {
    let trace = difflore_core::infra::env::trace_hook();
    let started = std::time::Instant::now();
    let req: Request = match serde_json::from_str(line.trim()) {
        Ok(req) => req,
        Err(e) => {
            return Response {
                ok: false,
                output: None,
                error: Some(format!("invalid forward request: {e}")),
            };
        }
    };
    let adapter = crate::hooks::get_platform_adapter(&req.client);
    let response = match crate::hook_runtime::hook_output_for_raw(
        &req.client,
        &*adapter,
        &req.raw,
        false,
        true,
        Some(state),
    )
    .await
    {
        Ok(output) => {
            if trace {
                eprintln!(
                    "[difflore.forward.trace] hook_output={}ms",
                    started.elapsed().as_millis()
                );
            }
            Response {
                ok: true,
                output: Some(output),
                error: None,
            }
        }
        Err(e) => {
            if trace {
                eprintln!(
                    "[difflore.forward.trace] hook_error={}ms",
                    started.elapsed().as_millis()
                );
            }
            Response {
                ok: false,
                output: None,
                error: Some(e.to_string()),
            }
        }
    };
    if trace {
        eprintln!(
            "[difflore.forward.trace] response_ready={}ms",
            started.elapsed().as_millis()
        );
    }
    response
}

/// Cross-platform local-socket endpoint: `interprocess` treats the same path as
/// a Unix-domain socket on Unix and a named-pipe-equivalent on Windows.
fn endpoint() -> anyhow::Result<std::path::PathBuf> {
    Ok(difflore_core::infra::paths::data_home()
        .map_err(anyhow::Error::msg)?
        .join("hook-forward.sock"))
}

async fn ipc_roundtrip(request_line: &str) -> anyhow::Result<String> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let path = endpoint()?;
    let name = path.to_fs_name::<GenericFilePath>()?;
    let stream = LocalSocketStream::connect(name).await?;
    let (reader, mut writer) = stream.split();
    writer.write_all(request_line.as_bytes()).await?;
    writer.flush().await?;
    let mut reader = BufReader::new(reader);
    let mut response = String::new();
    reader.read_line(&mut response).await?;
    if response.trim().is_empty() {
        anyhow::bail!("hook forwarder returned an empty response");
    }
    Ok(response)
}

async fn run_ipc_server(state: Arc<State>) -> anyhow::Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let socket = endpoint()?;
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // On Unix the listener takes a real filesystem path, so remove any stale
    // socket file from a prior run; on Windows there is no file and this no-ops.
    let _ = std::fs::remove_file(&socket);
    let name = socket.to_fs_name::<GenericFilePath>()?;
    let listener = ListenerOptions::new().name(name).create_tokio()?;
    loop {
        let stream = listener.accept().await?;
        let state = Arc::<State>::clone(&state);
        tokio::spawn(async move {
            let trace = difflore_core::infra::env::trace_hook();
            let started = std::time::Instant::now();
            let (reader, mut writer) = stream.split();
            let mut reader = BufReader::new(reader);
            let mut line = String::new();
            if reader.read_line(&mut line).await.is_err() {
                return;
            }
            if trace {
                eprintln!(
                    "[difflore.forward.trace] request_read={}ms",
                    started.elapsed().as_millis()
                );
            }
            let response = handle_request(&state, &line).await;
            let response_line = match serde_json::to_string(&response) {
                Ok(s) => s + "\n",
                Err(_) => "{\"ok\":false,\"error\":\"serialize response failed\"}\n".to_owned(),
            };
            let _ = writer.write_all(response_line.as_bytes()).await;
            let _ = writer.flush().await;
            if trace {
                eprintln!(
                    "[difflore.forward.trace] response_written={}ms",
                    started.elapsed().as_millis()
                );
            }
        });
    }
}
