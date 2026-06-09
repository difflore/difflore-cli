//! CLI style helpers. The public terminal surface uses an ASCII-safe
//! 5-symbol set so Windows logs, CI, and screenshot captures stay aligned.
//! NO_COLOR env-var disables coloring; support is detected once at startup via
//! `detect_color_support()` and cached.

use std::io::{IsTerminal, stdout};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use colored::{ColoredString, Colorize};

// ── Color palette ────────────────────────────────────────────────
//
// Two tuned ramps. The dark ramp is the original brand palette; the
// light ramp is darkened so every role clears WCAG 4.5:1 on white and
// cream backgrounds (the dark ramp's greens/ambers/blues fall to
// ~2.2:1 on white). `background()` picks the ramp; see `Background`.

pub fn pewter(s: &str) -> ColoredString {
    match background() {
        Background::Light => paint(s, 0x55, 0x5c, 0x5f),
        Background::Dark => paint(s, 0x7d, 0x85, 0x88),
    }
}
pub fn emerald(s: &str) -> ColoredString {
    match background() {
        Background::Light => paint(s, 0x15, 0x80, 0x3d),
        Background::Dark => paint(s, 0x22, 0xc5, 0x5e),
    }
}
pub fn amber(s: &str) -> ColoredString {
    match background() {
        Background::Light => paint(s, 0x8a, 0x62, 0x00),
        Background::Dark => paint(s, 0xe0, 0xa5, 0x2a),
    }
}
pub fn danger(s: &str) -> ColoredString {
    match background() {
        Background::Light => paint(s, 0xc0, 0x1c, 0x1c),
        Background::Dark => paint(s, 0xef, 0x54, 0x54),
    }
}
pub fn info(s: &str) -> ColoredString {
    match background() {
        Background::Light => paint(s, 0x1d, 0x6c, 0xd4),
        Background::Dark => paint(s, 0x5a, 0xa0, 0xf2),
    }
}

// Runnable commands get their own token color (info/blue) so "things
// you can type" read as a distinct class, not as bold narration. Kept
// non-bold: the hue carries the affordance, weight would over-shout.
pub fn cmd(s: &str) -> ColoredString {
    info(s)
}
// Primary-content emphasis for non-command text: result titles, recalled
// rule names, verdict text. Bold default ink so it reads as the headline
// of an entry without borrowing the blue "runnable command" affordance.
pub fn title(s: &str) -> ColoredString {
    match color_support() {
        ColorSupport::None => s.normal(),
        _ => s.bold(),
    }
}
// Inline identifiers that are neither runnable commands nor headlines:
// file paths, ids, repo names, counts, agent lists. Bold neutral (pewter)
// so they stand out as distinct tokens in prose without the command blue.
pub fn ident(s: &str) -> ColoredString {
    pewter(s).bold()
}
// Bold variants — used for status verbs.
pub fn ok(s: &str) -> ColoredString {
    emerald(s).bold()
}
pub fn warn(s: &str) -> ColoredString {
    amber(s).bold()
}
pub fn err(s: &str) -> ColoredString {
    danger(s).bold()
}

fn paint(s: &str, r: u8, g: u8, b: u8) -> ColoredString {
    match color_support() {
        ColorSupport::None => s.normal(),
        _ => s.truecolor(r, g, b),
    }
}

// ── Symbols — ASCII-safe public status set ───────────────────────

pub mod sym {
    pub const OK: &str = "OK";
    pub const ERR: &str = "X";
    pub const WARN: &str = "!";
    pub const TIP: &str = ">";
    pub const BULLET: &str = "-";
}

// ── Color support detection ──────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorSupport {
    TrueColor,
    Ansi256,
    None,
}

static COLOR_CACHE: OnceLock<ColorSupport> = OnceLock::new();

// Cached after first call. Call once at startup from `main`.
pub fn detect_color_support() -> ColorSupport {
    *COLOR_CACHE.get_or_init(detect_color_support_uncached)
}

fn color_support() -> ColorSupport {
    *COLOR_CACHE.get_or_init(detect_color_support_uncached)
}

// ── Background detection (light vs dark terminal) ────────────────
//
// The dark ramp is unreadable on a light terminal. We pick the light
// ramp when the terminal advertises a light background. Detection is
// best-effort and conservative — default to Dark, since most dev
// terminals are dark and a wrong Light guess would wash colors out.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Background {
    Dark,
    Light,
}

static BG_CACHE: OnceLock<Background> = OnceLock::new();

fn background() -> Background {
    *BG_CACHE.get_or_init(detect_background_uncached)
}

fn detect_background_uncached() -> Background {
    use difflore_core::env;
    // Explicit override always wins.
    if let Some(theme) = env::var(env::DIFFLORE_THEME) {
        match theme.trim().to_ascii_lowercase().as_str() {
            "light" => return Background::Light,
            "dark" => return Background::Dark,
            _ => {}
        }
    }
    // COLORFGBG is "fg;bg" (sometimes "fg;default;bg"); bg is the last
    // field. ANSI indices 0-6 and 8 are dark, 7/9-15 are light.
    if let Some(fgbg) = env::var(env::COLORFGBG)
        && let Some(bg) = fgbg.rsplit(';').next()
        && let Ok(idx) = bg.trim().parse::<u8>()
    {
        return if matches!(idx, 0..=6 | 8) {
            Background::Dark
        } else {
            Background::Light
        };
    }
    Background::Dark
}

fn detect_color_support_uncached() -> ColorSupport {
    use difflore_core::env;
    if env::flag_set(env::NO_COLOR) {
        return ColorSupport::None;
    }
    if !stdout().is_terminal() {
        return ColorSupport::None;
    }
    match env::var(env::COLORTERM).as_deref() {
        Some("truecolor" | "24bit") => ColorSupport::TrueColor,
        _ => match env::var(env::TERM).as_deref() {
            Some(t) if t.contains("256color") => ColorSupport::Ansi256,
            _ => ColorSupport::TrueColor, // default optimistic
        },
    }
}

// ── Error reporter ───────────────────────────────────────────────

/// Locked vocabulary for `Hint::label` — currently `"try"`.
/// New labels require a CONTRACTS amendment.
pub struct Hint {
    pub label: &'static str,
    pub body: String,
}

impl Hint {
    pub(crate) fn try_(body: impl Into<String>) -> Self {
        Self {
            label: "try",
            body: body.into(),
        }
    }
}

/// Print a uniform error block.
///
/// Layout:
/// ```text
/// X error - <summary>
///
///     <context line 1>
///     <context line 2>
///
///   > try   <hint body>
///   > docs  <hint body>
/// ```
pub fn report_error(summary: &str, context: &str, hints: &[Hint]) {
    eprintln!(
        "{} {} {} {}",
        danger(sym::ERR),
        err("error"),
        pewter(sym::BULLET),
        summary
    );
    eprintln!();
    if !context.is_empty() {
        for line in context.lines() {
            eprintln!("    {line}");
        }
        eprintln!();
    }
    for h in hints {
        eprintln!("  {} {} {}", emerald(sym::TIP), pewter(h.label), h.body);
    }
}

// ── Spinner ──────────────────────────────────────────────────────

const SPIN_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

// Caller invokes `tick()` between async awaits; `finish_ok` /
// `finish_err` clears the line and prints the final glyph.
pub struct Spinner {
    label: String,
    frame: std::cell::Cell<usize>,
    last_tick: std::cell::Cell<Instant>,
    tick_interval: Duration,
}

impl Spinner {
    pub(crate) fn new(label: &str) -> Self {
        let s = Self {
            label: label.to_owned(),
            frame: std::cell::Cell::new(0),
            last_tick: std::cell::Cell::new(Instant::now()),
            tick_interval: Duration::from_millis(80),
        };
        // Render the first frame immediately so callers see the
        // spinner without waiting for the first await boundary.
        s.draw();
        s
    }

    pub(crate) fn tick(&self) {
        let now = Instant::now();
        if now.duration_since(self.last_tick.get()) < self.tick_interval {
            return;
        }
        self.last_tick.set(now);
        self.frame.set((self.frame.get() + 1) % SPIN_FRAMES.len());
        self.draw();
    }

    fn draw(&self) {
        if color_support() == ColorSupport::None {
            // No-color terminals: skip the animation; the final
            // glyph + message in `finish_*` is enough.
            return;
        }
        let glyph = SPIN_FRAMES[self.frame.get()];
        // \r writes over the in-progress line; flush via println-
        // family is intentional — spinner runs on stderr to keep
        // stdout clean for piped JSON.
        eprint!("\r{} {}  ", pewter(glyph), self.label);
        let _ = std::io::Write::flush(&mut std::io::stderr());
    }

    pub(crate) fn set_message(&mut self, msg: &str) {
        msg.clone_into(&mut self.label);
        self.draw();
    }

    /// Final line: `OK <msg>` (emerald), spinner cleared.
    pub(crate) fn finish_ok(self, msg: &str) {
        self.clear_line();
        eprintln!("{} {}", emerald(sym::OK), msg);
    }

    /// Final line: `X <msg>` (danger), spinner cleared.
    pub(crate) fn finish_err(self, msg: &str) {
        self.clear_line();
        eprintln!("{} {}", danger(sym::ERR), msg);
    }

    fn clear_line(&self) {
        if color_support() == ColorSupport::None {
            return;
        }
        // Clear the spinner's line: `\r` to home, spaces to wipe,
        // then `\r` again so the next write starts at column 0.
        let pad = " ".repeat(self.label.chars().count() + 6);
        eprint!("\r{pad}\r");
        let _ = std::io::Write::flush(&mut std::io::stderr());
    }
}

// Two-toned `difflore` wordmark; falls back to plain text under NO_COLOR.
pub fn wordmark() -> String {
    format!("{}{}", pewter("diff").bold(), emerald("lore").bold())
}

pub const DIVIDER: &str = "─────────────────────────────────────────────";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hint_helpers_use_locked_vocabulary() {
        assert_eq!(Hint::try_("x").label, "try");
    }

    #[test]
    fn symbols_match_contract() {
        assert_eq!(sym::OK, "OK");
        assert_eq!(sym::ERR, "X");
        assert_eq!(sym::WARN, "!");
        assert_eq!(sym::TIP, ">");
        assert_eq!(sym::BULLET, "-");
    }
}
