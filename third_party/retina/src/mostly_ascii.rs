// Copyright (C) The Retina Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Formatter for byte data that is expected to be mostly ASCII.

use std::fmt;

/// Holds something that's supposed to be mostly ASCII.
///
/// Some SDP values are allowed to be UTF-8-encoded ISO-10646 characters, but mostly it's
/// supposed to be valid ASCII. This has a `Debug` impl that is fairly readable for ASCII
/// characters and can be copy'n'pasted into Python's `codecs.decode(..., 'string_escape'`)`
/// or the like.
pub(crate) struct MostlyAscii<'a> {
    pub bytes: &'a [u8],
    pub escape_newline: bool,
}

impl<'a> MostlyAscii<'a> {
    #[cfg(test)]
    pub fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            escape_newline: false,
        }
    }
}

/// Longest escaped text an [`Excerpt`] writes before it stops.
pub(crate) const MAX_EXCERPT_BYTES: usize = 512;

/// Peer-supplied bytes for a log line or an error message: one line of at
/// most [`MAX_EXCERPT_BYTES`] escaped bytes, so the peer can neither split
/// the line, carry terminal controls into it, nor make it grow with the input.
///
/// Printable ASCII is written as is; `\n`, `\r`, and `\t` are written as
/// those two-character escapes and every other byte as `\xNN`. An excerpt
/// that stops early ends with `...` and the input's full length.
#[derive(Clone, Copy)]
pub(crate) struct Excerpt<'a>(pub &'a [u8]);

impl<'a> Excerpt<'a> {
    pub fn of(text: &'a str) -> Self {
        Self(text.as_bytes())
    }
}

impl fmt::Display for Excerpt<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut written = 0;
        for &b in self.0 {
            let mut hex = [b'\\', b'x', 0, 0];
            let escaped: &[u8] = match b {
                b'\n' => b"\\n",
                b'\r' => b"\\r",
                b'\t' => b"\\t",
                0x20..=0x7E => std::slice::from_ref(&b),
                _ => {
                    const DIGITS: &[u8; 16] = b"0123456789ABCDEF";
                    hex[2] = DIGITS[usize::from(b >> 4)];
                    hex[3] = DIGITS[usize::from(b & 0xF)];
                    &hex
                }
            };
            if written + escaped.len() > MAX_EXCERPT_BYTES {
                return write!(f, "...({} bytes)", self.0.len());
            }
            f.write_str(std::str::from_utf8(escaped).expect("escapes are ASCII"))?;
            written += escaped.len();
        }
        Ok(())
    }
}

impl fmt::Debug for MostlyAscii<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "\"")?;
        for &b in self.bytes {
            match b {
                b'"' => write!(f, "\"")?,
                b'\r' => write!(f, "\\r")?,
                b'\n' if self.escape_newline => write!(f, "\\n")?,
                // Note: when escape_newline is false, newlines are written in unescaped form.
                b' ' | b'\n' | 0x20..=0x7E => write!(f, "{}", b as char)?,
                _ => write!(f, "\\x{b:02X}")?,
            }
        }
        write!(f, "\"")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_ascii() {
        let m = MostlyAscii::new(b"hello world");
        assert_eq!(format!("{m:?}"), "\"hello world\"");
    }

    #[test]
    fn newline_unescaped() {
        let m = MostlyAscii::new(b"line1\nline2");
        assert_eq!(format!("{m:?}"), "\"line1\nline2\"");
    }

    #[test]
    fn newline_escaped() {
        let m = MostlyAscii {
            bytes: b"line1\nline2",
            escape_newline: true,
        };
        assert_eq!(format!("{m:?}"), "\"line1\\nline2\"");
    }

    #[test]
    fn cr_always_escaped() {
        let m = MostlyAscii::new(b"line1\r\nline2");
        assert_eq!(format!("{m:?}"), "\"line1\\r\nline2\"");
    }

    #[test]
    fn non_ascii() {
        let m = MostlyAscii::new(b"hello\x80\xFF");
        assert_eq!(format!("{m:?}"), "\"hello\\x80\\xFF\"");
    }

    #[test]
    fn excerpt_escapes_every_control_byte() {
        let e = Excerpt(b"a\nb\r\tc\x1b[31m\x00\x7f\xc3\xa9\"\\");
        assert_eq!(
            e.to_string(),
            "a\\nb\\r\\tc\\x1B[31m\\x00\\x7F\\xC3\\xA9\"\\"
        );
    }

    #[test]
    fn excerpt_stops_at_the_limit_with_the_full_length() {
        let input = vec![b'\n'; 4 * MAX_EXCERPT_BYTES];
        let text = Excerpt(&input).to_string();
        assert!(text.starts_with("\\n\\n"));
        assert!(text.ends_with(&format!("...({} bytes)", input.len())));
        assert!(text.len() <= MAX_EXCERPT_BYTES + "...(2048 bytes)".len());
        assert!(!text.bytes().any(|b| b.is_ascii_control()));

        let exact = vec![b'x'; MAX_EXCERPT_BYTES];
        assert_eq!(Excerpt(&exact).to_string(), "x".repeat(MAX_EXCERPT_BYTES));
    }
}
