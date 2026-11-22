//! The rule of error texts: a test of its crate asserts each text that an
//! `#[error("...")]` attribute gives, since a refusal's variant and its text
//! together are one behaviour.

use std::collections::BTreeMap;
use std::ops::Range;

use super::finding::{Finding, Rule};
use super::modules::Module;
use super::rust::{self, Class};

/// The fewest characters of a part of a text that the rule looks for. A text
/// whose fixed parts are all shorter is nearly all values, and passes.
const PART_MIN: usize = 8;

/// The text of the string literal `token`, as Rust takes it. The escapes of
/// a plain, byte, or C string are read, and a `\` at the end of a line joins
/// the line to the first character of the next that is not white space. A
/// raw string stands as written, and a character literal has no text.
fn literal_text(token: &str) -> Option<String> {
    let rest = token
        .strip_prefix(['b', 'c'])
        .filter(|rest| rest.starts_with(['"', 'r']))
        .unwrap_or(token);
    if let Some(raw) = rest.strip_prefix('r') {
        let hashes = raw.bytes().take_while(|byte| *byte == b'#').count();
        let body = raw[hashes..].strip_prefix('"')?;
        let close = format!("\"{}", "#".repeat(hashes));
        let end = body.find(&close).unwrap_or(body.len());
        return Some(body[..end].to_owned());
    }
    let body = rest.strip_prefix('"')?;
    let body = body.strip_suffix('"').unwrap_or(body);
    let mut text = String::with_capacity(body.len());
    let mut chars = body.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            text.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') => text.push('\n'),
            Some('r') => text.push('\r'),
            Some('t') => text.push('\t'),
            Some('0') => text.push('\0'),
            Some('x') => {
                let digits: String = chars.by_ref().take(2).collect();
                let code = u8::from_str_radix(&digits, 16).ok()?;
                text.push(char::from(code));
            }
            Some('u') => {
                let digits: String = chars
                    .by_ref()
                    .skip_while(|ch| *ch == '{')
                    .take_while(|ch| *ch != '}')
                    .collect();
                let code = u32::from_str_radix(&digits, 16).ok()?;
                text.push(char::from_u32(code)?);
            }
            Some('\n' | '\r') => while chars.next_if(|ch| ch.is_whitespace()).is_some() {},
            Some(other) => text.push(other),
            None => text.push('\\'),
        }
    }
    Some(text)
}

/// The string literals of `source` whose first byte `within` takes, each as
/// [`literal_text`] reads it.
fn literals(source: &str, classes: &[Class], within: impl Fn(usize) -> bool) -> Vec<String> {
    let mut found = Vec::new();
    let mut at = 0;
    while at < classes.len() {
        if classes[at] != Class::Literal {
            at += 1;
            continue;
        }
        let start = at;
        while at < classes.len() && classes[at] == Class::Literal {
            at += 1;
        }
        if within(start) {
            found.extend(literal_text(&source[start..at]));
        }
    }
    found
}

/// The fixed parts of the format string `format`: what lies between its
/// `{...}` placeholders, with `{{` and `}}` read as braces.
fn fixed_parts(format: &str) -> Vec<String> {
    let mut parts = vec![String::new()];
    let mut chars = format.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '{' if chars.next_if_eq(&'{').is_some() => parts.last_mut().expect("a part").push('{'),
            '}' if chars.next_if_eq(&'}').is_some() => parts.last_mut().expect("a part").push('}'),
            '{' => {
                while chars.next().is_some_and(|ch| ch != '}') {}
                parts.push(String::new());
            }
            other => parts.last_mut().expect("a part").push(other),
        }
    }
    parts
}

/// The part of the format string `format` that the rule looks for: the
/// longest of its fixed parts, and the first of the longest. Each part is
/// trimmed of white space and of the marks at its ends, such as the colon
/// that joins it to a value. A text whose longest part is shorter than
/// [`PART_MIN`] has none.
fn looked_for(format: &str) -> Option<String> {
    let mut longest: Option<String> = None;
    for part in fixed_parts(format) {
        let part = part.trim_matches(|ch: char| ch.is_whitespace() || ch.is_ascii_punctuation());
        let longer = longest
            .as_ref()
            .is_none_or(|kept| part.chars().count() > kept.chars().count());
        if longer {
            longest = Some(part.to_owned());
        }
    }
    longest.filter(|part| part.chars().count() >= PART_MIN)
}

/// The texts of the `#[error("...")]` attributes of `source` that lie
/// outside `tests`, the spans of its code built for tests alone: the offset
/// of each attribute and its format string. `#[error(transparent)]` has none.
fn error_texts(source: &str, classes: &[Class], tests: &[Range<usize>]) -> Vec<(usize, String)> {
    let bytes = source.as_bytes();
    let mut found = Vec::new();
    for (at, opening) in source.match_indices("#[error(") {
        let in_code = classes.get(at) == Some(&Class::Code);
        if !in_code || tests.iter().any(|range| range.contains(&at)) {
            continue;
        }
        let mut start = at + opening.len();
        while bytes.get(start).is_some_and(u8::is_ascii_whitespace) {
            start += 1;
        }
        if classes.get(start) != Some(&Class::Literal) {
            continue;
        }
        let mut end = start;
        while classes.get(end) == Some(&Class::Literal) {
            end += 1;
        }
        found.extend(literal_text(&source[start..end]).map(|text| (at, text)));
    }
    found
}

/// The findings of `unasserted-error-text` in the crate in `dir`, whose Rust
/// files with their texts are `files` and whose modules are `modules`. An
/// error text outside the crate's code built for tests alone passes when
/// the part of it that [`looked_for`] gives stands inside a string literal
/// of the crate's tests. The tests are its modules built for tests alone,
/// with every item that only a build of tests holds, and its files under
/// `tests/`.
pub fn findings(dir: &str, modules: &[Module], files: &BTreeMap<&str, &str>) -> Vec<Finding> {
    let mut asserted = String::new();
    let mut checked = Vec::new();
    let mut add = |found: Vec<String>| {
        for literal in found {
            asserted.push_str(&literal);
            asserted.push('\0');
        }
    };
    let under_tests = format!("{dir}/tests/");
    for (path, source) in files {
        if path.starts_with(&under_tests) {
            add(literals(source, &rust::classify(source), |_| true));
        }
    }
    for module in modules {
        let Some(source) = files.get(module.path) else {
            continue;
        };
        let classes = &module.scan.classes;
        if module.test {
            add(literals(source, classes, |_| true));
            continue;
        }
        let tests = module.scan.test_spans();
        add(literals(source, classes, |at| {
            tests.iter().any(|range| range.contains(&at))
        }));
        let starts = rust::line_starts(source);
        for (at, text) in error_texts(source, classes, &tests) {
            if let Some(part) = looked_for(&text) {
                checked.push((module.path, rust::line_at(&starts, at), part));
            }
        }
    }
    checked
        .into_iter()
        .filter(|(_, _, part)| !asserted.contains(part.as_str()))
        .map(|(path, line, part)| {
            let message = format!(
                "{part:?}: no test of the crate asserts this error text; assert it beside its \
                 variant"
            );
            Finding::new(Rule::UnassertedErrorText, path, line, message)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::modules;
    use super::*;

    /// The findings in a crate at `crates/a` of `files`, each a path under
    /// `crates/a/` and a text: the line of each and the part it quotes.
    fn found(files: &[(&str, &str)]) -> Vec<(String, usize, String)> {
        let paths: Vec<(String, &str)> = files
            .iter()
            .map(|(path, text)| (format!("crates/a/{path}"), *text))
            .collect();
        let files: BTreeMap<&str, &str> = paths
            .iter()
            .map(|(path, text)| (path.as_str(), *text))
            .collect();
        let modules = modules::crate_modules("crates/a", &files);
        findings("crates/a", &modules, &files)
            .into_iter()
            .map(|finding| {
                let path = finding.path.trim_start_matches("crates/a/").to_owned();
                let part = finding
                    .message
                    .split(": no test")
                    .next()
                    .unwrap_or("")
                    .to_owned();
                (path, finding.line, part)
            })
            .collect()
    }

    /// A library with one error text of `format` and the test module `tests`.
    fn library(format: &str, tests: &str) -> String {
        format!(
            "#[derive(Debug, Error)]\npub enum E {{\n    #[error(\"{format}\")]\n    A,\n}}\n\
             {tests}"
        )
    }

    #[test]
    fn an_error_text_no_test_asserts_is_found() {
        let lib = library("the playlist is empty", "");
        assert_eq!(
            found(&[("src/lib.rs", &lib)]),
            [(
                "src/lib.rs".to_owned(),
                3,
                "\"the playlist is empty\"".to_owned()
            )]
        );
    }

    #[test]
    fn the_message_says_what_to_write() {
        let lib = library("the playlist is empty", "");
        let files: BTreeMap<&str, &str> = [("crates/a/src/lib.rs", lib.as_str())]
            .into_iter()
            .collect();
        let modules = modules::crate_modules("crates/a", &files);
        let found = findings("crates/a", &modules, &files);
        assert_eq!(
            found[0].message,
            "\"the playlist is empty\": no test of the crate asserts this error text; assert it \
             beside its variant"
        );
    }

    #[test]
    fn an_error_text_a_test_module_asserts_passes() {
        let tests = "#[cfg(test)]\nmod tests {\n    const A: &str = \"the playlist is empty\";\n}";
        assert_eq!(
            found(&[("src/lib.rs", &library("the playlist is empty", tests))]),
            []
        );
    }

    #[test]
    fn an_error_text_a_test_module_file_asserts_passes() {
        let lib = library("the playlist is empty", "#[cfg(test)]\nmod tests;");
        let files = [
            ("src/lib.rs", lib.as_str()),
            ("src/tests.rs", "const A: &str = \"the playlist is empty\";"),
        ];
        assert_eq!(found(&files), []);
    }

    #[test]
    fn an_error_text_a_file_under_tests_asserts_passes() {
        let lib = library("the playlist is empty", "");
        let files = [
            ("src/lib.rs", lib.as_str()),
            (
                "tests/play.rs",
                "const A: &str = \"the playlist is empty\";",
            ),
        ];
        assert_eq!(found(&files), []);
    }

    #[test]
    fn a_literal_outside_the_tests_asserts_nothing() {
        let lib = library(
            "the playlist is empty",
            "const A: &str = \"the playlist is empty\";",
        );
        assert_eq!(found(&[("src/lib.rs", &lib)]).len(), 1);
    }

    #[test]
    fn a_literal_of_another_crate_asserts_nothing() {
        let lib = library("the playlist is empty", "");
        let other = "const A: &str = \"the playlist is empty\";";
        let files: BTreeMap<&str, &str> = [
            ("crates/a/src/lib.rs", lib.as_str()),
            ("crates/b/tests/play.rs", other),
        ]
        .into_iter()
        .collect();
        let modules = modules::crate_modules("crates/a", &files);
        assert_eq!(findings("crates/a", &modules, &files).len(), 1);
    }

    #[test]
    fn the_longest_fixed_part_is_looked_for() {
        let lib = library("at {0}: the playlist is empty after {1}", "");
        assert_eq!(
            found(&[("src/lib.rs", &lib)])[0].2,
            "\"the playlist is empty after\""
        );
    }

    #[test]
    fn the_marks_at_the_ends_of_a_part_are_not_looked_for() {
        let lib = library("{0}: the (playlist) is empty.", "");
        assert_eq!(
            found(&[("src/lib.rs", &lib)])[0].2,
            "\"the (playlist) is empty\""
        );
    }

    #[test]
    fn the_first_of_two_longest_parts_is_looked_for() {
        let lib = library("abcdefgh {0} ijklmnop", "");
        assert_eq!(found(&[("src/lib.rs", &lib)])[0].2, "\"abcdefgh\"");
    }

    #[test]
    fn a_text_whose_longest_part_is_seven_characters_is_skipped() {
        assert_eq!(
            found(&[("src/lib.rs", &library("{0} abcdefg {1}", ""))]),
            []
        );
    }

    #[test]
    fn a_text_whose_longest_part_is_eight_characters_is_checked() {
        assert_eq!(
            found(&[("src/lib.rs", &library("{0} abcdefgh {1}", ""))]).len(),
            1
        );
    }

    #[test]
    fn doubled_braces_are_fixed_text() {
        let lib = library("a {{0}} brace {1}", "");
        assert_eq!(found(&[("src/lib.rs", &lib)])[0].2, "\"a {0} brace\"");
    }

    #[test]
    fn a_transparent_error_is_skipped() {
        let lib = "#[derive(Debug, Error)]\npub enum E {\n    #[error(transparent)]\n    A(B),\n}";
        assert_eq!(found(&[("src/lib.rs", lib)]), []);
    }

    #[test]
    fn an_error_text_of_a_test_module_is_skipped() {
        let tests = format!(
            "#[cfg(test)]\nmod tests {{\n{}\n}}",
            library("a {{x}} literal brace {0}", "")
        );
        assert_eq!(found(&[("src/lib.rs", &tests)]), []);
    }

    #[test]
    fn an_error_text_written_over_lines_is_read() {
        let lib = "pub enum E {\n    #[error(\n        \"the playlist \\\n         \
                   is empty\"\n    )]\nA,\n}\n\
                   #[cfg(test)]\nmod tests {\n    const A: &str = \"the playlist is empty\";\n}";
        assert_eq!(found(&[("src/lib.rs", lib)]), []);
    }

    #[test]
    fn a_literal_of_a_test_is_read_as_rust_reads_it() {
        let tests = "#[cfg(test)]\nmod tests {\n    \
                     const A: &str = \"the \\\"playlist\\\" \\\n        is empty\";\n}";
        let lib = library("the \\\"playlist\\\" is empty", tests);
        assert_eq!(found(&[("src/lib.rs", &lib)]), []);
    }

    #[test]
    fn a_raw_literal_of_a_test_asserts_its_text() {
        let tests =
            "#[cfg(test)]\nmod tests {\n    const A: &str = r#\"the \"playlist\" is empty\"#;\n}";
        let lib = library("the \\\"playlist\\\" is empty", tests);
        assert_eq!(found(&[("src/lib.rs", &lib)]), []);
    }

    #[test]
    fn an_error_text_in_a_binary_is_checked() {
        let main = library("the playlist is empty", "");
        assert_eq!(found(&[("src/main.rs", &main)]).len(), 1);
    }

    #[test]
    fn an_error_text_in_a_string_of_code_is_no_attribute() {
        let lib = "const A: &str = r#\"#[error(\"the playlist is empty\")]\"#;";
        assert_eq!(found(&[("src/lib.rs", lib)]), []);
    }

    #[test]
    fn the_escapes_of_a_literal_are_read() {
        let text = literal_text("\"a\\n\\t\\\\\\x41\\u{e9}\\0\"").unwrap();
        assert_eq!(text, "a\n\t\\A\u{e9}\0");
    }

    #[test]
    fn a_byte_string_is_read() {
        assert_eq!(literal_text("b\"ab\"").unwrap(), "ab");
        assert_eq!(literal_text("br#\"a\"b\"#").unwrap(), "a\"b");
    }

    #[test]
    fn a_character_literal_has_no_text() {
        assert_eq!(literal_text("'a'"), None);
        assert_eq!(literal_text("b'a'"), None);
    }

    #[test]
    fn a_literal_that_the_file_ends_within_is_read() {
        let source = "const A: &str = \"the playlist";
        let classes = rust::classify(source);
        assert_eq!(literals(source, &classes, |_| true), ["the playlist"]);
    }

    #[test]
    fn a_placeholder_is_no_part_of_a_text() {
        assert_eq!(fixed_parts("a {name} b"), ["a ", " b"]);
    }

    #[test]
    fn a_text_on_the_line_after_its_attribute_opens_is_read() {
        let lib = "#[derive(Debug, Error)]\npub enum E {\n    #[error(\n        \
                   \"the playlist is empty\"\n    )]\n    A,\n}\n";
        assert_eq!(
            found(&[("src/lib.rs", lib)]),
            [(
                "src/lib.rs".to_owned(),
                3,
                "\"the playlist is empty\"".to_owned()
            )]
        );
    }
}
