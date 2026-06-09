//! Optional sampler that bumps a small fraction of `top_k == 5` serves to
//! `top_k = 8` so recall ledgers collect data on deeper ranks.
//!
//! Explicit caller-chosen `top_k` values pass through unchanged, and a
//! `0.0` rate is a strict off-switch.
//!
//! The caller records the returned value in `mcp_rule_serves.top_k`.

use rand::{Rng, RngExt};

/// The caller-requested `top_k` value the sampler targets. Only serves
/// using this exact value are eligible for a bump; everything else is
/// passed through unchanged.
pub(crate) const SAMPLER_TRIGGER_TOP_K: usize = 5;

/// Bumped `top_k` written for sampled serves.
pub(crate) const SAMPLER_BUMPED_TOP_K: usize = 8;

/// Possibly bump a caller-requested `top_k` from
/// [`SAMPLER_TRIGGER_TOP_K`] to [`SAMPLER_BUMPED_TOP_K`].
///
/// Behaviour summary:
/// * `requested_top_k != SAMPLER_TRIGGER_TOP_K` → return unchanged
///   (explicit caller choice wins).
/// * `sample_rate <= 0.0` → return unchanged (off-switch).
/// * Otherwise roll a uniform `f32` in `[0.0, 1.0)`; if it falls below
///   `sample_rate`, return [`SAMPLER_BUMPED_TOP_K`], else return unchanged.
///
/// `sample_rate` is **not** clamped here — the env-layer accessor
/// (`env::deep_recall_sample_rate`) is the single validation point and
/// guarantees the value is in `[0.0, env::MAX_DEEP_RECALL_SAMPLE_RATE]`.
/// Passing a value outside that range from test code is fine; the
/// behaviour is still defined (`rate >= 1.0` always bumps, `rate <= 0.0`
/// never bumps).
#[must_use]
pub(crate) fn maybe_bump_top_k(requested_top_k: usize, sample_rate: f32) -> usize {
    maybe_bump_top_k_with_rng(requested_top_k, sample_rate, &mut rand::rng())
}

/// Testable variant of [`maybe_bump_top_k`] with an injected RNG.
#[must_use]
pub(crate) fn maybe_bump_top_k_with_rng<R: Rng + ?Sized>(
    requested_top_k: usize,
    sample_rate: f32,
    rng: &mut R,
) -> usize {
    if requested_top_k != SAMPLER_TRIGGER_TOP_K {
        return requested_top_k;
    }
    if sample_rate <= 0.0 {
        return requested_top_k;
    }
    let roll: f32 = rng.random();
    if roll < sample_rate {
        SAMPLER_BUMPED_TOP_K
    } else {
        requested_top_k
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use rand::rngs::StdRng;

    /// Wide tolerance band keeps the stochastic assertion stable.
    const TRIALS: usize = 1000;
    const TOLERANCE_LOWER: usize = 5;
    const TOLERANCE_UPPER: usize = 35;

    #[test]
    fn bumps_within_tolerance_band_at_default_rate() {
        // Seeded RNG so the test is deterministic across runs.
        let mut rng = StdRng::seed_from_u64(0xD1FF_10AE_DEEE_EC8C);
        let mut bumped = 0usize;
        for _ in 0..TRIALS {
            if maybe_bump_top_k_with_rng(5, 0.02, &mut rng) == SAMPLER_BUMPED_TOP_K {
                bumped += 1;
            }
        }
        assert!(
            (TOLERANCE_LOWER..=TOLERANCE_UPPER).contains(&bumped),
            "expected {TOLERANCE_LOWER}..={TOLERANCE_UPPER} bumps over {TRIALS} trials at rate=0.02, got {bumped}"
        );
    }

    #[test]
    fn never_bumps_when_rate_is_zero() {
        let mut rng = StdRng::seed_from_u64(0x0FF5_FACE_DEAD_BEEF);
        for _ in 0..TRIALS {
            assert_eq!(maybe_bump_top_k_with_rng(5, 0.0, &mut rng), 5);
        }
    }

    #[test]
    fn never_bumps_when_caller_requested_non_five_top_k() {
        // Explicit caller choices must not be overridden.
        let mut rng = StdRng::seed_from_u64(0xCAFE_BABE_DEAD_BEEF);
        for caller_choice in [1usize, 2, 3, 4, 6, 7, 8, 10, 20, 50] {
            for _ in 0..TRIALS {
                assert_eq!(
                    maybe_bump_top_k_with_rng(caller_choice, 1.0, &mut rng),
                    caller_choice,
                    "sampler must not override caller-chosen top_k={caller_choice}"
                );
            }
        }
    }

    #[test]
    fn always_bumps_when_rate_is_one() {
        // Confirms the comparison direction.
        let mut rng = StdRng::seed_from_u64(0xA11_BBBB_8765_4321);
        for _ in 0..TRIALS {
            assert_eq!(
                maybe_bump_top_k_with_rng(5, 1.0, &mut rng),
                SAMPLER_BUMPED_TOP_K
            );
        }
    }

    #[test]
    fn never_bumps_when_rate_is_negative() {
        // Treat any non-positive rate as the off-switch.
        let mut rng = StdRng::seed_from_u64(0xB16_CAFE_FEED_0001);
        for _ in 0..TRIALS {
            assert_eq!(maybe_bump_top_k_with_rng(5, -0.5, &mut rng), 5);
        }
    }
}
