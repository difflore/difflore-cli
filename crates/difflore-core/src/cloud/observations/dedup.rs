use super::events::ObservationEvent;
use crate::domain::glob_match::{GlobErrorPolicy, glob_match};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;

pub const RECENT_RULE_FIRE_WINDOW_MS: i64 = 60_000;

pub fn event_content_hash(event: &ObservationEvent) -> String {
    let payload = serde_json::to_string(event)
        .unwrap_or_else(|error| format!("serialization-error:{error};event:{event:?}"));
    let digest = Sha256::digest(payload.as_bytes());
    let mut out = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

/// Whether `file_path` is in scope for a rule whose `file_patterns_json`
/// is a JSON glob array. Absent/empty/`[]` patterns are universal.
/// Malformed JSON or an unbuildable glob set drops the rule, so
/// attribution never credits an unproven match.
pub(super) fn file_patterns_match(file_patterns_json: Option<&str>, file_path: &str) -> bool {
    glob_match(file_patterns_json, file_path, GlobErrorPolicy::Drop)
}
