//! Hook forwarder: a warm daemon that handles hook events for the
//! `difflore-hook` shim so the hot path skips process startup, plus the async
//! client used when this process is on the shim side itself.
//!
//! Wire shapes, endpoint, and the shim's blocking transport live in
//! [`protocol`] — the single protocol definition both binaries compile
//! against.
//!
//! ## Socket-per-project isolation
//!
//! Each repo + binary version gets its own `hook-forward-...sock` and warm
//! daemon. The daemon is launched with the project hash on the command line
//! ([`run_server_for_hash`]) and freezes the matching per-project index pool at
//! startup, so a request landing on a socket is always answered against the
//! right repo's index and current wire version. The global `data.db` is shared
//! (cross-repo features need one aggregate view); only the index pool is
//! per-project, so isolating it by socket is sufficient — `Request` never
//! carries `cwd`.
//!
//! ## Lifecycle
//!
//! The daemon is best-effort and self-managing: the shim spawns one on a cache
//! miss (detached, non-blocking, this hook still falls back in-process), a
//! single-instance probe makes concurrent spawns idempotent (exactly one binds,
//! the rest connect-and-exit), and an idle timeout reaps the process after a
//! quiet window. See [`run_server_for_hash`].

pub mod protocol;
pub mod spawn;

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

/// Run the warm hook-forward daemon for a single project, identified by its
/// `project_hash`. The hash is taken verbatim from the launcher's command line
/// rather than re-derived from the daemon's own cwd: the daemon is detached and
/// its cwd is not the edited repo, so trusting cwd here would select the wrong
/// index. The frozen `index_pool` is the one and only repo this process serves.
///
/// Startup is idempotent under concurrent launches (see [`bind_or_yield`]): if
/// another daemon already serves this hash, this call returns `Ok(())` without
/// binding. The global `data.db` is shared across daemons by design.
pub async fn run_server_for_hash(project_hash: &str) -> anyhow::Result<()> {
    let socket = protocol::endpoint_for_hash(project_hash).map_err(anyhow::Error::msg)?;
    // Single-instance gate before paying for db/index init: if a live daemon
    // already owns this socket, yield immediately.
    let Some(listener) = bind_or_yield(project_hash, &socket).await? else {
        return Ok(());
    };

    let db = difflore_core::infra::db::init_db()
        .await
        .map_err(anyhow::Error::msg)?;
    let index_pool = difflore_core::context::index_db::get_pool_for_project(project_hash).await?;
    let state = Arc::new(State { db, index_pool });

    if difflore_core::infra::env::trace_hook() {
        eprintln!("[difflore.forward.trace] daemon ready hash={project_hash}");
    }
    serve_until_idle(listener, &socket, state).await;
    Ok(())
}

/// Single-instance bind: probe for an existing live daemon, clear a stale
/// socket file if present, then bind. Returns `Ok(Some(listener))` when this
/// process won the bind, or `Ok(None)` when another daemon already owns the
/// socket (this process should exit cleanly — *not* an error).
///
/// Replaces the old unconditional `remove_file` that could delete a *live*
/// daemon's socket and split it into two accept loops on stale/new fds.
///
/// Bind-first sequence — chosen specifically to be race-safe against N daemons
/// launched at once for the same hash. The naive "probe → remove → bind" order
/// has a fatal window: two daemons both probe (neither bound yet), both
/// `remove_file`, and the *second* removal deletes the *first* daemon's
/// freshly-bound socket file, so the second binds a new inode at the same path
/// and both believe they won (split brain). We never unlink a path we have not
/// proven is unowned, so:
///
/// 1. Bind directly. Success → we own it (`Some`). This is the *only* path that
///    creates a listener, and the OS guarantees exactly one binder per path.
/// 2. `AddrInUse` → something holds the path. Probe it:
///    - connect succeeds → a *live* daemon owns it → yield (`None`).
///    - connect fails → the file is stale (dead daemon) or a leftover
///      non-socket file. Remove it (now provably unowned) and retry the bind
///      once. A second `AddrInUse` means a real daemon won the narrow re-race →
///      yield. Anything else is a genuine error.
async fn bind_or_yield(
    project_hash: &str,
    socket: &std::path::Path,
) -> anyhow::Result<Option<interprocess::local_socket::tokio::Listener>> {
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent)?;
    }

    match try_bind(socket) {
        Ok(listener) => return Ok(Some(listener)),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {}
        Err(e) => return Err(anyhow::Error::new(e).context("bind hook daemon socket")),
    }

    // Path is occupied. Distinguish a *live* daemon from a *stale* socket or a
    // leftover non-socket file. CRITICAL for concurrent-spawn safety: we never
    // unlink a path that might still be owned by a live peer.
    match probe_socket(socket).await {
        ProbeResult::Live => {
            if difflore_core::infra::env::trace_hook() {
                eprintln!(
                    "[difflore.forward.trace] live daemon already owns hash={project_hash}; yielding"
                );
            }
            Ok(None)
        }
        ProbeResult::Stale => {
            // Confirmed dead/leftover: removing it cannot delete a live peer's
            // socket. Clear + rebind once.
            let _ = std::fs::remove_file(socket);
            match try_bind(socket) {
                Ok(listener) => Ok(Some(listener)),
                Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                    // Another daemon won the narrow re-bind race after our probe.
                    if difflore_core::infra::env::trace_hook() {
                        eprintln!(
                            "[difflore.forward.trace] lost re-bind race for hash={project_hash}; yielding"
                        );
                    }
                    Ok(None)
                }
                Err(e) => Err(anyhow::Error::new(e).context("re-bind hook daemon socket")),
            }
        }
    }
}

enum ProbeResult {
    /// A listener accepted, or ownership is ambiguous — assume live, do not
    /// reclaim.
    Live,
    /// The path is confirmed safe to reclaim: a leftover non-socket file, or a
    /// stale socket whose listener is gone (connection refused).
    Stale,
}

/// Classify an occupied socket path. Two distinct stale cases, separated so we
/// never unlink a path that might still be owned:
///
/// * **Non-socket leftover** — a previous run left a regular file (or the path
///   is otherwise not a socket). The filesystem type alone proves no daemon
///   listens; reclaim.
/// * **Stale socket** — a socket file whose daemon died. We connect-probe it
///   (async, so we don't block a worker under concurrent spawns); a run of
///   `ConnectionRefused`/`NotFound` confirms no listener → reclaim. A single
///   successful connect — or any ambiguous error — means "treat as live" → keep.
async fn probe_socket(socket: &std::path::Path) -> ProbeResult {
    // A path that exists but is not a socket cannot have a listener; it is a
    // safe-to-remove leftover. (On Windows named pipes have no filesystem entry,
    // so `symlink_metadata` errors and we fall through to the connect probe.)
    if let Ok(meta) = std::fs::symlink_metadata(socket)
        && !is_socket(&meta)
    {
        return ProbeResult::Stale;
    }

    let Ok(name) = socket.to_fs_name::<GenericFilePath>() else {
        // Can't even form the name; be conservative and don't reclaim.
        return ProbeResult::Live;
    };
    for attempt in 0..3 {
        match LocalSocketStream::connect(name.clone()).await {
            Ok(_stream) => return ProbeResult::Live,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                ) => {}
            // Any other error (e.g. transient resource limit) is ambiguous:
            // assume live rather than risk unlinking a peer's socket.
            Err(_) => return ProbeResult::Live,
        }
        if attempt < 2 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    ProbeResult::Stale
}

#[cfg(unix)]
fn is_socket(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::FileTypeExt as _;
    meta.file_type().is_socket()
}

#[cfg(not(unix))]
const fn is_socket(_meta: &std::fs::Metadata) -> bool {
    // No filesystem socket files on Windows; treat any present entry as
    // ambiguous (the connect probe decides).
    true
}

/// Bind a tokio listener at `socket`, surfacing the raw `io::Error` (so callers
/// can branch on [`std::io::ErrorKind::AddrInUse`]).
fn try_bind(
    socket: &std::path::Path,
) -> std::io::Result<interprocess::local_socket::tokio::Listener> {
    let name = socket
        .to_fs_name::<GenericFilePath>()
        .map_err(std::io::Error::other)?;
    ListenerOptions::new().name(name).create_tokio()
}

async fn handle_request(state: &State, line: &str) -> Response {
    let trace = difflore_core::infra::env::trace_hook();
    let started = std::time::Instant::now();
    let req: Request = match serde_json::from_str(line.trim()) {
        Ok(req) => req,
        Err(e) => {
            return Response::error(format!("invalid forward request: {e}"));
        }
    };
    if let Err(e) = req.protocol.validate() {
        return Response::error(e);
    }
    let adapter = crate::hook::adapters::get_platform_adapter(&req.payload.client);
    let response = match crate::hook::runtime::hook_output_for_raw(
        &req.payload.client,
        &*adapter,
        &req.payload.raw,
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
            Response::ok(output)
        }
        Err(e) => {
            if trace {
                eprintln!(
                    "[difflore.forward.trace] hook_error={}ms",
                    started.elapsed().as_millis()
                );
            }
            Response::error(e.to_string())
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
    use tokio::io::{AsyncWriteExt, BufReader};

    let path = protocol::endpoint_for_current_project().map_err(anyhow::Error::msg)?;
    let name = path.to_fs_name::<GenericFilePath>()?;
    let stream = LocalSocketStream::connect(name).await?;
    let (reader, mut writer) = stream.split();
    writer.write_all(request_line.as_bytes()).await?;
    writer.flush().await?;
    let mut reader = BufReader::new(reader);
    let mut response = String::new();
    read_ipc_line_capped(&mut reader, &mut response).await?;
    if response.trim().is_empty() {
        anyhow::bail!("hook forwarder returned an empty response");
    }
    Ok(response)
}

async fn read_ipc_line_capped<R>(reader: &mut R, line: &mut String) -> anyhow::Result<usize>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    use tokio::io::{AsyncBufReadExt, AsyncReadExt as _};

    let n = reader
        .take(protocol::MAX_IPC_BYTES + 1)
        .read_line(line)
        .await?;
    if n as u64 > protocol::MAX_IPC_BYTES {
        anyhow::bail!(
            "hook forwarder IPC line exceeds {} bytes",
            protocol::MAX_IPC_BYTES
        );
    }
    Ok(n)
}

/// Accept-loop with an idle reaper. Each `accept` is wrapped in a
/// [`tokio::time::timeout`] of [`env::hook_daemon_idle_secs`]; a quiet window
/// breaks the loop and the daemon exits, removing its socket so a future shim
/// re-spawns a fresh one. The timer naturally resets per loop iteration, so any
/// accepted connection (the request itself is handled on a detached task and
/// `accept` returns fast) postpones the reap.
///
/// Removing the socket on exit is safe: this process is the sole listener
/// (it bound the socket). A new shim connecting after removal simply falls back
/// in-process and re-spawns — self-healing.
async fn serve_until_idle(
    listener: interprocess::local_socket::tokio::Listener,
    socket: &std::path::Path,
    state: Arc<State>,
) {
    let idle = Duration::from_secs(difflore_core::infra::env::hook_daemon_idle_secs());
    loop {
        match timeout(idle, listener.accept()).await {
            Ok(Ok(stream)) => {
                let state = Arc::<State>::clone(&state);
                tokio::spawn(handle_connection(stream, state));
            }
            Ok(Err(_e)) => {
                // A transient accept error shouldn't kill the daemon; keep
                // waiting (the loop re-arms the idle timer). A persistent error
                // will eventually idle-time-out.
            }
            Err(_elapsed) => {
                if difflore_core::infra::env::trace_hook() {
                    eprintln!("[difflore.forward.trace] daemon idle timeout; exiting");
                }
                break;
            }
        }
    }
    // Best-effort cleanup (no-op on Windows / if already gone).
    let _ = std::fs::remove_file(socket);
}

async fn handle_connection(stream: interprocess::local_socket::tokio::Stream, state: Arc<State>) {
    use tokio::io::{AsyncWriteExt, BufReader};

    let trace = difflore_core::infra::env::trace_hook();
    let started = std::time::Instant::now();
    let (reader, mut writer) = stream.split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    if let Err(error) = read_ipc_line_capped(&mut reader, &mut line).await {
        let response = Response::error(error.to_string());
        let response_line = serde_json::to_string(&response).map_or_else(
            |_| "{\"ok\":false,\"error\":\"serialize response failed\"}\n".to_owned(),
            |s| s + "\n",
        );
        let _ = writer.write_all(response_line.as_bytes()).await;
        let _ = writer.flush().await;
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
}
