// The kill-switch reads a process-wide env var, so these tests mutate
// global state. `temp_env::with_vars` serialises and restores so they can't
// race each other.
use super::super::*;

/// All env vars consulted by the kill-switch / haiku auto-disable gate.
/// Cleared around each test so a stray `ANTHROPIC_MODEL=…haiku…` in the dev
/// shell can't flip the gate underneath us.
const GATE_ENV_KEYS: &[&str] = &[
    "DIFFLORE_DISABLE_RULES",
    "DIFFLORE_FORCE_RULES_ON_HAIKU",
    "DIFFLORE_AGENT_MODEL",
    "ANTHROPIC_MODEL",
    "CLAUDE_MODEL",
];

/// Run `f` with the requested overrides applied to the gate env, then
/// restore. `None` clears a var, `Some(v)` sets it; vars not listed are
/// cleared for the duration so the harness is hermetic.
fn with_gate_env<F: FnOnce()>(overrides: &[(&str, Option<&str>)], f: F) {
    let mut vars: Vec<(&str, Option<&str>)> =
        GATE_ENV_KEYS.iter().map(|k| (*k, None)).collect();
    for (k, v) in overrides {
        if let Some(slot) = vars.iter_mut().find(|(name, _)| name == k) {
            slot.1 = *v;
        } else {
            vars.push((k, *v));
        }
    }
    temp_env::with_vars(vars, f);
}

fn with_env<F: FnOnce()>(key: &str, value: Option<&str>, f: F) {
    with_gate_env(&[(key, value)], f);
}

#[test]
fn unset_returns_none() {
    with_env("DIFFLORE_DISABLE_RULES", None, || {
        assert!(rule_injection_disabled().is_none());
    });
}

#[test]
fn truthy_value_disables() {
    with_env("DIFFLORE_DISABLE_RULES", Some("1"), || {
        assert!(rule_injection_disabled().is_some());
    });
    with_env("DIFFLORE_DISABLE_RULES", Some("yes"), || {
        assert!(rule_injection_disabled().is_some());
    });
}

#[test]
fn falsy_value_does_not_disable() {
    // Empty / "0" / "false" count as not-set so users can unset by writing
    // `=0` rather than removing the var.
    with_env("DIFFLORE_DISABLE_RULES", Some(""), || {
        assert!(rule_injection_disabled().is_none());
    });
    with_env("DIFFLORE_DISABLE_RULES", Some("0"), || {
        assert!(rule_injection_disabled().is_none());
    });
    with_env("DIFFLORE_DISABLE_RULES", Some("false"), || {
        assert!(rule_injection_disabled().is_none());
    });
}

#[test]
fn haiku_model_auto_disables_injection() {
    // Haiku detected via any supported env var auto-disables without the
    // explicit kill switch.
    for key in ["DIFFLORE_AGENT_MODEL", "ANTHROPIC_MODEL", "CLAUDE_MODEL"] {
        with_gate_env(&[(key, Some("claude-haiku-4-5-20251001"))], || {
            let reason = rule_injection_disabled().expect("haiku should auto-disable");
            assert!(
                reason.contains("haiku"),
                "reason should mention haiku, got `{reason}`"
            );
        });
    }
}

#[test]
fn haiku_with_force_override_runs_injection() {
    with_gate_env(
        &[
            ("ANTHROPIC_MODEL", Some("claude-haiku-4-5")),
            ("DIFFLORE_FORCE_RULES_ON_HAIKU", Some("1")),
        ],
        || {
            assert!(
                rule_injection_disabled().is_none(),
                "force-override must let injection run on haiku"
            );
        },
    );
}

#[test]
fn non_haiku_model_does_not_disable() {
    with_gate_env(&[("ANTHROPIC_MODEL", Some("claude-sonnet-4-6"))], || {
        assert!(rule_injection_disabled().is_none());
    });
    with_gate_env(&[("ANTHROPIC_MODEL", Some("claude-opus-4-7"))], || {
        assert!(rule_injection_disabled().is_none());
    });
}

#[test]
fn explicit_kill_switch_wins_over_force_override() {
    // When both the kill switch and the haiku override are set, the kill
    // switch wins and we report its reason.
    with_gate_env(
        &[
            ("DIFFLORE_DISABLE_RULES", Some("1")),
            ("ANTHROPIC_MODEL", Some("claude-haiku-4-5")),
            ("DIFFLORE_FORCE_RULES_ON_HAIKU", Some("1")),
        ],
        || {
            let reason = rule_injection_disabled().expect("kill switch wins");
            assert!(reason.contains("DIFFLORE_DISABLE_RULES"));
        },
    );
}

#[test]
fn haiku_auto_disable_active_observable_for_doctor() {
    with_gate_env(&[("ANTHROPIC_MODEL", Some("CLAUDE-HAIKU-4-5"))], || {
        assert!(haiku_auto_disable_active());
    });
    with_gate_env(
        &[
            ("ANTHROPIC_MODEL", Some("claude-haiku-4-5")),
            ("DIFFLORE_FORCE_RULES_ON_HAIKU", Some("1")),
        ],
        || {
            assert!(!haiku_auto_disable_active());
        },
    );
    with_gate_env(&[("ANTHROPIC_MODEL", Some("claude-sonnet-4-6"))], || {
        assert!(!haiku_auto_disable_active());
    });
    with_gate_env(&[], || {
        assert!(!haiku_auto_disable_active());
    });
}

#[test]
fn disabled_response_carries_zero_rules_meta() {
    let v = disabled_response("test reason");
    assert_eq!(v["_meta"]["impact"]["rulesInjected"], 0);
    assert_eq!(v["_meta"]["impact"]["disabled"], true);
    assert_eq!(
        v["_meta"]["embedding"]["degradedReason"].as_str(),
        Some("rules_disabled")
    );
    assert_eq!(
        v["_meta"]["embedding"]["vectorLaneAvailable"].as_bool(),
        Some(false)
    );
    let text = v["content"][0]["text"].as_str().expect("text");
    assert!(text.contains("test reason"));
    assert!(text.contains("DIFFLORE_DISABLE_RULES"));
}
