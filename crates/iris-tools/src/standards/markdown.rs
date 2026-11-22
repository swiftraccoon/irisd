//! The rules for Markdown: width, timeless wording, what scripted edits
//! leave, counts of cameras, and the recipes and paths a document names. It
//! runs the rules of sentences and phrases in `prose.rs` and the rule of
//! names in `names.rs`.

use std::path::Path;

use super::finding::{Finding, Rule};
use super::layout::Repo;
use super::names::{self, Shapes};
use super::{prose, text};

/// The widest line of prose (`STANDARDS.md` §7).
const WIDTH: usize = 100;

/// The directories a path in a document starts from.
const ROOTS: [&str; 8] = [
    "crates/",
    "docs/",
    "fuzz/",
    "config/",
    "packaging/",
    "third_party/",
    ".github/",
    ".rust-reference/",
];

/// What a line of a Markdown file is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Inside a fenced code block, or one of its fences.
    Fenced,
    /// A table row.
    Table,
    /// A heading.
    Heading,
    /// Anything else.
    Prose,
}

/// The lines of `text`, numbered from 1, each with what it is.
fn lines(text: &str) -> Vec<(usize, &str, Kind)> {
    let mut fenced = false;
    let mut out = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        let fence = trimmed.starts_with("```") || trimmed.starts_with("~~~");
        let kind = if fenced || fence {
            Kind::Fenced
        } else if trimmed.starts_with('|') {
            Kind::Table
        } else if trimmed.starts_with('#') {
            Kind::Heading
        } else {
            Kind::Prose
        };
        if fence {
            fenced = !fenced;
        }
        out.push((index + 1, line, kind));
    }
    out
}

/// The stretches of prose of `text`: runs of prose lines that no blank line
/// parts, and each table row and heading by itself.
fn blocks(text: &str) -> Vec<Vec<(usize, &str)>> {
    let mut blocks: Vec<Vec<(usize, &str)>> = vec![Vec::new()];
    for (number, line, kind) in lines(text) {
        let apart = kind != Kind::Prose || line.trim().is_empty();
        if apart {
            blocks.push(Vec::new());
        }
        if kind != Kind::Fenced {
            blocks
                .last_mut()
                .expect("one block at least")
                .push((number, line));
        }
        if apart {
            blocks.push(Vec::new());
        }
    }
    blocks.retain(|block| !block.is_empty());
    blocks
}

/// `line` without its indentation, quote marks, and list marker.
fn body(line: &str) -> &str {
    let mut rest = line.trim_start().trim_start_matches(['>', ' ']);
    for marker in ["- ", "* ", "+ "] {
        rest = rest.strip_prefix(marker).unwrap_or(rest);
    }
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    if digits > 0 {
        rest = rest[digits..].strip_prefix(". ").unwrap_or(rest);
    }
    rest.trim()
}

/// Whether a line cannot be wrapped: one word, one link, one command, or a
/// link's definition (`STANDARDS.md` §7).
fn unbreakable(line: &str) -> bool {
    let body = body(line);
    let closing = |rest: &str| rest.trim_matches(['.', ',', ';', ':']).is_empty();
    let one_link = || {
        let label = text::balanced(body, 0)?;
        let open = label.len() + 2;
        let target = text::balanced(body, open)?;
        body.as_bytes().get(open).filter(|byte| **byte == b'(')?;
        Some(closing(&body[open + target.len() + 2..]))
    };
    let one_command = body.starts_with('`')
        && body.matches('`').count() == 2
        && body.rfind('`').is_some_and(|at| closing(&body[at + 1..]));
    !body.contains(char::is_whitespace)
        || (body.starts_with('[') && body.contains("]: "))
        || one_link() == Some(true)
        || one_command
}

/// What a scripted edit leaves in prose: a capture group that was never
/// filled in.
fn leftover(prose: &str) -> bool {
    let bytes = prose.as_bytes();
    bytes
        .windows(2)
        .any(|pair| (pair[0] == b'$' || pair[0] == b'\\') && (b'1'..=b'9').contains(&pair[1]))
        || bytes
            .windows(3)
            .any(|three| three[0] == b'$' && three[1] == b'{' && three[2].is_ascii_digit())
}

/// The path a code span or a link target names, if it names one of the tree.
fn named_path<'a>(span: &'a str, file: &str, repo: &Repo) -> Option<String> {
    let odd = |ch: char| ch.is_whitespace() || "*<>{}$|\"'=…,".contains(ch);
    let span: &'a str = span.trim_end_matches(['.', ';', ':', ')']);
    if span.contains(odd) || span.contains("..") || span.contains("//") {
        return None;
    }
    let path = span.split(['#', ':']).next()?;
    if ROOTS.iter().any(|root| path.starts_with(root)) {
        return Some(path.to_owned());
    }
    let dir = Path::new(file).parent()?.to_str()?;
    let in_crate = path.starts_with("src/") || path.starts_with("tests/");
    (in_crate && repo.exists(&format!("{dir}/Cargo.toml"))).then(|| format!("{dir}/{path}"))
}

/// The targets of the inline links of `line` that point into the tree,
/// relative to the repository's root.
fn link_targets(line: &str, file: &str) -> Vec<String> {
    let dir = Path::new(file)
        .parent()
        .and_then(Path::to_str)
        .unwrap_or("");
    let mut targets = Vec::new();
    for (at, _) in line.match_indices("](") {
        let Some(target) = text::balanced(line, at + 1) else {
            continue;
        };
        let target = target.split(['#', ' ']).next().unwrap_or("");
        let remote = target.contains("://") || target.starts_with("mailto:");
        if target.is_empty() || remote || target.contains('<') {
            continue;
        }
        targets.push(if dir.is_empty() {
            target.to_owned()
        } else {
            format!("{dir}/{target}")
        });
    }
    targets
}

/// The recipe a command names, if the command runs `just`.
fn recipe(command: &str) -> Option<&str> {
    let rest = command
        .trim()
        .trim_start_matches("$ ")
        .strip_prefix("just ")?;
    let name = rest.split_whitespace().next()?;
    let plain = name
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    (plain && !name.starts_with('-')).then_some(name)
}

/// The findings in the recipes and paths one line names.
fn references(file: &str, number: usize, line: &str, kind: Kind, repo: &Repo) -> Vec<Finding> {
    let mut findings = Vec::new();
    let commands = if kind == Kind::Fenced {
        vec![line]
    } else {
        text::code_spans(line)
    };
    for command in &commands {
        if let Some(name) = recipe(command).filter(|name| !repo.recipes.contains(*name)) {
            let message = format!("the justfile has no recipe `{name}`");
            findings.push(Finding::new(Rule::Reference, file, number, message));
        }
    }
    if kind == Kind::Fenced {
        return findings;
    }
    let mut paths: Vec<String> = commands
        .iter()
        .filter_map(|span| named_path(span, file, repo))
        .collect();
    paths.extend(link_targets(line, file));
    for path in paths {
        if !repo.exists(&path) && !repo.ignores(&path) {
            let message = format!("`{path}` does not exist");
            findings.push(Finding::new(Rule::Reference, file, number, message));
        }
    }
    findings
}

/// Which code spans of `file` the rule `unknown-name` checks. The documents
/// of a crate and of the root name the tree's code, and their type names are
/// checked. The other documents, research and design among them, name the
/// APIs of other systems throughout, so only their paths and calls are.
fn shapes(file: &str) -> Shapes {
    let crate_document = file.strip_prefix("crates/").is_some_and(|rest| {
        rest.split_once('/')
            .is_some_and(|(_, name)| name == "README.md" || name == "AGENTS.md")
    });
    let root = ["AGENTS.md", "README.md", "STANDARDS.md", "fuzz/README.md"].contains(&file);
    if crate_document || root {
        Shapes::All
    } else {
        Shapes::PathsAndCalls
    }
}

/// The findings in the Markdown file `file`, whose contents are `text`.
pub fn check(file: &str, text: &str, repo: &Repo) -> Vec<Finding> {
    let mut findings = Vec::new();
    for (number, line, kind) in lines(text) {
        let mut find = |rule: Rule, message: String| {
            findings.push(Finding::new(rule, file, number, message));
        };
        let columns = line.chars().count();
        if kind == Kind::Prose && columns > WIDTH && !unbreakable(line) {
            find(
                Rule::ProseWidth,
                format!("{columns} columns; prose wraps at {WIDTH}"),
            );
        }
        if kind != Kind::Fenced {
            let prose = text::without_code_spans(line);
            if leftover(&prose) {
                find(Rule::Leftover, "an unfilled capture group".to_owned());
            }
        }
        findings.extend(references(file, number, line, kind, repo));
    }
    for block in blocks(text) {
        for (number, term) in prose::timeless(&block) {
            let message = format!("\"{term}\": say what the software does");
            findings.push(Finding::new(Rule::Timeless, file, number, message));
        }
        for (number, phrase) in prose::narration(&block) {
            let message = format!("\"{phrase}\": say what is, not how it came to be");
            findings.push(Finding::new(Rule::Narration, file, number, message));
        }
        for number in prose::camera_counts(&block) {
            let message = "a count of cameras describes a site";
            findings.push(Finding::new(Rule::DeviceCount, file, number, message));
        }
        findings.extend(prose::findings(file, &block));
        if let Some(names) = &repo.names {
            findings.extend(names::findings(file, &block, names, shapes(file)));
        }
    }
    findings.sort();
    findings
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn rules(text: &str) -> Vec<(usize, Rule)> {
        let repo = Repo::default();
        check("docs/a.md", text, &repo)
            .into_iter()
            .map(|finding| (finding.line, finding.rule))
            .collect()
    }

    #[test]
    fn prose_past_the_width_is_found_and_what_cannot_wrap_is_not() {
        let long = "word ".repeat(21);
        assert_eq!(rules(&long), [(1, Rule::ProseWidth)]);
        let url = format!("https://example.com/{}", "a".repeat(100));
        let exempt = [
            format!("| {long} |"),
            format!("## {long}"),
            format!("```\n{long}\n```"),
            format!("- {url}"),
            format!("[a label of words]({url})."),
            format!("[![badge]({url})]({url})"),
            format!("1. `cargo run -- {}`", "x ".repeat(50)),
            format!("[label]: {url} \"title\""),
        ];
        for text in exempt {
            assert_eq!(rules(&text), [], "{text}");
        }
        // A link with words after it can wrap.
        assert_eq!(
            rules(&format!("[a]({url}) and more words")),
            [(1, Rule::ProseWidth)]
        );
        // Columns are characters, not bytes.
        assert_eq!(rules(&"§".repeat(100)), []);
    }

    #[test]
    fn timeless_words_are_found_in_prose_and_not_in_code() {
        assert_eq!(
            rules("The new index is now built."),
            [(1, Rule::Timeless), (1, Rule::Timeless)]
        );
        assert_eq!(rules("| a | does not yet work |"), [(1, Rule::Timeless)]);
        assert_eq!(
            rules("Call `Foo::new` on renewal; the threshold holds."),
            []
        );
        assert_eq!(rules("```\nnew\n```"), []);
    }

    #[test]
    fn counts_of_cameras_are_found() {
        assert_eq!(rules("Six test cameras ran."), [(1, Rule::DeviceCount)]);
        assert_eq!(rules("with 14 cameras"), [(1, Rule::DeviceCount)]);
        assert_eq!(rules("three sessions per camera; a test camera"), []);
        assert_eq!(rules("cameras of one behaviour"), []);
    }

    #[test]
    fn unfilled_capture_groups_are_found() {
        assert_eq!(rules("the $1 fragment"), [(1, Rule::Leftover)]);
        assert_eq!(rules("the ${1}s and \\2"), [(1, Rule::Leftover)]);
        assert_eq!(rules("run `sed 's/a/$1/'` for $HOME"), []);
    }

    #[test]
    fn named_recipes_and_paths_must_exist() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("crates/a/src")).unwrap();
        fs::create_dir_all(root.join("docs")).unwrap();
        fs::write(root.join("crates/a/Cargo.toml"), "").unwrap();
        fs::write(root.join("crates/a/src/lib.rs"), "").unwrap();
        fs::write(root.join("docs/b.md"), "").unwrap();
        fs::write(root.join("justfile"), "check:\n").unwrap();
        let repo = Repo::open(root.to_owned());
        let found = |file: &str, text: &str| -> Vec<String> {
            let found = check(file, text, &repo);
            found.into_iter().map(|finding| finding.message).collect()
        };
        let none = Vec::<String>::new();
        assert_eq!(
            found(
                "docs/a.md",
                "Run `just check`, read `crates/a/src/lib.rs:12`."
            ),
            none
        );
        assert_eq!(
            found("docs/a.md", "See [b](b.md#part) and `crates/<crate>/x`."),
            Vec::<String>::new()
        );
        assert_eq!(
            found(
                "crates/a/README.md",
                "`src/lib.rs` and [b](../../docs/b.md)"
            ),
            none
        );
        assert_eq!(
            found("docs/a.md", "Run `just chek`."),
            ["the justfile has no recipe `chek`"]
        );
        assert_eq!(
            found("docs/a.md", "```\n$ just fmt --all\n```"),
            ["the justfile has no recipe `fmt`"]
        );
        assert_eq!(
            found("docs/a.md", "`crates/a/src/gone.rs`, [c](c.md)"),
            [
                "`crates/a/src/gone.rs` does not exist",
                "`docs/c.md` does not exist"
            ]
        );
        assert_eq!(
            found("crates/a/README.md", "`src/gone.rs`"),
            ["`crates/a/src/gone.rs` does not exist"]
        );
    }

    #[test]
    fn a_paragraph_ends_a_sentence() {
        let words = "word ".repeat(15);
        assert_eq!(rules(&format!("{words}\n{words}\n\n{words}\n{words}")), []);
    }

    #[test]
    fn a_code_block_holds_no_sentence() {
        assert_eq!(rules(&format!("```\n{}\n```", "word ".repeat(40))), []);
    }

    #[test]
    fn the_rules_of_words_read_markdown() {
        let words = "word ".repeat(18);
        let text = format!(
            "{words}\n{words}.\n\nIt reads as one.\n\nThe core checks it.\n\nThe `Feed`'s token."
        );
        assert_eq!(
            rules(&text),
            [
                (1, Rule::SentenceLength),
                (4, Rule::BarredPhrase),
                (6, Rule::TheCore),
                (8, Rule::PossessiveCodeSpan)
            ]
        );
    }

    /// The rules found in `text` as the file `file`, against the names of the
    /// Rust code `code`.
    fn named_rules(file: &str, text: &str, code: &str) -> Vec<(usize, Rule)> {
        let repo = Repo {
            names: Some(names::Names::of([("src/a.rs", code)], &[])),
            ..Repo::default()
        };
        check(file, text, &repo)
            .into_iter()
            .map(|finding| (finding.line, finding.rule))
            .collect()
    }

    #[test]
    fn a_document_of_a_crate_names_only_types_the_code_holds() {
        let found = named_rules("crates/a/README.md", "The `Gone` type.", "struct Held;");
        assert_eq!(found, [(1, Rule::UnknownName)]);
    }

    #[test]
    fn a_document_of_the_root_names_only_types_the_code_holds() {
        let found = named_rules("STANDARDS.md", "The `Gone` type.", "struct Held;");
        assert_eq!(found, [(1, Rule::UnknownName)]);
    }

    #[test]
    fn a_research_document_may_name_a_type_of_another_system() {
        let found = named_rules("docs/research/a.md", "The `Gone` type.", "struct Held;");
        assert_eq!(found, []);
    }

    #[test]
    fn a_research_document_names_only_paths_the_code_holds() {
        let found = named_rules("docs/research/a.md", "Call `Held::gone()`.", "struct Held;");
        assert_eq!(found, [(1, Rule::UnknownName)]);
    }

    #[test]
    fn a_name_in_a_code_block_is_not_checked() {
        assert_eq!(
            named_rules("README.md", "```\n`Gone`\n```", "struct Held;"),
            []
        );
    }
}
