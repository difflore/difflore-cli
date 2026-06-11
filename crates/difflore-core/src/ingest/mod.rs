//! Rule ingestion surface: where rules come from.
//!
//! * [`github`] — import PR review threads via the GitHub API.
//! * [`gitlab`] — GitLab PAT auth storage (REST import client lands later).
//! * [`provider`] — review-provider identity + provider-aware remote detection.
//! * [`common`] — provider-neutral comment metadata / durability signal.
//! * [`agent_files`] — detect + read cross-vendor agent memory / rule files.

pub mod agent_files;
pub(crate) mod common;
pub mod github;
pub mod gitlab;
pub mod provider;
