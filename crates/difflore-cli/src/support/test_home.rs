//! Shared test harness for pinning `DIFFLORE_HOME` to one process-unique
//! tempdir. The single `unsafe` `set_var` and its SAFETY justification live
//! here so reviewers audit the isolation invariant in exactly one place
//! instead of in every test module that needs an isolated index DB.

#![allow(unsafe_code)] // reason: `env::set_var` is unsafe in 2024 edition; the OnceLock invariant below makes it sound.

use std::sync::OnceLock;
use tempfile::TempDir;

/// Point `DIFFLORE_HOME` at a process-unique tempdir so the per-project index
/// DB is isolated per test process. Without it, parallel nextest processes
/// contend on a shared on-disk index DB ("database is locked").
///
/// Idempotent: the tempdir is created and the env var written exactly once per
/// process; the returned reference stays valid for the process lifetime.
pub(crate) fn pin_test_home() -> &'static TempDir {
    static HOME: OnceLock<TempDir> = OnceLock::new();
    HOME.get_or_init(|| {
        let dir = TempDir::new().expect("create test home tempdir");
        // SAFETY: OnceLock guarantees this closure runs exactly once per
        // process; the env var is never removed afterwards, so no other
        // thread can observe a torn read/write of the environment.
        unsafe {
            std::env::set_var("DIFFLORE_HOME", dir.path());
        }
        dir
    })
}
