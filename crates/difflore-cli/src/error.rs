use std::path::PathBuf;
use thiserror::Error;

use difflore_core::CoreError;

#[derive(Error, Debug)]
pub enum CliError {
    // Single canonical wrapper for crate-level errors. `sqlx::Error`,
    // `std::io::Error`, and `serde_json::Error` reach `CliError` through
    // `CoreError`'s own `#[from]` arms (e.g. `CoreError::Database`), so each
    // underlying error has one representation and one Display, rather than a
    // second bare-`{0}` path competing with the contextual Core rendering.
    #[error("{0}")]
    Core(#[from] CoreError),

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

#[cfg(test)]
mod tests {
    use super::*;

    // An underlying `std::io::Error` reaches `CliError` through exactly one
    // path — `CoreError::Io` via `?` — so its Display carries the canonical
    // "IO error: ..." context, never a competing bare `{0}` rendering.
    #[test]
    fn underlying_io_error_funnels_through_core_with_context() {
        fn fails() -> CliResult<()> {
            Err(CoreError::Io(std::io::Error::other("disk full")))?;
            Ok(())
        }

        let err = fails().expect_err("expected error");
        assert!(matches!(err, CliError::Core(CoreError::Io(_))));
        assert_eq!(err.to_string(), "IO error: disk full");
    }
}
