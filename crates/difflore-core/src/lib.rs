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
pub mod contract;
pub mod domain;
pub mod error;
pub mod export;
pub mod hook_signal;
pub mod infra;
pub mod ingest;
pub mod mcp_server;
pub mod memory_autopilot;
pub mod memory_autopilot_schedule;
pub mod memory_curator;
pub mod memory_inbox;
pub mod memory_overview;
pub mod migration;
pub mod observability;
pub mod repo_aliases;
pub mod review_engine;
pub mod review_store;
pub mod skills;
pub mod team;

pub use error::{CoreError, Result};
pub use sqlx::SqlitePool;
