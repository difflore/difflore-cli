//! Origin taxonomy: id → label / color / base confidence.

pub struct OriginDef {
    pub id: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    pub color_hex: &'static str,
    pub base_confidence: f64,
}

pub const ORIGINS: &[OriginDef] = &[
    OriginDef {
        id: "manual",
        label: "Manual",
        description: "Rule entered via the CLI by a human operator.",
        color_hex: "#9CA0B0",
        base_confidence: 0.7,
    },
    OriginDef {
        id: "conversation",
        label: "Conversation",
        description: "Rule captured mid-session via the remember_rule MCP tool.",
        color_hex: "#CBA6F7",
        base_confidence: 0.6,
    },
    OriginDef {
        id: "pr_review",
        label: "PR Review",
        description: "Rule extracted from a pull-request review comment.",
        color_hex: "#89DCEB",
        base_confidence: 0.7,
    },
    OriginDef {
        id: "extracted",
        label: "Extracted",
        description: "Rule distilled from existing docs or codebase signal.",
        color_hex: "#A6E3A1",
        base_confidence: 0.65,
    },
    OriginDef {
        id: "cloud",
        label: "Cloud",
        description: "Rule synced from the DiffLore cloud catalogue.",
        color_hex: "#F9E2AF",
        base_confidence: 0.7,
    },
    OriginDef {
        id: "team",
        label: "Team",
        description: "Rule shared by a team the user belongs to.",
        color_hex: "#89B4FA",
        base_confidence: 0.75,
    },
    OriginDef {
        id: "agent-memory",
        label: "Agent memory",
        description: "Rule extracted from a coding agent's local memory or rules file.",
        color_hex: "#94E2D5",
        base_confidence: 0.6,
    },
];

pub fn origin(id: &str) -> Option<&'static OriginDef> {
    ORIGINS.iter().find(|o| o.id == id)
}

pub fn color_hex_for(id: &str) -> Option<&'static str> {
    origin(id).map(|o| o.color_hex)
}

pub fn label_for(id: &str) -> Option<&'static str> {
    origin(id).map(|o| o.label)
}

pub fn base_confidence_for(id: &str) -> Option<f64> {
    origin(id).map(|o| o.base_confidence)
}

/// Recommended display priority: evidence-bearing origins (`pr_review`,
/// `extracted`) sort first, everything else after.
pub fn sort_order(origin: &str) -> u8 {
    match origin {
        "pr_review" | "extracted" => 0,
        _ => 1,
    }
}

/// Distribution-histogram ordering for rule-mix summaries, finer-grained than
/// `sort_order`. Must stay stable across releases: callers rely on it for
/// layout.
pub fn distribution_sort_key(origin: &str) -> u8 {
    match origin {
        "conversation" => 0,
        "manual" => 1,
        "pr_review" => 2,
        "extracted" => 3,
        "cloud" => 4,
        "team" => 5,
        "agent-memory" => 6,
        _ => 7,
    }
}

/// Group rules by their origin string. Returns a `BTreeMap` so callers
/// get deterministic key iteration without a sort step.
pub fn group_by_origin<'a, R: crate::domain::rule_view::RuleView>(
    rules: &'a [R],
) -> std::collections::BTreeMap<String, Vec<&'a R>> {
    let mut out: std::collections::BTreeMap<String, Vec<&'a R>> = std::collections::BTreeMap::new();
    for r in rules {
        out.entry(r.origin().to_owned()).or_default().push(r);
    }
    out
}

/// Generic HTTP / network / timeout error classifier shared by domain-specific
/// formatters. Returns a user-facing message and retains the raw error on the
/// unrecognised path so triage info isn't lost. Domain-specific framings (cloud
/// BYOK guidance, GitHub auth hints) belong in the cli wrappers, not here.
///
/// The raw-status fallback intentionally recognizes only the legacy upstream
/// message shapes this crate already emitted (`API error NNN` and
/// `"status":NNN`). Callers that still hold a typed HTTP status should use
/// [`format_api_error_with_status`] so a wording change upstream cannot silently
/// bypass classification.
pub fn format_api_error(label: &str, raw: &str) -> String {
    format_api_error_with_status(label, raw, None)
}

/// Like [`format_api_error`], but prefers a structured HTTP status when the
/// caller still has one. This keeps the user-facing copy aligned with the raw
/// helper while avoiding brittle re-parsing in typed API-client paths.
pub fn format_api_error_with_status(label: &str, raw: &str, status: Option<u16>) -> String {
    let lower = raw.to_ascii_lowercase();
    if let Some(formatted) =
        status.and_then(|status| format_api_error_from_status(label, raw, status))
    {
        return formatted;
    }
    if let Some(status) = status_from_raw_api_error(raw).or_else(|| status_from_json_status(raw))
        && let Some(formatted) = format_api_error_from_status(label, raw, status)
    {
        return formatted;
    }
    if raw.contains("INTERNAL_SERVER_ERROR") {
        return format!("{label}: server error (5xx). Likely transient.\n\n  raw: {raw}");
    }
    if lower.contains("connection refused")
        || lower.contains("connect error")
        || lower.contains("dns")
        || lower.contains("connection reset")
        || lower.contains("network is unreachable")
        || lower.contains("actively refused")
        || lower.contains("os error 10061")
        || (lower.contains("error sending request") && lower.contains("localhost"))
    {
        return format!(
            "{label}: network unreachable (DNS or connectivity issue).\n\n  raw: {raw}"
        );
    }
    if lower.contains("timed out") || lower.contains("timeout") {
        return format!("{label}: request timed out.\n\n  raw: {raw}");
    }
    format!("{label}: {raw}")
}

/// Format a known user-facing status class, or return `None` for statuses that
/// should keep their caller-specific wording.
pub fn format_api_error_from_status(label: &str, raw: &str, status: u16) -> Option<String> {
    match status {
        401 => Some(format!(
            "{label}: session expired or revoked (401).\n\n  raw: {raw}"
        )),
        403 => Some(format!("{label}: request rejected (403).\n\n  raw: {raw}")),
        429 => Some(format!("{label}: rate-limited (429).\n\n  raw: {raw}")),
        500..=599 => Some(format!(
            "{label}: server error (5xx). Likely transient.\n\n  raw: {raw}"
        )),
        _ => None,
    }
}

fn status_from_raw_api_error(raw: &str) -> Option<u16> {
    const MARKER: &str = "API error ";
    raw.match_indices(MARKER).find_map(|(idx, _)| {
        let start = idx + MARKER.len();
        parse_exact_three_digit_status(&raw[start..])
    })
}

fn status_from_json_status(raw: &str) -> Option<u16> {
    const MARKER: &str = "status\":";
    raw.match_indices(MARKER).find_map(|(idx, _)| {
        let start = idx + MARKER.len();
        parse_exact_three_digit_status(raw[start..].trim_start())
    })
}

fn parse_exact_three_digit_status(input: &str) -> Option<u16> {
    let bytes = input.as_bytes();
    if bytes.len() < 3 || !bytes[..3].iter().all(u8::is_ascii_digit) {
        return None;
    }
    if bytes.get(3).is_some_and(u8::is_ascii_digit) {
        return None;
    }
    input[..3].parse::<u16>().ok()
}

pub fn parse_hex_rgb(hex: &str) -> Option<(u8, u8, u8)> {
    let s = hex.trim();
    let s = s.strip_prefix('#').unwrap_or(s);
    if s.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some((r, g, b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_origins_have_valid_fields() {
        // Iterate ORIGINS directly so a new entry is auto-covered.
        assert!(!ORIGINS.is_empty(), "ORIGINS taxonomy is empty");
        for def in ORIGINS {
            assert!(!def.id.is_empty(), "origin id is empty");
            assert!(!def.label.is_empty(), "origin {} has empty label", def.id);
            assert!(
                parse_hex_rgb(def.color_hex).is_some(),
                "origin {} color {} fails to parse",
                def.id,
                def.color_hex
            );
            assert!(
                def.base_confidence > 0.0 && def.base_confidence <= 1.0,
                "origin {} confidence {} out of (0,1]",
                def.id,
                def.base_confidence
            );
            // Round-trip the public lookups so the helpers stay aligned with
            // the table.
            assert_eq!(origin(def.id).map(|o| o.label), Some(def.label));
            assert_eq!(color_hex_for(def.id), Some(def.color_hex));
            assert_eq!(label_for(def.id), Some(def.label));
            assert_eq!(base_confidence_for(def.id), Some(def.base_confidence));
        }
        assert!(origin("does-not-exist").is_none());
        assert!(color_hex_for("does-not-exist").is_none());
        assert!(label_for("does-not-exist").is_none());
        assert!(base_confidence_for("does-not-exist").is_none());
    }

    #[test]
    fn sort_order_prioritises_evidence_origins() {
        assert!(sort_order("pr_review") < sort_order("manual"));
        assert!(sort_order("extracted") < sort_order("conversation"));
        assert!(sort_order("manual") == sort_order("cloud"));
        assert!(sort_order("totally-unknown") == sort_order("cloud"));
    }

    #[test]
    fn distribution_sort_key_covers_known_origins_before_unknown() {
        assert_eq!(distribution_sort_key("conversation"), 0);
        assert_eq!(distribution_sort_key("manual"), 1);
        assert_eq!(distribution_sort_key("pr_review"), 2);
        assert_eq!(distribution_sort_key("extracted"), 3);
        assert_eq!(distribution_sort_key("cloud"), 4);
        assert_eq!(distribution_sort_key("team"), 5);
        assert_eq!(distribution_sort_key("agent-memory"), 6);
        assert_eq!(distribution_sort_key("unknown-x"), 7);
    }

    #[test]
    fn group_by_origin_buckets_rules() {
        use crate::domain::rule_view::RuleView;
        struct R {
            id: String,
            origin: String,
        }
        impl RuleView for R {
            fn id(&self) -> &str {
                &self.id
            }
            fn content(&self) -> &str {
                &self.id[..0]
            }
            fn origin(&self) -> &str {
                &self.origin
            }
            fn confidence(&self) -> Option<f64> {
                None
            }
        }
        let rules = vec![
            R {
                id: "a".into(),
                origin: "pr_review".into(),
            },
            R {
                id: "b".into(),
                origin: "manual".into(),
            },
            R {
                id: "c".into(),
                origin: "pr_review".into(),
            },
        ];
        let grouped = group_by_origin(&rules);
        assert_eq!(grouped.get("pr_review").map(Vec::len), Some(2));
        assert_eq!(grouped.get("manual").map(Vec::len), Some(1));
        assert!(!grouped.contains_key("missing"));
    }

    #[test]
    fn format_api_error_classifies_4xx_5xx_network_timeout_fallback() {
        let s = format_api_error("Sync", "API error 401: token revoked");
        assert!(s.contains("session expired"));
        assert!(s.contains("token revoked"), "raw retained: {s}");

        let s = format_api_error("Sync", r#"{"status":401,"message":"token revoked"}"#);
        assert!(s.contains("session expired"));

        let s = format_api_error("Sync", "API error 429: too many");
        assert!(s.contains("rate-limited"));

        let s = format_api_error("Sync", r#"API error 500: {"code":"INTERNAL_SERVER_ERROR"}"#);
        assert!(s.contains("server error"));

        let s = format_api_error("Sync", "request timed out after 30s");
        assert!(s.contains("timed out"));

        let s = format_api_error("Sync", "connection refused");
        assert!(s.to_lowercase().contains("unreachable"));

        let s = format_api_error("Sync", "os error 10061");
        assert!(s.to_lowercase().contains("unreachable"));

        let s = format_api_error("Sync", "totally novel xyz");
        assert!(s.contains("totally novel xyz"));
    }

    #[test]
    fn format_api_error_prefers_structured_status() {
        let s = format_api_error_with_status("Sync", "returned 401: token revoked", Some(401));
        assert!(s.contains("session expired"));
        assert!(s.contains("returned 401: token revoked"));

        let s =
            format_api_error_with_status("Sync", "returned 503: upstream unavailable", Some(503));
        assert!(s.contains("server error"));
        assert!(s.contains("returned 503"));
    }

    #[test]
    fn format_api_error_requires_exact_three_digit_statuses() {
        let s = format_api_error("Sync", "API error 50001: application code");
        assert!(!s.contains("server error"), "misclassified app code: {s}");
        assert_eq!(s, "Sync: API error 50001: application code");

        let s = format_api_error("Sync", r#"{"status":4010,"message":"app status"}"#);
        assert!(
            !s.contains("session expired"),
            "misclassified app code: {s}"
        );
        assert_eq!(s, r#"Sync: {"status":4010,"message":"app status"}"#);
    }

    #[test]
    fn parse_hex_rgb_round_trip() {
        assert_eq!(parse_hex_rgb("#CBA6F7"), Some((0xcb, 0xa6, 0xf7)));
        assert_eq!(parse_hex_rgb("CBA6F7"), Some((0xcb, 0xa6, 0xf7)));
        assert_eq!(parse_hex_rgb("#cba6f7"), Some((0xcb, 0xa6, 0xf7)));
        assert!(parse_hex_rgb("#XYZ123").is_none());
        assert!(parse_hex_rgb("#ABC").is_none());
        assert!(parse_hex_rgb("").is_none());
    }
}
