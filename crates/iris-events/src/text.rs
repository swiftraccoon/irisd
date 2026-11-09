//! Bounds on free text a camera sends: discovery scopes, profile tokens, and
//! profile names.

/// Longest text kept from a scope, a profile token, or a profile name.
pub const MAX_TEXT_CHARS: usize = 128;

/// `raw` without control characters, trimmed, and cut to `max_chars`; `None`
/// when nothing is left.
pub(crate) fn clean_text(raw: &str, max_chars: usize) -> Option<String> {
    let text: String = raw.chars().filter(|c| !c.is_control()).collect();
    let text: String = text.trim().chars().take(max_chars).collect();
    (!text.is_empty()).then_some(text)
}

/// `raw` cleaned and cut to [`MAX_TEXT_CHARS`], or empty when nothing is left.
pub(crate) fn bounded_text(raw: &str) -> String {
    clean_text(raw, MAX_TEXT_CHARS).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_characters_are_removed_and_the_rest_is_cut() {
        assert_eq!(clean_text(" a\u{7}b\n ", 8).as_deref(), Some("ab"));
        assert_eq!(clean_text(" \u{7} ", 8), None);
        assert_eq!(
            bounded_text(&"x".repeat(500)).chars().count(),
            MAX_TEXT_CHARS
        );
        assert_eq!(bounded_text("\u{1b}"), "");
    }
}
