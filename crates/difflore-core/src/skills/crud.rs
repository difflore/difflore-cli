use std::collections::{HashMap, HashSet};

use crate::domain::models::{
    DiscoverSkillsInput, DiscoveredSkillRecord, InstallSkillInput, RemoveSkillInput, SkillRecord,
    ToggleSkillEngineInput,
};
use crate::error::CoreError;

use super::{SkillRow, decode_base64_lossy, parse_skill_frontmatter};

/// Map `skill_id → source_repo` for every active rule. Lets the TUI
/// default the rules tab to the current repo without widening the stable
/// `SkillRecord` serde surface.
pub async fn list_source_repos(
    db: &sqlx::SqlitePool,
) -> crate::Result<HashMap<String, Option<String>>> {
    let rows = sqlx::query!("SELECT id, source_repo FROM skills WHERE status = 'active'")
        .fetch_all(db)
        .await?;
    let mut out = HashMap::with_capacity(rows.len());
    for row in rows {
        out.insert(row.id, row.source_repo);
    }
    Ok(out)
}

/// Expand git remotes with a conservative source-repo alias.
///
/// A fork's local `origin` (e.g. `difflore-fixtures/fastapi`) may differ from
/// the upstream repo the imported memory is scoped to (e.g. `fastapi/fastapi`).
/// When exactly one active source repo shares the same repository name, add it
/// as an extra recall scope. With zero or multiple candidates, keep only the
/// original remotes so unrelated repos never receive global memory.
pub async fn expand_repo_scopes_with_source_aliases(
    db: &sqlx::SqlitePool,
    repo_full_names: &[String],
) -> crate::Result<Vec<String>> {
    let mut scopes = Vec::new();
    let mut seen = HashSet::new();
    let mut repo_names = Vec::new();

    for raw in repo_full_names {
        let Some(repo) = crate::infra::git::normalize_github_repo_full_name(raw) else {
            continue;
        };
        if seen.insert(repo.clone()) {
            if let Some((_, name)) = repo.rsplit_once('/')
                && !repo_names.iter().any(|existing| existing == name)
            {
                repo_names.push(name.to_owned());
            }
            scopes.push(repo);
        }
    }

    for repo_name in repo_names {
        let pattern = format!("%/{repo_name}");
        let rows: Vec<String> = sqlx::query_scalar(
            "SELECT LOWER(source_repo)
             FROM skills
             WHERE status = 'active'
               AND source_repo IS NOT NULL
               AND TRIM(source_repo) <> ''
               AND LOWER(source_repo) LIKE ?1
             GROUP BY LOWER(source_repo)",
        )
        .bind(pattern)
        .fetch_all(db)
        .await?;

        let candidates: Vec<String> = rows
            .into_iter()
            .filter_map(|repo| crate::infra::git::normalize_github_repo_full_name(&repo))
            .filter(|repo| {
                repo.rsplit_once('/')
                    .is_some_and(|(_, name)| name == repo_name)
                    && !seen.contains(repo)
            })
            .collect();

        if candidates.len() == 1 {
            let Some(alias) = candidates.into_iter().next() else {
                continue;
            };
            seen.insert(alias.clone());
            scopes.push(alias);
        }
    }

    Ok(scopes)
}

pub async fn list(db: &sqlx::SqlitePool) -> crate::Result<Vec<SkillRecord>> {
    let rows = sqlx::query_as!(
        SkillRow,
        "SELECT id, name, source, directory, version, description, type, \
         engines, tags, trigger, check_prompt, repo_owner, repo_name, repo_branch, readme_url, \
         enabled_for_codex, enabled_for_claude, enabled_for_gemini, enabled_for_cursor, \
         installed_at, updated_at, origin FROM skills WHERE status = 'active' \
         ORDER BY installed_at DESC",
    )
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(SkillRecord::from).collect())
}

/// Like `list()` but without the `status='active'` filter, so callers can
/// see pending rows alongside active ones.
pub async fn list_all(db: &sqlx::SqlitePool) -> crate::Result<Vec<SkillRecord>> {
    let rows = sqlx::query_as!(
        SkillRow,
        "SELECT id, name, source, directory, version, description, type, \
         engines, tags, trigger, check_prompt, repo_owner, repo_name, repo_branch, readme_url, \
         enabled_for_codex, enabled_for_claude, enabled_for_gemini, enabled_for_cursor, \
         installed_at, updated_at, origin FROM skills ORDER BY installed_at DESC",
    )
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(SkillRecord::from).collect())
}

pub async fn add(db: &sqlx::SqlitePool, input: InstallSkillInput) -> crate::Result<SkillRecord> {
    let id = format!("skill-{}-{}", input.owner, input.directory);
    let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let engines_json = serde_json::to_string(&["claude"])?;
    let tags_json = serde_json::to_string(&["github", "imported"])?;
    let name = input.directory.replace('-', " ");
    let description = format!("Imported from {}/{}", input.owner, input.repo);

    sqlx::query!(
        "INSERT OR IGNORE INTO skills
         (id, name, source, directory, version, description, type, engines, tags,
          repo_owner, repo_name, repo_branch, enabled_for_claude, installed_at, updated_at)
         VALUES (?1, ?2, 'github', ?3, '1.0.0', ?4, 'skill', ?5, ?6,
                 ?7, ?8, ?9, 1, ?10, ?10)",
        id,
        name,
        input.directory,
        description,
        engines_json,
        tags_json,
        input.owner,
        input.repo,
        input.branch,
        now
    )
    .execute(db)
    .await?;

    let row = sqlx::query_as!(
        SkillRow,
        "SELECT id, name, source, directory, version, description, type, \
         engines, tags, trigger, check_prompt, repo_owner, repo_name, repo_branch, readme_url, \
         enabled_for_codex, enabled_for_claude, enabled_for_gemini, enabled_for_cursor, \
         installed_at, updated_at, origin FROM skills WHERE id = ?1",
        id
    )
    .fetch_one(db)
    .await?;
    Ok(SkillRecord::from(row))
}

pub async fn remove(db: &sqlx::SqlitePool, input: RemoveSkillInput) -> crate::Result<()> {
    let skill: Option<SkillRecord> = sqlx::query_as!(
        SkillRow,
        "SELECT id, name, source, directory, version, description, type, \
             engines, tags, trigger, check_prompt, repo_owner, repo_name, repo_branch, readme_url, \
             enabled_for_codex, enabled_for_claude, enabled_for_gemini, enabled_for_cursor, \
             installed_at, updated_at, origin FROM skills WHERE id = ?1",
        input.id
    )
    .fetch_optional(db)
    .await?
    .map(SkillRecord::from);

    // Fail loud on an unknown id; otherwise a typo returns a phantom
    // "Removed rule: X" that's impossible to debug.
    let Some(skill) = skill else {
        return Err(CoreError::NotFound(format!(
            "rule '{}' not found. Inspect local memory with `difflore status --json`.",
            input.id
        )));
    };

    sqlx::query!("DELETE FROM skills WHERE id = ?1", input.id)
        .execute(db)
        .await?;

    {
        for engine in &["codex", "claude", "gemini", "cursor"] {
            if let Err(e) =
                crate::skills::fs::sync_engine_link(&skill.source, &skill.directory, engine, false)
            {
                eprintln!("warning: sync_engine_link failed for engine {engine}: {e}");
            }
        }
        let skill_dir = crate::skills::fs::skills_base_dir()
            .map_err(CoreError::Internal)?
            .join(&skill.source)
            .join(&skill.directory);
        if skill_dir.exists() {
            let _ = std::fs::remove_dir_all(&skill_dir);
        }
    }

    Ok(())
}

pub async fn toggle_engine(
    db: &sqlx::SqlitePool,
    input: ToggleSkillEngineInput,
) -> crate::Result<()> {
    let val = i32::from(input.enabled);
    match input.engine.as_str() {
        "codex" => {
            sqlx::query!("UPDATE skills SET enabled_for_codex = ?1, updated_at = datetime('now') WHERE id = ?2",
                val, input.id).execute(db).await?;
        }
        "claude" => {
            sqlx::query!("UPDATE skills SET enabled_for_claude = ?1, updated_at = datetime('now') WHERE id = ?2",
                val, input.id).execute(db).await?;
        }
        "gemini" => {
            sqlx::query!("UPDATE skills SET enabled_for_gemini = ?1, updated_at = datetime('now') WHERE id = ?2",
                val, input.id).execute(db).await?;
        }
        "cursor" => {
            sqlx::query!("UPDATE skills SET enabled_for_cursor = ?1, updated_at = datetime('now') WHERE id = ?2",
                val, input.id).execute(db).await?;
        }
        other => return Err(CoreError::Internal(format!("unknown engine: {other}"))),
    }

    if let Some(row) = sqlx::query_as!(
        SkillRow,
        "SELECT id, name, source, directory, version, description, type, \
         engines, tags, trigger, check_prompt, repo_owner, repo_name, repo_branch, readme_url, \
         enabled_for_codex, enabled_for_claude, enabled_for_gemini, enabled_for_cursor, \
         installed_at, updated_at, origin FROM skills WHERE id = ?1",
        input.id
    )
    .fetch_optional(db)
    .await?
    {
        let skill = SkillRecord::from(row);
        if let Err(e) = crate::skills::fs::sync_engine_link(
            &skill.source,
            &skill.directory,
            &input.engine,
            input.enabled,
        ) {
            eprintln!(
                "warning: sync_engine_link failed for engine {}: {e}",
                input.engine
            );
        }
    }

    Ok(())
}

pub async fn discover(
    db: &sqlx::SqlitePool,
    input: DiscoverSkillsInput,
) -> crate::Result<Vec<DiscoveredSkillRecord>> {
    let branch = input.branch.unwrap_or_else(|| "main".into());
    let repo_slug = format!("{}/{}", input.owner, input.repo);

    let dirs: Vec<String> = if which::which("gh").is_ok() {
        let output = std::process::Command::new("gh")
            .args([
                "api",
                &format!("repos/{repo_slug}/contents"),
                "--jq",
                ".[].name",
            ])
            .output();

        match output {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(|l| l.trim().to_owned())
                .filter(|l| !l.is_empty() && !l.starts_with('.'))
                .collect(),
            _ => vec![],
        }
    } else {
        vec![]
    };

    let installed_ids: Vec<String> =
        sqlx::query_scalar!("SELECT id FROM skills WHERE source = 'github'")
            .fetch_all(db)
            .await?;

    if dirs.is_empty() {
        return Ok(vec![DiscoveredSkillRecord {
            name: format!("{} skills", input.repo),
            description: format!("Skills from {repo_slug}"),
            r#type: "skill".into(),
            engines: vec!["claude".into()],
            tags: vec!["remote".into()],
            version: "1.0.0".into(),
            directory: input.repo.clone(),
            repo_owner: input.owner.clone(),
            repo_name: input.repo,
            repo_branch: branch,
            installed: installed_ids.iter().any(|id| id.contains(&input.owner)),
        }]);
    }

    let mut results = Vec::new();
    for dir in dirs {
        let skill_id = format!("skill-{}-{}", input.owner, dir);
        let installed = installed_ids.contains(&skill_id);

        let (name, description, fm) = if which::which("gh").is_ok() {
            let md_output = std::process::Command::new("gh")
                .args([
                    "api",
                    &format!("repos/{repo_slug}/contents/{dir}/SKILL.md"),
                    "--jq",
                    ".content",
                ])
                .output();

            match md_output {
                Ok(o) if o.status.success() => {
                    let raw = String::from_utf8_lossy(&o.stdout).trim().to_owned();
                    let decoded = decode_base64_lossy(&raw);
                    let fm = parse_skill_frontmatter(&decoded);
                    let first_line = fm
                        .body
                        .lines()
                        .find(|l| !l.trim().is_empty())
                        .unwrap_or(&dir)
                        .trim_start_matches('#')
                        .trim()
                        .to_owned();
                    let desc = fm
                        .body
                        .lines()
                        .skip(1)
                        .find(|l| !l.trim().is_empty())
                        .unwrap_or("Remote skill")
                        .trim()
                        .to_owned();
                    (first_line, desc, fm)
                }
                _ => (
                    dir.replace('-', " "),
                    format!("Skill from {repo_slug}/{dir}"),
                    parse_skill_frontmatter(""),
                ),
            }
        } else {
            (
                dir.replace('-', " "),
                format!("Skill from {repo_slug}/{dir}"),
                parse_skill_frontmatter(""),
            )
        };

        results.push(DiscoveredSkillRecord {
            name,
            description,
            r#type: fm.r#type.unwrap_or_else(|| "skill".into()),
            engines: fm.engines.unwrap_or_else(|| vec!["claude".into()]),
            tags: fm
                .tags
                .unwrap_or_else(|| vec!["remote".into(), "github".into()]),
            version: fm.version.unwrap_or_else(|| "1.0.0".into()),
            directory: dir,
            repo_owner: input.owner.clone(),
            repo_name: input.repo.clone(),
            repo_branch: branch.clone(),
            installed,
        });
    }

    Ok(results)
}

pub async fn sync_links(db: &sqlx::SqlitePool) -> crate::Result<()> {
    let rows = sqlx::query_as!(
        SkillRow,
        "SELECT id, name, source, directory, version, description, type, \
         engines, tags, trigger, check_prompt, repo_owner, repo_name, repo_branch, readme_url, \
         enabled_for_codex, enabled_for_claude, enabled_for_gemini, enabled_for_cursor, \
         installed_at, updated_at, origin FROM skills WHERE status = 'active'",
    )
    .fetch_all(db)
    .await?;
    let skills: Vec<SkillRecord> = rows.into_iter().map(SkillRecord::from).collect();

    for skill in &skills {
        let engines = [
            ("codex", skill.enabled_for_codex),
            ("claude", skill.enabled_for_claude),
            ("gemini", skill.enabled_for_gemini),
            ("cursor", skill.enabled_for_cursor),
        ];
        for (engine, enabled) in engines {
            if let Err(e) = crate::skills::fs::sync_engine_link(
                &skill.source,
                &skill.directory,
                engine,
                enabled,
            ) {
                eprintln!("warning: sync_engine_link failed for engine {engine}: {e}");
            }
        }
    }

    Ok(())
}
