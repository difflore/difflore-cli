//! Best-effort detached spawn of the warm hook-forward daemon.
//!
//! Called by the `difflore-hook` shim on a cache miss (auto mode): the current
//! hook still falls back in-process, but a daemon is launched so the *next*
//! hook hits the warm path. Everything here is best-effort — any failure is
//! swallowed (optionally logged) and never escalates into a hook error.
//!
//! The daemon must outlive the short-lived shim: on Unix we `setsid` in a
//! `pre_exec` hook so it leaves the shim's session and survives SIGHUP; on
//! Windows we set `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP`. We never
//! `wait()` — on Unix `setsid` reparents the orphan to init; on Windows the
//! detached process is independent.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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

/// Like [`spawn_daemon_detached`] but with an explicit binary path.
pub fn spawn_daemon_at(main_bin: &Path, project_hash: &str) -> Result<(), String> {
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

/// Where the detached daemon's stderr goes. Routes to
/// `data_home/logs/hook-daemon.log` when resolvable (handy for diagnosing a
/// daemon that exits early), else `null`. Best-effort: any path/IO failure
/// degrades to `null` rather than aborting the spawn.
fn daemon_log_stdio() -> Stdio {
    let Ok(home) = difflore_core::infra::paths::data_home() else {
        return Stdio::null();
    };
    let logs = home.join("logs");
    if std::fs::create_dir_all(&logs).is_err() {
        return Stdio::null();
    }
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(logs.join("hook-daemon.log"))
    {
        Ok(file) => Stdio::from(file),
        Err(_) => Stdio::null(),
    }
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
fn configure_detached(cmd: &mut Command) {
    use std::os::windows::process::CommandExt as _;
    // DETACHED_PROCESS: no inherited console (the shim may be console-attached).
    // CREATE_NEW_PROCESS_GROUP: the daemon is not killed by Ctrl-C sent to the
    // shim's group. Together these are the Windows analogue of setsid.
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
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
            assert_eq!(main.file_name().and_then(|n| n.to_str()), Some(expected.as_str()));
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
}
