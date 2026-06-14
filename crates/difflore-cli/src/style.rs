//! CLI style helpers. The public terminal surface uses an ASCII-safe
//! 5-symbol set so Windows logs, CI, and screenshot captures stay aligned.
//! NO_COLOR env-var disables coloring; support is detected once at startup via
//! `detect_color_support()` and cached.

use std::io::{IsTerminal, Write, stderr, stdout};
use std::sync::{
    Arc, Mutex, MutexGuard, OnceLock,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::thread;
use std::time::Duration;

use colored::{ColoredString, Colorize};

// Color palette: two tuned ramps. The light ramp is darkened so every role
// clears WCAG 4.5:1 on white/cream backgrounds (the dark ramp falls to ~2.2:1
// on white). `background()` picks the ramp.

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

// Runnable commands get the info/blue token color so "things you can type"
// read as a distinct class. Non-bold: the hue carries the affordance.
pub fn cmd(s: &str) -> ColoredString {
    info(s)
}
// Headline emphasis for non-command text (result titles, rule names, verdicts):
// bold default ink, without the blue "runnable command" affordance.
pub fn title(s: &str) -> ColoredString {
    match color_support() {
        ColorSupport::None => s.normal(),
        _ => s.bold(),
    }
}
// Inline identifiers (file paths, ids, repo names, counts): bold neutral
// pewter so they stand out as distinct tokens without the command blue.
pub fn ident(s: &str) -> ColoredString {
    pewter(s).bold()
}
// Bold variants for status verbs.
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

// Symbols — ASCII-safe public status set.

pub mod sym {
    pub const OK: &str = "OK";
    pub const ERR: &str = "X";
    pub const WARN: &str = "!";
    pub const TIP: &str = ">";
    pub const BULLET: &str = "-";
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorSupport {
    TrueColor,
    Ansi256,
    None,
}

static COLOR_CACHE: OnceLock<ColorSupport> = OnceLock::new();

// Cached after the first call; call once at startup from `main`.
pub fn detect_color_support() -> ColorSupport {
    *COLOR_CACHE.get_or_init(detect_color_support_uncached)
}

fn color_support() -> ColorSupport {
    *COLOR_CACHE.get_or_init(detect_color_support_uncached)
}

const DEFAULT_WRAP_WIDTH: usize = 100;
const MIN_WRAP_WIDTH: usize = 40;
const MAX_WRAP_WIDTH: usize = 160;
const ANSI_RESET: &str = "\x1b[0m";

pub(crate) fn terminal_width() -> usize {
    use difflore_core::infra::env;
    env::var(env::COLUMNS)
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .filter(|width| *width >= MIN_WRAP_WIDTH)
        .unwrap_or(DEFAULT_WRAP_WIDTH)
        .clamp(MIN_WRAP_WIDTH, MAX_WRAP_WIDTH)
}

pub(crate) fn wrap_human_text(text: &str) -> String {
    wrap_human_text_for_width(text, terminal_width())
}

pub(crate) fn println_wrapped(line: &str) {
    println!("{}", wrap_human_text(line));
}

pub(crate) fn wrap_human_text_for_width(text: &str, width: usize) -> String {
    let mut out = String::new();
    for (index, line) in text.lines().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        out.push_str(&wrap_ansi_line_for_width(line, width));
    }
    if text.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn wrap_ansi_line_for_width(line: &str, width: usize) -> String {
    let width = width.clamp(MIN_WRAP_WIDTH, MAX_WRAP_WIDTH);
    if visible_width_ansi(line) <= width {
        return line.to_owned();
    }

    let first_indent = leading_ascii_spaces(line);
    let continuation_indent = continuation_indent(line);
    let content = line.trim_start_matches(' ');
    let words = ansi_words(content);
    if words.len() <= 1 {
        return line.to_owned();
    }

    let mut out = String::new();
    out.push_str(first_indent);
    let mut line_width = first_indent.len();
    let mut active_sgr: Option<String> = None;
    let mut wrote_word_on_line = false;

    for word in words {
        let word_width = visible_width_ansi(&word);
        let space_width = usize::from(wrote_word_on_line);
        if wrote_word_on_line && line_width + space_width + word_width > width {
            if let Some(active) = active_sgr.as_deref() {
                out.push_str(ANSI_RESET);
                out.push('\n');
                out.push_str(&continuation_indent);
                out.push_str(active);
            } else {
                out.push('\n');
                out.push_str(&continuation_indent);
            }
            line_width = continuation_indent.len();
            wrote_word_on_line = false;
        }

        if wrote_word_on_line {
            out.push(' ');
            line_width += 1;
        }
        out.push_str(&word);
        line_width += word_width;
        update_active_sgr(&word, &mut active_sgr);
        wrote_word_on_line = true;
    }

    if active_sgr.is_some() {
        out.push_str(ANSI_RESET);
    }
    out
}

fn leading_ascii_spaces(line: &str) -> &str {
    let end = line
        .char_indices()
        .find_map(|(idx, ch)| (ch != ' ').then_some(idx))
        .unwrap_or(line.len());
    &line[..end]
}

fn continuation_indent(line: &str) -> String {
    let plain = strip_ansi(line);
    let leading = plain.chars().take_while(|ch| *ch == ' ').count();
    let trimmed = plain.trim_start();
    let marker_width = if trimmed.starts_with("OK ") {
        3
    } else if trimmed.starts_with("next: ") {
        6
    } else if trimmed.starts_with("X ")
        || trimmed.starts_with("- ")
        || trimmed.starts_with("> ")
        || trimmed.starts_with("! ")
    {
        2
    } else {
        0
    };
    " ".repeat(leading + marker_width)
}

fn ansi_words(input: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            current.push(ch);
            if chars.peek() == Some(&'[') {
                current.push(chars.next().unwrap_or('['));
                for seq_ch in chars.by_ref() {
                    current.push(seq_ch);
                    if seq_ch.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }

        if ch.is_whitespace() {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            continue;
        }

        current.push(ch);
    }

    if !current.is_empty() {
        words.push(current);
    }
    words
}

fn strip_ansi(input: &str) -> String {
    let mut out = String::new();
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' && chars.peek() == Some(&'[') {
            let _ = chars.next();
            for seq_ch in chars.by_ref() {
                if seq_ch.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        out.push(ch);
    }
    out
}

fn visible_width_ansi(input: &str) -> usize {
    strip_ansi(input).chars().count()
}

fn update_active_sgr(input: &str, active: &mut Option<String>) {
    let bytes = input.as_bytes();
    let mut idx = 0;
    while idx + 1 < bytes.len() {
        if bytes[idx] == 0x1b && bytes[idx + 1] == b'[' {
            let start = idx;
            idx += 2;
            while idx < bytes.len() && !bytes[idx].is_ascii_alphabetic() {
                idx += 1;
            }
            if idx < bytes.len() {
                let end = idx + 1;
                if bytes[idx] == b'm' {
                    let seq = &input[start..end];
                    if sgr_is_reset(seq) {
                        *active = None;
                    } else {
                        *active = Some(seq.to_owned());
                    }
                }
                idx = end;
                continue;
            }
            break;
        }
        idx += 1;
    }
}

fn sgr_is_reset(seq: &str) -> bool {
    seq == ANSI_RESET || seq == "\x1b[m" || seq.trim_start_matches("\x1b[").starts_with("0;")
}

// Background detection. The dark ramp is unreadable on a light terminal, so we
// pick the light ramp when the terminal advertises a light background.
// Best-effort and conservative — default to Dark, since most dev terminals are
// dark and a wrong Light guess would wash colors out.

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
    use difflore_core::infra::env;
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
    use difflore_core::infra::env;
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

const SPIN_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

static SPINNER_IO_LOCK: Mutex<()> = Mutex::new(());

struct SpinnerState {
    label: Mutex<String>,
    frame: AtomicUsize,
    last_width: AtomicUsize,
    done: AtomicBool,
    enabled: bool,
    tick_interval: Duration,
}

#[derive(Clone)]
pub(crate) struct SpinnerHandle {
    state: Arc<SpinnerState>,
}

// Animates on stderr in the background; `finish_ok` / `finish_err` stops the
// worker, clears the active line, and prints the final glyph.
pub struct Spinner {
    state: Arc<SpinnerState>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Spinner {
    pub(crate) fn new(label: &str) -> Self {
        let state = Arc::new(SpinnerState {
            label: Mutex::new(label.to_owned()),
            frame: AtomicUsize::new(0),
            last_width: AtomicUsize::new(0),
            done: AtomicBool::new(false),
            enabled: spinner_enabled(color_support(), stderr().is_terminal()),
            tick_interval: Duration::from_millis(80),
        });

        let worker = if state.enabled {
            draw_current_state(&state);
            let worker_state = Arc::clone(&state);
            Some(thread::spawn(move || {
                while !worker_state.done.load(Ordering::Acquire) {
                    thread::sleep(worker_state.tick_interval);
                    if worker_state.done.load(Ordering::Acquire) {
                        break;
                    }
                    advance_and_draw(&worker_state);
                }
            }))
        } else {
            None
        };

        Self { state, worker }
    }

    pub(crate) fn handle(&self) -> SpinnerHandle {
        SpinnerHandle {
            state: Arc::clone(&self.state),
        }
    }

    pub(crate) fn tick(&self) {
        advance_and_draw(&self.state);
    }

    pub(crate) fn set_message(&self, msg: &str) {
        self.handle().set_message(msg);
    }

    /// Final line: `OK <msg>` (emerald), spinner cleared.
    pub(crate) fn finish_ok(mut self, msg: &str) {
        self.stop_and_clear();
        eprintln!("{} {}", emerald(sym::OK), msg);
    }

    /// Final line: `X <msg>` (danger), spinner cleared.
    pub(crate) fn finish_err(mut self, msg: &str) {
        self.stop_and_clear();
        eprintln!("{} {}", danger(sym::ERR), msg);
    }

    fn stop_and_clear(&mut self) {
        self.state.done.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        clear_state_line(&self.state);
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        if self.worker.is_some() {
            self.stop_and_clear();
        }
    }
}

impl SpinnerHandle {
    pub(crate) fn set_message(&self, msg: &str) {
        {
            let mut label = lock_or_recover(&self.state.label);
            msg.clone_into(&mut label);
        }
        draw_current_state(&self.state);
    }

    pub(crate) fn println(&self, msg: &str) {
        if !self.state.enabled {
            eprintln!("{msg}");
            return;
        }

        let _guard = lock_or_recover(&SPINNER_IO_LOCK);
        clear_state_line_locked(&self.state);
        eprintln!("{msg}");
        draw_current_state_locked(&self.state);
    }
}

fn advance_and_draw(state: &SpinnerState) {
    if !state.enabled || state.done.load(Ordering::Acquire) {
        return;
    }
    state.frame.fetch_add(1, Ordering::AcqRel);
    draw_current_state(state);
}

fn draw_current_state(state: &SpinnerState) {
    if !state.enabled || state.done.load(Ordering::Acquire) {
        return;
    }
    let _guard = lock_or_recover(&SPINNER_IO_LOCK);
    draw_current_state_locked(state);
}

fn draw_current_state_locked(state: &SpinnerState) {
    if !state.enabled || state.done.load(Ordering::Acquire) {
        return;
    }
    let frame = state.frame.load(Ordering::Acquire) % SPIN_FRAMES.len();
    let label = lock_or_recover(&state.label).clone();
    let width = spinner_line_width(&label);
    let previous_width = state.last_width.swap(width, Ordering::AcqRel);
    let pad = " ".repeat(previous_width.saturating_sub(width));
    eprint!("\r{} {}  {pad}", pewter(SPIN_FRAMES[frame]), label);
    let _ = stderr().flush();
}

fn clear_state_line(state: &SpinnerState) {
    if !state.enabled {
        return;
    }
    let _guard = lock_or_recover(&SPINNER_IO_LOCK);
    clear_state_line_locked(state);
}

fn clear_state_line_locked(state: &SpinnerState) {
    let width = state.last_width.swap(0, Ordering::AcqRel);
    if width == 0 {
        return;
    }
    let pad = " ".repeat(width + 1);
    eprint!("\r{pad}\r");
    let _ = stderr().flush();
}

const fn spinner_enabled(color_support: ColorSupport, stderr_is_tty: bool) -> bool {
    !matches!(color_support, ColorSupport::None) && stderr_is_tty
}

fn spinner_line_width(label: &str) -> usize {
    SPIN_FRAMES[0].chars().count() + 1 + label.chars().count() + 2
}

fn lock_or_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

// Two-toned `difflore` wordmark; falls back to plain text under NO_COLOR.
pub fn wordmark() -> String {
    format!("{}{}", pewter("diff").bold(), emerald("lore").bold())
}

pub const DIVIDER: &str = "---------------------------------------------";

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

    #[test]
    fn spinner_requires_stderr_tty() {
        assert!(spinner_enabled(ColorSupport::TrueColor, true));
        assert!(!spinner_enabled(ColorSupport::TrueColor, false));
        assert!(!spinner_enabled(ColorSupport::None, true));
    }

    #[test]
    fn wrap_human_text_keeps_bullet_continuation_indented() {
        let wrapped = wrap_human_text_for_width(
            "  - semantic recall: local keyword fallback (free: difflore cloud login; advanced/BYOK: difflore embeddings setup)",
            56,
        );

        assert!(
            wrapped.lines().count() > 1,
            "fixture should wrap: {wrapped}"
        );
        for line in wrapped.lines() {
            assert!(
                visible_width_ansi(line) <= 56,
                "line over width: {line:?} in {wrapped:?}"
            );
        }
        assert!(
            wrapped
                .lines()
                .skip(1)
                .all(|line| line.starts_with("    ") && !line.starts_with("     ")),
            "continuation lines should align under bullet body: {wrapped}"
        );
    }

    #[test]
    fn wrap_human_text_ignores_ansi_escape_width() {
        let wrapped = wrap_human_text_for_width(
            "  - review command: \x1b[34mdifflore import-reviews --upload\x1b[0m after setup",
            44,
        );

        assert!(
            wrapped.lines().count() > 1,
            "fixture should wrap: {wrapped:?}"
        );
        for line in wrapped.lines() {
            assert!(
                visible_width_ansi(line) <= 44,
                "line over width: {line:?} in {wrapped:?}"
            );
        }
        assert!(
            wrapped.contains(ANSI_RESET),
            "wrapped colored output should preserve/reset SGR state: {wrapped:?}"
        );
    }
}
