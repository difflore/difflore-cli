//! Telemetry capture kill-switch.
//!
//! `DIFFLORE_CAPTURE=false` stops local fire-and-forget rows from
//! entering any cloud outbox. Every capture-emitting entry point checks
//! this gate before opening pools or building payloads.
//!
//! Both cloud queues must honor the gate:
//! - [`crate::cloud::outbox::OutboxQueue::enqueue`] — the original
//!   `cloud_outbox` SQLite queue.
//! - [`crate::cloud::observations::storage::ObservationEmitter::enqueue`]
//!   — the second `observation_events` queue added for PostToolUse
//!   observation capture.
//!
//! When disabled, no row enters either queue and later drain passes have
//! nothing to upload.

/// Env var documented as the telemetry capture kill-switch.
pub const DIFFLORE_CAPTURE_ENV: &str = "DIFFLORE_CAPTURE";

/// Whether telemetry capture is enabled.
///
/// Only the exact lowercase string `"false"` disables capture; unset
/// and every other value leave it enabled.
#[must_use]
pub fn capture_enabled() -> bool {
    std::env::var(DIFFLORE_CAPTURE_ENV).as_deref() != Ok("false")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_enabled_returns_true_when_unset() {
        temp_env::with_var(DIFFLORE_CAPTURE_ENV, None::<&str>, || {
            assert!(capture_enabled(), "unset env must leave capture enabled");
        });
    }

    #[test]
    fn capture_enabled_returns_true_when_set_to_other_values() {
        // A typo must not silently disable capture.
        for value in ["true", "1", "", "FALSE", "False", "no", "off", " false"] {
            temp_env::with_var(DIFFLORE_CAPTURE_ENV, Some(value), || {
                assert!(
                    capture_enabled(),
                    "value {value:?} must not disable capture (only exact lowercase \"false\" does)",
                );
            });
        }
    }

    #[test]
    fn capture_enabled_returns_false_only_for_exact_lowercase_false() {
        temp_env::with_var(DIFFLORE_CAPTURE_ENV, Some("false"), || {
            assert!(
                !capture_enabled(),
                "exact lowercase \"false\" must disable capture",
            );
        });
    }
}
