//! Spawn `difflore import-reviews` in the background and report what was
//! queued. Lives in its own module so the integration test can call
//! [`build_import_command`] without driving a real child process.
//!
//! Shells out rather than calling `commands::import_reviews::handle`
//! directly because the install path is sync, while `import_reviews` needs a
//! tokio runtime and a fully-built `CommandContext` (and could re-enter
//! `startup::ensure_ready`). Shelling out also lets install print the exact
//! command the user can re-run themselves.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::outcome::PostInstallScanOutcome;

/// Build the (program, argv) the runner will invoke. Public so tests can
/// verify the import command without spawning a real binary.
#[must_use]
pub fn build_import_command(
    exe: &Path,
    max_prs: u32,
    since: &str,
    wall_timeout_secs: u64,
) -> (PathBuf, Vec<OsString>) {
    let argv: Vec<OsString> = vec![
        "import-reviews".into(),
        "--max-prs".into(),
        max_prs.to_string().into(),
        "--since".into(),
        since.into(),
        "--wall-timeout-secs".into(),
        wall_timeout_secs.to_string().into(),
    ];
    (exe.to_path_buf(), argv)
}

#[must_use]
pub fn build_agent_file_import_command(exe: &Path) -> (PathBuf, Vec<OsString>) {
    let argv: Vec<OsString> = vec!["memory".into(), "import-agent-files".into()];
    (exe.to_path_buf(), argv)
}

#[must_use]
pub fn since_date_utc(days: i64) -> String {
    let days = days.max(0);
    let date = chrono::Utc::now().date_naive() - chrono::Duration::days(days);
    date.format("%Y-%m-%d").to_string()
}

/// Locate the difflore binary to re-invoke. Prefers `current_exe()` so a dev
/// build re-uses the same binary it was launched from; falls back to `which`.
pub fn resolve_self_binary() -> Result<PathBuf, String> {
    if let Ok(exe) = std::env::current_exe() {
        let canon = exe.canonicalize().unwrap_or(exe);
        return Ok(canon);
    }
    which::which("difflore").map_err(|e| format!("could not locate `difflore` on PATH: {e}"))
}

/// Spawn `difflore import-reviews` in `cwd` as a detached child. Returns
/// [`PostInstallScanOutcome::ImportedReviews`] once the worker is queued; the
/// child enforces its own hidden wall-clock cap.
///
/// `pr_count` / `rule_count` are best-effort: the child's stdout is not parsed
/// (that would couple us to the import's `--json` shape). For exact counts run
/// `difflore status --json` afterwards.
pub fn run_import(
    exe: &Path,
    cwd: &Path,
    max_prs: u32,
    since: &str,
    wall_timeout_secs: u64,
) -> PostInstallScanOutcome {
    let (program, argv) = build_import_command(exe, max_prs, since, wall_timeout_secs);

    let mut cmd = Command::new(&program);
    cmd.args(&argv)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Force-disable capture in the child so it does not enqueue observations
    // into the outbox the parent is already managing.
    cmd.env(difflore_core::cloud::capture::DIFFLORE_CAPTURE_ENV, "false");
    configure_detached(&mut cmd);

    match cmd.spawn() {
        Ok(_) => PostInstallScanOutcome::ImportedReviews {
            pr_count: max_prs,
            rule_count: 0,
        },
        Err(e) => PostInstallScanOutcome::ImportFailed {
            error: format!("failed to spawn `difflore import-reviews`: {e}"),
        },
    }
}

pub fn run_agent_file_import(exe: &Path, cwd: &Path) -> Result<(), String> {
    let (program, argv) = build_agent_file_import_command(exe);
    let mut cmd = Command::new(&program);
    cmd.args(&argv)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd.env(difflore_core::cloud::capture::DIFFLORE_CAPTURE_ENV, "false");
    configure_detached(&mut cmd);
    cmd.spawn()
        .map(|_| ())
        .map_err(|e| format!("failed to spawn `difflore memory import-agent-files`: {e}"))
}

#[cfg(unix)]
fn configure_detached(cmd: &mut Command) {
    use std::os::unix::process::CommandExt as _;
    #[allow(unsafe_code)]
    unsafe {
        cmd.pre_exec(|| {
            let _ = libc::setsid();
            Ok(())
        });
    }
}

#[cfg(windows)]
fn configure_detached(cmd: &mut Command) {
    use std::os::windows::process::CommandExt as _;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_argv_matches_documented_public_cli() {
        // The exact argv shape is load-bearing: post-install must queue a
        // bounded local import, not an upload or local-agent distillation.
        let exe = Path::new("/opt/difflore/bin/difflore");
        let (program, argv) = build_import_command(exe, 50, "2026-02-25", 20);

        assert_eq!(program, exe);
        assert_eq!(
            argv,
            vec![
                OsString::from("import-reviews"),
                OsString::from("--max-prs"),
                OsString::from("50"),
                OsString::from("--since"),
                OsString::from("2026-02-25"),
                OsString::from("--wall-timeout-secs"),
                OsString::from("20"),
            ]
        );
    }

    #[test]
    fn import_argv_honours_custom_max_prs() {
        let (_, argv) = build_import_command(Path::new("difflore"), 25, "2026-01-01", 7);
        assert!(argv.iter().any(|a| a == "25"));
        assert!(argv.iter().any(|a| a == "7"));
    }

    #[test]
    fn agent_file_import_argv_matches_manual_rerun_command() {
        let exe = Path::new("/opt/difflore/bin/difflore");
        let (program, argv) = build_agent_file_import_command(exe);

        assert_eq!(program, exe);
        assert_eq!(
            argv,
            vec![
                OsString::from("memory"),
                OsString::from("import-agent-files"),
            ]
        );
    }

    #[test]
    fn since_date_utc_uses_iso_calendar_format() {
        let since = since_date_utc(120);
        assert_eq!(since.len(), 10);
        assert!(chrono::NaiveDate::parse_from_str(&since, "%Y-%m-%d").is_ok());
    }
}
