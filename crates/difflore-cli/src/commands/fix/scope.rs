use anyhow::{Context, bail};

use difflore_core::domain::models::{DiffContentRecord, GitDiffInput};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum DiffScope {
    Staged,
    Worktree,
    All,
    PullRequest { label: String },
}

impl DiffScope {
    pub(super) const fn label(&self) -> &str {
        match self {
            Self::Staged => "staged changes",
            Self::Worktree => "working tree",
            Self::All => "staged + working tree",
            Self::PullRequest { label } => label.as_str(),
        }
    }

    pub(super) const fn should_sync_index_after_apply(&self) -> bool {
        matches!(self, Self::Staged)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RequestedScope {
    Auto,
    Staged,
    Worktree,
    All,
}

pub(super) fn parse_diff_scope(raw: Option<&str>) -> anyhow::Result<RequestedScope> {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(RequestedScope::Auto),
        Some(s) => match s.to_ascii_lowercase().as_str() {
            "staged" | "stage" | "index" => Ok(RequestedScope::Staged),
            "worktree" | "working" | "working-tree" => Ok(RequestedScope::Worktree),
            "all" | "both" => Ok(RequestedScope::All),
            other => {
                bail!("unknown --diff scope `{other}`; expected one of: staged, worktree, all")
            }
        },
    }
}

// Auto: staged first, fall back to worktree if index is clean.
// All: union of both, but apply must NOT mirror patches into the index. The
// union can contain unstaged files whose generated patch is worktree-relative,
// so index writes are only safe for an explicitly staged-only scope.
pub(super) async fn collect_diff(
    path: &std::path::Path,
    requested: RequestedScope,
) -> anyhow::Result<(Vec<DiffContentRecord>, DiffScope)> {
    let proj = path.to_string_lossy().to_string();
    let staged = || async {
        difflore_core::infra::git::diff(GitDiffInput {
            project_path: proj.clone(),
            staged: Some(true),
            ref1: None,
            ref2: None,
        })
        .await
        .context("Failed to get staged diff")
    };
    let worktree = || async {
        difflore_core::infra::git::diff(GitDiffInput {
            project_path: proj.clone(),
            staged: Some(false),
            ref1: None,
            ref2: None,
        })
        .await
        .context("Failed to get working-tree diff")
    };
    let all = || async {
        difflore_core::infra::git::diff(GitDiffInput {
            project_path: proj.clone(),
            staged: None,
            ref1: Some("HEAD".to_owned()),
            ref2: None,
        })
        .await
        .context("Failed to get staged + working-tree diff")
    };

    match requested {
        RequestedScope::Staged => Ok((staged().await?, DiffScope::Staged)),
        RequestedScope::Worktree => Ok((worktree().await?, DiffScope::Worktree)),
        RequestedScope::All => Ok((all().await?, DiffScope::All)),
        RequestedScope::Auto => {
            let s = staged().await?;
            if s.is_empty() {
                Ok((worktree().await?, DiffScope::Worktree))
            } else {
                Ok((s, DiffScope::Staged))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_diff_scope_accepts_all_aliases() {
        assert_eq!(parse_diff_scope(None).unwrap(), RequestedScope::Auto);
        assert_eq!(
            parse_diff_scope(Some("staged")).unwrap(),
            RequestedScope::Staged
        );
        assert_eq!(
            parse_diff_scope(Some("working-tree")).unwrap(),
            RequestedScope::Worktree
        );
        assert_eq!(parse_diff_scope(Some("both")).unwrap(), RequestedScope::All);
    }

    #[test]
    fn all_scope_is_not_treated_as_index_sync_safe() {
        assert_eq!(DiffScope::All.label(), "staged + working tree");
        assert!(!DiffScope::All.should_sync_index_after_apply());
        assert!(DiffScope::Staged.should_sync_index_after_apply());
        assert!(!DiffScope::Worktree.should_sync_index_after_apply());
    }

    #[tokio::test]
    async fn all_scope_includes_staged_and_unstaged_changes_in_the_same_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        git(repo, &["init"]);
        git(repo, &["config", "user.email", "test@example.com"]);
        git(repo, &["config", "user.name", "DiffLore Test"]);
        std::fs::write(repo.join("file.txt"), "base\n").expect("write base");
        git(repo, &["add", "file.txt"]);
        git(repo, &["commit", "-m", "base"]);

        std::fs::write(repo.join("file.txt"), "base\nstaged\n").expect("write staged");
        git(repo, &["add", "file.txt"]);
        std::fs::write(repo.join("file.txt"), "base\nstaged\nunstaged\n").expect("write unstaged");

        let (records, scope) = collect_diff(repo, RequestedScope::All)
            .await
            .expect("collect all diff");
        assert_eq!(scope, DiffScope::All);
        let diff_text = records
            .iter()
            .flat_map(|record| &record.hunks)
            .map(|hunk| hunk.body.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            diff_text.contains("+staged"),
            "all diff should include staged change: {diff_text}"
        );
        assert!(
            diff_text.contains("+unstaged"),
            "all diff should include unstaged change: {diff_text}"
        );
    }

    fn git(repo: &std::path::Path, args: &[&str]) {
        let output = difflore_core::infra::git::git_command(repo)
            .args(args)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {:?} failed\nstdout:\n{}\nstderr:\n{}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
