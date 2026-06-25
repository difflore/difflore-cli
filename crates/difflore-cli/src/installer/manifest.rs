//! Install manifest for the hash-tracked canonical record at
//! `~/.difflore/mcp.json`.
//!
//! Per target, the manifest stores the config path, `block_kind`, the rendered
//! block's SHA-256, and the `block_version`. This lets `update` tell an
//! unchanged DiffLore block (safe to replace) from a hand-edited one (must not
//! clobber), and run a targeted migration when the block shape changes.
//!
//! We hash the rendered difflore block in isolation, not the whole file (the
//! file is co-owned). The install-time hash uses the same `Value`/string the
//! installer writes; the update-time check re-extracts our block from disk and
//! re-hashes it with identical canonicalisation (see the `render_*` /
//! `extract_*` pairs in `json_config.rs`, `hooks_install.rs`, `goose_yaml.rs`).
//!
//! Stores hashes + paths only, never config contents. Externally-CLI-managed
//! targets (Claude MCP via `claude mcp add`, Codex via `codex mcp add`) are
//! marked `managed_by: "external-cli"` with no hash and no path, since we never
//! author those bytes.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    common::{MCP_SERVER_ARG, difflore_mcp_record_path, resolve_difflore_binary},
    goose_yaml::{extract_goose_block, render_goose_block},
    hooks_install::{
        extract_hook_groups_on_disk, legacy_claude_code_hook_blocks, render_claude_code_hook_block,
        render_codex_hook_block, render_cursor_hook_block, render_gemini_cli_hook_block,
        render_windsurf_hook_block,
    },
    json_config::{extract_mcp_json_block, render_mcp_json_block},
    registry::{self, AgentSpec, BlockKind, HookSurface},
};

/// Current manifest schema version. v1 = the legacy `command`/`args`/
/// `installed_targets`-only record (no `manifest_version`, no `targets`).
pub(super) const MANIFEST_VERSION: u32 = 2;

/// How DiffLore manages a target's bytes. `Difflore` = we wrote the block
/// (hashable); `ExternalCli` = the agent's own CLI owns the file (not hashable
/// by us).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum ManagedBy {
    Difflore,
    ExternalCli,
}

/// One per-target row in the v2 manifest. `surface_key` is the stable
/// [`registry::canonical_target_key`] join key; `block_hash` / `config_path`
/// are `None` for externally CLI-managed targets.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct ManifestTarget {
    /// Canonical display name (== `TargetOutcome.name` == `AgentSpec.name`).
    pub name: String,
    /// Stable lower-cased join key.
    pub surface_key: String,
    pub managed_by: ManagedBy,
    /// Absolute config path we wrote; `null` for external-cli targets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_path: Option<String>,
    /// MCP server map key (`mcpServers` / `servers`); `null` for hook / goose /
    /// external-cli surfaces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub servers_key: Option<String>,
    /// `mcp_json | hooks_json | goose_yaml | external_cli`.
    pub block_kind: String,
    /// Monotonic per `block_kind`; the version actually written for this target.
    pub block_version: u32,
    /// `"sha256:<hex>"` of the exact difflore block we rendered; `null` for
    /// external-cli targets (we never authored bytes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_hash: Option<String>,
    pub installed_at: String,
    pub updated_at: String,
}

/// The v2 install manifest. Keeps the v1 top-level fields so v1 readers keep
/// working, and adds `manifest_version` + the per-target `targets` array.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct InstallManifest {
    pub manifest_version: u32,
    pub command: String,
    pub args: Vec<String>,
    /// v1-compat: display names of every managed target.
    pub installed_targets: Vec<String>,
    #[serde(default)]
    pub targets: Vec<ManifestTarget>,
}

// Hashing

/// Hash a rendered block's bytes into `"sha256:<hex>"`.
pub(super) fn hash_block(bytes: &[u8]) -> String {
    difflore_core::infra::crypto::sha256_block_hex(bytes)
}

/// Canonical bytes for a single `Value` block (mcp_json), built with the same
/// `serde_json::to_string` the installers write through so the hash matches our
/// render. A serialization failure degrades to an empty hash input rather than
/// panicking.
fn value_bytes(value: &Value) -> Vec<u8> {
    serde_json::to_string(value)
        .unwrap_or_default()
        .into_bytes()
}

/// Canonical bytes for a set of hook group `Value`s. Render and extract walk
/// events in different orders, so we sort the per-group serializations before
/// joining; the hash is over the content of our contributed groups,
/// independent of the file's event-iteration order.
fn hook_groups_bytes(groups: &[Value]) -> Vec<u8> {
    let mut serialized: Vec<String> = groups
        .iter()
        .map(|g| serde_json::to_string(g).unwrap_or_default())
        .collect();
    serialized.sort();
    serialized.join("\n").into_bytes()
}

/// Install-time hash: the bytes DiffLore would render for `spec` with binaries
/// `mcp_bin` (JSON/YAML config) / `cli_bin` (hook shim path). `None` for
/// external-cli surfaces.
pub(super) fn render_block_hash(spec: &AgentSpec, mcp_bin: &str, cli_bin: &str) -> Option<String> {
    match registry::block_kind_of(spec) {
        BlockKind::McpJson => Some(hash_block(&value_bytes(&render_mcp_json_block(
            mcp_bin,
            registry::mcp_entry_shape_of(spec),
        )))),
        BlockKind::GooseYaml => Some(hash_block(render_goose_block(mcp_bin).as_bytes())),
        BlockKind::HooksJson => {
            let surface = registry::hook_surface_of(spec)?;
            let groups = render_hook_groups(surface, cli_bin);
            Some(hash_block(&hook_groups_bytes(&groups)))
        }
        BlockKind::ExternalCli => None,
    }
}

/// Update-time check: the bytes of the difflore block currently on disk for
/// `spec`, canonicalised identically to [`render_block_hash`]. `None` when the
/// file is missing or has no difflore block, or for external-cli surfaces.
pub(super) fn on_disk_block_hash(spec: &AgentSpec, _cli_bin: &str) -> Option<String> {
    let path = registry::resolve_path(spec).ok()?;
    match registry::block_kind_of(spec) {
        BlockKind::McpJson => {
            let servers_key = registry::servers_key_of(spec)?;
            let value = extract_mcp_json_block(&path, servers_key)?;
            Some(hash_block(&value_bytes(&value)))
        }
        BlockKind::GooseYaml => {
            let block = extract_goose_block(&path)?;
            Some(hash_block(block.as_bytes()))
        }
        BlockKind::HooksJson => {
            let surface = registry::hook_surface_of(spec)?;
            let groups = extract_hook_groups_on_disk(&path, hook_client(surface));
            if groups.is_empty() {
                return None;
            }
            Some(hash_block(&hook_groups_bytes(&groups)))
        }
        BlockKind::ExternalCli => None,
    }
}

/// Update-time recognition aid: the hashes of every *historical* render of
/// `spec`'s block (excluding the current one), canonicalised identically to
/// [`render_block_hash`]. `agents update` treats an on-disk block matching one
/// of these as DiffLore-authored — pristine, just old — and upgrades it instead
/// of skipping it as locally edited. Empty for surfaces whose rendered shape
/// never changed.
pub(super) fn legacy_render_hashes(spec: &AgentSpec, cli_bin: &str) -> Vec<String> {
    if registry::block_kind_of(spec) == BlockKind::HooksJson
        && matches!(registry::hook_surface_of(spec), Some(HookSurface::Claude))
    {
        return legacy_claude_code_hook_blocks(cli_bin)
            .iter()
            .map(|groups| hash_block(&hook_groups_bytes(groups)))
            .collect();
    }
    Vec::new()
}

/// The render fn for each hook surface. Kept here so the manifest, not the
/// registry driver, owns the render↔extract pairing.
fn render_hook_groups(surface: HookSurface, cli_bin: &str) -> Vec<Value> {
    match surface {
        HookSurface::Claude => render_claude_code_hook_block(cli_bin),
        HookSurface::Codex => render_codex_hook_block(cli_bin),
        HookSurface::Cursor => render_cursor_hook_block(cli_bin),
        HookSurface::Gemini => render_gemini_cli_hook_block(cli_bin),
        HookSurface::Windsurf => render_windsurf_hook_block(cli_bin),
    }
}

/// The `--client` marker string each hook surface uses (matches the extract
/// predicates in `hooks_install.rs`).
const fn hook_client(surface: HookSurface) -> &'static str {
    match surface {
        HookSurface::Claude => "claude-code",
        HookSurface::Codex => "codex",
        HookSurface::Cursor => "cursor",
        HookSurface::Gemini => "gemini-cli",
        HookSurface::Windsurf => "windsurf",
    }
}

// Building manifest targets after a successful install

/// Build the v2 `targets` array from the installed surface names plus the
/// current binaries. `installed_names` is the canonical-display-name list of
/// every surface we just wired.
///
/// `prior` is the previously-loaded manifest, used to preserve each target's
/// original `installed_at` so a re-install only bumps `updated_at`. A "Claude
/// Code" install also emits a "Claude Code hooks" target since the MCP row
/// carries its lifecycle hooks.
pub(super) fn build_targets(
    installed_names: &[&str],
    mcp_bin: &str,
    cli_bin: &str,
    prior: Option<&InstallManifest>,
) -> Vec<ManifestTarget> {
    let now = now_rfc3339();
    let mut targets: Vec<ManifestTarget> = Vec::new();
    // Dedup by surface_key: `installed_names` can list both "Claude Code hooks"
    // and "Claude Code", and the latter rides along a hooks target — without
    // this guard we'd emit the hooks row twice.
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut push_unique = |targets: &mut Vec<ManifestTarget>, spec: &'static AgentSpec| {
        let target = target_for_spec(spec, mcp_bin, cli_bin, &now, prior);
        if seen.insert(target.surface_key.clone()) {
            targets.push(target);
        }
    };
    for name in installed_names {
        let Some(spec) = registry::find_spec(name) else {
            continue;
        };
        push_unique(&mut targets, spec);
        // Record the hooks surface too so `update` tracks the hook block version.
        if spec.name == "Claude Code"
            && let Some(hook_spec) = registry::find_spec("Claude Code hooks")
        {
            push_unique(&mut targets, hook_spec);
        }
    }
    targets
}

/// Build provisional manifest targets for a v1 record (no `targets` array):
/// one row per `installed_targets` display name, with `block_hash: None` and
/// `block_version: 0` so `update` re-renders the current block and adopts it
/// only when the on-disk block still matches. External-CLI targets get version
/// 0 too so a `ReissueCli` re-stamps them.
pub(super) fn v1_provisional_targets(installed_targets: &[String]) -> Vec<ManifestTarget> {
    let now = now_rfc3339();
    let mut targets: Vec<ManifestTarget> = Vec::new();
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for name in installed_targets {
        let Some(spec) = registry::find_spec(name) else {
            continue;
        };
        let surface_key = registry::canonical_target_key(spec.name);
        if !seen.insert(surface_key.clone()) {
            continue;
        }
        let block_kind = registry::block_kind_of(spec);
        let (managed_by, config_path, servers_key) = match block_kind {
            BlockKind::ExternalCli => (ManagedBy::ExternalCli, None, None),
            _ => (
                ManagedBy::Difflore,
                registry::resolve_path(spec)
                    .ok()
                    .map(|p| p.display().to_string()),
                registry::servers_key_of(spec).map(ToOwned::to_owned),
            ),
        };
        targets.push(ManifestTarget {
            name: spec.name.to_owned(),
            surface_key,
            managed_by,
            config_path,
            servers_key,
            block_kind: block_kind.as_str().to_owned(),
            // version 0 + hash None → "behind, hash unknown" → adoption path.
            block_version: 0,
            block_hash: None,
            installed_at: now.clone(),
            updated_at: now.clone(),
        });
    }
    targets
}

fn target_for_spec(
    spec: &AgentSpec,
    mcp_bin: &str,
    cli_bin: &str,
    now: &str,
    prior: Option<&InstallManifest>,
) -> ManifestTarget {
    let block_kind = registry::block_kind_of(spec);
    let surface_key = registry::canonical_target_key(spec.name);
    let installed_at = prior
        .and_then(|m| m.targets.iter().find(|t| t.surface_key == surface_key))
        .map_or_else(|| now.to_owned(), |t| t.installed_at.clone());

    let (managed_by, config_path, servers_key, block_hash) = if block_kind == BlockKind::ExternalCli
    {
        (ManagedBy::ExternalCli, None, None, None)
    } else {
        let path = registry::resolve_path(spec)
            .ok()
            .map(|p| p.display().to_string());
        (
            ManagedBy::Difflore,
            path,
            registry::servers_key_of(spec).map(ToOwned::to_owned),
            render_block_hash(spec, mcp_bin, cli_bin),
        )
    };

    ManifestTarget {
        name: spec.name.to_owned(),
        surface_key,
        managed_by,
        config_path,
        servers_key,
        block_kind: block_kind.as_str().to_owned(),
        block_version: block_kind.current_version(),
        block_hash,
        installed_at,
        updated_at: now.to_owned(),
    }
}

// Load / save (with v1→v2 read shim)

/// Load the manifest at `~/.difflore/mcp.json`, upgrading a v1 record in memory
/// to a v2 shape with an empty `targets` array. Returns `None` when the record
/// is missing or unparseable (callers treat that as "nothing installed").
pub(super) fn load() -> Option<InstallManifest> {
    let path = difflore_mcp_record_path().ok()?;
    if !path.exists() {
        return None;
    }
    let raw = std::fs::read_to_string(&path).ok()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    manifest_from_value(&value)
}

fn manifest_from_value(value: &Value) -> Option<InstallManifest> {
    let obj = value.as_object()?;

    let targets: Vec<ManifestTarget> = obj
        .get("targets")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|t| serde_json::from_value::<ManifestTarget>(t.clone()).ok())
                .collect()
        })
        .unwrap_or_default();
    let command = obj
        .get("command")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| {
            (!targets.is_empty())
                .then(|| resolve_difflore_binary().ok())
                .flatten()
        })?;
    let args = obj.get("args").and_then(Value::as_array).map_or_else(
        || vec![MCP_SERVER_ARG.to_owned()],
        |arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(ToOwned::to_owned))
                .collect()
        },
    );
    let installed_targets = obj
        .get("installed_targets")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(ToOwned::to_owned))
                .collect()
        })
        .unwrap_or_default();

    // v1 records lack `manifest_version` / `targets`, so we get an empty vec —
    // every target's hash is then "unknown".
    let manifest_version = obj
        .get("manifest_version")
        .and_then(Value::as_u64)
        .map_or(1, |v| v as u32);
    Some(InstallManifest {
        manifest_version,
        command,
        args,
        installed_targets,
        targets,
    })
}

/// Serialize + write the v2 manifest to `~/.difflore/mcp.json` (pretty, the
/// same shape `write_install_manifest` emits).
pub(super) fn save(manifest: &InstallManifest) -> Result<PathBuf, String> {
    let path = difflore_mcp_record_path()?;
    let pretty = serde_json::to_string_pretty(manifest)
        .map_err(|e| format!("failed to serialize mcp.json: {e}"))?;
    super::common::write_atomic(&path, pretty.as_bytes())
        .map_err(|e| format!("failed to write {}: {e}", path.display()))?;
    Ok(path)
}

/// RFC-3339 UTC timestamp (`2026-06-02T12:00:00Z`) for `installed_at` /
/// `updated_at`.
pub(super) fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::super::goose_yaml::merge_goose_yaml_config;
    use super::super::hooks_install::{
        hook_command_string, merge_claude_code_hooks, merge_cursor_hooks,
    };
    use super::super::json_config::install_json_config_at;
    use super::super::registry::find_spec;
    use super::*;

    fn spec(name: &str) -> &'static AgentSpec {
        find_spec(name).expect("known surface")
    }

    const MCP_BIN: &str = "/tmp/fake/difflore";

    // Render ↔ extract round-trips: install a block to a temp file, then
    // re-extract + re-hash from disk and assert it matches the install-time
    // render hash. Then mutate the file and assert the hash changes, so an edit
    // is never silently clobbered.

    #[test]
    fn mcp_json_block_round_trips_then_diverges_on_edit() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let path = tmp.path().join("mcp.json");
        install_json_config_at(
            &path,
            MCP_BIN,
            "mcpServers",
            super::super::json_config::McpEntryShape::Standard,
            false,
        )
        .expect("install");

        let render_hash = hash_block(&value_bytes(&render_mcp_json_block(
            MCP_BIN,
            super::super::json_config::McpEntryShape::Standard,
        )));
        let extracted = extract_mcp_json_block(&path, "mcpServers").expect("difflore entry");
        assert_eq!(
            hash_block(&value_bytes(&extracted)),
            render_hash,
            "freshly-installed mcp_json block must re-hash to the render hash"
        );

        // A user edit (extra env) must change the extracted hash → not clobbered.
        let mut obj = super::super::json_config::load_json_object(&path).expect("load");
        if let Some(entry) = obj
            .get_mut("mcpServers")
            .and_then(|s| s.as_object_mut())
            .and_then(|s| s.get_mut("difflore"))
            .and_then(|d| d.as_object_mut())
        {
            entry.insert("env".to_owned(), serde_json::json!({"FOO": "bar"}));
        }
        super::super::json_config::write_json_object(&path, &obj).expect("write edited");
        let edited = extract_mcp_json_block(&path, "mcpServers").expect("still present");
        assert_ne!(
            hash_block(&value_bytes(&edited)),
            render_hash,
            "an edited block must hash differently so update skips it"
        );
    }

    #[test]
    fn goose_block_round_trips_then_diverges_on_edit() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let path = tmp.path().join("config.yaml");
        merge_goose_yaml_config(&path, MCP_BIN, false).expect("install");

        let render_hash = hash_block(render_goose_block(MCP_BIN).as_bytes());
        let extracted = extract_goose_block(&path).expect("difflore block");
        assert_eq!(
            hash_block(extracted.as_bytes()),
            render_hash,
            "freshly-installed goose block must re-hash to the render hash"
        );

        // Append a child line under the difflore block → edited → different hash.
        let edited_yaml = fs::read_to_string(&path)
            .expect("read")
            .replace("      - mcp-server\n", "      - mcp-server\n    env: FOO\n");
        fs::write(&path, edited_yaml).expect("write edited");
        let edited = extract_goose_block(&path).expect("still present");
        assert_ne!(
            hash_block(edited.as_bytes()),
            render_hash,
            "an edited goose block must hash differently"
        );
    }

    #[test]
    fn windows_quoted_goose_path_round_trips() {
        // Windows drive-colon paths are single-quoted by yaml_escape_scalar; the
        // render and extract must agree on the quoted form.
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let path = tmp.path().join("config.yaml");
        let win_bin = r"C:\Users\me\difflore.exe";
        merge_goose_yaml_config(&path, win_bin, false).expect("install");
        assert_eq!(
            hash_block(extract_goose_block(&path).expect("block").as_bytes()),
            hash_block(render_goose_block(win_bin).as_bytes()),
            "Windows-quoted goose path must round-trip"
        );
    }

    #[test]
    fn hooks_block_round_trips_regardless_of_event_order() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let path = tmp.path().join("hooks.json");
        merge_cursor_hooks(&path, MCP_BIN, false).expect("install");

        let render_hash = hash_block(&hook_groups_bytes(&render_cursor_hook_block(MCP_BIN)));
        let groups = extract_hook_groups_on_disk(&path, "cursor");
        assert!(!groups.is_empty(), "extracted difflore hook entries");
        assert_eq!(
            hash_block(&hook_groups_bytes(&groups)),
            render_hash,
            "freshly-installed cursor hooks must re-hash to the render hash, \
             independent of the file's event-iteration order"
        );
    }

    #[test]
    fn claude_hooks_block_round_trips_render_and_merge() {
        // Guard-rail for the render↔merge lockstep: when the two matcher
        // tables drift (as they once did over `|Bash` on PostToolUse), a
        // freshly-merged block no longer re-hashes to the render hash and
        // `agents update` wrongly classifies every pristine install as
        // locally edited.
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let path = tmp.path().join("settings.json");
        merge_claude_code_hooks(&path, MCP_BIN).expect("install");

        let render_hash = hash_block(&hook_groups_bytes(&render_claude_code_hook_block(MCP_BIN)));
        let groups = extract_hook_groups_on_disk(&path, "claude-code");
        assert!(!groups.is_empty(), "extracted difflore hook groups");
        assert_eq!(
            hash_block(&hook_groups_bytes(&groups)),
            render_hash,
            "merge_claude_code_hooks and render_claude_code_hook_block drifted \
             — they must share one event/matcher table"
        );
    }

    /// Seed `path` with the exact hook groups an old-era merge wrote:
    /// PreToolUse(Read) still registered and `post_matcher` on PostToolUse.
    fn seed_legacy_claude_settings(path: &std::path::Path, post_matcher: &str) {
        let command = hook_command_string(MCP_BIN, "claude-code");
        let group = |matcher: Option<&str>, timeout: u32| {
            let mut g = serde_json::Map::new();
            if let Some(m) = matcher {
                g.insert("matcher".to_owned(), Value::from(m));
            }
            g.insert(
                "hooks".to_owned(),
                serde_json::json!([{"type": "command", "command": command, "timeout": timeout}]),
            );
            Value::Array(vec![Value::Object(g)])
        };
        let seed = serde_json::json!({
            "hooks": {
                "PreToolUse": group(Some("Read"), 2000),
                "PostToolUse": group(Some(post_matcher), 5000),
                "SessionStart": group(Some("startup|clear|compact"), 10000),
                "UserPromptSubmit": group(None, 5000),
                "Stop": group(None, 10000),
                "SessionEnd": group(None, 10000),
            }
        });
        fs::write(
            path,
            serde_json::to_string_pretty(&seed).expect("serialise seed"),
        )
        .expect("seed");
    }

    #[test]
    fn legacy_claude_hook_installs_hash_to_known_legacy_renders() {
        // Both historical shapes — the initial release (no `Bash`) and the
        // interim one (with `Bash`, PreToolUse still registered) — must be
        // recognised via `legacy_render_hashes` so `agents update` upgrades
        // them instead of skipping them as locally edited. Neither may be
        // mistaken for the current standard render.
        let hook_spec = spec("Claude Code hooks");
        let legacy = legacy_render_hashes(hook_spec, MCP_BIN);
        let standard = render_block_hash(hook_spec, MCP_BIN, MCP_BIN).expect("standard hash");

        let mut seen = Vec::new();
        for post_matcher in ["Edit|MultiEdit|Write", "Edit|MultiEdit|Write|Bash"] {
            let tmp = tempfile::TempDir::new().expect("tempdir");
            let path = tmp.path().join("settings.json");
            seed_legacy_claude_settings(&path, post_matcher);

            let on_disk = extract_hook_groups_on_disk(&path, "claude-code");
            assert_eq!(on_disk.len(), 6, "all six old-era groups extracted");
            let on_disk_hash = hash_block(&hook_groups_bytes(&on_disk));
            assert!(
                legacy.contains(&on_disk_hash),
                "old install ({post_matcher}) must hash to a known legacy render: \
                 {on_disk_hash} not in {legacy:?}"
            );
            assert_ne!(
                on_disk_hash, standard,
                "legacy shape must not collide with the current standard render"
            );
            seen.push(on_disk_hash);
        }
        assert_ne!(seen[0], seen[1], "the two legacy eras hash differently");
    }

    #[test]
    fn legacy_render_hashes_only_cover_the_claude_hook_surface() {
        // Other surfaces never changed shape; their legacy list must be empty
        // so the recognition path cannot mis-adopt anything there.
        for name in ["Codex hooks", "Cursor hooks", "Cursor", "Goose"] {
            assert!(
                legacy_render_hashes(spec(name), MCP_BIN).is_empty(),
                "{name} should have no legacy renders"
            );
        }
    }

    #[test]
    fn render_block_hash_is_stable_and_prefixed_for_each_kind() {
        // mcp_json (Cursor), goose_yaml (Goose), hooks_json (Codex/Cursor hooks).
        for name in ["Cursor", "Goose", "Codex hooks", "Cursor hooks"] {
            let h = render_block_hash(spec(name), MCP_BIN, MCP_BIN)
                .unwrap_or_else(|| panic!("{name} should render a hash"));
            assert!(h.starts_with("sha256:"), "{name}: {h}");
            // Deterministic across calls.
            assert_eq!(h, render_block_hash(spec(name), MCP_BIN, MCP_BIN).unwrap());
        }
    }

    #[test]
    fn external_cli_surfaces_have_no_block_hash() {
        for name in ["Claude Code", "Codex"] {
            assert!(
                render_block_hash(spec(name), MCP_BIN, MCP_BIN).is_none(),
                "{name} is CLI-managed and must not be hashed"
            );
        }
    }

    #[test]
    fn build_targets_emits_claude_hooks_alongside_claude_mcp() {
        let targets = build_targets(&["Claude Code"], MCP_BIN, MCP_BIN, None);
        let keys: Vec<&str> = targets.iter().map(|t| t.surface_key.as_str()).collect();
        assert!(keys.contains(&"claude"), "Claude MCP target: {keys:?}");
        assert!(
            keys.contains(&"claude hooks"),
            "Claude hooks target must ride along: {keys:?}"
        );
        let claude = targets.iter().find(|t| t.surface_key == "claude").unwrap();
        assert_eq!(claude.managed_by, ManagedBy::ExternalCli);
        assert!(claude.block_hash.is_none());
        assert!(claude.config_path.is_none());
        let hooks = targets
            .iter()
            .find(|t| t.surface_key == "claude hooks")
            .unwrap();
        assert_eq!(hooks.managed_by, ManagedBy::Difflore);
        assert_eq!(hooks.block_kind, "hooks_json");
        assert!(hooks.block_hash.is_some());
    }

    #[test]
    fn build_targets_records_difflore_managed_json_surface() {
        let targets = build_targets(&["Cursor"], MCP_BIN, MCP_BIN, None);
        let cursor = targets.iter().find(|t| t.surface_key == "cursor").unwrap();
        assert_eq!(cursor.managed_by, ManagedBy::Difflore);
        assert_eq!(cursor.block_kind, "mcp_json");
        assert_eq!(cursor.servers_key.as_deref(), Some("mcpServers"));
        assert_eq!(cursor.block_version, registry::MCP_JSON_BLOCK_VERSION);
        assert!(cursor.block_hash.as_deref().unwrap().starts_with("sha256:"));
        assert!(cursor.config_path.is_some());
    }

    #[test]
    fn build_targets_preserves_installed_at_from_prior_manifest() {
        let first = InstallManifest {
            manifest_version: MANIFEST_VERSION,
            command: MCP_BIN.to_owned(),
            args: vec![MCP_SERVER_ARG.to_owned()],
            installed_targets: vec!["Cursor".to_owned()],
            targets: build_targets(&["Cursor"], MCP_BIN, MCP_BIN, None),
        };
        let original_installed_at = first.targets[0].installed_at.clone();
        let again = build_targets(&["Cursor"], MCP_BIN, MCP_BIN, Some(&first));
        assert_eq!(
            again[0].installed_at, original_installed_at,
            "installed_at must be preserved across re-install"
        );
    }

    #[test]
    fn v1_provisional_targets_seed_unknown_hashes_for_adoption() {
        // A v1 record lists display names only; the seed gives each a hash-None,
        // version-0 row so update's adoption path can recognise/claim it.
        let targets = v1_provisional_targets(&[
            "Cursor".to_owned(),
            "Claude Code".to_owned(),
            // Duplicate display name must be deduped by surface_key.
            "Cursor".to_owned(),
        ]);
        let cursor: Vec<_> = targets
            .iter()
            .filter(|t| t.surface_key == "cursor")
            .collect();
        assert_eq!(cursor.len(), 1, "duplicate display names must dedup");
        assert!(cursor[0].block_hash.is_none(), "hash unknown for v1 seed");
        assert_eq!(cursor[0].block_version, 0, "version 0 → treated as behind");
        assert_eq!(cursor[0].managed_by, ManagedBy::Difflore);
        let claude = targets.iter().find(|t| t.surface_key == "claude").unwrap();
        assert_eq!(claude.managed_by, ManagedBy::ExternalCli);
        assert_eq!(claude.block_version, 0);
    }

    #[test]
    fn manifest_load_preserves_targets_when_top_level_command_is_missing() {
        let value = serde_json::json!({
            "manifest_version": 2,
            "args": ["mcp-server"],
            "installed_targets": ["Cursor"],
            "targets": [{
                "name": "Cursor",
                "surface_key": "cursor",
                "managed_by": "difflore",
                "config_path": "/tmp/cursor/mcp.json",
                "servers_key": "mcpServers",
                "block_kind": "mcp_json",
                "block_version": 1,
                "block_hash": "abc123",
                "installed_at": "2026-06-01T00:00:00Z",
                "updated_at": "2026-06-01T00:00:00Z"
            }]
        });

        let manifest = manifest_from_value(&value).expect("manifest should load");
        assert!(!manifest.command.is_empty());
        assert_eq!(manifest.args, vec![MCP_SERVER_ARG.to_owned()]);
        assert_eq!(manifest.targets.len(), 1);
        assert_eq!(manifest.targets[0].surface_key, "cursor");
    }

    #[test]
    fn manifest_load_still_rejects_record_without_command_or_targets() {
        assert!(manifest_from_value(&serde_json::json!({ "args": ["mcp-server"] })).is_none());
    }

    #[test]
    fn block_kind_round_trips_through_manifest_string() {
        for k in [
            BlockKind::McpJson,
            BlockKind::HooksJson,
            BlockKind::GooseYaml,
            BlockKind::ExternalCli,
        ] {
            assert_eq!(BlockKind::from_str(k.as_str()), Some(k));
        }
        assert_eq!(BlockKind::from_str("nope"), None);
    }
}
