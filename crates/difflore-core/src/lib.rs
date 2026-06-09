#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::many_single_char_names
    )
)]

pub mod cloud;
pub mod context;
pub mod domain;
pub mod infra;
pub mod mcp_server;
pub mod migration;
pub mod observability;
pub mod packs;
pub mod review;
pub mod reviews;
pub mod skills;
pub mod sources;
pub mod team;

// ---------------------------------------------------------------------------
// Public API compatibility re-exports.
//
// The previous layout exposed every domain/observability/infra module as a
// flat top-level path (e.g. `difflore_core::models`, `difflore_core::db`).
// Files have been grouped into subdirectories for organisation, but the old
// import paths must continue to resolve for the rest of the workspace and
// downstream consumers.
// ---------------------------------------------------------------------------

// domain/*
pub use domain::errors;
pub use domain::files;
pub use domain::models;
pub use domain::origins;
pub use domain::projects;
pub use domain::providers;
pub use domain::rule_view;
pub use domain::settings;

// observability/*
pub use observability::activity_stream;
pub use observability::cost;
pub use observability::fix_outcomes;
pub use observability::injection_log;
pub use observability::mcp_rule_serves;
pub use observability::observation;
pub use observability::privacy;
pub use observability::rule_outcomes;
pub use observability::stated_vs_actual;
pub use observability::trajectory as review_trajectory;

// infra/*
pub use infra::config;
pub use infra::crypto;
pub use infra::daemon;
pub use infra::db;
pub use infra::env;
pub use infra::git;
pub use infra::github_import;
pub use infra::paths;
pub use infra::skill_fs;
pub use infra::startup;

pub use errors::{CoreError, Result};
pub use sqlx::SqlitePool;
