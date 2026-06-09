//! Review cost transparency.
//!
//! A small pricing table for the LLM models `DiffLore` actually calls, plus
//! a helper to estimate the USD cost of a single review turn from the
//! provider's returned `usage` block.
//!
//! The table is intentionally conservative: unknown models return `None`
//! rather than guessing, so downstream code can persist `NULL` and the
//! cloud aggregation skips the row instead of under-reporting.

/// Per-1K-token pricing for a single LLM model. Mirrors the public pricing
/// pages for each provider as of the plan date; update when providers
/// change their rate cards.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelPricing {
    pub input_usd_per_1k: f64,
    pub output_usd_per_1k: f64,
}

/// Look up the per-1K pricing for a model. Returns `None` for unknown
/// identifiers so callers can record `NULL` and avoid under/over-reporting.
pub fn pricing_for(model: &str) -> Option<ModelPricing> {
    match model {
        // ── Anthropic ──
        // Sonnet 4 snapshot ids share the same published rate card. Keep both
        // entries so archived fix_runs.ai_model values still resolve.
        "claude-sonnet-4-20250514" | "claude-sonnet-4-6" => Some(ModelPricing {
            input_usd_per_1k: 0.003,
            output_usd_per_1k: 0.015,
        }),
        "claude-haiku-4-5-20251001" | "claude-haiku-4-5" => Some(ModelPricing {
            input_usd_per_1k: 0.0008,
            output_usd_per_1k: 0.004,
        }),
        "claude-opus-4-6" | "claude-opus-4-7" => Some(ModelPricing {
            input_usd_per_1k: 0.015,
            output_usd_per_1k: 0.075,
        }),
        // ── OpenAI ──
        "gpt-4o" => Some(ModelPricing {
            input_usd_per_1k: 0.005,
            output_usd_per_1k: 0.015,
        }),
        "gpt-4o-mini" => Some(ModelPricing {
            input_usd_per_1k: 0.00015,
            output_usd_per_1k: 0.0006,
        }),
        _ => None,
    }
}

/// Compute the estimated USD cost of a single LLM call.
///
/// Returns `None` for unknown models. The arithmetic is deliberately simple
/// (no rounding, no currency conversion); downstream consumers persist the
/// value in a `numeric(10, 6)` column where it will be rounded once.
pub fn estimate_cost_usd(model: &str, input_tokens: u32, output_tokens: u32) -> Option<f64> {
    let p = pricing_for(model)?;
    let cost = (f64::from(input_tokens) / 1000.0).mul_add(
        p.input_usd_per_1k,
        (f64::from(output_tokens) / 1000.0) * p.output_usd_per_1k,
    );
    Some(cost)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    clippy::float_cmp
)] // reason: tests assert exact values
mod tests {
    use super::*;

    #[test]
    fn pricing_for_known_models_table() {
        // (model, input_per_1k, output_per_1k). Sonnet 4 aliases share a rate
        // card so archived fix_runs values still resolve.
        let cases: &[(&str, f64, f64)] = &[
            ("claude-sonnet-4-20250514", 0.003, 0.015),
            ("claude-sonnet-4-6", 0.003, 0.015),
            ("claude-haiku-4-5-20251001", 0.0008, 0.004),
            ("claude-opus-4-6", 0.015, 0.075),
            ("gpt-4o", 0.005, 0.015),
            ("gpt-4o-mini", 0.00015, 0.0006),
        ];
        for (model, input, output) in cases {
            let p = pricing_for(model).unwrap_or_else(|| panic!("missing: {model}"));
            assert_eq!(p.input_usd_per_1k, *input, "model: {model}");
            assert_eq!(p.output_usd_per_1k, *output, "model: {model}");
        }
    }

    #[test]
    fn pricing_for_unknown_or_miscased_model_returns_none() {
        // Strict lookup: never silently attribute Sonnet pricing to a
        // miscapitalised tag.
        for m in ["", "gpt-5-secret", "CLAUDE-SONNET-4-20250514", "GPT-4o"] {
            assert!(pricing_for(m).is_none(), "unexpectedly priced: {m}");
        }
    }

    #[test]
    fn estimate_cost_usd_linear_and_edge_cases() {
        // Sonnet: 1000 in + 500 out = 0.003 + 0.5 * 0.015 = 0.0105
        let cost = estimate_cost_usd("claude-sonnet-4-20250514", 1000, 500).unwrap();
        assert!((cost - 0.0105).abs() < 1e-9, "sonnet got {cost}");

        // gpt-4o-mini: 10_000 in + 1_000 out = 0.0015 + 0.0006 = 0.0021
        let cost = estimate_cost_usd("gpt-4o-mini", 10_000, 1_000).unwrap();
        assert!((cost - 0.0021).abs() < 1e-9, "mini got {cost}");

        // Edge cases: zero tokens, unknown model.
        assert_eq!(
            estimate_cost_usd("claude-sonnet-4-20250514", 0, 0).unwrap(),
            0.0
        );
        assert!(estimate_cost_usd("unknown-model", 1000, 500).is_none());

        // Output dominates total for typical review turns (Sonnet rate card).
        let cost = estimate_cost_usd("claude-sonnet-4-20250514", 2_000, 500).unwrap();
        let input_cost = (2_000.0 / 1000.0) * 0.003;
        let output_cost = (500.0 / 1000.0) * 0.015;
        assert!((cost - (input_cost + output_cost)).abs() < 1e-9);
        assert!(output_cost > input_cost);
    }
}
