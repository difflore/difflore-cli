//! Shared hook signal classifiers.
//!
//! Keep these tiny, dependency-free helpers in core so the short-lived hook
//! shim and the full runtime make the same skip/defer decision.

pub const BASH_MIN_ERROR_OUTPUT_CHARS: usize = 50;

#[must_use]
pub fn bash_output_is_high_signal_failure(output: &str) -> bool {
    output.contains("Traceback (most recent call last):")
        || output.contains("panic:")
        || output.contains("panicked at")
        || output.contains("error[E")
        || output.contains("FATAL:")
        || bash_errorish_line_count(output) >= 2
}

#[must_use]
pub fn bash_errorish_line_count(output: &str) -> usize {
    output
        .lines()
        .filter(|line| bash_line_is_errorish(line))
        .count()
}

#[must_use]
pub fn bash_line_is_meaningful_error(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.contains("Traceback")
        || trimmed.contains("panic:")
        || trimmed.contains("panicked at")
        || trimmed.contains("error[E")
        || trimmed.contains("FATAL:")
        || bash_line_is_errorish(trimmed)
}

fn bash_line_is_errorish(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with("Error:")
        || trimmed.starts_with("Exception:")
        || trimmed.contains(" Exception:")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn high_signal_matches_known_runtime_error_shapes() {
        assert!(bash_output_is_high_signal_failure(
            "Traceback (most recent call last):\nValueError: bad"
        ));
        assert!(bash_output_is_high_signal_failure(
            "thread panicked at src/main.rs:1"
        ));
        assert!(bash_output_is_high_signal_failure(
            "error[E0308]: mismatched types"
        ));
        assert!(bash_output_is_high_signal_failure(
            "FATAL: database unavailable"
        ));
        assert!(bash_output_is_high_signal_failure(
            "Error: first\nError: second"
        ));
    }

    #[test]
    fn high_signal_ignores_single_generic_error_line() {
        assert!(!bash_output_is_high_signal_failure(
            "Error: one-off tool message"
        ));
        assert_eq!(bash_errorish_line_count("Error: one\nException: two"), 2);
    }
}
