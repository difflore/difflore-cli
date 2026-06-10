//! Platform hook adapter layer.
//!
//! Each supported AI coding client (Claude Code, Cursor, Zed, …) speaks its own
//! JSON dialect for lifecycle hooks. This module defines the `PlatformAdapter`
//! trait every client implements, plus the `get_platform_adapter` dispatcher
//! that looks up the right adapter by name.
//!
//! The CLI's job is thin: read stdin, hand it to the adapter, get a normalised
//! `HookEvent`, run `DiffLore` logic, hand the `HookResult` back for
//! platform-specific JSON, write to stdout. Per-platform quirks live inside the
//! adapter.

pub mod claude_code;
pub mod cursor;
pub mod gemini_cli;
pub(crate) mod synth;
pub mod types;
pub mod windsurf;

/// Static, generic half of an adapter: owns the raw payload type, the label
/// used in parse-error messages, and the canonical-event mapping. Carries the
/// type-level pieces (associated types/consts) that `PlatformAdapter` keeps off
/// itself to stay object-safe for `Box<dyn PlatformAdapter>` dispatch.
///
/// Adapters implement both traits; `PlatformAdapter::parse_stdin` delegates to
/// `Self::parse_stdin_default`.
pub(crate) trait PayloadAdapter {
    /// Strongly-typed view of the IDE's stdin envelope. Each adapter keeps its
    /// own per-IDE struct since the wire shapes diverge.
    type Raw: serde::de::DeserializeOwned;

    /// Used in the "invalid <label> hook JSON" parse-error message.
    const PARSE_LABEL: &'static str;

    /// Map a parsed `Raw` into the canonical `HookEvent`: validate the
    /// discriminator field, dispatch by event name, and pull per-event payload
    /// fields out of the (often loosely-typed) `Raw`.
    fn into_canonical(raw: Self::Raw) -> Result<types::HookEvent, String>;

    /// Default `parse_stdin` body: trim, deserialize into `Raw`, hand off to
    /// `into_canonical`.
    fn parse_stdin_default(raw: &str) -> Result<types::HookEvent, String> {
        let payload: Self::Raw = serde_json::from_str(raw.trim())
            .map_err(|e| format!("invalid {} hook JSON: {e}", Self::PARSE_LABEL))?;
        Self::into_canonical(payload)
    }
}

/// Contract every platform adapter implements. Object-safe so
/// `get_platform_adapter` can return a `Box<dyn PlatformAdapter>` and adding a
/// new client is a module-level addition plus one `get_platform_adapter` arm.
pub trait PlatformAdapter: Send + Sync {
    /// Stable identifier used in logs + telemetry. Must match the string
    /// `get_platform_adapter` dispatches on.
    fn name(&self) -> &'static str;

    /// Parse a hook invocation's stdin payload into the canonical `HookEvent`.
    /// Adapters SHOULD be permissive about unknown fields (clients evolve faster
    /// than adapter updates) and strict only about the subset they need.
    ///
    /// On unrecognised / unsupported events, return `Err` with a human-readable
    /// reason. The CLI logs it and no-ops — hooks must never block the user
    /// workflow, even on malformed input.
    fn parse_stdin(&self, raw: &str) -> Result<types::HookEvent, String>;

    /// Format a `HookResult` as the exact JSON the client expects on stdout.
    /// Returns a complete, newline-free string; the caller adds the newline.
    fn format_output(&self, result: types::HookResult) -> String;

    /// Bucket an error from the hook's core work so the CLI can pick an exit
    /// code. Default walks the `anyhow` chain for transport-ish hints (io kinds,
    /// reqwest connect/timeout, HTTP 5xx) vs client-ish hints (HTTP 4xx, serde
    /// parse failures). Adapters override when their transport layer carries
    /// richer context than the default sniffer can see.
    fn classify_error(&self, err: &anyhow::Error) -> types::ErrorClass {
        default_classify_error(err)
    }
}

/// Default error classifier shared by every adapter. A free function so tests
/// don't need a concrete adapter to exercise it.
pub fn default_classify_error(err: &anyhow::Error) -> types::ErrorClass {
    use types::ErrorClass;

    for cause in err.chain() {
        // reqwest: connection refused / timeout / DNS failure surface through
        // these two predicates. HTTP status is checked separately — `is_connect`
        // and `is_timeout` return false on a 5xx response body.
        if let Some(re) = cause.downcast_ref::<reqwest::Error>() {
            if re.is_timeout() || re.is_connect() {
                return ErrorClass::Transport;
            }
            if let Some(status) = re.status() {
                if status.is_server_error() {
                    return ErrorClass::Transport;
                }
                // 429 (Too Many Requests) and 408 (Request Timeout) are
                // "wait + retry" signals, so they belong in Transport, not
                // Client — matching how `format_cloud_err` routes 429.
                if status.as_u16() == 429 || status.as_u16() == 408 {
                    return ErrorClass::Transport;
                }
                if status.is_client_error() {
                    return ErrorClass::Client;
                }
            }
        }

        // std::io: connection refused, socket half-closed mid-request, kernel
        // timeout — all transport-class.
        if let Some(io) = cause.downcast_ref::<std::io::Error>() {
            use std::io::ErrorKind::{ConnectionRefused, ConnectionReset, NotConnected, TimedOut};
            if matches!(
                io.kind(),
                ConnectionRefused | TimedOut | ConnectionReset | NotConnected
            ) {
                return ErrorClass::Transport;
            }
        }

        // serde: a parse failure means the other side sent malformed input —
        // surface it so the parser gets fixed, not retried as transport.
        if cause.downcast_ref::<serde_json::Error>().is_some() {
            return ErrorClass::Client;
        }
    }

    ErrorClass::Fatal
}

#[cfg(test)]
mod classifier_tests {
    use super::*;
    use types::ErrorClass;

    #[test]
    fn io_kinds_map_to_expected_class() {
        use std::io::ErrorKind;
        let cases: &[(ErrorKind, ErrorClass)] = &[
            (ErrorKind::ConnectionRefused, ErrorClass::Transport),
            (ErrorKind::TimedOut, ErrorClass::Transport),
            (ErrorKind::ConnectionReset, ErrorClass::Transport),
            (ErrorKind::NotConnected, ErrorClass::Transport),
            // PermissionDenied is NOT in the transport allow-list — must
            // fall through to Fatal so real bugs aren't silently retried.
            (ErrorKind::PermissionDenied, ErrorClass::Fatal),
        ];
        for (kind, want) in cases {
            let err: anyhow::Error = std::io::Error::new(*kind, "x").into();
            assert_eq!(default_classify_error(&err), *want, "for {kind:?}");
        }
    }

    #[test]
    fn serde_parse_error_is_client_and_plain_anyhow_is_fatal() {
        // Malformed JSON → Client (caller's bug, surface it).
        let parse_err = serde_json::from_str::<serde_json::Value>("{not json").unwrap_err();
        let err: anyhow::Error = parse_err.into();
        assert_eq!(default_classify_error(&err), ErrorClass::Client);

        // No downcast hit → Fatal default.
        let err = anyhow::anyhow!("something exploded");
        assert_eq!(default_classify_error(&err), ErrorClass::Fatal);
    }

    #[test]
    fn wrapped_io_transport_still_classifies_through_context() {
        // Callers add `.context(...)` before the error escapes; chain-walking
        // must still spot the io kind.
        let root: anyhow::Error =
            std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "down").into();
        let wrapped = root
            .context("fetch relevant rules")
            .context("hook dispatch");
        assert_eq!(default_classify_error(&wrapped), ErrorClass::Transport);
    }
}

/// Dispatch by client name. Unknown names fall through to the Claude Code
/// adapter: most users are on Claude Code, and a wrong-but-compatible parse
/// fails loudly via `parse_stdin` while a panic would kill the assistant
/// session. The `"claude-code"`/`"claude_code"`/`"claude"` aliases all map to it.
pub fn get_platform_adapter(client_name: &str) -> Box<dyn PlatformAdapter> {
    // Match case-insensitively and ignoring separator style so env-var typos and
    // casing differences across hook configs all route correctly.
    let normalized = client_name.to_ascii_lowercase();
    match normalized.as_str() {
        "cursor" => Box::new(cursor::CursorAdapter),
        "gemini-cli" | "gemini_cli" | "gemini" => Box::new(gemini_cli::GeminiCliAdapter),
        "windsurf" => Box::new(windsurf::WindsurfAdapter),
        // claude aliases plus any unknown name route to Claude Code.
        _ => Box::new(claude_code::ClaudeCodeAdapter),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_routes_aliases_and_unknown_falls_back_to_claude_code() {
        let cases: &[(&str, &str)] = &[
            ("claude-code", "claude-code"),
            ("claude_code", "claude-code"),
            ("claude", "claude-code"),
            ("cursor", "cursor"),
            ("Cursor", "cursor"),
            ("gemini-cli", "gemini-cli"),
            ("gemini_cli", "gemini-cli"),
            ("gemini", "gemini-cli"),
            ("Gemini-CLI", "gemini-cli"),
            ("windsurf", "windsurf"),
            ("Windsurf", "windsurf"),
            ("definitely-not-a-real-client", "claude-code"),
        ];
        for (input, want) in cases {
            assert_eq!(
                get_platform_adapter(input).name(),
                *want,
                "alias {input} misrouted"
            );
        }
    }
}
