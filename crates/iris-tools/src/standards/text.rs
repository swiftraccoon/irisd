//! Scanning lines of text without a pattern library.

/// Whether `byte` can be part of a word.
fn is_word(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte >= 0x80
}

/// The byte offsets at which `term` stands as a whole word in `line`. Both are
/// compared as given: lowercase them for a search that ignores case.
pub fn word_offsets(line: &str, term: &str) -> Vec<usize> {
    let bytes = line.as_bytes();
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(at) = line.get(from..).and_then(|rest| rest.find(term)) {
        let start = from + at;
        let end = start + term.len();
        let open = start == 0 || !is_word(bytes[start - 1]);
        let closed = end == bytes.len() || !is_word(bytes[end]);
        if open && closed {
            found.push(start);
        }
        from = end;
    }
    found
}

/// Whether `term` stands as a whole word in `line`.
pub fn has_word(line: &str, term: &str) -> bool {
    !word_offsets(line, term).is_empty()
}

/// `line` with the contents of its backtick code spans replaced by spaces, so
/// offsets hold and what is code is not read as prose.
pub fn without_code_spans(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut inside = false;
    for ch in line.chars() {
        if ch == '`' {
            inside = !inside;
            out.push(ch);
        } else if inside {
            out.extend(std::iter::repeat_n(' ', ch.len_utf8()));
        } else {
            out.push(ch);
        }
    }
    out
}

/// The contents of the backtick code spans of `line`.
pub fn code_spans(line: &str) -> Vec<&str> {
    line.split('`').skip(1).step_by(2).collect()
}

/// The text between the bracket that opens at `open` and the one that closes
/// it, where `open` is the offset of a `(`, `[`, `{`, or `<`. A `>` that ends
/// an arrow closes nothing.
pub fn balanced(text: &str, open: usize) -> Option<&str> {
    let bytes = text.as_bytes();
    let (up, down) = match bytes.get(open)? {
        b'(' => (b'(', b')'),
        b'[' => (b'[', b']'),
        b'{' => (b'{', b'}'),
        b'<' => (b'<', b'>'),
        _ => return None,
    };
    let mut depth = 0_usize;
    for (at, &byte) in bytes.iter().enumerate().skip(open) {
        if byte == up {
            depth += 1;
        } else if byte == down && !(down == b'>' && at > 0 && bytes[at - 1] == b'-') {
            depth -= 1;
            if depth == 0 {
                return text.get(open + 1..at);
            }
        }
    }
    None
}

/// `list` split at the commas that sit inside no bracket.
pub fn top_level_items(list: &str) -> Vec<&str> {
    let bytes = list.as_bytes();
    let mut items = Vec::new();
    let mut depth = 0_i32;
    let mut start = 0;
    for (at, &byte) in bytes.iter().enumerate() {
        match byte {
            b'(' | b'[' | b'{' | b'<' => depth += 1,
            b'>' if at > 0 && bytes[at - 1] == b'-' => {}
            b')' | b']' | b'}' | b'>' => depth -= 1,
            b',' if depth == 0 => {
                items.push(&list[start..at]);
                start = at + 1;
            }
            _ => {}
        }
    }
    items.push(&list[start..]);
    items
        .into_iter()
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_word_is_found_only_whole() {
        assert_eq!(word_offsets("new news, renew; new", "new"), vec![0, 17]);
        assert!(has_word("the latest.", "latest"));
        assert!(!has_word("threshold", "old"));
        assert!(!has_word("old_name", "old"));
    }

    #[test]
    fn code_spans_are_blanked_in_place() {
        assert_eq!(without_code_spans("a `new` b"), "a `   ` b");
        assert_eq!(code_spans("a `x/y` and `z`"), vec!["x/y", "z"]);
        assert_eq!(without_code_spans("`§`.").len(), "`§`.".len());
    }

    #[test]
    fn brackets_balance_past_arrows() {
        assert_eq!(balanced("f(a, (b))", 1), Some("a, (b)"));
        assert_eq!(balanced("<F: Fn() -> T>(x)", 0), Some("F: Fn() -> T"));
        assert_eq!(balanced("(open", 0), None);
        assert_eq!(
            top_level_items("a: u8, b: (u8, u8), c: Vec<(u8, u8)>"),
            vec!["a: u8", "b: (u8, u8)", "c: Vec<(u8, u8)>"]
        );
    }
}
