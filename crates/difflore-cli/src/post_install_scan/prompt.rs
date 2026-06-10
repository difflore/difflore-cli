//! Synchronous `[Y/n]` prompt for the post-install offer.
//!
//! Stays sync (unlike `commands/welcome.rs::prompt_yes`) so it slots into
//! the sync `installer::install_all` flow without forcing a tokio runtime
//! up the call stack.

use std::io::{self, BufRead, Write};

use crate::style::{self, sym};

/// Default-yes confirmation prompt. Empty input -> yes; anything starting
/// with `n` -> no; EOF / read error -> no (never opt a non-interactive
/// caller in by accident).
#[must_use]
pub fn ask_yes_default_yes(question: &str) -> bool {
    print_question(question);
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let mut buf = String::new();
    let read = reader.read_line(&mut buf);
    ask_yes_default_yes_with(read.ok().map(|_| buf))
}

/// Pure decision for a collected line (`None` = EOF / read error): treat it
/// as "yes"? Split out so tests can exercise the logic without real stdin.
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

/// Render the `[Y/n]` line.
fn print_question(question: &str) {
    print!(
        "  {} {} {} ",
        style::emerald(sym::TIP),
        question,
        style::pewter("[Y/n]"),
    );
    let _ = io::stdout().flush();
}

/// Strip whitespace, NULs, and BOM, then lowercase, so a Windows shell
/// injecting `\r\n` + a stray BOM still yields a clean answer string.
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
