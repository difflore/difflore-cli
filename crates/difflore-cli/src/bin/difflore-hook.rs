use std::io::{Read, Write};
use std::process::ExitCode;

use interprocess::local_socket::traits::Stream;
use interprocess::local_socket::{GenericFilePath, Stream as LocalStream, ToFsName};
use serde::{Deserialize, Serialize};

const HOOK_FORWARD_ENV: &str = difflore_core::env::DIFFLORE_HOOK_FORWARD;

#[derive(Debug, Serialize)]
struct HookForwardRequest {
    client: String,
    raw: String,
}

#[derive(Debug, Deserialize)]
struct HookForwardResponse {
    ok: bool,
    output: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ForwardMode {
    Auto,
    Always,
    Never,
}

impl ForwardMode {
    fn from_env() -> Self {
        match difflore_core::env::var(HOOK_FORWARD_ENV)
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

#[tokio::main]
async fn main() -> ExitCode {
    let mode = ForwardMode::from_env();
    let client = parse_client_arg().unwrap_or_else(|| {
        difflore_core::env::var(difflore_core::env::DIFFLORE_HOOK_CLIENT)
            .unwrap_or_else(|| "claude-code".to_owned())
    });

    // Cap stdin so a hostile or runaway hook producer cannot OOM the hook.
    // Reading the ceiling + 1 lets us no-op on oversized payloads instead of
    // processing truncated events.
    const MAX_HOOK_STDIN_BYTES: usize = 16 * 1024 * 1024;
    let mut raw = String::new();
    let read = std::io::stdin()
        .take(MAX_HOOK_STDIN_BYTES as u64 + 1)
        .read_to_string(&mut raw);
    if read.is_err() || raw.len() > MAX_HOOK_STDIN_BYTES {
        println!("{{\"continue\":true}}");
        return ExitCode::SUCCESS;
    }

    if mode != ForwardMode::Never {
        match forward_once(&client, &raw) {
            Ok(output) => {
                println!("{output}");
                return ExitCode::SUCCESS;
            }
            Err(e) if mode == ForwardMode::Always => {
                eprintln!("[difflore-hook] forwarder required but unavailable: {e}");
                return ExitCode::from(2);
            }
            Err(_) => {}
        }
    }

    fallback_to_runtime(&client, &raw).await;
    ExitCode::SUCCESS
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

fn forward_once(client: &str, raw: &str) -> Result<String, String> {
    let trace = difflore_core::env::flag_set(difflore_core::env::DIFFLORE_HOOK_SHIM_TRACE);
    let started = std::time::Instant::now();
    let req = HookForwardRequest {
        client: client.to_owned(),
        raw: raw.to_owned(),
    };
    let request = serde_json::to_string(&req).map_err(|e| e.to_string())? + "\n";
    if trace {
        eprintln!(
            "[difflore-hook.trace] encode={}ms",
            started.elapsed().as_millis()
        );
    }
    let response = ipc_roundtrip(&request)?;
    if trace {
        eprintln!(
            "[difflore-hook.trace] ipc={}ms",
            started.elapsed().as_millis()
        );
    }
    let response: HookForwardResponse =
        serde_json::from_str(response.trim()).map_err(|e| e.to_string())?;
    if trace {
        eprintln!(
            "[difflore-hook.trace] decode={}ms",
            started.elapsed().as_millis()
        );
    }
    if response.ok {
        Ok(response
            .output
            .unwrap_or_else(|| r#"{"continue":true}"#.to_owned()))
    } else {
        Err(response
            .error
            .unwrap_or_else(|| "hook forwarder returned an unknown error".to_owned()))
    }
}

/// Synchronous round-trip to the hook forwarder over a cross-platform
/// local socket (Unix domain socket on Unix, named pipe on Windows;
/// the interprocess crate picks the right backend at runtime).
fn ipc_roundtrip(request: &str) -> Result<String, String> {
    let endpoint = hook_forward_endpoint()?;
    let name = endpoint
        .to_fs_name::<GenericFilePath>()
        .map_err(|e| e.to_string())?;
    let mut stream = LocalStream::connect(name).map_err(|e| e.to_string())?;
    stream
        .write_all(request.as_bytes())
        .map_err(|e| e.to_string())?;
    stream.flush().map_err(|e| e.to_string())?;
    // Bound the forwarder response too; a truncated response fails JSON parse
    // downstream and degrades to the runtime fallback.
    const MAX_HOOK_IPC_BYTES: u64 = 16 * 1024 * 1024;
    let mut response = String::new();
    stream
        .take(MAX_HOOK_IPC_BYTES)
        .read_to_string(&mut response)
        .map_err(|e| e.to_string())?;
    if response.trim().is_empty() {
        return Err("hook forwarder returned an empty response".to_owned());
    }
    Ok(response)
}

/// File-path-style endpoint. `interprocess` interprets the same path
/// as a Unix-domain socket on Unix and as a named-pipe-equivalent on
/// Windows (the path resolves into the local namespace).
fn hook_forward_endpoint() -> Result<std::path::PathBuf, String> {
    Ok(difflore_home()?.join("hook-forward.sock"))
}

fn difflore_home() -> Result<std::path::PathBuf, String> {
    if let Some(custom) = difflore_core::env::difflore_home() {
        return Ok(std::path::PathBuf::from(custom));
    }
    dirs::home_dir()
        .map(|p| p.join(".difflore"))
        .ok_or_else(|| "cannot resolve home directory".to_owned())
}

async fn fallback_to_runtime(client: &str, raw: &str) {
    let debug = difflore_core::env::flag_set(difflore_core::env::DIFFLORE_DEBUG_HOOKS);
    match difflore_cli::hook_runtime::output_for_raw(client, raw, debug).await {
        Ok(output) => println!("{output}"),
        Err(e) => {
            eprintln!("[difflore-hook] runtime fallback failed: {e:#}");
            println!("{{\"continue\":true}}");
        }
    }
}
