use std::path::{Path, PathBuf};

use crate::hook::forward;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct HookProjectContext {
    pub primary_file: Option<String>,
    pub recall_file: String,
    pub repo_root: Option<PathBuf>,
    pub project_hash: Option<String>,
    pub repo_scopes: Vec<String>,
    pub reason: &'static str,
}

pub(super) async fn resolve_hook_project_context(
    cwd: Option<&str>,
    target_files: &[String],
) -> HookProjectContext {
    let base = cwd.and_then(non_empty_path).or_else(current_dir_ok);
    let candidates = ordered_non_empty(target_files);

    for raw in &candidates {
        let abs = absolutize(base.as_deref(), raw);
        if let Some(root) = git_root_for_path(&abs) {
            return context_for_root(Some(&abs), root, "target_file_git_root");
        }
    }

    if let Some(base) = base
        && let Some(root) = git_root_for_path(&base)
    {
        let primary = candidates.first().map(|raw| absolutize(Some(&base), raw));
        return context_for_root(primary.as_deref(), root, "cwd_git_root");
    }

    let primary = candidates
        .first()
        .map(|raw| absolutize(None, raw))
        .map(|path| path_to_string(&path));
    HookProjectContext {
        recall_file: primary.clone().unwrap_or_else(|| "unknown".to_owned()),
        primary_file: primary,
        repo_root: None,
        project_hash: None,
        repo_scopes: Vec::new(),
        reason: "no_git_root",
    }
}

pub(super) async fn index_pool_for_project_context(
    hot_state: Option<&forward::State>,
    project_hash: Option<&str>,
) -> Result<difflore_core::SqlitePool, difflore_core::error::CoreError> {
    if let (Some(state), Some(hash)) = (hot_state, project_hash) {
        if state.project_hash == hash {
            return Ok(state.index_pool.clone());
        }
        return difflore_core::context::index_db::get_pool_for_project(hash).await;
    }

    if let Some(state) = hot_state {
        return Ok(state.index_pool.clone());
    }

    if let Some(hash) = project_hash {
        return difflore_core::context::index_db::get_pool_for_project(hash).await;
    }

    difflore_core::context::index_db::get_pool_for_cwd().await
}

pub(super) async fn refresh_repo_scopes(context: &mut HookProjectContext) {
    let Some(repo_root) = context.repo_root.as_ref() else {
        return;
    };
    let configured_gitlab_hosts = difflore_core::ingest::gitlab::auth::configured_hosts().await;
    let repo_root = path_to_string(repo_root);
    // git remote detection forks git; run it off the async worker thread so a
    // slow (AV-throttled) spawn doesn't stall other concurrent hook/MCP work.
    let repo_scopes = tokio::task::spawn_blocking(move || {
        difflore_core::infra::git::detect_repo_full_names_with_gitlab_hosts(
            &repo_root,
            &configured_gitlab_hosts,
        )
    })
    .await
    .unwrap_or_default();
    context.repo_scopes = repo_scopes;
}

pub(super) fn git_repo_context_for_file(
    cwd: Option<&str>,
    file_path: &str,
) -> Option<(PathBuf, String)> {
    let base = cwd.and_then(non_empty_path).or_else(current_dir_ok);
    let abs = absolutize(base.as_deref(), file_path);
    let root = git_root_for_path(&abs)?;
    let rel = repo_relative_path(&abs, &root)?;
    Some((root, rel))
}

fn ordered_non_empty(values: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for value in values {
        let trimmed = value.trim();
        if trimmed.is_empty() || out.iter().any(|existing| existing == trimmed) {
            continue;
        }
        out.push(trimmed.to_owned());
    }
    out
}

fn context_for_root(
    primary_abs: Option<&Path>,
    repo_root: PathBuf,
    reason: &'static str,
) -> HookProjectContext {
    let project_hash = difflore_core::infra::db::project_hash_from_root(&repo_root);
    let repo_scopes = difflore_core::infra::git::detect_repo_full_names_with_gitlab_hosts(
        &path_to_string(&repo_root),
        &[],
    );
    let recall_file = primary_abs
        .and_then(|path| repo_relative_path(path, &repo_root))
        .or_else(|| primary_abs.map(path_to_string))
        .unwrap_or_else(|| "unknown".to_owned());
    HookProjectContext {
        primary_file: primary_abs.map(path_to_string),
        recall_file,
        repo_root: Some(repo_root),
        project_hash: Some(project_hash),
        repo_scopes,
        reason,
    }
}

fn non_empty_path(value: &str) -> Option<PathBuf> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| PathBuf::from(trimmed))
}

fn current_dir_ok() -> Option<PathBuf> {
    std::env::current_dir().ok()
}

fn absolutize(base: Option<&Path>, path: &str) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else if let Some(base) = base {
        base.join(path)
    } else if let Ok(cwd) = std::env::current_dir() {
        cwd.join(path)
    } else {
        path
    }
}

fn git_root_for_path(path: &Path) -> Option<PathBuf> {
    let probe = existing_probe_dir(path)?;
    // Route through the core no-window git builder so this hook-path probe does
    // not flash a transient console window on Windows.
    let output = difflore_core::infra::git::git_command(&probe)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let root = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!root.is_empty()).then(|| PathBuf::from(root))
}

fn existing_probe_dir(path: &Path) -> Option<PathBuf> {
    let mut probe = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent()?.to_path_buf()
    };
    loop {
        if probe.exists() {
            return Some(probe);
        }
        if !probe.pop() {
            return None;
        }
    }
}

fn repo_relative_path(path: &Path, repo_root: &Path) -> Option<String> {
    if let Ok(rel) = path.strip_prefix(repo_root)
        && !rel.as_os_str().is_empty()
    {
        return Some(path_to_string(rel));
    }

    let probe = existing_probe_dir(path)?;
    let probe_canonical = std::fs::canonicalize(&probe).ok()?;
    let root_canonical = std::fs::canonicalize(repo_root).ok()?;
    let rel_dir = probe_canonical.strip_prefix(root_canonical).ok()?;
    let suffix = path.strip_prefix(&probe).unwrap_or_else(|_| Path::new(""));
    let rel = rel_dir.join(suffix);
    (!rel.as_os_str().is_empty()).then(|| path_to_string(&rel))
}

fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resolves_child_git_repo_from_target_file_when_cwd_is_parent() {
        let temp = tempfile::tempdir().expect("tempdir");
        let parent = temp.path();
        let repo = parent.join("child");
        std::fs::create_dir_all(repo.join("src")).expect("repo dirs");
        run_git(&repo, &["init"]);
        run_git(
            &repo,
            &["remote", "add", "origin", "git@github.com:acme/widgets.git"],
        );
        let target = repo.join("src").join("foo.ts");
        let ctx =
            resolve_hook_project_context(Some(&path_to_string(parent)), &[path_to_string(&target)])
                .await;

        let actual_repo = ctx.repo_root.as_deref().expect("repo root");
        assert_same_canonical_path(actual_repo, &repo);
        let expected_hash = difflore_core::infra::db::project_hash_from_root(actual_repo);
        assert_eq!(ctx.project_hash.as_deref(), Some(expected_hash.as_str()));
        assert_eq!(ctx.repo_scopes, vec!["acme/widgets"]);
        assert_eq!(ctx.recall_file, "src/foo.ts");
        assert_eq!(ctx.reason, "target_file_git_root");
    }

    #[tokio::test]
    async fn resolves_relative_target_file_inside_child_repo() {
        let temp = tempfile::tempdir().expect("tempdir");
        let parent = temp.path();
        let repo = parent.join("child");
        std::fs::create_dir_all(repo.join("src")).expect("repo dirs");
        run_git(&repo, &["init"]);
        run_git(
            &repo,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/acme/widgets.git",
            ],
        );
        let ctx = resolve_hook_project_context(
            Some(&path_to_string(parent)),
            &["child/src/foo.ts".to_owned()],
        )
        .await;

        assert_same_canonical_path(ctx.repo_root.as_deref().expect("repo root"), &repo);
        assert_eq!(ctx.repo_scopes, vec!["acme/widgets"]);
        assert_eq!(ctx.recall_file, "src/foo.ts");
    }

    #[tokio::test]
    async fn resolves_sibling_target_repo_even_when_cwd_is_another_repo() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo_a = temp.path().join("repo-a");
        let repo_b = temp.path().join("repo-b");
        std::fs::create_dir_all(&repo_a).expect("repo a dir");
        std::fs::create_dir_all(repo_b.join("src")).expect("repo b dirs");
        run_git(&repo_a, &["init"]);
        run_git(&repo_b, &["init"]);
        run_git(
            &repo_b,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/acme/widgets.git",
            ],
        );

        let target = repo_b.join("src").join("foo.ts");
        let ctx = resolve_hook_project_context(
            Some(&path_to_string(&repo_a)),
            &[path_to_string(&target)],
        )
        .await;

        assert_same_canonical_path(ctx.repo_root.as_deref().expect("repo root"), &repo_b);
        assert_eq!(ctx.repo_scopes, vec!["acme/widgets"]);
        assert_eq!(ctx.recall_file, "src/foo.ts");
        assert_eq!(ctx.reason, "target_file_git_root");
    }

    #[tokio::test]
    async fn refresh_repo_scopes_allows_authoritative_empty_result() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("repo dir");
        run_git(&repo, &["init"]);
        let mut ctx = HookProjectContext {
            primary_file: None,
            recall_file: "unknown".to_owned(),
            repo_root: Some(repo),
            project_hash: Some("stale-project".to_owned()),
            repo_scopes: vec!["stale/remote".to_owned()],
            reason: "target_file_git_root",
        };

        refresh_repo_scopes(&mut ctx).await;

        assert!(ctx.repo_scopes.is_empty());
    }

    fn assert_same_canonical_path(actual: &Path, expected: &Path) {
        let actual = std::fs::canonicalize(actual).expect("canonical actual");
        let expected = std::fs::canonicalize(expected).expect("canonical expected");
        assert_eq!(actual, expected);
    }

    fn run_git(cwd: &Path, args: &[&str]) {
        let status = difflore_core::infra::git::git_command(cwd)
            .args(args)
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed");
    }
}
