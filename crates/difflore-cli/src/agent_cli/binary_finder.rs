//! Locate the binary on disk for each supported agent CLI.
//!
//! Strategy is the same one hivemind's `gate-runner.ts` uses:
//!
//!   1. Check a small, hand-curated list of well-known install paths
//!      for the agent on the current OS. Catches the case where the
//!      user installed via the official installer (which writes outside
//!      anything `PATH` covers — e.g. `~/.claude/local/claude.exe` on
//!      Windows) but their shell `PATH` doesn't include that dir.
//!   2. Fall back to `which::which(<command>)` — picks up anything the
//!      user installed via brew / apt / a manual `PATH` edit / npm-global.
//!
//! Returning the absolute path (rather than the bare command name) lets
//! `tokio::process::Command::new` skip its own `PATH` walk and gets us
//! deterministic behaviour when multiple installs exist.

use std::path::PathBuf;

use super::types::AgentKind;

/// Resolve the binary for `agent` on this host, or `None` if it can't
/// be located. `None` is the signal `dispatch_gate` uses to short-
/// circuit with an errored `GateResult` — callers never see the
/// `which::Error` directly.
#[must_use]
pub(super) fn find_binary(agent: AgentKind) -> Option<PathBuf> {
    let command = command_name(agent)?;

    for candidate in candidate_paths(agent) {
        if candidate.is_file() {
            return Some(candidate);
        }
    }

    which::which(command).ok()
}

/// Bare command name expected on `PATH`. `None` for agents with no
/// headless CLI today (`Windsurf`) so callers can short-circuit before
/// touching the filesystem.
#[must_use]
pub(super) const fn command_name(agent: AgentKind) -> Option<&'static str> {
    Some(match agent {
        AgentKind::ClaudeCode => "claude",
        AgentKind::Codex => "codex",
        AgentKind::Cursor => "cursor-agent",
        AgentKind::GeminiCli => "gemini",
        AgentKind::Windsurf => return None,
    })
}

/// Build the OS-specific list of well-known install paths to probe
/// before falling back to `which`. Order matters: official installers
/// first (these are the most surprising-but-common install locations),
/// then package-manager defaults.
fn candidate_paths(agent: AgentKind) -> Vec<PathBuf> {
    let mut paths = Vec::new();

    let home = dirs::home_dir();

    #[cfg(target_os = "windows")]
    {
        let exe = match agent {
            AgentKind::ClaudeCode => Some("claude.exe"),
            AgentKind::Codex => Some("codex.exe"),
            AgentKind::Cursor => Some("cursor-agent.exe"),
            AgentKind::GeminiCli => Some("gemini.exe"),
            AgentKind::Windsurf => None,
        };
        if let Some(exe) = exe {
            // Official-installer-style: `%USERPROFILE%\.claude\local\claude.exe`,
            // `%USERPROFILE%\.codex\bin\codex.exe`, etc.
            if let Some(home) = home.as_ref() {
                if let Some(home_dir) = home_subdir_for(agent) {
                    paths.push(home.join(home_dir).join("local").join(exe));
                    paths.push(home.join(home_dir).join("bin").join(exe));
                }
                paths.push(home.join("AppData").join("Local").join(exe));
                paths.push(
                    home.join("AppData")
                        .join("Roaming")
                        .join("npm")
                        .join(exe),
                );
            }
            // Env-var-driven `%LOCALAPPDATA%` / `%PROGRAMFILES%` — these
            // are usually under `%USERPROFILE%\AppData\Local` already
            // (covered above) but a tweaked `LOCALAPPDATA` deserves a
            // direct probe.
            if let Some(local) = std::env::var_os("LOCALAPPDATA") {
                let local = PathBuf::from(local);
                paths.push(local.join(exe));
                paths.push(local.join("Programs").join(exe));
            }
            if let Some(pf) = std::env::var_os("PROGRAMFILES") {
                paths.push(PathBuf::from(pf).join(exe));
            }
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        let exe = match agent {
            AgentKind::ClaudeCode => Some("claude"),
            AgentKind::Codex => Some("codex"),
            AgentKind::Cursor => Some("cursor-agent"),
            AgentKind::GeminiCli => Some("gemini"),
            AgentKind::Windsurf => None,
        };
        if let Some(exe) = exe {
            if let Some(home) = home.as_ref() {
                if let Some(home_dir) = home_subdir_for(agent) {
                    paths.push(home.join(home_dir).join("local").join(exe));
                    paths.push(home.join(home_dir).join("bin").join(exe));
                }
                paths.push(home.join(".local").join("bin").join(exe));
                paths.push(home.join(".npm-global").join("bin").join(exe));
            }
            paths.push(PathBuf::from("/usr/local/bin").join(exe));
            paths.push(PathBuf::from("/opt/homebrew/bin").join(exe));
            paths.push(PathBuf::from("/usr/bin").join(exe));
        }
    }

    paths
}

/// The `~/.X/` home subdir that holds the official-installer output
/// for each agent. Returning `None` means "no known per-user install
/// location" — fall back to PATH / `which`.
#[must_use]
const fn home_subdir_for(agent: AgentKind) -> Option<&'static str> {
    Some(match agent {
        AgentKind::ClaudeCode => ".claude",
        AgentKind::Codex => ".codex",
        AgentKind::Cursor => ".cursor",
        AgentKind::GeminiCli => ".gemini",
        AgentKind::Windsurf => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windsurf_has_no_command_name() {
        // Windsurf is wired into the enum for symmetry with the hook
        // adapters but has no headless CLI. The runner relies on this
        // returning `None` to short-circuit with a meaningful error
        // before touching the filesystem.
        assert_eq!(command_name(AgentKind::Windsurf), None);
    }

    #[test]
    fn supported_agents_have_unique_command_names() {
        // Sanity: two agents mapping to the same binary name would be
        // a copy-paste bug — every candidate path for the wrong agent
        // would still resolve, and dispatch_gate would call the wrong
        // CLI.
        let mut seen = std::collections::HashSet::new();
        for agent in [
            AgentKind::ClaudeCode,
            AgentKind::Codex,
            AgentKind::Cursor,
            AgentKind::GeminiCli,
        ] {
            let name = command_name(agent).expect("supported agent has command");
            assert!(
                seen.insert(name),
                "duplicate command name {name} across agents",
            );
        }
    }

    #[test]
    fn candidate_paths_nonempty_for_supported_agents() {
        for agent in [
            AgentKind::ClaudeCode,
            AgentKind::Codex,
            AgentKind::Cursor,
            AgentKind::GeminiCli,
        ] {
            assert!(
                !candidate_paths(agent).is_empty(),
                "expected at least one candidate path for {}",
                agent.label(),
            );
        }
    }

    #[test]
    fn candidate_paths_for_windsurf_is_empty() {
        assert!(candidate_paths(AgentKind::Windsurf).is_empty());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_candidate_paths_use_exe_suffix() {
        // Spot-check: every Windows candidate path must end in `.exe`,
        // otherwise the official-installer path won't match.
        let paths = candidate_paths(AgentKind::ClaudeCode);
        assert!(!paths.is_empty());
        for path in &paths {
            assert!(
                path.extension().is_some_and(|e| e == "exe"),
                "expected .exe suffix: {}",
                path.display(),
            );
        }
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn unix_candidate_paths_have_no_extension() {
        // Spot-check the reverse: Unix paths should not carry `.exe`.
        let paths = candidate_paths(AgentKind::ClaudeCode);
        assert!(!paths.is_empty());
        for path in &paths {
            assert!(
                path.extension().is_none(),
                "unexpected extension on unix candidate: {}",
                path.display(),
            );
        }
    }
}
