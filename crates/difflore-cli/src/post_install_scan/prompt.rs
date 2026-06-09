//! Synchronous `[Y/n]` prompt for the post-install offer.
//!
//! Mirrors the helper in `commands/welcome.rs::prompt_yes` but stays
//! sync so it slots into the (sync) `mcp_install::install_all` flow
//! without forcing a tokio runtime up the call stack. Visual style
//! tracks the existing pewter/emerald palette from `style.rs`.

use std::io::{self, BufRead, Write};

use crate::style::{self, sym};

/// Default-yes confirmation prompt. Empty input -> yes (matches the
/// `[Y/n]` capitalization convention); anything starting with `n` -> no;
/// EOF / read error -> no (we never opt a non-interactive caller in by
/// accident).
///
/// Pulled out as a function so the runner can swap it for a stub in
/// unit tests via the `ask_yes_default_yes_with` indirection below.
#[must_use]
pub fn ask_yes_default_yes(question: &str) -> bool {
    print_question(question);
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let mut buf = String::new();
    let read = reader.read_line(&mut buf);
    ask_yes_default_yes_with(read.ok().map(|_| buf))
}

/// Pure decision: given the line the prompt collected (or `None` for
/// EOF / read error), should we treat it as "yes"? Public to the
/// module so tests can exercise the cleanup + default-yes logic
/// without driving a real stdin.
#[must_use]
pub fn ask_yes_default_yes_with(line: Option<String>) -> bool {
    let Some(line) = line else {
        // EOF / read error: caller is not a real human, fall back to no.
        return false;
    };
    let answer = clean_prompt_answer(&line);
    if answer.is_empty() {
        return true; // default-yes — bare Enter accepts the offer
    }
    !(answer == "n" || answer == "no")
}

/// Render the `[Y/n]` line. Kept separate so the prompt's visual style
/// can be tweaked without touching the decision logic.
fn print_question(question: &str) {
    print!(
        "  {} {} {} ",
        style::emerald(sym::TIP),
        question,
        style::pewter("[Y/n]"),
    );
    let _ = io::stdout().flush();
}

/// Same cleanup welcome.rs uses: strip whitespace, NULs, BOM, then
/// lowercase. Means a Windows shell that injects `\r\n` + a stray BOM
/// still produces a clean answer string.
fn clean_prompt_answer(line: &str) -> String {
    line.trim_matches(|c: char| c.is_whitespace() || c == '\0' || c == '\u{feff}')
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_is_default_yes() {
        // Bare Enter (just a newline) should keep the [Y/n] default.
        assert!(ask_yes_default_yes_with(Some("\n".to_owned())));
        assert!(ask_yes_default_yes_with(Some(String::new())));
    }

    #[test]
    fn lowercase_n_declines_offer() {
        assert!(!ask_yes_default_yes_with(Some("n\n".to_owned())));
        assert!(!ask_yes_default_yes_with(Some("N\n".to_owned())));
        assert!(!ask_yes_default_yes_with(Some("no\r\n".to_owned())));
        assert!(!ask_yes_default_yes_with(Some("NO".to_owned())));
    }

    #[test]
    fn any_yes_variant_accepts_offer() {
        for ans in ["y", "Y", "yes", "YES", " yes ", "yeah"] {
            assert!(
                ask_yes_default_yes_with(Some(ans.to_owned())),
                "expected yes for {ans:?}",
            );
        }
    }

    #[test]
    fn eof_or_read_error_declines_offer() {
        // `None` models EOF — non-interactive shell with stdin closed.
        // We must not opt them in: default-yes only applies when the
        // user actually saw the prompt and pressed Enter.
        assert!(!ask_yes_default_yes_with(None));
    }

    #[test]
    fn windows_control_bytes_are_stripped_before_decision() {
        assert!(!ask_yes_default_yes_with(Some("\u{feff}n\0\r\n".to_owned())));
        assert!(ask_yes_default_yes_with(Some("\u{feff}\0\r\n".to_owned())));
    }
}
