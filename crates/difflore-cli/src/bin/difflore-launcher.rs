#![cfg_attr(windows, windows_subsystem = "windows")]

use std::{
    env,
    ffi::OsString,
    path::{Path, PathBuf},
    process::{Command, ExitCode, ExitStatus},
};

use serde_json::Value;

const HOOK_SENTINEL: &str = "--difflore-hook";

#[derive(Debug, PartialEq, Eq)]
enum LauncherTarget {
    Managed {
        key: &'static str,
        args: Vec<OsString>,
    },
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(127)
        }
    }
}

fn run() -> Result<ExitCode, String> {
    let exe = env::current_exe().map_err(|e| format!("could not resolve launcher path: {e}"))?;
    let LauncherTarget::Managed { key, args } = launcher_target_and_args(&exe);
    let target = {
        let home = difflore_home()?;
        target_binary_for_launcher(&home, key, &exe)?.into_os_string()
    };
    let mut cmd = Command::new(&target);
    cmd.args(args);
    configure_child(&mut cmd);
    let status = cmd
        .status()
        .map_err(|e| format!("failed to launch {}: {e}", PathBuf::from(&target).display()))?;
    Ok(ExitCode::from(child_exit_code_value(status)))
}

fn launcher_target_and_args(exe: &Path) -> LauncherTarget {
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    launcher_target_from_path_and_args(exe, args)
}

fn launcher_target_from_path_and_args(exe: &Path, mut args: Vec<OsString>) -> LauncherTarget {
    if path_invokes_hook(exe) {
        return LauncherTarget::Managed { key: "hook", args };
    }
    if args.first().and_then(|a| a.to_str()) == Some(HOOK_SENTINEL) {
        args.remove(0);
        return LauncherTarget::Managed { key: "hook", args };
    }
    LauncherTarget::Managed { key: "bin", args }
}

fn normalize_exit_code(code: i32) -> u8 {
    u8::try_from(code).unwrap_or(255)
}

#[cfg(unix)]
fn signal_exit_code(signal: i32) -> u8 {
    normalize_exit_code(128 + signal)
}

fn child_exit_code_value(status: ExitStatus) -> u8 {
    if let Some(code) = status.code() {
        return normalize_exit_code(code);
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return signal_exit_code(signal);
        }
    }

    1
}

fn difflore_home() -> Result<PathBuf, String> {
    if let Some(home) = env::var_os("DIFFLORE_HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(home));
    }
    env::var_os("USERPROFILE")
        .or_else(|| env::var_os("HOME"))
        .map(PathBuf::from)
        .map(|home| home.join(".difflore"))
        .ok_or_else(|| "could not resolve home directory for DiffLore".to_owned())
}

fn read_current_json(home: &Path) -> Result<Value, String> {
    let path = home.join("current.json");
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    serde_json::from_str(&raw).map_err(|e| format!("invalid {}: {e}", path.display()))
}

fn target_binary_for_launcher(
    home: &Path,
    key: &str,
    launcher_exe: &Path,
) -> Result<PathBuf, String> {
    if is_managed_launcher(home, launcher_exe)
        && let Ok(target) = target_binary_from_current(home, key)
    {
        return Ok(target);
    }

    sibling_binary_path(key, launcher_exe)
}

fn target_binary_from_current(home: &Path, key: &str) -> Result<PathBuf, String> {
    let current_path = home.join("current.json");
    if !current_path.exists() {
        return Err(format!("{} not found", current_path.display()));
    }

    let current = read_current_json(home)?;
    current
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| format!("DiffLore current.json is missing `{key}`"))
}

fn is_managed_launcher(home: &Path, launcher_exe: &Path) -> bool {
    let home = canonical_path_lossy(home);
    let exe = canonical_path_lossy(launcher_exe);
    exe.starts_with(home.join("bin")) || exe.starts_with(home.join("versions"))
}

fn canonical_path_lossy(path: &Path) -> PathBuf {
    let mut missing = Vec::new();
    let mut cursor = path;
    loop {
        if let Ok(base) = cursor.canonicalize() {
            let mut out = base;
            for component in missing.iter().rev() {
                out.push(component);
            }
            return out;
        }
        if let Some(name) = cursor.file_name() {
            missing.push(name.to_owned());
        }
        let Some(parent) = cursor.parent() else {
            break;
        };
        cursor = parent;
    }

    path.to_path_buf()
}

fn sibling_binary_path(key: &str, exe: &Path) -> Result<PathBuf, String> {
    let Some(parent) = exe.parent() else {
        return Err("could not resolve launcher directory".to_owned());
    };
    let stem = match key {
        "hook" => "difflore-hook",
        "bin" => "difflore",
        _ => return Err(format!("unknown DiffLore launcher target `{key}`")),
    };
    Ok(parent.join(format!("{stem}{}", env::consts::EXE_SUFFIX)))
}

#[cfg(windows)]
fn configure_child(cmd: &mut Command) {
    use std::os::windows::process::CommandExt as _;

    // Keep stdio pipes inherited for MCP/hook protocols, but do not let
    // Windows allocate a transient console when a GUI app launches us.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
const fn configure_child(_cmd: &mut Command) {}

fn path_invokes_hook(path: &Path) -> bool {
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    name == "difflore-hook"
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_current_json(home: &Path, key: &str, value: &Path) -> Result<(), String> {
        let raw = serde_json::json!({ key: value.to_string_lossy() }).to_string();
        fs::write(home.join("current.json"), raw).map_err(|e| e.to_string())
    }

    #[test]
    fn launcher_name_selects_main_binary() {
        assert!(!path_invokes_hook(Path::new("difflore")));
        assert!(!path_invokes_hook(Path::new("difflore.exe")));
        assert!(!path_invokes_hook(Path::new("difflore-launcher.exe")));
    }

    #[test]
    fn hook_launcher_name_selects_hook_binary() {
        assert!(path_invokes_hook(Path::new("difflore-hook")));
        assert!(path_invokes_hook(Path::new("difflore-hook.exe")));
    }

    #[test]
    fn launcher_without_sentinel_selects_main_binary() {
        let target = launcher_target_from_path_and_args(
            Path::new("difflore-launcher.exe"),
            vec![OsString::from("mcp-server")],
        );
        assert_eq!(
            target,
            LauncherTarget::Managed {
                key: "bin",
                args: vec![OsString::from("mcp-server")]
            }
        );
    }

    #[test]
    fn hook_sentinel_selects_hook_binary_and_is_not_forwarded() {
        let target = launcher_target_from_path_and_args(
            Path::new("difflore-launcher.exe"),
            vec![
                OsString::from(HOOK_SENTINEL),
                OsString::from("--client"),
                OsString::from("claude-code"),
            ],
        );
        assert_eq!(
            target,
            LauncherTarget::Managed {
                key: "hook",
                args: vec![OsString::from("--client"), OsString::from("claude-code")]
            }
        );
    }

    #[test]
    fn hook_named_launcher_does_not_require_sentinel() {
        let target = launcher_target_from_path_and_args(
            Path::new("difflore-hook.exe"),
            vec![OsString::from("--client"), OsString::from("codex")],
        );
        assert_eq!(
            target,
            LauncherTarget::Managed {
                key: "hook",
                args: vec![OsString::from("--client"), OsString::from("codex")]
            }
        );
    }

    #[test]
    fn large_child_exit_codes_saturate_instead_of_collapsing_to_one() {
        assert_eq!(normalize_exit_code(300), 255);
    }

    #[test]
    fn sibling_fallback_resolves_main_binary_name() -> Result<(), String> {
        let path = sibling_binary_path("bin", Path::new("/tmp/difflore-launcher"))?;
        let expected = format!("difflore{}", env::consts::EXE_SUFFIX);
        assert_eq!(
            path.file_name().and_then(|n| n.to_str()),
            Some(expected.as_str())
        );
        Ok(())
    }

    #[test]
    fn sibling_fallback_resolves_hook_binary_name() -> Result<(), String> {
        let path = sibling_binary_path("hook", Path::new("/tmp/difflore-hook"))?;
        let expected = format!("difflore-hook{}", env::consts::EXE_SUFFIX);
        assert_eq!(
            path.file_name().and_then(|n| n.to_str()),
            Some(expected.as_str())
        );
        Ok(())
    }

    #[test]
    fn unmanaged_launcher_ignores_current_json_and_uses_sibling_hook() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let home = dir.path().join(".difflore");
        fs::create_dir_all(&home).map_err(|e| e.to_string())?;
        let managed_hook = dir.path().join("managed/difflore-hook.exe");
        write_current_json(&home, "hook", &managed_hook)?;

        let launcher = dir
            .path()
            .join("checkout/target/debug")
            .join(format!("difflore-launcher{}", env::consts::EXE_SUFFIX));
        let target = target_binary_for_launcher(&home, "hook", &launcher)?;

        assert_eq!(
            target,
            launcher.with_file_name(format!("difflore-hook{}", env::consts::EXE_SUFFIX))
        );
        Ok(())
    }

    #[test]
    fn managed_launcher_prefers_current_json() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let home = dir.path().join(".difflore");
        let managed_bin = dir
            .path()
            .join("versions/0.2.0")
            .join(format!("difflore-hook{}", env::consts::EXE_SUFFIX));
        fs::create_dir_all(&home).map_err(|e| e.to_string())?;
        write_current_json(&home, "hook", &managed_bin)?;

        let launcher = home
            .join("bin")
            .join(format!("difflore-launcher{}", env::consts::EXE_SUFFIX));
        let target = target_binary_for_launcher(&home, "hook", &launcher)?;

        assert_eq!(target, managed_bin);
        Ok(())
    }

    #[test]
    fn managed_launcher_falls_back_to_sibling_when_current_json_is_unusable() -> Result<(), String>
    {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let home = dir.path().join(".difflore");
        fs::create_dir_all(&home).map_err(|e| e.to_string())?;
        fs::write(home.join("current.json"), r#"{"bin":""}"#).map_err(|e| e.to_string())?;

        let launcher = home
            .join("bin")
            .join(format!("difflore-launcher{}", env::consts::EXE_SUFFIX));
        let target = target_binary_for_launcher(&home, "hook", &launcher)?;

        assert_eq!(
            target,
            launcher.with_file_name(format!("difflore-hook{}", env::consts::EXE_SUFFIX))
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn signal_exit_codes_use_shell_convention() {
        assert_eq!(signal_exit_code(9), 137);
    }
}
