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

/// Recommended display priority: evidence-bearing origins
/// (`pr_review`, `extracted`) sort first, everything else after.
/// Used by `rules list --view recommended` and any other
/// "high-value first" surface.
pub fn sort_order(origin: &str) -> u8 {
    match origin {
        "pr_review" | "extracted" => 0,
        _ => 1,
    }
}

/// Distribution-histogram ordering for the rule mix UI in the TUI:
/// frequency / value mixed, finer-grained than `sort_order`. Stable
/// ordering across releases is the contract — callers rely on this for
/// chart layout.
pub fn distribution_sort_key(origin: &str) -> u8 {
    match origin {
        "conversation" => 0,
        "manual" => 1,
        "pr_review" => 2,
        "extracted" => 3,
        "cloud" => 4,
        _ => 5,
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

/// Generic HTTP / network / timeout error classifier shared by
/// domain-specific formatters (`cli::format_cloud_err`,
/// `cli::format_github_import_err`). Returns a user-facing message;
/// keeps the raw error in the output on the unrecognised path so triage
/// info isn't lost.
///
/// Domain-specific framings (cloud BYOK guidance, GitHub auth hints)
/// belong in the cli wrappers — this layer must not bake product
/// language for a specific surface.
pub fn format_api_error(label: &str, raw: &str) -> String {
    let lower = raw.to_ascii_lowercase();
    if raw.contains("API error 401") || raw.contains("status\":401") {
        return format!("{label}: session expired or revoked (401).\n\n  raw: {raw}");
    }
    if raw.contains("API error 403") || raw.contains("status\":403") {
        return format!("{label}: request rejected (403).\n\n  raw: {raw}");
    }
    if raw.contains("API error 429") || raw.contains("status\":429") {
        return format!("{label}: rate-limited (429).\n\n  raw: {raw}");
    }
    if raw.contains("API error 5") || raw.contains("INTERNAL_SERVER_ERROR") {
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
        // Iterate ORIGINS directly so adding a new entry is auto-covered;
        // a hand-maintained list silently skipped new origins until a
        // reviewer noticed and updated the test.
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
            // Round-trip the public lookups so the helpers stay aligned
            // with the table.
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
    fn distribution_sort_key_six_buckets() {
        assert_eq!(distribution_sort_key("conversation"), 0);
        assert_eq!(distribution_sort_key("manual"), 1);
        assert_eq!(distribution_sort_key("pr_review"), 2);
        assert_eq!(distribution_sort_key("extracted"), 3);
        assert_eq!(distribution_sort_key("cloud"), 4);
        assert_eq!(distribution_sort_key("agent-memory"), 5);
        assert_eq!(distribution_sort_key("unknown-x"), 5);
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
    fn parse_hex_rgb_round_trip() {
        assert_eq!(parse_hex_rgb("#CBA6F7"), Some((0xcb, 0xa6, 0xf7)));
        assert_eq!(parse_hex_rgb("CBA6F7"), Some((0xcb, 0xa6, 0xf7)));
        assert_eq!(parse_hex_rgb("#cba6f7"), Some((0xcb, 0xa6, 0xf7)));
        assert!(parse_hex_rgb("#XYZ123").is_none());
        assert!(parse_hex_rgb("#ABC").is_none());
        assert!(parse_hex_rgb("").is_none());
    }
}
