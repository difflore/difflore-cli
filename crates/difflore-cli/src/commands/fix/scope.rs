use anyhow::{Context, bail};

use difflore_core::models::{DiffContentRecord, GitDiffInput};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum DiffScope {
    Staged,
    Worktree,
    PullRequest { label: String },
}

impl DiffScope {
    pub(super) const fn label(&self) -> &str {
        match self {
            Self::Staged => "staged changes",
            Self::Worktree => "working tree",
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
// All: union of both, labelled Staged to keep the index-sync apply path engaged.
pub(super) async fn collect_diff(
    path: &std::path::Path,
    requested: RequestedScope,
) -> anyhow::Result<(Vec<DiffContentRecord>, DiffScope)> {
    let proj = path.to_string_lossy().to_string();
    let staged = || async {
        difflore_core::git::diff(GitDiffInput {
            project_path: proj.clone(),
            staged: Some(true),
            ref1: None,
            ref2: None,
        })
        .await
        .context("Failed to get staged diff")
    };
    let worktree = || async {
        difflore_core::git::diff(GitDiffInput {
            project_path: proj.clone(),
            staged: Some(false),
            ref1: None,
            ref2: None,
        })
        .await
        .context("Failed to get working-tree diff")
    };

    match requested {
        RequestedScope::Staged => Ok((staged().await?, DiffScope::Staged)),
        RequestedScope::Worktree => Ok((worktree().await?, DiffScope::Worktree)),
        RequestedScope::All => {
            let mut combined = staged().await?;
            let extra = worktree().await?;
            let seen: std::collections::HashSet<String> =
                combined.iter().map(|r| r.file_path.clone()).collect();
            for record in extra {
                if !seen.contains(&record.file_path) {
                    combined.push(record);
                }
            }
            Ok((combined, DiffScope::Staged))
        }
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
