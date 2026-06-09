//! Centred-rect helpers shared by the modal layer and the app chrome.
//!
//! There used to be six near-identical copies of this: the
//! percentage-based `centered_rect` and the absolute `centered_rect_abs`
//! in `app::render`, plus a byte-for-byte `center_rect` duplicated into
//! each of the four modal files. Both capabilities now live here once.

use ratatui::layout::{Constraint, Direction, Layout, Rect};

/// Centre a sub-rect sized as a percentage of `area` (both axes).
///
/// `percent_x` / `percent_y` are 0..=100. Used for modals that should
/// scale with the terminal rather than pin to a fixed glyph size.
pub fn centered_rect_pct(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

/// Centre a sub-rect of an absolute `width` × `height`, clamped to
/// `area` so an oversized request never escapes the parent. Used for
/// fixed-size art (modals, the help overlay).
pub fn centered_rect_abs(width: u16, height: u16, area: Rect) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    Rect {
        x,
        y,
        width: w,
        height: h,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abs_clamps_to_parent() {
        let parent = Rect::new(0, 0, 50, 12);
        let r = centered_rect_abs(100, 100, parent);
        assert_eq!(r.width, 50);
        assert_eq!(r.height, 12);
    }

    #[test]
    fn abs_centers_inside_parent() {
        let parent = Rect::new(0, 0, 80, 24);
        let r = centered_rect_abs(40, 12, parent);
        assert_eq!(r.width, 40);
        assert_eq!(r.height, 12);
        assert_eq!(r.x, 20);
        assert_eq!(r.y, 6);
    }

    #[test]
    fn pct_half_is_centered_quarter_offset() {
        let parent = Rect::new(0, 0, 100, 100);
        let r = centered_rect_pct(50, 50, parent);
        assert_eq!(r.width, 50);
        assert_eq!(r.height, 50);
        assert_eq!(r.x, 25);
        assert_eq!(r.y, 25);
    }
}
