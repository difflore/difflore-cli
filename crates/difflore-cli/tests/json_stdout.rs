#![allow(clippy::expect_used)]

use std::process::Command;

#[test]
fn yes_json_command_writes_only_parseable_json_to_stdout() {
    let bin = std::env::var_os("CARGO_BIN_EXE_difflore").expect("difflore binary path");
    let repo = tempfile::tempdir().expect("temp repo");
    let home = repo.path().join("home");
    std::fs::create_dir_all(&home).expect("home dir");

    run_git(repo.path(), ["init"]);
    run_git(repo.path(), ["config", "user.email", "test@example.com"]);
    run_git(repo.path(), ["config", "user.name", "Test"]);
    std::fs::write(repo.path().join("README.md"), "hello").expect("fixture file");
    run_git(repo.path(), ["add", "README.md"]);
    run_git(repo.path(), ["commit", "-m", "init"]);

    let output = Command::new(bin)
        .current_dir(repo.path())
        .env("DIFFLORE_HOME", &home)
        .env("DIFFLORE_NO_COLOR", "1")
        .args(["status", "--json"])
        .output()
        .expect("run difflore");

    assert!(
        output.status.success(),
        "status={:?}\nstderr={}\nstdout={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );

    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    let trimmed = stdout.trim();
    assert!(!trimmed.is_empty(), "stdout should contain JSON");
    assert_eq!(
        trimmed.lines().count(),
        1,
        "--json stdout must not include human progress lines: {trimmed}"
    );

    let payload: serde_json::Value =
        serde_json::from_str(trimmed).expect("--json stdout should parse");
    assert!(payload.get("activeRules").is_some());
    assert!(payload.get("next").is_some());
}

fn run_git<const N: usize>(cwd: &std::path::Path, args: [&str; N]) {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git failed: {}\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
}
