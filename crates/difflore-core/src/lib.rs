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
pub mod ingest;
pub mod mcp_server;
pub mod migration;
pub mod observability;
pub mod packs;
pub mod review_engine;
pub mod review_store;
pub mod skills;
pub mod team;

// Public API compatibility re-exports: modules grouped into subdirectories,
// but the old flat top-level paths (e.g. `difflore_core::models`,
// `difflore_core::db`) must continue to resolve for the workspace and
// downstream consumers.

// domain/*
pub use domain::errors;
pub use domain::models;
pub use domain::origins;
pub use domain::projects;
pub use domain::providers;
pub use domain::rule_view;

// observability/*
pub use observability::activity_stream;
pub use observability::classifier as observation;
pub use observability::cost;
pub use observability::fix_outcomes;
pub use observability::injection_log;
pub use observability::mcp_rule_serves;
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
pub use infra::files;
pub use infra::git;
pub use infra::paths;
pub use infra::settings;
pub use infra::startup;

// Renamed/relocated modules: old names kept resolving until the re-export
// cleanup batch removes them.
pub use ingest::agent_files as sources;
pub use ingest::github as github_import;
pub use review_engine as review;
pub use review_store as reviews;
pub use skills::fs as skill_fs;

pub use errors::{CoreError, Result};
pub use sqlx::SqlitePool;
