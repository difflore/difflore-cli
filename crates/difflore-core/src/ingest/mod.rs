//! Rule ingestion surface: where rules come from.
//!
//! * [`github`] — import PR review threads via the GitHub API.
//! * [`gitlab`] — import MR discussions via the GitLab REST API (+ PAT auth).
//! * [`provider`] — review-provider identity + provider-aware remote detection.
//! * [`common`] — provider-neutral comment metadata / durability signal.
//! * [`agent_files`] — detect + read cross-vendor agent memory / rule files.

pub mod agent_files;
pub(crate) mod common;
pub mod github;
pub mod gitlab;
pub mod provider;

/// Progress counters shared by the review importers (GitHub PRs, GitLab MRs).
/// Field names say "PR" because the GitHub importer landed first; the GitLab
/// importer reuses them 1:1 with MR semantics (a "PR number" is the MR IID).
pub struct ImportProgress {
    /// PRs/MRs we've finished processing (regardless of whether they had
    /// content).
    pub prs_fetched: usize,
    /// PRs/MRs with at least one review comment or non-empty top-level
    /// review. This is the number the progress bar divides by — a repo with
    /// mostly empty PRs won't spam the user with `0 comments imported` lines.
    pub prs_total: usize,
    pub comments_imported: usize,
    pub comments_skipped: usize,
    /// PRs/MRs that were requested by number but not found / inaccessible
    /// (deleted, private, or never existed). Only populated by the
    /// direct-number query paths.
    pub prs_missing: usize,
    /// Exact requested numbers the provider returned as missing/inaccessible.
    pub missing_pr_numbers: Vec<i32>,
}

pub type ProgressCallback = Box<dyn Fn(&ImportProgress) + Send>;

pub(crate) fn validate_since_date(since: &str) -> crate::Result<()> {
    chrono::NaiveDate::parse_from_str(since, "%Y-%m-%d")
        .map(|_| ())
        .map_err(|_| {
            crate::error::CoreError::Internal(format!(
                "invalid --since date {since:?}: expected YYYY-MM-DD"
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::validate_since_date;

    #[test]
    fn since_date_accepts_only_iso_calendar_dates() {
        assert!(validate_since_date("2026-01-15").is_ok());
        assert!(validate_since_date("2026-02-30").is_err());
        assert!(validate_since_date("2026-01-15 sort:updated-desc").is_err());
        assert!(validate_since_date("2026-01-15T00:00:00Z").is_err());
    }
}
