//! Rule ingestion surface: where rules come from.
//!
//! * [`github`] — import PR review threads via the GitHub API.
//! * [`agent_files`] — detect + read cross-vendor agent memory / rule files.

pub mod agent_files;
pub mod github;
