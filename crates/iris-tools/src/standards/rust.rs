//! The rules for Rust source that neither rustfmt nor clippy checks: import
//! form, hex case, lint exceptions, `TODO` owners, getter names, `bool`
//! parameters, bare integer tuples, and timeless rustdoc.

use super::finding::{Finding, Rule};
use super::{prose, text};

/// What a byte of a source file belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    /// Code.
    Code,
    /// A comment that is not documentation.
    Comment,
    /// A documentation comment.
    Doc,
    /// The inside of a string or character literal.
    Literal,
}

/// The end of the block comment that opens at `at`; block comments nest.
fn block_comment_end(bytes: &[u8], at: usize) -> usize {
    let mut depth = 0_usize;
    let mut at = at;
    while at < bytes.len() {
        if bytes[at..].starts_with(b"/*") {
            depth += 1;
            at += 2;
        } else if bytes[at..].starts_with(b"*/") {
            depth -= 1;
            at += 2;
            if depth == 0 {
                break;
            }
        } else {
            at += 1;
        }
    }
    at.min(bytes.len())
}

/// The end of the string literal whose prefix starts at `at`, if one starts
/// there: a plain, byte, or C string, raw or not.
fn string_end(bytes: &[u8], at: usize) -> Option<usize> {
    let mut quote = at;
    if matches!(bytes.get(quote), Some(b'b' | b'c')) {
        quote += 1;
    }
    let raw = bytes.get(quote) == Some(&b'r');
    if raw {
        quote += 1;
    }
    let hashes = bytes[quote.min(bytes.len())..]
        .iter()
        .take_while(|byte| **byte == b'#')
        .count();
    if !raw && hashes > 0 {
        return None;
    }
    quote += hashes;
    if bytes.get(quote) != Some(&b'"') {
        return None;
    }
    let mut end = quote + 1;
    while end < bytes.len() {
        match bytes[end] {
            b'\\' if !raw => end += 2,
            b'"' if bytes[end + 1..]
                .iter()
                .take(hashes)
                .filter(|b| **b == b'#')
                .count()
                == hashes =>
            {
                return Some(end + 1 + hashes);
            }
            _ => end += 1,
        }
    }
    Some(bytes.len())
}

/// The end of the character literal that opens at `at`, or nothing when the
/// quote there starts a lifetime.
fn char_end(source: &str, at: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    if bytes.get(at + 1) == Some(&b'\\') {
        let close = bytes[at + 3..].iter().position(|byte| *byte == b'\'')?;
        return Some(at + 3 + close + 1);
    }
    let inner = source.get(at + 1..)?.chars().next()?;
    let close = at + 1 + inner.len_utf8();
    (inner != '\'' && bytes.get(close) == Some(&b'\'')).then_some(close + 1)
}

/// What each byte of `source` belongs to.
fn classify(source: &str) -> Vec<Class> {
    let bytes = source.as_bytes();
    let mut classes = vec![Class::Code; bytes.len()];
    let mut at = 0;
    while at < bytes.len() {
        let rest = &bytes[at..];
        let word_before =
            at > 0 && (bytes[at - 1].is_ascii_alphanumeric() || bytes[at - 1] == b'_');
        let (end, class) = if rest.starts_with(b"//") {
            let end = rest
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(bytes.len(), |n| at + n);
            let doc = (rest.starts_with(b"///") && !rest.starts_with(b"////"))
                || rest.starts_with(b"//!");
            (end, if doc { Class::Doc } else { Class::Comment })
        } else if rest.starts_with(b"/*") {
            let doc = (rest.starts_with(b"/**") && !rest.starts_with(b"/***"))
                || rest.starts_with(b"/*!");
            let class = if doc && !rest.starts_with(b"/**/") {
                Class::Doc
            } else {
                Class::Comment
            };
            (block_comment_end(bytes, at), class)
        } else if let Some(end) = string_end(bytes, at).filter(|_| !word_before) {
            (end, Class::Literal)
        } else if rest[0] == b'\'' {
            match char_end(source, at) {
                Some(end) => (end, Class::Literal),
                None => (at + 1, Class::Code),
            }
        } else {
            (at + 1, Class::Code)
        };
        let end = end.min(bytes.len());
        classes[at..end].fill(class);
        at = end;
    }
    classes
}

/// `source` with every byte that is not of `class` blanked, line breaks kept,
/// so that line numbers and columns hold.
fn only(source: &str, classes: &[Class], class: Class) -> String {
    let kept: Vec<u8> = source
        .bytes()
        .zip(classes)
        .map(|(byte, of)| {
            if *of == class || byte == b'\n' {
                byte
            } else {
                b' '
            }
        })
        .collect();
    String::from_utf8_lossy(&kept).into_owned()
}

/// The line, from 1, of the byte at `offset`.
fn line_of(text: &str, offset: usize) -> usize {
    text[..offset].matches('\n').count() + 1
}

/// `use` declarations with a path inside their braces (`STANDARDS.md` §2: one
/// `use` per module).
fn use_braces(code: &str) -> Vec<(usize, String)> {
    let mut found = Vec::new();
    let mut offset = 0;
    for line in code.split_inclusive('\n') {
        let start = offset;
        offset += line.len();
        let trimmed = line.trim_start();
        let declared = ["use ", "pub use ", "pub(crate) use ", "pub(super) use "]
            .iter()
            .any(|prefix| trimmed.starts_with(prefix));
        if !declared {
            continue;
        }
        let statement = code[start..].split(';').next().unwrap_or("");
        let nested = statement
            .split_once('{')
            .is_some_and(|(_, inside)| inside.contains("::"));
        if nested {
            let message = "a path inside the braces: one `use` per module";
            found.push((line_of(code, start), message.to_owned()));
        }
    }
    found
}

/// Hex literals with an uppercase digit.
fn hex_case(code: &str) -> Vec<(usize, String)> {
    let bytes = code.as_bytes();
    let mut found = Vec::new();
    for (at, _) in code.match_indices("0x") {
        let apart = at == 0 || !(bytes[at - 1].is_ascii_alphanumeric() || bytes[at - 1] == b'_');
        let digits: &str = code[at + 2..]
            .split(|ch: char| !(ch.is_ascii_hexdigit() || ch == '_'))
            .next()
            .unwrap_or("");
        if apart && digits.bytes().any(|byte| byte.is_ascii_uppercase()) {
            found.push((
                line_of(code, at),
                format!("0x{digits}: lowercase hex digits"),
            ));
        }
    }
    found
}

/// `#[allow]` attributes, and `#[expect]` attributes without a reason.
fn lint_exceptions(code: &str) -> Vec<(usize, String)> {
    let mut found = Vec::new();
    for (at, _) in code.match_indices("allow(") {
        let before = code[..at].trim_end();
        if before.ends_with("#[") || before.ends_with("#![") || before.ends_with("cfg_attr(") {
            let message = "`allow`: use `#[expect(lint, reason = \"…\")]`";
            found.push((line_of(code, at), message.to_owned()));
        }
    }
    for (at, _) in code.match_indices("expect(") {
        let before = code[..at].trim_end();
        let attribute = before.ends_with("#[") || before.ends_with("#![");
        let reasoned = text::balanced(code, at + "expect".len())
            .is_some_and(|inside| text::has_word(inside, "reason"));
        if attribute && !reasoned {
            found.push((line_of(code, at), "`expect` without a reason".to_owned()));
        }
    }
    found
}

/// `TODO`, `FIXME`, and `XXX` notes without an owner or an issue.
fn todos(comments: &str) -> Vec<(usize, String)> {
    let comments = &text::without_code_spans(comments);
    let mut found = Vec::new();
    for marker in ["TODO", "FIXME", "XXX"] {
        for at in text::word_offsets(comments, marker) {
            let rest = &comments[at + marker.len()..];
            let owned =
                rest.starts_with('(') || rest.trim_start_matches([':', ' ']).starts_with('#');
            if !owned {
                let message = format!("{marker} without an owner or an issue");
                found.push((line_of(comments, at), message));
            }
        }
    }
    found
}

/// The primitive number types: a tuple of them carries no unit.
fn is_bare_number(ty: &str) -> bool {
    const NUMBERS: [&str; 14] = [
        "u8", "u16", "u32", "u64", "u128", "usize", "i8", "i16", "i32", "i64", "i128", "isize",
        "f32", "f64",
    ];
    NUMBERS.contains(&ty)
}

/// Whether a type holds a tuple of two or more bare numbers.
fn has_bare_tuple(ty: &str) -> bool {
    ty.match_indices('(').any(|(at, _)| {
        text::balanced(ty, at).is_some_and(|inside| {
            let items = text::top_level_items(inside);
            items.len() >= 2 && items.iter().all(|item| is_bare_number(item))
        })
    })
}

/// The findings in the signatures of the functions of `code`: getters named
/// `get_`, and in public functions `bool` parameters and tuples of bare
/// numbers.
fn signatures(code: &str) -> Vec<(usize, Rule, String)> {
    let mut found = Vec::new();
    for at in text::word_offsets(code, "fn") {
        let line_start = code[..at].rfind('\n').map_or(0, |n| n + 1);
        let qualifiers = &code[line_start..at];
        let after = &code[at + 2..];
        let name: &str = after
            .trim_start()
            .split(|ch: char| !(ch.is_alphanumeric() || ch == '_'))
            .next()
            .unwrap_or("");
        let Some(mut open) = after.find(['(', '<']) else {
            continue;
        };
        if after.as_bytes()[open] == b'<' {
            let Some(generics) = text::balanced(after, open) else {
                continue;
            };
            open += generics.len() + 2;
        }
        let Some(parameters) = text::balanced(after, open) else {
            continue;
        };
        if name.is_empty() || after.as_bytes().get(open) != Some(&b'(') {
            continue;
        }
        let line = line_of(code, at);
        let items = text::top_level_items(parameters);
        let only_self = items.len() == 1 && text::has_word(items[0], "self");
        if name.starts_with("get_") && only_self {
            let message = format!("`{name}`: a getter has no `get_` prefix");
            found.push((line, Rule::GetPrefix, message));
        }
        if !text::has_word(qualifiers, "pub") || qualifiers.contains("pub(") {
            continue;
        }
        for item in &items {
            if item
                .rsplit(':')
                .next()
                .is_some_and(|ty| ty.trim() == "bool")
                && item.contains(':')
            {
                let message = format!("`{name}` takes a `bool`: prefer an enum");
                found.push((line, Rule::BoolParameter, message));
            }
        }
        let rest = &after[open + parameters.len() + 2..];
        let returned: &str = rest.split(['{', ';']).next().unwrap_or("");
        let returned = returned.split(" where ").next().unwrap_or("");
        if has_bare_tuple(returned) {
            let message = format!("`{name}` returns a tuple of bare numbers: name the units");
            found.push((line, Rule::BareTuple, message));
        }
    }
    found
}

/// The timeless words and the narration in rustdoc, outside its code.
fn wording(docs: &str) -> Vec<(usize, Rule, String)> {
    let mut blocks: Vec<Vec<(usize, &str)>> = vec![Vec::new()];
    let mut fenced = false;
    for (index, line) in docs.lines().enumerate() {
        let prose = line.trim_start().trim_start_matches(['/', '!', '*']).trim();
        let fence = prose.starts_with("```");
        if fence {
            fenced = !fenced;
        }
        if fence || fenced || prose.is_empty() {
            blocks.push(Vec::new());
        } else {
            blocks
                .last_mut()
                .expect("one block at least")
                .push((index + 1, prose));
        }
    }
    let mut found = Vec::new();
    for block in &blocks {
        for (line, term) in prose::timeless(block) {
            let message = format!("\"{term}\": say what the software does");
            found.push((line, Rule::Timeless, message));
        }
        for (line, phrase) in prose::narration(block) {
            let message = format!("\"{phrase}\": say what is, not how it came to be");
            found.push((line, Rule::Narration, message));
        }
    }
    found
}

/// The findings in the Rust file `file`, whose contents are `source`.
pub fn check(file: &str, source: &str) -> Vec<Finding> {
    let classes = classify(source);
    let code = only(source, &classes, Class::Code);
    let comments = only(source, &classes, Class::Comment);
    let docs = only(source, &classes, Class::Doc);
    let mut findings = Vec::new();
    let mut add = |rule: Rule, found: Vec<(usize, String)>| {
        for (line, message) in found {
            findings.push(Finding::new(rule, file, line, message));
        }
    };
    add(Rule::UseBraces, use_braces(&code));
    add(Rule::HexCase, hex_case(&code));
    add(Rule::LintException, lint_exceptions(&code));
    add(Rule::Todo, todos(&comments));
    add(Rule::Todo, todos(&docs));
    for (line, rule, message) in signatures(&code).into_iter().chain(wording(&docs)) {
        findings.push(Finding::new(rule, file, line, message));
    }
    findings.sort();
    findings
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(source: &str) -> Vec<(usize, Rule)> {
        check("a.rs", source)
            .into_iter()
            .map(|finding| (finding.line, finding.rule))
            .collect()
    }

    #[test]
    fn code_comments_docs_and_literals_are_told_apart() {
        let source = "/// d\nfn f<'a>() { let c = '\"'; let s = r#\"x \" // y\"#; } // c\n/* b */";
        let classes = classify(source);
        assert_eq!(only(source, &classes, Class::Doc).trim(), "/// d");
        assert_eq!(
            only(source, &classes, Class::Comment)
                .split_whitespace()
                .collect::<Vec<_>>(),
            ["//", "c", "/*", "b", "*/"]
        );
        let code = only(source, &classes, Class::Code);
        assert!(code.contains("fn f<'a>() { let c ="));
        assert!(!code.contains('x') && !code.contains('y') && !code.contains('"'));
        // An escaped quote and a byte string end where they should.
        assert_eq!(
            rules("const A: &[u8] = b\"\\\"0xFF\"; const B: u8 = 0x0f;"),
            []
        );
        assert_eq!(
            rules("const A: char = '\\''; const B: u8 = 0xF0;"),
            [(1, Rule::HexCase)]
        );
    }

    #[test]
    fn a_path_inside_use_braces_is_found() {
        assert_eq!(rules("use std::{fs, io};\nuse std::io::{self, Read};"), []);
        assert_eq!(
            rules("use std::{\n    fs,\n    io::Read,\n};"),
            [(1, Rule::UseBraces)]
        );
        assert_eq!(rules("    pub use a::{b::C, d};"), [(1, Rule::UseBraces)]);
    }

    #[test]
    fn uppercase_hex_is_found_in_code_only() {
        assert_eq!(
            rules("let a = 0xFF_u8;\nlet b = 0xff;"),
            [(1, Rule::HexCase)]
        );
        assert_eq!(rules("// 0xFF\nlet s = \"0xFF\"; let n = a0xF;"), []);
    }

    #[test]
    fn lint_exceptions_need_expect_and_a_reason() {
        assert_eq!(
            rules("#[allow(dead_code)]\nfn f() {}"),
            [(1, Rule::LintException)]
        );
        assert_eq!(
            rules("#![allow(\n    clippy::x\n)]"),
            [(1, Rule::LintException)]
        );
        assert_eq!(
            rules("#[expect(dead_code)]\nfn f() {}"),
            [(1, Rule::LintException)]
        );
        assert_eq!(
            rules("#[expect(dead_code, reason = \"used by a test\")]\nfn f() {}"),
            []
        );
        assert_eq!(rules("fn f() { x.expect(\"reason\"); allow(1); }"), []);
    }

    #[test]
    fn a_todo_names_its_owner() {
        assert_eq!(
            rules("// TODO: later\n/// FIXME soon"),
            [(1, Rule::Todo), (2, Rule::Timeless), (2, Rule::Todo)]
        );
        assert_eq!(
            rules("// TODO(owner): x\n// TODO: #12 x\nlet s = \"TODO\";"),
            []
        );
    }

    #[test]
    fn signatures_are_read() {
        assert_eq!(
            rules("fn get_name(&self) -> &str { \"\" }"),
            [(1, Rule::GetPrefix)]
        );
        assert_eq!(
            rules("fn get_or_start(&self, key: u8) {}\nfn get_page(url: &str) {}"),
            []
        );
        assert_eq!(
            rules("pub fn f(a: u8, flag: bool) {}"),
            [(1, Rule::BoolParameter)]
        );
        assert_eq!(
            rules("fn f(flag: bool) {}\npub(crate) fn g(flag: bool) {}"),
            []
        );
        assert_eq!(
            rules("pub fn f<F: Fn(bool) -> bool>(f: F, on: Option<bool>) {}"),
            []
        );
        assert_eq!(
            rules("pub fn f(a: u8) -> Option<(u64, i64)> { None }"),
            [(1, Rule::BareTuple)]
        );
        assert_eq!(
            rules("pub fn f() -> (u64, WallUs) {}\npub fn g() -> Vec<u8> {}"),
            []
        );
        assert_eq!(
            rules("pub async fn f(\n    on: bool,\n) {}"),
            [(1, Rule::BoolParameter)]
        );
    }

    #[test]
    fn rustdoc_is_timeless_outside_its_code() {
        assert_eq!(
            rules("/// The new one.\n//! Runs now.\n// a new comment\nfn f() {}"),
            [(1, Rule::Timeless), (2, Rule::Timeless)]
        );
        assert_eq!(
            rules("/// Calls `new`.\n/// ```\n/// let new = 1;\n/// ```\nfn f() {}"),
            []
        );
    }
}
