pub mod files;
pub mod glob_match;
pub mod models;
pub mod origins;
pub mod projects;
pub mod providers;
pub mod rule_view;
pub mod settings;

use thiserror::Error;

#[derive(Error, Debug)]
pub enum CoreError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Validation error: {0}")]
    Validation(String),
    #[error("Not found: {0}")]
    NotFound(String),
    #[error("Internal error: {0}")]
    Internal(String),
    #[error("API error: {0}")]
    Api(#[from] openapi_contract::ApiError),
    // Cloud-managed embedding cap hit. Free tier reaches its rule cap;
    // callers fall back to lexical retrieval for the offending embed call
    // so recall keeps functioning. `cap` is the user's tier ceiling (e.g.
    // 200); `used` is the current count from the cloud's response.
    #[error("Embedding cap reached: {used}/{cap}")]
    EmbedCapReached { cap: u32, used: u32 },
}

pub type Result<T> = std::result::Result<T, CoreError>;

// Backwards-compatible re-export so `crate::errors::*` paths keep working.
pub mod errors {
    pub use super::{CoreError, Result};
}
