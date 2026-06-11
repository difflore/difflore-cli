//! Distribution and marketplace manifest verification.
//!
//! A guardrail against release drift: checks that the repo's plugin manifests
//! agree with the CLI package version and that the plugin bundle still contains
//! the runtime files the marketplaces expect.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

use crate::support::util::exit_code;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DistSeverity {
    Error,
    Warning,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DistIssue {
    pub severity: DistSeverity,
    pub path: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DistCheckReport {
    pub repo_root: String,
    pub expected_version: Option<String>,
    pub issues: Vec<DistIssue>,
}

impl DistCheckReport {
    pub(crate) fn ok(&self) -> bool {
        self.issues
            .iter()
            .all(|issue| issue.severity != DistSeverity::Error)
    }

    pub(crate) fn error_count(&self) -> usize {
        self.issues
            .iter()
            .filter(|issue| issue.severity == DistSeverity::Error)
            .count()
    }

    pub(crate) fn warning_count(&self) -> usize {
        self.issues
            .iter()
            .filter(|issue| issue.severity == DistSeverity::Warning)
            .count()
    }
}

pub fn find_repo_root_from(start: &Path) -> Option<PathBuf> {
    let mut cur = start.to_path_buf();
    loop {
        if cur.join("Cargo.toml").exists()
            && cur.join("crates").is_dir()
            && cur.join("plugin").is_dir()
        {
            return Some(cur);
        }
        if !cur.pop() {
            return None;
        }
    }
}

pub fn verify_from_cwd() -> Result<DistCheckReport, String> {
    let cwd = std::env::current_dir().map_err(|e| format!("could not resolve cwd: {e}"))?;
    // `dist verify` only has work inside a difflore source checkout.
    let root = find_repo_root_from(&cwd).ok_or_else(|| {
        format!(
            "`difflore dist verify` is a maintainer command — run it from a checkout \
             of the difflore source tree (the one with `crates/difflore-cli/`). \
             Current directory: {}",
            cwd.display()
        )
    })?;
    Ok(verify_repo(&root))
}

/// Maintainer-only entry point for `difflore dist verify`.
/// Exits non-zero when release-drift errors are found.
pub(crate) fn handle_verify(json: bool) {
    let report = match verify_from_cwd() {
        Ok(report) => report,
        Err(message) => {
            eprintln!("{message}");
            exit_code(2);
        }
    };

    if json {
        match serde_json::to_string_pretty(&report) {
            Ok(rendered) => println!("{rendered}"),
            Err(e) => {
                eprintln!("could not serialize dist report: {e}");
                exit_code(2);
            }
        }
    } else {
        println!("dist verify — repo root: {}", report.repo_root);
        if let Some(version) = &report.expected_version {
            println!("manifest version: {version}");
        }
        for issue in &report.issues {
            println!("  {:?}: {} — {}", issue.severity, issue.path, issue.message);
        }
        println!(
            "{}: {} error(s), {} warning(s)",
            if report.ok() { "ok" } else { "FAILED" },
            report.error_count(),
            report.warning_count(),
        );
    }

    if !report.ok() {
        exit_code(1);
    }
}

pub fn verify_repo(root: &Path) -> DistCheckReport {
    let expected_version = read_crate_version(&root.join("crates/difflore-cli/Cargo.toml"));
    let mut report = DistCheckReport {
        repo_root: root.display().to_string(),
        expected_version,
        issues: Vec::new(),
    };

    check_required_files(root, &mut report);
    check_json_manifest(root, ".claude-plugin/plugin.json", &mut report);
    check_json_manifest(root, "plugin/.claude-plugin/plugin.json", &mut report);
    check_json_manifest(root, ".codex-plugin/plugin.json", &mut report);
    check_manifest_consistency(root, &mut report);
    check_marketplace(root, &mut report);
    check_mcp_bundle(root, &mut report);
    check_hook_bundle(root, &mut report);

    report
}

fn check_required_files(root: &Path, report: &mut DistCheckReport) {
    for rel in [
        ".claude-plugin/marketplace.json",
        ".claude-plugin/plugin.json",
        ".codex-plugin/plugin.json",
        "plugin/.claude-plugin/plugin.json",
        "plugin/.mcp.json",
        "plugin/hooks/hooks.json",
        "plugin/skills/rule-search/SKILL.md",
        "plugin/skills/remember-rule-guide/SKILL.md",
        "plugin/skills/rule-why-fired/SKILL.md",
        "plugin/skills/rule-gap/SKILL.md",
        "plugin/skills/rule-diff/SKILL.md",
        "plugin/skills/rule-journey/SKILL.md",
        "plugin/skills/smart-explore/SKILL.md",
        "plugin/skills/knowledge-agent/SKILL.md",
        "plugin/skills/session-recap/SKILL.md",
        "plugin/skills/difflore-onboard/SKILL.md",
    ] {
        if !root.join(rel).exists() {
            push(
                report,
                DistSeverity::Error,
                rel,
                "required distribution file is missing",
            );
        }
    }
}

fn check_json_manifest(root: &Path, rel: &str, report: &mut DistCheckReport) {
    let Some(value) = read_json(root, rel, report) else {
        return;
    };
    expect_string(&value, "name", "difflore", rel, report);
    expect_string(&value, "license", "Apache-2.0", rel, report);
    if let Some(version) = report.expected_version.clone() {
        expect_string(&value, "version", &version, rel, report);
    }
    let repo = value
        .get("repository")
        .and_then(Value::as_str)
        .unwrap_or("");
    let canonical = difflore_core::cloud::endpoints::GITHUB_REPO;
    if !repo.contains(canonical) {
        push(
            report,
            DistSeverity::Warning,
            rel,
            &format!("repository does not point at {canonical}"),
        );
    }
}

/// The repo root manifest (direct-install path) and the bundle manifest
/// (marketplace path) describe the same plugin; any field drift between them
/// ships inconsistent metadata to one of the two install flows.
fn check_manifest_consistency(root: &Path, report: &mut DistCheckReport) {
    let rel_root = ".claude-plugin/plugin.json";
    let rel_bundle = "plugin/.claude-plugin/plugin.json";
    let read = |rel: &str| -> Option<Value> {
        let raw = fs::read_to_string(root.join(rel)).ok()?;
        serde_json::from_str(&raw).ok()
    };
    // Unreadable or invalid manifests are already reported by check_json_manifest.
    let (Some(root_manifest), Some(bundle_manifest)) = (read(rel_root), read(rel_bundle)) else {
        return;
    };
    if root_manifest != bundle_manifest {
        push(
            report,
            DistSeverity::Error,
            rel_bundle,
            &format!("manifest drifted from {rel_root}; keep both files identical"),
        );
    }
}

fn check_marketplace(root: &Path, report: &mut DistCheckReport) {
    let rel = ".claude-plugin/marketplace.json";
    let Some(value) = read_json(root, rel, report) else {
        return;
    };
    expect_string(&value, "name", "difflore", rel, report);
    let plugin = value
        .get("plugins")
        .and_then(Value::as_array)
        .and_then(|plugins| {
            plugins
                .iter()
                .find(|p| p.get("name") == Some(&Value::String("difflore".into())))
        });
    let Some(plugin) = plugin else {
        push(
            report,
            DistSeverity::Error,
            rel,
            "plugins[] does not contain a difflore entry",
        );
        return;
    };
    if let Some(version) = report.expected_version.clone() {
        expect_string(plugin, "version", &version, rel, report);
    }
    expect_string(plugin, "source", "./plugin", rel, report);
}

fn check_mcp_bundle(root: &Path, report: &mut DistCheckReport) {
    let rel = "plugin/.mcp.json";
    let Some(value) = read_json(root, rel, report) else {
        return;
    };
    let server = value
        .pointer("/mcpServers/difflore")
        .or_else(|| value.pointer("/servers/difflore"));
    let Some(server) = server else {
        push(
            report,
            DistSeverity::Error,
            rel,
            "missing mcpServers.difflore entry",
        );
        return;
    };
    expect_string(server, "command", "difflore", rel, report);
    let has_mcp_server_arg = server
        .get("args")
        .and_then(Value::as_array)
        .is_some_and(|args| args.iter().any(|arg| arg.as_str() == Some("mcp-server")));
    if !has_mcp_server_arg {
        push(
            report,
            DistSeverity::Error,
            rel,
            "difflore MCP entry must pass the mcp-server arg",
        );
    }
}

fn check_hook_bundle(root: &Path, report: &mut DistCheckReport) {
    let rel = "plugin/hooks/hooks.json";
    let raw = match fs::read_to_string(root.join(rel)) {
        Ok(raw) => raw,
        Err(e) => {
            push(
                report,
                DistSeverity::Error,
                rel,
                &format!("could not read hooks bundle: {e}"),
            );
            return;
        }
    };
    // No PreToolUse: the Read pre-hook was retired to a dispatcher noop and
    // its registration removed (it cost a hook spawn per Read for nothing).
    for needle in [
        "difflore-hook --client claude-code",
        "PostToolUse",
        "SessionStart",
        "UserPromptSubmit",
    ] {
        if !raw.contains(needle) {
            push(
                report,
                DistSeverity::Error,
                rel,
                &format!("hooks bundle missing `{needle}`"),
            );
        }
    }
}

fn read_json(root: &Path, rel: &str, report: &mut DistCheckReport) -> Option<Value> {
    let path = root.join(rel);
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) => {
            push(
                report,
                DistSeverity::Error,
                rel,
                &format!("could not read JSON: {e}"),
            );
            return None;
        }
    };
    match serde_json::from_str(&raw) {
        Ok(v) => Some(v),
        Err(e) => {
            push(
                report,
                DistSeverity::Error,
                rel,
                &format!("invalid JSON: {e}"),
            );
            None
        }
    }
}

fn expect_string(
    value: &Value,
    key: &str,
    expected: &str,
    rel: &str,
    report: &mut DistCheckReport,
) {
    match value.get(key).and_then(Value::as_str) {
        Some(actual) if actual == expected => {}
        Some(actual) => push(
            report,
            DistSeverity::Error,
            rel,
            &format!("`{key}` is `{actual}`, expected `{expected}`"),
        ),
        None => push(
            report,
            DistSeverity::Error,
            rel,
            &format!("missing string field `{key}`"),
        ),
    }
}

fn read_crate_version(path: &Path) -> Option<String> {
    let raw = fs::read_to_string(path).ok()?;
    for line in raw.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("version") {
            // Accept only a literal `version = "..."`; dotted Cargo keys like
            // `version.workspace = true` are not comparable release versions.
            let next = rest.chars().next()?;
            if next != '=' && !next.is_whitespace() {
                continue;
            }
            let (_, value) = rest.split_once('=')?;
            return Some(value.trim().trim_matches('"').to_owned());
        }
    }
    None
}

fn push(report: &mut DistCheckReport, severity: DistSeverity, path: &str, message: &str) {
    report.issues.push(DistIssue {
        severity,
        path: path.to_owned(),
        message: message.to_owned(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crate_version_parser_reads_package_version() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let path = tmp.path().join("Cargo.toml");
        fs::write(
            &path,
            "[package]\nname = \"difflore-cli\"\nversion = \"0.1.0\"\n",
        )
        .expect("write");
        assert_eq!(read_crate_version(&path).as_deref(), Some("0.1.0"));
    }

    #[test]
    fn manifest_consistency_flags_drift_between_root_and_bundle() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let root = tmp.path();
        fs::create_dir_all(root.join(".claude-plugin")).expect("mkdir");
        fs::create_dir_all(root.join("plugin/.claude-plugin")).expect("mkdir");
        let manifest = r#"{"name":"difflore","version":"0.1.0"}"#;
        fs::write(root.join(".claude-plugin/plugin.json"), manifest).expect("write");
        fs::write(root.join("plugin/.claude-plugin/plugin.json"), manifest).expect("write");

        let mut report = DistCheckReport {
            repo_root: root.display().to_string(),
            expected_version: None,
            issues: Vec::new(),
        };
        check_manifest_consistency(root, &mut report);
        assert!(report.issues.is_empty());

        fs::write(
            root.join("plugin/.claude-plugin/plugin.json"),
            r#"{"name":"difflore","version":"0.2.0"}"#,
        )
        .expect("write");
        check_manifest_consistency(root, &mut report);
        assert_eq!(report.error_count(), 1);
    }

    #[test]
    fn report_ok_requires_no_error_issues() {
        let mut report = DistCheckReport {
            repo_root: ".".into(),
            expected_version: Some("0.1.0".into()),
            issues: Vec::new(),
        };
        push(&mut report, DistSeverity::Warning, "x", "warn");
        assert!(report.ok());
        push(&mut report, DistSeverity::Error, "x", "error");
        assert!(!report.ok());
        assert_eq!(report.error_count(), 1);
        assert_eq!(report.warning_count(), 1);
    }
}
