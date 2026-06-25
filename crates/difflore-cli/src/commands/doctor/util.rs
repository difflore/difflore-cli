//! Small formatting helpers shared across the doctor table and `--report`
//! surfaces.

/// Render a millisecond age as a coarse `Ns/m/h/d ago` label. Shared by the
/// default table (`table.rs`) and the markdown report (`report/formatters.rs`)
/// so the two never drift on rounding or unit boundaries.
pub(in crate::commands::doctor) fn age_label_ms(ms: i64) -> String {
    let secs = (ms / 1000).max(0);
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3_600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3_600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}
