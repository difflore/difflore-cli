//! Request-argument validation and rule-injection gating shared by the MCP
//! tool handlers (split out of the former `tools/util.rs`).

use serde_json::{Value, json};

pub(crate) const MCP_TEXT_ARG_CHAR_LIMIT: usize = 16 * 1024;

pub(crate) fn validate_mcp_text_arg(
    name: &str,
    value: &str,
    limit: usize,
) -> Result<(), (i32, String)> {
    if value.chars().count() > limit {
        return Err((-32602, format!("{name} must be {limit} chars or fewer")));
    }
    Ok(())
}

/// Check whether rule injection should be suppressed for the current
/// process. Two paths trigger a skip:
///
/// 1. Explicit kill-switch: `DIFFLORE_DISABLE_RULES=1` (any truthy value).
/// 2. Auto-disable on haiku-class models: when the active agent model
///    (read from `DIFFLORE_AGENT_MODEL` / `ANTHROPIC_MODEL` /
///    `CLAUDE_MODEL`) contains "haiku". Override with
///    `DIFFLORE_FORCE_RULES_ON_HAIKU=1` if you want to opt back in.
///
/// The Haiku auto-disable avoids injecting rules into models where extra
/// context has shown poor precision. Users can opt back in explicitly.
pub(crate) fn rule_injection_disabled() -> Option<&'static str> {
    if is_disable_rules_env_set() {
        return Some("rule injection disabled via DIFFLORE_DISABLE_RULES");
    }
    if haiku_auto_disable_active() {
        return Some(
            "rule injection auto-disabled on haiku (override: DIFFLORE_FORCE_RULES_ON_HAIKU=1)",
        );
    }
    None
}

/// Truthy-value check shared by the explicit kill switch and the
/// haiku override flag. Empty / `0` / `false` count as unset so users
/// can leave the var defined but neutralised.
fn env_truthy(key: &str) -> bool {
    crate::infra::env::truthy(key)
}

fn is_disable_rules_env_set() -> bool {
    crate::infra::env::truthy(crate::infra::env::DIFFLORE_DISABLE_RULES)
}

/// Read the active agent model from the standard env vars Claude Code,
/// Cursor and the Anthropic SDK populate. Returns the first non-empty
/// match in priority order. Lower-cased so callers can do substring
/// checks without re-normalising.
pub fn detect_active_model() -> Option<String> {
    for key in ["DIFFLORE_AGENT_MODEL", "ANTHROPIC_MODEL", "CLAUDE_MODEL"] {
        if let Some(v) = crate::infra::env::var(key) {
            let trimmed = v.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_ascii_lowercase());
            }
        }
    }
    None
}

/// True when the detected model id looks like a Haiku variant (any
/// generation). Substring match keeps us forward-compatible with
/// future haiku revs without a hard-coded list.
pub fn is_haiku_model(model: &str) -> bool {
    model.to_ascii_lowercase().contains("haiku")
}

/// True when (a) the active model resolves to haiku and (b) the user
/// hasn't opted back in via `DIFFLORE_FORCE_RULES_ON_HAIKU`. Exposed
/// for `difflore doctor` so it can report "auto-applied" instead of
/// "recommended".
pub fn haiku_auto_disable_active() -> bool {
    let Some(model) = detect_active_model() else {
        return false;
    };
    if !is_haiku_model(&model) {
        return false;
    }
    !env_truthy("DIFFLORE_FORCE_RULES_ON_HAIKU")
}

pub(crate) fn disabled_response(reason: &str) -> Value {
    json!({
        "content": [{
            "type": "text",
            "text": format!(
                "DiffLore: {reason}.\n\n\
                 No rules surfaced. Unset DIFFLORE_DISABLE_RULES to re-enable. \
                 Rule injection is off by default on haiku-class models. \
                 Sonnet+ users should leave the var unset."
            )
        }],
        "_meta": {
            "impact": { "rulesInjected": 0, "kind": "rules", "disabled": true },
            "embedding": {
                "activeProfile": null,
                "indexProfile": null,
                "profileMatch": false,
                "degraded": false,
                "degradedReason": "rules_disabled",
                "vectorLaneAvailable": false
            }
        }
    })
}
