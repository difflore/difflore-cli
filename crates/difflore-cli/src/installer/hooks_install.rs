//! Hook integrations for Claude Code / Cursor / Gemini CLI / Windsurf.
//!
//! These clients support hook lifecycles that let `DiffLore` surface rules
//! after file edits, shell runs, and session events. For each client we
//! register a hook entry that invokes
//!
//! ```text
//! <difflore-hook> --client <name>
//! ```
//!
//! The shim forwards to the hot daemon over local IPC, falling back to the
//! in-process hook runtime when the daemon is down. Existing non-difflore
//! hooks in the target config are preserved.

use std::{fs, path::Path};

use anyhow::{Context, anyhow};
use serde_json::{Value, json};

use crate::error::{CliError, CliResult};

use super::common::{cwd_path, error_outcome, home_path};
use super::json_config::{load_json_object, write_json_object};
use super::{InstallState, Status, TargetOutcome, TargetStatus};

// ── Outcome helpers ──────────────────────────────────────────────────────

fn skipped_outcome(name: &'static str, msg: impl Into<String>) -> TargetOutcome {
    TargetOutcome {
        name,
        status: Status::Skipped(msg.into()),
        detail: String::new(),
    }
}

fn finalize_hook_outcome(
    name: &'static str,
    primary_path: &Path,
    res: anyhow::Result<bool>,
) -> TargetOutcome {
    match res {
        Ok(existed) => TargetOutcome {
            name,
            status: if existed {
                Status::Updated
            } else {
                Status::Installed
            },
            detail: primary_path.display().to_string(),
        },
        Err(e) => TargetOutcome {
            name,
            status: Status::Error(e.to_string()),
            detail: String::new(),
        },
    }
}

fn load_json_object_anyhow(path: &Path) -> anyhow::Result<serde_json::Map<String, Value>> {
    load_json_object(&path.to_path_buf()).map_err(|e| anyhow!(e))
}

fn write_json_object_anyhow(
    path: &Path,
    obj: &serde_json::Map<String, Value>,
) -> anyhow::Result<()> {
    write_json_object(&path.to_path_buf(), obj).map_err(|e| anyhow!(e))
}

// ── Claude Code hooks ────────────────────────────────────────────────────

/// Single source of truth for the Claude Code `PostToolUse` matcher.
///
/// The writer ([`merge_claude_code_hooks`]) and the hash source
/// ([`render_claude_code_hook_block`]) MUST agree on this string: `agents
/// update` compares the hash of our rendered block against the bytes on disk,
/// so a drifted matcher makes every fresh install re-hash as "locally edited"
/// and blocks it from ever upgrading.
pub(super) const CLAUDE_POST_TOOL_USE_MATCHER: &str = "Edit|MultiEdit|Write|Bash";

/// The Claude Code lifecycle events DiffLore registers, with their matchers.
/// Shared verbatim by [`merge_claude_code_hooks`] and
/// [`render_claude_code_hook_block`] so the two cannot drift.
///
/// NOTE: no PreToolUse(Read) registration. Pre-read injection was retired to
/// a dispatcher noop (a Read is too weak a signal — see
/// hook/runtime/dispatch.rs), so registering it spawned the hook binary on
/// EVERY Read for a guaranteed empty response. The dead registration is also
/// stripped from existing installs by the cleanup pass in
/// [`merge_claude_code_hooks`].
const CLAUDE_HOOK_EVENT_MATCHERS: &[(&str, Option<&str>)] = &[
    ("PostToolUse", Some(CLAUDE_POST_TOOL_USE_MATCHER)),
    ("SessionStart", Some("startup|clear|compact")),
    ("UserPromptSubmit", None),
    ("Stop", None),
    ("SessionEnd", None),
];

/// Build the hook group object for one Claude Code event — the single shape
/// both [`merge_claude_code_hooks`] writes and [`render_claude_code_hook_block`]
/// hashes. (`PreToolUse` only occurs in the legacy tables of
/// [`legacy_claude_code_hook_blocks`]; its 2s budget is kept so legacy hashes
/// reproduce the bytes old installers wrote.)
fn claude_hook_group(event: &str, matcher: Option<&str>, command: &str) -> Value {
    let timeout_ms = match event {
        "PreToolUse" => 2000,
        "PostToolUse" | "UserPromptSubmit" => 5000,
        _ => 10000,
    };
    let mut group = serde_json::Map::new();
    if let Some(m) = matcher {
        group.insert("matcher".to_owned(), Value::from(m));
    }
    group.insert(
        "hooks".to_owned(),
        Value::Array(vec![json!({
            "type": "command",
            "command": command,
            "timeout": timeout_ms,
        })]),
    );
    Value::Object(group)
}

/// Merge `DiffLore` lifecycle hooks into `~/.claude/settings.json`. Returns the
/// number of event matchers added or refreshed (`Ok(0)` = already wired).
pub(super) fn install_claude_code_hooks(bin: &str) -> CliResult<usize> {
    let settings_path = home_path(&[".claude", "settings.json"]).map_err(CliError::Message)?;
    merge_claude_code_hooks(&settings_path, bin).map_err(|e| CliError::Message(e.to_string()))
}

pub(super) fn merge_claude_code_hooks(settings_path: &Path, bin: &str) -> anyhow::Result<usize> {
    if let Some(parent) = settings_path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }

    let command = hook_command_string(bin, "claude-code");
    let mut cfg = load_json_object_anyhow(settings_path)?;
    let hooks_value = cfg
        .entry("hooks".to_owned())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    let hooks_obj = hooks_value
        .as_object_mut()
        .ok_or_else(|| anyhow!("{}: `hooks` is not a JSON object", settings_path.display()))?;

    let mut changed = 0usize;
    // Strip every existing difflore claude-code group first — including
    // groups under events we no longer register (the retired
    // PreToolUse(Read)), so an upgrade cleans old installs instead of
    // leaving the dead registration to fire on every Read forever.
    let mut emptied_events: Vec<String> = Vec::new();
    for (event, groups_value) in hooks_obj.iter_mut() {
        let Some(groups) = groups_value.as_array_mut() else {
            continue;
        };
        let before = groups.len();
        groups.retain(|g| {
            let hooks = g.get("hooks").and_then(|h| h.as_array());
            match hooks {
                Some(arr) => !arr.iter().any(|h| {
                    h.get("command")
                        .and_then(|c| c.as_str())
                        .is_some_and(|s| hook_command_matches_client(s, "claude-code"))
                }),
                None => true,
            }
        });
        if groups.len() != before {
            changed += 1;
        }
        if groups.is_empty() {
            emptied_events.push(event.clone());
        }
    }
    // Drop event arrays the strip emptied so a retired event doesn't leave a
    // dangling `"PreToolUse": []` behind; events still registered are
    // recreated by the `entry()` below.
    for event in emptied_events {
        hooks_obj.remove(&event);
    }

    for (event, matcher) in CLAUDE_HOOK_EVENT_MATCHERS {
        let groups_value = hooks_obj
            .entry((*event).to_owned())
            .or_insert_with(|| Value::Array(Vec::new()));
        let groups = groups_value
            .as_array_mut()
            .ok_or_else(|| anyhow!("{}: hooks.{event} is not an array", settings_path.display()))?;

        groups.push(claude_hook_group(event, *matcher, &command));
        changed += 1;
    }

    let serialised =
        serde_json::to_string_pretty(&Value::Object(cfg)).context("serialise settings")?;
    super::common::write_atomic(settings_path, serialised)
        .with_context(|| format!("write {}", settings_path.display()))?;
    Ok(changed)
}

/// Inverse of [`install_claude_code_hooks`]: strip DiffLore's lifecycle hook
/// groups from `~/.claude/settings.json`, preserving every user hook. Returns
/// the number of event matchers that had a DiffLore group removed.
pub(super) fn uninstall_claude_code_hooks(dry_run: bool) -> CliResult<usize> {
    let settings_path = home_path(&[".claude", "settings.json"]).map_err(CliError::Message)?;
    remove_claude_code_hooks(&settings_path, dry_run).map_err(|e| CliError::Message(e.to_string()))
}

pub(super) fn remove_claude_code_hooks(
    settings_path: &Path,
    dry_run: bool,
) -> anyhow::Result<usize> {
    if !settings_path.exists() {
        return Ok(0);
    }
    let mut cfg = load_json_object_anyhow(settings_path)?;
    let Some(hooks_value) = cfg.get_mut("hooks") else {
        return Ok(0);
    };
    let Some(hooks_obj) = hooks_value.as_object_mut() else {
        return Ok(0);
    };

    let mut removed = 0usize;
    let mut empty_events: Vec<String> = Vec::new();
    for (event, groups_value) in hooks_obj.iter_mut() {
        let Some(groups) = groups_value.as_array_mut() else {
            continue;
        };
        let before = groups.len();
        groups.retain(|g| {
            let hooks = g.get("hooks").and_then(|h| h.as_array());
            match hooks {
                Some(arr) => !arr.iter().any(|h| {
                    h.get("command")
                        .and_then(|c| c.as_str())
                        .is_some_and(|s| hook_command_matches_client(s, "claude-code"))
                }),
                None => true,
            }
        });
        if groups.len() != before {
            removed += 1;
        }
        if groups.is_empty() {
            empty_events.push(event.clone());
        }
    }
    for event in empty_events {
        hooks_obj.remove(&event);
    }
    if hooks_obj.is_empty() {
        cfg.remove("hooks");
    }

    if removed > 0 && !dry_run {
        let serialised =
            serde_json::to_string_pretty(&Value::Object(cfg)).context("serialise settings")?;
        super::common::write_atomic(settings_path, serialised)
            .with_context(|| format!("write {}", settings_path.display()))?;
    }
    Ok(removed)
}

// ── Cursor hooks ────────────────────────────────────────────────────────

pub(super) fn install_cursor_hooks(bin: &str, dry_run: bool) -> TargetOutcome {
    let cursor_dir = match cwd_path(&[".cursor"]) {
        Ok(p) => p,
        Err(e) => return error_outcome("Cursor hooks", e),
    };
    if !cursor_dir.exists() {
        return skipped_outcome(
            "Cursor hooks",
            "./.cursor/ not found (Cursor hooks are project-local)",
        );
    }
    let path = cursor_dir.join("hooks.json");
    let res = merge_cursor_hooks(&path, bin, dry_run);
    finalize_hook_outcome("Cursor hooks", &path, res)
}

pub(super) const CURSOR_HOOK_EVENTS: &[&str] = &[
    "afterFileEdit",
    "afterMCPExecution",
    "afterShellExecution",
    "beforeSubmitPrompt",
    "stop",
];

pub(super) fn merge_cursor_hooks(path: &Path, bin: &str, dry_run: bool) -> anyhow::Result<bool> {
    let mut cfg = load_json_object_anyhow(path)?;
    cfg.entry("version".to_owned())
        .or_insert_with(|| Value::from(1));
    let hooks = cfg
        .entry("hooks".to_owned())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    let hooks_obj = hooks
        .as_object_mut()
        .ok_or_else(|| anyhow!("{}: `hooks` is not a JSON object", path.display()))?;

    let command = hook_command_string(bin, "cursor");
    let mut existed = false;
    for event in CURSOR_HOOK_EVENTS {
        let entries = hooks_obj
            .entry((*event).to_owned())
            .or_insert_with(|| Value::Array(Vec::new()));
        let arr = entries
            .as_array_mut()
            .ok_or_else(|| anyhow!("{}: hooks.{event} is not an array", path.display()))?;
        let before = arr.len();
        arr.retain(|v| v.get("name").and_then(|n| n.as_str()) != Some("difflore"));
        if arr.len() != before {
            existed = true;
        }
        arr.push(json!({
            "name": "difflore",
            "command": command,
            "timeout": 5000,
        }));
    }

    if !dry_run {
        write_json_object_anyhow(path, &cfg)?;
    }
    Ok(existed)
}

pub(super) fn uninstall_cursor_hooks(dry_run: bool) -> TargetOutcome {
    let cursor_dir = match cwd_path(&[".cursor"]) {
        Ok(p) => p,
        Err(e) => return error_outcome("Cursor hooks", e),
    };
    let path = cursor_dir.join("hooks.json");
    finalize_uninstall_outcome("Cursor hooks", &path, remove_cursor_hooks(&path, dry_run))
}

pub(super) fn remove_cursor_hooks(path: &Path, dry_run: bool) -> anyhow::Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let mut cfg = load_json_object_anyhow(path)?;
    let Some(hooks) = cfg.get_mut("hooks") else {
        return Ok(false);
    };
    let Some(hooks_obj) = hooks.as_object_mut() else {
        return Ok(false);
    };
    let mut removed = false;
    let mut empty_events: Vec<String> = Vec::new();
    for (event, entries) in hooks_obj.iter_mut() {
        let Some(arr) = entries.as_array_mut() else {
            continue;
        };
        let before = arr.len();
        arr.retain(|v| v.get("name").and_then(|n| n.as_str()) != Some("difflore"));
        if arr.len() != before {
            removed = true;
        }
        if arr.is_empty() {
            empty_events.push(event.clone());
        }
    }
    for event in empty_events {
        hooks_obj.remove(&event);
    }
    if removed && !dry_run {
        write_json_object_anyhow(path, &cfg)?;
    }
    Ok(removed)
}

// ── Gemini CLI hooks ────────────────────────────────────────────────────

pub(super) fn install_gemini_cli_hooks(bin: &str, dry_run: bool) -> TargetOutcome {
    let settings_path = match home_path(&[".gemini", "settings.json"]) {
        Ok(p) => p,
        Err(e) => return error_outcome("Gemini hooks", e),
    };
    let parent_exists = settings_path.parent().is_some_and(Path::exists);
    if !parent_exists && which::which("gemini").is_err() {
        return skipped_outcome(
            "Gemini hooks",
            "~/.gemini/ not found and `gemini` CLI not on PATH",
        );
    }
    let md_path = match home_path(&[".gemini", "GEMINI.md"]) {
        Ok(p) => p,
        Err(e) => return error_outcome("Gemini hooks", e),
    };
    let res = merge_gemini_cli_hooks(&settings_path, &md_path, bin, dry_run);
    finalize_hook_outcome("Gemini hooks", &settings_path, res)
}

pub(super) const GEMINI_HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "BeforeAgent",
    "AfterAgent",
    "AfterTool",
    "SessionEnd",
];

pub(super) fn merge_gemini_cli_hooks(
    settings_path: &Path,
    md_path: &Path,
    bin: &str,
    dry_run: bool,
) -> anyhow::Result<bool> {
    let mut cfg = load_json_object_anyhow(settings_path)?;
    let hooks = cfg
        .entry("hooks".to_owned())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    let hooks_obj = hooks
        .as_object_mut()
        .ok_or_else(|| anyhow!("{}: `hooks` is not a JSON object", settings_path.display()))?;

    let command = hook_command_string(bin, "gemini-cli");
    let hook_entry = json!({
        "name": "difflore",
        "type": "command",
        "command": command,
        "timeout": 5000,
    });
    let mut existed = false;
    for event in GEMINI_HOOK_EVENTS {
        let entries = hooks_obj
            .entry((*event).to_owned())
            .or_insert_with(|| Value::Array(Vec::new()));
        let arr = entries
            .as_array_mut()
            .ok_or_else(|| anyhow!("{}: hooks.{event} is not an array", settings_path.display()))?;
        let before = arr.len();
        arr.retain(|v| {
            let nested = v.get("hooks").and_then(|h| h.as_array());
            !nested.is_some_and(|arr| {
                arr.iter()
                    .any(|h| h.get("name").and_then(|n| n.as_str()) == Some("difflore"))
            })
        });
        if arr.len() != before {
            existed = true;
        }
        arr.push(json!({
            "matcher": "*",
            "hooks": [hook_entry.clone()],
        }));
    }

    if !dry_run {
        write_json_object_anyhow(settings_path, &cfg)?;
        upsert_gemini_md_context(md_path)?;
    }
    Ok(existed)
}

pub(super) fn upsert_gemini_md_context(md_path: &Path) -> anyhow::Result<()> {
    let placeholder = "<difflore-context>\n# DiffLore team rules\n\nRules pulled from DiffLore will be injected here by the hook runtime.\n</difflore-context>\n";
    if md_path.exists() {
        let content = fs::read_to_string(md_path)
            .with_context(|| format!("failed to read {}", md_path.display()))?;
        if content.contains("<difflore-context>") {
            return Ok(());
        }
        let sep = if content.ends_with('\n') || content.is_empty() {
            ""
        } else {
            "\n"
        };
        let new_content = format!("{content}{sep}\n{placeholder}");
        super::common::write_atomic(md_path, new_content.as_bytes())
            .with_context(|| format!("failed to write {}", md_path.display()))?;
    } else {
        if let Some(parent) = md_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        super::common::write_atomic(md_path, placeholder.as_bytes())
            .with_context(|| format!("failed to write {}", md_path.display()))?;
    }
    Ok(())
}

pub(super) fn uninstall_gemini_cli_hooks(dry_run: bool) -> TargetOutcome {
    let settings_path = match home_path(&[".gemini", "settings.json"]) {
        Ok(p) => p,
        Err(e) => return error_outcome("Gemini hooks", e),
    };
    finalize_uninstall_outcome(
        "Gemini hooks",
        &settings_path,
        remove_gemini_cli_hooks(&settings_path, dry_run),
    )
}

pub(super) fn remove_gemini_cli_hooks(settings_path: &Path, dry_run: bool) -> anyhow::Result<bool> {
    if !settings_path.exists() {
        return Ok(false);
    }
    let mut cfg = load_json_object_anyhow(settings_path)?;
    let Some(hooks) = cfg.get_mut("hooks") else {
        return Ok(false);
    };
    let Some(hooks_obj) = hooks.as_object_mut() else {
        return Ok(false);
    };
    let mut removed = false;
    let mut empty_events: Vec<String> = Vec::new();
    for (event, entries) in hooks_obj.iter_mut() {
        let Some(arr) = entries.as_array_mut() else {
            continue;
        };
        let before = arr.len();
        arr.retain(|v| {
            let nested = v.get("hooks").and_then(|h| h.as_array());
            !nested.is_some_and(|arr| {
                arr.iter()
                    .any(|h| h.get("name").and_then(|n| n.as_str()) == Some("difflore"))
            })
        });
        if arr.len() != before {
            removed = true;
        }
        if arr.is_empty() {
            empty_events.push(event.clone());
        }
    }
    for event in empty_events {
        hooks_obj.remove(&event);
    }
    // GEMINI.md context tag is left in place: it may hold user-customized
    // content, so removing it risks clobbering it (mirrors install's
    // no-overwrite policy).
    if removed && !dry_run {
        write_json_object_anyhow(settings_path, &cfg)?;
    }
    Ok(removed)
}

// ── Windsurf hooks ──────────────────────────────────────────────────────

pub(super) fn install_windsurf_hooks(bin: &str, dry_run: bool) -> TargetOutcome {
    let hooks_path = match home_path(&[".codeium", "windsurf", "hooks.json"]) {
        Ok(p) => p,
        Err(e) => return error_outcome("Windsurf hooks", e),
    };
    let hooks_parent = hooks_path.parent().is_some_and(Path::exists);
    let windsurf_root = home_path(&[".codeium", "windsurf"]).ok();
    let codeium_root = home_path(&[".codeium"]).ok();
    let detected = hooks_parent
        || windsurf_root.as_ref().is_some_and(|p| p.exists())
        || codeium_root.as_ref().is_some_and(|p| p.exists());
    if !detected {
        return skipped_outcome("Windsurf hooks", "~/.codeium/ not found");
    }
    let context_path = match cwd_path(&[".windsurf", "rules", "difflore-context.md"]) {
        Ok(p) => p,
        Err(e) => return error_outcome("Windsurf hooks", e),
    };
    let res = merge_windsurf_hooks(&hooks_path, &context_path, bin, dry_run);
    finalize_hook_outcome("Windsurf hooks", &hooks_path, res)
}

pub(super) const WINDSURF_HOOK_EVENTS: &[&str] = &[
    "pre_user_prompt",
    "post_write_code",
    "post_run_command",
    "post_mcp_tool_use",
    "post_cascade_response",
];

pub(super) fn merge_windsurf_hooks(
    hooks_path: &Path,
    context_path: &Path,
    bin: &str,
    dry_run: bool,
) -> anyhow::Result<bool> {
    let mut cfg = load_json_object_anyhow(hooks_path)?;
    let hooks = cfg
        .entry("hooks".to_owned())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    let hooks_obj = hooks
        .as_object_mut()
        .ok_or_else(|| anyhow!("{}: `hooks` is not a JSON object", hooks_path.display()))?;

    let command = hook_command_string(bin, "windsurf");
    let mut existed = false;
    for event in WINDSURF_HOOK_EVENTS {
        let entries = hooks_obj
            .entry((*event).to_owned())
            .or_insert_with(|| Value::Array(Vec::new()));
        let arr = entries
            .as_array_mut()
            .ok_or_else(|| anyhow!("{}: hooks.{event} is not an array", hooks_path.display()))?;
        let before = arr.len();
        arr.retain(|v| {
            v.get("command")
                .and_then(|c| c.as_str())
                .is_none_or(|c| !hook_command_matches_client(c, "windsurf"))
        });
        if arr.len() != before {
            existed = true;
        }
        arr.push(json!({
            "command": command,
            "show_output": false,
        }));
    }

    if !dry_run {
        write_json_object_anyhow(hooks_path, &cfg)?;
        write_windsurf_context_file(context_path)?;
    }
    Ok(existed)
}

pub(super) fn write_windsurf_context_file(path: &Path) -> anyhow::Result<()> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let placeholder = "# DiffLore context\n\nRules pulled from DiffLore will be surfaced here after your first session.\n";
    super::common::write_atomic(path, placeholder.as_bytes())
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

pub(super) fn uninstall_windsurf_hooks(dry_run: bool) -> TargetOutcome {
    let hooks_path = match home_path(&[".codeium", "windsurf", "hooks.json"]) {
        Ok(p) => p,
        Err(e) => return error_outcome("Windsurf hooks", e),
    };
    finalize_uninstall_outcome(
        "Windsurf hooks",
        &hooks_path,
        remove_windsurf_hooks(&hooks_path, dry_run),
    )
}

pub(super) fn remove_windsurf_hooks(hooks_path: &Path, dry_run: bool) -> anyhow::Result<bool> {
    if !hooks_path.exists() {
        return Ok(false);
    }
    let mut cfg = load_json_object_anyhow(hooks_path)?;
    let Some(hooks) = cfg.get_mut("hooks") else {
        return Ok(false);
    };
    let Some(hooks_obj) = hooks.as_object_mut() else {
        return Ok(false);
    };
    let mut removed = false;
    let mut empty_events: Vec<String> = Vec::new();
    for (event, entries) in hooks_obj.iter_mut() {
        let Some(arr) = entries.as_array_mut() else {
            continue;
        };
        let before = arr.len();
        arr.retain(|v| {
            v.get("command")
                .and_then(|c| c.as_str())
                .is_none_or(|c| !hook_command_matches_client(c, "windsurf"))
        });
        if arr.len() != before {
            removed = true;
        }
        if arr.is_empty() {
            empty_events.push(event.clone());
        }
    }
    for event in empty_events {
        hooks_obj.remove(&event);
    }
    // The `.windsurf/rules/difflore-context.md` file is left untouched: like
    // install, we never overwrite it since it may hold user content.
    if removed && !dry_run {
        write_json_object_anyhow(hooks_path, &cfg)?;
    }
    Ok(removed)
}

// ── Uninstall outcome helper ─────────────────────────────────────────────

fn finalize_uninstall_outcome(
    name: &'static str,
    path: &Path,
    res: anyhow::Result<bool>,
) -> TargetOutcome {
    match res {
        Ok(true) => TargetOutcome {
            name,
            status: Status::Removed,
            detail: path.display().to_string(),
        },
        Ok(false) => TargetOutcome {
            name,
            status: Status::Skipped("no difflore hooks to remove".into()),
            detail: String::new(),
        },
        Err(e) => TargetOutcome {
            name,
            status: Status::Error(e.to_string()),
            detail: String::new(),
        },
    }
}

// ── Probes for hook-based installs ──────────────────────────────────────

#[allow(clippy::enum_variant_names)] // reason: variants name traversal styles; the `By` prefix reads naturally at call sites.
pub(super) enum JsonHookProber<'a> {
    ByName(&'a str),
    ByGroup(&'a str),
    ByCommand(&'a str),
    ByNestedCommand(&'a str),
}

impl JsonHookProber<'_> {
    fn matches(&self, hooks: &serde_json::Map<String, Value>) -> bool {
        hooks.values().any(|v| {
            v.as_array().is_some_and(|arr| match self {
                Self::ByName(target) => arr
                    .iter()
                    .any(|h| h.get("name").and_then(|n| n.as_str()) == Some(*target)),
                Self::ByGroup(target) => arr.iter().any(|g| {
                    g.get("hooks")
                        .and_then(|h| h.as_array())
                        .is_some_and(|inner| {
                            inner
                                .iter()
                                .any(|h| h.get("name").and_then(|n| n.as_str()) == Some(*target))
                        })
                }),
                Self::ByCommand(client) => arr.iter().any(|h| {
                    h.get("command")
                        .and_then(|c| c.as_str())
                        .is_some_and(|c| hook_command_matches_client(c, client))
                }),
                Self::ByNestedCommand(client) => arr.iter().any(|group| {
                    group
                        .get("hooks")
                        .and_then(|h| h.as_array())
                        .is_some_and(|inner| {
                            inner.iter().any(|h| {
                                h.get("command")
                                    .and_then(|c| c.as_str())
                                    .is_some_and(|c| hook_command_matches_client(c, client))
                            })
                        })
                }),
            })
        })
    }
}

fn probe_json_hooks(name: &'static str, path: &Path, prober: &JsonHookProber<'_>) -> TargetStatus {
    if !path.exists() {
        return TargetStatus {
            name,
            detected: false,
            state: InstallState::NotInstalled,
            detail: Some(format!("{} not found", path.display())),
        };
    }
    let obj = match load_json_object(&path.to_path_buf()) {
        Ok(obj) => obj,
        Err(e) => {
            return TargetStatus {
                name,
                detected: true,
                state: InstallState::Conflict,
                detail: Some(e),
            };
        }
    };
    let Some(hooks) = obj.get("hooks").and_then(|v| v.as_object()) else {
        return TargetStatus {
            name,
            detected: true,
            state: InstallState::NotInstalled,
            detail: Some(format!("{} has no hooks object", path.display())),
        };
    };
    let found = prober.matches(hooks);
    TargetStatus {
        name,
        detected: true,
        state: if found {
            InstallState::Installed
        } else {
            InstallState::NotInstalled
        },
        detail: Some(path.display().to_string()),
    }
}

pub(super) fn probe_json_hooks_by_name(name: &'static str, path: &Path) -> TargetStatus {
    probe_json_hooks(name, path, &JsonHookProber::ByName("difflore"))
}

pub(super) fn probe_json_hooks_by_group(name: &'static str, path: &Path) -> TargetStatus {
    probe_json_hooks(name, path, &JsonHookProber::ByGroup("difflore"))
}

pub(super) fn probe_json_hooks_by_command(
    name: &'static str,
    path: &Path,
    client: &str,
) -> TargetStatus {
    probe_json_hooks(name, path, &JsonHookProber::ByCommand(client))
}

pub(super) fn probe_json_hooks_by_nested_command(
    name: &'static str,
    path: &Path,
    client: &str,
) -> TargetStatus {
    probe_json_hooks(name, path, &JsonHookProber::ByNestedCommand(client))
}

// ── Rendered-block extraction ─────────────────────────────────────────────
//
// `agents update` hashes the exact difflore block we author so it can tell
// "unchanged since DiffLore wrote it" (safe to upgrade) from "human edited"
// (must not clobber). These render fns reproduce, per client, the same `json!`
// group objects the `merge_*` fns push, in event order, so the hash covers only
// our render. They MUST stay in lockstep with the matching `merge_*` writer;
// guard-rail tests in `manifest.rs` install, re-extract, and assert byte-equality.

/// The Claude Code hook groups DiffLore contributes, in event order. Built
/// from the same [`CLAUDE_HOOK_EVENT_MATCHERS`] table and
/// [`claude_hook_group`] builder as [`merge_claude_code_hooks`], so the hash
/// always covers exactly the bytes the merge writes.
pub(super) fn render_claude_code_hook_block(bin: &str) -> Vec<Value> {
    let command = hook_command_string(bin, "claude-code");
    CLAUDE_HOOK_EVENT_MATCHERS
        .iter()
        .map(|(event, matcher)| claude_hook_group(event, *matcher, &command))
        .collect()
}

/// Every historical Claude Code hook block shape, oldest first, so `agents
/// update` can recognise a pristine old install by its bytes and upgrade it
/// instead of skipping it as "locally edited":
///
/// 1. initial release: PreToolUse(Read) registered, PostToolUse matcher
///    without `Bash`;
/// 2. interim: PostToolUse gained `Bash` while PreToolUse(Read) was still
///    registered (the render fn missed that matcher change, so manifests from
///    that window recorded hashes that never matched any bytes on disk).
///
/// Append a new entry whenever the rendered shape changes (and bump
/// `HOOKS_JSON_BLOCK_VERSION`); never edit existing entries — they must keep
/// reproducing the bytes old installers wrote.
pub(super) fn legacy_claude_code_hook_blocks(bin: &str) -> Vec<Vec<Value>> {
    let command = hook_command_string(bin, "claude-code");
    let legacy_tables: &[&[(&str, Option<&str>)]] = &[
        &[
            ("PreToolUse", Some("Read")),
            ("PostToolUse", Some("Edit|MultiEdit|Write")),
            ("SessionStart", Some("startup|clear|compact")),
            ("UserPromptSubmit", None),
            ("Stop", None),
            ("SessionEnd", None),
        ],
        &[
            ("PreToolUse", Some("Read")),
            ("PostToolUse", Some("Edit|MultiEdit|Write|Bash")),
            ("SessionStart", Some("startup|clear|compact")),
            ("UserPromptSubmit", None),
            ("Stop", None),
            ("SessionEnd", None),
        ],
    ];
    legacy_tables
        .iter()
        .map(|table| {
            table
                .iter()
                .map(|(event, matcher)| claude_hook_group(event, *matcher, &command))
                .collect()
        })
        .collect()
}

/// The Cursor hook entries DiffLore contributes, one per [`CURSOR_HOOK_EVENTS`]
/// event. Must mirror the `json!` pushed in [`merge_cursor_hooks`].
pub(super) fn render_cursor_hook_block(bin: &str) -> Vec<Value> {
    let command = hook_command_string(bin, "cursor");
    CURSOR_HOOK_EVENTS
        .iter()
        .map(|_event| {
            json!({
                "name": "difflore",
                "command": command,
                "timeout": 5000,
            })
        })
        .collect()
}

/// The Gemini CLI hook groups DiffLore contributes, one per
/// [`GEMINI_HOOK_EVENTS`] event. Must mirror the `json!` pushed in
/// [`merge_gemini_cli_hooks`].
pub(super) fn render_gemini_cli_hook_block(bin: &str) -> Vec<Value> {
    let command = hook_command_string(bin, "gemini-cli");
    let hook_entry = json!({
        "name": "difflore",
        "type": "command",
        "command": command,
        "timeout": 5000,
    });
    GEMINI_HOOK_EVENTS
        .iter()
        .map(|_event| {
            json!({
                "matcher": "*",
                "hooks": [hook_entry.clone()],
            })
        })
        .collect()
}

/// The Windsurf hook entries DiffLore contributes, one per
/// [`WINDSURF_HOOK_EVENTS`] event. Must mirror the `json!` pushed in
/// [`merge_windsurf_hooks`].
pub(super) fn render_windsurf_hook_block(bin: &str) -> Vec<Value> {
    let command = hook_command_string(bin, "windsurf");
    WINDSURF_HOOK_EVENTS
        .iter()
        .map(|_event| {
            json!({
                "command": command,
                "show_output": false,
            })
        })
        .collect()
}

/// Extract the difflore hook groups on disk for `client` from a hooks JSON file,
/// in event-iteration order, exactly as stored (so the hash sees the persisted
/// form). Used by `agents update` to re-hash and compare against the manifest.
/// A missing file / object yields an empty vec.
///
/// `client` selects the same matcher each `merge_*` / probe uses:
/// - "claude-code" / "windsurf": nested/flat `command` containing the client
///   shim marker;
/// - "cursor": flat entry whose `name == "difflore"`;
/// - "gemini-cli": group whose nested `hooks[].name == "difflore"`.
pub(super) fn extract_hook_groups_on_disk(path: &Path, client: &str) -> Vec<Value> {
    let Ok(obj) = load_json_object(&path.to_path_buf()) else {
        return Vec::new();
    };
    let Some(hooks) = obj.get("hooks").and_then(|v| v.as_object()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entries in hooks.values() {
        let Some(arr) = entries.as_array() else {
            continue;
        };
        for v in arr {
            if hook_value_is_difflore(v, client) {
                out.push(v.clone());
            }
        }
    }
    out
}

/// Does a single hooks-array element belong to DiffLore for `client`? Reuses the
/// same predicates as the `merge_*`/`remove_*` `retain(...)` closures so extract
/// and write agree on what "our block" is.
fn hook_value_is_difflore(v: &Value, client: &str) -> bool {
    match client {
        "cursor" => v.get("name").and_then(|n| n.as_str()) == Some("difflore"),
        "gemini-cli" => v
            .get("hooks")
            .and_then(|h| h.as_array())
            .is_some_and(|inner| {
                inner
                    .iter()
                    .any(|h| h.get("name").and_then(|n| n.as_str()) == Some("difflore"))
            }),
        "windsurf" => v
            .get("command")
            .and_then(|c| c.as_str())
            .is_some_and(|c| hook_command_matches_client(c, "windsurf")),
        // claude-code: nested group → hooks[].command.
        _ => v
            .get("hooks")
            .and_then(|h| h.as_array())
            .is_some_and(|inner| {
                inner.iter().any(|h| {
                    h.get("command")
                        .and_then(|c| c.as_str())
                        .is_some_and(|c| hook_command_matches_client(c, client))
                })
            }),
    }
}

// ── Hook command formatting ─────────────────────────────────────────────

pub(super) fn hook_command_string(bin: &str, client: &str) -> String {
    let shim = hook_shim_path(bin);
    let normalised = shim.replace('\\', "/");
    format!("\"{normalised}\" --client {client}")
}

fn hook_shim_path(bin: &str) -> String {
    let shim_name = format!("difflore-hook{}", std::env::consts::EXE_SUFFIX);
    match bin.rfind(['/', '\\']) {
        Some(idx) => format!("{}{}", &bin[..=idx], shim_name),
        None => shim_name,
    }
}

pub(super) fn hook_command_matches_client(command: &str, client: &str) -> bool {
    let new_marker = "difflore-hook".to_owned();
    let client_marker = format!("--client {client}");
    command.contains(&new_marker) && command.contains(&client_marker)
}

#[cfg(test)]
mod tests {
    use super::super::test_util::tmp_settings_path;
    use super::*;

    const BIN: &str = "/tmp/fake/difflore";

    #[test]
    fn hook_command_string_normalizes_backslashes_to_forward_slashes() {
        let cmd = hook_command_string(r"C:\Users\me\difflore.exe", "claude-code");
        assert!(
            !cmd.contains('\\'),
            "command should not contain backslashes, got: {cmd}"
        );
        assert!(
            cmd.contains("C:/Users/me/"),
            "expected forward-slash path, got: {cmd}"
        );
    }

    fn read_json(path: &Path) -> Value {
        let s = fs::read_to_string(path).expect("read config");
        serde_json::from_str(&s).expect("parse config")
    }

    #[test]
    fn claude_hooks_first_install_writes_all_events_to_settings() {
        let (tmp, _) = tmp_settings_path();
        let path = tmp.path().join(".claude/settings.json");
        let added = merge_claude_code_hooks(&path, BIN).expect("merge");
        assert_eq!(added, 5, "first install must add 5 event matchers");
        let v = read_json(&path);
        for event in [
            "PostToolUse",
            "SessionStart",
            "UserPromptSubmit",
            "Stop",
            "SessionEnd",
        ] {
            let groups = v["hooks"][event]
                .as_array()
                .unwrap_or_else(|| panic!("hooks.{event} array missing"));
            assert_eq!(groups.len(), 1, "expected one group for {event}");
            let cmd = groups[0]["hooks"][0]["command"].as_str().expect("cmd");
            assert!(
                cmd.contains("--client claude-code"),
                "claude-code adapter must be invoked, got: {cmd}"
            );
            assert!(cmd.contains("difflore-hook"), "hook shim missing: {cmd}");
        }
        // PreToolUse(Read) was retired to a dispatcher noop; a fresh install
        // must not register it (it would spawn the hook on every Read for
        // zero value).
        assert!(
            v["hooks"].get("PreToolUse").is_none(),
            "retired PreToolUse(Read) must not be registered: {v}"
        );
        assert_eq!(
            v["hooks"]["PostToolUse"][0]["matcher"],
            "Edit|MultiEdit|Write|Bash"
        );
        assert_eq!(
            v["hooks"]["PostToolUse"][0]["hooks"][0]["timeout"], 5000,
            "PostToolUse keeps its 5s budget"
        );
        assert!(
            v["hooks"]["UserPromptSubmit"][0].get("matcher").is_none(),
            "UserPromptSubmit fires on every prompt — no matcher needed"
        );
    }

    #[test]
    fn claude_hooks_reinstall_replaces_difflore_preserving_user_hooks() {
        let (_tmp, path) = tmp_settings_path();
        fs::write(
            &path,
            r#"{
                "permissions": { "allow": ["Bash(**)"] },
                "hooks": {
                    "PreToolUse": [
                        {
                            "matcher": "Read",
                            "hooks": [{"type": "command", "command": "user-tool --pre-read"}]
                        },
                        {
                            "matcher": "Read",
                            "hooks": [{"type": "command", "command": "/old/bin/difflore-hook --client claude-code"}]
                        }
                    ]
                }
            }"#,
        )
        .expect("seed");

        let added = merge_claude_code_hooks(&path, BIN).expect("merge");
        assert!(added >= 5, "reinstall touches every event matcher");
        let v = read_json(&path);
        // The user's own PreToolUse hook survives untouched...
        let groups = v["hooks"]["PreToolUse"].as_array().expect("groups");
        let user_kept = groups.iter().any(|g| {
            g["hooks"][0]["command"]
                .as_str()
                .is_some_and(|c| c.contains("user-tool"))
        });
        assert!(user_kept, "user hook must survive a difflore reinstall");
        // ...while the stale difflore PreToolUse(Read) registration from the
        // old install is removed for good: the dispatcher noops pre-read, so
        // re-registering would spawn the hook on every Read for zero value.
        let difflore_groups: Vec<_> = groups
            .iter()
            .filter(|g| {
                g["hooks"][0]["command"]
                    .as_str()
                    .is_some_and(|c| hook_command_matches_client(c, "claude-code"))
            })
            .collect();
        assert!(
            difflore_groups.is_empty(),
            "stale difflore PreToolUse(Read) must be stripped on upgrade: {difflore_groups:?}"
        );
        // The live events are (re)installed with the fresh binary path.
        let post_cmd = v["hooks"]["PostToolUse"][0]["hooks"][0]["command"]
            .as_str()
            .expect("cmd");
        assert!(
            post_cmd.contains("difflore-hook"),
            "hook shim was not installed: {post_cmd}"
        );
        assert!(
            !post_cmd.contains("/old/bin/difflore"),
            "stale entry leaked through: {post_cmd}"
        );
        assert_eq!(v["permissions"]["allow"][0], "Bash(**)");
    }

    #[test]
    fn claude_hooks_upgrade_replaces_old_matcher_block_without_duplicating() {
        // An install written before the PostToolUse matcher gained `Bash`:
        // the merge must recognise the old-matcher group as ours (recognition
        // is by hook command, not matcher), replace it in place, and never
        // append a second difflore group next to it.
        let (_tmp, path) = tmp_settings_path();
        fs::write(
            &path,
            r#"{
                "hooks": {
                    "PostToolUse": [
                        {
                            "matcher": "Edit|MultiEdit|Write",
                            "hooks": [{"type": "command", "command": "/old/bin/difflore-hook --client claude-code", "timeout": 5000}]
                        }
                    ]
                }
            }"#,
        )
        .expect("seed");

        merge_claude_code_hooks(&path, BIN).expect("merge");
        let v = read_json(&path);
        let groups = v["hooks"]["PostToolUse"].as_array().expect("groups");
        let difflore_groups: Vec<_> = groups
            .iter()
            .filter(|g| {
                g["hooks"][0]["command"]
                    .as_str()
                    .is_some_and(|c| hook_command_matches_client(c, "claude-code"))
            })
            .collect();
        assert_eq!(
            difflore_groups.len(),
            1,
            "old-matcher block must be replaced, not duplicated: {difflore_groups:?}"
        );
        assert_eq!(
            difflore_groups[0]["matcher"], CLAUDE_POST_TOOL_USE_MATCHER,
            "replaced block must carry the unified matcher"
        );
    }

    #[test]
    fn claude_hooks_upgrade_drops_emptied_retired_event_entirely() {
        // An old install whose PreToolUse array holds ONLY the difflore
        // group: the upgrade must remove the group AND the now-empty event
        // key, not leave a dangling `"PreToolUse": []`.
        let (_tmp, path) = tmp_settings_path();
        fs::write(
            &path,
            r#"{
                "hooks": {
                    "PreToolUse": [
                        {
                            "matcher": "Read",
                            "hooks": [{"type": "command", "command": "/old/bin/difflore-hook --client claude-code"}]
                        }
                    ]
                }
            }"#,
        )
        .expect("seed");

        merge_claude_code_hooks(&path, BIN).expect("merge");
        let v = read_json(&path);
        assert!(
            v["hooks"].get("PreToolUse").is_none(),
            "emptied retired event must be dropped: {v}"
        );
        assert!(
            v["hooks"]["PostToolUse"].as_array().is_some(),
            "live events must still be installed"
        );
    }

    #[test]
    fn claude_hooks_returns_error_when_hooks_field_is_not_an_object() {
        let (_tmp, path) = tmp_settings_path();
        fs::write(&path, r#"{"hooks": []}"#).expect("seed");
        let err = merge_claude_code_hooks(&path, BIN).expect_err("must fail");
        assert!(
            err.to_string().contains("`hooks` is not a JSON object"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn cursor_hooks_install_creates_all_events_with_difflore_entry() {
        let (tmp, _) = tmp_settings_path();
        let path = tmp.path().join(".cursor/hooks.json");
        let existed = merge_cursor_hooks(&path, BIN, false).expect("merge");
        assert!(!existed, "first install must report new entry");
        let v = read_json(&path);
        assert_eq!(v["version"], 1);
        for event in CURSOR_HOOK_EVENTS {
            let arr = v["hooks"][event].as_array().expect("event array");
            assert_eq!(arr.len(), 1, "expected one entry for {event}");
            assert_eq!(arr[0]["name"], "difflore");
            assert_eq!(arr[0]["timeout"], 5000);
            let cmd = arr[0]["command"].as_str().expect("cmd");
            assert!(cmd.contains("--client cursor"), "got: {cmd}");
            assert!(cmd.contains("difflore-hook"), "hook shim missing: {cmd}");
        }
    }

    #[test]
    fn cursor_hooks_reinstall_replaces_existing_difflore_but_preserves_other_hooks() {
        let (_tmp, path) = tmp_settings_path();
        fs::write(
            &path,
            r#"{
                "version": 1,
                "hooks": {
                    "afterFileEdit": [{"name": "other-tool", "command": "xyz"}]
                }
            }"#,
        )
        .expect("seed");
        let existed = merge_cursor_hooks(&path, BIN, false).expect("merge");
        assert!(!existed);
        let existed2 = merge_cursor_hooks(&path, BIN, false).expect("merge");
        assert!(existed2, "second install must detect the prior difflore");

        let v = read_json(&path);
        let arr = v["hooks"]["afterFileEdit"].as_array().expect("arr");
        assert!(
            arr.iter()
                .any(|h| h["name"] == "other-tool" && h["command"] == "xyz"),
            "other-tool hook was clobbered: {arr:?}"
        );
        let difflore_count = arr.iter().filter(|h| h["name"] == "difflore").count();
        assert_eq!(difflore_count, 1, "expected exactly one difflore entry");
    }

    #[test]
    fn gemini_hooks_install_writes_matcher_wrapped_groups() {
        let (tmp, settings) = tmp_settings_path();
        let md = tmp.path().join("GEMINI.md");
        let existed = merge_gemini_cli_hooks(&settings, &md, BIN, false).expect("merge");
        assert!(!existed);
        let v = read_json(&settings);
        for event in GEMINI_HOOK_EVENTS {
            let groups = v["hooks"][event].as_array().expect("groups");
            assert_eq!(groups.len(), 1);
            assert_eq!(groups[0]["matcher"], "*");
            let inner = groups[0]["hooks"].as_array().expect("inner");
            assert_eq!(inner[0]["name"], "difflore");
            assert_eq!(inner[0]["type"], "command");
            assert_eq!(inner[0]["timeout"], 5000);
            let cmd = inner[0]["command"].as_str().expect("cmd");
            assert!(cmd.contains("--client gemini-cli"), "got: {cmd}");
        }
        let md_text = fs::read_to_string(&md).expect("read");
        assert!(md_text.contains("<difflore-context>"));
        assert!(md_text.contains("</difflore-context>"));
    }

    #[test]
    fn gemini_hooks_reinstall_replaces_difflore_group_preserving_others() {
        let (tmp, settings) = tmp_settings_path();
        let md = tmp.path().join("GEMINI.md");
        fs::write(
            &settings,
            r#"{
                "theme": "dark",
                "hooks": {
                    "AfterTool": [
                        {"matcher": "*", "hooks": [{"name":"other","type":"command","command":"x","timeout":1}]}
                    ]
                }
            }"#,
        )
        .expect("seed");
        merge_gemini_cli_hooks(&settings, &md, BIN, false).expect("merge");
        let existed2 = merge_gemini_cli_hooks(&settings, &md, BIN, false).expect("merge");
        assert!(existed2);
        let v = read_json(&settings);
        assert_eq!(v["theme"], "dark");
        let groups = v["hooks"]["AfterTool"].as_array().expect("groups");
        assert_eq!(groups.len(), 2);
        let names: Vec<&str> = groups
            .iter()
            .filter_map(|g| g["hooks"][0]["name"].as_str())
            .collect();
        assert!(names.contains(&"other"));
        assert!(names.contains(&"difflore"));
    }

    #[test]
    fn gemini_hooks_md_append_preserves_prior_content() {
        let (tmp, _) = tmp_settings_path();
        let md = tmp.path().join("GEMINI.md");
        fs::write(&md, "# My rules\n\nBe concise.\n").expect("seed");
        upsert_gemini_md_context(&md).expect("upsert");
        let text = fs::read_to_string(&md).expect("read");
        assert!(text.contains("# My rules"), "prior content lost: {text:?}");
        assert!(text.contains("<difflore-context>"));
    }

    #[test]
    fn gemini_hooks_md_leaves_existing_tag_alone() {
        let (tmp, _) = tmp_settings_path();
        let md = tmp.path().join("GEMINI.md");
        let seed = "<difflore-context>\nReal team rules here\n</difflore-context>\n";
        fs::write(&md, seed).expect("seed");
        upsert_gemini_md_context(&md).expect("upsert");
        let text = fs::read_to_string(&md).expect("read");
        assert_eq!(text, seed, "tag body must not be rewritten");
    }

    #[test]
    fn windsurf_hooks_install_writes_all_events_and_context_file() {
        let (tmp, _) = tmp_settings_path();
        let hooks = tmp.path().join("hooks.json");
        let ctx = tmp.path().join(".windsurf/rules/difflore-context.md");
        let existed = merge_windsurf_hooks(&hooks, &ctx, BIN, false).expect("merge");
        assert!(!existed);
        let v = read_json(&hooks);
        for event in WINDSURF_HOOK_EVENTS {
            let arr = v["hooks"][event].as_array().expect("event array");
            assert_eq!(arr.len(), 1);
            let cmd = arr[0]["command"].as_str().expect("cmd");
            assert!(cmd.contains("--client windsurf"), "got: {cmd}");
            assert_eq!(arr[0]["show_output"], false);
        }
        assert!(ctx.exists());
        let text = fs::read_to_string(&ctx).expect("read");
        assert!(text.contains("DiffLore"));
    }

    #[test]
    fn windsurf_hooks_reinstall_replaces_stale_difflore_entry() {
        let (tmp, _) = tmp_settings_path();
        let hooks = tmp.path().join("hooks.json");
        let ctx = tmp.path().join(".windsurf/rules/difflore-context.md");
        fs::create_dir_all(hooks.parent().expect("parent")).expect("mkdir");
        fs::write(
            &hooks,
            r#"{
                "hooks": {
                    "post_write_code": [
                        {"command": "/old/difflore-hook --client windsurf", "show_output": false},
                        {"command": "/other/tool --do-stuff", "show_output": true}
                    ]
                }
            }"#,
        )
        .expect("seed");
        let existed = merge_windsurf_hooks(&hooks, &ctx, BIN, false).expect("merge");
        assert!(existed, "must detect the pre-existing difflore entry");

        let v = read_json(&hooks);
        let arr = v["hooks"]["post_write_code"].as_array().expect("arr");
        let difflore_entries: Vec<&str> = arr
            .iter()
            .filter_map(|h| h["command"].as_str())
            .filter(|c| c.contains("--client windsurf"))
            .collect();
        assert_eq!(difflore_entries.len(), 1, "got: {difflore_entries:?}");
        assert!(difflore_entries[0].contains("difflore-hook"));
        assert!(
            arr.iter().any(|h| h["command"] == "/other/tool --do-stuff"),
            "other tool clobbered: {arr:?}"
        );
    }

    #[test]
    fn windsurf_hooks_context_placeholder_not_overwritten_on_reinstall() {
        let (tmp, _) = tmp_settings_path();
        let hooks = tmp.path().join("hooks.json");
        let ctx = tmp.path().join(".windsurf/rules/difflore-context.md");
        fs::create_dir_all(ctx.parent().expect("parent")).expect("mkdir");
        fs::write(&ctx, "# User custom context\n").expect("seed");
        merge_windsurf_hooks(&hooks, &ctx, BIN, false).expect("merge");
        let text = fs::read_to_string(&ctx).expect("read");
        assert_eq!(text, "# User custom context\n");
    }

    // ── Hook uninstall round-trips (inverse of the merges) ──────────────

    #[test]
    fn claude_hooks_install_then_uninstall_leaves_no_difflore_groups() {
        let (tmp, _) = tmp_settings_path();
        let path = tmp.path().join(".claude/settings.json");
        merge_claude_code_hooks(&path, BIN).expect("merge");
        let removed = remove_claude_code_hooks(&path, false).expect("remove");
        assert_eq!(removed, 5, "uninstall should clear all 5 event matchers");
        let v = read_json(&path);
        // hooks object had only difflore groups → it should be dropped entirely.
        assert!(
            v.get("hooks").is_none(),
            "empty hooks object should be removed: {v}"
        );
    }

    #[test]
    fn claude_hooks_uninstall_preserves_user_hooks() {
        let (_tmp, path) = tmp_settings_path();
        fs::write(
            &path,
            r#"{
                "permissions": { "allow": ["Bash(**)"] },
                "hooks": {
                    "PreToolUse": [
                        {
                            "matcher": "Read",
                            "hooks": [{"type": "command", "command": "user-tool --pre-read"}]
                        }
                    ]
                }
            }"#,
        )
        .expect("seed");
        merge_claude_code_hooks(&path, BIN).expect("merge");
        let removed = remove_claude_code_hooks(&path, false).expect("remove");
        assert!(removed >= 1, "difflore groups were removed");
        let v = read_json(&path);
        assert_eq!(v["permissions"]["allow"][0], "Bash(**)", "perms clobbered");
        let groups = v["hooks"]["PreToolUse"]
            .as_array()
            .expect("PreToolUse kept");
        let user_kept = groups.iter().any(|g| {
            g["hooks"][0]["command"]
                .as_str()
                .is_some_and(|c| c.contains("user-tool"))
        });
        assert!(user_kept, "user hook must survive uninstall: {groups:?}");
        let difflore_left = groups.iter().any(|g| {
            g["hooks"][0]["command"]
                .as_str()
                .is_some_and(|c| hook_command_matches_client(c, "claude-code"))
        });
        assert!(!difflore_left, "difflore group leaked: {groups:?}");
    }

    #[test]
    fn cursor_hooks_install_then_uninstall_round_trips_to_clean() {
        let (tmp, _) = tmp_settings_path();
        let path = tmp.path().join(".cursor/hooks.json");
        merge_cursor_hooks(&path, BIN, false).expect("merge");
        let removed = remove_cursor_hooks(&path, false).expect("remove");
        assert!(removed, "uninstall removed the difflore entries");
        let v = read_json(&path);
        assert!(
            v.get("hooks").is_none()
                || v["hooks"]
                    .as_object()
                    .is_some_and(serde_json::Map::is_empty),
            "no difflore hooks should remain: {v}"
        );
        // `version` key is left intact (it isn't ours to remove).
        assert_eq!(v["version"], 1);
    }

    #[test]
    fn cursor_hooks_uninstall_preserves_other_tools() {
        let (_tmp, path) = tmp_settings_path();
        fs::write(
            &path,
            r#"{ "version": 1, "hooks": { "afterFileEdit": [{"name": "other-tool", "command": "xyz"}] } }"#,
        )
        .expect("seed");
        merge_cursor_hooks(&path, BIN, false).expect("merge");
        let removed = remove_cursor_hooks(&path, false).expect("remove");
        assert!(removed);
        let v = read_json(&path);
        let arr = v["hooks"]["afterFileEdit"].as_array().expect("arr kept");
        assert!(
            arr.iter().any(|h| h["name"] == "other-tool"),
            "other-tool clobbered: {arr:?}"
        );
        assert!(
            !arr.iter().any(|h| h["name"] == "difflore"),
            "difflore leaked: {arr:?}"
        );
    }

    #[test]
    fn gemini_hooks_install_then_uninstall_preserves_other_groups() {
        let (tmp, settings) = tmp_settings_path();
        let md = tmp.path().join("GEMINI.md");
        fs::write(
            &settings,
            r#"{ "theme": "dark", "hooks": { "AfterTool": [
                {"matcher": "*", "hooks": [{"name":"other","type":"command","command":"x","timeout":1}]}
            ] } }"#,
        )
        .expect("seed");
        merge_gemini_cli_hooks(&settings, &md, BIN, false).expect("merge");
        let removed = remove_gemini_cli_hooks(&settings, false).expect("remove");
        assert!(removed);
        let v = read_json(&settings);
        assert_eq!(v["theme"], "dark");
        let groups = v["hooks"]["AfterTool"].as_array().expect("AfterTool kept");
        let names: Vec<&str> = groups
            .iter()
            .filter_map(|g| g["hooks"][0]["name"].as_str())
            .collect();
        assert!(names.contains(&"other"), "other group lost: {names:?}");
        assert!(!names.contains(&"difflore"), "difflore leaked: {names:?}");
    }

    #[test]
    fn windsurf_hooks_install_then_uninstall_preserves_other_tools() {
        let (tmp, _) = tmp_settings_path();
        let hooks = tmp.path().join("hooks.json");
        let ctx = tmp.path().join(".windsurf/rules/difflore-context.md");
        fs::create_dir_all(hooks.parent().expect("parent")).expect("mkdir");
        fs::write(
            &hooks,
            r#"{ "hooks": { "post_write_code": [
                {"command": "/other/tool --do-stuff", "show_output": true}
            ] } }"#,
        )
        .expect("seed");
        merge_windsurf_hooks(&hooks, &ctx, BIN, false).expect("merge");
        let removed = remove_windsurf_hooks(&hooks, false).expect("remove");
        assert!(removed);
        let v = read_json(&hooks);
        let arr = v["hooks"]["post_write_code"].as_array().expect("arr kept");
        assert!(
            arr.iter().any(|h| h["command"] == "/other/tool --do-stuff"),
            "other tool clobbered: {arr:?}"
        );
        assert!(
            !arr.iter()
                .filter_map(|h| h["command"].as_str())
                .any(|c| hook_command_matches_client(c, "windsurf")),
            "difflore windsurf hook leaked: {arr:?}"
        );
    }

    #[test]
    fn hook_uninstall_dry_run_does_not_write() {
        let (tmp, _) = tmp_settings_path();
        let path = tmp.path().join(".cursor/hooks.json");
        merge_cursor_hooks(&path, BIN, false).expect("merge");
        let before = fs::read_to_string(&path).expect("read");
        let removed = remove_cursor_hooks(&path, true).expect("dry-run remove");
        assert!(removed, "dry-run reports it would remove");
        assert_eq!(
            fs::read_to_string(&path).expect("read"),
            before,
            "dry-run wrote"
        );
    }

    #[test]
    fn hook_uninstall_missing_file_is_noop() {
        let (tmp, _) = tmp_settings_path();
        let path = tmp.path().join("absent.json");
        assert_eq!(remove_claude_code_hooks(&path, false).expect("noop"), 0);
        assert!(!remove_cursor_hooks(&path, false).expect("noop"));
        assert!(!remove_gemini_cli_hooks(&path, false).expect("noop"));
        assert!(!remove_windsurf_hooks(&path, false).expect("noop"));
    }
}
