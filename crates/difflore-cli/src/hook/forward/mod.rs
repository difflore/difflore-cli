//! Hook forwarder: a warm in-process server that handles hook events for the
//! `difflore-hook` shim so the hot path skips process startup, plus the async
//! client used when this process is on the shim side itself.
//!
//! Wire shapes, endpoint, and the shim's blocking transport live in
//! [`protocol`] — the single protocol definition both binaries compile
//! against.
//!
//! KNOWN-UNWIRED (R1/R4): [`try_forward`] and [`run_server`] currently have no
//! callers in the workspace — the daemon that would host `run_server`, and the
//! client path that would call `try_forward`, are not yet wired. Only
//! [`protocol::ipc_roundtrip_blocking`] is live today (used by the
//! `difflore-hook` shim binary). These two entry points are deliberately kept
//! `pub` ahead of the daemon landing; see ARCHITECTURE.md "Known unwired".

pub mod protocol;

use std::sync::Arc;
use std::time::Duration;

use interprocess::local_socket::tokio::prelude::*;
use interprocess::local_socket::{GenericFilePath, ListenerOptions, ToFsName};
use tokio::time::timeout;

pub use protocol::{ENV, Mode};

use protocol::{Request, Response};

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

pub async fn try_forward(client: &str, raw: &str) -> Attempt {
    let mode = Mode::from_env();
    if mode == Mode::Never {
        return Attempt::Disabled;
    }
    let fut = roundtrip(client, raw);
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

async fn roundtrip(client: &str, raw: &str) -> anyhow::Result<String> {
    let line = protocol::encode_request_line(client, raw).map_err(anyhow::Error::msg)?;
    let response_line = ipc_roundtrip(&line).await?;
    protocol::decode_response_line(&response_line).map_err(|e| anyhow::anyhow!("{e}"))
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
    let adapter = crate::hook::adapters::get_platform_adapter(&req.client);
    let response = match crate::hook::runtime::hook_output_for_raw(
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

async fn ipc_roundtrip(request_line: &str) -> anyhow::Result<String> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let path = protocol::endpoint().map_err(anyhow::Error::msg)?;
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

    let socket = protocol::endpoint().map_err(anyhow::Error::msg)?;
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
