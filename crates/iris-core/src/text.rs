//! Text that came from a camera or another peer, made safe for one log line.
//!
//! A log line that embeds peer-controlled text can be split, forged (a journal
//! priority prefix at the start of a line), or made to carry terminal controls.
//! [`LogSafe`] prints any [`Display`] value as one bounded, escaped line.
//!
//! ```
//! use iris_core::text::LogSafe;
//!
//! let hostile = "fault\n<0>forged line\u{7}";
//! assert_eq!(LogSafe(&hostile).to_string(), "fault\\n<0>forged line\\u{7}");
//! ```

use std::fmt::{self, Display, Write as _};

/// Longest text, marker included, that [`LogSafe`] puts in a log line.
pub const MAX_LOGGED_CHARS: usize = 512;
/// Ends a text cut at [`MAX_LOGGED_CHARS`].
pub const TRUNCATED: &str = "...";

/// Formats a value as one bounded, escaped line: printable ASCII passes,
/// `\n`, `\r`, and `\t` become those escapes, every other character becomes
/// `\u{…}`, and a longer text is cut to [`MAX_LOGGED_CHARS`] characters
/// ending in [`TRUNCATED`]. A peer can then neither split a journal entry,
/// start a line with a forged priority prefix, carry terminal controls into
/// the log, nor make the line grow with what it sends.
pub struct LogSafe<'a, T: ?Sized>(pub &'a T);

impl<T: Display + ?Sized> Display for LogSafe<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut line = EscapedLine(String::with_capacity(MAX_LOGGED_CHARS + 16));
        // The only error is the line filling up, which stops the formatting.
        if write!(line, "{}", self.0).is_err() {
            line.0.truncate(MAX_LOGGED_CHARS - TRUNCATED.len());
            line.0.push_str(TRUNCATED);
        }
        f.write_str(&line.0)
    }
}

/// The escaped text of [`LogSafe`], ASCII only, refusing more once it is
/// longer than [`MAX_LOGGED_CHARS`].
struct EscapedLine(String);

impl fmt::Write for EscapedLine {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for c in text.chars() {
            match c {
                ' '..='~' => self.0.push(c),
                '\n' => self.0.push_str("\\n"),
                '\r' => self.0.push_str("\\r"),
                '\t' => self.0.push_str("\\t"),
                _ => write!(self.0, "\\u{{{:x}}}", u32::from(c))?,
            }
            if self.0.len() > MAX_LOGGED_CHARS {
                return Err(fmt::Error);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controls_and_non_ascii_are_escaped_on_one_line() {
        let text = "a\nb\rc\td\u{1b}[31me\u{e9}";
        assert_eq!(
            LogSafe(text).to_string(),
            "a\\nb\\rc\\td\\u{1b}[31me\\u{e9}"
        );
    }

    #[test]
    fn text_at_the_limit_passes_and_longer_text_is_cut() {
        let exact = "x".repeat(MAX_LOGGED_CHARS);
        assert_eq!(LogSafe(&exact).to_string(), exact);
        let long = "y".repeat(MAX_LOGGED_CHARS + 1);
        let line = LogSafe(&long).to_string();
        assert_eq!(line.len(), MAX_LOGGED_CHARS);
        assert!(line.ends_with(TRUNCATED), "{line}");
    }

    #[test]
    fn a_forged_priority_prefix_cannot_start_a_line() {
        let line = LogSafe("fault\n<0>EMERGENCY").to_string();
        assert!(!line.contains('\n'));
        assert!(line.contains("\\n<0>"), "{line}");
    }
}
