//! JSON-file-based config helpers and the generic `install_json_config_at`
//! installer used by every Tier-3 (and several Tier-1) clients.

use std::{
    fs,
    path::{Path, PathBuf},
};

use serde_json::{Value, json};

use super::{Status, TargetOutcome, common::MCP_SERVER_ARG};

pub(super) fn load_json_object(path: &PathBuf) -> Result<serde_json::Map<String, Value>, String> {
    if !path.exists() {
        return Ok(serde_json::Map::new());
    }
    let raw =
        fs::read_to_string(path).map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(serde_json::Map::new());
    }
    let v: Value = serde_json::from_str(trimmed)
        .map_err(|e| format!("invalid JSON in {}: {e}", path.display()))?;
    v.as_object()
        .cloned()
        .ok_or_else(|| format!("{} is not a JSON object at the top level", path.display()))
}

pub(super) fn write_json_object(
    path: &PathBuf,
    obj: &serde_json::Map<String, Value>,
) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
    }
    let pretty =
        serde_json::to_string_pretty(obj).map_err(|e| format!("failed to serialize JSON: {e}"))?;
    super::common::write_atomic(path, pretty.as_bytes())
        .map_err(|e| format!("failed to write {}: {e}", path.display()))?;
    Ok(())
}

/// Merge `{ <servers_key>: { difflore: { command, args: ["mcp-server"] } } }`
/// into a JSON object, preserving every other entry. `servers_key` is
/// "mcpServers" for most tools, "servers" for Copilot CLI. Returns true if a
/// prior `difflore` entry existed (an update rather than a first install).
fn merge_difflore_entry_with_key(
    config: &mut serde_json::Map<String, Value>,
    bin: &str,
    servers_key: &str,
) -> bool {
    let servers = config
        .entry(servers_key.to_owned())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    let Some(obj) = servers.as_object_mut() else {
        return false;
    };
    let new_entry = json!({
        "command": bin,
        "args": [MCP_SERVER_ARG],
    });
    let existed = obj.contains_key("difflore");
    obj.insert("difflore".to_owned(), new_entry);
    existed
}

/// Inverse of [`merge_difflore_entry_with_key`]: remove the `difflore` entry
/// from the `servers_key` block, preserving every other server. Drops the
/// `servers_key` block entirely if it becomes empty so we don't leave an
/// orphaned `{}`. Returns true if a `difflore` entry was actually present.
fn remove_difflore_entry_with_key(
    config: &mut serde_json::Map<String, Value>,
    servers_key: &str,
) -> bool {
    let Some(servers) = config.get_mut(servers_key) else {
        return false;
    };
    let Some(obj) = servers.as_object_mut() else {
        return false;
    };
    let removed = obj.remove("difflore").is_some();
    if obj.is_empty() {
        config.remove(servers_key);
    }
    removed
}

/// Core of every JSON-file-based installer. Reads the file (empty map if
/// missing), merges in our entry under `servers_key`, writes it back.
/// Returns true if an existing `difflore` entry was overwritten.
pub(super) fn install_json_config_at(
    path: &PathBuf,
    bin: &str,
    servers_key: &str,
    dry_run: bool,
) -> Result<bool, String> {
    let mut cfg = load_json_object(path)?;
    let existed = merge_difflore_entry_with_key(&mut cfg, bin, servers_key);
    if !dry_run {
        write_json_object(path, &cfg)?;
    }
    Ok(existed)
}

/// Inverse of [`install_json_config_at`]. Reads the file, removes the
/// `difflore` entry under `servers_key`, writes it back (unless `dry_run`).
/// Returns true if a `difflore` entry was present (i.e. something was
/// removed). A missing file or missing entry is a no-op returning false.
pub(super) fn uninstall_json_config_at(
    path: &PathBuf,
    servers_key: &str,
    dry_run: bool,
) -> Result<bool, String> {
    if !path.exists() {
        return Ok(false);
    }
    let mut cfg = load_json_object(path)?;
    let removed = remove_difflore_entry_with_key(&mut cfg, servers_key);
    if removed && !dry_run {
        write_json_object(path, &cfg)?;
    }
    Ok(removed)
}

pub(super) fn finish_json_uninstall(
    name: &'static str,
    path: &PathBuf,
    servers_key: &str,
    dry_run: bool,
) -> TargetOutcome {
    match uninstall_json_config_at(path, servers_key, dry_run) {
        Ok(true) => TargetOutcome {
            name,
            status: Status::Removed,
            detail: if dry_run {
                format!(
                    "would remove difflore from: {}",
                    public_config_path(name, path)
                )
            } else {
                public_config_path(name, path)
            },
        },
        Ok(false) => TargetOutcome {
            name,
            status: Status::Skipped("no difflore entry to remove".into()),
            detail: String::new(),
        },
        Err(e) => TargetOutcome {
            name,
            status: Status::Error(e),
            detail: String::new(),
        },
    }
}

pub(super) fn finish_json_install(
    name: &'static str,
    path: &PathBuf,
    bin: &str,
    servers_key: &str,
    dry_run: bool,
) -> TargetOutcome {
    match install_json_config_at(path, bin, servers_key, dry_run) {
        Ok(existed) => TargetOutcome {
            name,
            status: if existed {
                Status::Updated
            } else {
                Status::Installed
            },
            detail: if dry_run {
                format!("would write: {}", public_config_path(name, path))
            } else {
                public_config_path(name, path)
            },
        },
        Err(e) => TargetOutcome {
            name,
            status: Status::Error(e),
            detail: String::new(),
        },
    }
}

// Rendered-block helpers

/// The exact `difflore` MCP-server value object written under `servers_key`:
/// `{ "command": bin, "args": ["mcp-server"] }`. This is the subtree the install
/// manifest hashes (not the whole co-owned file), built from the same `json!`
/// [`merge_difflore_entry_with_key`] inserts so the hash matches what we wrote.
pub(super) fn render_mcp_json_block(bin: &str) -> Value {
    json!({
        "command": bin,
        "args": [MCP_SERVER_ARG],
    })
}

/// Read the on-disk `difflore` entry under `servers_key`, if present, so
/// `agents update` can re-hash it and compare against the manifest. Returns
/// `None` when the file is missing/unreadable or has no difflore entry.
pub(super) fn extract_mcp_json_block(path: &PathBuf, servers_key: &str) -> Option<Value> {
    if !path.exists() {
        return None;
    }
    let obj = load_json_object(path).ok()?;
    obj.get(servers_key)?.as_object()?.get("difflore").cloned()
}

fn public_config_path(name: &str, path: &Path) -> String {
    match name {
        "Copilot CLI" => "~/.github/copilot/mcp.json".to_owned(),
        "Antigravity" => "~/.gemini/antigravity/mcp_config.json".to_owned(),
        "Crush" => "~/.config/crush/mcp.json".to_owned(),
        "Roo Code" => "./.roo/mcp.json".to_owned(),
        "Warp" => "~/.warp/mcp.json".to_owned(),
        _ => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_util::{tmp_named_path, tmp_settings_path};
    use super::*;

    const BIN: &str = "/tmp/fake/difflore";

    fn read_json(path: &PathBuf) -> Value {
        let s = fs::read_to_string(path).expect("read config");
        serde_json::from_str(&s).expect("parse config")
    }

    // Tier-3 JSON installers — table-driven across all clients

    #[test]
    fn json_installers_write_difflore_under_servers_key() {
        // (relative path, servers_key) — one row per client surface.
        let cases: &[(&str, &str)] = &[
            (".github/copilot/mcp.json", "servers"),
            (".gemini/antigravity/mcp_config.json", "mcpServers"),
            (".config/crush/mcp.json", "mcpServers"),
            (".roo/mcp.json", "mcpServers"),
            (".warp/mcp.json", "mcpServers"),
        ];
        for (rel, key) in cases {
            let (tmp, _) = tmp_settings_path();
            let path = tmp.path().join(rel);
            let existed = install_json_config_at(&path, BIN, key, false).unwrap();
            assert!(!existed, "first install for {rel} must report new entry");
            let v = read_json(&path);
            let entry = v
                .get(*key)
                .and_then(|s| s.get("difflore"))
                .unwrap_or_else(|| panic!("{key}.difflore missing for {rel}"));
            assert_eq!(entry["command"], BIN, "wrong command for {rel}");
            assert_eq!(entry["args"], json!(["mcp-server"]), "wrong args for {rel}");
        }
    }

    // Merge-preservation: reinstall, other entries left alone

    #[test]
    fn reinstall_reports_updated_and_preserves_other_entries() {
        let (_tmp, path) = tmp_named_path("mcp.json");
        // Seed with an unrelated entry.
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(
            &path,
            r#"{ "mcpServers": { "other": { "command": "x", "args": [] } } }"#,
        )
        .unwrap();

        let existed = install_json_config_at(&path, BIN, "mcpServers", false).unwrap();
        assert!(!existed, "difflore wasn't there yet");
        let existed2 = install_json_config_at(&path, BIN, "mcpServers", false).unwrap();
        assert!(existed2, "second install must report update");

        let v = read_json(&path);
        assert_eq!(v["mcpServers"]["other"]["command"], "x");
        assert_eq!(v["mcpServers"]["difflore"]["command"], BIN);
    }

    // Uninstall round-trips (inverse of the merge)

    #[test]
    fn uninstall_removes_difflore_and_preserves_other_entries() {
        let (_tmp, path) = tmp_named_path("mcp.json");
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(
            &path,
            r#"{ "mcpServers": { "other": { "command": "x", "args": [] } } }"#,
        )
        .unwrap();

        // Install, then uninstall: the difflore entry should be gone but the
        // unrelated server must survive untouched.
        install_json_config_at(&path, BIN, "mcpServers", false).unwrap();
        let removed = uninstall_json_config_at(&path, "mcpServers", false).unwrap();
        assert!(removed, "uninstall must report it removed a difflore entry");

        let v = read_json(&path);
        assert!(
            v["mcpServers"].get("difflore").is_none(),
            "difflore entry must be gone: {v}"
        );
        assert_eq!(
            v["mcpServers"]["other"]["command"], "x",
            "unrelated server clobbered: {v}"
        );
    }

    #[test]
    fn uninstall_round_trip_on_fresh_file_leaves_empty_object() {
        // A file that only ever held difflore should end up `{}` (the
        // mcpServers block is dropped once empty, not left as `{}`).
        for (rel, key) in &[
            (".github/copilot/mcp.json", "servers"),
            (".roo/mcp.json", "mcpServers"),
        ] {
            let (tmp, _) = tmp_settings_path();
            let path = tmp.path().join(rel);
            install_json_config_at(&path, BIN, key, false).unwrap();
            let removed = uninstall_json_config_at(&path, key, false).unwrap();
            assert!(
                removed,
                "fresh install then uninstall must remove for {rel}"
            );
            let v = read_json(&path);
            assert_eq!(v, json!({}), "{rel}: leftover keys after uninstall: {v}");
        }
    }

    #[test]
    fn uninstall_is_noop_when_no_difflore_entry() {
        let (_tmp, path) = tmp_named_path("mcp.json");
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(
            &path,
            r#"{ "mcpServers": { "other": { "command": "x" } } }"#,
        )
        .unwrap();

        let removed = uninstall_json_config_at(&path, "mcpServers", false).unwrap();
        assert!(!removed, "no difflore entry → nothing removed");
        let v = read_json(&path);
        assert_eq!(v["mcpServers"]["other"]["command"], "x");
    }

    #[test]
    fn uninstall_missing_file_is_noop() {
        let (_tmp, path) = tmp_named_path("absent.json");
        let removed = uninstall_json_config_at(&path, "mcpServers", false).unwrap();
        assert!(!removed);
        assert!(!path.exists(), "uninstall must not create the file");
    }

    #[test]
    fn uninstall_dry_run_does_not_write() {
        let (_tmp, path) = tmp_named_path("mcp.json");
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        install_json_config_at(&path, BIN, "mcpServers", false).unwrap();
        let before = fs::read_to_string(&path).unwrap();

        let removed = uninstall_json_config_at(&path, "mcpServers", true).unwrap();
        assert!(removed, "dry-run still reports what it would remove");
        let after = fs::read_to_string(&path).unwrap();
        assert_eq!(before, after, "dry-run must not touch the file");
    }
}
