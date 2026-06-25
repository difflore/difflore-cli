//! Best-effort detached spawn of the warm hook-forward daemon.
//!
//! Called by the `difflore-hook` shim on a cache miss (auto mode): the current
//! hook still falls back in-process, but a daemon is launched so the *next*
//! hook hits the warm path. Everything here is best-effort — any failure is
//! swallowed (optionally logged) and never escalates into a hook error.
//!
//! The daemon must outlive the short-lived shim: on Unix we `setsid` in a
//! `pre_exec` hook so it leaves the shim's session and survives SIGHUP; on
//! Windows we set `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP |
//! CREATE_BREAKAWAY_FROM_JOB`. We never `wait()` — on Unix `setsid` reparents
//! the orphan to init; on Windows the detached process is independent when the
//! host job allows breakaway.

use std::ffi::OsString;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
#[cfg(not(windows))]
use std::process::{Command, Stdio};

const MAX_DAEMON_LOG_BYTES: u64 = 1024 * 1024;

/// Derive the main `difflore` binary path from the shim's own
/// `current_exe()`. The two binaries ship in the same directory (see the
/// crate's `[[bin]]` targets), so we swap the file name, mirroring the
/// installer's forward derivation in `hooks_install::hook_shim_path`.
///
/// Returns `None` when `current_exe()` fails (rare; e.g. the binary was
/// unlinked) — the caller then skips the spawn and relies on fallback.
fn main_binary_path() -> Option<PathBuf> {
    let shim = std::env::current_exe().ok()?;
    let main_name = format!("difflore{}", std::env::consts::EXE_SUFFIX);
    Some(shim.with_file_name(main_name))
}

/// Spawn a detached warm daemon for `project_hash`. Best-effort: returns `Ok`
/// once the child is launched (we do not wait for it to bind), `Err` only for
/// observability — the shim treats every outcome as "carry on and fall back".
///
/// `main_bin` is injectable so tests can point at a known-good / known-bad
/// path without depending on `current_exe()`.
pub fn spawn_daemon_detached(project_hash: &str) -> Result<(), String> {
    let bin = main_binary_path().ok_or_else(|| "cannot resolve difflore binary path".to_owned())?;
    spawn_daemon_at(&bin, project_hash)
}

/// Spawn the hidden memory-autopilot worker as an independent process. This
/// intentionally shares the same detach mechanics as the hook daemon so it
/// survives the short-lived hook shim process.
pub fn spawn_memory_autopilot_detached(lease_owner: &str) -> Result<(), String> {
    let bin = main_binary_path().ok_or_else(|| "cannot resolve difflore binary path".to_owned())?;
    spawn_memory_autopilot_at(&bin, lease_owner)
}

/// Spawn the hidden cloud-outbox drain daemon. This is best-effort and only
/// used after the hook has durable local work for the daemon to drain.
pub fn spawn_outbox_daemon_detached() -> Result<(), String> {
    let bin = main_binary_path().ok_or_else(|| "cannot resolve difflore binary path".to_owned())?;
    spawn_outbox_daemon_at(&bin)
}

/// Like [`spawn_daemon_detached`] but with an explicit binary path.
pub fn spawn_daemon_at(main_bin: &Path, project_hash: &str) -> Result<(), String> {
    #[cfg(windows)]
    {
        spawn_detached_no_inherit(main_bin, &["__hook-daemon", "--project-hash", project_hash])
    }

    #[cfg(not(windows))]
    {
        let mut cmd = Command::new(main_bin);
        cmd.args(["__hook-daemon", "--project-hash", project_hash])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(daemon_log_stdio());

        configure_detached(&mut cmd);

        // Spawn and drop the handle: we deliberately never `wait`. On Unix the
        // setsid'd child is reparented to init; dropping `Child` leaks no zombie.
        match cmd.spawn() {
            Ok(_child) => Ok(()),
            Err(e) => Err(format!("spawn hook daemon failed: {e}")),
        }
    }
}

/// Like [`spawn_outbox_daemon_detached`] but with an explicit binary path.
pub fn spawn_outbox_daemon_at(main_bin: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        spawn_detached_no_inherit(main_bin, &["__outbox-daemon"])
    }

    #[cfg(not(windows))]
    {
        let mut cmd = Command::new(main_bin);
        cmd.arg("__outbox-daemon")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(outbox_daemon_log_stdio());

        configure_detached(&mut cmd);

        match cmd.spawn() {
            Ok(_child) => Ok(()),
            Err(e) => Err(format!("spawn outbox daemon failed: {e}")),
        }
    }
}

/// Like [`spawn_memory_autopilot_detached`] but with an explicit binary path.
pub fn spawn_memory_autopilot_at(main_bin: &Path, lease_owner: &str) -> Result<(), String> {
    #[cfg(windows)]
    {
        spawn_detached_no_inherit(
            main_bin,
            &[
                "memory",
                "autopilot",
                "--background",
                "--lease-owner",
                lease_owner,
            ],
        )
    }

    #[cfg(not(windows))]
    {
        let mut cmd = Command::new(main_bin);
        cmd.args([
            "memory",
            "autopilot",
            "--background",
            "--lease-owner",
            lease_owner,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(autopilot_log_stdio());

        configure_detached(&mut cmd);

        match cmd.spawn() {
            Ok(_child) => Ok(()),
            Err(e) => Err(format!("spawn memory autopilot failed: {e}")),
        }
    }
}

/// Where the detached daemon's stderr goes. Routes to
/// `data_home/logs/hook-daemon.log` when resolvable (handy for diagnosing a
/// daemon that exits early), else `null`. Best-effort: any path/IO failure
/// degrades to `null` rather than aborting the spawn.
#[cfg(not(windows))]
fn daemon_log_stdio() -> Stdio {
    log_stdio("hook-daemon.log")
}

pub(crate) fn rotate_hook_daemon_log_best_effort() {
    rotate_log_by_name_best_effort("hook-daemon.log");
}

#[cfg(not(windows))]
fn autopilot_log_stdio() -> Stdio {
    log_stdio("memory-autopilot.log")
}

#[cfg(not(windows))]
fn outbox_daemon_log_stdio() -> Stdio {
    log_stdio("outbox-daemon.log")
}

#[cfg(not(windows))]
fn log_stdio(file_name: &str) -> Stdio {
    rotate_log_by_name_best_effort(file_name);
    let Ok(path) = log_path(file_name) else {
        return Stdio::null();
    };
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        Ok(file) => Stdio::from(file),
        Err(_) => Stdio::null(),
    }
}

fn rotate_log_by_name_best_effort(file_name: &str) {
    if let Ok(path) = log_path(file_name) {
        let _ = rotate_log_if_large(&path, MAX_DAEMON_LOG_BYTES);
    }
}

fn log_path(file_name: &str) -> std::io::Result<PathBuf> {
    let Ok(home) = difflore_core::infra::paths::data_home() else {
        return Err(std::io::Error::other(
            "could not resolve DiffLore data home",
        ));
    };
    let logs = home.join("logs");
    std::fs::create_dir_all(&logs)?;
    Ok(logs.join(file_name))
}

fn rotated_log_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map_or_else(|| OsString::from("difflore.log"), OsString::from);
    name.push(".1");
    path.with_file_name(name)
}

fn rotate_log_if_large(path: &Path, max_bytes: u64) -> std::io::Result<()> {
    let Ok(metadata) = std::fs::metadata(path) else {
        return Ok(());
    };
    if metadata.len() <= max_bytes {
        return Ok(());
    }

    let rotated = rotated_log_path(path);
    let keep = max_bytes.min(metadata.len()) as usize;
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::End(-(keep as i64)))?;
    let mut tail = vec![0; keep];
    file.read_exact(&mut tail)?;
    std::fs::write(&rotated, tail)?;
    std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
        .map(|_| ())
}

#[cfg(unix)]
fn configure_detached(cmd: &mut Command) {
    use std::os::unix::process::CommandExt as _;
    // SAFETY: `pre_exec` runs in the forked child after `fork` and before
    // `exec`. Only async-signal-safe calls are permitted there; `setsid(2)` is
    // explicitly async-signal-safe. We make no allocations and touch no shared
    // state. Detaching into a new session is what lets the daemon outlive the
    // shim's controlling terminal (no SIGHUP on shim exit).
    #[allow(unsafe_code)]
    unsafe {
        cmd.pre_exec(|| {
            // setsid fails only if the caller is already a process-group
            // leader, which a freshly forked child is not; treat any error as
            // non-fatal so exec still proceeds (the daemon may catch SIGHUP,
            // but losing one daemon only costs a re-spawn).
            let _ = libc::setsid();
            Ok(())
        });
    }
}

#[cfg(windows)]
const DETACHED_PROCESS: u32 = 0x0000_0008;
#[cfg(windows)]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
#[cfg(windows)]
const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

#[cfg(windows)]
const fn windows_detached_creation_flags() -> u32 {
    DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB
}

#[cfg(windows)]
fn spawn_detached_no_inherit(main_bin: &Path, args: &[&str]) -> Result<(), String> {
    use std::mem::size_of;
    use std::os::windows::ffi::OsStrExt as _;
    use std::ptr;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        CreateProcessW, PROCESS_INFORMATION, STARTUPINFOW,
    };

    let mut app_name: Vec<u16> = main_bin.as_os_str().encode_wide().chain([0]).collect();
    let mut command_line = windows_command_line(main_bin, args);
    let startup_info = STARTUPINFOW {
        cb: size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    let mut process_info = PROCESS_INFORMATION::default();

    // bInheritHandles must stay FALSE. Hook runners capture stdout/stderr with
    // pipes; if the long-lived daemon inherits those handles, the foreground
    // hook can exit while the runner still waits forever for pipe EOF.
    #[allow(unsafe_code)]
    let created = unsafe {
        CreateProcessW(
            app_name.as_mut_ptr(),
            command_line.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            0,
            windows_detached_creation_flags(),
            ptr::null(),
            ptr::null(),
            &raw const startup_info,
            &raw mut process_info,
        )
    };

    if created == 0 {
        return Err(format!(
            "spawn detached process failed: {}",
            std::io::Error::last_os_error()
        ));
    }

    #[allow(unsafe_code)]
    unsafe {
        CloseHandle(process_info.hThread);
        CloseHandle(process_info.hProcess);
    }

    Ok(())
}

#[cfg(windows)]
fn windows_command_line(main_bin: &Path, args: &[&str]) -> Vec<u16> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt as _;

    let mut out = Vec::new();
    append_quoted_windows_arg(main_bin.as_os_str().encode_wide(), &mut out);
    for arg in args {
        out.push(b' ' as u16);
        append_quoted_windows_arg(OsStr::new(arg).encode_wide(), &mut out);
    }
    out.push(0);
    out
}

#[cfg(windows)]
fn append_quoted_windows_arg<I>(units: I, out: &mut Vec<u16>)
where
    I: IntoIterator<Item = u16>,
{
    let units: Vec<u16> = units.into_iter().collect();
    let needs_quotes = units.is_empty()
        || units
            .iter()
            .any(|&unit| unit == b' ' as u16 || unit == b'\t' as u16 || unit == b'"' as u16);

    if !needs_quotes {
        out.extend(units);
        return;
    }

    out.push(b'"' as u16);
    let mut backslashes = 0usize;
    for unit in units {
        if unit == b'\\' as u16 {
            backslashes += 1;
            continue;
        }

        if unit == b'"' as u16 {
            out.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2 + 1));
        } else {
            out.extend(std::iter::repeat_n(b'\\' as u16, backslashes));
        }
        out.push(unit);
        backslashes = 0;
    }

    out.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2));
    out.push(b'"' as u16);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn main_binary_sits_beside_the_shim() {
        // Whatever current_exe resolves to in the test harness, the derived
        // main binary must be its sibling (same parent dir, `difflore` name).
        if let Some(main) = main_binary_path() {
            let shim = std::env::current_exe().expect("current_exe in test");
            assert_eq!(main.parent(), shim.parent());
            let expected = format!("difflore{}", std::env::consts::EXE_SUFFIX);
            assert_eq!(
                main.file_name().and_then(|n| n.to_str()),
                Some(expected.as_str())
            );
        }
    }

    #[test]
    fn spawn_at_nonexistent_binary_errors_without_panicking() {
        // A bad binary path must surface as Err (caller swallows it), never a
        // panic — the shim's fallback must always run.
        let bogus = Path::new("/nonexistent/difflore-binary-xyz");
        let result = spawn_daemon_at(bogus, "deadbeef0000");
        assert!(result.is_err(), "spawning a missing binary should error");
    }

    #[test]
    fn spawn_outbox_daemon_at_nonexistent_binary_errors_without_panicking() {
        let bogus = Path::new("/nonexistent/difflore-binary-xyz");
        let result = spawn_outbox_daemon_at(bogus);
        assert!(result.is_err(), "spawning a missing binary should error");
    }

    #[test]
    fn daemon_log_rotation_keeps_previous_large_file_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hook-daemon.log");
        std::fs::write(&path, "0123456789abcdef").unwrap();

        rotate_log_if_large(&path, 8).unwrap();

        let rotated = rotated_log_path(&path);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
        assert_eq!(std::fs::read_to_string(rotated).unwrap(), "89abcdef");
    }

    #[test]
    fn daemon_log_rotation_leaves_small_file_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory-autopilot.log");
        std::fs::write(&path, "small").unwrap();

        rotate_log_if_large(&path, 8).unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "small");
        assert!(!rotated_log_path(&path).exists());
    }

    #[cfg(windows)]
    #[test]
    fn windows_daemons_break_away_from_hook_runner_job() {
        assert_ne!(
            windows_detached_creation_flags() & CREATE_BREAKAWAY_FROM_JOB,
            0,
            "detached daemons must not keep Codex/Claude hook jobs alive"
        );
    }
}
