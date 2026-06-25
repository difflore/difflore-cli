use std::collections::HashSet;
use std::fs;
use std::path::Component;
use std::path::{Path, PathBuf};

use difflore_core::export::{
    ExportCollectOptions, ExportRule, collect_rules_for_export, is_explicit_local_rule,
    repo_scope_matches,
};
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};

use crate::cli::MemoryPackageFormatArg;
use crate::runtime::CommandContext;
use crate::style;
use crate::support::util::{exit_code, exit_err, json_or};

const PACKAGE_SCHEMA_VERSION: &str = "difflore.memory-package.v1";
const RULE_SCHEMA_VERSION: &str = "difflore.memory-rule.v1";
const PACKAGE_VERSION: u32 = 1;
const HASH_ALGORITHM: &str = "sha1";
const MANIFEST_FILE: &str = "manifest.json";
const RULES_DIR: &str = "rules";
const MD_META_START: &str = "<!-- difflore-memory-rule";
const MD_META_END: &str = "-->";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum PackageFormat {
    Json,
    Markdown,
}

impl PackageFormat {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Markdown => "markdown",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MemoryPackage {
    schema_version: String,
    version: u32,
    hash: String,
    manifest: PackageManifest,
    rules: Vec<PackageRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PackageManifest {
    package_id: String,
    exported_at_utc: String,
    difflore_version: String,
    format: PackageFormat,
    repo_scopes: Vec<String>,
    rule_count: usize,
    total_rules: usize,
    truncated: bool,
    hash_algorithm: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PackageRule {
    schema_version: String,
    version: u32,
    id: String,
    name: String,
    description: String,
    #[serde(rename = "type")]
    rule_type: String,
    source: String,
    origin: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_repo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    check_prompt: Option<String>,
    file_patterns: Vec<String>,
    hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ExportPackageReport {
    dry_run: bool,
    format: &'static str,
    output: String,
    schema_version: &'static str,
    version: u32,
    hash: String,
    rules: usize,
    total_rules: usize,
    truncated: bool,
    files: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ImportPackageReport {
    dry_run: bool,
    format: &'static str,
    source: String,
    schema_version: String,
    version: u32,
    package_hash: String,
    current_hash: String,
    changed_since_export: bool,
    rules: usize,
    updated: usize,
    unchanged: usize,
    missing: usize,
    rejected: usize,
    operations: Vec<ImportOperation>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ImportOperation {
    id: String,
    action: &'static str,
    changed: Vec<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
}

#[derive(Debug, Clone)]
struct ParsedPackage {
    package: MemoryPackage,
    current_hash: String,
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct ExistingRuleRow {
    name: String,
    description: String,
    check_prompt: Option<String>,
    file_patterns: Option<String>,
    source: String,
    source_repo: Option<String>,
    status: String,
    cloud_id: Option<String>,
}

pub(crate) async fn handle_export_package(
    ctx: &CommandContext,
    output: PathBuf,
    format_arg: MemoryPackageFormatArg,
    dry_run: bool,
    json: bool,
    local_only: bool,
    max_rules: Option<usize>,
) {
    let format = resolve_format(format_arg, &output);
    ensure_export_target(&output, format, dry_run)
        .unwrap_or_else(|err| exit_structured_err(&err, json));

    let collection = collect_rules_for_export(
        &ctx.db,
        &ctx.project,
        ExportCollectOptions {
            local_only,
            include_examples: false,
            max_rules,
            ..ExportCollectOptions::default()
        },
    )
    .await
    .unwrap_or_else(|err| exit_structured_err(&format!("failed to collect rules: {err}"), json));

    let package = build_package(
        &collection.rules,
        collection.repo_scopes,
        collection.total_in_scope,
        format,
    );
    let files = planned_files(&output, format, &package);
    if !dry_run {
        write_package(&output, format, &package)
            .unwrap_or_else(|err| exit_structured_err(&err, json));
    }

    let report = ExportPackageReport {
        dry_run,
        format: format.as_str(),
        output: output.display().to_string(),
        schema_version: PACKAGE_SCHEMA_VERSION,
        version: PACKAGE_VERSION,
        hash: package.hash,
        rules: package.rules.len(),
        total_rules: package.manifest.total_rules,
        truncated: package.manifest.truncated,
        files,
    };
    if json {
        println!("{}", json_or(&report, "{}"));
    } else {
        print_export_report(&report);
    }
}

pub(crate) async fn handle_import_package(
    ctx: &CommandContext,
    source: PathBuf,
    dry_run: bool,
    json: bool,
) {
    let parsed = read_package(&source).unwrap_or_else(|err| exit_structured_err(&err, json));
    validate_package(&parsed.package).unwrap_or_else(|err| exit_structured_err(&err, json));
    let report = import_existing_rules(ctx, parsed, source, dry_run)
        .await
        .unwrap_or_else(|err| exit_structured_err(&err, json));

    if json {
        println!("{}", json_or(&report, "{}"));
    } else {
        print_import_report(&report);
    }
}

fn resolve_format(format_arg: MemoryPackageFormatArg, output: &Path) -> PackageFormat {
    match format_arg {
        MemoryPackageFormatArg::Auto => {
            if output
                .extension()
                .and_then(|value| value.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
            {
                PackageFormat::Json
            } else {
                PackageFormat::Markdown
            }
        }
        MemoryPackageFormatArg::Json => PackageFormat::Json,
        MemoryPackageFormatArg::Markdown => PackageFormat::Markdown,
    }
}

fn ensure_export_target(path: &Path, format: PackageFormat, dry_run: bool) -> Result<(), String> {
    match format {
        PackageFormat::Json => ensure_json_target(path, dry_run),
        PackageFormat::Markdown => ensure_markdown_target(path, dry_run),
    }
}

fn ensure_json_target(path: &Path, dry_run: bool) -> Result<(), String> {
    if path.is_dir() {
        return Err(format!(
            "json package target `{}` is a directory",
            path.display()
        ));
    }
    if path.exists() {
        let metadata = fs::metadata(path)
            .map_err(|err| format!("failed to inspect `{}`: {err}", path.display()))?;
        if metadata.len() > 0 {
            return Err(format!(
                "refusing to overwrite non-empty package file `{}`",
                path.display()
            ));
        }
    }
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
        && !dry_run
    {
        return Err(format!(
            "parent directory `{}` does not exist",
            parent.display()
        ));
    }
    Ok(())
}

fn ensure_markdown_target(path: &Path, dry_run: bool) -> Result<(), String> {
    if path.is_file() {
        return Err(format!(
            "markdown package target `{}` is a file",
            path.display()
        ));
    }
    if path.exists() {
        let mut entries = fs::read_dir(path)
            .map_err(|err| format!("failed to inspect `{}`: {err}", path.display()))?;
        if entries.next().is_some() {
            return Err(format!(
                "refusing to write markdown package into non-empty directory `{}`",
                path.display()
            ));
        }
    } else if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
        && !dry_run
    {
        return Err(format!(
            "parent directory `{}` does not exist",
            parent.display()
        ));
    }
    Ok(())
}

fn build_package(
    rules: &[ExportRule],
    repo_scopes: Vec<String>,
    total_rules: usize,
    format: PackageFormat,
) -> MemoryPackage {
    let exported_at_utc = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let mut package_rules: Vec<PackageRule> = rules
        .iter()
        .enumerate()
        .map(|(index, rule)| package_rule_from_export(index, rule, format))
        .collect();
    for rule in &mut package_rules {
        rule.hash = rule_hash(rule);
    }
    let manifest = PackageManifest {
        package_id: format!("difflore-memory-{}", short_hash(&exported_at_utc)),
        exported_at_utc,
        difflore_version: env!("CARGO_PKG_VERSION").to_owned(),
        format,
        repo_scopes,
        rule_count: package_rules.len(),
        total_rules,
        truncated: total_rules > package_rules.len(),
        hash_algorithm: HASH_ALGORITHM.to_owned(),
    };
    let hash = package_hash(&manifest, &package_rules);
    MemoryPackage {
        schema_version: PACKAGE_SCHEMA_VERSION.to_owned(),
        version: PACKAGE_VERSION,
        hash,
        manifest,
        rules: package_rules,
    }
}

fn package_rule_from_export(index: usize, rule: &ExportRule, format: PackageFormat) -> PackageRule {
    let path = (format == PackageFormat::Markdown).then(|| {
        format!(
            "{RULES_DIR}/{:03}-{}.md",
            index + 1,
            slug_or_id(&rule.name, &rule.id)
        )
    });
    PackageRule {
        schema_version: RULE_SCHEMA_VERSION.to_owned(),
        version: PACKAGE_VERSION,
        id: rule.id.clone(),
        name: rule.name.clone(),
        description: rule.description.clone(),
        rule_type: rule.r#type.clone(),
        source: rule.source.clone(),
        origin: rule.origin.clone(),
        source_repo: rule.repo_scope.clone(),
        check_prompt: rule.check_prompt.clone(),
        file_patterns: rule.file_patterns.clone(),
        hash: String::new(),
        path,
    }
}

fn planned_files(output: &Path, format: PackageFormat, package: &MemoryPackage) -> Vec<String> {
    match format {
        PackageFormat::Json => vec![output.display().to_string()],
        PackageFormat::Markdown => {
            let mut files = vec![output.join(MANIFEST_FILE).display().to_string()];
            files.extend(package.rules.iter().filter_map(|rule| {
                rule.path
                    .as_ref()
                    .map(|relative| output.join(relative).display().to_string())
            }));
            files
        }
    }
}

fn write_package(
    path: &Path,
    format: PackageFormat,
    package: &MemoryPackage,
) -> Result<(), String> {
    match format {
        PackageFormat::Json => write_json_file(path, package),
        PackageFormat::Markdown => write_markdown_dir(path, package),
    }
}

fn write_json_file(path: &Path, package: &MemoryPackage) -> Result<(), String> {
    let body = serde_json::to_string_pretty(package)
        .map_err(|err| format!("failed to serialize package: {err}"))?;
    fs::write(path, format!("{body}\n"))
        .map_err(|err| format!("failed to write `{}`: {err}", path.display()))
}

fn write_markdown_dir(path: &Path, package: &MemoryPackage) -> Result<(), String> {
    fs::create_dir_all(path)
        .map_err(|err| format!("failed to create `{}`: {err}", path.display()))?;
    let rules_dir = path.join(RULES_DIR);
    fs::create_dir_all(&rules_dir)
        .map_err(|err| format!("failed to create `{}`: {err}", rules_dir.display()))?;
    write_json_file(&path.join(MANIFEST_FILE), package)?;
    for rule in &package.rules {
        let relative = rule
            .path
            .as_deref()
            .ok_or_else(|| format!("rule `{}` is missing markdown path", rule.id))?;
        let body = render_rule_markdown(rule)?;
        let full_path = package_relative_path(path, relative)?;
        fs::write(&full_path, body)
            .map_err(|err| format!("failed to write `{}`: {err}", full_path.display()))?;
    }
    Ok(())
}

fn render_rule_markdown(rule: &PackageRule) -> Result<String, String> {
    let meta = MarkdownRuleMeta::from_rule(rule);
    let meta_json = serde_json::to_string_pretty(&meta)
        .map_err(|err| format!("failed to serialize rule metadata: {err}"))?;
    let meta_json = markdown_comment_safe_json(&meta_json);
    Ok(format!(
        "{MD_META_START}\n{meta_json}\n{MD_META_END}\n\n# {}\n\n{}\n",
        rule.name.trim(),
        rule.description.trim()
    ))
}

fn markdown_comment_safe_json(value: &str) -> String {
    value.replace(MD_META_END, "--\\u003e")
}

fn read_package(source: &Path) -> Result<ParsedPackage, String> {
    if source.is_dir() {
        read_markdown_package(source)
    } else {
        read_json_package(source)
    }
}

fn read_json_package(source: &Path) -> Result<ParsedPackage, String> {
    let raw = fs::read_to_string(source)
        .map_err(|err| format!("failed to read `{}`: {err}", source.display()))?;
    let mut package: MemoryPackage = serde_json::from_str(&raw)
        .map_err(|err| format!("failed to parse JSON package `{}`: {err}", source.display()))?;
    for rule in &mut package.rules {
        let current = rule_hash(rule);
        if rule.hash.trim().is_empty() {
            rule.hash = current;
        }
    }
    let current_hash = package_hash(&package.manifest, &package.rules);
    Ok(ParsedPackage {
        package,
        current_hash,
    })
}

fn read_markdown_package(source: &Path) -> Result<ParsedPackage, String> {
    let manifest_path = source.join(MANIFEST_FILE);
    let raw = fs::read_to_string(&manifest_path)
        .map_err(|err| format!("failed to read `{}`: {err}", manifest_path.display()))?;
    let mut package: MemoryPackage = serde_json::from_str(&raw).map_err(|err| {
        format!(
            "failed to parse markdown package manifest `{}`: {err}",
            manifest_path.display()
        )
    })?;
    let mut parsed_rules = Vec::with_capacity(package.rules.len());
    for manifest_rule in &package.rules {
        let relative = manifest_rule
            .path
            .as_deref()
            .ok_or_else(|| format!("manifest rule `{}` is missing path", manifest_rule.id))?;
        let full_path = package_relative_path(source, relative)?;
        let raw_rule = fs::read_to_string(&full_path)
            .map_err(|err| format!("failed to read `{}`: {err}", full_path.display()))?;
        let mut parsed = parse_rule_markdown(&raw_rule, relative)?;
        if parsed.id != manifest_rule.id {
            return Err(format!(
                "rule file `{relative}` declares id `{}` but manifest expects `{}`",
                parsed.id, manifest_rule.id
            ));
        }
        parsed.path = Some(relative.to_owned());
        parsed.hash = rule_hash(&parsed);
        parsed_rules.push(parsed);
    }
    package.rules = parsed_rules;
    let current_hash = package_hash(&package.manifest, &package.rules);
    Ok(ParsedPackage {
        package,
        current_hash,
    })
}

fn package_relative_path(root: &Path, relative: &str) -> Result<PathBuf, String> {
    let rel = Path::new(relative);
    if rel.is_absolute() {
        return Err(format!("package path `{relative}` must be relative"));
    }
    let mut components = rel.components();
    match components.next() {
        Some(Component::Normal(first)) if first == RULES_DIR => {}
        _ => {
            return Err(format!(
                "package path `{relative}` must stay under `{RULES_DIR}/`"
            ));
        }
    }
    for component in components {
        match component {
            Component::Normal(_) => {}
            _ => {
                return Err(format!(
                    "package path `{relative}` must not contain parent, root, or prefix components"
                ));
            }
        }
    }
    Ok(root.join(rel))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MarkdownRuleMeta {
    schema_version: String,
    version: u32,
    id: String,
    #[serde(rename = "type")]
    rule_type: String,
    source: String,
    origin: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_repo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    check_prompt: Option<String>,
    file_patterns: Vec<String>,
    hash: String,
}

impl MarkdownRuleMeta {
    fn from_rule(rule: &PackageRule) -> Self {
        Self {
            schema_version: RULE_SCHEMA_VERSION.to_owned(),
            version: PACKAGE_VERSION,
            id: rule.id.clone(),
            rule_type: rule.rule_type.clone(),
            source: rule.source.clone(),
            origin: rule.origin.clone(),
            source_repo: rule.source_repo.clone(),
            check_prompt: rule.check_prompt.clone(),
            file_patterns: rule.file_patterns.clone(),
            hash: rule.hash.clone(),
        }
    }
}

fn parse_rule_markdown(raw: &str, relative_path: &str) -> Result<PackageRule, String> {
    let start = raw.find(MD_META_START).ok_or_else(|| {
        format!("rule file `{relative_path}` is missing difflore metadata comment")
    })?;
    let meta_start = start + MD_META_START.len();
    let end_offset = raw[meta_start..].find(MD_META_END).ok_or_else(|| {
        format!("rule file `{relative_path}` has an unterminated metadata comment")
    })?;
    let meta_end = meta_start + end_offset;
    let meta_raw = raw[meta_start..meta_end].trim();
    let meta: MarkdownRuleMeta = serde_json::from_str(meta_raw)
        .map_err(|err| format!("rule file `{relative_path}` has invalid metadata: {err}"))?;
    validate_rule_meta(&meta, relative_path)?;
    let body_start = meta_end + MD_META_END.len();
    let markdown_body = raw[body_start..].trim_start_matches(['\r', '\n']);
    let (name, description) = parse_markdown_title_and_body(markdown_body, relative_path)?;
    let mut rule = PackageRule {
        schema_version: meta.schema_version,
        version: meta.version,
        id: meta.id,
        name,
        description,
        rule_type: meta.rule_type,
        source: meta.source,
        origin: meta.origin,
        source_repo: meta.source_repo,
        check_prompt: meta.check_prompt,
        file_patterns: meta.file_patterns,
        hash: meta.hash,
        path: Some(relative_path.to_owned()),
    };
    rule.hash = rule_hash(&rule);
    Ok(rule)
}

fn parse_markdown_title_and_body(
    raw: &str,
    relative_path: &str,
) -> Result<(String, String), String> {
    let mut lines = raw.lines();
    let title_line = lines
        .find(|line| !line.trim().is_empty())
        .ok_or_else(|| format!("rule file `{relative_path}` is missing a markdown title"))?;
    let title = title_line
        .trim()
        .strip_prefix("# ")
        .ok_or_else(|| {
            format!("rule file `{relative_path}` must put the editable title in a `# ` heading")
        })?
        .trim()
        .to_owned();
    if title.is_empty() {
        return Err(format!("rule file `{relative_path}` has an empty title"));
    }
    let body = lines.collect::<Vec<_>>().join("\n");
    let description = body.trim().to_owned();
    if description.is_empty() {
        return Err(format!("rule file `{relative_path}` has an empty body"));
    }
    Ok((title, description))
}

fn validate_package(package: &MemoryPackage) -> Result<(), String> {
    if package.schema_version != PACKAGE_SCHEMA_VERSION {
        return Err(format!(
            "unsupported package schemaVersion `{}`",
            package.schema_version
        ));
    }
    if package.version != PACKAGE_VERSION {
        return Err(format!("unsupported package version `{}`", package.version));
    }
    if package.manifest.hash_algorithm != HASH_ALGORITHM {
        return Err(format!(
            "unsupported package hash algorithm `{}`",
            package.manifest.hash_algorithm
        ));
    }
    if package.manifest.rule_count != package.rules.len() {
        return Err(format!(
            "manifest ruleCount {} does not match {} rule entries",
            package.manifest.rule_count,
            package.rules.len()
        ));
    }
    let mut seen = HashSet::new();
    for rule in &package.rules {
        validate_rule(rule)?;
        if !seen.insert(rule.id.clone()) {
            return Err(format!("duplicate rule id `{}` in package", rule.id));
        }
    }
    Ok(())
}

fn validate_rule(rule: &PackageRule) -> Result<(), String> {
    if rule.schema_version != RULE_SCHEMA_VERSION {
        return Err(format!(
            "rule `{}` has unsupported schemaVersion `{}`",
            rule.id, rule.schema_version
        ));
    }
    if rule.version != PACKAGE_VERSION {
        return Err(format!(
            "rule `{}` has unsupported version `{}`",
            rule.id, rule.version
        ));
    }
    if rule.id.trim().is_empty() {
        return Err("package contains a rule with an empty id".to_owned());
    }
    if rule.name.trim().is_empty() {
        return Err(format!("rule `{}` has an empty name", rule.id));
    }
    if rule.description.trim().is_empty() {
        return Err(format!("rule `{}` has an empty description", rule.id));
    }
    Ok(())
}

fn validate_rule_meta(meta: &MarkdownRuleMeta, relative_path: &str) -> Result<(), String> {
    if meta.schema_version != RULE_SCHEMA_VERSION {
        return Err(format!(
            "rule file `{relative_path}` has unsupported schemaVersion `{}`",
            meta.schema_version
        ));
    }
    if meta.version != PACKAGE_VERSION {
        return Err(format!(
            "rule file `{relative_path}` has unsupported version `{}`",
            meta.version
        ));
    }
    if meta.id.trim().is_empty() {
        return Err(format!("rule file `{relative_path}` has an empty id"));
    }
    Ok(())
}

async fn import_existing_rules(
    ctx: &CommandContext,
    parsed: ParsedPackage,
    source: PathBuf,
    dry_run: bool,
) -> Result<ImportPackageReport, String> {
    let repo_scopes = import_repo_scopes(ctx).await;
    import_existing_rules_with_scopes(&ctx.db, parsed, source, dry_run, &repo_scopes).await
}

async fn import_repo_scopes(ctx: &CommandContext) -> Vec<String> {
    let configured_gitlab_hosts = difflore_core::ingest::gitlab::auth::configured_hosts().await;
    difflore_core::infra::git::detect_repo_full_names_with_gitlab_hosts(
        &ctx.project.to_string_lossy(),
        &configured_gitlab_hosts,
    )
}

async fn import_existing_rules_with_scopes(
    db: &difflore_core::SqlitePool,
    parsed: ParsedPackage,
    source: PathBuf,
    dry_run: bool,
    repo_scopes: &[String],
) -> Result<ImportPackageReport, String> {
    let mut operations = Vec::with_capacity(parsed.package.rules.len());
    let mut updated = 0;
    let mut unchanged = 0;
    let mut missing = 0;
    let mut rejected = 0;

    // Apply every per-rule UPDATE inside a single transaction so a non-dry-run
    // import is all-or-nothing: if any rule fails to update we roll back instead
    // of leaving the database partially mutated by earlier rules in the batch.
    let mut tx = if dry_run {
        None
    } else {
        Some(
            db.begin()
                .await
                .map_err(|err| format!("failed to begin import transaction: {err}"))?,
        )
    };

    for rule in &parsed.package.rules {
        // Read through the open transaction when one exists so the load and the
        // subsequent update share a single connection (the pool may only hand
        // out one), and so the read observes this batch's own writes.
        let existing = match tx.as_mut() {
            Some(tx) => load_existing_rule(&mut **tx, &rule.id).await?,
            None => load_existing_rule(db, &rule.id).await?,
        };
        let Some(existing) = existing else {
            missing += 1;
            operations.push(ImportOperation {
                id: rule.id.clone(),
                action: "missing",
                changed: Vec::new(),
                reason: None,
                path: rule.path.clone(),
            });
            continue;
        };

        if let Some(reason) = import_rejection_reason(&existing, repo_scopes) {
            rejected += 1;
            operations.push(ImportOperation {
                id: rule.id.clone(),
                action: "rejected",
                changed: Vec::new(),
                reason: Some(reason),
                path: rule.path.clone(),
            });
            continue;
        }

        let changed = changed_fields(&existing, rule);
        if changed.is_empty() {
            unchanged += 1;
            operations.push(ImportOperation {
                id: rule.id.clone(),
                action: "unchanged",
                changed,
                reason: None,
                path: rule.path.clone(),
            });
            continue;
        }

        updated += 1;
        if let Some(tx) = tx.as_mut() {
            update_existing_rule(tx, rule, repo_scopes).await?;
        }
        operations.push(ImportOperation {
            id: rule.id.clone(),
            action: if dry_run { "would-update" } else { "updated" },
            changed,
            reason: None,
            path: rule.path.clone(),
        });
    }

    if let Some(tx) = tx {
        tx.commit()
            .await
            .map_err(|err| format!("failed to commit import transaction: {err}"))?;
    }

    Ok(ImportPackageReport {
        dry_run,
        format: parsed.package.manifest.format.as_str(),
        source: source.display().to_string(),
        schema_version: parsed.package.schema_version,
        version: parsed.package.version,
        package_hash: parsed.package.hash.clone(),
        current_hash: parsed.current_hash.clone(),
        changed_since_export: parsed.current_hash != parsed.package.hash,
        rules: parsed.package.rules.len(),
        updated,
        unchanged,
        missing,
        rejected,
        operations,
    })
}

async fn load_existing_rule<'e, E>(db: E, id: &str) -> Result<Option<ExistingRuleRow>, String>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    sqlx::query_as::<_, ExistingRuleRow>(
        "SELECT name, description, check_prompt, file_patterns, source, source_repo, status, cloud_id
         FROM skills WHERE id = ?1",
    )
    .bind(id)
    .fetch_optional(db)
    .await
    .map_err(|err| format!("failed to load existing rule `{id}`: {err}"))
}

fn import_rejection_reason(
    existing: &ExistingRuleRow,
    repo_scopes: &[String],
) -> Option<&'static str> {
    if existing.status != "active" {
        return Some(if existing.status == "pending" {
            "pending"
        } else {
            "not-active"
        });
    }
    let source = existing.source.trim();
    if source.eq_ignore_ascii_case("team") {
        return Some("team-synced");
    }
    if source.eq_ignore_ascii_case("cloud") {
        return Some("cloud-synced");
    }
    if normalize_optional(existing.cloud_id.as_deref()).is_some() {
        return Some("cloud-synced");
    }

    let repo_scope = difflore_core::context::rule_source::repo_scope_from_source_repo(
        existing.source_repo.as_deref(),
    );
    if repo_scope_matches(repo_scope.as_deref(), repo_scopes)
        || is_explicit_local_rule(source, existing.source_repo.as_deref())
    {
        return None;
    }
    if normalize_optional(existing.source_repo.as_deref()).is_some() {
        Some("cross-repo")
    } else {
        Some("not-importable")
    }
}

fn changed_fields(existing: &ExistingRuleRow, rule: &PackageRule) -> Vec<&'static str> {
    let mut changed = Vec::new();
    if existing.name != rule.name {
        changed.push("name");
    }
    if existing.description != rule.description {
        changed.push("description");
    }
    if normalize_optional(existing.check_prompt.as_deref())
        != normalize_optional(rule.check_prompt.as_deref())
    {
        changed.push("checkPrompt");
    }
    if parse_file_patterns(existing.file_patterns.as_deref())
        != normalize_file_patterns(&rule.file_patterns)
    {
        changed.push("filePatterns");
    }
    changed
}

async fn update_existing_rule(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    rule: &PackageRule,
    repo_scopes: &[String],
) -> Result<(), String> {
    let file_patterns_json = file_patterns_json(&rule.file_patterns)?;
    let repo_scopes_json = repo_scopes_json(repo_scopes)?;
    let content_hash = difflore_core::skills::remember_rule_content_hash(
        &rule.name,
        &rule.description,
        Some(&rule.file_patterns),
    );
    let result = sqlx::query(
        "UPDATE skills
         SET name = ?1,
             description = ?2,
             check_prompt = ?3,
             file_patterns = ?4,
             content_hash = ?5,
             hash_created_at = (unixepoch('now') * 1000),
             updated_at = datetime('now')
         WHERE id = ?6
           AND status = 'active'
           AND lower(trim(source)) NOT IN ('cloud', 'team')
           AND (cloud_id IS NULL OR trim(cloud_id) = '')
           AND (
                (trim(source) = 'local' AND (source_repo IS NULL OR trim(source_repo) = ''))
                OR lower(trim(source_repo)) IN (SELECT value FROM json_each(?7))
           )",
    )
    .bind(rule.name.trim())
    .bind(rule.description.trim())
    .bind(normalize_optional(rule.check_prompt.as_deref()))
    .bind(file_patterns_json)
    .bind(content_hash)
    .bind(rule.id.as_str())
    .bind(repo_scopes_json)
    .execute(&mut **tx)
    .await
    .map_err(|err| format!("failed to update rule `{}`: {err}", rule.id))?;
    if result.rows_affected() == 0 {
        return Err(format!(
            "refusing to update rule `{}` because it is no longer active/importable for this repo",
            rule.id
        ));
    }
    Ok(())
}

fn repo_scopes_json(repo_scopes: &[String]) -> Result<String, String> {
    let mut normalized: Vec<String> = repo_scopes
        .iter()
        .filter_map(|scope| difflore_core::infra::git::normalize_canonical_repo_scope(scope))
        .map(|scope| scope.to_ascii_lowercase())
        .collect();
    normalized.sort();
    normalized.dedup();
    serde_json::to_string(&normalized)
        .map_err(|err| format!("failed to serialize repo scopes: {err}"))
}

fn normalize_optional(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn parse_file_patterns(raw: Option<&str>) -> Vec<String> {
    raw.and_then(|value| serde_json::from_str::<Vec<String>>(value).ok())
        .map(|patterns| normalize_file_patterns(&patterns))
        .unwrap_or_default()
}

fn normalize_file_patterns(patterns: &[String]) -> Vec<String> {
    let mut out: Vec<String> = patterns
        .iter()
        .map(|pattern| pattern.trim())
        .filter(|pattern| !pattern.is_empty())
        .map(ToOwned::to_owned)
        .collect();
    out.sort();
    out.dedup();
    out
}

fn file_patterns_json(patterns: &[String]) -> Result<String, String> {
    serde_json::to_string(&normalize_file_patterns(patterns))
        .map_err(|err| format!("failed to serialize file patterns: {err}"))
}

fn package_hash(manifest: &PackageManifest, rules: &[PackageRule]) -> String {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct HashMaterial<'a> {
        schema_version: &'static str,
        version: u32,
        manifest: &'a PackageManifest,
        rules: Vec<RuleHashMaterial<'a>>,
    }
    let rules = rules.iter().map(RuleHashMaterial::from_rule).collect();
    let material = HashMaterial {
        schema_version: PACKAGE_SCHEMA_VERSION,
        version: PACKAGE_VERSION,
        manifest,
        rules,
    };
    hash_json(&material)
}

fn rule_hash(rule: &PackageRule) -> String {
    hash_json(&RuleHashMaterial::from_rule(rule))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RuleHashMaterial<'a> {
    schema_version: &'a str,
    version: u32,
    id: &'a str,
    name: &'a str,
    description: &'a str,
    #[serde(rename = "type")]
    rule_type: &'a str,
    source: &'a str,
    origin: &'a str,
    source_repo: Option<&'a str>,
    check_prompt: Option<&'a str>,
    file_patterns: Vec<String>,
    path: Option<&'a str>,
}

impl<'a> RuleHashMaterial<'a> {
    fn from_rule(rule: &'a PackageRule) -> Self {
        Self {
            schema_version: &rule.schema_version,
            version: rule.version,
            id: rule.id.trim(),
            name: rule.name.trim(),
            description: rule.description.trim(),
            rule_type: rule.rule_type.trim(),
            source: rule.source.trim(),
            origin: rule.origin.trim(),
            source_repo: rule.source_repo.as_deref().map(str::trim),
            check_prompt: rule.check_prompt.as_deref().map(str::trim),
            file_patterns: normalize_file_patterns(&rule.file_patterns),
            path: rule.path.as_deref(),
        }
    }
}

fn hash_json(value: &impl Serialize) -> String {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    let mut hasher = Sha1::new();
    hasher.update(bytes);
    format!("sha1:{}", hex_digest(hasher.finalize().as_slice()))
}

fn short_hash(value: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(value.as_bytes());
    hex_digest(&hasher.finalize()[..6])
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

fn slug_or_id(name: &str, id: &str) -> String {
    let slug = slugify(name);
    if slug.is_empty() { slugify(id) } else { slug }
}

fn slugify(value: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in value.chars() {
        let next = if ch.is_ascii_alphanumeric() {
            last_dash = false;
            Some(ch.to_ascii_lowercase())
        } else if !last_dash {
            last_dash = true;
            Some('-')
        } else {
            None
        };
        if let Some(ch) = next {
            out.push(ch);
        }
        if out.len() >= 64 {
            break;
        }
    }
    out.trim_matches('-').to_owned()
}

fn print_export_report(report: &ExportPackageReport) {
    let verb = if report.dry_run {
        "Export package plan"
    } else {
        "Exported memory package"
    };
    println!("{}", style::title(verb));
    println!("  format: {}", report.format);
    println!("  output: {}", style::ident(&report.output));
    println!("  schema: {}", report.schema_version);
    println!("  hash: {}", report.hash);
    println!(
        "  rules: {}{}",
        report.rules,
        if report.truncated {
            format!(" of {}", report.total_rules)
        } else {
            String::new()
        }
    );
    if !report.files.is_empty() {
        println!();
        println!("{}", style::title("Files"));
        for file in &report.files {
            println!("  {file}");
        }
    }
}

fn print_import_report(report: &ImportPackageReport) {
    let verb = if report.dry_run {
        "Import package plan"
    } else {
        "Imported memory package"
    };
    println!("{}", style::title(verb));
    println!("  format: {}", report.format);
    println!("  source: {}", style::ident(&report.source));
    println!("  package hash: {}", report.package_hash);
    println!("  current hash: {}", report.current_hash);
    println!(
        "  changed since export: {}",
        if report.changed_since_export {
            "yes"
        } else {
            "no"
        }
    );
    println!(
        "  result: {} updated, {} unchanged, {} missing, {} rejected",
        report.updated, report.unchanged, report.missing, report.rejected
    );
    if !report.operations.is_empty() {
        println!();
        println!("{}", style::title("Rules"));
        for op in &report.operations {
            let fields = if op.changed.is_empty() {
                String::new()
            } else {
                format!(" ({})", op.changed.join(", "))
            };
            let reason = op
                .reason
                .map(|reason| format!(" [{reason}]"))
                .unwrap_or_default();
            println!(
                "  {} {}{}{}",
                op.action,
                style::ident(&op.id),
                fields,
                reason
            );
        }
    }
}

fn exit_structured_err(message: &str, json: bool) -> ! {
    if json {
        #[derive(Serialize)]
        struct ErrorBody<'a> {
            error: &'a str,
        }
        println!("{}", json_or(&ErrorBody { error: message }, "{}"));
        exit_code(1);
    }
    exit_err(message);
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    fn sample_rule() -> PackageRule {
        let mut rule = PackageRule {
            schema_version: RULE_SCHEMA_VERSION.to_owned(),
            version: PACKAGE_VERSION,
            id: "rule-1".to_owned(),
            name: "Prefer small modules".to_owned(),
            description: "Split large modules before adding more exports.".to_owned(),
            rule_type: "review_standard".to_owned(),
            source: "local".to_owned(),
            origin: "manual".to_owned(),
            source_repo: None,
            check_prompt: Some("Did you keep the module small?".to_owned()),
            file_patterns: vec!["**/*.rs".to_owned()],
            hash: String::new(),
            path: Some("rules/001-prefer-small-modules.md".to_owned()),
        };
        rule.hash = rule_hash(&rule);
        rule
    }

    fn sample_package(format: PackageFormat) -> MemoryPackage {
        let mut rule = sample_rule();
        if format == PackageFormat::Json {
            rule.path = None;
            rule.hash = rule_hash(&rule);
        }
        let manifest = PackageManifest {
            package_id: "pkg-test".to_owned(),
            exported_at_utc: "2026-06-19T00:00:00Z".to_owned(),
            difflore_version: "0.2.0".to_owned(),
            format,
            repo_scopes: Vec::new(),
            rule_count: 1,
            total_rules: 1,
            truncated: false,
            hash_algorithm: HASH_ALGORITHM.to_owned(),
        };
        let rules = vec![rule];
        let hash = package_hash(&manifest, &rules);
        MemoryPackage {
            schema_version: PACKAGE_SCHEMA_VERSION.to_owned(),
            version: PACKAGE_VERSION,
            hash,
            manifest,
            rules,
        }
    }

    fn package_rule_for_update(id: &str) -> PackageRule {
        let mut rule = sample_rule();
        rule.id = id.to_owned();
        rule.name = format!("Updated {id}");
        rule.description = format!("Updated body for {id}.");
        rule.check_prompt = Some(format!("Did you update {id}?"));
        rule.file_patterns = vec!["src/**/*.rs".to_owned()];
        rule.path = None;
        rule.hash = rule_hash(&rule);
        rule
    }

    fn parsed_json_package(mut rules: Vec<PackageRule>) -> ParsedPackage {
        for rule in &mut rules {
            rule.path = None;
            rule.hash = rule_hash(rule);
        }
        let manifest = PackageManifest {
            package_id: "pkg-test".to_owned(),
            exported_at_utc: "2026-06-19T00:00:00Z".to_owned(),
            difflore_version: "0.2.0".to_owned(),
            format: PackageFormat::Json,
            repo_scopes: Vec::new(),
            rule_count: rules.len(),
            total_rules: rules.len(),
            truncated: false,
            hash_algorithm: HASH_ALGORITHM.to_owned(),
        };
        let hash = package_hash(&manifest, &rules);
        ParsedPackage {
            current_hash: hash.clone(),
            package: MemoryPackage {
                schema_version: PACKAGE_SCHEMA_VERSION.to_owned(),
                version: PACKAGE_VERSION,
                hash,
                manifest,
                rules,
            },
        }
    }

    async fn pool() -> difflore_core::SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("connect");
        sqlx::query(
            "CREATE TABLE skills (
                id TEXT PRIMARY KEY NOT NULL,
                name TEXT NOT NULL,
                description TEXT DEFAULT '' NOT NULL,
                check_prompt TEXT,
                file_patterns TEXT,
                source TEXT DEFAULT 'local' NOT NULL,
                source_repo TEXT,
                status TEXT DEFAULT 'active' NOT NULL,
                cloud_id TEXT,
                content_hash TEXT,
                hash_created_at INTEGER,
                updated_at TEXT DEFAULT (datetime('now')) NOT NULL
            )",
        )
        .execute(&pool)
        .await
        .expect("create skills");
        pool
    }

    async fn insert_skill(
        pool: &difflore_core::SqlitePool,
        id: &str,
        source: &str,
        source_repo: Option<&str>,
        status: &str,
        cloud_id: Option<&str>,
    ) {
        sqlx::query(
            "INSERT INTO skills
             (id, name, description, check_prompt, file_patterns, source, source_repo, status, cloud_id, content_hash, hash_created_at)
             VALUES (?1, ?2, ?3, NULL, '[]', ?4, ?5, ?6, ?7, 'old-hash', 1)",
        )
        .bind(id)
        .bind(format!("Old {id}"))
        .bind(format!("Old body for {id}."))
        .bind(source)
        .bind(source_repo)
        .bind(status)
        .bind(cloud_id)
        .execute(pool)
        .await
        .expect("insert rule");
    }

    #[test]
    fn markdown_target_rejects_non_empty_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("keep.txt"), "user file").expect("write");

        let err = ensure_markdown_target(dir.path(), false).expect_err("should reject");
        assert!(err.contains("non-empty directory"));
    }

    #[test]
    fn json_target_rejects_non_empty_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("memory.json");
        fs::write(&file, "{}").expect("write");

        let err = ensure_json_target(&file, false).expect_err("should reject");
        assert!(err.contains("non-empty package file"));
    }

    #[test]
    fn json_dry_run_allows_missing_parent_like_markdown_dry_run() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("missing-parent").join("memory.json");

        ensure_json_target(&target, true).expect("dry-run should preview missing parent");
        let err = ensure_json_target(&target, false).expect_err("real write should reject");
        assert!(err.contains("parent directory"));
    }

    #[test]
    fn markdown_package_round_trips_edited_title_and_body() {
        let package = sample_package(PackageFormat::Markdown);
        let rule = &package.rules[0];
        let mut markdown = render_rule_markdown(rule).expect("render");
        markdown = markdown.replace("# Prefer small modules", "# Prefer tiny modules");
        markdown = markdown.replace(
            "Split large modules before adding more exports.",
            "Split large modules before adding more public exports.",
        );

        let parsed =
            parse_rule_markdown(&markdown, "rules/001-prefer-small-modules.md").expect("parse");

        assert_eq!(parsed.name, "Prefer tiny modules");
        assert!(parsed.description.contains("public exports"));
        assert_ne!(parsed.hash, rule.hash);
    }

    #[test]
    fn markdown_metadata_escapes_comment_end_marker_without_losing_values() {
        let mut rule = sample_rule();
        rule.check_prompt = Some("Reject values containing --> in metadata".to_owned());
        rule.file_patterns = vec!["src/**/-->/*.rs".to_owned()];
        rule.hash = rule_hash(&rule);

        let markdown = render_rule_markdown(&rule).expect("render");

        assert_eq!(markdown.matches(MD_META_END).count(), 1);
        assert!(markdown.contains("--\\u003e"));
        let parsed =
            parse_rule_markdown(&markdown, "rules/001-prefer-small-modules.md").expect("parse");
        assert_eq!(parsed.check_prompt, rule.check_prompt);
        assert_eq!(parsed.file_patterns, rule.file_patterns);
    }

    #[test]
    fn package_relative_path_rejects_paths_outside_rules_dir() {
        let root = Path::new("package-root");

        assert!(package_relative_path(root, "rules/001-rule.md").is_ok());
        assert!(package_relative_path(root, "../secret.md").is_err());
        assert!(package_relative_path(root, "rules/../secret.md").is_err());
        assert!(package_relative_path(root, "/tmp/secret.md").is_err());
        assert!(package_relative_path(root, "other/001-rule.md").is_err());
    }

    #[test]
    fn markdown_dir_write_and_read_uses_edited_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let package = sample_package(PackageFormat::Markdown);
        write_markdown_dir(dir.path(), &package).expect("write package");
        let rule_path = dir.path().join("rules/001-prefer-small-modules.md");
        let edited = fs::read_to_string(&rule_path)
            .expect("read rule")
            .replace("# Prefer small modules", "# Prefer tiny modules");
        fs::write(&rule_path, edited).expect("edit rule");

        let parsed = read_markdown_package(dir.path()).expect("read package");

        assert_eq!(parsed.package.rules[0].name, "Prefer tiny modules");
        assert_ne!(parsed.current_hash, package.hash);
    }

    #[test]
    fn package_validation_requires_manifest_count_to_match_rules() {
        let mut package = sample_package(PackageFormat::Json);
        package.manifest.rule_count = 2;

        let err = validate_package(&package).expect_err("should reject");
        assert!(err.contains("ruleCount"));
    }

    #[tokio::test]
    async fn import_dry_run_reports_would_update_and_preserves_db() {
        let pool = pool().await;
        insert_skill(&pool, "rule-1", "local", None, "active", None).await;
        let rule = package_rule_for_update("rule-1");
        let existing = load_existing_rule(&pool, "rule-1")
            .await
            .expect("load")
            .expect("exists");
        let changed = changed_fields(&existing, &rule);
        assert_eq!(
            changed,
            vec!["name", "description", "checkPrompt", "filePatterns"]
        );

        let report = import_existing_rules_with_scopes(
            &pool,
            parsed_json_package(vec![rule]),
            PathBuf::from("package.json"),
            true,
            &[],
        )
        .await
        .expect("import dry-run");

        assert_eq!(report.updated, 1);
        assert_eq!(report.unchanged, 0);
        assert_eq!(report.rejected, 0);
        assert_eq!(report.operations[0].action, "would-update");
        let after = load_existing_rule(&pool, "rule-1")
            .await
            .expect("load")
            .expect("exists");
        assert_eq!(after.name, "Old rule-1");
        assert_eq!(after.description, "Old body for rule-1.");
    }

    #[tokio::test]
    async fn import_updates_only_active_current_repo_or_explicit_local_rules() {
        let pool = pool().await;
        for (id, source, source_repo, status, cloud_id) in [
            ("local-rule", "local", None, "active", None),
            ("repo-rule", "local", Some("acme/widgets"), "active", None),
            (
                "trimmed-repo-rule",
                "local",
                Some(" Acme/Widgets "),
                "active",
                None,
            ),
            ("pending-rule", "local", None, "pending", None),
            ("cloud-rule", "cloud", Some("acme/widgets"), "active", None),
            ("team-rule", "team", Some("acme/widgets"), "active", None),
            (
                "synced-local-rule",
                "local",
                None,
                "active",
                Some("cloud-rule-id"),
            ),
            (
                "other-repo-rule",
                "local",
                Some("other/repo"),
                "active",
                None,
            ),
        ] {
            insert_skill(&pool, id, source, source_repo, status, cloud_id).await;
        }
        let rules = [
            "local-rule",
            "repo-rule",
            "trimmed-repo-rule",
            "pending-rule",
            "cloud-rule",
            "team-rule",
            "synced-local-rule",
            "other-repo-rule",
            "missing-rule",
        ]
        .into_iter()
        .map(package_rule_for_update)
        .collect();

        let report = import_existing_rules_with_scopes(
            &pool,
            parsed_json_package(rules),
            PathBuf::from("package.json"),
            false,
            &["acme/widgets".to_owned()],
        )
        .await
        .expect("import");

        assert_eq!(report.updated, 3);
        assert_eq!(report.unchanged, 0);
        assert_eq!(report.missing, 1);
        assert_eq!(report.rejected, 5);
        let reason_for = |id: &str| {
            report
                .operations
                .iter()
                .find(|op| op.id == id)
                .and_then(|op| op.reason)
        };
        assert_eq!(reason_for("pending-rule"), Some("pending"));
        assert_eq!(reason_for("cloud-rule"), Some("cloud-synced"));
        assert_eq!(reason_for("team-rule"), Some("team-synced"));
        assert_eq!(reason_for("synced-local-rule"), Some("cloud-synced"));
        assert_eq!(reason_for("other-repo-rule"), Some("cross-repo"));

        let local = load_existing_rule(&pool, "local-rule")
            .await
            .expect("load")
            .expect("local exists");
        let repo = load_existing_rule(&pool, "repo-rule")
            .await
            .expect("load")
            .expect("repo exists");
        assert_eq!(local.name, "Updated local-rule");
        assert_eq!(repo.name, "Updated repo-rule");
        let repo_hash: Option<String> =
            sqlx::query_scalar("SELECT content_hash FROM skills WHERE id = 'repo-rule'")
                .fetch_one(&pool)
                .await
                .expect("load content hash");
        let expected_hash = difflore_core::skills::remember_rule_content_hash(
            &repo.name,
            &repo.description,
            Some(&parse_file_patterns(repo.file_patterns.as_deref())),
        );
        assert_eq!(repo_hash.as_deref(), Some(expected_hash.as_str()));
        let trimmed = load_existing_rule(&pool, "trimmed-repo-rule")
            .await
            .expect("load")
            .expect("trimmed repo exists");
        assert_eq!(trimmed.name, "Updated trimmed-repo-rule");
        assert_eq!(
            parse_file_patterns(repo.file_patterns.as_deref()),
            vec!["src/**/*.rs".to_owned()]
        );
        for protected in [
            "pending-rule",
            "cloud-rule",
            "team-rule",
            "synced-local-rule",
            "other-repo-rule",
        ] {
            let row = load_existing_rule(&pool, protected)
                .await
                .expect("load")
                .expect("protected exists");
            assert_eq!(row.name, format!("Old {protected}"));
        }
    }

    #[tokio::test]
    async fn update_existing_rule_sql_guard_refuses_protected_rows() {
        let pool = pool().await;
        insert_skill(
            &pool,
            "rule-1",
            "cloud",
            Some("acme/widgets"),
            "active",
            None,
        )
        .await;
        let rule = package_rule_for_update("rule-1");

        let mut tx = pool.begin().await.expect("begin");
        let err = update_existing_rule(&mut tx, &rule, &["acme/widgets".to_owned()])
            .await
            .expect_err("guarded update must refuse protected row");
        assert!(
            err.contains("no longer active/importable"),
            "unexpected error: {err}"
        );
        tx.rollback().await.expect("rollback");
        let row = load_existing_rule(&pool, "rule-1")
            .await
            .expect("load")
            .expect("exists");
        assert_eq!(row.name, "Old rule-1");
    }

    #[tokio::test]
    async fn import_rolls_back_earlier_updates_when_a_later_rule_aborts_the_batch() {
        // The non-dry-run import applies every per-rule UPDATE inside one
        // transaction, so a later guarded UPDATE that matches zero rows (e.g. a
        // row that was concurrently protected) must surface an error AND roll
        // back the earlier successful updates, rather than leaving the database
        // partially mutated.
        let pool = pool().await;
        insert_skill(&pool, "good-rule", "local", None, "active", None).await;
        insert_skill(
            &pool,
            "protected-rule",
            "cloud",
            Some("acme/widgets"),
            "active",
            None,
        )
        .await;

        let mut tx = pool.begin().await.expect("begin");
        update_existing_rule(&mut tx, &package_rule_for_update("good-rule"), &[])
            .await
            .expect("good update succeeds inside tx");
        let err = update_existing_rule(
            &mut tx,
            &package_rule_for_update("protected-rule"),
            &["acme/widgets".to_owned()],
        )
        .await
        .expect_err("guarded update on a protected row must fail");
        assert!(
            err.contains("no longer active/importable"),
            "unexpected error: {err}"
        );
        // Rolling back mirrors what `import_existing_rules_with_scopes` does
        // when the `?` early return drops the uncommitted transaction.
        tx.rollback().await.expect("rollback");

        let good = load_existing_rule(&pool, "good-rule")
            .await
            .expect("load")
            .expect("exists");
        assert_eq!(
            good.name, "Old good-rule",
            "earlier update must be rolled back when a later rule aborts the batch"
        );
    }
}
