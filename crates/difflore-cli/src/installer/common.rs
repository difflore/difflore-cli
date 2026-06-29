use std::{
    collections::BTreeSet,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde_json::{Value, json};

use super::{
    CanonicalRecordState, CanonicalRecordStatus, InstallState, McpRuntimeProbe, RuntimeProbeState,
    Status, TargetOutcome, TargetStatus,
    json_config::{McpEntryShape, load_json_object},
    manifest::{self, InstallManifest, ManifestTarget},
    registry,
};
use crate::style;

const MCP_RUNTIME_PROBE_TIMEOUT: Duration = Duration::from_secs(45);
pub(super) const MCP_SERVER_ARG: &str = "mcp-server";

/// Atomically write `contents` to `path`, a drop-in for `std::fs::write` that
/// cannot leave a half-written or empty file. Writes a sibling temp file,
/// fsyncs it, preserves the target's existing permissions, then atomically
/// renames it over `path`, so a crash or power loss leaves the original file
/// intact rather than truncated.
pub(super) fn write_atomic(
    path: impl AsRef<Path>,
    contents: impl AsRef<[u8]>,
) -> std::io::Result<()> {
    let path = path.as_ref();
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(contents.as_ref())?;
    // Flush to disk before the rename so a power loss can't surface a
    // renamed-but-empty file. Best-effort: some filesystems reject fsync.
    let _ = tmp.as_file().sync_all();
    // NamedTempFile is created 0600 on Unix; without this an existing 0644
    // config would silently tighten. Mirror the target's current perms.
    #[cfg(unix)]
    if let Ok(meta) = fs::metadata(path) {
        let _ = tmp.as_file().set_permissions(meta.permissions());
    }
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

pub(super) fn difflore_mcp_record_path() -> Result<PathBuf, String> {
    let dir = difflore_core::infra::paths::data_home().map_err(|e| e.to_string())?;
    fs::create_dir_all(&dir).map_err(|e| format!("failed to create {}: {e}", dir.display()))?;
    Ok(dir.join("mcp.json"))
}

/// Write the v2 install manifest to `~/.difflore/mcp.json`. Retains the v1
/// top-level `command` / `args` / `installed_targets` fields for backward
/// compatibility with older readers.
pub(super) fn write_install_manifest(
    bin: &str,
    targets: Vec<ManifestTarget>,
) -> Result<PathBuf, String> {
    let installed_targets: Vec<String> = targets.iter().map(|t| t.name.clone()).collect();
    let record = InstallManifest {
        manifest_version: manifest::MANIFEST_VERSION,
        command: bin.to_owned(),
        args: vec![MCP_SERVER_ARG.to_owned()],
        installed_targets,
        targets,
    };
    manifest::save(&record)
}

/// Read the `installed_targets` array from `~/.difflore/mcp.json`, returning
/// an empty vec when the record is missing, unreadable, or has no such field.
/// Used by `agents uninstall` to undo exactly the surfaces that were wired.
pub(super) fn read_canonical_record_targets() -> Vec<String> {
    let Ok(path) = difflore_mcp_record_path() else {
        return Vec::new();
    };
    if !path.exists() {
        return Vec::new();
    }
    let Ok(obj) = load_json_object(&path) else {
        return Vec::new();
    };
    obj.get("installed_targets")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(ToOwned::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// Delete the canonical record at `~/.difflore/mcp.json`. Idempotent: a
/// missing file is treated as success.
pub(super) fn delete_canonical_record() -> Result<Option<PathBuf>, String> {
    let path = difflore_mcp_record_path()?;
    if !path.exists() {
        return Ok(None);
    }
    fs::remove_file(&path).map_err(|e| format!("failed to remove {}: {e}", path.display()))?;
    Ok(Some(path))
}

pub(super) fn resolve_difflore_binary() -> Result<String, String> {
    let exe = std::env::current_exe()
        .map_err(|e| format!("could not resolve current binary path: {e}"))?;
    let canon = exe.canonicalize().unwrap_or(exe);
    let preferred = difflore_core::infra::paths::data_home()
        .ok()
        .and_then(|home| preferred_managed_binary(&canon, &home))
        .unwrap_or(canon);
    // Agent configs should point at the agent-facing command, not necessarily
    // the human-facing CLI. On Windows the launcher is a no-console wrapper
    // that preserves stdio for MCP while avoiding transient GUI-spawned
    // console windows. Other platforms keep the CLI path because GUI apps do
    // not allocate terminal windows for stdio children.
    let command = preferred_agent_binary(&preferred).unwrap_or(preferred);
    Ok(path_for_command(&command))
}

fn preferred_managed_binary(exe: &Path, home: &Path) -> Option<PathBuf> {
    if !exe.starts_with(home.join("versions")) {
        return None;
    }
    let shim = managed_cli_shim_path(home);
    shim.is_file().then_some(shim)
}

fn managed_cli_shim_path(home: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        home.join("bin").join("difflore.exe")
    }
    #[cfg(not(windows))]
    {
        home.join("bin").join("difflore")
    }
}

#[cfg(windows)]
fn preferred_agent_binary(cli_bin: &Path) -> Option<PathBuf> {
    let launcher = cli_bin.with_file_name("difflore-launcher.exe");
    launcher.is_file().then_some(launcher)
}

#[cfg(not(windows))]
const fn preferred_agent_binary(_cli_bin: &Path) -> Option<PathBuf> {
    None
}

fn path_for_command(path: &Path) -> String {
    let s = path.to_string_lossy().into_owned();
    s.strip_prefix(r"\\?\").map(ToOwned::to_owned).unwrap_or(s)
}

pub(super) fn home_path(suffix: &[&str]) -> Result<PathBuf, String> {
    let mut p = difflore_core::infra::env::var_os(difflore_core::infra::env::DIFFLORE_MCP_HOME)
        .map(PathBuf::from)
        .or_else(dirs::home_dir)
        .ok_or("could not resolve home directory")?;
    for seg in suffix {
        p.push(seg);
    }
    Ok(p)
}

pub(super) fn cwd_path(suffix: &[&str]) -> Result<PathBuf, String> {
    let mut p = std::env::current_dir().map_err(|e| format!("could not resolve cwd: {e}"))?;
    for seg in suffix {
        p.push(seg);
    }
    Ok(p)
}

// The plugin route registers MCP via bundled config, so the CLI installer
// must short-circuit here to avoid creating a duplicate MCP entry.
pub(super) fn claude_plugin_installed() -> bool {
    let Ok(cache_dir) = home_path(&[".claude", "plugins", "cache"]) else {
        return false;
    };
    let Ok(entries) = fs::read_dir(&cache_dir) else {
        return false;
    };
    for owner in entries.flatten() {
        if owner.path().join("difflore").is_dir() {
            return true;
        }
    }
    false
}

pub(super) const fn error_outcome(name: &'static str, e: String) -> TargetOutcome {
    TargetOutcome {
        name,
        status: Status::Error(e),
        detail: String::new(),
    }
}

pub(super) fn probe_cli_mcp(
    name: &'static str,
    tool: &'static str,
    args: &[&str],
    expected_command: &str,
) -> TargetStatus {
    let detected = which::which(tool).is_ok();
    if !detected {
        return TargetStatus {
            name,
            detected: false,
            state: InstallState::NotInstalled,
            detail: Some(format!("`{tool}` CLI not on PATH")),
        };
    }
    match Command::new(tool).args(args).output() {
        Ok(o) if o.status.success() => evaluate_cli_mcp_get_output(
            name,
            tool,
            args,
            expected_command,
            &String::from_utf8_lossy(&o.stdout),
        ),
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr).trim().to_owned();
            let state = if stderr.contains("invalid")
                || stderr.contains("parse")
                || stderr.contains("malformed")
                || stderr.contains("corrupt")
            {
                InstallState::Conflict
            } else {
                InstallState::NotInstalled
            };
            TargetStatus {
                name,
                detected: true,
                state,
                detail: Some(if stderr.is_empty() {
                    format!("`{tool} {}` exit {}", args.join(" "), o.status)
                } else {
                    format!("`{tool} {}` exit {}: {stderr}", args.join(" "), o.status)
                }),
            }
        }
        Err(e) => TargetStatus {
            name,
            detected: true,
            state: InstallState::Unknown,
            detail: Some(format!("could not invoke `{tool}`: {e}")),
        },
    }
}

fn evaluate_cli_mcp_get_output(
    name: &'static str,
    tool: &str,
    args: &[&str],
    expected_command: &str,
    stdout: &str,
) -> TargetStatus {
    let Some(command) = labeled_cli_value(stdout, "command") else {
        return TargetStatus {
            name,
            detected: true,
            state: InstallState::Unknown,
            detail: Some(format!(
                "`{tool} {}` succeeded but DiffLore could not parse a command from its output",
                args.join(" ")
            )),
        };
    };
    let actual_args = labeled_cli_value(stdout, "args")
        .as_deref()
        .map(parse_cli_args_value)
        .unwrap_or_default();
    let command_ok = cli_command_matches(&command, expected_command);
    let args_ok = actual_args.len() == 1
        && actual_args
            .first()
            .is_some_and(|arg| arg.as_str() == MCP_SERVER_ARG);
    if command_ok && args_ok {
        return TargetStatus {
            name,
            detected: true,
            state: InstallState::Installed,
            detail: None,
        };
    }

    TargetStatus {
        name,
        detected: true,
        state: InstallState::Conflict,
        detail: Some(format!(
            "`{tool} {}` difflore entry drifted (command={}, args={})",
            args.join(" "),
            command,
            format_cli_args_detail(&actual_args)
        )),
    }
}

fn labeled_cli_value(stdout: &str, label: &str) -> Option<String> {
    stdout.lines().find_map(|line| {
        let (key, value) = line.trim().split_once(':')?;
        key.trim()
            .eq_ignore_ascii_case(label)
            .then(|| value.trim().to_owned())
    })
}

fn parse_cli_args_value(value: &str) -> Vec<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed == "-" {
        return Vec::new();
    }
    if let Ok(json) = serde_json::from_str::<Value>(trimmed)
        && let Some(items) = json.as_array()
    {
        return items
            .iter()
            .filter_map(Value::as_str)
            .map(ToOwned::to_owned)
            .collect();
    }
    trimmed.split_whitespace().map(ToOwned::to_owned).collect()
}

fn format_cli_args_detail(args: &[String]) -> String {
    if args.is_empty() {
        "(missing)".to_owned()
    } else {
        format!("{args:?}")
    }
}

fn cli_command_matches(actual: &str, expected: &str) -> bool {
    let actual = unquote_cli_value(actual);
    let expected = unquote_cli_value(expected);
    commands_equal(&actual, &expected)
        || canonical_command(&actual)
            .zip(canonical_command(&expected))
            .is_some_and(|(actual, expected)| commands_equal(&actual, &expected))
}

fn unquote_cli_value(value: &str) -> String {
    value
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .trim()
        .to_owned()
}

fn canonical_command(command: &str) -> Option<String> {
    PathBuf::from(command)
        .canonicalize()
        .ok()
        .map(|path| path_for_command(&path))
}

#[cfg(windows)]
fn commands_equal(left: &str, right: &str) -> bool {
    left.replace('\\', "/")
        .eq_ignore_ascii_case(&right.replace('\\', "/"))
}

#[cfg(not(windows))]
fn commands_equal(left: &str, right: &str) -> bool {
    left == right
}

pub(super) fn probe_json_install(
    name: &'static str,
    path: &PathBuf,
    servers_key: &str,
    expected_command: &str,
    shape: McpEntryShape,
) -> TargetStatus {
    if !path.exists() {
        return TargetStatus {
            name,
            detected: false,
            state: InstallState::NotInstalled,
            detail: Some(format!("{} not found", path.display())),
        };
    }
    let obj = match load_json_object(path) {
        Ok(obj) => obj,
        Err(e) => {
            return TargetStatus {
                name,
                detected: true,
                state: if e.contains("invalid JSON") || e.contains("not a JSON object") {
                    InstallState::Conflict
                } else {
                    InstallState::Unknown
                },
                detail: Some(e),
            };
        }
    };
    let Some(servers) = obj.get(servers_key).and_then(|v| v.as_object()) else {
        return TargetStatus {
            name,
            detected: true,
            state: InstallState::NotInstalled,
            detail: Some(format!("{servers_key} block not present")),
        };
    };
    let Some(entry) = servers.get("difflore") else {
        return TargetStatus {
            name,
            detected: true,
            state: InstallState::NotInstalled,
            detail: Some(format!("{} has no difflore entry", path.display())),
        };
    };
    let Some(entry_obj) = entry.as_object() else {
        return TargetStatus {
            name,
            detected: true,
            state: InstallState::Conflict,
            detail: Some(format!(
                "{}: difflore entry is not an object",
                path.display()
            )),
        };
    };
    if mcp_json_entry_matches(entry_obj, expected_command, shape) {
        return TargetStatus {
            name,
            detected: true,
            state: InstallState::Installed,
            detail: Some(path.display().to_string()),
        };
    }
    TargetStatus {
        name,
        detected: true,
        state: InstallState::Conflict,
        detail: Some(format!(
            "{}: difflore entry drifted ({})",
            path.display(),
            mcp_json_entry_detail(entry_obj, shape)
        )),
    }
}

fn mcp_json_entry_matches(
    entry_obj: &serde_json::Map<String, Value>,
    expected_command: &str,
    shape: McpEntryShape,
) -> bool {
    match shape {
        McpEntryShape::Standard => {
            let command_ok = entry_obj
                .get("command")
                .and_then(Value::as_str)
                .is_some_and(|cmd| cli_command_matches(cmd, expected_command));
            let args_ok = entry_obj
                .get("args")
                .and_then(Value::as_array)
                .is_some_and(|args| {
                    args.len() == 1 && args.first().and_then(Value::as_str) == Some(MCP_SERVER_ARG)
                });
            command_ok && args_ok
        }
        McpEntryShape::Opencode => {
            let Some(command) = entry_obj.get("command").and_then(Value::as_array) else {
                return false;
            };
            let command_ok = command
                .first()
                .and_then(Value::as_str)
                .is_some_and(|cmd| cli_command_matches(cmd, expected_command));
            let args_ok = command.len() == 2
                && command.get(1).and_then(Value::as_str) == Some(MCP_SERVER_ARG);
            let type_ok = entry_obj.get("type").and_then(Value::as_str) == Some("local");
            let enabled_ok = entry_obj.get("enabled").and_then(Value::as_bool) == Some(true);
            command_ok && args_ok && type_ok && enabled_ok
        }
    }
}

fn mcp_json_entry_detail(
    entry_obj: &serde_json::Map<String, Value>,
    shape: McpEntryShape,
) -> String {
    match shape {
        McpEntryShape::Standard => {
            let command = entry_obj
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or("(missing)");
            let args = entry_obj
                .get("args")
                .map_or_else(|| "(missing)".to_owned(), ToString::to_string);
            format!("command={command}, args={args}")
        }
        McpEntryShape::Opencode => {
            let type_value = entry_obj
                .get("type")
                .map_or_else(|| "(missing)".to_owned(), ToString::to_string);
            let command = entry_obj
                .get("command")
                .map_or_else(|| "(missing)".to_owned(), ToString::to_string);
            let enabled = entry_obj
                .get("enabled")
                .map_or_else(|| "(missing)".to_owned(), ToString::to_string);
            format!("type={type_value}, command={command}, enabled={enabled}")
        }
    }
}

pub(super) fn canonical_record_snapshot(
    bin: &str,
    installed_targets: &[&'static str],
) -> CanonicalRecordStatus {
    let path = match difflore_mcp_record_path() {
        Ok(path) => path,
        Err(e) => {
            return CanonicalRecordStatus {
                path: None,
                state: CanonicalRecordState::Conflict,
                detail: Some(e),
                recorded_targets: Vec::new(),
                actual_targets: installed_targets.iter().map(ToString::to_string).collect(),
            };
        }
    };
    if !path.exists() {
        return CanonicalRecordStatus {
            path: Some(path.display().to_string()),
            state: CanonicalRecordState::Missing,
            detail: Some("run `difflore agents install` to create it".into()),
            recorded_targets: Vec::new(),
            actual_targets: installed_targets.iter().map(ToString::to_string).collect(),
        };
    }
    let obj = match load_json_object(&path) {
        Ok(obj) => obj,
        Err(e) => {
            return CanonicalRecordStatus {
                path: Some(path.display().to_string()),
                state: CanonicalRecordState::Conflict,
                detail: Some(e),
                recorded_targets: Vec::new(),
                actual_targets: installed_targets.iter().map(ToString::to_string).collect(),
            };
        }
    };
    let command = obj.get("command").and_then(|v| v.as_str());
    let args_ok = obj
        .get("args")
        .and_then(|v| v.as_array())
        .is_some_and(|args| {
            args.len() == 1 && args.first().and_then(Value::as_str) == Some(MCP_SERVER_ARG)
        });
    let recorded_targets: Vec<String> = if let Some(v) = obj.get("installed_targets") {
        if let Some(arr) = v.as_array() {
            arr.iter()
                .filter_map(|v| v.as_str().map(ToOwned::to_owned))
                .collect()
        } else {
            eprintln!(
                "{} {}: `installed_targets` is not an array; treating as empty",
                style::warn("warning:"),
                path.display()
            );
            Vec::new()
        }
    } else {
        Vec::new()
    };
    if command != Some(bin) || !args_ok {
        return CanonicalRecordStatus {
            path: Some(path.display().to_string()),
            state: CanonicalRecordState::Conflict,
            detail: Some(format!(
                "record points at {} and args {:?}",
                command.unwrap_or("(missing)"),
                obj.get("args").map(ToString::to_string).unwrap_or_default()
            )),
            recorded_targets,
            actual_targets: installed_targets.iter().map(ToString::to_string).collect(),
        };
    }
    let recorded: BTreeSet<_> = recorded_targets
        .iter()
        .map(|s| canonical_target_key(s))
        .collect();
    let actual: BTreeSet<_> = installed_targets
        .iter()
        .map(|s| canonical_target_key(s))
        .collect();
    if recorded != actual {
        let missing: Vec<String> = actual.difference(&recorded).cloned().collect();
        let stale: Vec<String> = recorded.difference(&actual).cloned().collect();
        return CanonicalRecordStatus {
            path: Some(path.display().to_string()),
            state: CanonicalRecordState::Stale,
            detail: Some(format!(
                "record targets drifted; missing={}, stale={}",
                if missing.is_empty() {
                    "none".to_owned()
                } else {
                    missing.join(", ")
                },
                if stale.is_empty() {
                    "none".to_owned()
                } else {
                    stale.join(", ")
                }
            )),
            recorded_targets,
            actual_targets: installed_targets.iter().map(ToString::to_string).collect(),
        };
    }
    CanonicalRecordStatus {
        path: Some(path.display().to_string()),
        state: CanonicalRecordState::Present,
        detail: Some("canonical record matches current probe snapshot".into()),
        recorded_targets,
        actual_targets: installed_targets.iter().map(ToString::to_string).collect(),
    }
}

/// Canonical lowercased key for a surface name. Delegates to
/// [`registry::canonical_target_key`].
pub(super) fn canonical_target_key(name: &str) -> String {
    registry::canonical_target_key(name)
}

pub(super) fn probe_runtime_mcp_server(bin: &str) -> McpRuntimeProbe {
    let mut child = match Command::new(bin)
        .arg(MCP_SERVER_ARG)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => return McpRuntimeProbe::failed(format!("could not start `{bin}`: {e}")),
    };

    let probe_input = build_runtime_probe_input(runtime_probe_file_from_diff());

    if let Some(mut stdin) = child.stdin.take() {
        if let Err(e) = stdin.write_all(probe_input.as_bytes()) {
            let _ = child.kill();
            return McpRuntimeProbe::failed(format!(
                "could not write JSON-RPC probe to MCP stdin: {e}"
            ));
        }
    } else {
        let _ = child.kill();
        return McpRuntimeProbe::failed("MCP child did not expose stdin");
    }

    let deadline = Instant::now() + MCP_RUNTIME_PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(25)),
            Ok(None) => {
                let _ = child.kill();
                let output = child.wait_with_output();
                let detail = match output {
                    Ok(output) => {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        if stderr.trim().is_empty() {
                            format!(
                                "`{bin}` did not answer initialize/tools-list within {}ms",
                                MCP_RUNTIME_PROBE_TIMEOUT.as_millis()
                            )
                        } else {
                            format!(
                                "`{bin}` timed out after {}ms; stderr: {}",
                                MCP_RUNTIME_PROBE_TIMEOUT.as_millis(),
                                truncate_probe_detail(stderr.trim())
                            )
                        }
                    }
                    Err(e) => format!(
                        "`{bin}` timed out after {}ms and output capture failed: {e}",
                        MCP_RUNTIME_PROBE_TIMEOUT.as_millis()
                    ),
                };
                return McpRuntimeProbe::timed_out(detail);
            }
            Err(e) => {
                let _ = child.kill();
                return McpRuntimeProbe::failed(format!("could not wait for MCP self-check: {e}"));
            }
        }
    }

    match child.wait_with_output() {
        Ok(output) => evaluate_runtime_probe_output(
            &String::from_utf8_lossy(&output.stdout),
            &String::from_utf8_lossy(&output.stderr),
            output.status.success(),
        ),
        Err(e) => McpRuntimeProbe::failed(format!("could not collect MCP self-check output: {e}")),
    }
}

pub(super) fn build_runtime_probe_input(probe_file: Option<String>) -> String {
    let messages = [
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "difflore-status", "version": "0" },
            },
        }),
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "search_rules",
                "arguments": runtime_probe_search_arguments(probe_file),
            },
        }),
    ];

    let mut input = messages
        .iter()
        .map(|message| serde_json::to_string(message).unwrap_or_else(|_| "{}".to_owned()))
        .collect::<Vec<_>>()
        .join("\n");
    input.push('\n');
    input
}

fn runtime_probe_search_arguments(probe_file: Option<String>) -> Value {
    let mut args = json!({
        "intent": "verify DiffLore MCP can return team rules",
        "top_k": 1,
        "session_id": "difflore-mcp-status",
    });

    if let Some(file) = probe_file.filter(|file| !file.trim().is_empty()) {
        args["file"] = Value::String(file.clone());
        args["intent"] = Value::String(format!(
            "verify DiffLore MCP can return team rules for {file}"
        ));
    }

    args
}

fn runtime_probe_file_from_diff() -> Option<String> {
    first_git_diff_name(&[
        "diff",
        "--name-only",
        "--diff-filter=ACMRT",
        "--cached",
        "--",
    ])
    .or_else(|| first_git_diff_name(&["diff", "--name-only", "--diff-filter=ACMRT", "--"]))
}

fn first_git_diff_name(args: &[&str]) -> Option<String> {
    let output = difflore_core::infra::git::git_command(".")
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(ToOwned::to_owned)
}

pub(super) fn evaluate_runtime_probe_output(
    stdout: &str,
    stderr: &str,
    process_ok: bool,
) -> McpRuntimeProbe {
    let mut initialized = false;
    let mut tools_listed = false;
    let mut tool_call_completed = false;
    let mut tool_call_name = None;
    let mut tool_call_rules_injected = None;
    let mut tool_call_rules_indexed = None;
    let mut tool_call_top_result = None;
    let mut tool_names = Vec::new();
    let mut errors = Vec::new();

    for line in stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let id = value.get("id").and_then(Value::as_i64);
        if let Some(error) = value.get("error") {
            errors.push(error.to_string());
            continue;
        }
        match id {
            Some(1) if value.get("result").is_some() => initialized = true,
            Some(2) => {
                if let Some(tools) = value
                    .get("result")
                    .and_then(|result| result.get("tools"))
                    .and_then(Value::as_array)
                {
                    tools_listed = true;
                    tool_names = tools
                        .iter()
                        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
                        .map(ToOwned::to_owned)
                        .collect();
                }
            }
            Some(3) => {
                if let Some(result) = value.get("result") {
                    tool_call_completed = true;
                    tool_call_name = Some("search_rules".to_owned());
                    if let Some(impact) = result.get("_meta").and_then(|m| m.get("impact")) {
                        tool_call_rules_injected = impact
                            .get("rulesInjected")
                            .and_then(Value::as_u64)
                            .map(|n| usize::try_from(n).unwrap_or(usize::MAX));
                        tool_call_rules_indexed = impact
                            .get("rulesIndexed")
                            .and_then(Value::as_u64)
                            .map(|n| usize::try_from(n).unwrap_or(usize::MAX));
                    }
                    tool_call_top_result = result
                        .get("content")
                        .and_then(Value::as_array)
                        .and_then(|items| items.first())
                        .and_then(|item| item.get("text"))
                        .and_then(Value::as_str)
                        .and_then(parse_search_rules_top_result);
                }
            }
            _ => {}
        }
    }

    let tool_count = tools_listed.then_some(tool_names.len());
    if initialized && tools_listed && !tool_names.is_empty() && tool_call_completed {
        return McpRuntimeProbe {
            state: RuntimeProbeState::Ok,
            detail: format!(
                "MCP handshake and tool listing OK ({} tool{} available)",
                tool_names.len(),
                if tool_names.len() == 1 { "" } else { "s" }
            ),
            initialized,
            tools_listed,
            tool_call_completed,
            tool_call_name,
            tool_call_rules_injected,
            tool_call_rules_indexed,
            tool_call_top_result,
            tool_count,
            tool_names,
        };
    }

    let mut detail = if !errors.is_empty() {
        format!("MCP JSON-RPC returned error(s): {}", errors.join("; "))
    } else if !process_ok {
        "MCP process exited non-zero before serving the expected tool list".to_owned()
    } else if stdout.trim().is_empty() {
        "MCP process returned no stdout for initialize/tools-list/search_rules".to_owned()
    } else if initialized && tools_listed && !tool_call_completed {
        "MCP process served initialize/tools-list but did not complete search_rules tools/call"
            .to_owned()
    } else {
        "MCP process did not return initialize, tools/list, and search_rules responses".to_owned()
    };
    let stderr = stderr.trim();
    if !stderr.is_empty() {
        detail.push_str("; stderr: ");
        detail.push_str(&truncate_probe_detail(stderr));
    }

    McpRuntimeProbe {
        state: RuntimeProbeState::Failed,
        detail,
        initialized,
        tools_listed,
        tool_call_completed,
        tool_call_name,
        tool_call_rules_injected,
        tool_call_rules_indexed,
        tool_call_top_result,
        tool_count,
        tool_names,
    }
}

fn parse_search_rules_top_result(text: &str) -> Option<String> {
    let value = serde_json::from_str::<Value>(text).ok()?;
    let title = value
        .get("results")?
        .as_array()?
        .first()?
        .get("title")?
        .as_str()?
        .trim();
    (!title.is_empty()).then(|| truncate_probe_detail(title))
}

fn truncate_probe_detail(detail: &str) -> String {
    const MAX: usize = 240;
    let trimmed = detail.trim();
    if trimmed.len() <= MAX {
        return trimmed.to_owned();
    }
    let preview = trimmed.chars().take(MAX).collect::<String>();
    format!("{preview}...")
}

#[cfg(test)]
mod atomic_write_tests {
    use super::*;

    #[test]
    fn write_atomic_creates_then_replaces_with_no_temp_litter() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.json");

        write_atomic(&path, br#"{"a":1}"#).expect("create");
        assert_eq!(fs::read_to_string(&path).unwrap(), r#"{"a":1}"#);

        write_atomic(&path, br#"{"a":2}"#).expect("replace");
        assert_eq!(fs::read_to_string(&path).unwrap(), r#"{"a":2}"#);

        // The persisted temp file must not litter the directory.
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("config.json")]);
    }

    #[test]
    fn write_atomic_creates_missing_parent_dirs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested/sub/config.json");
        write_atomic(&path, b"hi").expect("write nested");
        assert_eq!(fs::read_to_string(&path).unwrap(), "hi");
    }

    #[test]
    fn managed_version_binary_prefers_stable_shim() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join(".difflore");
        let exe = home
            .join("versions")
            .join("0.3.0")
            .join(format!("difflore{}", std::env::consts::EXE_SUFFIX));
        let shim = managed_cli_shim_path(&home);
        fs::create_dir_all(exe.parent().expect("exe parent")).expect("mkdir version");
        fs::create_dir_all(shim.parent().expect("shim parent")).expect("mkdir shim");
        fs::write(&exe, b"bin").expect("write exe");
        fs::write(&shim, b"shim").expect("write shim");

        assert_eq!(preferred_managed_binary(&exe, &home), Some(shim));
    }

    #[test]
    fn unmanaged_binary_keeps_current_exe_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join(".difflore");
        let exe = dir.path().join("target/debug/difflore");
        fs::create_dir_all(exe.parent().expect("exe parent")).expect("mkdir target");
        fs::write(&exe, b"bin").expect("write exe");

        assert_eq!(preferred_managed_binary(&exe, &home), None);
    }

    #[cfg(windows)]
    #[test]
    fn windows_agent_binary_prefers_no_console_launcher_when_present() {
        let dir = tempfile::tempdir().expect("tempdir");
        let exe = dir.path().join("difflore.exe");
        let launcher = dir.path().join("difflore-launcher.exe");
        fs::write(&exe, b"bin").expect("write exe");
        fs::write(&launcher, b"launcher").expect("write launcher");

        assert_eq!(preferred_agent_binary(&exe), Some(launcher));
    }

    #[cfg(windows)]
    #[test]
    fn windows_agent_binary_keeps_cli_when_launcher_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let exe = dir.path().join("difflore.exe");
        fs::write(&exe, b"bin").expect("write exe");

        assert_eq!(preferred_agent_binary(&exe), None);
    }

    #[test]
    fn cli_mcp_probe_accepts_claude_get_output_when_command_matches() {
        let stdout = concat!(
            "difflore:\n",
            "  Scope: User config (available in all your projects)\n",
            "  Status: OK\n",
            "  Type: stdio\n",
            "  Command: C:\\Users\\lizq\\.cargo\\bin\\difflore-launcher.exe\n",
            "  Args: mcp-server\n"
        );

        let status = evaluate_cli_mcp_get_output(
            "Claude Code",
            "claude",
            &["mcp", "get", "difflore"],
            r"C:\Users\lizq\.cargo\bin\difflore-launcher.exe",
            stdout,
        );

        assert_eq!(status.state, InstallState::Installed);
    }

    #[test]
    fn cli_mcp_probe_flags_stale_external_cli_command() {
        let stdout = concat!(
            "difflore\n",
            "  enabled: true\n",
            "  transport: stdio\n",
            "  command: C:\\Users\\lizq\\.difflore\\versions\\0.2.0\\difflore.exe\n",
            "  args: mcp-server\n"
        );

        let status = evaluate_cli_mcp_get_output(
            "Codex",
            "codex",
            &["mcp", "get", "difflore"],
            r"C:\Users\lizq\.cargo\bin\difflore-launcher.exe",
            stdout,
        );

        assert_eq!(status.state, InstallState::Conflict);
        assert!(
            status
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("entry drifted"))
        );
    }
}
