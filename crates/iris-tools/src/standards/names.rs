//! The rule of names in prose. A code span with the shape of Rust code names
//! only identifiers that the code of the tree holds, so a document that names
//! a renamed or a removed item is found.

use std::collections::HashSet;
use std::path::Path;

use super::finding::{Finding, Rule};
use super::rust::{self, Class};
use super::{prose, text};

/// What the tree holds that a name in prose is checked against.
#[derive(Debug, Default)]
pub struct Names {
    /// The identifiers of the code of the tree's Rust files, outside their
    /// comments and literals, and the names of its packages and libraries.
    held: HashSet<String>,
    /// The names that the code of the workspace declares, outside the
    /// vendored trees, and the names of its packages and libraries. Code
    /// declares the identifier after `struct`, `enum`, `union`, `trait`,
    /// `type`, `mod`, `fn`, `const`, `static`, `crate`, `macro_rules!`, or
    /// `as`.
    declared: HashSet<String>,
    /// The words the tree holds as data, in the string literals of its Rust
    /// files and in its files that are neither Rust, Markdown, nor plain
    /// text. A unit file and a fixture are such files. [`EXCEPTIONS`] is
    /// none: the reason of a waiver may name what it waives.
    data: HashSet<String>,
}

/// The file of accepted exceptions, whose words are no data of the tree.
const EXCEPTIONS: &str = "standards.toml";

/// The keywords after which code declares a name.
const DECLARING: [&str; 12] = [
    "struct",
    "enum",
    "union",
    "trait",
    "type",
    "mod",
    "fn",
    "const",
    "static",
    "crate",
    "macro_rules",
    "as",
];

impl Names {
    /// The names of `files`, each a path and its text. A file under one of
    /// the `vendored` directories declares no name of the workspace.
    pub fn of<'a>(files: impl IntoIterator<Item = (&'a str, &'a str)>, vendored: &[&str]) -> Self {
        let mut names = Self::default();
        for (path, text) in files {
            let file = Path::new(path);
            let extension = file.extension().and_then(|extension| extension.to_str());
            let own = !vendored.iter().any(|tree| path.starts_with(tree));
            if extension == Some("rs") {
                let classes = rust::classify(text);
                let mut after_keyword = false;
                for (class, name) in classed_identifiers(text, &classes) {
                    match class {
                        Class::Code => {
                            if after_keyword && own {
                                add(&mut names.declared, name);
                            }
                            after_keyword = DECLARING.contains(&name);
                            add(&mut names.held, name);
                        }
                        Class::Literal => add(&mut names.data, name),
                        Class::Comment | Class::Doc => {}
                    }
                }
            } else if file.file_name().is_some_and(|name| name == "Cargo.toml") {
                let packages = manifest_names(text);
                names.held.extend(packages.iter().cloned());
                if own {
                    names.declared.extend(packages);
                }
            } else if !matches!(extension, Some("md" | "txt")) && path != EXCEPTIONS {
                for word in identifiers(text) {
                    add(&mut names.data, word);
                }
            }
        }
        names
    }
}

/// Puts `name` in `set`, making a copy only when the set lacks it.
fn add(set: &mut HashSet<String>, name: &str) {
    if !set.contains(name) {
        set.insert(name.to_owned());
    }
}

/// Whether `byte` can be part of an identifier.
fn in_identifier(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// The identifiers of `code`: each run of letters, digits, and underscores
/// that starts with a letter or an underscore. A number such as `0x1f`
/// starts with a digit and is none.
fn identifiers(code: &str) -> impl Iterator<Item = &str> {
    code.split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
        .filter(|run| run.starts_with(|ch: char| ch.is_ascii_alphabetic() || ch == '_'))
}

/// The identifiers of the Rust source `source`, as [`identifiers`] finds
/// them, each with what it belongs to. A run of one class ends where the
/// class does.
fn classed_identifiers<'a>(source: &'a str, classes: &[Class]) -> Vec<(Class, &'a str)> {
    let bytes = source.as_bytes();
    let mut found = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        if !in_identifier(bytes[at]) {
            at += 1;
            continue;
        }
        let (start, class) = (at, classes[at]);
        while at < bytes.len() && in_identifier(bytes[at]) && classes[at] == class {
            at += 1;
        }
        if !bytes[start].is_ascii_digit() {
            found.push((class, &source[start..at]));
        }
    }
    found
}

/// The names of the package and the library that a manifest declares, each
/// `-` made `_` as a path writes it.
fn manifest_names(manifest: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut section = "";
    for line in manifest.lines().map(str::trim) {
        if line.starts_with('[') {
            section = line;
            continue;
        }
        let value = line
            .strip_prefix("name")
            .map(str::trim_start)
            .and_then(|rest| rest.strip_prefix('='));
        if let Some(value) = value.filter(|_| ["[package]", "[lib]"].contains(&section)) {
            names.push(value.trim().trim_matches('"').replace('-', "_"));
        }
    }
    names
}

/// The shapes of a code span that the rule takes for a name of code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// Two identifiers or more joined by `::`, perhaps with generic
    /// arguments and then `()` or `!`.
    Path,
    /// One identifier in snake case and `()`.
    Call,
    /// One identifier in upper camel case.
    Type,
}

/// Which shapes of code span a text is checked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shapes {
    /// Paths, calls, and the names of types.
    All,
    /// Paths and calls: a name in upper camel case alone is not checked.
    PathsAndCalls,
}

/// The keywords that start a path within the tree.
const KEYWORDS: [&str; 4] = ["Self", "self", "crate", "super"];

/// The primitive types, whose paths lead into the standard library.
const PRIMITIVES: [&str; 17] = [
    "bool", "char", "str", "u8", "u16", "u32", "u64", "u128", "usize", "i8", "i16", "i32", "i64",
    "i128", "isize", "f32", "f64",
];

/// Whether `name` is in upper camel case as the Rust API Guidelines write it
/// (C-CASE), where an acronym is one word. It has a capital first, a
/// lowercase letter somewhere, only letters and digits, and no two capitals
/// in a row. `AVPlayer` and `CSeq` are names of another system.
fn upper_camel(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.first().is_some_and(u8::is_ascii_uppercase)
        && bytes.iter().any(u8::is_ascii_lowercase)
        && bytes.iter().all(u8::is_ascii_alphanumeric)
        && !bytes
            .windows(2)
            .any(|pair| pair[0].is_ascii_uppercase() && pair[1].is_ascii_uppercase())
}

/// Whether `name` is made on `foo`, the placeholder of the Rust guides, as
/// `foo_mut` and `FooBar` are: it stands for any name.
fn placeholder(name: &str) -> bool {
    let camel = name
        .strip_prefix("Foo")
        .is_some_and(|rest| !rest.starts_with(|ch: char| ch.is_ascii_lowercase()));
    camel || name.split('_').any(|word| word == "foo")
}

/// Whether `name` is in snake case, as a Rust function is named (C-CASE).
fn snake(name: &str) -> bool {
    name.bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

/// The characters that generic arguments without white space may hold.
fn in_generics(byte: u8) -> bool {
    in_identifier(byte) || b":<>,&'[];()".contains(&byte)
}

/// What a code span names.
#[derive(Debug)]
struct Named<'a> {
    /// Its shape.
    shape: Shape,
    /// Its first identifier.
    root: &'a str,
    /// Its identifiers but the keywords, the primitive types, and the
    /// placeholders, each once.
    identifiers: Vec<&'a str>,
}

/// What `span` names, when it has one of the shapes of [`Shape`]. A span that
/// holds any other character, such as a space, `-`, `.`, `/`, or `=`, is a
/// command, a file, a header, or a name of another language, and names no
/// code. A lifetime in the generic arguments is no identifier.
fn named(span: &str) -> Option<Named<'_>> {
    let bytes = span.as_bytes();
    let mut segments = Vec::new();
    let mut at = 0;
    loop {
        if !bytes
            .get(at)
            .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
        {
            return None;
        }
        let start = at;
        while bytes.get(at).copied().is_some_and(in_identifier) {
            at += 1;
        }
        segments.push(&span[start..at]);
        let next = bytes.get(at + 2);
        if bytes[at..].starts_with(b"::") && next.is_some_and(|byte| *byte != b'<') {
            at += 2;
        } else {
            break;
        }
    }
    let open = if bytes[at..].starts_with(b"::<") {
        Some(at + 2)
    } else {
        (bytes.get(at) == Some(&b'<')).then_some(at)
    };
    let mut generics = "";
    if let Some(open) = open {
        generics = text::balanced(span, open)?;
        if !generics.bytes().all(in_generics) {
            return None;
        }
        at = open + generics.len() + 2;
    }
    let root = segments[0];
    let shape = match (segments.len(), generics.is_empty(), &span[at..]) {
        (2.., _, "" | "()" | "!") => Shape::Path,
        (1, true, "()") if snake(root) => Shape::Call,
        (1, true, "") if upper_camel(root) => Shape::Type,
        _ => return None,
    };
    let bytes = generics.as_bytes();
    let in_generic = generics
        .match_indices(|ch: char| ch.is_ascii_alphabetic() || ch == '_')
        .filter(|(start, _)| *start == 0 || !in_identifier(bytes[start - 1]))
        .filter(|(start, _)| *start == 0 || bytes[start - 1] != b'\'')
        .map(|(start, _)| {
            let length = bytes[start..]
                .iter()
                .take_while(|byte| in_identifier(**byte))
                .count();
            &generics[start..start + length]
        });
    let mut identifiers: Vec<&str> = Vec::new();
    for name in segments.into_iter().chain(in_generic) {
        let built_in = KEYWORDS.contains(&name) || PRIMITIVES.contains(&name) || name == "_";
        if !built_in && !placeholder(name) && !identifiers.contains(&name) {
            identifiers.push(name);
        }
    }
    Some(Named {
        shape,
        root,
        identifiers,
    })
}

/// The code spans of `lines`, a stretch of prose with no blank line in it:
/// the line each opens on and what it holds. A span may wrap from one line to
/// the next.
fn spans(lines: &[(usize, &str)]) -> Vec<(usize, String)> {
    let mut text = String::new();
    let mut starts = Vec::new();
    for (number, line) in lines {
        starts.push((text.len(), *number));
        text.push_str(line);
        text.push(' ');
    }
    let line_at =
        |offset: usize| starts[starts.partition_point(|(start, _)| *start <= offset) - 1].1;
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] != b'`' {
            at += 1;
            continue;
        }
        let run = bytes[at..].iter().take_while(|byte| **byte == b'`').count();
        match prose::closing_run(bytes, at + run, run) {
            Some(close) => {
                found.push((line_at(at), text[at + run..close].to_owned()));
                at = close + run;
            }
            None => at += run,
        }
    }
    found
}

/// The identifiers of `given` that the code of the tree does not hold, of
/// those the rule checks:
///
/// - Of a call, its name.
/// - Of a type name, the name, when `shapes` checks type names and the tree
///   does not hold it as data. Such a name is a header, an operation, or a
///   field of another system, such as `Authorization`.
/// - Of a path that starts in the workspace's code, at a keyword or at a name
///   the workspace declares, every identifier.
/// - Of a path from a name that code holds and the workspace does not
///   declare, none. It goes into a dependency or the standard library, as
///   `reqwest::Client` and `str::from_utf8` do, whose items a document may
///   name at another version.
/// - Of a path from a name that no code holds, the name, checked as a type
///   name is. A lowercase one is a crate that the tree does not use.
fn refused<'a>(given: &Named<'a>, names: &Names, shapes: Shapes) -> Vec<&'a str> {
    let unheld = |name: &&str| !names.held.contains(*name);
    let type_refused = |name: &str| {
        shapes == Shapes::All
            && upper_camel(name)
            && !placeholder(name)
            && !names.held.contains(name)
            && !names.data.contains(name)
    };
    let root = given.root;
    let within = KEYWORDS.contains(&root) || names.declared.contains(root);
    match given.shape {
        Shape::Call => given.identifiers.iter().copied().filter(unheld).collect(),
        Shape::Path if within => given.identifiers.iter().copied().filter(unheld).collect(),
        Shape::Path | Shape::Type if type_refused(root) => vec![root],
        Shape::Path | Shape::Type => Vec::new(),
    }
}

/// The findings of `unknown-name` in `lines`, a stretch of the prose of
/// `file` with no blank line in it: each code span of the `shapes` checked
/// that holds an identifier [`refused`] gives. `Self`, `self`, `crate`,
/// `super`, the primitive types, and the names made on `foo` pass.
pub fn findings(
    file: &str,
    lines: &[(usize, &str)],
    names: &Names,
    shapes: Shapes,
) -> Vec<Finding> {
    let mut found = Vec::new();
    for (line, span) in spans(lines) {
        let Some(given) = named(&span) else {
            continue;
        };
        let unknown: Vec<String> = refused(&given, names, shapes)
            .into_iter()
            .map(|name| format!("`{name}`"))
            .collect();
        if !unknown.is_empty() {
            let message = format!(
                "`{span}`: no code of the tree holds {}; name what the code holds",
                unknown.join(", ")
            );
            found.push(Finding::new(Rule::UnknownName, file, line, message));
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The names of one Rust file whose code is `code`.
    fn names(code: &str) -> Names {
        Names::of([("src/a.rs", code)], &[])
    }

    /// The lines of `text`, numbered from 1.
    fn numbered(text: &str) -> Vec<(usize, &str)> {
        text.lines()
            .enumerate()
            .map(|(at, line)| (at + 1, line))
            .collect()
    }

    /// The messages of the findings in `text` against the names of `code`.
    fn found(text: &str, code: &str) -> Vec<String> {
        findings("a.md", &numbered(text), &names(code), Shapes::All)
            .into_iter()
            .map(|finding| finding.message)
            .collect()
    }

    const CODE: &str = "enum ApiError { Status }\nuse reqwest::Client;\nfn take() {}";

    #[test]
    fn a_path_to_a_name_no_code_holds_is_found() {
        assert_eq!(
            found("It returns `ApiError::Encode`.", CODE),
            ["`ApiError::Encode`: no code of the tree holds `Encode`; name what the code holds"]
        );
    }

    #[test]
    fn a_path_whose_names_the_code_holds_passes() {
        assert_eq!(found("`ApiError::Status`", CODE), [""; 0]);
    }

    #[test]
    fn a_path_from_a_type_name_no_code_holds_is_found() {
        assert_eq!(
            found("`Gone::Encode`", CODE),
            ["`Gone::Encode`: no code of the tree holds `Gone`; name what the code holds"]
        );
    }

    #[test]
    fn a_path_into_a_dependency_passes() {
        assert_eq!(found("`reqwest::Client::gone()`", CODE), [""; 0]);
    }

    #[test]
    fn a_path_from_a_name_only_vendored_code_declares_passes() {
        let files = [
            ("third_party/x/src/a.rs", "pub struct Session;"),
            ("src/a.rs", "use x::Session;"),
        ];
        let tree = Names::of(files, &["third_party/"]);
        let found = findings("a.md", &numbered("`Session::gone()`"), &tree, Shapes::All);
        assert_eq!(found, []);
    }

    #[test]
    fn a_path_from_a_lowercase_name_no_code_holds_passes() {
        assert_eq!(found("`wgpu::Features::gone`", CODE), [""; 0]);
    }

    #[test]
    fn a_path_from_a_type_name_no_code_holds_passes_where_type_names_are_not_checked() {
        let lines = numbered("`Gone::Status`");
        let found = findings("a.md", &lines, &names(CODE), Shapes::PathsAndCalls);
        assert_eq!(found, []);
    }

    #[test]
    fn a_name_made_on_foo_passes() {
        let text = "`foo()`, `set_foo()`, `foo_mut()`, `Foo`, `FooBar::gone`, `ApiError::foo`";
        assert_eq!(found(text, CODE), [""; 0]);
    }

    #[test]
    fn a_name_that_starts_with_foo_as_a_word_part_is_checked() {
        assert_eq!(found("`Food`, `food()`", CODE).len(), 2);
    }

    #[test]
    fn a_path_from_a_primitive_type_passes() {
        assert_eq!(found("`str::from_gone()`", CODE), [""; 0]);
    }

    #[test]
    fn a_path_from_a_keyword_is_checked() {
        assert_eq!(found("`Self::Encode`", CODE).len(), 1);
    }

    #[test]
    fn a_path_from_a_package_of_the_tree_is_checked() {
        let manifest = "[package]\nname = \"iris-a\"\n";
        let tree = Names::of(
            [("crates/iris-a/Cargo.toml", manifest), ("src/a.rs", CODE)],
            &[],
        );
        let lines = numbered("`iris_a::ApiError` and `iris_a::Gone`");
        let found = findings("a.md", &lines, &tree, Shapes::All);
        assert_eq!(found.len(), 1);
        assert!(found[0].message.starts_with("`iris_a::Gone`"));
    }

    #[test]
    fn a_call_of_a_name_no_code_holds_is_found() {
        assert_eq!(found("`encode()`", CODE).len(), 1);
    }

    #[test]
    fn a_call_of_a_name_the_code_holds_passes() {
        assert_eq!(found("`take()`", CODE), [""; 0]);
    }

    #[test]
    fn a_call_that_is_not_in_snake_case_passes() {
        assert_eq!(found("`sampleReceiver()`", CODE), [""; 0]);
    }

    #[test]
    fn a_type_name_no_code_holds_is_found() {
        assert_eq!(found("`Encoder`", CODE).len(), 1);
    }

    #[test]
    fn a_type_name_with_two_capitals_in_a_row_passes() {
        assert_eq!(found("`AVPlayer`, `CSeq`, `NRestarts`", CODE), [""; 0]);
    }

    #[test]
    fn a_type_name_in_a_string_literal_passes() {
        let code = "const A: &str = \"Authorization: Digest\";";
        assert_eq!(found("`Authorization`", code), [""; 0]);
    }

    #[test]
    fn a_type_name_in_a_file_of_configuration_passes() {
        let tree = Names::of([("packaging/a.service", "WatchdogSec=10\n")], &[]);
        let found = findings("a.md", &numbered("`WatchdogSec`"), &tree, Shapes::All);
        assert_eq!(found, []);
    }

    #[test]
    fn a_word_of_prose_is_no_data() {
        let tree = Names::of([("docs/a.md", "Encoder"), ("docs/b.txt", "Encoder")], &[]);
        let found = findings("a.md", &numbered("`Encoder`"), &tree, Shapes::All);
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn a_word_of_the_file_of_exceptions_is_no_data() {
        let tree = Names::of([("standards.toml", "reason = \"Encoder\"\n")], &[]);
        let found = findings("a.md", &numbered("`Encoder`"), &tree, Shapes::All);
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn a_type_name_passes_where_only_paths_or_calls_are_checked() {
        let lines = numbered("`Encoder`");
        let found = findings("a.md", &lines, &names(CODE), Shapes::PathsAndCalls);
        assert_eq!(found, []);
    }

    #[test]
    fn a_word_that_is_no_type_name_passes() {
        assert_eq!(found("`encode`, `ENCODE`, `E`, `Encode_It`", CODE), [""; 0]);
    }

    #[test]
    fn a_path_with_a_call_or_a_bang_is_read() {
        assert_eq!(found("`ApiError::encode()`", CODE).len(), 1);
        assert_eq!(found("`ApiError::encode!`", CODE).len(), 1);
    }

    #[test]
    fn a_path_with_arguments_in_its_call_passes() {
        assert_eq!(found("`ApiError::encode(a)`", CODE), [""; 0]);
    }

    #[test]
    fn the_generic_arguments_of_a_path_are_read() {
        assert_eq!(
            found("`ApiError::Status<Encode>`", CODE),
            [concat!(
                "`ApiError::Status<Encode>`: no code of the tree holds `Encode`; ",
                "name what the code holds"
            )]
        );
        assert_eq!(found("`ApiError::take::<Encode>()`", CODE).len(), 1);
    }

    #[test]
    fn a_lifetime_in_generic_arguments_is_no_name() {
        assert_eq!(found("`ApiError::Status<'encode>`", CODE), [""; 0]);
    }

    #[test]
    fn the_keywords_of_paths_pass() {
        let text = "`Self::Status`, `self::take()`, `crate::ApiError`, `super::take()`";
        assert_eq!(found(text, CODE), [""; 0]);
    }

    #[test]
    fn the_primitive_types_pass() {
        assert_eq!(
            found("`ApiError::Status<u64>`, `u8::take()`", CODE),
            [""; 0]
        );
    }

    #[test]
    fn a_span_with_a_character_no_path_holds_passes() {
        let text = "`ApiError:: Encode`, `iris-encode`, `a.encode()`, `src/Encode`, `a::b=c`";
        assert_eq!(found(text, CODE), [""; 0]);
    }

    #[test]
    fn every_unknown_name_of_a_span_is_named() {
        assert_eq!(
            found("`ApiError::Encode<Decode>`", CODE),
            [concat!(
                "`ApiError::Encode<Decode>`: no code of the tree holds `Encode`, `Decode`; ",
                "name what the code holds"
            )]
        );
    }

    #[test]
    fn a_finding_is_on_the_line_its_span_opens() {
        let lines = numbered("One line.\nThen `ApiError::Encode`.");
        let found = findings("a.md", &lines, &names(CODE), Shapes::All);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].line, 2);
    }

    #[test]
    fn a_span_that_wraps_passes() {
        assert_eq!(found("`ApiError::\nEncode`", CODE), [""; 0]);
    }

    #[test]
    fn a_span_of_two_backticks_is_one_span() {
        assert_eq!(found("``ApiError::Encode``", CODE).len(), 1);
        assert_eq!(found("`` `Encode` ``", CODE), [""; 0]);
    }

    #[test]
    fn a_name_in_a_comment_or_a_literal_of_code_is_not_held() {
        let tree = names("// Encode\nconst A: &str = \"Encode\";\n/// Encode\nfn f() {}");
        assert!(!tree.held.contains("Encode"));
    }

    #[test]
    fn a_number_in_code_is_no_name() {
        let tree = names("const A: u8 = 0x1f;");
        assert!(!tree.held.contains("x1f"));
        assert!(tree.held.contains("A"));
    }

    #[test]
    fn the_names_after_a_declaring_keyword_are_declared() {
        let code = "mod m;\nfn f() {}\nmacro_rules! g {}\nextern crate h;\nuse x::Y as Z;\n\
                    impl W {}";
        let tree = names(code);
        let declared: Vec<&str> = ["m", "f", "g", "h", "Z"]
            .into_iter()
            .filter(|name| tree.declared.contains(*name))
            .collect();
        assert_eq!(declared, ["m", "f", "g", "h", "Z"]);
        assert!(!tree.declared.contains("W"));
    }

    #[test]
    fn a_package_or_a_library_of_a_manifest_is_declared_with_underscores() {
        let manifest = "[package]\nname = \"iris-encode\"\n\n[lib]\nname = \"encode_lib\"\n";
        let tree = Names::of([("crates/a/Cargo.toml", manifest)], &[]);
        assert!(tree.declared.contains("iris_encode"));
        assert!(tree.held.contains("encode_lib"));
    }

    #[test]
    fn another_name_of_a_manifest_is_not_held() {
        let manifest = "[[bin]]\nname = \"encode-bin\"\n\n[dependencies]\nserde = \"1\"\n";
        let tree = Names::of([("crates/a/Cargo.toml", manifest)], &[]);
        assert!(!tree.held.contains("encode_bin"));
        assert!(!tree.held.contains("serde"));
    }

    #[test]
    fn a_markdown_file_adds_no_name() {
        let tree = Names::of([("docs/a.md", "fn Encode() {}")], &[]);
        assert!(!tree.held.contains("Encode"));
    }

    #[test]
    fn the_words_of_data_start_with_a_letter_or_an_underscore() {
        let words: Vec<&str> = identifiers("_a 0x1f b").collect();
        assert_eq!(words, ["_a", "b"]);
    }

    #[test]
    fn an_identifier_that_ends_its_file_is_held() {
        assert_eq!(found("`take()`", "fn take"), [""; 0]);
    }

    #[test]
    fn a_call_with_an_underscore_in_its_name_is_checked() {
        assert_eq!(found("`gone_call()`", CODE).len(), 1);
    }

    #[test]
    fn a_call_with_a_digit_in_its_name_is_checked() {
        assert_eq!(found("`gone2()`", CODE).len(), 1);
    }

    #[test]
    fn a_path_whose_generic_arguments_hold_a_space_passes() {
        assert_eq!(found("`ApiError::take<Gone, Other>`", CODE), [""; 0]);
    }

    #[test]
    fn a_path_segment_may_start_with_an_underscore() {
        assert_eq!(found("`crate::_gone`", CODE).len(), 1);
    }

    #[test]
    fn a_word_alone_in_lower_case_has_no_shape() {
        assert!(named("encode").is_none());
    }

    #[test]
    fn each_generic_argument_of_a_path_is_checked() {
        assert_eq!(
            found("`ApiError::take<Status,Gone>`", CODE),
            [
                "`ApiError::take<Status,Gone>`: no code of the tree holds `Gone`; name what the \
              code holds"
            ]
        );
    }

    #[test]
    fn the_text_between_two_code_spans_is_no_span() {
        assert_eq!(found("`ApiError`Gone::x`take()`", CODE), [""; 0]);
    }

    #[test]
    fn a_backtick_that_nothing_closes_opens_no_span() {
        assert_eq!(
            found("`ApiError::Status` and a stray ` here", CODE),
            [""; 0]
        );
    }
}
