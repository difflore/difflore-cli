//! `difflore-hook`: the thin shim every client's hook config invokes.
//!
//! Fast path: forward the raw event to the warm hook forwarder over the local
//! socket. Fallback: run the hook runtime in-process. The wire protocol —
//! request/response shapes, endpoint, blocking transport — is defined once in
//! `difflore_cli::hook::forward::protocol` and only consumed here.

use std::io::Read;
use std::process::ExitCode;

use difflore_cli::hook::forward::protocol;

#[tokio::main]
async fn main() -> ExitCode {
    let mode = protocol::Mode::from_env();
    let client = parse_client_arg().unwrap_or_else(|| {
        difflore_core::infra::env::var(difflore_core::infra::env::DIFFLORE_HOOK_CLIENT)
            .unwrap_or_else(|| "claude-code".to_owned())
    });

    // Cap stdin so a hostile or runaway hook producer cannot OOM the hook.
    // Reading the ceiling + 1 lets us no-op on oversized payloads instead of
    // processing truncated events.
    let mut raw = String::new();
    let read = std::io::stdin()
        .take(protocol::MAX_IPC_BYTES + 1)
        .read_to_string(&mut raw);
    if read.is_err() || raw.len() as u64 > protocol::MAX_IPC_BYTES {
        println!("{}", protocol::NOOP_OUTPUT);
        return ExitCode::SUCCESS;
    }

    if mode != protocol::Mode::Never {
        match forward_once(&client, &raw) {
            Ok(output) => {
                println!("{output}");
                return ExitCode::SUCCESS;
            }
            Err(e) if mode == protocol::Mode::Always => {
                eprintln!("DiffLore hook could not start its background helper: {e}");
                return ExitCode::from(2);
            }
            Err(_) => {
                // Auto mode, warm path missed: best-effort spawn a detached
                // daemon so the *next* hook hits the warm path, then fall back
                // in-process for *this* event (we never block waiting for the
                // daemon to bind). Spawn failure is swallowed — it must never
                // turn a working fallback into a hook error.
                maybe_spawn_daemon();
            }
        }
    }

    fallback_to_runtime(&client, &raw).await;
    ExitCode::SUCCESS
}

/// Best-effort detached daemon spawn for the current project. Only logged
/// under `DIFFLORE_DEBUG_HOOKS`; the caller proceeds to fallback regardless.
fn maybe_spawn_daemon() {
    let hash = protocol::current_project_hash();
    if let Err(e) = difflore_cli::hook::forward::spawn::spawn_daemon_detached(&hash) {
        if difflore_core::infra::env::flag_set(difflore_core::infra::env::DIFFLORE_DEBUG_HOOKS) {
            eprintln!("[difflore-hook] daemon spawn skipped: {e}");
        }
    }
}

fn parse_client_arg() -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--client" {
            return args.next();
        }
        if let Some(value) = arg.strip_prefix("--client=") {
            return Some(value.to_owned());
        }
    }
    None
}

/// One warm-path attempt: encode, blocking socket round-trip, decode. Trace
/// timings stay here (shim-only concern); the wire mechanics live in
/// [`protocol`].
fn forward_once(client: &str, raw: &str) -> Result<String, String> {
    let trace =
        difflore_core::infra::env::flag_set(difflore_core::infra::env::DIFFLORE_HOOK_SHIM_TRACE);
    let started = std::time::Instant::now();
    let request = protocol::encode_request_line(client, raw)?;
    if trace {
        eprintln!(
            "[difflore-hook.trace] encode={}ms",
            started.elapsed().as_millis()
        );
    }
    let response = protocol::ipc_roundtrip_blocking(&request)?;
    if trace {
        eprintln!(
            "[difflore-hook.trace] ipc={}ms",
            started.elapsed().as_millis()
        );
    }
    let output = protocol::decode_response_line(&response)?;
    if trace {
        eprintln!(
            "[difflore-hook.trace] decode={}ms",
            started.elapsed().as_millis()
        );
    }
    Ok(output)
}

async fn fallback_to_runtime(client: &str, raw: &str) {
    let debug =
        difflore_core::infra::env::flag_set(difflore_core::infra::env::DIFFLORE_DEBUG_HOOKS);
    match difflore_cli::hook::runtime::output_for_raw(client, raw, debug).await {
        Ok(output) => println!("{output}"),
        Err(e) => {
            if debug {
                eprintln!("[difflore-hook] runtime fallback failed: {e:#}");
            }
            println!("{}", protocol::NOOP_OUTPUT);
        }
    }
}
