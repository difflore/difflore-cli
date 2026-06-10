//! Single-table agent registry driving detect + install + uninstall for every
//! supported AI coding tool.
//!
//! `AGENTS` is the one source of truth: adding an agent means adding one
//! [`AgentSpec`] row. Each row is one surface (an agent can contribute
//! several, e.g. its MCP entry plus a hooks surface).
//!
//! The three drivers ([`detect`], [`install`], [`uninstall`]) each `match` over
//! [`ConfigFormat`] and delegate to the leaf format engines (`json_config.rs`,
//! `goose_yaml.rs`, `hooks_install.rs`).

use std::path::PathBuf;

use super::{
    Status, TargetOutcome, TargetStatus,
    common::{
        claude_plugin_installed, cwd_path, error_outcome, home_path, probe_cli_mcp,
        probe_json_install,
    },
    goose_yaml::{merge_goose_yaml_config, probe_goose_install, remove_goose_yaml_config},
    hooks_install::{
        install_claude_code_hooks, install_cursor_hooks, install_gemini_cli_hooks,
        install_windsurf_hooks, probe_json_hooks_by_command, probe_json_hooks_by_group,
        probe_json_hooks_by_name, probe_json_hooks_by_nested_command, uninstall_claude_code_hooks,
        uninstall_cursor_hooks, uninstall_gemini_cli_hooks, uninstall_windsurf_hooks,
    },
    json_config::{finish_json_install, finish_json_uninstall},
};

// Block versioning: the install manifest stamps the version it wrote per
// target, so `agents update` can tell which targets are behind purely from the
// local manifest. Bump the relevant constant when a block's rendered shape
// changes; [`BlockKind::current_version`] reads them.

/// Version of the JSON MCP-server block (`{command, args:["mcp-server"]}`).
pub(super) const MCP_JSON_BLOCK_VERSION: u32 = 1;
/// Version of the JSON lifecycle-hooks block (the per-client event matchers).
pub(super) const HOOKS_JSON_BLOCK_VERSION: u32 = 1;
/// Version of the Goose YAML `difflore:` block.
pub(super) const GOOSE_YAML_BLOCK_VERSION: u32 = 1;
/// Version of the externally-CLI-managed shape (the `mcp add … mcp-server`
/// arg/shim shape). Bumping it re-issues the idempotent CLI add on the next
/// `agents update`.
pub(super) const CLI_DELEGATE_BLOCK_VERSION: u32 = 1;

/// Which writer/reader pair a manifest target uses. Selecting on this (plus
/// `servers_key`) lets `agents update` dispatch without special-casing every
/// installer. Mirrors the `block_kind` string persisted in the manifest.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum BlockKind {
    /// JSON MCP-server map entry under `servers_key`.
    McpJson,
    /// JSON lifecycle-hooks groups (Claude / Cursor / Gemini / Windsurf).
    HooksJson,
    /// Goose-style YAML `difflore:` block.
    GooseYaml,
    /// Owned by the agent's own CLI (`claude`/`codex`); we never author bytes.
    ExternalCli,
}

impl BlockKind {
    /// Stable string persisted in the manifest (`block_kind` field).
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::McpJson => "mcp_json",
            Self::HooksJson => "hooks_json",
            Self::GooseYaml => "goose_yaml",
            Self::ExternalCli => "external_cli",
        }
    }

    /// Parse a persisted manifest `block_kind` string back into the enum.
    /// Only exercised by the round-trip unit test; the live update path
    /// re-derives the kind from [`AgentSpec`] via [`block_kind_of`].
    #[allow(dead_code)]
    pub(super) fn from_str(s: &str) -> Option<Self> {
        match s {
            "mcp_json" => Some(Self::McpJson),
            "hooks_json" => Some(Self::HooksJson),
            "goose_yaml" => Some(Self::GooseYaml),
            "external_cli" => Some(Self::ExternalCli),
            _ => None,
        }
    }

    /// Current in-binary version for this block kind. The manifest compares the
    /// per-target stamped version against this to decide upgrade vs. leave-alone.
    pub(super) const fn current_version(self) -> u32 {
        match self {
            Self::McpJson => MCP_JSON_BLOCK_VERSION,
            Self::HooksJson => HOOKS_JSON_BLOCK_VERSION,
            Self::GooseYaml => GOOSE_YAML_BLOCK_VERSION,
            Self::ExternalCli => CLI_DELEGATE_BLOCK_VERSION,
        }
    }
}

/// The [`BlockKind`] a surface row writes, derived from its [`ConfigFormat`].
/// Used by the install manifest + `agents update` so the per-surface writer
/// metadata lives on the one `AGENTS` table.
pub(super) const fn block_kind_of(spec: &AgentSpec) -> BlockKind {
    match &spec.format {
        ConfigFormat::Json { .. } => BlockKind::McpJson,
        ConfigFormat::Yaml => BlockKind::GooseYaml,
        ConfigFormat::Hooks { .. } => BlockKind::HooksJson,
        ConfigFormat::CliDelegate { .. } => BlockKind::ExternalCli,
    }
}

/// The MCP-server map key for a JSON surface (`mcpServers` / `servers`);
/// `None` for hook / Goose / CLI-delegate surfaces (no `servers_key`).
pub(super) const fn servers_key_of(spec: &AgentSpec) -> Option<&'static str> {
    match &spec.format {
        ConfigFormat::Json { servers_key } => Some(servers_key),
        _ => None,
    }
}

/// The hook client a `Hooks` surface drives, used to render/extract the right
/// per-client hook block for the manifest. `None` for non-hook surfaces.
pub(super) const fn hook_surface_of(spec: &AgentSpec) -> Option<HookSurface> {
    match &spec.format {
        ConfigFormat::Hooks { surface } => Some(*surface),
        _ => None,
    }
}

/// Where the config file lives.
pub(super) enum PathScope {
    /// Resolved under `$HOME` (or the `DIFFLORE_MCP_HOME` override; see
    /// [`super::common::home_path`]).
    Home,
    /// Resolved under the current working directory (project-local, e.g. Roo
    /// Code, Cursor hooks).
    Cwd,
}

/// Which hook client a [`ConfigFormat::Hooks`] surface drives. Each variant
/// pins the install / uninstall / probe trio already implemented in
/// `hooks_install.rs`, so install (merge) and uninstall (drop) share the exact
/// per-client `retain(...)` matcher rather than re-deriving it.
#[derive(Clone, Copy)]
pub(super) enum HookSurface {
    /// Claude Code lifecycle hooks. These are *not* installed standalone — the
    /// Claude MCP `CliDelegate` arm piggybacks the merge (mirrors the legacy
    /// `install_claude_code` behaviour). The row exists so detect + uninstall
    /// see Claude hooks as a first-class surface.
    Claude,
    Cursor,
    Gemini,
    Windsurf,
}

/// How DiffLore is written into / read from this surface.
pub(super) enum ConfigFormat {
    /// JSON file, MCP server map under `servers_key` ("mcpServers" for most,
    /// "servers" for Copilot CLI).
    Json { servers_key: &'static str },

    /// Goose-style YAML, top-level `mcpServers:` block, line-edited (no YAML
    /// dependency — best-effort line editor, not a parser).
    Yaml,

    /// Delegate to the agent's own CLI (Claude / Codex). `add_dry_run` /
    /// `remove_dry_run` carry the human "would run: …" strings (bin shown as
    /// `difflore`, never the absolute path, to keep redaction stable). The
    /// runtime `add`/`remove` invocations splice the resolved binary in via the
    /// `{bin}` placeholder.
    CliDelegate {
        cli: &'static str,
        /// Args for `add`; `{bin}` is replaced with the resolved MCP binary.
        add_args: &'static [&'static str],
        /// Args for `remove` (idempotent).
        remove_args: &'static [&'static str],
        /// Detection probe args, e.g. `["mcp", "get", "difflore"]`.
        get_args: &'static [&'static str],
        /// Human dry-run string for install (already redacted).
        add_dry_run: &'static str,
        /// Human dry-run string for uninstall (already redacted).
        remove_dry_run: &'static str,
        /// Public path shown on a successful real run.
        installed_detail: &'static str,
    },

    /// Lifecycle hooks (not an MCP entry). Reuses `hooks_install.rs`
    /// merge/probe/remove fns unchanged.
    Hooks { surface: HookSurface },
}

/// What signals "this agent is installed on this machine?" — mirrors the
/// scattered parent-exists / which-CLI / sibling-dir checks the per-agent fns
/// used to do inline. The `Skipped(..)` reason strings are reproduced verbatim
/// so `outcome_already_installed` / dry-run partitioning stay correct.
pub(super) enum DetectSignal {
    /// Config-file parent directory exists. `skip_reason` is the verbatim
    /// `Skipped(..)` message emitted when it doesn't.
    ParentDir { skip_reason: &'static str },
    /// Parent dir exists, OR the named CLI is on PATH (Gemini, Crush, Copilot,
    /// Goose). `skip_reason` is emitted when neither holds.
    ParentDirOrCli {
        cli: &'static str,
        skip_reason: &'static str,
    },
    /// A sibling directory (relative to home) must exist (Antigravity rides
    /// `~/.gemini/`; Windsurf hooks ride `~/.codeium/...`).
    SiblingDir {
        segments: &'static [&'static [&'static str]],
        skip_reason: &'static str,
    },
    /// The named CLI on PATH is the sole signal (Claude / Codex `CliDelegate`).
    Cli {
        cli: &'static str,
        skip_reason: &'static str,
    },
    /// The config-file parent dir must exist; if absent emit manual
    /// instructions (never create speculatively — Warp).
    DirOrManual { manual_hint: &'static str },
    /// Installation is driven entirely by another surface (Claude hooks ride
    /// the Claude MCP install). Never installed/uninstalled standalone.
    RidesAlong,
}

pub(super) struct AgentSpec {
    /// Surface display name == `TargetOutcome.name` == `TargetStatus.name`.
    /// `canonical_target_key` + `client_name_for_surface` (and their tests)
    /// key off these exact strings.
    pub name: &'static str,
    /// Display client this surface rolls up into (`snapshot.rs` grouping). e.g.
    /// both "Cursor" and "Cursor hooks" roll up into client "Cursor".
    pub client: &'static str,
    pub scope: PathScope,
    /// Path segments under home/cwd. Empty for `CliDelegate` rows (no file).
    pub segments: &'static [&'static str],
    /// Public, redacted display path ("~/.cursor/mcp.json", "./.roo/mcp.json").
    pub display: &'static str,
    pub format: ConfigFormat,
    pub detect: DetectSignal,
    /// Claude-only: skip if the plugin route already wired MCP + hooks.
    pub skip_if_plugin: bool,
}

/// One row per surface. Order is load-bearing: `collect_agent_statuses`
/// surfaces Claude Code → Claude Code hooks → Codex first, then the rest.
pub(super) static AGENTS: &[AgentSpec] = &[
    AgentSpec {
        name: "Claude Code",
        client: "Claude Code",
        scope: PathScope::Home,
        segments: &[],
        display: "claude mcp add -s user difflore",
        format: ConfigFormat::CliDelegate {
            cli: "claude",
            add_args: &[
                "mcp",
                "add",
                "-s",
                "user",
                "difflore",
                "{bin}",
                "mcp-server",
            ],
            remove_args: &["mcp", "remove", "-s", "user", "difflore"],
            get_args: &["mcp", "get", "difflore"],
            add_dry_run: "would run: claude mcp add -s user difflore difflore mcp-server",
            remove_dry_run: "would run: claude mcp remove -s user difflore",
            installed_detail: "user-scope MCP via `claude mcp add`",
        },
        detect: DetectSignal::Cli {
            cli: "claude",
            skip_reason: "`claude` CLI not on PATH",
        },
        skip_if_plugin: true,
    },
    AgentSpec {
        name: "Claude Code hooks",
        client: "Claude Code",
        scope: PathScope::Home,
        segments: &[".claude", "settings.json"],
        display: "~/.claude/settings.json",
        format: ConfigFormat::Hooks {
            surface: HookSurface::Claude,
        },
        detect: DetectSignal::RidesAlong,
        skip_if_plugin: false,
    },
    AgentSpec {
        name: "Codex",
        client: "Codex",
        scope: PathScope::Home,
        segments: &[],
        display: "codex mcp add difflore",
        format: ConfigFormat::CliDelegate {
            cli: "codex",
            add_args: &["mcp", "add", "difflore", "--", "{bin}", "mcp-server"],
            remove_args: &["mcp", "remove", "difflore"],
            get_args: &["mcp", "get", "difflore"],
            add_dry_run: "would run: codex mcp add difflore -- difflore mcp-server",
            remove_dry_run: "would run: codex mcp remove difflore",
            installed_detail: "~/.codex/config.toml via `codex mcp add`",
        },
        detect: DetectSignal::Cli {
            cli: "codex",
            skip_reason: "`codex` CLI not on PATH",
        },
        skip_if_plugin: false,
    },
    AgentSpec {
        name: "Cursor",
        client: "Cursor",
        scope: PathScope::Home,
        segments: &[".cursor", "mcp.json"],
        display: "~/.cursor/mcp.json",
        format: ConfigFormat::Json {
            servers_key: "mcpServers",
        },
        detect: DetectSignal::ParentDir {
            skip_reason: "~/.cursor/ not found",
        },
        skip_if_plugin: false,
    },
    AgentSpec {
        name: "Cursor hooks",
        client: "Cursor",
        scope: PathScope::Cwd,
        segments: &[".cursor", "hooks.json"],
        display: "./.cursor/hooks.json",
        format: ConfigFormat::Hooks {
            surface: HookSurface::Cursor,
        },
        // Detection signal handled inside `install_cursor_hooks`; the table row
        // exists for detect (probe) + uninstall.
        detect: DetectSignal::RidesAlong,
        skip_if_plugin: false,
    },
    AgentSpec {
        name: "Gemini",
        client: "Gemini CLI",
        scope: PathScope::Home,
        segments: &[".gemini", "settings.json"],
        display: "~/.gemini/settings.json",
        format: ConfigFormat::Json {
            servers_key: "mcpServers",
        },
        detect: DetectSignal::ParentDirOrCli {
            cli: "gemini",
            skip_reason: "~/.gemini/ not found and `gemini` CLI not on PATH",
        },
        skip_if_plugin: false,
    },
    AgentSpec {
        name: "Gemini hooks",
        client: "Gemini CLI",
        scope: PathScope::Home,
        segments: &[".gemini", "settings.json"],
        display: "~/.gemini/settings.json",
        format: ConfigFormat::Hooks {
            surface: HookSurface::Gemini,
        },
        detect: DetectSignal::RidesAlong,
        skip_if_plugin: false,
    },
    AgentSpec {
        name: "Copilot CLI",
        client: "Copilot CLI",
        scope: PathScope::Home,
        segments: &[".github", "copilot", "mcp.json"],
        display: "~/.github/copilot/mcp.json",
        format: ConfigFormat::Json {
            servers_key: "servers",
        },
        detect: DetectSignal::ParentDirOrCli {
            cli: "copilot",
            skip_reason: "~/.github/copilot/ not found and `copilot` CLI not on PATH",
        },
        skip_if_plugin: false,
    },
    AgentSpec {
        name: "Antigravity",
        client: "Antigravity",
        scope: PathScope::Home,
        segments: &[".gemini", "antigravity", "mcp_config.json"],
        display: "~/.gemini/antigravity/mcp_config.json",
        format: ConfigFormat::Json {
            servers_key: "mcpServers",
        },
        detect: DetectSignal::SiblingDir {
            segments: &[&[".gemini"]],
            skip_reason: "~/.gemini/antigravity/ not found",
        },
        skip_if_plugin: false,
    },
    AgentSpec {
        name: "Goose",
        client: "Goose",
        scope: PathScope::Home,
        segments: &[".config", "goose", "config.yaml"],
        display: "~/.config/goose/config.yaml",
        format: ConfigFormat::Yaml,
        detect: DetectSignal::ParentDirOrCli {
            cli: "goose",
            skip_reason: "~/.config/goose/ not found and `goose` CLI not on PATH",
        },
        skip_if_plugin: false,
    },
    AgentSpec {
        name: "Crush",
        client: "Crush",
        scope: PathScope::Home,
        segments: &[".config", "crush", "mcp.json"],
        display: "~/.config/crush/mcp.json",
        format: ConfigFormat::Json {
            servers_key: "mcpServers",
        },
        detect: DetectSignal::ParentDirOrCli {
            cli: "crush",
            skip_reason: "~/.config/crush/ not found and `crush` CLI not on PATH",
        },
        skip_if_plugin: false,
    },
    AgentSpec {
        name: "Roo Code",
        client: "Roo Code",
        scope: PathScope::Cwd,
        segments: &[".roo", "mcp.json"],
        display: "./.roo/mcp.json",
        format: ConfigFormat::Json {
            servers_key: "mcpServers",
        },
        detect: DetectSignal::ParentDir {
            skip_reason: "./.roo/ not found in current workspace (Roo Code is project-local)",
        },
        skip_if_plugin: false,
    },
    AgentSpec {
        name: "Warp",
        client: "Warp",
        scope: PathScope::Home,
        segments: &[".warp", "mcp.json"],
        display: "~/.warp/mcp.json",
        format: ConfigFormat::Json {
            servers_key: "mcpServers",
        },
        detect: DetectSignal::DirOrManual {
            #[allow(clippy::literal_string_with_formatting_args)]
            // reason: literal is a template fed to .replace(), not a format string.
            manual_hint: "~/.warp/ not found. In Warp, open Settings → AI → Manage MCP servers and add: \
                 command=`{bin}`, args=[\"mcp-server\"]",
        },
        skip_if_plugin: false,
    },
    AgentSpec {
        name: "Windsurf hooks",
        client: "Windsurf",
        scope: PathScope::Home,
        segments: &[".codeium", "windsurf", "hooks.json"],
        display: "~/.codeium/windsurf/hooks.json",
        format: ConfigFormat::Hooks {
            surface: HookSurface::Windsurf,
        },
        detect: DetectSignal::RidesAlong,
        skip_if_plugin: false,
    },
];

/// Find the `AGENTS` row for an exact surface display name. Used by the install
/// manifest to recover a target's writer metadata from its recorded name.
/// Returns `None` for an unknown name.
pub(super) fn find_spec(name: &str) -> Option<&'static AgentSpec> {
    AGENTS.iter().find(|spec| spec.name == name)
}

/// Resolve a spec's config path under its [`PathScope`]. `Err` only on a home /
/// cwd resolution failure, which the callers fold into an Unknown/error
/// outcome the same way the legacy per-agent fns did.
pub(super) fn resolve_path(spec: &AgentSpec) -> Result<PathBuf, String> {
    match spec.scope {
        PathScope::Home => home_path(spec.segments),
        PathScope::Cwd => cwd_path(spec.segments),
    }
}

/// Detection driver: produces a `TargetStatus`, reading every signal from
/// `spec`.
pub(super) fn detect(spec: &AgentSpec, bin: &str) -> TargetStatus {
    match &spec.format {
        ConfigFormat::CliDelegate { cli, get_args, .. } => probe_cli_mcp(spec.name, cli, get_args),
        ConfigFormat::Json { servers_key } => with_path(spec, |path| {
            probe_json_install(spec.name, path, servers_key, bin)
        }),
        ConfigFormat::Yaml => with_path(spec, |path| probe_goose_install(spec.name, path, bin)),
        ConfigFormat::Hooks { surface } => with_path(spec, |path| match surface {
            HookSurface::Claude => {
                probe_json_hooks_by_nested_command(spec.name, path, "claude-code")
            }
            HookSurface::Cursor => probe_json_hooks_by_name(spec.name, path),
            HookSurface::Gemini => probe_json_hooks_by_group(spec.name, path),
            HookSurface::Windsurf => probe_json_hooks_by_command(spec.name, path, "windsurf"),
        }),
    }
}

/// Resolve `spec`'s path and run `f`, mapping a resolution failure to an
/// Unknown `TargetStatus`.
fn with_path<F: FnOnce(&PathBuf) -> TargetStatus>(spec: &AgentSpec, f: F) -> TargetStatus {
    match resolve_path(spec) {
        Ok(path) => f(&path),
        Err(_) => TargetStatus {
            name: spec.name,
            detected: false,
            state: super::InstallState::Unknown,
            detail: Some(format!("could not resolve {}", spec.display)),
        },
    }
}

/// Install driver: one row → one outcome. `mcp_bin` is written into JSON/YAML
/// config; `cli_bin` is used to derive the hook shim path. Applies the
/// detection gating + `Skipped(..)` reasons, then delegates the write to the
/// leaf engines.
pub(super) fn install(
    spec: &AgentSpec,
    mcp_bin: &str,
    cli_bin: &str,
    dry_run: bool,
) -> TargetOutcome {
    // Detection gate first (PATH / parent-dir checks). Hooks surfaces
    // `RidesAlong` and do their own detection inside their installers, so this
    // is a no-op for them.
    if let Some(skip) = detect_gate(spec) {
        return skip;
    }
    // Claude (and only Claude) short-circuits when the plugin route already
    // wired MCP + hooks; the CLI add would otherwise dupe the MCP entry. Must
    // run after the PATH check.
    if spec.skip_if_plugin && claude_plugin_installed() {
        return TargetOutcome {
            name: spec.name,
            status: Status::Skipped(
                "DiffLore plugin already installed; MCP + hooks auto-registered".into(),
            ),
            detail: "~/.claude/plugins/cache/.../difflore/".into(),
        };
    }

    match &spec.format {
        ConfigFormat::CliDelegate {
            cli,
            add_args,
            remove_args,
            add_dry_run,
            installed_detail,
            ..
        } => install_cli_delegate(
            spec,
            cli,
            add_args,
            remove_args,
            add_dry_run,
            installed_detail,
            mcp_bin,
            cli_bin,
            dry_run,
        ),
        ConfigFormat::Json { servers_key } => match resolve_path(spec) {
            Ok(path) => finish_json_install(spec.name, &path, mcp_bin, servers_key, dry_run),
            Err(e) => error_outcome(spec.name, e),
        },
        ConfigFormat::Yaml => match resolve_path(spec) {
            Ok(path) => match merge_goose_yaml_config(&path, mcp_bin, dry_run) {
                Ok(existed) => TargetOutcome {
                    name: spec.name,
                    status: if existed {
                        Status::Updated
                    } else {
                        Status::Installed
                    },
                    detail: path.display().to_string(),
                },
                Err(e) => error_outcome(spec.name, e),
            },
            Err(e) => error_outcome(spec.name, e),
        },
        ConfigFormat::Hooks { surface } => match surface {
            // Claude hooks are installed as a side effect of the Claude MCP
            // `CliDelegate` arm (see `install_cli_delegate`); no standalone
            // install step.
            HookSurface::Claude => TargetOutcome {
                name: spec.name,
                status: Status::Skipped("installed with Claude Code MCP".into()),
                detail: String::new(),
            },
            HookSurface::Cursor => install_cursor_hooks(cli_bin, dry_run),
            HookSurface::Gemini => install_gemini_cli_hooks(cli_bin, dry_run),
            HookSurface::Windsurf => install_windsurf_hooks(cli_bin, dry_run),
        },
    }
}

/// Run a `CliDelegate` install (the PATH detect-gate + plugin short-circuit
/// already ran in [`install`]): render the dry-run string, otherwise
/// `remove`-then-`add` (idempotent) through the agent's own CLI. Claude
/// additionally piggybacks its lifecycle-hook merge so one install wires
/// MCP + hooks.
#[allow(clippy::too_many_arguments)]
// reason: the CliDelegate row's data (cli, add/remove args, dry-run + detail
// strings) plus the two binaries are all genuinely independent inputs; bundling
// them into a struct purely to satisfy the lint would add indirection without
// improving clarity.
fn install_cli_delegate(
    spec: &AgentSpec,
    cli: &str,
    add_args: &[&str],
    remove_args: &[&str],
    add_dry_run: &str,
    installed_detail: &str,
    mcp_bin: &str,
    cli_bin: &str,
    dry_run: bool,
) -> TargetOutcome {
    if dry_run {
        return TargetOutcome {
            name: spec.name,
            status: Status::Installed,
            detail: add_dry_run.to_owned(),
        };
    }

    // `remove`-then-`add` keeps the entry canonical regardless of prior state.
    let _ = std::process::Command::new(cli).args(remove_args).output();
    let resolved: Vec<String> = add_args
        .iter()
        .map(|a| {
            if *a == "{bin}" {
                mcp_bin.to_owned()
            } else {
                (*a).to_owned()
            }
        })
        .collect();
    let out = std::process::Command::new(cli).args(&resolved).output();
    match out {
        Ok(o) if o.status.success() => {
            // Claude `mcp add` registers tools but not the read-gate hooks;
            // merge them directly so the next session has hook coverage.
            if spec.skip_if_plugin {
                let hook_summary = match install_claude_code_hooks(cli_bin) {
                    Ok(installed) if installed > 0 => format!(
                        "user-scope MCP + {installed} lifecycle hooks merged into ~/.claude/settings.json"
                    ),
                    Ok(_) => {
                        "user-scope MCP via `claude mcp add` (hooks already up-to-date)".to_owned()
                    }
                    Err(err) => format!(
                        "user-scope MCP via `claude mcp add` (hook merge failed: {err}; rerun later or use `/plugin install difflore` inside Claude Code)"
                    ),
                };
                return TargetOutcome {
                    name: spec.name,
                    status: Status::Installed,
                    detail: hook_summary,
                };
            }
            TargetOutcome {
                name: spec.name,
                status: Status::Installed,
                detail: installed_detail.to_owned(),
            }
        }
        Ok(o) => TargetOutcome {
            name: spec.name,
            status: Status::Error(format!(
                "`{cli} {}` exit {}: {}",
                add_command_label(add_args),
                o.status,
                String::from_utf8_lossy(&o.stderr).trim()
            )),
            detail: String::new(),
        },
        Err(e) => TargetOutcome {
            name: spec.name,
            status: Status::Error(format!("could not invoke `{cli}`: {e}")),
            detail: String::new(),
        },
    }
}

/// The leading subcommand words of an add invocation, for error messages
/// (`\`claude mcp add\` exit …`). Both delegates start `mcp add …`, so taking
/// the first two words yields the `<cli> mcp add` label.
fn add_command_label(add_args: &[&str]) -> String {
    add_args
        .iter()
        .take(2)
        .copied()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Evaluate a spec's [`DetectSignal`]; `Some(outcome)` means "skip, not
/// detected". `None` means "proceed". Hooks surfaces that `RidesAlong` return
/// `None` (their installer does its own detection).
fn detect_gate(spec: &AgentSpec) -> Option<TargetOutcome> {
    // `Some(reason)` => not detected, skip; `None` => proceed.
    let skip_reason: Option<String> = match &spec.detect {
        DetectSignal::Cli { cli, skip_reason } => which::which(cli)
            .is_err()
            .then(|| (*skip_reason).to_owned()),
        DetectSignal::ParentDir { skip_reason } => {
            (!parent_exists(spec)).then(|| (*skip_reason).to_owned())
        }
        DetectSignal::ParentDirOrCli { cli, skip_reason } => {
            (!parent_exists(spec) && which::which(cli).is_err()).then(|| (*skip_reason).to_owned())
        }
        DetectSignal::SiblingDir {
            segments,
            skip_reason,
        } => (!any_sibling_exists(segments)).then(|| (*skip_reason).to_owned()),
        DetectSignal::DirOrManual { manual_hint } => (!parent_exists(spec)).then(|| {
            #[allow(clippy::literal_string_with_formatting_args)]
            // reason: hint is a template fed to .replace(), not a format string.
            manual_hint.replace("{bin}", "difflore")
        }),
        DetectSignal::RidesAlong => None,
    };
    skip_reason.map(|reason| TargetOutcome {
        name: spec.name,
        status: Status::Skipped(reason),
        detail: String::new(),
    })
}

fn parent_exists(spec: &AgentSpec) -> bool {
    resolve_path(spec)
        .ok()
        .and_then(|path| path.parent().map(std::path::Path::exists))
        .unwrap_or(false)
}

fn any_sibling_exists(segments: &[&[&str]]) -> bool {
    segments
        .iter()
        .any(|seg| home_path(seg).ok().is_some_and(|p| p.exists()))
}

/// Uninstall driver: the inverse of [`install`]. Each format removes only
/// DiffLore's own entry, leaving every other server / hook intact.
pub(super) fn uninstall(spec: &AgentSpec, dry_run: bool) -> TargetOutcome {
    match &spec.format {
        ConfigFormat::CliDelegate {
            cli,
            remove_args,
            remove_dry_run,
            ..
        } => uninstall_cli_delegate(spec, cli, remove_args, remove_dry_run, dry_run),
        ConfigFormat::Json { servers_key } => match resolve_path(spec) {
            Ok(path) => finish_json_uninstall(spec.name, &path, servers_key, dry_run),
            Err(e) => error_outcome(spec.name, e),
        },
        ConfigFormat::Yaml => match resolve_path(spec) {
            Ok(path) => match remove_goose_yaml_config(&path, dry_run) {
                Ok(true) => TargetOutcome {
                    name: spec.name,
                    status: Status::Removed,
                    detail: path.display().to_string(),
                },
                Ok(false) => TargetOutcome {
                    name: spec.name,
                    status: Status::Skipped("no difflore block to remove".into()),
                    detail: String::new(),
                },
                Err(e) => error_outcome(spec.name, e),
            },
            Err(e) => error_outcome(spec.name, e),
        },
        ConfigFormat::Hooks { surface } => match surface {
            // Claude hooks are stripped inside the Claude MCP uninstall (handled
            // by `uninstall.rs`'s claude dispatch).
            HookSurface::Claude => uninstall_claude_code_combined(spec, dry_run),
            HookSurface::Cursor => uninstall_cursor_hooks(dry_run),
            HookSurface::Gemini => uninstall_gemini_cli_hooks(dry_run),
            HookSurface::Windsurf => uninstall_windsurf_hooks(dry_run),
        },
    }
}

/// Run a `CliDelegate` uninstall: graceful skip when the CLI is absent,
/// otherwise the idempotent `remove`. Codex/Claude differ only in their data
/// rows (skip reason, remove args, dry-run text).
fn uninstall_cli_delegate(
    spec: &AgentSpec,
    cli: &str,
    remove_args: &[&str],
    remove_dry_run: &str,
    dry_run: bool,
) -> TargetOutcome {
    // Claude's uninstall also strips lifecycle hooks and surfaces a plugin
    // hint, so it has a dedicated combined path.
    if spec.skip_if_plugin {
        return uninstall_claude_code_combined(spec, dry_run);
    }
    if which::which(cli).is_err() {
        return TargetOutcome {
            name: spec.name,
            status: Status::Skipped(format!("`{cli}` CLI not on PATH")),
            detail: String::new(),
        };
    }
    if dry_run {
        return TargetOutcome {
            name: spec.name,
            status: Status::Removed,
            detail: remove_dry_run.to_owned(),
        };
    }
    match std::process::Command::new(cli).args(remove_args).output() {
        Ok(o) if o.status.success() => TargetOutcome {
            name: spec.name,
            status: Status::Removed,
            detail: format!("~/.codex/config.toml via `{cli} mcp remove`"),
        },
        // `mcp remove` is idempotent in practice; a non-zero exit most often
        // means "no such server". Treat it as already-removed (a Skip) rather
        // than a hard error so a partial uninstall can still finish.
        Ok(o) => TargetOutcome {
            name: spec.name,
            status: Status::Skipped(format!(
                "`{cli} mcp remove` exit {} (likely no difflore entry): {}",
                o.status,
                String::from_utf8_lossy(&o.stderr).trim()
            )),
            detail: String::new(),
        },
        Err(e) => TargetOutcome {
            name: spec.name,
            status: Status::Error(format!("could not invoke `{cli}`: {e}")),
            detail: String::new(),
        },
    }
}

/// Claude's combined uninstall: strip lifecycle hooks from
/// `~/.claude/settings.json`, then `claude mcp remove` the MCP entry. The
/// plugin route manages both itself, so we surface a hint and don't touch it.
/// Reachable from both the `CliDelegate` and `Hooks` Claude rows; the dispatch
/// in `uninstall.rs` runs it once.
fn uninstall_claude_code_combined(_spec: &AgentSpec, dry_run: bool) -> TargetOutcome {
    // Always reported as the "Claude Code" surface regardless of which row
    // dispatched here, so canonical-record matching and client roll-up stay
    // consistent.
    let name = "Claude Code";
    if claude_plugin_installed() {
        return TargetOutcome {
            name,
            status: Status::Skipped(
                "DiffLore plugin manages MCP + hooks; remove with `/plugin uninstall difflore` inside Claude Code".into(),
            ),
            detail: "~/.claude/plugins/cache/.../difflore/".into(),
        };
    }

    // Strip the lifecycle hook groups first. A hard error here is the only case
    // that fails the surface; everything else is reported as detail.
    let hooks_removed = match uninstall_claude_code_hooks(dry_run) {
        Ok(removed) => removed,
        Err(err) => {
            return TargetOutcome {
                name,
                status: Status::Error(format!("hook removal failed: {err}")),
                detail: String::new(),
            };
        }
    };
    let verb = if dry_run { "would remove" } else { "removed" };
    let hook_summary = if hooks_removed > 0 {
        format!("{verb} {hooks_removed} lifecycle hook group(s) from ~/.claude/settings.json")
    } else {
        "no DiffLore hooks in ~/.claude/settings.json".to_owned()
    };

    if which::which("claude").is_err() {
        let status = if hooks_removed > 0 {
            Status::Removed
        } else {
            Status::Skipped("`claude` CLI not on PATH and no DiffLore hooks to remove".into())
        };
        return TargetOutcome {
            name,
            status,
            detail: if hooks_removed > 0 {
                hook_summary
            } else {
                String::new()
            },
        };
    }

    if dry_run {
        return TargetOutcome {
            name,
            status: Status::Removed,
            detail: format!("would run: claude mcp remove -s user difflore; {hook_summary}"),
        };
    }

    // `claude mcp remove` is idempotent; ignore its exit code (a non-zero
    // status usually just means the entry was already absent).
    let _ = std::process::Command::new("claude")
        .args(["mcp", "remove", "-s", "user", "difflore"])
        .output();
    TargetOutcome {
        name,
        status: Status::Removed,
        detail: format!("user-scope MCP removed via `claude mcp remove`; {hook_summary}"),
    }
}

/// Canonical lowercased key for a surface name, used to match against the
/// canonical record's `installed_targets`. Derived from `AGENTS` so the
/// normalization table lives in one place.
pub(super) fn canonical_target_key(name: &str) -> String {
    let trimmed = name.trim();
    // Special-case the CLI aliases that aren't surface display names: the
    // `claude`/`codex` CLIs report bare lowercase names.
    let lower = trimmed.to_ascii_lowercase();
    match lower.as_str() {
        "claude" | "claude code" => return "claude".into(),
        "claude hooks" | "claude code hooks" => return "claude hooks".into(),
        _ => {}
    }
    if let Some(spec) = AGENTS.iter().find(|spec| spec.name == trimmed) {
        return surface_key(spec.name);
    }
    lower
}

/// The canonical key for a known `AGENTS` surface name.
fn surface_key(name: &str) -> String {
    match name {
        "Claude Code" => "claude".into(),
        "Claude Code hooks" => "claude hooks".into(),
        other => other.to_ascii_lowercase(),
    }
}

/// Display client a surface rolls up into (`Cursor hooks` → `Cursor`). Derived
/// from `AGENTS`; unknown surfaces report "unknown client".
pub(super) fn client_name_for_surface(surface: &str) -> &'static str {
    let key = canonical_target_key(surface);
    // Match by the canonical key so CLI aliases ("claude") and hook surfaces
    // ("claude hooks") both resolve to their display client.
    for spec in AGENTS {
        if surface_key(spec.name) == key {
            return spec.client;
        }
    }
    "unknown client"
}
