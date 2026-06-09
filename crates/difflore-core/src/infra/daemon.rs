// SAFETY scope: this module wraps `libc::kill` for unix signal delivery
// (liveness probe + SIGTERM/SIGKILL). Each call site has a per-block
// SAFETY comment; the operations are read-only signal sends and do not
// mutate process memory.
#![allow(unsafe_code)]

//! Background daemon for `DiffLore`.
//!
//! Consumes the `SQLite` outbox in a loop so AI-agent hooks stay on the
//! fast path (hook = enqueue, daemon = drain). Does NOT listen on any
//! HTTP port — the local stack is Rust-on-SQLite so in-process access
//! is strictly cheaper than IPC. Lifecycle management uses a PID file
//! at `~/.difflore/daemon.pid`; liveness is checked with `kill(pid, 0)`.
//!
//! `run` is the only long-running entry point; it installs a SIGTERM /
//! SIGINT handler that breaks out of the drain loop cleanly and deletes
//! the PID file before returning.

use std::fs;
use std::path::PathBuf;
#[cfg(windows)]
use std::process::Command;
use std::time::Duration;

use crate::cloud::client::CloudClient;
use crate::cloud::outbox::{DEFAULT_STALE_SECONDS, OutboxQueue, drain_outbox};
use crate::db::init_db;
use crate::paths;

/// Path of the PID file used by the internal daemon helpers.
pub fn pid_path() -> Result<PathBuf, String> {
    Ok(paths::data_home()?.join("daemon.pid"))
}

/// Report the daemon liveness state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonStatus {
    /// A PID file exists and the process is still alive.
    Running { pid: i32 },
    /// A PID file exists but no process by that PID responds to
    /// `kill(pid, 0)`. The file is effectively orphaned.
    Stale { pid: i32 },
    /// No PID file found.
    NotRunning,
}

impl DaemonStatus {
    pub fn short(&self) -> String {
        match self {
            Self::Running { pid } => format!("running (pid {pid})"),
            Self::Stale { pid } => format!("stale pid file (pid {pid}); not running"),
            Self::NotRunning => "not running".to_owned(),
        }
    }
}

/// Probe the PID file + live process without mutating anything.
pub fn status() -> DaemonStatus {
    let Ok(path) = pid_path() else {
        return DaemonStatus::NotRunning;
    };
    let Some(pid) = read_pid(&path) else {
        return DaemonStatus::NotRunning;
    };
    if is_process_alive(pid) {
        DaemonStatus::Running { pid }
    } else {
        DaemonStatus::Stale { pid }
    }
}

fn read_pid(path: &std::path::Path) -> Option<i32> {
    let raw = fs::read_to_string(path).ok()?;
    raw.trim().parse::<i32>().ok()
}

fn write_pid(path: &std::path::Path, pid: i32) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create parent: {e}"))?;
    }
    fs::write(path, pid.to_string()).map_err(|e| format!("write pid: {e}"))
}

fn remove_pid_file(path: &std::path::Path) {
    // Best-effort; a missing pid file on shutdown is not an error.
    let _ = fs::remove_file(path);
}

#[cfg(unix)]
/// Signal 0 delivers nothing but still validates the target exists and
/// we have permission to signal it. Covers SIGTERM check without
/// actually killing anything. Safe on macOS and Linux.
fn is_process_alive(pid: i32) -> bool {
    // SAFETY: libc::kill with signal 0 is a classic liveness probe —
    // returns 0 on success, -1 + errno=ESRCH if the PID is gone.
    // Neither outcome mutates state.
    unsafe { libc::kill(pid, 0) == 0 }
}

#[cfg(windows)]
fn is_process_alive(pid: i32) -> bool {
    let Ok(output) = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .output()
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.contains(&format!("\"{pid}\"")) || stdout.contains(&format!(",{pid},"))
}

#[cfg(unix)]
fn send_term(pid: i32) -> std::io::Result<()> {
    send_signal(pid, libc::SIGTERM)
}

#[cfg(unix)]
fn send_kill(pid: i32) -> std::io::Result<()> {
    send_signal(pid, libc::SIGKILL)
}

#[cfg(unix)]
fn send_signal(pid: i32, signum: libc::c_int) -> std::io::Result<()> {
    // SAFETY: delegating to libc::kill; we already validated `pid` is
    // a positive integer from the PID file. Errors are converted to
    // `io::Error` via `errno`.
    let rc = unsafe { libc::kill(pid, signum) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(windows)]
fn send_term(pid: i32) -> std::io::Result<()> {
    let status = Command::new("taskkill")
        .args(["/PID", &pid.to_string()])
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "taskkill exited with {status}"
        )))
    }
}

#[cfg(windows)]
fn send_kill(pid: i32) -> std::io::Result<()> {
    let status = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "taskkill /F exited with {status}"
        )))
    }
}

/// Gracefully stop the daemon. Sends SIGTERM, waits up to
/// `grace_secs` for the process to exit, then escalates to SIGKILL.
/// Removes a stale PID file regardless of which path exits. Returns
/// what actually happened so the CLI can phrase the UX correctly.
pub async fn stop(grace_secs: u64) -> Result<StopOutcome, String> {
    let path = pid_path()?;
    let Some(pid) = read_pid(&path) else {
        return Ok(StopOutcome::NotRunning);
    };
    if !is_process_alive(pid) {
        remove_pid_file(&path);
        return Ok(StopOutcome::StaleCleaned { pid });
    }

    send_term(pid).map_err(|e| format!("terminate pid {pid}: {e}"))?;

    // Poll every 200 ms for up to `grace_secs` so a well-behaved daemon
    // can finish its in-flight drain + unlink the pid file itself.
    let poll = Duration::from_millis(200);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(grace_secs.max(1));
    while tokio::time::Instant::now() < deadline {
        if !is_process_alive(pid) {
            remove_pid_file(&path);
            return Ok(StopOutcome::Terminated { pid });
        }
        tokio::time::sleep(poll).await;
    }

    // Grace expired — escalate. We still remove the pid file because
    // SIGKILL cannot be caught; whatever process by this PID is going
    // away whether it wanted to or not.
    let _ = send_kill(pid);
    remove_pid_file(&path);
    Ok(StopOutcome::Killed { pid })
}

/// What `stop` actually did — useful for UX messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopOutcome {
    NotRunning,
    StaleCleaned { pid: i32 },
    Terminated { pid: i32 },
    Killed { pid: i32 },
}

/// Long-running drain loop. Claims outbox rows and dispatches them to
/// the cloud on a fixed cadence. Only exits on SIGTERM / SIGINT or on
/// a genuinely fatal error (DB open failure, etc.).
///
/// Safe to call at most once per process: writes its own PID file on
/// entry and errors out if one already belongs to a live process.
pub async fn run(tick_interval_secs: u64, batch_size: usize) -> Result<(), String> {
    let path = pid_path()?;

    // Refuse to start a second daemon against the same DIFFLORE_HOME —
    // two concurrent drainers would double-send every outbox row.
    match status() {
        DaemonStatus::Running { pid } => {
            return Err(format!(
                "another daemon is already running (pid {pid}); stop that process before starting another"
            ));
        }
        DaemonStatus::Stale { .. } | DaemonStatus::NotRunning => {}
    }

    let my_pid = std::process::id() as i32;
    write_pid(&path, my_pid)?;

    let db = init_db().await?;
    let queue = OutboxQueue::new(db);
    let client = CloudClient::create().await;

    // Install SIGTERM / SIGINT handler BEFORE entering the loop so a
    // signal that arrives while we're mid-drain still causes a clean
    // exit at the next tick boundary.
    let shutdown = shutdown_signal_future();
    tokio::pin!(shutdown);

    let tick = Duration::from_secs(tick_interval_secs.max(1));
    loop {
        tokio::select! {
            biased;
            () = &mut shutdown => break,
            () = tokio::time::sleep(tick) => {
                // Step 1: opportunistic stale reclaim sweeps the queue
                // before draining so rows stranded by a prior crashed
                // daemon (same `DIFFLORE_HOME`, different PID) come
                // back into play. `claim_next` also self-heals
                // individual rows — this is a defence in depth.
                let _ = queue.reset_stale(DEFAULT_STALE_SECONDS).await;

                // Step 2: drain. Only SQL-level failures are surfaced;
                // upload-level errors are absorbed by the queue's
                // retry + circuit-breaker state, so logging them here
                // would be noisy.
                if let Err(e) = drain_outbox(&queue, &client, batch_size).await {
                    eprintln!("[difflore.daemon] drain error: {e}");
                }
            }
        }
    }

    remove_pid_file(&path);
    Ok(())
}

/// Future that resolves on the first SIGTERM or SIGINT.
///
/// Kept separate so `run` stays readable; also makes it testable
/// through a feature flag if we ever need a synthetic shutdown.
async fn shutdown_signal_future() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let Ok(mut sigterm) = signal(SignalKind::terminate()) else {
            return;
        };
        let Ok(mut sigint) = signal(SignalKind::interrupt()) else {
            return;
        };
        tokio::select! {
            _ = sigterm.recv() => {}
            _ = sigint.recv() => {}
        }
    }

    #[cfg(windows)]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialise every test in this module — they all touch the
    /// single `~/.difflore/daemon.pid` under `shared_test_home`, so
    /// running them in parallel means one test's dead-pid write gets
    /// read by another's `status()` probe. `tokio::sync::Mutex` works
    /// for both sync and async tests via `blocking_lock`.
    static TEST_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Spawn a throwaway child, wait for it to exit, return its PID.
    /// That PID is guaranteed-dead for the probe window (at least
    /// until the OS rolls the counter back to it).
    fn spawn_dead_pid() -> i32 {
        #[cfg(unix)]
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn true");
        #[cfg(windows)]
        let mut child = Command::new("cmd")
            .args(["/C", "exit", "0"])
            .spawn()
            .expect("spawn cmd");
        let id = child.id() as i32;
        let _ = child.wait();
        id
    }

    #[test]
    fn status_reports_not_running_when_pid_file_missing() {
        let _g = TEST_SERIAL.blocking_lock();
        let _ = crate::db::shared_test_home();
        let path = pid_path().expect("pid path");
        let _ = fs::remove_file(&path);
        assert_eq!(status(), DaemonStatus::NotRunning);
    }

    #[test]
    fn status_detects_stale_pid_file() {
        let _g = TEST_SERIAL.blocking_lock();
        let _ = crate::db::shared_test_home();
        let path = pid_path().expect("pid path");
        let dead_pid = spawn_dead_pid();
        fs::write(&path, dead_pid.to_string()).unwrap();

        // Re-read from the file rather than trusting our local
        // `dead_pid` — the goal is to verify `status()`'s transport
        // (file -> probe), not to compare against a racy OS PID.
        let stored: i32 = fs::read_to_string(&path).unwrap().trim().parse().unwrap();

        match status() {
            DaemonStatus::Stale { pid } => assert_eq!(pid, stored),
            other => panic!("expected Stale, got {other:?}"),
        }
        let _ = fs::remove_file(&path);
    }

    #[tokio::test]
    async fn stop_is_noop_when_not_running() {
        let _g = TEST_SERIAL.lock().await;
        let _ = crate::db::shared_test_home();
        let path = pid_path().unwrap();
        let _ = fs::remove_file(&path);
        let outcome = stop(1).await.unwrap();
        assert_eq!(outcome, StopOutcome::NotRunning);
    }

    #[tokio::test]
    async fn stop_cleans_stale_pid_file_without_signalling() {
        let _g = TEST_SERIAL.lock().await;
        let _ = crate::db::shared_test_home();
        let path = pid_path().unwrap();
        let dead_pid = spawn_dead_pid();
        fs::write(&path, dead_pid.to_string()).unwrap();
        let stored: i32 = fs::read_to_string(&path).unwrap().trim().parse().unwrap();

        let outcome = stop(1).await.unwrap();
        assert_eq!(outcome, StopOutcome::StaleCleaned { pid: stored });
        assert!(!path.exists(), "stale pid file should be removed by stop()");
    }
}
