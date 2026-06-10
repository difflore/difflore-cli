//! MCP server tests.
//!
//! Split out of the former monolithic `mcp_server/tests.rs` (R4) along its
//! pre-existing inline-`mod` seams — a pure positional move, no test logic
//! changed. Each submodule keeps the `#[allow(test scaffolding)]` lint
//! relaxations it carried inline. Path resolution is unchanged: a submodule's
//! `super::super::*` still resolves to the `mcp_server` module exactly as it
//! did when these were nested `mod` blocks inside `tests.rs`.

#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::needless_pass_by_value,
    reason = "test scaffolding"
)]
mod remember_tool_tests;

#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::needless_pass_by_value,
    reason = "test scaffolding"
)]
mod resource_uri_tests;

#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::needless_pass_by_value,
    reason = "test scaffolding"
)]
mod git_remote_tests;

#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::needless_pass_by_value,
    reason = "test scaffolding"
)]
mod kill_switch_tests;

#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::needless_pass_by_value,
    reason = "test scaffolding"
)]
mod plan_pr_tests;
