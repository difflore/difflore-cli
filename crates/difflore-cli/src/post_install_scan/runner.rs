//! Shell out to `difflore import-reviews --max-prs <N>` and report what
//! happened. Lives in its own module so the integration test can call
//! [`build_import_command`] without driving a real child process.
//!
//! Shells out rather than calling `commands::import_reviews::handle`
//! directly because the install path is sync, while `import_reviews` needs a
//! tokio runtime and a fully-built `CommandContext` (and could re-enter
//! `startup::ensure_ready`). Shelling out also lets the offer print the exact
//! command the user can re-run themselves.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::outcome::PostInstallScanOutcome;

/// Build the (program, argv) the runner will invoke. Public so tests can
/// verify the import command without spawning a real binary.
#[must_use]
pub fn build_import_command(exe: &Path, max_prs: u32) -> (PathBuf, Vec<OsString>) {
    let argv: Vec<OsString> = vec![
        "import-reviews".into(),
        "--max-prs".into(),
        max_prs.to_string().into(),
    ];
    (exe.to_path_buf(), argv)
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

/// Spawn `difflore import-reviews --max-prs <N>` in `cwd`, streaming the
/// child's stdout/stderr through. Returns [`PostInstallScanOutcome::ImportedReviews`]
/// on success, [`PostInstallScanOutcome::ImportFailed`] on non-zero exit.
///
/// `pr_count` / `rule_count` are best-effort: the child's stdout is not parsed
/// (that would couple us to the import's `--json` shape). For exact counts run
/// `difflore status --json` afterwards.
pub fn run_import(exe: &Path, cwd: &Path, max_prs: u32) -> PostInstallScanOutcome {
    let (program, argv) = build_import_command(exe, max_prs);

    let mut cmd = Command::new(&program);
    cmd.args(&argv).current_dir(cwd);
    // Force-disable capture in the child so it does not enqueue observations
    // into the outbox the parent is already managing.
    cmd.env(difflore_core::cloud::capture::DIFFLORE_CAPTURE_ENV, "false");

    let status = match cmd.status() {
        Ok(s) => s,
        Err(e) => {
            return PostInstallScanOutcome::ImportFailed {
                error: format!("failed to spawn `difflore import-reviews`: {e}"),
            };
        }
    };

    if status.success() {
        // We don't parse the child's stdout — same reasoning as the
        // module docstring. The success line the install caller prints
        // points at `difflore status` for the authoritative numbers.
        return PostInstallScanOutcome::ImportedReviews {
            pr_count: max_prs,
            rule_count: 0,
        };
    }

    PostInstallScanOutcome::ImportFailed {
        error: format!("`difflore import-reviews` exited with {status}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_argv_matches_documented_public_cli() {
        // The exact argv shape is load-bearing: the message we print to
        // the user mentions `difflore import-reviews --max-prs 5`, so
        // that's exactly what we must spawn. No --upload (project scope
        // invariant), no --json (we want streamed human-readable output).
        let exe = Path::new("/opt/difflore/bin/difflore");
        let (program, argv) = build_import_command(exe, 5);

        assert_eq!(program, exe);
        assert_eq!(
            argv,
            vec![
                OsString::from("import-reviews"),
                OsString::from("--max-prs"),
                OsString::from("5"),
            ]
        );
    }

    #[test]
    fn import_argv_honours_custom_max_prs() {
        let (_, argv) = build_import_command(Path::new("difflore"), 25);
        assert!(argv.iter().any(|a| a == "25"));
    }
}
