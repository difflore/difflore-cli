//! `PostToolUse` observation classifier.
//!
//! [`classify`] turns an Edit / `MultiEdit` / Write hook event into a
//! structured [`Observation`] for the cloud rule-promoter.
//!
//! Classification is deterministic and keyword-driven (no LLM call):
//!
//!   * `Write` of a brand-new file ⇒ `feature`
//!   * Edit that strips a visible `FIXME` / `BUG` / `TODO` ⇒ `bugfix`
//!   * Edit where the diff is whitespace-only ⇒ `refactor`
//!   * Anything else ⇒ `change`
//!
//! Privacy guard: edits touching secret-bearing paths (`.env*`,
//! `*.secrets*`, `*.key`, `*.pem`, `id_rsa*`, `credentials*`) are
//! dropped *before* classification and cannot be opted into — these
//! files must never leave the local machine.

use sha2::{Digest, Sha256};

pub use crate::contract::{Observation, ObservationScope};
use crate::observability::privacy::strip_private_tagged_regions;

/// Borrowed input payload for [`classify`].
#[derive(Debug, Clone, Copy)]
pub struct ClassifyInput<'a> {
    /// Tool name: `"Edit" | "MultiEdit" | "Write"`. Any other tool
    /// returns `None`.
    pub tool: &'a str,
    /// Target file path. `None` short-circuits the classifier.
    pub file_path: Option<&'a str>,
    /// Adapter-synthesised diff (`-old\n+new\n` lines). Used for
    /// whitespace-only detection.
    pub diff: Option<&'a str>,
    /// Post-edit text (`new_string` / content).
    pub new_text: Option<&'a str>,
    /// Pre-edit text (`old_string`). `None` for Write events.
    pub old_text: Option<&'a str>,
    /// Platform session id. Empty string when unknown.
    pub session_id: Option<&'a str>,
    /// Timestamp override for tests. `None` falls back to
    /// `SystemTime::now()`.
    pub ts_ms: Option<i64>,
}

/// Maximum size of the diff excerpt captured in the observation payload.
pub const DIFF_EXCERPT_MAX_BYTES: usize = 1024;

pub const TITLE_MAX_CHARS: usize = 120;

pub const NARRATIVE_MAX_CHARS: usize = 500;

/// Secret-bearing path patterns that short-circuit classification.
/// Hardcoded — the user cannot disable this guard from config.
const PRIVACY_DENY_SUBSTRINGS: &[&str] =
    &[".env", ".secrets", ".key", ".pem", "id_rsa", "credentials"];

/// Classify a `PostToolUse` event. Returns `None` when the event should
/// not produce an observation (non-edit tool, no file path, missing
/// diff signal, or a privacy-denied path).
pub fn classify(input: &ClassifyInput<'_>) -> Option<Observation> {
    if !matches!(input.tool, "Edit" | "MultiEdit" | "Write") {
        return None;
    }

    let file_path = input.file_path?;
    if is_privacy_denied(file_path) {
        return None;
    }

    // Need at least one of diff / new_text to key off.
    if input.diff.is_none() && input.new_text.is_none() {
        return None;
    }

    let obs_type = determine_obs_type(input);
    let title = build_title(input.tool, file_path, &obs_type);
    let narrative = build_narrative(input);
    let diff_excerpt = input
        .diff
        .map(strip_private_tagged_regions)
        .map(|diff| truncate_diff_excerpt(&diff));

    let session_id = input.session_id.unwrap_or("").to_owned();
    let ts_ms = input.ts_ms.unwrap_or_else(now_unix_ms);
    let content_hash =
        compute_content_hash(&session_id, Some(file_path), &title, narrative.as_deref());

    Some(Observation {
        session_id,
        ts_ms,
        obs_type,
        tool: input.tool.to_owned(),
        file_path: Some(file_path.to_owned()),
        scope: derive_scope(file_path),
        title,
        narrative,
        diff_excerpt,
        content_hash,
    })
}

/// Heuristic core. Order matters: most-specific patterns first,
/// falling through to the generic `change` label.
fn determine_obs_type(input: &ClassifyInput<'_>) -> String {
    // Write with no old_text is a new file (only Write has this shape).
    if input.tool == "Write" && input.old_text.is_none() {
        return "feature".to_owned();
    }

    if let Some(old) = input.old_text
        && removes_bug_marker(old, input.new_text.unwrap_or(""))
    {
        return "bugfix".to_owned();
    }

    if let Some(diff) = input.diff {
        if diff_is_whitespace_only(diff) {
            return "refactor".to_owned();
        }
    } else if let (Some(old), Some(new)) = (input.old_text, input.new_text)
        && strip_ws(old) == strip_ws(new)
        && old != new
    {
        return "refactor".to_owned();
    }

    "change".to_owned()
}

/// `true` when `old` has more standalone uppercase bug markers (FIXME /
/// BUG / TODO) than `new`. Uppercase-only to avoid false positives on
/// lowercase `todo`; word-boundary counting keeps `DEBUG` from matching
/// `BUG`.
fn removes_bug_marker(old: &str, new: &str) -> bool {
    const MARKERS: &[&str] = &["FIXME", "BUG", "TODO"];
    for marker in MARKERS {
        let before = count_word_occurrences(old, marker);
        let after = count_word_occurrences(new, marker);
        if before > after {
            return true;
        }
    }
    false
}

fn count_word_occurrences(haystack: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return 0;
    }
    let bytes = haystack.as_bytes();
    let nbytes = needle.as_bytes();
    let mut count = 0;
    let mut i = 0;
    while i + nbytes.len() <= bytes.len() {
        if &bytes[i..i + nbytes.len()] == nbytes {
            let prev_ok = i == 0 || !is_word_byte(bytes[i - 1]);
            let next_ok = i + nbytes.len() == bytes.len() || !is_word_byte(bytes[i + nbytes.len()]);
            if prev_ok && next_ok {
                count += 1;
                i += nbytes.len();
                continue;
            }
        }
        i += 1;
    }
    count
}

const fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// `true` iff the `-` and `+` lines have identical content after
/// stripping whitespace. Context lines are ignored.
fn diff_is_whitespace_only(diff: &str) -> bool {
    let mut removed = String::new();
    let mut added = String::new();
    let mut saw_change = false;
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix('-') {
            saw_change = true;
            removed.push_str(rest);
            removed.push('\n');
        } else if let Some(rest) = line.strip_prefix('+') {
            saw_change = true;
            added.push_str(rest);
            added.push('\n');
        }
    }
    if !saw_change {
        return false;
    }
    strip_ws(&removed) == strip_ws(&added)
}

/// Remove all whitespace. Good enough for the refactor heuristic:
/// reorderings slip past, but so do hand-written `rustfmt` tweaks.
fn strip_ws(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Build a ≤ 120-char `"{tool} {file}: {hint}"` title, where the hint
/// derives from `obs_type`. Truncation appends `"…"`.
fn build_title(tool: &str, file_path: &str, obs_type: &str) -> String {
    let hint = match obs_type {
        "feature" => "new file",
        "bugfix" => "remove bug marker",
        "refactor" => "whitespace/rename",
        _ => "edit",
    };
    let base = format!("{tool} {file_path}: {hint}");
    truncate_chars(&base, TITLE_MAX_CHARS)
}

/// Build a ≤ 500-char narrative from the first few diff lines.
fn build_narrative(input: &ClassifyInput<'_>) -> Option<String> {
    let diff = strip_private_tagged_regions(input.diff?);
    let mut collected = String::new();
    for line in diff.lines().take(6) {
        if !collected.is_empty() {
            collected.push('\n');
        }
        collected.push_str(line);
    }
    if collected.is_empty() {
        return None;
    }
    Some(truncate_chars(&collected, NARRATIVE_MAX_CHARS))
}

/// Truncate at a char boundary, appending "…" when truncated.
fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Byte-level truncation for the diff excerpt: first
/// `DIFF_EXCERPT_MAX_BYTES` bytes plus a marker.
fn truncate_diff_excerpt(diff: &str) -> String {
    if diff.len() <= DIFF_EXCERPT_MAX_BYTES {
        return diff.to_owned();
    }
    // Largest char boundary ≤ max, so we don't split a codepoint.
    let mut end = DIFF_EXCERPT_MAX_BYTES;
    while end > 0 && !diff.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = String::with_capacity(end + 16);
    out.push_str(&diff[..end]);
    out.push_str("\n…[truncated]");
    out
}

/// `sha256(session_id|file|title|narrative)[:16]` as lowercase hex.
/// Matches the 16-char convention used by `remember_rule` content
/// hashes for cloud-side dedup.
pub(crate) fn compute_content_hash(
    session_id: &str,
    file_path: Option<&str>,
    title: &str,
    narrative: Option<&str>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(session_id.as_bytes());
    hasher.update(b"|");
    hasher.update(file_path.unwrap_or("").as_bytes());
    hasher.update(b"|");
    hasher.update(title.as_bytes());
    hasher.update(b"|");
    hasher.update(narrative.unwrap_or("").as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// `true` when the path matches a hardcoded secret pattern. Lowercase
/// substring match (not glob), covering both extensions and embedded
/// tokens like `src/config/.env.local`.
pub fn is_privacy_denied(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    PRIVACY_DENY_SUBSTRINGS
        .iter()
        .any(|needle| lower.contains(needle))
}

fn derive_scope(file_path: &str) -> Option<ObservationScope> {
    let trimmed = file_path.trim_matches('/');
    if trimmed.is_empty() {
        return None;
    }

    let parts: Vec<&str> = trimmed.split('/').filter(|part| !part.is_empty()).collect();
    if parts.is_empty() {
        return None;
    }

    let display_name = parts.last().map(|part| (*part).to_owned());
    let parent_path = if parts.len() > 1 {
        Some(parts[..parts.len() - 1].join("/"))
    } else {
        None
    };

    Some(ObservationScope {
        anchor_kind: "file".to_owned(),
        anchor_key: parts.join("/"),
        parent_path,
        display_name,
    })
}

fn now_unix_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input<'a>(
        tool: &'a str,
        file: &'a str,
        diff: Option<&'a str>,
        new_text: Option<&'a str>,
        old_text: Option<&'a str>,
    ) -> ClassifyInput<'a> {
        ClassifyInput {
            tool,
            file_path: Some(file),
            diff,
            new_text,
            old_text,
            session_id: Some("sess_test"),
            ts_ms: Some(1_714_000_000_000),
        }
    }

    #[test]
    fn classify_write_new_file_returns_feature() {
        let inp = input(
            "Write",
            "src/new_mod.rs",
            Some("+fn hello() {}\n"),
            Some("fn hello() {}\n"),
            None,
        );
        let obs = classify(&inp).expect("some");
        assert_eq!(obs.obs_type, "feature");
        assert_eq!(obs.tool, "Write");
        assert_eq!(obs.file_path.as_deref(), Some("src/new_mod.rs"));
        assert!(
            obs.title.contains("Write"),
            "title missing tool: {}",
            obs.title
        );
    }

    #[test]
    fn classify_edit_removing_fixme_returns_bugfix() {
        let old = "// FIXME: panics on None\nfoo.unwrap();\n";
        let new = "if let Some(x) = foo { use_x(x); }\n";
        let diff =
            "-// FIXME: panics on None\n-foo.unwrap();\n+if let Some(x) = foo { use_x(x); }\n";
        let inp = input("Edit", "src/foo.rs", Some(diff), Some(new), Some(old));
        let obs = classify(&inp).expect("some");
        assert_eq!(obs.obs_type, "bugfix");
    }

    #[test]
    fn classify_edit_whitespace_only_returns_refactor() {
        let old = "let x=1;let y=2;";
        let new = "let x = 1;\nlet y = 2;";
        let diff = "-let x=1;let y=2;\n+let x = 1;\n+let y = 2;\n";
        let inp = input("Edit", "src/foo.rs", Some(diff), Some(new), Some(old));
        let obs = classify(&inp).expect("some");
        assert_eq!(obs.obs_type, "refactor");
    }

    #[test]
    fn removing_debug_line_does_not_count_as_bug_marker_removal() {
        // Regression: substring matching let "DEBUG" trigger the "BUG"
        // marker. Word-boundary counting keeps BUG/DEBUG distinct.
        let old = "// DEBUG: tracing\nlog::trace!(\"x={x}\");\n";
        let new = "// (debug line removed)\n";
        let diff = "-// DEBUG: tracing\n-log::trace!(\"x={x}\");\n+// (debug line removed)\n";
        let inp = input("Edit", "src/foo.rs", Some(diff), Some(new), Some(old));
        let obs = classify(&inp).expect("some");
        assert_ne!(
            obs.obs_type, "bugfix",
            "DEBUG → empty must not be classified as a bugfix"
        );
    }

    #[test]
    fn classify_edit_default_returns_change() {
        let old = "let x = 1;";
        let new = "let x = compute_answer();";
        let diff = "-let x = 1;\n+let x = compute_answer();\n";
        let inp = input("Edit", "src/foo.rs", Some(diff), Some(new), Some(old));
        let obs = classify(&inp).expect("some");
        assert_eq!(obs.obs_type, "change");
    }

    #[test]
    fn privacy_guard_blocks_env_files() {
        let inp = input(
            "Write",
            "src/app/.env.local",
            Some("+SECRET=abc\n"),
            Some("SECRET=abc\n"),
            None,
        );
        assert!(classify(&inp).is_none());
    }

    #[test]
    fn privacy_guard_allows_normal_source_files() {
        let inp = input(
            "Write",
            "src/foo.rs",
            Some("+fn main() {}\n"),
            Some("fn main() {}\n"),
            None,
        );
        assert!(classify(&inp).is_some());
    }

    #[test]
    fn privacy_guard_covers_pem_key_credentials() {
        for path in &[
            "config/.env",
            "app.secrets.json",
            "infra/prod.secrets.yaml",
            "keys/server.key",
            "certs/app.pem",
            "home/user/.ssh/id_rsa",
            "credentials.json",
        ] {
            assert!(is_privacy_denied(path), "expected deny for `{path}`");
        }
    }

    #[test]
    fn private_tagged_regions_are_redacted_from_observation_payload() {
        let diff = "-safe\n+safe <private>token=abc</private>\n+done\n";
        let inp = input(
            "Edit",
            "src/foo.rs",
            Some(diff),
            Some("safe done\n"),
            Some("safe\n"),
        );

        let obs = classify(&inp).expect("some");

        assert!(
            obs.narrative
                .as_deref()
                .unwrap()
                .contains("[redacted private content]")
        );
        assert!(
            obs.diff_excerpt
                .as_deref()
                .unwrap()
                .contains("[redacted private content]")
        );
        assert!(!obs.narrative.as_deref().unwrap().contains("token=abc"));
        assert!(!obs.diff_excerpt.as_deref().unwrap().contains("token=abc"));
    }

    #[test]
    fn content_hash_is_stable_and_file_sensitive() {
        let old = "let x = 1;";
        let new = "let x = compute_answer();";
        let diff = "-let x = 1;\n+let x = compute_answer();\n";
        let inp = input("Edit", "src/foo.rs", Some(diff), Some(new), Some(old));
        let a = classify(&inp).expect("some");
        let b = classify(&inp).expect("some");
        assert_eq!(a.content_hash, b.content_hash);
        assert_eq!(a.content_hash.len(), 16);

        let other = classify(&input("Edit", "b.rs", Some(diff), Some(new), Some(old))).unwrap();
        assert_ne!(a.content_hash, other.content_hash);
    }

    #[test]
    fn non_edit_tool_returns_none() {
        let inp = input("Read", "src/foo.rs", None, None, None);
        assert!(classify(&inp).is_none());
    }

    #[test]
    fn missing_diff_and_new_text_returns_none() {
        let inp = input("Edit", "src/foo.rs", None, None, Some("old"));
        assert!(classify(&inp).is_none());
    }

    #[test]
    fn classify_emits_structured_scope_metadata() {
        let old = "let x = 1;";
        let new = "let x = compute_answer();";
        let diff = "-let x = 1;\n+let x = compute_answer();\n";
        let obs = classify(&input(
            "Edit",
            "src/auth/login/handler.rs",
            Some(diff),
            Some(new),
            Some(old),
        ))
        .expect("some");

        assert_eq!(
            obs.scope,
            Some(ObservationScope {
                anchor_kind: "file".to_owned(),
                anchor_key: "src/auth/login/handler.rs".to_owned(),
                parent_path: Some("src/auth/login".to_owned()),
                display_name: Some("handler.rs".to_owned()),
            })
        );
    }

    #[test]
    fn wire_shape_accepts_optional_scope_metadata() {
        let payload = serde_json::json!({
            "session_id": "sess_new",
            "ts_ms": 2,
            "obs_type": "bugfix",
            "tool": "Edit",
            "file_path": "src/auth/login/handler.rs",
            "scope": {
                "anchor_kind": "file",
                "anchor_key": "src/auth/login/handler.rs",
                "parent_path": "src/auth/login",
                "display_name": "handler.rs"
            },
            "title": "Edit src/auth/login/handler.rs: remove bug marker",
            "narrative": "guard login retry state",
            "diff_excerpt": "-old\n+new",
            "content_hash": "def456"
        });

        let obs: Observation = serde_json::from_value(payload).expect("deserialize");
        assert_eq!(
            obs.scope.as_ref().map(|scope| scope.anchor_key.as_str()),
            Some("src/auth/login/handler.rs")
        );
    }

    /// Prints a wire-format example for each `obs_type`. Run with
    /// `cargo test -p difflore-core --lib
    /// observation::tests::print_wire_samples -- --ignored --nocapture`.
    #[test]
    #[ignore = "doc helper for sample wire output, run manually"]
    fn print_wire_samples() {
        let samples = [
            (
                "feature",
                input(
                    "Write",
                    "src/new_mod.rs",
                    Some("+fn hello() {}\n+pub fn world() {}\n"),
                    Some("fn hello() {}\npub fn world() {}\n"),
                    None,
                ),
            ),
            (
                "bugfix",
                input(
                    "Edit",
                    "src/foo.rs",
                    Some(
                        "-// FIXME: crash on None\n-foo.unwrap();\n+if let Some(x) = foo { use_x(x); }\n",
                    ),
                    Some("if let Some(x) = foo { use_x(x); }\n"),
                    Some("// FIXME: crash on None\nfoo.unwrap();\n"),
                ),
            ),
            (
                "refactor",
                input(
                    "Edit",
                    "src/foo.rs",
                    Some("-let x=1;let y=2;\n+let x = 1;\n+let y = 2;\n"),
                    Some("let x = 1;\nlet y = 2;"),
                    Some("let x=1;let y=2;"),
                ),
            ),
            (
                "change",
                input(
                    "Edit",
                    "src/foo.rs",
                    Some("-let x = 1;\n+let x = compute_answer();\n"),
                    Some("let x = compute_answer();"),
                    Some("let x = 1;"),
                ),
            ),
        ];
        for (label, inp) in samples {
            let obs = classify(&inp).expect("some");
            let json = serde_json::to_string_pretty(&obs).unwrap();
            println!("=== {label} ===\n{json}\n");
        }
    }

    #[test]
    fn diff_excerpt_truncates_large_diffs() {
        let big: String = (0..4096).map(|_| 'x').collect();
        let diff = format!("-{big}\n+{big}Y\n");
        let inp = input("Edit", "src/foo.rs", Some(&diff), Some("yYY"), Some("xxx"));
        let obs = classify(&inp).expect("some");
        let excerpt = obs.diff_excerpt.expect("excerpt present");
        assert!(
            excerpt.len() <= DIFF_EXCERPT_MAX_BYTES + 32,
            "excerpt too long: {}",
            excerpt.len()
        );
        assert!(excerpt.ends_with("[truncated]"));
    }
}
