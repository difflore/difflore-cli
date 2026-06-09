//! Install manifest for the hash-tracked canonical record at
//! `~/.difflore/mcp.json`.
//!
//! ## Why
//!
//! The v1 record stored *what* we installed (binary + arg + a list of
//! display-name strings) but **not how**: no per-target config path, no record
//! of the exact bytes we wrote, no block-shape version. That made two things
//! impossible: (1) telling "this difflore block is unchanged since DiffLore
//! wrote it" (safe to replace) from "the human hand-edited it" (must not
//! clobber); and (2) running a *targeted* migration when DiffLore ships a new
//! block shape. The v2 manifest stores, per target, the config path, the
//! `block_kind`, the rendered block's SHA-256, and the `block_version` we
//! stamped — every field a fact we directly observe, never a fabricated metric
//! and never config *contents*.
//!
//! ## What we hash
//!
//! The *rendered difflore block in isolation*, not the whole file (the file is
//! co-owned — the user legitimately edits other entries). The install-time hash
//! is computed from the same `Value`/string the installer hands to `fs::write`;
//! the update-time check re-extracts our block from disk and re-hashes it with
//! the identical canonicalisation (see the `render_*` / `extract_*` pairs in
//! `json_config.rs`, `hooks_install.rs`, `goose_yaml.rs`).
//!
//! ## Honesty / isolation guardrails
//!
//! The manifest lives under [`difflore_core::paths::data_home`] like every
//! other local artifact (honours `$DIFFLORE_HOME` / the test home) and stores
//! hashes + paths only — never config contents, never anything repo-scoped.
//! Externally-CLI-managed targets (Claude MCP via `claude mcp add`, Codex via
//! `codex mcp add`) are explicitly marked `managed_by: "external-cli"` with no
//! hash and no path, since we never author those bytes.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    common::{MCP_SERVER_ARG, difflore_mcp_record_path},
    goose_yaml::{extract_goose_block, render_goose_block},
    hooks_install::{
        extract_hook_groups_on_disk, render_claude_code_hook_block, render_cursor_hook_block,
        render_gemini_cli_hook_block, render_windsurf_hook_block,
    },
    json_config::{extract_mcp_json_block, render_mcp_json_block},
    registry::{self, AgentSpec, BlockKind, HookSurface},
};

/// Current manifest schema version. v1 = the legacy `command`/`args`/
/// `installed_targets`-only record (no `manifest_version`, no `targets`).
pub(super) const MANIFEST_VERSION: u32 = 2;

/// How DiffLore manages a target's bytes. Mirrors the manifest `managed_by`
/// field. `Difflore` = we wrote the block (hashable); `ExternalCli` = the
/// agent's own CLI owns the file (not hashable by us).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum ManagedBy {
    Difflore,
    ExternalCli,
}

/// One per-target row in the v2 manifest. `surface_key` is the stable
/// [`registry::canonical_target_key`] join key (matches probe/status output and
/// uninstall planning); `block_hash` / `config_path` are `None` for externally
/// CLI-managed targets.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct ManifestTarget {
    /// Canonical display name (== `TargetOutcome.name` == `AgentSpec.name`).
    pub name: String,
    /// `canonical_target_key(name)` — stable lower-cased join key.
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

/// The v2 install manifest. Keeps the v1 top-level fields (`command`, `args`,
/// `installed_targets`) so v1 readers keep working, and adds
/// `manifest_version` + the per-target `targets` array.
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

// ── Hashing ────────────────────────────────────────────────────────────────

/// Hash a rendered block's bytes into `"sha256:<hex>"`. Delegates to
/// [`difflore_core::crypto::sha256_block_hex`] so the algorithm choice + the
/// `sha2` dep live in one crate.
pub(super) fn hash_block(bytes: &[u8]) -> String {
    difflore_core::crypto::sha256_block_hex(bytes)
}

/// Canonical bytes for a single `Value` block (mcp_json). Built with the same
/// `serde_json::to_string` the installers write through, so the hash matches our
/// render. A serialization failure (never expected for the simple objects we
/// build) degrades to an empty hash input rather than panicking.
fn value_bytes(value: &Value) -> Vec<u8> {
    serde_json::to_string(value)
        .unwrap_or_default()
        .into_bytes()
}

/// Canonical bytes for a *set* of hook group `Value`s. The render walks events
/// in event-list order while the extract walks the on-disk hooks object in
/// (BTreeMap-sorted) key order, so to make the install-time and update-time
/// hashes agree we sort the per-group serializations before joining. The hash
/// is therefore over the *content* of our contributed groups, independent of
/// the file's event-iteration order.
fn hook_groups_bytes(groups: &[Value]) -> Vec<u8> {
    let mut serialized: Vec<String> = groups
        .iter()
        .map(|g| serde_json::to_string(g).unwrap_or_default())
        .collect();
    serialized.sort();
    serialized.join("\n").into_bytes()
}

/// The hashable bytes DiffLore *would render* for `spec` with binaries
/// `mcp_bin` (JSON/YAML config) / `cli_bin` (hook shim path). `None` for
/// external-cli surfaces (no authored bytes). This is the install-time hash
/// input.
pub(super) fn render_block_hash(spec: &AgentSpec, mcp_bin: &str, cli_bin: &str) -> Option<String> {
    match registry::block_kind_of(spec) {
        BlockKind::McpJson => Some(hash_block(&value_bytes(&render_mcp_json_block(mcp_bin)))),
        BlockKind::GooseYaml => Some(hash_block(render_goose_block(mcp_bin).as_bytes())),
        BlockKind::HooksJson => {
            let surface = registry::hook_surface_of(spec)?;
            let groups = render_hook_groups(surface, cli_bin);
            Some(hash_block(&hook_groups_bytes(&groups)))
        }
        BlockKind::ExternalCli => None,
    }
}

/// The hashable bytes of the difflore block *currently on disk* for `spec`,
/// canonicalised identically to [`render_block_hash`]. `None` when the file is
/// missing or has no difflore block (the "gone" case in `update`), or for
/// external-cli surfaces. This is the update-time check input.
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

/// The render fn for each hook surface (kept here so the manifest, not the
/// registry driver, owns the render↔extract pairing).
fn render_hook_groups(surface: HookSurface, cli_bin: &str) -> Vec<Value> {
    match surface {
        HookSurface::Claude => render_claude_code_hook_block(cli_bin),
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
        HookSurface::Cursor => "cursor",
        HookSurface::Gemini => "gemini-cli",
        HookSurface::Windsurf => "windsurf",
    }
}

// ── Building manifest targets after a successful install ────────────────────

/// Build the v2 `targets` array from the set of installed surface names plus
/// the current binaries. `installed_names` is the canonical-display-name list
/// of every `difflore`/external-cli surface we just wired (derived from the
/// install outcomes + current probe snapshot, exactly like the v1 record).
///
/// `prior` is the previously-loaded manifest (if any), used to preserve each
/// target's original `installed_at` so a re-install/upgrade only bumps
/// `updated_at`. The Claude Code MCP row carries its lifecycle hooks, so a
/// "Claude Code" install also emits a "Claude Code hooks" manifest target.
pub(super) fn build_targets(
    installed_names: &[&str],
    mcp_bin: &str,
    cli_bin: &str,
    prior: Option<&InstallManifest>,
) -> Vec<ManifestTarget> {
    let now = now_rfc3339();
    let mut targets: Vec<ManifestTarget> = Vec::new();
    // Dedup by surface_key: `installed_names` (probe-derived) can already list
    // "Claude Code hooks" *and* "Claude Code", and the latter also rides-along a
    // hooks target — without this guard we'd emit the hooks row twice.
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
        // The Claude Code MCP install also merges the lifecycle hooks; record
        // that hooks surface too so `update` tracks the hook block version.
        if spec.name == "Claude Code"
            && let Some(hook_spec) = registry::find_spec("Claude Code hooks")
        {
            push_unique(&mut targets, hook_spec);
        }
    }
    targets
}

/// Build *provisional* manifest targets for a v1 record (no `targets` array):
/// one row per `installed_targets` display name, with `block_hash: None` and
/// `block_version: 0` so `update` re-renders the current block and adopts it
/// only when the on-disk block still matches. Path / `block_kind` /
/// `servers_key` are derived from the registry.
/// External-CLI targets get version 0 too so a `ReissueCli` re-stamps them.
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

// ── Load / save (with v1→v2 read shim) ──────────────────────────────────────

/// Load the manifest at `~/.difflore/mcp.json`, upgrading a v1 record in memory
/// to a v2 shape with an empty `targets` array (every target then has an
/// unknown hash that the update path may adopt). Returns `None` when the record
/// is missing or unparseable (callers treat that as "nothing installed").
pub(super) fn load() -> Option<InstallManifest> {
    let path = difflore_mcp_record_path().ok()?;
    if !path.exists() {
        return None;
    }
    let raw = std::fs::read_to_string(&path).ok()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    let obj = value.as_object()?;

    let command = obj.get("command").and_then(Value::as_str)?.to_owned();
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

    // v2 manifest: parse the `targets` array. v1 record: no `manifest_version`
    // / `targets`, so we get an empty vec — every target's hash is "unknown".
    let manifest_version = obj
        .get("manifest_version")
        .and_then(Value::as_u64)
        .map_or(1, |v| v as u32);
    let targets = obj
        .get("targets")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|t| serde_json::from_value::<ManifestTarget>(t.clone()).ok())
                .collect()
        })
        .unwrap_or_default();

    Some(InstallManifest {
        manifest_version,
        command,
        args,
        installed_targets,
        targets,
    })
}

/// Serialize + write the v2 manifest to `~/.difflore/mcp.json` (pretty, the
/// same shape `write_install_manifest` emits). Used by `update` after an
/// in-place upgrade re-stamps a target.
pub(super) fn save(manifest: &InstallManifest) -> Result<PathBuf, String> {
    let path = difflore_mcp_record_path()?;
    let pretty = serde_json::to_string_pretty(manifest)
        .map_err(|e| format!("failed to serialize mcp.json: {e}"))?;
    super::common::write_atomic(&path, pretty.as_bytes())
        .map_err(|e| format!("failed to write {}: {e}", path.display()))?;
    Ok(path)
}

/// RFC-3339 UTC timestamp (`2026-06-02T12:00:00Z`) for `installed_at` /
/// `updated_at`. Uses the workspace `chrono` dep already pulled by difflore-cli.
pub(super) fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::super::goose_yaml::merge_goose_yaml_config;
    use super::super::hooks_install::merge_cursor_hooks;
    use super::super::json_config::install_json_config_at;
    use super::super::registry::find_spec;
    use super::*;

    fn spec(name: &str) -> &'static AgentSpec {
        find_spec(name).expect("known surface")
    }

    const MCP_BIN: &str = "/tmp/fake/difflore";

    // ── Render ↔ extract round-trips ───────────────────────────────────────
    //
    // Install a block to a temp file, then re-extract + re-hash from disk and
    // assert it matches the install-time render hash (so `update` sees
    // "unchanged"). Then mutate the file and assert the hash changes (so an edit
    // is *never* silently clobbered). These exercise the explicit-path
    // merge/extract fns directly, independent of home-path resolution.

    #[test]
    fn mcp_json_block_round_trips_then_diverges_on_edit() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let path = tmp.path().join("mcp.json");
        install_json_config_at(&path, MCP_BIN, "mcpServers", false).expect("install");

        let render_hash = hash_block(&value_bytes(&render_mcp_json_block(MCP_BIN)));
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
    fn render_block_hash_is_stable_and_prefixed_for_each_kind() {
        // mcp_json (Cursor), goose_yaml (Goose), hooks_json (Cursor hooks).
        for name in ["Cursor", "Goose", "Cursor hooks"] {
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
