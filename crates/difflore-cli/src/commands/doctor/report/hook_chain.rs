use std::path::{Path, PathBuf};

use serde_json::Value;

pub(super) fn hook_chain_section(s: &mut String) {
    sw!(s, "\n## · Hook chain\n");
    sw!(s, "- platform: `{}`", std::env::consts::OS);
    sw!(
        s,
        "- forward mode: `{}`",
        crate::hook::forward::Mode::from_env()
    );

    let project_hash = crate::hook::forward::protocol::current_project_hash();
    sw!(s, "- current project hash: `{project_hash}`");
    match crate::hook::forward::protocol::endpoint_for_hash(&project_hash) {
        Ok(endpoint) => {
            sw!(s, "- endpoint: `{}`", endpoint.display());
            if cfg!(windows) {
                let pipe = endpoint
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("(unknown)");
                sw!(s, "- windows named pipe basename: `{pipe}`");
            }
        }
        Err(e) => sw!(s, "- endpoint: unavailable ({e})"),
    }

    if cfg!(windows) {
        let self_warm = match std::env::var("DIFFLORE_WINDOWS_HOOK_SELF_WARM") {
            Ok(value)
                if matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "0" | "false" | "off" | "never" | "no"
                ) =>
            {
                "disabled by DIFFLORE_WINDOWS_HOOK_SELF_WARM"
            }
            _ => "enabled; breakaway spawn is best-effort and falls back cold on failure",
        };
        sw!(
            s,
            "- windows strategy: direct GUI-subsystem `difflore-hook.exe`; MCP-hosted forwarders warm the current project plus a small set of known projects; cold miss falls back in-process"
        );
        sw!(s, "- windows self-warm: {self_warm}");
    } else {
        sw!(
            s,
            "- warm strategy: hook shim self-spawns a detached per-project forwarder on cold miss"
        );
    }
    sw!(
        s,
        "- forwarder scope: each endpoint serves one project hash; MCP may host multiple endpoints in one process"
    );

    let configs = hook_config_paths();
    let mut total = 0usize;
    for config in configs {
        total += append_hook_config_summary(s, &config);
    }
    if total == 0 {
        sw!(
            s,
            "- configured DiffLore hook commands: none found in known hook config files"
        );
    }
}

struct HookConfig {
    client: &'static str,
    path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HookCommand {
    event: String,
    command: String,
    timeout: Option<i64>,
}

fn hook_config_paths() -> Vec<HookConfig> {
    let mut configs = Vec::new();
    if let Some(home) = home_dir() {
        configs.extend([
            HookConfig {
                client: "claude-code",
                path: home.join(".claude").join("settings.json"),
            },
            HookConfig {
                client: "codex",
                path: home.join(".codex").join("hooks.json"),
            },
            HookConfig {
                client: "gemini-cli",
                path: home.join(".gemini").join("settings.json"),
            },
            HookConfig {
                client: "windsurf",
                path: home.join(".codeium").join("windsurf").join("hooks.json"),
            },
        ]);
    }
    if let Ok(cwd) = std::env::current_dir() {
        configs.push(HookConfig {
            client: "cursor",
            path: cwd.join(".cursor").join("hooks.json"),
        });
    }
    configs
}

fn home_dir() -> Option<PathBuf> {
    difflore_core::infra::env::var_os(difflore_core::infra::env::DIFFLORE_MCP_HOME)
        .map(PathBuf::from)
        .or_else(dirs::home_dir)
}

fn append_hook_config_summary(s: &mut String, config: &HookConfig) -> usize {
    if !config.path.exists() {
        sw!(
            s,
            "- {} hooks: missing `{}`",
            config.client,
            config.path.display()
        );
        return 0;
    }

    let raw = match std::fs::read_to_string(&config.path) {
        Ok(raw) => raw,
        Err(e) => {
            sw!(
                s,
                "- {} hooks: unreadable `{}` ({e})",
                config.client,
                config.path.display()
            );
            return 0;
        }
    };
    let value = match serde_json::from_str::<Value>(&raw) {
        Ok(value) => value,
        Err(e) => {
            sw!(
                s,
                "- {} hooks: invalid JSON `{}` ({e})",
                config.client,
                config.path.display()
            );
            return 0;
        }
    };

    let commands = collect_difflore_hook_commands(&value);
    if commands.is_empty() {
        sw!(
            s,
            "- {} hooks: no DiffLore command in `{}`",
            config.client,
            config.path.display()
        );
        return 0;
    }

    sw!(
        s,
        "- {} hooks: {} DiffLore command(s) in `{}`",
        config.client,
        commands.len(),
        config.path.display()
    );
    for command in &commands {
        let exe = command_executable(&command.command);
        let exists = exe
            .as_deref()
            .filter(|path| path.is_absolute())
            .map(Path::exists);
        let exists_label = match exists {
            Some(true) => "exists",
            Some(false) => "missing",
            None => "not checked",
        };
        let timeout = command
            .timeout
            .map_or_else(|| "unset".to_owned(), |value| format!("{value}ms"));
        sw!(
            s,
            "  - event `{}`: kind=`{}`, binary=`{}`, timeout={}, file={}",
            command.event,
            classify_hook_command(&command.command),
            classify_hook_binary(exe.as_deref()),
            timeout,
            exists_label
        );
        sw!(s, "    command: `{}`", truncate_command(&command.command));
    }
    commands.len()
}

fn collect_difflore_hook_commands(value: &Value) -> Vec<HookCommand> {
    let mut out = Vec::new();
    if let Some(hooks) = value.get("hooks").and_then(Value::as_object) {
        for (event, node) in hooks {
            collect_commands_from_node(event, node, &mut out);
        }
    }
    out
}

fn collect_commands_from_node(event: &str, node: &Value, out: &mut Vec<HookCommand>) {
    match node {
        Value::Object(obj) => {
            if let Some(command) = obj.get("command").and_then(Value::as_str)
                && command.contains("difflore")
            {
                out.push(HookCommand {
                    event: event.to_owned(),
                    command: command.to_owned(),
                    timeout: obj.get("timeout").and_then(Value::as_i64),
                });
            }
            for value in obj.values() {
                collect_commands_from_node(event, value, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_commands_from_node(event, item, out);
            }
        }
        _ => {}
    }
}

fn classify_hook_command(command: &str) -> &'static str {
    if command.contains("difflore-launcher") && command.contains("--difflore-hook") {
        "legacy-launcher-hook"
    } else if command.contains("difflore-hook") {
        "direct-hook"
    } else if command.contains("difflore") {
        "difflore-unknown"
    } else {
        "other"
    }
}

fn classify_hook_binary(path: Option<&Path>) -> &'static str {
    let Some(path) = path else {
        return "unknown";
    };
    let normalized = path.to_string_lossy().replace('\\', "/");
    if normalized.contains("/target/debug/") {
        "debug"
    } else if normalized.contains("/target/release/") {
        "local-release"
    } else if normalized.contains("/.difflore/bin/") {
        "managed-stable"
    } else if normalized.contains("/.difflore/versions/") {
        "managed-version"
    } else {
        "unknown"
    }
}

fn command_executable(command: &str) -> Option<PathBuf> {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(rest) = trimmed.strip_prefix('"') {
        let end = rest.find('"')?;
        return Some(PathBuf::from(&rest[..end]));
    }
    let first = trimmed.split_whitespace().next()?;
    Some(PathBuf::from(first))
}

fn truncate_command(command: &str) -> String {
    const MAX: usize = 260;
    if command.chars().count() <= MAX {
        return command.to_owned();
    }
    let mut truncated = command.chars().take(MAX - 1).collect::<String>();
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collects_nested_and_flat_difflore_hook_commands() {
        let value = serde_json::json!({
            "hooks": {
                "PostToolUse": [{
                    "matcher": "Edit",
                    "hooks": [{
                        "command": "\"/tmp/difflore-hook\" --client claude-code",
                        "timeout": 5000
                    }]
                }],
                "post_run_command": [{
                    "command": "/tmp/difflore-hook --client windsurf",
                    "show_output": false
                }],
                "Other": [{"command": "/tmp/other"}]
            }
        });

        let commands = collect_difflore_hook_commands(&value);

        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0].event, "PostToolUse");
        assert_eq!(commands[0].timeout, Some(5000));
        assert_eq!(commands[1].event, "post_run_command");
        assert_eq!(commands[1].timeout, None);
    }

    #[test]
    fn classifies_direct_legacy_and_binary_channels() {
        assert_eq!(
            classify_hook_command(
                r#""C:/Users/me/.difflore/bin/difflore-launcher.exe" --difflore-hook --client claude-code"#
            ),
            "legacy-launcher-hook"
        );
        assert_eq!(
            classify_hook_command(r#""/repo/target/debug/difflore-hook" --client codex"#),
            "direct-hook"
        );
        assert_eq!(
            classify_hook_binary(Some(Path::new("/repo/target/debug/difflore-hook"))),
            "debug"
        );
        assert_eq!(
            classify_hook_binary(Some(Path::new("/Users/me/.difflore/bin/difflore-hook"))),
            "managed-stable"
        );
    }
}
