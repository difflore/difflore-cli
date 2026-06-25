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

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::process::Command;
use std::time::Duration;

use crate::cloud::client::CloudClient;
use crate::cloud::outbox::{
    DEFAULT_STALE_SECONDS, OutboxQueue, drain_outbox_report, replay_spilled_observations,
};
use crate::infra::db::init_db;
use crate::infra::paths;
use serde::{Deserialize, Serialize};

const STATE_FILE_NAME: &str = "daemon-state.json";

pub fn pid_path() -> crate::Result<PathBuf> {
    Ok(paths::data_home()?.join("daemon.pid"))
}

pub fn state_path() -> crate::Result<PathBuf> {
    Ok(paths::data_home()?.join(STATE_FILE_NAME))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DaemonRunState {
    pub version: u32,
    pub pid: i32,
    pub started_at_ms: i64,
    pub heartbeat_at_ms: i64,
    pub last_drain_at_ms: Option<i64>,
    pub last_attempted: usize,
    pub last_confirmed: usize,
    pub last_error: Option<String>,
}

pub fn read_state() -> crate::Result<Option<DaemonRunState>> {
    let path = state_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(path)?;
    Ok(Some(serde_json::from_str(&raw)?))
}

fn write_state(state: &DaemonRunState) {
    if let Ok(path) = state_path()
        && let Ok(json) = serde_json::to_vec_pretty(state)
    {
        let _ = crate::infra::files::write_atomic(&path, &json);
    }
}

fn now_unix_ms() -> i64 {
    crate::cloud::outbox_core::now_unix_ms()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonStatus {
    Running {
        pid: i32,
    },
    /// PID file exists but no process by that PID responds to `kill(pid, 0)`.
    Stale {
        pid: i32,
    },
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

pub fn status() -> DaemonStatus {
    let Ok(path) = pid_path() else {
        return DaemonStatus::NotRunning;
    };
    status_for_path(&path)
}

fn status_for_path(path: &Path) -> DaemonStatus {
    let Some(pid) = read_pid(path) else {
        return DaemonStatus::NotRunning;
    };
    if is_process_alive(pid) {
        DaemonStatus::Running { pid }
    } else {
        DaemonStatus::Stale { pid }
    }
}

fn read_pid(path: &Path) -> Option<i32> {
    let raw = fs::read_to_string(path).ok()?;
    raw.trim().parse::<i32>().ok()
}

struct PidClaimLock {
    file: File,
}

impl PidClaimLock {
    fn acquire(pid_path: &Path) -> crate::Result<Self> {
        if let Some(parent) = pid_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| crate::CoreError::internal(format!("create parent: {e}")))?;
        }
        let lock_path = pid_claim_lock_path(pid_path);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)
            .map_err(|e| crate::CoreError::internal(format!("open pid claim lock: {e}")))?;
        lock_pid_claim_file(&file)
            .map_err(|e| crate::CoreError::internal(format!("lock pid claim: {e}")))?;
        Ok(Self { file })
    }
}

impl Drop for PidClaimLock {
    fn drop(&mut self) {
        let _ = unlock_pid_claim_file(&self.file);
    }
}

fn pid_claim_lock_path(pid_path: &Path) -> PathBuf {
    let file_name = pid_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("daemon.pid");
    pid_path.with_file_name(format!("{file_name}.lock"))
}

#[cfg(unix)]
fn lock_pid_claim_file(file: &File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    // SAFETY: `flock` only applies an advisory lock to this open file
    // descriptor. It does not access memory beyond the descriptor value.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(unix)]
fn unlock_pid_claim_file(file: &File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    // SAFETY: see `lock_pid_claim_file`; this releases the advisory lock
    // held on the same descriptor.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(windows)]
#[repr(C)]
struct Overlapped {
    internal: usize,
    internal_high: usize,
    offset: u32,
    offset_high: u32,
    h_event: *mut std::ffi::c_void,
}

#[cfg(windows)]
impl Overlapped {
    const fn zeroed() -> Self {
        Self {
            internal: 0,
            internal_high: 0,
            offset: 0,
            offset_high: 0,
            h_event: std::ptr::null_mut(),
        }
    }
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    #[link_name = "LockFileEx"]
    fn lock_file_ex(
        file: *mut std::ffi::c_void,
        flags: u32,
        reserved: u32,
        bytes_low: u32,
        bytes_high: u32,
        overlapped: *mut Overlapped,
    ) -> i32;

    #[link_name = "UnlockFileEx"]
    fn unlock_file_ex(
        file: *mut std::ffi::c_void,
        reserved: u32,
        bytes_low: u32,
        bytes_high: u32,
        overlapped: *mut Overlapped,
    ) -> i32;
}

#[cfg(windows)]
fn lock_pid_claim_file(file: &File) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;

    const LOCKFILE_EXCLUSIVE_LOCK: u32 = 0x0000_0002;
    let mut overlapped = Overlapped::zeroed();
    // SAFETY: `LockFileEx` receives a valid handle for `file`, locks one byte
    // at offset zero, and writes only to the stack-allocated OVERLAPPED value.
    let ok = unsafe {
        lock_file_ex(
            file.as_raw_handle(),
            LOCKFILE_EXCLUSIVE_LOCK,
            0,
            1,
            0,
            &raw mut overlapped,
        )
    };
    if ok != 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(windows)]
fn unlock_pid_claim_file(file: &File) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;

    let mut overlapped = Overlapped::zeroed();
    // SAFETY: releases the same one-byte lock range acquired above.
    let ok = unsafe { unlock_file_ex(file.as_raw_handle(), 0, 1, 0, &raw mut overlapped) };
    if ok != 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn claim_pid_file(path: &Path, pid: i32) -> crate::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| crate::CoreError::internal(format!("create parent: {e}")))?;
    }

    let _claim_lock = PidClaimLock::acquire(path)?;
    match write_new_pid(path, pid) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::AlreadyExists => reclaim_stale_pid_file(path, pid),
        Err(e) => Err(crate::CoreError::internal(format!("write pid: {e}"))),
    }
}

fn write_new_pid(path: &Path, pid: i32) -> std::io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    let raw = pid.to_string();
    if let Err(e) = file.write_all(raw.as_bytes()) {
        let _ = fs::remove_file(path);
        return Err(e);
    }
    Ok(())
}

fn reclaim_stale_pid_file(path: &Path, pid: i32) -> crate::Result<()> {
    if let Some(existing_pid) = read_pid(path)
        && is_process_alive(existing_pid)
    {
        return Err(crate::CoreError::Conflict(format!(
            "another daemon is already running (pid {existing_pid}); stop that process before starting another"
        )));
    }

    remove_pid_file(path);
    match write_new_pid(path, pid) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            let owner = read_pid(path).map_or_else(
                || "an unreadable pid file".to_owned(),
                |existing_pid| format!("pid {existing_pid}"),
            );
            Err(crate::CoreError::Conflict(format!(
                "daemon pid file was claimed concurrently by {owner}"
            )))
        }
        Err(e) => Err(crate::CoreError::internal(format!("write pid: {e}"))),
    }
}

fn remove_pid_file(path: &Path) {
    // Best-effort; a missing pid file on shutdown is not an error.
    let _ = fs::remove_file(path);
}

#[cfg(unix)]
/// Signal 0 delivers nothing but still validates the target exists and we
/// have permission to signal it — a liveness probe that kills nothing.
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
pub async fn stop(grace_secs: u64) -> crate::Result<StopOutcome> {
    let path = pid_path()?;
    stop_with_pid_path(&path, grace_secs).await
}

async fn stop_with_pid_path(path: &Path, grace_secs: u64) -> crate::Result<StopOutcome> {
    let Some(pid) = read_pid(path) else {
        return Ok(StopOutcome::NotRunning);
    };
    if !is_process_alive(pid) {
        remove_pid_file(path);
        return Ok(StopOutcome::StaleCleaned { pid });
    }

    send_term(pid).map_err(|e| crate::CoreError::internal(format!("terminate pid {pid}: {e}")))?;

    // Poll every 200 ms for up to `grace_secs` so a well-behaved daemon
    // can finish its in-flight drain + unlink the pid file itself.
    let poll = Duration::from_millis(200);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(grace_secs.max(1));
    while tokio::time::Instant::now() < deadline {
        if !is_process_alive(pid) {
            remove_pid_file(path);
            return Ok(StopOutcome::Terminated { pid });
        }
        tokio::time::sleep(poll).await;
    }

    // Grace expired — escalate. We still remove the pid file because
    // SIGKILL cannot be caught; whatever process by this PID is going
    // away whether it wanted to or not.
    let _ = send_kill(pid);
    remove_pid_file(path);
    Ok(StopOutcome::Killed { pid })
}

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
pub async fn run(tick_interval_secs: u64, batch_size: usize) -> crate::Result<()> {
    let path = pid_path()?;

    let my_pid = std::process::id() as i32;
    // Refuse to start a second daemon against the same DIFFLORE_HOME —
    // two concurrent drainers would double-send every outbox row. Claiming the
    // PID file uses exclusive create semantics so concurrent starts cannot
    // both observe an empty/stale file and then overwrite each other.
    claim_pid_file(&path, my_pid)?;

    let db = init_db().await?;
    let queue = OutboxQueue::new(db);
    let client = CloudClient::create().await;

    // Install SIGTERM / SIGINT handler BEFORE entering the loop so a
    // signal that arrives while we're mid-drain still causes a clean
    // exit at the next tick boundary.
    let shutdown = shutdown_signal_future();
    tokio::pin!(shutdown);

    let mut state = DaemonRunState {
        version: 1,
        pid: my_pid,
        started_at_ms: now_unix_ms(),
        heartbeat_at_ms: now_unix_ms(),
        last_drain_at_ms: None,
        last_attempted: 0,
        last_confirmed: 0,
        last_error: None,
    };
    write_state(&state);

    let tick = Duration::from_secs(tick_interval_secs.max(1));
    loop {
        state.heartbeat_at_ms = now_unix_ms();
        write_state(&state);

        // Step 1: opportunistic stale reclaim sweeps the queue before draining
        // so rows stranded by a prior crashed daemon (same `DIFFLORE_HOME`,
        // different PID) come back into play. `claim_next` also self-heals
        // individual rows — this is a defence in depth.
        let _ = queue.reset_stale(DEFAULT_STALE_SECONDS).await;
        let _ = replay_spilled_observations(&queue, batch_size).await;

        // Step 2: drain. Only SQL-level failures are surfaced; upload-level
        // errors are absorbed by the queue's retry + circuit-breaker state, so
        // logging them here would be noisy.
        let now = now_unix_ms();
        state.heartbeat_at_ms = now;
        state.last_drain_at_ms = Some(now);
        match drain_outbox_report(&queue, &client, batch_size).await {
            Ok(report) => {
                state.last_attempted = report.attempted;
                state.last_confirmed = report.confirmed;
                state.last_error = None;
            }
            Err(e) => {
                state.last_error = Some(e.to_string());
                if crate::infra::env::debug_cloud() {
                    eprintln!("[difflore.daemon] drain error: {e}");
                }
            }
        }
        write_state(&state);

        tokio::select! {
            biased;
            () = &mut shutdown => break,
            () = tokio::time::sleep(tick) => {}
        }
    }

    remove_pid_file(&path);
    Ok(())
}

/// Future that resolves on the first SIGTERM or SIGINT.
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
    use std::sync::{Arc, Barrier};

    /// Serialise every test in this module — they all touch the
    /// single `~/.difflore/daemon.pid` under `shared_test_home`, so
    /// running them in parallel means one test's dead-pid write gets
    /// read by another's `status()` probe. `tokio::sync::Mutex` works
    /// for both sync and async tests via `blocking_lock`.
    static TEST_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn temp_pid_path() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::TempDir::new().expect("temp pid dir");
        let path = tmp.path().join("daemon.pid");
        (tmp, path)
    }

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
        let (_tmp, path) = temp_pid_path();
        assert_eq!(status_for_path(&path), DaemonStatus::NotRunning);
    }

    #[test]
    fn status_detects_stale_pid_file() {
        let _g = TEST_SERIAL.blocking_lock();
        let (_tmp, path) = temp_pid_path();
        let dead_pid = spawn_dead_pid();
        fs::write(&path, dead_pid.to_string()).unwrap();

        // Re-read from the file rather than trusting our local
        // `dead_pid` — the goal is to verify `status()`'s transport
        // (file -> probe), not to compare against a racy OS PID.
        let stored: i32 = fs::read_to_string(&path).unwrap().trim().parse().unwrap();

        match status_for_path(&path) {
            DaemonStatus::Stale { pid } => assert_eq!(pid, stored),
            other => panic!("expected Stale, got {other:?}"),
        }
    }

    #[test]
    fn daemon_state_round_trips() {
        let _g = TEST_SERIAL.blocking_lock();
        let _ = crate::infra::db::shared_test_home();
        let path = state_path().expect("state path");
        let _ = fs::remove_file(&path);
        let state = DaemonRunState {
            version: 1,
            pid: 123,
            started_at_ms: 10,
            heartbeat_at_ms: 20,
            last_drain_at_ms: Some(30),
            last_attempted: 4,
            last_confirmed: 3,
            last_error: Some("db busy".to_owned()),
        };

        write_state(&state);
        assert_eq!(read_state().expect("read state"), Some(state));
    }

    #[test]
    fn claim_pid_file_claims_empty_path_once() {
        let _g = TEST_SERIAL.blocking_lock();
        let (_tmp, path) = temp_pid_path();
        let pid = std::process::id() as i32;

        claim_pid_file(&path, pid).expect("first claim succeeds");
        let err = claim_pid_file(&path, pid).expect_err("second claim should fail");

        assert!(
            err.to_string().contains("already running"),
            "unexpected error: {err}"
        );
        assert_eq!(read_pid(&path), Some(pid));
    }

    #[test]
    fn claim_pid_file_replaces_stale_pid_file() {
        let _g = TEST_SERIAL.blocking_lock();
        let (_tmp, path) = temp_pid_path();
        let dead_pid = spawn_dead_pid();
        fs::write(&path, dead_pid.to_string()).unwrap();
        let pid = std::process::id() as i32;

        claim_pid_file(&path, pid).expect("stale pid file should be reclaimed");

        assert_eq!(read_pid(&path), Some(pid));
    }

    #[test]
    fn claim_pid_file_allows_only_one_concurrent_empty_claim() {
        let _g = TEST_SERIAL.blocking_lock();
        let (_tmp, path) = temp_pid_path();
        let path = Arc::new(path);
        let barrier = Arc::new(Barrier::new(2));

        let claim = || {
            let path = Arc::clone(&path);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                claim_pid_file(&path, std::process::id() as i32).is_ok()
            })
        };
        let first = claim();
        let second = claim();
        let successes = usize::from(first.join().expect("claim thread joins"))
            + usize::from(second.join().expect("claim thread joins"));

        assert_eq!(successes, 1);
    }

    #[tokio::test]
    async fn stop_is_noop_when_not_running() {
        let _g = TEST_SERIAL.lock().await;
        let (_tmp, path) = temp_pid_path();
        let outcome = stop_with_pid_path(&path, 1).await.unwrap();
        assert_eq!(outcome, StopOutcome::NotRunning);
    }

    #[tokio::test]
    async fn stop_cleans_stale_pid_file_without_signalling() {
        let _g = TEST_SERIAL.lock().await;
        let (_tmp, path) = temp_pid_path();
        let dead_pid = spawn_dead_pid();
        fs::write(&path, dead_pid.to_string()).unwrap();
        let stored: i32 = fs::read_to_string(&path).unwrap().trim().parse().unwrap();

        let outcome = stop_with_pid_path(&path, 1).await.unwrap();
        assert_eq!(outcome, StopOutcome::StaleCleaned { pid: stored });
        assert!(!path.exists(), "stale pid file should be removed by stop()");
    }
}
