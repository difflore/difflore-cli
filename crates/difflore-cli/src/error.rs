use std::path::PathBuf;
use thiserror::Error;

use difflore_core::CoreError;

#[derive(Error, Debug)]
pub enum CliError {
    #[error("{0}")]
    Core(#[from] CoreError),

    #[error("{0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Json(#[from] serde_json::Error),

    #[error("{0}")]
    Sqlx(#[from] sqlx::Error),

    #[error("{0}")]
    Message(String),

    #[error("config at {path}: {message}")]
    Config { path: PathBuf, message: String },
}

impl CliError {
    pub fn msg<S: Into<String>>(s: S) -> Self {
        Self::Message(s.into())
    }
}

impl From<String> for CliError {
    fn from(s: String) -> Self {
        Self::Message(s)
    }
}

impl From<&str> for CliError {
    fn from(s: &str) -> Self {
        Self::Message(s.to_owned())
    }
}

pub type CliResult<T> = Result<T, CliError>;
