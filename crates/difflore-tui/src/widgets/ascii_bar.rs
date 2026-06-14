//! Horizontal proportion bar for quota and capacity displays.
//!
//! Returns a UTF-8 string like `████████░░░░░░░░` whose filled portion
//! equals `value / total` of `width` columns. `total == 0` and `width == 0`
//! yield empty strings. The proportion is saturating: values above `total`
//! clamp to `width` filled cells.

const FILL: char = '█';
const EMPTY: char = '░';

/// Integer-count proportional bar for quota/capacity displays. Avoids
/// lossy float casts for the `used / quota` case.
pub fn ascii_bar_counts(value: u32, total: u32, width: usize) -> String {
    if width == 0 || total == 0 {
        return String::new();
    }
    let capped = value.min(total);
    let filled = rounded_cells_from_counts(capped, total, width);
    render_cells(filled, width)
}

fn render_cells(filled: usize, width: usize) -> String {
    let filled = filled.min(width);
    let mut out = String::with_capacity(width);
    for _ in 0..filled {
        out.push(FILL);
    }
    for _ in 0..(width - filled) {
        out.push(EMPTY);
    }
    out
}

fn rounded_cells_from_counts(value: u32, total: u32, width: usize) -> usize {
    let width_u64 = u64::try_from(width).unwrap_or(u64::MAX);
    let numerator = u128::from(value) * u128::from(width_u64) * 2 + u128::from(total);
    let denom = u128::from(total) * 2;
    let cells = numerator / denom;
    usize::try_from(cells).unwrap_or(width).min(width)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_width_or_zero_total_is_empty() {
        assert_eq!(ascii_bar_counts(1, 2, 0), "");
        assert_eq!(ascii_bar_counts(1, 0, 4), "");
    }

    #[test]
    fn full_value_fills_completely() {
        assert_eq!(ascii_bar_counts(2, 2, 4), "████");
    }

    #[test]
    fn half_value_fills_half() {
        let s = ascii_bar_counts(1, 2, 4);
        assert_eq!(s.chars().filter(|&c| c == '█').count(), 2);
        assert_eq!(s.chars().filter(|&c| c == '░').count(), 2);
    }

    #[test]
    fn over_value_saturates() {
        assert_eq!(ascii_bar_counts(10, 2, 4), "████");
    }

    #[test]
    fn count_variant_rounds_without_float_casts() {
        assert_eq!(ascii_bar_counts(1, 2, 5), "███░░");
        assert_eq!(ascii_bar_counts(10, 2, 4), "████");
        assert_eq!(ascii_bar_counts(1, 0, 4), "");
    }
}
