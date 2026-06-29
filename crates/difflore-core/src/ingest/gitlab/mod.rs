//! GitLab review-import surface.
//!
//! * [`auth`] — PAT storage + token resolution (`difflore auth gitlab`).
//! * [`import_mr_reviews`] — REST import of merged-MR discussions into the
//!   provider-neutral review store ([`client`] fetches, [`parse`] persists).
//!
//! REST + PAT instead of `glab` on purpose: self-managed enterprise
//! instances routinely forbid extra CLI installs, while HTTPS with a
//! `read_api` PAT passes IT policy everywhere.

use std::collections::HashSet;

use sqlx::SqlitePool;

use crate::error::CoreError;
use crate::ingest::{ImportProgress, ProgressCallback};

pub mod auth;
mod client;
mod parse;
mod schema;

use client::GitlabClient;
use schema::{DiffNode, Discussion, MergeRequest};

/// Options for one GitLab MR-review import run. Mirrors the GitHub
/// `ImportOptions` shape minus fork-flow fields (no `--from-upstream` in
/// GitLab v1) and plus the instance host (self-managed support).
pub struct ImportOptions {
    /// Instance host, optionally with port (`gitlab.com`,
    /// `gitlab.corp.example:8443`). Stored per-item under the `gitlabHost`
    /// metadata key; `repo_full_name` stays the bare namespace path.
    pub host: String,
    /// Full namespace path (`group/project` or `group/subgroup/project`).
    pub project_path: String,
    pub project_id: String,
    /// PAT with at least `read_api`, resolved by
    /// [`auth::resolve_token`] before the import starts.
    pub token: String,
    pub max_mrs: usize,
    /// Specific MR IIDs to import (the `--pr` flag in GitLab context).
    /// Empty → list merged MRs newest-updated first.
    pub mr_iids: Vec<i32>,
    /// MR IIDs that must contribute zero rules (leak-free recall eval),
    /// dropped before their notes become candidates.
    pub exclude_mrs: HashSet<i32>,
    /// `YYYY-MM-DD`; pushed server-side as `updated_after` (GitLab's MR list
    /// has no merged-at filter, so the window keys on update time).
    pub since: Option<String>,
}

/// Import merged-MR discussions into the local review store.
///
/// Flow per run: list (or directly fetch) MRs → fetch each MR's discussions
/// → drop MRs with no human notes → persist via the same
/// `ensure_item`/`add_comment` calls the GitHub importer uses.
pub async fn import_mr_reviews(
    db: &SqlitePool,
    opts: ImportOptions,
    on_progress: Option<ProgressCallback>,
) -> Result<ImportProgress, CoreError> {
    crate::ingest::provider::validate_gitlab_project_path(&opts.project_path)?;
    if let Some(since) = opts.since.as_deref() {
        crate::ingest::validate_since_date(since)?;
    }
    let client = GitlabClient::new(&opts.host, &opts.token)?;

    let mut progress = ImportProgress {
        prs_fetched: 0,
        prs_total: 0,
        comments_imported: 0,
        comments_skipped: 0,
        prs_missing: 0,
        missing_pr_numbers: Vec::new(),
    };

    let merge_requests = if opts.mr_iids.is_empty() {
        let updated_after = opts.since.as_deref().map(parse::updated_after_param);
        let listed = client
            .list_merged_merge_requests(&opts.project_path, updated_after.as_deref(), opts.max_mrs)
            .await?;
        // Leak-free eval: drop excluded MRs BEFORE their notes are fetched so
        // an excluded MR contributes zero rules.
        listed
            .into_iter()
            .filter(|mr| !iid_excluded(mr.iid, &opts.exclude_mrs))
            .collect()
    } else {
        let mut collected: Vec<MergeRequest> = Vec::new();
        let mut seen = HashSet::new();
        for iid in &opts.mr_iids {
            if !seen.insert(*iid) {
                continue;
            }
            // Honor `--exclude-prs` in the direct-IID path too: skip the
            // fetch entirely so an excluded MR neither contributes rules nor
            // counts as missing.
            if opts.exclude_mrs.contains(iid) {
                continue;
            }
            if let Some(mr) = client.get_merge_request(&opts.project_path, *iid).await? {
                collected.push(mr);
            } else {
                progress.prs_missing += 1;
                progress.missing_pr_numbers.push(*iid);
            }
        }
        collected
    };

    // Fetch discussions and drop content-free MRs (approval-only, system
    // notes only) so progress stays honest — mirrors the GitHub importer's
    // client-side emptiness filter.
    let mut with_discussions: Vec<(MergeRequest, Vec<Discussion>, Vec<DiffNode>)> = Vec::new();
    for mr in merge_requests {
        let discussions = client.list_discussions(&opts.project_path, mr.iid).await?;
        if parse::has_importable_notes(&discussions) {
            let diffs = client.list_diffs(&opts.project_path, mr.iid).await?;
            with_discussions.push((mr, discussions, diffs));
        }
    }

    progress.prs_total = with_discussions.len();
    if let Some(ref cb) = on_progress {
        cb(&progress);
    }

    for (mr, discussions, diffs) in &with_discussions {
        parse::persist_merge_request(db, &opts, mr, discussions, diffs, &mut progress).await?;
        progress.prs_fetched += 1;
        if let Some(ref cb) = on_progress {
            cb(&progress);
        }
    }

    Ok(progress)
}

/// Preflight `GET /api/v4/projects/:id`: one cheap request that surfaces
/// auth/visibility problems with a precise status before any import work.
pub async fn verify_project_access(
    host: &str,
    token: &str,
    project_path: &str,
) -> Result<(), CoreError> {
    crate::ingest::provider::validate_gitlab_project_path(project_path)?;
    let client = GitlabClient::new(host, token)?;
    client.check_project_access(project_path).await
}

fn iid_excluded(iid: i64, exclude: &HashSet<i32>) -> bool {
    exclude.iter().any(|excluded| i64::from(*excluded) == iid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iid_exclusion_compares_across_integer_widths() {
        let exclude: HashSet<i32> = std::iter::once(42).collect();
        assert!(iid_excluded(42, &exclude));
        assert!(!iid_excluded(43, &exclude));
        // A wire iid beyond i32 can never match an i32 exclusion.
        assert!(!iid_excluded(i64::from(i32::MAX) + 1, &exclude));
    }
}
