//! The rules for Rust source that neither rustfmt nor clippy checks. In
//! code: import form, hex case, lint exceptions, getter names, `bool`
//! parameters, bare integer tuples, types and derives by their paths, one
//! `use` per module path, and test names that join two behaviours. In
//! comments: `TODO` owners, timeless rustdoc, the heading of examples, and
//! the rules of `prose.rs` and `names.rs`. In every line: its width.

use std::collections::BTreeSet;

use super::finding::{Finding, Rule};
use super::names::{self, Names, Shapes};
use super::{prose, text};

/// The widest line of code (`STANDARDS.md` §2).
const CODE_WIDTH: usize = 100;

/// The widest line that holds only a comment, after its indentation.
const COMMENT_WIDTH: usize = 80;

/// What a byte of a source file belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
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
pub fn classify(source: &str) -> Vec<Class> {
    let bytes = source.as_bytes();
    let mut classes = vec![Class::Code; bytes.len()];
    let mut at = 0;
    while at < bytes.len() {
        // Only these bytes start a comment or a literal: most bytes pass here.
        if !b"/\"'bcr".contains(&bytes[at]) {
            at += 1;
            continue;
        }
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
pub fn only(source: &str, classes: &[Class], class: Class) -> String {
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

/// The blocks of prose of rustdoc: runs of documentation lines without their
/// markers, parted at a blank line and at a code fence, without what a fence
/// holds.
fn doc_blocks(docs: &str) -> Vec<Vec<(usize, &str)>> {
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
    blocks.retain(|block| !block.is_empty());
    blocks
}

/// The blocks of prose of the comments that are not documentation. A block
/// is a run of lines that hold only such a comment, without its markers. A
/// blank comment line and a code fence part blocks, and what a fence holds is
/// left out. A comment that follows code on its line is a block by itself.
fn comment_blocks<'a>(source: &'a str, classes: &[Class]) -> Vec<Vec<(usize, &'a str)>> {
    let mut blocks: Vec<Vec<(usize, &str)>> = vec![Vec::new()];
    let mut fenced = false;
    let mut offset = 0;
    for (index, line) in source.split('\n').enumerate() {
        let start = offset;
        offset += line.len() + 1;
        let end = start + line.len();
        let Some(first) = (start..end).find(|at| classes[*at] == Class::Comment) else {
            fenced = false;
            blocks.push(Vec::new());
            continue;
        };
        let last = (first..end)
            .find(|at| classes[*at] != Class::Comment)
            .unwrap_or(end);
        let prose = source[first..last]
            .trim_start()
            .trim_start_matches(['/', '!', '*'])
            .trim_end()
            .trim_end_matches("*/")
            .trim();
        let fence = prose.starts_with("```");
        if !source[start..first].trim().is_empty() {
            blocks.push(Vec::new());
            if !prose.is_empty() {
                blocks.push(vec![(index + 1, prose)]);
            }
            blocks.push(Vec::new());
        } else if fence || fenced || prose.is_empty() {
            fenced ^= fence;
            blocks.push(Vec::new());
        } else {
            blocks
                .last_mut()
                .expect("one block at least")
                .push((index + 1, prose));
        }
    }
    blocks.retain(|block| !block.is_empty());
    blocks
}

/// The offset at which each line of `text` starts.
pub fn line_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0];
    starts.extend(text.match_indices('\n').map(|(at, _)| at + 1));
    starts
}

/// The line, from 1, of the byte at `offset`, given the starts of the lines.
pub fn line_at(starts: &[usize], offset: usize) -> usize {
    starts.partition_point(|start| *start <= offset)
}

/// Whether what passes `limit` characters of the comment `line` is one token
/// that cannot wrap. It is the word over the limit, when that word runs to
/// the end of the line and would pass the limit even on a line by itself.
/// Such a line holds the marker and indentation of the comment, then the
/// word. A word that would fit there can wrap, however long the line before
/// it.
fn one_token_past(line: &str, limit: usize) -> bool {
    let Some((past, ch)) = line.char_indices().nth(limit) else {
        return false;
    };
    let start = line[..past]
        .rfind(char::is_whitespace)
        .map_or(0, |at| at + 1);
    let token = &line[start..];
    let text = line.trim_start_matches(['/', '!']).trim_start();
    let marker = line[..line.len() - text.len()].chars().count();
    let alone = marker + token.chars().count();
    !ch.is_whitespace() && !token.contains(char::is_whitespace) && alone > limit
}

/// The lines too wide: one that holds only a comment and passes
/// [`COMMENT_WIDTH`] characters after its indentation, and any other line
/// that passes [`CODE_WIDTH`]. A line counts characters, not bytes. A
/// comment passes when what passes the width is one token too long for a
/// line by itself, or when it is a row of a table. Neither can wrap, as
/// `STANDARDS.md` §7 says of Markdown.
fn widths(source: &str, classes: &[Class]) -> Vec<(usize, Rule, String)> {
    let mut found = Vec::new();
    let mut offset = 0;
    for (index, line) in source.split('\n').enumerate() {
        let start = offset;
        offset += line.len() + 1;
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.len() <= COMMENT_WIDTH {
            continue;
        }
        let body = line.trim_start();
        let at = start + line.len() - body.len();
        let comment = matches!(classes.get(at), Some(Class::Comment | Class::Doc));
        if comment && body.starts_with("//") {
            let columns = body.chars().count();
            let row = body
                .trim_start_matches(['/', '!'])
                .trim_start()
                .starts_with('|');
            if columns > COMMENT_WIDTH && !row && !one_token_past(body, COMMENT_WIDTH) {
                let message = format!("{columns} characters of comment: wrap at {COMMENT_WIDTH}");
                found.push((index + 1, Rule::CommentWidth, message));
            }
        } else {
            let columns = line.chars().count();
            if columns > CODE_WIDTH {
                let message = format!("{columns} characters: a line fits in {CODE_WIDTH}");
                found.push((index + 1, Rule::CodeWidth, message));
            }
        }
    }
    found
}

/// Rustdoc headings `# Examples`, at any level, in the blocks of prose of
/// rustdoc: the tree's heading is `# Example`.
fn example_headings(blocks: &[Vec<(usize, &str)>]) -> Vec<(usize, String)> {
    let mut found = Vec::new();
    for (line, prose) in blocks.iter().flatten() {
        let title = prose.trim_start_matches('#');
        let heading = title.len() < prose.len() && title.starts_with(' ');
        if heading && title.trim().eq_ignore_ascii_case("examples") {
            found.push((*line, format!("`{prose}`: the heading is `# Example`")));
        }
    }
    found
}

/// The offset of the first whole word `term` in `text`.
fn first_word(text: &str, term: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let word = |at: usize| {
        bytes
            .get(at)
            .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
    };
    let mut from = 0;
    while let Some(at) = text[from..].find(term).map(|n| from + n) {
        if (at == 0 || !word(at - 1)) && !word(at + term.len()) {
            return Some(at);
        }
        from = at + term.len();
    }
    None
}

/// The spans of the attributes of `code`, each from its `#` to the bracket
/// that closes it.
fn attributes(code: &str) -> Vec<(usize, usize)> {
    let bytes = code.as_bytes();
    let mut spans = Vec::new();
    let mut from = 0;
    while let Some(at) = code[from..].find('#').map(|n| from + n) {
        let open = if bytes.get(at + 1) == Some(&b'!') {
            at + 2
        } else {
            at + 1
        };
        let inside = (bytes.get(open) == Some(&b'['))
            .then(|| text::balanced(code, open))
            .flatten();
        from = match inside {
            Some(inside) => {
                let end = open + inside.len() + 2;
                spans.push((at, end));
                end
            }
            None => at + 1,
        };
    }
    spans
}

/// The spans of the `use` declarations of `code`, each from `use` to its
/// semicolon.
fn use_declarations(code: &str) -> Vec<(usize, usize)> {
    let mut spans: Vec<(usize, usize)> = Vec::new();
    for at in text::word_offsets(code, "use") {
        let declared = code[at + 3..].starts_with(char::is_whitespace);
        if declared && spans.last().is_none_or(|(_, end)| *end <= at) {
            let end = code[at..].find(';').map_or(code.len(), |n| at + n + 1);
            spans.push((at, end));
        }
    }
    spans
}

/// Whether `name` names a type or a trait: a capital, then a lowercase
/// letter somewhere, or a capital alone, as a type parameter is. A constant
/// is in capitals throughout.
fn is_type_name(name: &str) -> bool {
    name.starts_with(|ch: char| ch.is_ascii_uppercase())
        && (name.len() == 1 || name.bytes().any(|byte| byte.is_ascii_lowercase()))
}

/// The paths that write a type or a trait through two modules or more, such
/// as `std::time::Instant` and `crate::api::Window`. A path that goes on from
/// the type to a function or a variant is one of them too:
/// `std::time::Instant::now`. A path passes with one module before the type
/// (`fmt::Result`, `io::Error`), and with no type in it (`std::mem::take`,
/// `u64::MAX`). So does a path from `Self` or a type parameter (`T::Item`),
/// and one that goes on from a bracket (`<A as B>::C`), as neither has a
/// module before its type. A path in a macro (`$crate::x::Y`) passes too.
/// `use` declarations and attributes are not read.
fn full_path_types(
    code: &str,
    attributes: &[(usize, usize)],
    uses: &[(usize, usize)],
    starts: &[usize],
) -> Vec<(usize, String)> {
    let mut bytes = code.as_bytes().to_vec();
    for (start, end) in attributes.iter().chain(uses) {
        for byte in &mut bytes[*start..*end] {
            if *byte != b'\n' {
                *byte = b' ';
            }
        }
    }
    let starts_name = |byte: u8| byte.is_ascii_alphabetic() || byte == b'_';
    let in_name = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_';
    let mut found = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        if !starts_name(bytes[at]) {
            at += 1;
            continue;
        }
        let start = at;
        let mut names = Vec::new();
        loop {
            let name = at;
            while at < bytes.len() && in_name(bytes[at]) {
                at += 1;
            }
            names.push(&code[name..at]);
            if bytes[at..].starts_with(b"::") && bytes.get(at + 2).is_some_and(|b| starts_name(*b))
            {
                at += 2;
            } else {
                break;
            }
        }
        // A path of a macro names its crate as `$crate`, which it cannot import.
        let in_macro = start > 0 && bytes[start - 1] == b'$';
        let typed = names.iter().position(|name| is_type_name(name));
        if let Some(index) = typed.filter(|index| *index >= 2 && !in_macro) {
            let path = names[..=index].join("::");
            let (module, name) = (names[index - 1], names[index]);
            let message = format!("`{path}`: import `{name}`, or write `{module}::{name}`");
            found.push((line_at(starts, start), message));
        }
    }
    found
}

/// Derives written by their path in `#[derive(...)]`, such as
/// `thiserror::Error`: a derive is imported and written bare.
fn derive_paths(
    code: &str,
    attributes: &[(usize, usize)],
    starts: &[usize],
) -> Vec<(usize, String)> {
    let mut found = Vec::new();
    for (start, end) in attributes {
        let attribute = &code[*start..*end];
        for at in text::word_offsets(attribute, "derive") {
            let open = at + "derive".len();
            if attribute.as_bytes().get(open) != Some(&b'(') {
                continue;
            }
            let Some(inside) = text::balanced(attribute, open) else {
                continue;
            };
            for item in text::top_level_items(inside) {
                if let Some((_, name)) = item.rsplit_once("::") {
                    let message = format!("`{item}` in a derive: import it and write `{name}`");
                    found.push((line_at(starts, start + at), message));
                }
            }
        }
    }
    found
}

/// The module path that a `use` tree imports from: what stands before its
/// braces, or before its last segment. A tree of one segment has none.
fn module_path(tree: &str) -> Option<String> {
    let tree = tree.trim_end_matches(';');
    let path = match tree.find('{') {
        Some(brace) => tree[..brace].trim_end().trim_end_matches("::"),
        None => tree.rsplit_once("::")?.0,
    };
    let path: String = path.split_whitespace().collect();
    let path = path.trim_start_matches("::");
    (!path.is_empty()).then(|| path.to_owned())
}

/// What stands before a `use` declaration since the statement before it:
/// its outer attributes and its visibility, with the inner attributes of the
/// module left out and white space made single.
fn attributes_and_visibility(text: &str) -> String {
    let mut kept = String::new();
    let mut rest = text;
    while let Some(at) = rest.find("#![") {
        kept.push_str(&rest[..at]);
        let inside = text::balanced(rest, at + 2).map_or(rest.len(), str::len);
        rest = rest.get(at + inside + 4..).unwrap_or("");
    }
    kept.push_str(rest);
    kept.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The `use` declarations that import from a module path that another one
/// of their scope imports from, under the same attributes and visibility:
/// one `use` with braces holds both (`STANDARDS.md` §2). A scope is the file
/// or a pair of braces, so a `use` in a function body stands apart from those
/// of its module, and one under a `cfg` from one without it. A `use` that
/// carries documentation stands apart too, since the braces would lose it.
fn use_per_path(
    code: &str,
    docs: &str,
    uses: &[(usize, usize)],
    starts: &[usize],
) -> Vec<(usize, String)> {
    let bytes = code.as_bytes();
    let mut scopes = vec![0_usize];
    let mut opened = 0;
    let mut boundary = 0;
    let mut seen: BTreeSet<(usize, String, String)> = BTreeSet::new();
    let mut found = Vec::new();
    let mut next = uses.iter().peekable();
    let mut at = 0;
    while at < bytes.len() {
        if let Some((start, end)) = next.next_if(|(start, _)| *start == at) {
            let mut prefix = attributes_and_visibility(&code[boundary..*start]);
            // The documentation of the module is not the declaration's.
            let outer = docs[boundary..*start]
                .lines()
                .filter(|line| !line.trim_start().starts_with("//!"));
            prefix.extend(outer.map(str::trim));
            if let Some(path) = module_path(&code[start + 3..*end]) {
                let scope = *scopes.last().expect("the file's scope");
                if !seen.insert((scope, prefix, path.clone())) {
                    let message =
                        format!("another `use` of this scope imports from `{path}`: join them");
                    found.push((line_at(starts, *start), message));
                }
            }
            at = *end;
            boundary = *end;
            continue;
        }
        match bytes[at] {
            b'{' => {
                opened += 1;
                scopes.push(opened);
                boundary = at + 1;
            }
            b'}' => {
                if scopes.len() > 1 {
                    scopes.pop();
                }
                boundary = at + 1;
            }
            b';' => boundary = at + 1,
            _ => {}
        }
        at += 1;
    }
    found
}

/// Tests whose names hold `_and_`: the name of a test that asserts two
/// behaviours (`STANDARDS.md` §8). A test is the function under `#[test]`,
/// or under an attribute whose path ends in `::test`, such as
/// `#[tokio::test]`.
fn test_names(code: &str, attributes: &[(usize, usize)], starts: &[usize]) -> Vec<(usize, String)> {
    let mut found = Vec::new();
    for (start, end) in attributes {
        let inside = code[*start..*end]
            .trim_start_matches(['#', '!', '['])
            .trim_end_matches(']');
        let path = inside.split('(').next().unwrap_or("").trim();
        if path != "test" && !path.ends_with("::test") {
            continue;
        }
        let rest = &code[*end..];
        let Some(at) = first_word(rest, "fn") else {
            continue;
        };
        let name = rest[at + 2..]
            .trim_start()
            .split(|ch: char| !(ch.is_alphanumeric() || ch == '_'))
            .next()
            .unwrap_or("");
        if name.contains("_and_") {
            let message =
                format!("`{name}`: `_and_` joins two behaviours; give each a test named for it");
            found.push((line_at(starts, end + at), message));
        }
    }
    found
}

/// The timeless words and the narration in rustdoc, outside its code.
fn wording(blocks: &[Vec<(usize, &str)>]) -> Vec<(usize, Rule, String)> {
    let mut found = Vec::new();
    for block in blocks {
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

/// The findings in the Rust file `file`, whose contents are `source`. The
/// names its rustdoc and comments give are checked against `names`, the
/// names of the tree's code, when the tree was read.
pub fn check(file: &str, source: &str, names: Option<&Names>) -> Vec<Finding> {
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
    let starts = line_starts(source);
    let attributes = attributes(&code);
    let uses = use_declarations(&code);
    let doc_blocks = doc_blocks(&docs);
    add(Rule::ExampleHeading, example_headings(&doc_blocks));
    add(
        Rule::FullPathType,
        full_path_types(&code, &attributes, &uses, &starts),
    );
    add(Rule::DerivePath, derive_paths(&code, &attributes, &starts));
    add(Rule::UsePerPath, use_per_path(&code, &docs, &uses, &starts));
    add(Rule::TestNameAnd, test_names(&code, &attributes, &starts));
    let found = signatures(&code)
        .into_iter()
        .chain(wording(&doc_blocks))
        .chain(widths(source, &classes));
    for (line, rule, message) in found {
        findings.push(Finding::new(rule, file, line, message));
    }
    for block in doc_blocks.iter().chain(&comment_blocks(source, &classes)) {
        findings.extend(prose::findings(file, block));
        if let Some(names) = names {
            findings.extend(names::findings(file, block, names, Shapes::All));
        }
    }
    findings.sort();
    findings
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(source: &str) -> Vec<(usize, Rule)> {
        check("a.rs", source, None)
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

    /// A comment of `width` characters: words of two letters after `//`, the
    /// last of them longer when the width asks it.
    fn comment(width: usize) -> String {
        let mut line = "//".to_owned();
        while line.len() + 3 <= width {
            line.push_str(" aa");
        }
        while line.len() < width {
            line.push('a');
        }
        line
    }

    #[test]
    fn a_comment_line_holds_at_most_80_characters() {
        assert_eq!(rules(&comment(80)), []);
        assert_eq!(rules(&comment(81)), [(1, Rule::CommentWidth)]);
    }

    #[test]
    fn the_indentation_of_a_comment_is_not_counted() {
        assert_eq!(rules(&format!("        {}", comment(80))), []);
    }

    #[test]
    fn a_comment_counts_characters_not_bytes() {
        assert_eq!(rules(&format!("/// {}", "§ ".repeat(38))), []);
    }

    #[test]
    fn a_comment_past_the_width_by_a_token_too_long_to_wrap_passes() {
        let url = format!("https://example.com/{}", "a".repeat(70));
        assert_eq!(rules(&format!("/// See\n/// {url}")), []);
        assert_eq!(rules(&format!("/// See {url}.")), []);
    }

    #[test]
    fn a_comment_past_the_width_by_a_word_that_can_wrap_is_found() {
        let path = format!("`crates/{}`", "a".repeat(20));
        let line = format!("/// {} {path}", "word ".repeat(12).trim_end());
        assert_eq!(rules(&line), [(1, Rule::CommentWidth)]);
    }

    #[test]
    fn a_comment_past_the_width_by_words_is_found() {
        let url = format!("https://example.com/{}", "a".repeat(70));
        assert_eq!(
            rules(&format!("/// See {url} and more.")),
            [(1, Rule::CommentWidth)]
        );
    }

    #[test]
    fn a_row_of_a_table_in_a_comment_passes() {
        let row = format!("//! | `GET {}` | a byte-range playlist |", "/a".repeat(40));
        assert_eq!(rules(&row), []);
    }

    #[test]
    fn a_line_of_code_holds_at_most_100_characters() {
        let line = |width: usize| format!("let a = {};", "1".repeat(width - 9));
        assert_eq!(rules(&line(100)), []);
        assert_eq!(rules(&line(101)), [(1, Rule::CodeWidth)]);
    }

    #[test]
    fn a_line_of_code_counts_characters_not_bytes() {
        assert_eq!(rules(&format!("let a = \"{}\";", "§".repeat(89))), []);
    }

    #[test]
    fn a_comment_after_code_counts_as_code() {
        let line = format!("let a = 1; {}", comment(89));
        assert_eq!(rules(&line), []);
        let line = format!("let a = 1; {}", comment(90));
        assert_eq!(rules(&line), [(1, Rule::CodeWidth)]);
    }

    #[test]
    fn a_line_of_a_string_counts_as_code() {
        let source = format!("const A: &str = \"\n{}\n\";", comment(90));
        assert_eq!(rules(&source), []);
    }

    #[test]
    fn an_examples_heading_is_found() {
        assert_eq!(
            rules("/// # Examples\nfn f() {}"),
            [(1, Rule::ExampleHeading)]
        );
        assert_eq!(rules("//! ## Examples"), [(1, Rule::ExampleHeading)]);
        assert_eq!(rules("/// # Example\nfn f() {}"), []);
    }

    #[test]
    fn an_examples_line_in_a_code_block_passes() {
        assert_eq!(rules("/// ```\n/// # Examples\n/// ```\nfn f() {}"), []);
    }

    #[test]
    fn a_type_through_two_modules_is_found() {
        assert_eq!(
            rules("fn f(a: std::time::Instant) {}"),
            [(1, Rule::FullPathType)]
        );
        assert_eq!(
            rules("let w: crate::api::Window;"),
            [(1, Rule::FullPathType)]
        );
        assert_eq!(
            rules("let f = tokio::fs::File::open(p);"),
            [(1, Rule::FullPathType)]
        );
        assert_eq!(
            rules("impl std::fmt::Display for A {}"),
            [(1, Rule::FullPathType)]
        );
    }

    #[test]
    fn a_type_through_one_module_passes() {
        let source = "fn f(a: fmt::Result, b: io::Error) -> serde_json::Value {}";
        assert_eq!(rules(source), []);
    }

    #[test]
    fn a_path_without_a_type_passes() {
        let source = "let a = std::mem::take(&mut b);\nlet c = iris_client::sound_clock::DELAY;\n\
                      std::println!(\"{c:?}\");\nlet d = std::f64::consts::PI;";
        assert_eq!(rules(source), []);
    }

    #[test]
    fn a_path_from_self_or_a_type_parameter_passes() {
        assert_eq!(rules("fn f(a: Self::Item, b: T::Output::Error) {}"), []);
    }

    #[test]
    fn a_path_from_super_alone_passes() {
        assert_eq!(rules("fn f(a: super::Rule) {}"), []);
        assert_eq!(
            rules("fn f(a: super::finding::Rule) {}"),
            [(1, Rule::FullPathType)]
        );
    }

    #[test]
    fn a_path_after_a_bracket_passes() {
        assert_eq!(rules("fn f(a: <A as B>::C, b: Vec::<u8>::Item) {}"), []);
    }

    #[test]
    fn a_path_in_a_macro_passes() {
        let source = "macro_rules! m {\n    () => {\n        $crate::api::Window::new()\n    };\n}";
        assert_eq!(rules(source), []);
    }

    #[test]
    fn a_path_in_a_use_declaration_passes() {
        assert_eq!(
            rules("use std::time::Instant;\nfn f() {\n    use tokio::fs::File;\n}"),
            []
        );
    }

    #[test]
    fn a_path_in_an_attribute_passes() {
        assert_eq!(
            rules("#[cfg_attr(test, doc = std::path::Path)]\nfn f() {}"),
            []
        );
    }

    #[test]
    fn a_path_in_a_comment_or_a_string_passes() {
        assert_eq!(
            rules("// std::time::Instant\nlet a = \"std::time::Instant\";"),
            []
        );
    }

    #[test]
    fn a_derive_by_its_path_is_found() {
        let source = "#[derive(Debug, thiserror::Error)]\nenum E {}";
        assert_eq!(rules(source), [(1, Rule::DerivePath)]);
        let source = "#[cfg_attr(test, derive(serde::Serialize))]\nstruct S;";
        assert_eq!(rules(source), [(1, Rule::DerivePath)]);
    }

    #[test]
    fn a_bare_derive_passes() {
        assert_eq!(rules("#[derive(Debug, Error)]\nenum E {}"), []);
    }

    #[test]
    fn two_uses_of_one_module_path_are_found() {
        assert_eq!(rules("use a::b::X;\nuse a::b::Y;"), [(2, Rule::UsePerPath)]);
        assert_eq!(rules("use std::fs;\nuse std::io;"), [(2, Rule::UsePerPath)]);
        assert_eq!(
            rules("use a::b::{X, Y};\nuse a::b::Z as _;"),
            [(2, Rule::UsePerPath)]
        );
    }

    #[test]
    fn uses_of_one_module_path_in_one_function_are_found() {
        let source = "fn f() {\n    use a::X;\n    let b = 1;\n    use a::Y;\n}";
        assert_eq!(rules(source), [(4, Rule::UsePerPath)]);
    }

    #[test]
    fn an_inner_attribute_parts_no_uses() {
        let source = "#![forbid(unsafe_code)]\n\nuse a::X;\nuse a::Y;";
        assert_eq!(rules(source), [(4, Rule::UsePerPath)]);
    }

    #[test]
    fn uses_of_different_module_paths_pass() {
        assert_eq!(
            rules("use a::b::X;\nuse a::c::Y;\nuse a::b;\nuse super::*;"),
            []
        );
    }

    #[test]
    fn one_use_over_several_lines_passes() {
        assert_eq!(rules("use a::b::{\n    X,\n    Y,\n};"), []);
    }

    #[test]
    fn uses_under_different_cfg_attributes_pass() {
        assert_eq!(rules("use a::b::X;\n#[cfg(test)]\nuse a::b::Y;"), []);
    }

    #[test]
    fn uses_of_different_visibility_pass() {
        assert_eq!(rules("pub use a::X;\nuse a::Y;"), []);
    }

    #[test]
    fn a_documented_use_passes() {
        assert_eq!(rules("/// A.\npub use a::X;\n/// B.\npub use a::Y;"), []);
    }

    #[test]
    fn the_documentation_of_a_module_parts_no_uses() {
        let source = "//! A module.\n\nuse a::X;\nuse a::Y;";
        assert_eq!(rules(source), [(4, Rule::UsePerPath)]);
    }

    #[test]
    fn a_use_in_a_function_beside_one_of_its_module_passes() {
        assert_eq!(rules("use a::X;\nfn f() {\n    use a::Y;\n}"), []);
    }

    #[test]
    fn a_test_whose_name_joins_two_behaviours_is_found() {
        let tests = [
            ("#[test]\nfn reads_x_and_writes_y() {}", 2),
            ("#[tokio::test]\nasync fn reads_x_and_writes_y() {}", 2),
            (
                "#[tokio::test(flavor = \"current_thread\")]\n#[ignore]\nasync fn x_and_y() {}",
                3,
            ),
        ];
        for (source, line) in tests {
            assert_eq!(rules(source), [(line, Rule::TestNameAnd)], "{source}");
        }
    }

    #[test]
    fn a_function_that_is_no_test_passes() {
        assert_eq!(
            rules("fn reads_x_and_writes_y() {}\n#[cfg(test)]\nfn a_and_b() {}"),
            []
        );
    }

    #[test]
    fn a_test_with_band_in_its_name_passes() {
        assert_eq!(rules("#[test]\nfn handles_band_edges() {}"), []);
    }

    /// `count` words of comment after `marker`, ten to a line.
    fn comment_words(marker: &str, count: usize) -> String {
        let words = vec!["word"; count];
        let lines: Vec<String> = words
            .chunks(10)
            .map(|chunk| format!("{marker} {}", chunk.join(" ")))
            .collect();
        lines.join("\n")
    }

    #[test]
    fn a_long_sentence_in_a_comment_is_found() {
        let source = comment_words("//", 36);
        assert_eq!(rules(&source), [(1, Rule::SentenceLength)]);
    }

    #[test]
    fn a_comment_after_code_is_a_block_by_itself() {
        let words = comment_words("//", 20);
        let source = format!("{words}\nlet a = 1; // {}\n{words}", "word ".repeat(10));
        assert_eq!(rules(&source), []);
    }

    #[test]
    fn code_between_comments_parts_their_blocks() {
        let words = comment_words("//", 20);
        assert_eq!(rules(&format!("{words}\nfn f() {{}}\n{words}")), []);
        let docs = comment_words("///", 20);
        assert_eq!(rules(&format!("{docs}\nfn f() {{}}\n{docs}")), []);
    }

    #[test]
    fn a_code_block_in_rustdoc_holds_no_sentence() {
        let source = format!(
            "/// ```\n{}\n/// ```\nfn f() {{}}",
            comment_words("///", 40)
        );
        assert_eq!(rules(&source), []);
    }

    #[test]
    fn the_rules_of_words_read_every_comment() {
        let source = "/// It reads as one.\n// It runs over the core.\n/// The `Feed`'s token.";
        assert_eq!(
            rules(source),
            [
                (1, Rule::BarredPhrase),
                (2, Rule::TheCore),
                (3, Rule::PossessiveCodeSpan)
            ]
        );
    }

    /// The rules found in `source` against the names of the Rust code
    /// `code`.
    fn named_rules(source: &str, code: &str) -> Vec<(usize, Rule)> {
        let names = Names::of([("src/b.rs", code)], &[]);
        check("a.rs", source, Some(&names))
            .into_iter()
            .map(|finding| (finding.line, finding.rule))
            .collect()
    }

    #[test]
    fn a_name_in_rustdoc_is_checked() {
        let found = named_rules("/// The `Gone` type.\nstruct Held;", "struct Held;");
        assert_eq!(found, [(1, Rule::UnknownName)]);
    }

    #[test]
    fn a_name_in_a_comment_is_checked() {
        let found = named_rules("// See `Held::gone()`.\nstruct Held;", "struct Held;");
        assert_eq!(found, [(1, Rule::UnknownName)]);
    }

    #[test]
    fn a_name_in_a_code_block_of_rustdoc_is_not_checked() {
        let source = "/// ```\n/// `Gone`\n/// ```\nstruct Held;";
        assert_eq!(named_rules(source, "struct Held;"), []);
    }
}
