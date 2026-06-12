// Centralized accessors for environment-derived runtime configuration.
//
// All non-test env reads funnel through this module so the recognised vars
// are discoverable in one place and bool/integer parsing happens once. Most
// accessors cache per-process via `OnceLock`, so tests must set a var before
// its first access.

use std::ffi::OsString;
use std::sync::OnceLock;

// --- env var name constants ---

pub const OPENAI_API_KEY: &str = "OPENAI_API_KEY";
pub const ANTHROPIC_API_KEY: &str = "ANTHROPIC_API_KEY";

pub const DIFFLORE_FIX_DEBUG: &str = "DIFFLORE_FIX_DEBUG";
pub const DIFFLORE_FIX_DUMP_DIR: &str = "DIFFLORE_FIX_DUMP_DIR";
pub const DIFFLORE_FIX_PREVIEW_REVIEW_TIMEOUT_SECS: &str =
    "DIFFLORE_FIX_PREVIEW_REVIEW_TIMEOUT_SECS";
pub const DIFFLORE_TRACE_HOOK: &str = "DIFFLORE_TRACE_HOOK";
pub const DIFFLORE_HOOK_CACHE_TTL_MS: &str = "DIFFLORE_HOOK_CACHE_TTL_MS";
/// Controls the per-process `hook_post_edit` short-circuit cache.
///
/// `auto` (default): once a file extension has ≥10 hook serves in the trailing
/// window with ≥90% empties, skip the index round-trip and return an empty
/// rule list. `off`: always run full retrieval (debugging). `force`: always
/// short-circuit on the first call for every extension (diagnostic only — the
/// agent gets no rules).
///
/// The cache is in-process only; every fresh daemon launch re-learns.
/// Short-circuited calls don't write `mcp_rule_serves` or `cloud_outbox`, so an
/// extension can recover on the next launch.
pub const DIFFLORE_HOOK_SHORT_CIRCUIT: &str = "DIFFLORE_HOOK_SHORT_CIRCUIT";
pub const DIFFLORE_HOOK_CLIENT: &str = "DIFFLORE_HOOK_CLIENT";
pub const DIFFLORE_HOOK_FORWARD: &str = "DIFFLORE_HOOK_FORWARD";
/// Idle timeout (seconds) for a warm hook-forward daemon: after this long with
/// no accepted connection, the daemon exits and removes its socket so an idle
/// repo's process is reclaimed. Defaults to [`DEFAULT_HOOK_DAEMON_IDLE_SECS`].
pub const DIFFLORE_HOOK_DAEMON_IDLE_SECS: &str = "DIFFLORE_HOOK_DAEMON_IDLE_SECS";
pub const DIFFLORE_DEBUG_HOOKS: &str = "DIFFLORE_DEBUG_HOOKS";
pub const DIFFLORE_HOOK_SHIM_TRACE: &str = "DIFFLORE_HOOK_SHIM_TRACE";
/// Opt-in: allow the hook to fall back to cross-repo "starter" rules when the
/// current repo has no scoped memory. Default OFF — the hook surfaces only this
/// repo's own memory. (The `difflore recall` command keeps its own starter path.)
pub const DIFFLORE_HOOK_CROSS_REPO_STARTER: &str = "DIFFLORE_HOOK_CROSS_REPO_STARTER";
pub const DIFFLORE_MASTER_KEY: &str = "DIFFLORE_MASTER_KEY";
pub const DIFFLORE_HOME: &str = "DIFFLORE_HOME";
pub const DIFFLORE_MCP_HOME: &str = "DIFFLORE_MCP_HOME";
pub const DIFFLORE_NO_WELCOME: &str = "DIFFLORE_NO_WELCOME";
pub const DIFFLORE_CLOUD_TOKEN: &str = "DIFFLORE_CLOUD_TOKEN";
/// API key for `difflore embeddings setup` (BYOK), read from env/stdin so it
/// stays out of shell history. At embed time the runtime resolver uses the
/// encrypted key stored by that command, not this env var.
pub const DIFFLORE_EMBEDDING_KEY: &str = "DIFFLORE_EMBEDDING_KEY";
pub const DIFFLORE_TOKEN: &str = "DIFFLORE_TOKEN";
/// GitLab PAT override for review import; takes precedence over the
/// conventional `GITLAB_TOKEN` and over encrypted storage.
pub const DIFFLORE_GITLAB_TOKEN: &str = "DIFFLORE_GITLAB_TOKEN";
/// Conventional GitLab CI/tooling token env var, honored second.
pub const GITLAB_TOKEN: &str = "GITLAB_TOKEN";
pub const DIFFLORE_DEBUG_CLOUD: &str = "DIFFLORE_DEBUG_CLOUD";
pub const DIFFLORE_DEBUG_TELEMETRY: &str = "DIFFLORE_DEBUG_TELEMETRY";
pub const DIFFLORE_DEBUG_PROVIDERS: &str = "DIFFLORE_DEBUG_PROVIDERS";
pub const DIFFLORE_BFS_RETRIEVAL: &str = "DIFFLORE_BFS_RETRIEVAL";
pub const DIFFLORE_INTENT_RERANK: &str = "DIFFLORE_INTENT_RERANK";
pub const DIFFLORE_DISABLE_RULES: &str = "DIFFLORE_DISABLE_RULES";
/// Rollback switch for the deterministic serve-layer rule arbitration
/// (strict-hit → 10% score band → source priority → confidence → skill_id).
/// Truthy disables the arbitration re-sort at both serve exits (post-edit
/// hook + `search_rules`), restoring the pre-arbitration ordering. The
/// `why` ranking facts remain available either way.
pub const DIFFLORE_DISABLE_SOURCE_PRIORITY: &str = "DIFFLORE_DISABLE_SOURCE_PRIORITY";
/// Switch for the hook post-edit intent-alignment gate (C6 misapply
/// unification): `0`/`off`/`false`/`disabled` disables, any other non-empty
/// value enables, unset falls back to [`DEFAULT_HOOK_INTENT_GATE`].
pub const DIFFLORE_HOOK_INTENT_GATE: &str = "DIFFLORE_HOOK_INTENT_GATE";
/// Probability (0.0–0.10) that an MCP recall serve with caller-requested
/// `top_k == 5` is transparently bumped to `top_k = 8` so we accrue data on
/// whether rules at ranks 6–8 ever get accepted.
pub const DIFFLORE_DEEP_RECALL_SAMPLE_RATE: &str = "DIFFLORE_DEEP_RECALL_SAMPLE_RATE";
pub const DIFFLORE_CLAUDE_HOME: &str = "DIFFLORE_CLAUDE_HOME";
pub const DIFFLORE_CLOUD_URL: &str = "DIFFLORE_CLOUD_URL";
pub const DIFF_LORE_CLOUD_URL: &str = "DIFF_LORE_CLOUD_URL";

pub const NO_COLOR: &str = "NO_COLOR";
pub const COLORTERM: &str = "COLORTERM";
pub const TERM: &str = "TERM";
pub const COLUMNS: &str = "COLUMNS";
pub const PATH: &str = "PATH";
pub const COLORFGBG: &str = "COLORFGBG";
pub const DIFFLORE_THEME: &str = "DIFFLORE_THEME";

// --- low-level helpers ---

#[must_use]
pub fn var(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

#[must_use]
pub fn var_os(name: &str) -> Option<OsString> {
    std::env::var_os(name)
}

#[must_use]
pub fn flag_set(name: &str) -> bool {
    std::env::var_os(name).is_some()
}

#[must_use]
pub fn non_empty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

#[must_use]
pub fn truthy(name: &str) -> bool {
    matches!(std::env::var(name), Ok(v) if !v.is_empty() && v != "0" && v != "false")
}

// --- typed accessors (cached) ---

#[must_use]
pub fn fix_debug() -> bool {
    static CACHED: OnceLock<bool> = OnceLock::new();
    *CACHED.get_or_init(|| flag_set(DIFFLORE_FIX_DEBUG))
}

#[must_use]
pub fn trace_hook() -> bool {
    static CACHED: OnceLock<bool> = OnceLock::new();
    *CACHED.get_or_init(|| flag_set(DIFFLORE_TRACE_HOOK))
}

/// Whether the hook may inject cross-repo "starter" rules. See
/// [`DIFFLORE_HOOK_CROSS_REPO_STARTER`]; default OFF.
#[must_use]
pub fn hook_cross_repo_starter_enabled() -> bool {
    static CACHED: OnceLock<bool> = OnceLock::new();
    *CACHED.get_or_init(|| truthy(DIFFLORE_HOOK_CROSS_REPO_STARTER))
}

#[must_use]
pub fn debug_cloud() -> bool {
    static CACHED: OnceLock<bool> = OnceLock::new();
    *CACHED.get_or_init(|| flag_set(DIFFLORE_DEBUG_CLOUD))
}

#[must_use]
pub fn debug_telemetry() -> bool {
    static CACHED: OnceLock<bool> = OnceLock::new();
    *CACHED.get_or_init(|| flag_set(DIFFLORE_DEBUG_TELEMETRY))
}

#[must_use]
pub fn debug_providers() -> bool {
    static CACHED: OnceLock<bool> = OnceLock::new();
    *CACHED.get_or_init(|| flag_set(DIFFLORE_DEBUG_PROVIDERS))
}

#[must_use]
pub fn hook_cache_ttl_ms() -> Option<u64> {
    var(DIFFLORE_HOOK_CACHE_TTL_MS).and_then(|v| v.parse().ok())
}

/// Default idle timeout (seconds) for a warm hook-forward daemon: 10 minutes.
/// Long enough that a daemon spawned on a cache miss does not immediately
/// thrash (spawn → idle-exit → re-spawn) while the user keeps editing, short
/// enough that an abandoned repo's process is reclaimed within minutes.
pub const DEFAULT_HOOK_DAEMON_IDLE_SECS: u64 = 600;

/// Resolve [`DIFFLORE_HOOK_DAEMON_IDLE_SECS`] into the daemon idle timeout.
///
/// Read on every call (no cache) so a freshly spawned daemon picks up the
/// current env, and so tests can set a tiny value before launching. An unset,
/// empty, zero, or unparseable value falls back to
/// [`DEFAULT_HOOK_DAEMON_IDLE_SECS`] — a malformed knob must never disable the
/// idle reaper or make it fire instantly.
#[must_use]
pub fn hook_daemon_idle_secs() -> u64 {
    match var(DIFFLORE_HOOK_DAEMON_IDLE_SECS).and_then(|v| v.trim().parse::<u64>().ok()) {
        Some(n) if n > 0 => n,
        _ => DEFAULT_HOOK_DAEMON_IDLE_SECS,
    }
}

/// Tri-state knob for the `hook_post_edit` short-circuit heuristic.
///
/// See [`DIFFLORE_HOOK_SHORT_CIRCUIT`] for the env semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookShortCircuitMode {
    /// Apply the empirical heuristic — short-circuit when the trailing
    /// window's empty-rate clears the threshold.
    Auto,
    /// Never short-circuit (debugging).
    Off,
    /// Always short-circuit (diagnostic only).
    Force,
}

impl HookShortCircuitMode {
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "off" | "disable" | "disabled" | "0" | "false" => Self::Off,
            "force" | "always" => Self::Force,
            _ => Self::Auto,
        }
    }
}

/// Resolve [`DIFFLORE_HOOK_SHORT_CIRCUIT`] into a tri-state mode.
///
/// Read from the env on every call (no `OnceLock` cache) so tests can flip
/// behaviour without racing; one `std::env::var` lookup is negligible here.
#[must_use]
pub fn hook_short_circuit_mode() -> HookShortCircuitMode {
    match var(DIFFLORE_HOOK_SHORT_CIRCUIT) {
        Some(v) if !v.is_empty() => HookShortCircuitMode::parse(&v),
        _ => HookShortCircuitMode::Auto,
    }
}

/// Default deep-recall sample rate when the env var is unset (2%).
pub const DEFAULT_DEEP_RECALL_SAMPLE_RATE: f32 = 0.02;

/// Maximum permitted deep-recall sample rate (10%). Higher rates would shift
/// the cost/token profile of the hot recall path; the sampler is a cheap
/// occasional probe, not a second production mode.
pub const MAX_DEEP_RECALL_SAMPLE_RATE: f32 = 0.10;

/// Parse a raw `DIFFLORE_DEEP_RECALL_SAMPLE_RATE` string into a validated
/// `f32` in `[0.0, MAX_DEEP_RECALL_SAMPLE_RATE]`.
///
/// Returns a human-readable error message on:
/// * non-numeric input (e.g. `"two"`),
/// * non-finite values (`NaN`, `±inf`),
/// * negative values,
/// * values above `MAX_DEEP_RECALL_SAMPLE_RATE`.
///
/// Empty / whitespace input is rejected: an explicit
/// `DIFFLORE_DEEP_RECALL_SAMPLE_RATE=` is almost always a typo. Unset the var
/// to get the default.
pub fn parse_deep_recall_sample_rate(raw: &str) -> Result<f32, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(format!(
            "{DIFFLORE_DEEP_RECALL_SAMPLE_RATE} is empty; unset it to use the default \
             ({DEFAULT_DEEP_RECALL_SAMPLE_RATE}) or pass a value in \
             [0.0, {MAX_DEEP_RECALL_SAMPLE_RATE}]"
        ));
    }
    let parsed: f32 = trimmed.parse().map_err(|_| {
        format!(
            "{DIFFLORE_DEEP_RECALL_SAMPLE_RATE}={raw:?} is not a valid f32; expected a \
             number in [0.0, {MAX_DEEP_RECALL_SAMPLE_RATE}]"
        )
    })?;
    if !parsed.is_finite() {
        return Err(format!(
            "{DIFFLORE_DEEP_RECALL_SAMPLE_RATE}={raw:?} must be finite; got {parsed}"
        ));
    }
    if !(0.0..=MAX_DEEP_RECALL_SAMPLE_RATE).contains(&parsed) {
        return Err(format!(
            "{DIFFLORE_DEEP_RECALL_SAMPLE_RATE}={parsed} is out of range; expected \
             [0.0, {MAX_DEEP_RECALL_SAMPLE_RATE}]"
        ));
    }
    Ok(parsed)
}

/// Resolve [`DIFFLORE_DEEP_RECALL_SAMPLE_RATE`] into a validated probability.
///
/// Read on every call (no `OnceLock` cache) so tests can flip the rate without
/// racing. An invalid env value logs once to stderr and falls back to
/// [`DEFAULT_DEEP_RECALL_SAMPLE_RATE`] — recall must never fail because of a
/// malformed observability knob.
#[must_use]
pub fn deep_recall_sample_rate() -> f32 {
    match var(DIFFLORE_DEEP_RECALL_SAMPLE_RATE) {
        Some(raw) => match parse_deep_recall_sample_rate(&raw) {
            Ok(rate) => rate,
            Err(msg) => {
                eprintln!(
                    "[difflore] invalid {DIFFLORE_DEEP_RECALL_SAMPLE_RATE}: {msg}; \
                     falling back to default {DEFAULT_DEEP_RECALL_SAMPLE_RATE}"
                );
                DEFAULT_DEEP_RECALL_SAMPLE_RATE
            }
        },
        None => DEFAULT_DEEP_RECALL_SAMPLE_RATE,
    }
}

/// Whether [`DIFFLORE_DISABLE_SOURCE_PRIORITY`] is set (truthy). Read on
/// every call (no `OnceLock` cache) so an operator can roll the arbitration
/// back without restarting a warm hook daemon, and so tests can flip it.
#[must_use]
pub fn source_priority_disabled() -> bool {
    truthy(DIFFLORE_DISABLE_SOURCE_PRIORITY)
}

/// Default for the hook post-edit intent-alignment gate when
/// [`DIFFLORE_HOOK_INTENT_GATE`] is unset.
///
/// Default ON: the workspace regression run (hook serve-proof tests +
/// `difflore eval` self-recall, which goes through the untouched
/// `retrieve_rules_for_search` path) showed no regression with the gate
/// applied to the post-edit hook lane, and the gate-on hook tests pin the
/// no-empty-injection behaviour for aligned post-edit serves.
///
/// External-messaging note (C6): with the gate ON, the misapply guard can be
/// described as covering BOTH recall surfaces — explicit `search_rules` /
/// `recall` queries AND unsolicited post-edit hook injection. If this default
/// is ever flipped OFF, the claim must be split per surface again: "intent
/// alignment on explicit recall; hook injection guarded by file patterns +
/// score floors only".
pub const DEFAULT_HOOK_INTENT_GATE: bool = true;

/// Pure parse of a raw [`DIFFLORE_HOOK_INTENT_GATE`] value:
/// `0`/`off`/`false`/`disabled` disables, any other non-empty value enables,
/// unset/empty falls back to [`DEFAULT_HOOK_INTENT_GATE`]. Split from the env
/// read so the off-switch grammar is unit-testable without process-global
/// env mutation.
#[must_use]
pub fn parse_hook_intent_gate(raw: Option<&str>) -> bool {
    match raw.map(str::trim) {
        Some(v) if !v.is_empty() => {
            let v = v.to_ascii_lowercase();
            !matches!(v.as_str(), "0" | "off" | "false" | "disabled" | "disable")
        }
        _ => DEFAULT_HOOK_INTENT_GATE,
    }
}

/// Resolve [`DIFFLORE_HOOK_INTENT_GATE`]. Read on every call (no cache) so
/// warm hook daemons pick up changes without a restart.
#[must_use]
pub fn hook_intent_gate_enabled() -> bool {
    parse_hook_intent_gate(var(DIFFLORE_HOOK_INTENT_GATE).as_deref())
}

#[must_use]
pub fn master_key_hex() -> Option<String> {
    var(DIFFLORE_MASTER_KEY)
}

#[must_use]
pub fn difflore_home() -> Option<OsString> {
    var_os(DIFFLORE_HOME)
}

#[must_use]
pub fn fix_dump_dir() -> Option<String> {
    var(DIFFLORE_FIX_DUMP_DIR)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_deep_recall_sample_rate_accepts_in_range_values() {
        for raw in ["0.0", "0.02", "0.05", "0.10", " 0.02 "] {
            let parsed = parse_deep_recall_sample_rate(raw)
                .unwrap_or_else(|e| panic!("expected {raw:?} to parse, got error: {e}"));
            assert!(
                (0.0..=MAX_DEEP_RECALL_SAMPLE_RATE).contains(&parsed),
                "{raw:?} parsed to {parsed}, outside the permitted range"
            );
        }
        // Exact-value sanity for the canonical defaults users will set.
        assert!((parse_deep_recall_sample_rate("0.02").unwrap() - 0.02).abs() < 1e-6);
        assert!((parse_deep_recall_sample_rate("0.0").unwrap()).abs() < 1e-6);
    }

    #[test]
    fn hook_short_circuit_mode_parses_each_state() {
        assert_eq!(
            HookShortCircuitMode::parse("auto"),
            HookShortCircuitMode::Auto
        );
        assert_eq!(HookShortCircuitMode::parse(""), HookShortCircuitMode::Auto);
        assert_eq!(
            HookShortCircuitMode::parse("AUTO"),
            HookShortCircuitMode::Auto
        );
        assert_eq!(
            HookShortCircuitMode::parse("off"),
            HookShortCircuitMode::Off
        );
        assert_eq!(
            HookShortCircuitMode::parse("OFF"),
            HookShortCircuitMode::Off
        );
        assert_eq!(
            HookShortCircuitMode::parse("disabled"),
            HookShortCircuitMode::Off
        );
        assert_eq!(HookShortCircuitMode::parse("0"), HookShortCircuitMode::Off);
        assert_eq!(
            HookShortCircuitMode::parse("false"),
            HookShortCircuitMode::Off
        );
        assert_eq!(
            HookShortCircuitMode::parse("force"),
            HookShortCircuitMode::Force
        );
        assert_eq!(
            HookShortCircuitMode::parse("FORCE"),
            HookShortCircuitMode::Force
        );
        assert_eq!(
            HookShortCircuitMode::parse("always"),
            HookShortCircuitMode::Force
        );
        // Unknown values fall back to Auto rather than silently disabling
        // the feature.
        assert_eq!(
            HookShortCircuitMode::parse("yolo"),
            HookShortCircuitMode::Auto
        );
    }

    #[test]
    fn parse_hook_intent_gate_honours_off_grammar_and_default() {
        // Explicit off spellings.
        for off in ["0", "off", "OFF", "false", "disabled", "disable", " off "] {
            assert!(
                !parse_hook_intent_gate(Some(off)),
                "{off:?} must disable the hook intent gate"
            );
        }
        // Any other non-empty value enables.
        for on in ["1", "on", "true", "yes", "anything"] {
            assert!(
                parse_hook_intent_gate(Some(on)),
                "{on:?} must enable the hook intent gate"
            );
        }
        // Unset / empty fall back to the shipped default.
        assert_eq!(parse_hook_intent_gate(None), DEFAULT_HOOK_INTENT_GATE);
        assert_eq!(parse_hook_intent_gate(Some("")), DEFAULT_HOOK_INTENT_GATE);
        assert_eq!(parse_hook_intent_gate(Some("  ")), DEFAULT_HOOK_INTENT_GATE);
    }

    #[test]
    fn parse_deep_recall_sample_rate_rejects_out_of_range_and_garbage() {
        // Above the 10% cap.
        assert!(parse_deep_recall_sample_rate("0.5").is_err());
        assert!(parse_deep_recall_sample_rate("1.0").is_err());
        // Negative.
        assert!(parse_deep_recall_sample_rate("-0.01").is_err());
        // Non-numeric.
        assert!(parse_deep_recall_sample_rate("two").is_err());
        // Empty / whitespace.
        assert!(parse_deep_recall_sample_rate("").is_err());
        assert!(parse_deep_recall_sample_rate("   ").is_err());
        // Non-finite.
        assert!(parse_deep_recall_sample_rate("NaN").is_err());
        assert!(parse_deep_recall_sample_rate("inf").is_err());
    }
}
