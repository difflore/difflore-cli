use difflore_core::domain::CoreError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TuiError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Core(#[from] CoreError),
    #[error("{0}")]
    NotTty(String),
}

pub type Result<T> = std::result::Result<T, TuiError>;
