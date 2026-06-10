pub mod glob_match;
pub mod models;
pub mod origins;
pub mod projects;
pub mod providers;
pub mod rule_view;

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
    // Cloud embedding cap hit; callers fall back to lexical retrieval for the
    // offending embed call. `cap` is the tier ceiling, `used` the current count
    // from the cloud's response.
    #[error("Embedding cap reached: {used}/{cap}")]
    EmbedCapReached { cap: u32, used: u32 },
}

pub type Result<T> = std::result::Result<T, CoreError>;

// Re-export so `crate::errors::*` paths keep working.
pub mod errors {
    pub use super::{CoreError, Result};
}
