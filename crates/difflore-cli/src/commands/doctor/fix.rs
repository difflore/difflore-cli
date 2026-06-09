//! `difflore doctor --fix` diagnostic repair pass.
//!
//! Strict, narrow scope — only the mechanically safe subset:
//!
//!   1. Missing `~/.difflore/` directory — create it.
//!   2. MCP install drift — re-run the MCP installer for detected
//!      agents that are not yet wired to DiffLore, conflict, or have
//!      stale canonical record / hook-surface drift.
//!   3. Stale `difflore-hook` shim — diagnose only. We never invoke
//!      `cargo install` on the user's behalf; that's an explicit
//!      decision they make. We just print a clear, copy-pasteable
//!      command when the shim is missing or older than the running
//!      `difflore` binary.
//!
//! Everything else doctor surfaces (cloud login, provider/API keys,
//! BFS env vars, DB migrations) is left to the user — see the
//! `decline_*` helpers for the messaging.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use crate::mcp_install;
use crate::style;

const HOOK_SHIM_STALE_GRACE: Duration = Duration::from_secs(5);

/// Cheap pre-check: is there at least one repair the diagnostic pass
/// could apply right now? Used to gate the "Run `difflore doctor
/// --fix`" nudge on the diagnostic surface so a healthy install
/// never sees it.
pub(crate) fn has_fixable() -> bool {
    if !difflore_dir_exists() {
        return true;
    }
    if !mcp_install::detect_install_repair_targets().is_empty() {
        return true;
    }
    matches!(
        check_hook_shim(),
        HookShimState::Missing | HookShimState::Stale { .. }
    )
}

/// Run the fix pass. Prints a short banner, then walks the three
/// auto-repairable categories. Each step prints a single result line via
/// `style::ok` / `style::warn`. Closes with the canonical next-step
/// bridge so the user knows how to verify.
pub(crate) fn run_fix_pass() {
    let actions = collect_actions();
    println!();
    if actions.is_empty() {
        println!(
            "  {} {}",
            style::emerald(style::sym::OK),
            style::ok("Nothing to repair — diagnostic surface is clean."),
        );
        decline_notices();
        return;
    }
    println!(
        "  {} {} {} {}",
        style::emerald(style::sym::TIP),
        style::pewter("Repairing"),
        style::ident(&actions.len().to_string()),
        style::pewter(if actions.len() == 1 {
            "item…"
        } else {
            "items…"
        }),
    );

    for action in actions {
        match action {
            Action::CreateDiffloreDir => apply_create_difflore_dir(),
            Action::InstallMcpDrift(names) => apply_install_mcp_drift(&names),
            Action::HookShim(state) => apply_hook_shim(state),
        }
    }

    decline_notices();

    println!();
    println!(
        "  {} {} {}",
        style::pewter("next:"),
        style::cmd("difflore"),
        style::pewter("to verify."),
    );
}

// ── Action planning ────────────────────────────────────────────────

enum Action {
    CreateDiffloreDir,
    InstallMcpDrift(Vec<String>),
    HookShim(HookShimState),
}

fn collect_actions() -> Vec<Action> {
    let mut out: Vec<Action> = Vec::new();
    if !difflore_dir_exists() {
        out.push(Action::CreateDiffloreDir);
    }
    let drift = mcp_install::detect_install_repair_targets();
    if !drift.is_empty() {
        out.push(Action::InstallMcpDrift(drift));
    }
    let shim = check_hook_shim();
    if !matches!(shim, HookShimState::Ok) {
        out.push(Action::HookShim(shim));
    }
    out
}

// ── 1. ~/.difflore/ directory ──────────────────────────────────────

fn difflore_dir_path() -> Option<PathBuf> {
    difflore_core::paths::data_home().ok()
}

fn difflore_dir_exists() -> bool {
    difflore_dir_path().is_some_and(|p| p.exists())
}

fn apply_create_difflore_dir() {
    let Some(dir) = difflore_dir_path() else {
        println!(
            "  {} {}",
            style::amber(style::sym::WARN),
            style::warn("could not resolve ~/.difflore/ — HOME not set?"),
        );
        return;
    };
    match std::fs::create_dir_all(&dir) {
        Ok(()) => println!(
            "  {} {} {}",
            style::emerald(style::sym::OK),
            style::ok("created"),
            style::ident(&dir.display().to_string()),
        ),
        Err(e) => println!(
            "  {} {} {} ({e})",
            style::amber(style::sym::WARN),
            style::warn("failed to create"),
            style::ident(&dir.display().to_string()),
        ),
    }
}

// ── 2. MCP install drift ───────────────────────────────────────────

fn apply_install_mcp_drift(names: &[String]) {
    println!(
        "  {} {} {}",
        style::emerald(style::sym::TIP),
        style::pewter("MCP drift detected:"),
        style::ident(&names.join(", ")),
    );
    // `install_all` is idempotent — re-running picks up newly detected
    // agents without re-prompting for already-installed ones, which is
    // exactly what we want for drift recovery.
    mcp_install::install_all(false);
    println!(
        "  {} {}",
        style::emerald(style::sym::OK),
        style::ok("MCP install drift repaired"),
    );
}

// ── 3. difflore-hook shim ──────────────────────────────────────────

enum HookShimState {
    Ok,
    Missing,
    Stale {
        shim_path: PathBuf,
        cli_path: PathBuf,
    },
}

fn check_hook_shim() -> HookShimState {
    let Ok(cli) = std::env::current_exe() else {
        // Can't compare without a CLI path — treat as ok rather than
        // emit a spurious warning.
        return HookShimState::Ok;
    };
    let Some(shim) = hook_shim_for_cli(&cli).or_else(which_hook_shim) else {
        return HookShimState::Missing;
    };
    let shim_mtime = file_mtime(&shim);
    let cli_mtime = file_mtime(&cli);
    match (shim_mtime, cli_mtime) {
        (Some(s), Some(c)) if shim_older_than_cli(s, c) => HookShimState::Stale {
            shim_path: shim,
            cli_path: cli,
        },
        _ => HookShimState::Ok,
    }
}

fn hook_shim_for_cli(cli: &std::path::Path) -> Option<PathBuf> {
    let exe_name = format!("difflore-hook{}", std::env::consts::EXE_SUFFIX);
    let candidate = cli.parent()?.join(exe_name);
    candidate.is_file().then_some(candidate)
}

fn which_hook_shim() -> Option<PathBuf> {
    let exe_name = format!("difflore-hook{}", std::env::consts::EXE_SUFFIX);
    let path = difflore_core::env::var_os(difflore_core::env::PATH)?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(&exe_name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn file_mtime(p: &std::path::Path) -> Option<SystemTime> {
    std::fs::metadata(p).ok().and_then(|m| m.modified().ok())
}

fn shim_older_than_cli(shim_mtime: SystemTime, cli_mtime: SystemTime) -> bool {
    cli_mtime
        .duration_since(shim_mtime)
        .is_ok_and(|age| age > HOOK_SHIM_STALE_GRACE)
}

fn apply_hook_shim(state: HookShimState) {
    match state {
        HookShimState::Ok => {}
        HookShimState::Missing => {
            println!(
                "  {} {}",
                style::amber(style::sym::WARN),
                style::warn("difflore-hook shim not found next to difflore or on PATH"),
            );
            println!(
                "    {} {} {}",
                style::pewter(style::sym::TIP),
                style::pewter("re-run"),
                style::cmd(
                    "cargo install --git https://github.com/difflore/difflore-cli difflore-cli"
                ),
            );
        }
        HookShimState::Stale {
            shim_path,
            cli_path,
        } => {
            println!(
                "  {} {} {}",
                style::amber(style::sym::WARN),
                style::warn("difflore-hook is older than"),
                style::ident(&cli_path.display().to_string()),
            );
            println!(
                "    {} {} {}",
                style::pewter(style::sym::TIP),
                style::pewter("shim:"),
                style::ident(&shim_path.display().to_string()),
            );
            println!(
                "    {} {} {}",
                style::pewter(style::sym::TIP),
                style::pewter("re-run"),
                style::cmd(
                    "cargo install --git https://github.com/difflore/difflore-cli difflore-cli"
                ),
            );
        }
    }
}

// ── Decline notices ────────────────────────────────────────────────

/// Print the one-line "we won't auto-touch this" notices for the
/// privacy-sensitive surfaces. Only emitted under `--fix` so the
/// default doctor view stays uncluttered.
fn decline_notices() {
    println!();
    println!(
        "  {} {}",
        style::pewter(style::sym::BULLET),
        style::pewter("cloud login: never auto-touched — run `difflore cloud login` if needed"),
    );
    println!(
        "  {} {}",
        style::pewter(style::sym::BULLET),
        style::pewter("provider / API keys: never auto-touched — run `difflore providers setup`",),
    );
    println!(
        "  {} {}",
        style::pewter(style::sym::BULLET),
        style::pewter(
            "DB migrations: automatic on startup; back up the DB before switching binaries"
        ),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_shim_for_cli_finds_sibling_shim() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let cli = tmp
            .path()
            .join(format!("difflore{}", std::env::consts::EXE_SUFFIX));
        let shim = tmp
            .path()
            .join(format!("difflore-hook{}", std::env::consts::EXE_SUFFIX));
        std::fs::write(&shim, b"shim").expect("write shim");

        assert_eq!(hook_shim_for_cli(&cli), Some(shim));
    }

    #[test]
    fn hook_shim_for_cli_ignores_missing_sibling_shim() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let cli = tmp
            .path()
            .join(format!("difflore{}", std::env::consts::EXE_SUFFIX));

        assert!(hook_shim_for_cli(&cli).is_none());
    }

    #[test]
    fn shim_older_than_cli_tolerates_install_timestamp_skew() {
        let shim = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let cli = SystemTime::UNIX_EPOCH + Duration::from_secs(104);

        assert!(!shim_older_than_cli(shim, cli));
    }

    #[test]
    fn shim_older_than_cli_flags_meaningful_stale_gap() {
        let shim = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let cli = SystemTime::UNIX_EPOCH + Duration::from_secs(110);

        assert!(shim_older_than_cli(shim, cli));
    }
}
