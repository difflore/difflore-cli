//! Crate-level error kernel.
//!
//! Layering: `error` sits above the contract layer (it converts
//! `openapi_contract::ApiError`) and below everything else — any module may
//! depend on `crate::error`, but `error` must not depend on crate modules.

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

pub type Result<T, E = CoreError> = std::result::Result<T, E>;

impl From<String> for CoreError {
    fn from(value: String) -> Self {
        Self::Internal(value)
    }
}

impl From<&str> for CoreError {
    fn from(value: &str) -> Self {
        Self::Internal(value.to_owned())
    }
}

/// Render an error together with its `source()` chain (deduplicated against
/// text the message already carries).
///
/// Transport libraries hide the actionable classification in the chain:
/// reqwest's `Display` for a connect failure is just
/// `error sending request for url (...)`, while the part user-facing error
/// mappers key on — `dns error`, `certificate verify failed`,
/// `connection refused` — only appears in the nested sources.
#[must_use]
pub fn error_chain_text(e: &(dyn std::error::Error + 'static)) -> String {
    let mut message = e.to_string();
    let mut source = e.source();
    while let Some(cause) = source {
        let cause_text = cause.to_string();
        // thiserror-style wrappers ("X error: {0}") already embed the cause
        // text; appending it again would stutter.
        if !message.contains(&cause_text) {
            message.push_str(": ");
            message.push_str(&cause_text);
        }
        source = cause.source();
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Error, Debug)]
    #[error("error sending request for url (https://x.example/)")]
    struct Outer {
        #[source]
        cause: Middle,
    }

    #[derive(Error, Debug)]
    #[error("client error (Connect)")]
    struct Middle {
        #[source]
        cause: std::io::Error,
    }

    #[test]
    fn error_chain_text_appends_hidden_sources() {
        let outer = Outer {
            cause: Middle {
                cause: std::io::Error::other("dns error: failed to lookup address"),
            },
        };
        let text = error_chain_text(&outer);
        assert_eq!(
            text,
            "error sending request for url (https://x.example/): client error (Connect): \
             dns error: failed to lookup address"
        );
    }

    #[test]
    fn error_chain_text_skips_causes_already_embedded() {
        // `CoreError::Io` interpolates the cause into its own Display; the
        // chain renderer must not repeat it.
        let wrapped = CoreError::Io(std::io::Error::other("disk full"));
        assert_eq!(error_chain_text(&wrapped), "IO error: disk full");
    }
}
