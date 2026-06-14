//! `DiffLore` TUI theme palette.
//!
//! Dark mode uses the pewter + emerald brand palette. Light mode is
//! opt-in via `~/.difflore/config.toml` `theme = "light"` and shifts
//! brand/status colors to text-safe variants for light terminals.
//! Config-file IO and the mtime-debounced cache live in [`source`].

mod source;

use ratatui::style::{Color, Style};

#[derive(Clone, Copy, Debug)]
pub struct Theme {
    // Surfaces
    pub bg: Color,
    pub surface: Color,
    pub highlight_bg: Color,
    pub crust: Color,

    // Text + chrome
    pub foreground: Color,
    pub muted: Color,
    pub subtle: Color,
    pub border: Color,

    // Brand
    pub diff: Color,
    pub lore: Color,
    pub accent: Color,
    pub accent_muted: Color,
    pub accent_ink: Color,

    // Status
    pub danger: Color,
    pub warn: Color,
    pub success: Color,
    pub info: Color,

    // Origin
    pub origin_conversation: Color,
    pub origin_manual: Color,
    pub origin_pr_review: Color,
    pub origin_extracted: Color,
    pub origin_cloud: Color,
    pub origin_team: Color,
}

impl Theme {
    pub const DARK: Self = Self {
        // Cool-shifted dark surfaces — matches DiffLore Cloud v2.
        bg: Color::Rgb(0x0a, 0x0c, 0x10),
        surface: Color::Rgb(0x12, 0x15, 0x19),
        highlight_bg: Color::Rgb(0x1f, 0x24, 0x2a),
        crust: Color::Rgb(0x06, 0x07, 0x0a),

        foreground: Color::Rgb(0xea, 0xec, 0xef),
        muted: Color::Rgb(0x9a, 0xa2, 0xab),
        subtle: Color::Rgb(0x6f, 0x78, 0x82),
        border: Color::Rgb(0x26, 0x2c, 0x33),

        diff: Color::Rgb(0x8a, 0x96, 0x91),
        // Brand emerald — only for brand identity & highlights. Status
        // greens use a different hex so "ok" ≠ "branded".
        lore: Color::Rgb(0x16, 0xb9, 0x68),
        // TUI `accent` is a fg highlight (not a solid-button bg), so we
        // can't use cloud's neutral-inverse — keep it as the emerald and
        // rely on `accent_ink` for any text-on-emerald moments.
        accent: Color::Rgb(0x16, 0xb9, 0x68),
        accent_muted: Color::Rgb(0x0e, 0x24, 0x18),
        accent_ink: Color::Rgb(0x0a, 0x0c, 0x10),

        // Status text values (cloud `*.text dark`) — readable on the dark
        // surface, hue-separated from `lore` so "success" ≠ "brand".
        danger: Color::Rgb(0xf1, 0x77, 0x80),
        warn: Color::Rgb(0xe8, 0xb7, 0x5a),
        success: Color::Rgb(0x5f, 0xcf, 0x99),
        info: Color::Rgb(0x92, 0xb3, 0xf3),

        origin_conversation: Color::Rgb(0x7f, 0xbc, 0xff),
        origin_manual: Color::Rgb(0xc8, 0xa8, 0xff),
        origin_pr_review: Color::Rgb(0xf5, 0xa2, 0x5e),
        // Jade, kept distinct from brand emerald so origin tag ≠ brand color.
        origin_extracted: Color::Rgb(0x22, 0xd3, 0xa8),
        origin_cloud: Color::Rgb(0x5e, 0xe0, 0xc8),
        origin_team: Color::Rgb(0xff, 0xd8, 0x6b),
    };

    /// Light terminal variant with text-safe pewter / emerald accents.
    pub const LIGHT: Self = Self {
        bg: Color::Rgb(0xf6, 0xf7, 0xf5),
        surface: Color::Rgb(0xff, 0xff, 0xff),
        highlight_bg: Color::Rgb(0xee, 0xf0, 0xec),
        crust: Color::Rgb(0xe8, 0xea, 0xe5),

        foreground: Color::Rgb(0x0d, 0x14, 0x11),
        muted: Color::Rgb(0x52, 0x5e, 0x5a),
        subtle: Color::Rgb(0x6c, 0x77, 0x73),
        border: Color::Rgb(0xd8, 0xdd, 0xd5),

        diff: Color::Rgb(0x6e, 0x7a, 0x78),
        // Light bg needs the darker emerald variant for text contrast
        // (cloud's `lore-text light`). Pure `#16b968` washes out on
        // white at 2.3:1.
        lore: Color::Rgb(0x0e, 0x7c, 0x44),
        accent: Color::Rgb(0x0e, 0x7c, 0x44),
        accent_muted: Color::Rgb(0xdc, 0xf2, 0xe3),
        accent_ink: Color::Rgb(0xff, 0xff, 0xff),

        // Status text values for light terminals (cloud `*.text light`).
        danger: Color::Rgb(0xb4, 0x1f, 0x29),
        warn: Color::Rgb(0x7a, 0x4f, 0x00),
        success: Color::Rgb(0x0a, 0x6f, 0x43),
        info: Color::Rgb(0x23, 0x56, 0xc5),

        // Origin glyphs are dot/border-only at 8–12px — keep canvas
        // hex; the 3:1 large-graphical-element WCAG floor passes on
        // both backgrounds.
        origin_conversation: Color::Rgb(0x7f, 0xbc, 0xff),
        origin_manual: Color::Rgb(0xc8, 0xa8, 0xff),
        origin_pr_review: Color::Rgb(0xf5, 0xa2, 0x5e),
        origin_extracted: Color::Rgb(0x22, 0xd3, 0xa8),
        origin_cloud: Color::Rgb(0x5e, 0xe0, 0xc8),
        origin_team: Color::Rgb(0xff, 0xd8, 0x6b),
    };

    /// Resolve the active theme. The config file is debounced so the
    /// draw path can call this freely without re-reading and re-parsing
    /// `~/.difflore/config.toml` dozens of times per frame.
    pub fn current() -> Self {
        source::cached_current_theme()
    }
}

pub fn box_title(t: &Theme) -> Style {
    Style::default().fg(t.lore).bg(t.bg)
}

// Convert a `#RRGGBB` hex into a ratatui `Color`, falling back to
// the active theme's `muted` on malformed input.
pub fn hex_to_color(hex: &str) -> Color {
    match difflore_core::domain::origins::parse_hex_rgb(hex) {
        Some((r, g, b)) => Color::Rgb(r, g, b),
        None => Theme::current().muted,
    }
}

/// Shared origin-to-color mapping through the bundled origin taxonomy.
pub(crate) fn origin_color(origin: &str) -> Color {
    match difflore_core::domain::origins::color_hex_for(origin) {
        Some(hex) => hex_to_color(hex),
        None => Theme::current().muted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_origins_match_brand_canvas_hexes() {
        assert_eq!(
            Theme::DARK.origin_conversation,
            Color::Rgb(0x7f, 0xbc, 0xff)
        );
        assert_eq!(Theme::DARK.lore, Color::Rgb(0x16, 0xb9, 0x68));
    }

    #[test]
    fn current_returns_dark_or_light() {
        // Verify the mapping returns one of the two known palettes; TOML
        // parsing is covered by core::infra::config tests.
        let t = Theme::current();
        let is_dark = t.bg == Theme::DARK.bg;
        let is_light = t.bg == Theme::LIGHT.bg;
        assert!(
            is_dark || is_light,
            "Theme::current must map to DARK or LIGHT"
        );
    }

    #[test]
    fn origin_color_round_trips_through_registry() {
        let muted = Theme::current().muted;
        for id in [
            "manual",
            "conversation",
            "pr_review",
            "extracted",
            "cloud",
            "team",
        ] {
            #[allow(clippy::panic)] // reason: test invariant — taxonomy must list every id
            let hex = difflore_core::domain::origins::color_hex_for(id)
                .unwrap_or_else(|| panic!("missing {id}"));
            let expected = hex_to_color(hex);
            assert_eq!(origin_color(id), expected, "round-trip failed for {id}");
            assert_ne!(origin_color(id), muted, "{id} fell back to muted");
        }
    }

    #[test]
    fn unknown_origin_falls_back_to_muted() {
        assert_eq!(origin_color("not-a-real-origin"), Theme::current().muted);
    }
}
