//! Platform hook adapter layer.
//!
//! Each supported AI coding client (Claude Code, Cursor, Zed, …) expects
//! lifecycle hooks to speak its own JSON dialect on stdin/stdout. This
//! module defines the `PlatformAdapter` trait every client implementation
//! conforms to, plus the `get_platform_adapter` dispatcher that the CLI
//! uses to look up the right adapter by name in the `difflore-hook` shim
//! time.
//!
//! The CLI's job is thin: read stdin, hand it to the adapter, get a
//! normalised `HookEvent`, run `DiffLore` logic, hand the `HookResult`
//! back to the adapter to get platform-specific JSON, write to stdout.
//! Any per-platform quirk lives *inside* the adapter — the CLI stays
//! platform-agnostic.

pub mod claude_code;
pub mod cursor;
pub mod gemini_cli;
// Since-last-session recap banner used by the `SessionStart` dispatch arm.
pub mod session_banner;
pub(crate) mod synth;
pub mod types;
pub mod windsurf;

/// Static, generic half of an adapter — owns the raw payload type,
/// the human-readable label used in parse-error messages, and the
/// canonical-event mapping. `PlatformAdapter` (above) stays
/// object-safe for the `Box<dyn PlatformAdapter>` dispatch site;
/// this trait carries the type-level pieces (associated types,
/// associated consts) the dispatcher doesn't need.
///
/// Adapters implement BOTH traits: `PlatformAdapter` for runtime
/// dispatch, `PayloadAdapter` for the parse pipeline. The two are
/// glued together by `PlatformAdapter::parse_stdin` delegating to
/// `Self::parse_stdin_default`.
pub(crate) trait PayloadAdapter {
    /// Strongly-typed view of the IDE's stdin envelope. Each adapter
    /// keeps its own per-IDE struct (the wire shapes diverge enough
    /// that a one-size struct would be a constant grab-bag of
    /// `Option<Value>`s).
    type Raw: serde::de::DeserializeOwned;

    /// Used in the "invalid <label> hook JSON" parse-error message.
    /// Keeps each adapter's wording self-explanatory in logs.
    const PARSE_LABEL: &'static str;

    /// Map a parsed `Raw` into the canonical `HookEvent`. Adapters
    /// are responsible for: validating the discriminator field,
    /// dispatching by event name, and pulling per-event payload
    /// fields out of the (often loosely-typed) `Raw`.
    fn into_canonical(raw: Self::Raw) -> Result<types::HookEvent, String>;

    /// Default `parse_stdin` body: trim, deserialize into `Raw`,
    /// hand off to `into_canonical`. Adapters' `PlatformAdapter::
    /// parse_stdin` impls delegate here so the boilerplate lives in
    /// exactly one place.
    fn parse_stdin_default(raw: &str) -> Result<types::HookEvent, String> {
        let payload: Self::Raw = serde_json::from_str(raw.trim())
            .map_err(|e| format!("invalid {} hook JSON: {e}", Self::PARSE_LABEL))?;
        Self::into_canonical(payload)
    }
}

/// Contract every platform adapter implements. The trait is object-safe
/// on purpose so `get_platform_adapter` can return a `Box<dyn
/// PlatformAdapter>` and the dispatch site doesn't need to know the
/// concrete type at compile time. That makes adding a new client (say
/// Cursor) a pure module-level addition — no changes to the CLI dispatch
/// loop beyond the `get_platform_adapter` match arm.
pub trait PlatformAdapter: Send + Sync {
    /// Stable identifier used in logs + telemetry. Must match the string
    /// `get_platform_adapter` dispatches on so `adapter.name() ==
    /// requested_name` round-trips.
    fn name(&self) -> &'static str;

    /// Parse a single hook invocation's stdin payload into our canonical
    /// `HookEvent`. Adapters SHOULD be permissive about unknown fields
    /// (clients evolve faster than we can ship adapter updates) and
    /// strict only about the tiny subset they actually need.
    ///
    /// On unrecognised / unsupported events, return `Err` with a human-
    /// readable reason. The CLI logs the error and no-ops — hooks must
    /// never block the user workflow, even on malformed input.
    fn parse_stdin(&self, raw: &str) -> Result<types::HookEvent, String>;

    /// Format a `HookResult` as the exact JSON the client expects on
    /// stdout. Returns a complete, newline-free string; the caller
    /// prints it + a trailing newline. Formatting is infallible because
    /// `HookResult` is a fixed shape we control.
    fn format_output(&self, result: types::HookResult) -> String;

    /// Bucket an error produced by the hook's core work so the CLI can
    /// pick an exit code (see `main.rs` `Hook::Run`). Default walks the
    /// `anyhow` error chain for transport-ish hints (io kinds, reqwest
    /// connect/timeout, HTTP 5xx) vs client-ish hints (HTTP 4xx, serde
    /// parse failures). Adapters can override when their transport layer
    /// carries richer context than the default sniffer can see.
    fn classify_error(&self, err: &anyhow::Error) -> types::ErrorClass {
        default_classify_error(err)
    }
}

/// Default error classifier shared by every adapter. Kept as a free
/// function (not an inherent method) so unit tests don't need to
/// construct a concrete adapter to exercise it.
pub fn default_classify_error(err: &anyhow::Error) -> types::ErrorClass {
    use types::ErrorClass;

    for cause in err.chain() {
        // reqwest: connection refused / timeout / DNS resolution failure
        // all surface through these two predicates. HTTP status is
        // checked separately — `is_connect` and `is_timeout` return
        // false on a 5xx response body.
        if let Some(re) = cause.downcast_ref::<reqwest::Error>() {
            if re.is_timeout() || re.is_connect() {
                return ErrorClass::Transport;
            }
            if let Some(status) = re.status() {
                if status.is_server_error() {
                    return ErrorClass::Transport;
                }
                // Retryable 4xx subset that semantically belongs in
                // Transport, not Client — the assistant session must
                // not block on them. 429 (Too Many Requests) and 408
                // (Request Timeout) are infrastructure-level signals
                // ("wait + retry"), same as a 5xx or DNS failure.
                // See memory `project_error_path_actionable_playbook.md`
                // — `format_cloud_err` already routes 429 through the
                // transport-style hint; classifier should match.
                if status.as_u16() == 429 || status.as_u16() == 408 {
                    return ErrorClass::Transport;
                }
                if status.is_client_error() {
                    return ErrorClass::Client;
                }
            }
        }

        // std::io: connection refused by the listener, socket half
        // closed mid-request, kernel timeout. All transport-class.
        if let Some(io) = cause.downcast_ref::<std::io::Error>() {
            use std::io::ErrorKind::{ConnectionRefused, ConnectionReset, NotConnected, TimedOut};
            if matches!(
                io.kind(),
                ConnectionRefused | TimedOut | ConnectionReset | NotConnected
            ) {
                return ErrorClass::Transport;
            }
        }

        // serde: a parse failure means the other side sent us something
        // malformed. Not our transport's fault — surface so the parser
        // (ours or theirs) gets fixed.
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

        // No downcast hit → conservative Fatal default.
        let err = anyhow::anyhow!("something exploded");
        assert_eq!(default_classify_error(&err), ErrorClass::Fatal);
    }

    #[test]
    fn wrapped_io_transport_still_classifies_through_context() {
        // Production callers almost always add `.context("...")` before
        // the error escapes. Chain-walking must still spot the io kind.
        let root: anyhow::Error =
            std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "down").into();
        let wrapped = root
            .context("fetch relevant rules")
            .context("hook dispatch");
        assert_eq!(default_classify_error(&wrapped), ErrorClass::Transport);
    }
}

/// Dispatch by client name. Unknown names fall through to the
/// Claude Code adapter as the pragmatic default — almost every
/// `DiffLore` user today is on Claude Code, and a wrong-but-compatible
/// parse fails loudly (via `parse_stdin`) while a panic would kill the
/// user's whole assistant session.
///
/// Accepted aliases: `"claude-code"` / `"claude_code"` / `"claude"` all
/// map to the Claude Code adapter so env-var typos don't silently
/// reach a `Cursor`/`Zed` codepath that doesn't yet exist.
pub fn get_platform_adapter(client_name: &str) -> Box<dyn PlatformAdapter> {
    // Match case-insensitively + ignoring separator style ("gemini-cli"
    // vs "gemini_cli") so env-var typos and different casing conventions
    // in hook configs across tools all route to the right adapter.
    let normalized = client_name.to_ascii_lowercase();
    match normalized.as_str() {
        "cursor" => Box::new(cursor::CursorAdapter),
        "gemini-cli" | "gemini_cli" | "gemini" => Box::new(gemini_cli::GeminiCliAdapter),
        "windsurf" => Box::new(windsurf::WindsurfAdapter),
        // "claude-code"/"claude_code"/"claude" plus any unknown name
        // deliberately route to Claude Code: see module docs.
        _ => Box::new(claude_code::ClaudeCodeAdapter),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_routes_aliases_and_unknown_falls_back_to_claude_code() {
        // Aliases (separator + casing variants) must all route correctly.
        // Unknown names fall back to Claude Code rather than panic — see
        // `get_platform_adapter` doc.
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
