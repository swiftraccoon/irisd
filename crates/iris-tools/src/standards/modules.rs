//! What the Rust files of a crate declare, for the rules that read a whole
//! crate. A scan of a file finds its items outside the bodies of functions
//! and types, the scope each is declared in, and its rustdoc. The tree of a
//! crate's modules says of each file whether a user of the crate can name
//! its module and whether only tests build it.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::path::Path;

use super::rust::{self, Class};
use super::text;

/// What an item is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A `struct`.
    Struct,
    /// An `enum`.
    Enum,
    /// A `union`.
    Union,
    /// A `trait`.
    Trait,
    /// A `type` alias, or a type of a trait.
    Type,
    /// A function or a method.
    Fn,
    /// A `macro_rules!` macro.
    Macro,
    /// A module, inline or in a file.
    Mod,
    /// An `impl` block.
    Impl,
    /// A `use` declaration.
    Use,
    /// A `const` or a `static`.
    Value,
}

/// Which builds hold an item, as its `cfg` attributes say. A later variant
/// holds fewer builds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Build {
    /// Every build, the one the documentation is made from among them.
    #[default]
    Every,
    /// Only a build with tests or a feature: a `cfg` that needs one of them,
    /// such as `cfg(any(test, feature = "x"))`.
    Gated,
    /// Only a build of tests: `cfg(test)`, `cfg(doctest)`, or a
    /// `cfg(all(...))` that holds `test`.
    Tests,
}

/// An item that a file declares outside the bodies of functions and types.
#[derive(Debug, Clone)]
pub struct Item {
    /// What it is.
    pub kind: Kind,
    /// Its name. An `impl` of a type takes the name of the type, and an
    /// `impl` of a trait has none.
    pub name: String,
    /// The line of its keyword, from 1.
    pub line: usize,
    /// The bytes it spans, from its first attribute to the end of its body
    /// or its semicolon.
    pub span: Range<usize>,
    /// Whether it is declared with a bare `pub`.
    pub public: bool,
    /// The builds that hold it.
    pub build: Build,
    /// Whether `#[doc(hidden)]` leaves it out of the documentation.
    pub hidden: bool,
    /// Whether it carries `#[macro_export]`.
    pub exported: bool,
    /// Its rustdoc: each `///` line before it, numbered, without its marker.
    pub docs: Vec<(usize, String)>,
    /// The scope it is declared in, an index into [`Scan::scopes`].
    pub scope: usize,
    /// The scope of its body, when it is a module, a trait, or an `impl`
    /// with one.
    pub body: Option<usize>,
    /// The identifiers that a `use` declaration gives.
    pub names: Vec<String>,
}

/// A scope that holds items: the file, or the body of a module, a trait, or
/// an `impl`.
#[derive(Debug, Clone)]
pub struct Scope {
    /// The item whose body it is; nothing for the file.
    pub item: Option<usize>,
    /// The scope it is in; nothing for the file.
    pub parent: Option<usize>,
    /// Its inner rustdoc: the `//!` lines before its first item.
    pub docs: Vec<(usize, String)>,
}

/// What one Rust file declares.
#[derive(Debug)]
pub struct Scan {
    /// Its items, in the order they start.
    pub items: Vec<Item>,
    /// Its scopes, the file first.
    pub scopes: Vec<Scope>,
    /// What each byte of the file belongs to.
    pub classes: Vec<Class>,
}

/// The modules of a scope, outermost first, and what they say of it.
#[derive(Debug)]
pub struct Chain {
    /// Whether each of the inline modules around the scope is a bare
    /// `pub mod`.
    pub public: bool,
    /// The fewest builds that an item around the scope is in.
    pub build: Build,
    /// The names of the inline modules around it, outermost first.
    pub names: Vec<String>,
    /// The item whose body the scope is, when it is not a module's.
    pub container: Option<usize>,
}

impl Scan {
    /// The chain of the scope `scope`.
    pub fn chain(&self, scope: usize) -> Chain {
        let mut chain = Chain {
            public: true,
            build: Build::Every,
            names: Vec::new(),
            container: None,
        };
        let mut at = Some(scope);
        let mut innermost = true;
        while let Some(index) = at {
            let scope = &self.scopes[index];
            if let Some(owner) = scope.item {
                let item = &self.items[owner];
                if item.kind == Kind::Mod {
                    chain.public &= item.public;
                    chain.names.push(item.name.clone());
                } else if innermost {
                    chain.container = Some(owner);
                }
                chain.build = chain.build.max(item.build);
            }
            innermost = false;
            at = scope.parent;
        }
        chain.names.reverse();
        chain
    }

    /// The spans of the items that only a build of tests holds. They hold
    /// every item of a test module.
    pub fn test_spans(&self) -> Vec<Range<usize>> {
        self.items
            .iter()
            .filter(|item| item.build == Build::Tests)
            .map(|item| item.span.clone())
            .collect()
    }
}

/// Whether `byte` can be part of an identifier.
fn in_identifier(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// The lines of `docs`, the doc comments of a file blanked to their class,
/// that start with `marker` and touch the bytes `range`: each with its
/// number and its text after the marker.
fn doc_lines(
    docs: &str,
    starts: &[usize],
    range: Range<usize>,
    marker: &str,
) -> Vec<(usize, String)> {
    let mut lines = Vec::new();
    let first = rust::line_at(starts, range.start);
    for (index, start) in starts.iter().enumerate().skip(first - 1) {
        if *start >= range.end {
            break;
        }
        let end = starts.get(index + 1).copied().unwrap_or(docs.len());
        let line = docs[(*start).max(range.start)..end.min(range.end)].trim();
        if let Some(rest) = line.strip_prefix(marker) {
            lines.push((index + 1, rest.trim().to_owned()));
        }
    }
    lines
}

/// The builds that hold an item under the attribute `inside`, written with
/// its white space removed. A `cfg` that refuses tests or a feature holds
/// the build the documentation is made from.
fn build_of(inside: &str) -> Build {
    let all_with_test = inside
        .strip_prefix("cfg(all(")
        .and_then(|rest| rest.strip_suffix("))"))
        .is_some_and(|list| text::top_level_items(list).contains(&"test"));
    if inside == "cfg(test)" || inside == "cfg(doctest)" || all_with_test {
        return Build::Tests;
    }
    let needs = ["test", "doctest", "feature"]
        .iter()
        .any(|word| text::has_word(inside, word));
    if inside.starts_with("cfg(") && !inside.starts_with("cfg(not(") && needs {
        Build::Gated
    } else {
        Build::Every
    }
}

/// The name of the type that the `impl` whose header goes on from `rest`
/// is for, or nothing when it is an `impl` of a trait.
fn impl_type(rest: &str) -> Option<String> {
    let mut rest = rest.trim_start();
    if rest.starts_with('<') {
        let generics = text::balanced(rest, 0)?;
        rest = rest[generics.len() + 2..].trim_start();
    }
    let bytes = rest.as_bytes();
    let mut depth = 0_i32;
    let mut cut = rest.len();
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'<' => depth += 1,
            b'>' if at == 0 || bytes[at - 1] != b'-' => depth -= 1,
            byte if in_identifier(byte) && depth == 0 => {
                let length = bytes[at..]
                    .iter()
                    .take_while(|b| in_identifier(**b))
                    .count();
                match &rest[at..at + length] {
                    "for" => return None,
                    "where" => {
                        cut = at;
                        break;
                    }
                    _ => {}
                }
                at += length;
                continue;
            }
            _ => {}
        }
        at += 1;
    }
    let path = rest[..cut].split('<').next()?.trim();
    let name = path
        .rsplit("::")
        .next()?
        .trim_start_matches(['&', '!'])
        .trim();
    Some(name.to_owned())
}

/// A place in the header of an item, which ends at `end`.
#[derive(Debug, Clone, Copy)]
struct Cursor<'a> {
    /// The file with its comments and literals blanked.
    code: &'a str,
    /// The offset of the place.
    at: usize,
    /// The offset at which the header ends.
    end: usize,
}

impl<'a> Cursor<'a> {
    /// The byte at the place, or nothing at the end of the header.
    fn byte(self) -> Option<u8> {
        self.code.as_bytes()[..self.end].get(self.at).copied()
    }

    /// The identifier that starts at the place, or nothing.
    fn word(self) -> &'a str {
        let from = self.at.min(self.end);
        let length = self.code.as_bytes()[from..self.end]
            .iter()
            .take_while(|byte| in_identifier(**byte))
            .count();
        &self.code[from..from + length]
    }

    /// The cursor `by` bytes on, past the white space there.
    fn after(self, by: usize) -> Self {
        let from = (self.at + by).min(self.end);
        let space = self.code.as_bytes()[from..self.end]
            .iter()
            .take_while(|byte| byte.is_ascii_whitespace())
            .count();
        Self {
            at: from + space,
            ..self
        }
    }

    /// The cursor past the bracket that opens at the place, and what the
    /// brackets hold.
    fn past_brackets(self) -> Option<(Self, &'a str)> {
        let inside = text::balanced(self.code, self.at)?;
        Some((self.after(inside.len() + 2), inside))
    }
}

/// What the header of an item says, before its body or its semicolon.
#[derive(Debug)]
struct Header {
    /// What the item is.
    kind: Kind,
    /// Its name, as [`Item::name`] holds it.
    name: String,
    /// The offset of its keyword.
    keyword: usize,
    /// The offset of its first attribute or word.
    first: usize,
    /// Whether it is declared with a bare `pub`.
    public: bool,
    /// Its attributes, without their brackets and white space.
    attributes: Vec<String>,
    /// The identifiers a `use` declaration gives.
    names: Vec<String>,
}

/// The attributes at `cursor`, each without its brackets and white space,
/// and the cursor past them.
fn attributes_at(mut cursor: Cursor<'_>) -> Option<(Cursor<'_>, Vec<String>)> {
    let mut attributes = Vec::new();
    while cursor.byte() == Some(b'#') {
        let inner = cursor.after(1);
        let open = if inner.byte() == Some(b'!') {
            cursor.after(2)
        } else {
            inner
        };
        if open.byte() != Some(b'[') {
            return None;
        }
        let (past, inside) = open.past_brackets()?;
        attributes.push(inside.split_whitespace().collect());
        cursor = past;
    }
    Some((cursor, attributes))
}

/// The visibility at `cursor`, as whether it is a bare `pub`, and the cursor
/// past it and past the qualifiers of a function.
fn visibility_at(mut cursor: Cursor<'_>) -> Option<(Cursor<'_>, bool)> {
    let mut public = false;
    if cursor.word() == "pub" {
        cursor = cursor.after(3);
        if cursor.byte() == Some(b'(') {
            cursor = cursor.past_brackets()?.0;
        } else {
            public = true;
        }
    }
    loop {
        let qualifier = cursor.word();
        let next = cursor.after(qualifier.len());
        match qualifier {
            "async" | "unsafe" | "default" | "safe" | "extern" => cursor = next,
            "const" if matches!(next.word(), "fn" | "async" | "unsafe" | "extern") => cursor = next,
            _ => break,
        }
    }
    Some((cursor, public))
}

/// The kind of the item whose keyword is at `cursor`, and the cursor at its
/// name.
fn kind_at(cursor: Cursor<'_>) -> Option<(Kind, Cursor<'_>)> {
    let leading = cursor.word();
    let rest = cursor.after(leading.len());
    let kind = match leading {
        "fn" => Kind::Fn,
        "struct" => Kind::Struct,
        "enum" => Kind::Enum,
        "union" => Kind::Union,
        "trait" => Kind::Trait,
        "type" => Kind::Type,
        "mod" => Kind::Mod,
        "impl" => Kind::Impl,
        "use" => Kind::Use,
        "const" | "static" => Kind::Value,
        "macro_rules" if rest.byte() == Some(b'!') => return Some((Kind::Macro, rest.after(1))),
        _ => return None,
    };
    if leading == "static" && rest.word() == "mut" {
        return Some((kind, rest.after(3)));
    }
    Some((kind, rest))
}

/// The header of the item that `code`, a file with its comments and
/// literals blanked, holds from `start` to `end`, if an item starts there.
fn header(code: &str, start: usize, end: usize) -> Option<Header> {
    let cursor = Cursor {
        code,
        at: start,
        end,
    }
    .after(0);
    let first = cursor.at;
    let (cursor, attributes) = attributes_at(cursor)?;
    if cursor.at >= end {
        return None;
    }
    let (cursor, public) = visibility_at(cursor)?;
    let keyword = cursor.at;
    let (kind, rest) = kind_at(cursor)?;
    let mut names = Vec::new();
    let name = match kind {
        Kind::Impl => impl_type(&code[rest.at..end]).unwrap_or_default(),
        Kind::Use => {
            let keywords = ["self", "super", "crate", "as"];
            names = code[rest.at..end]
                .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
                .filter(|name| !name.is_empty() && !keywords.contains(name))
                .map(str::to_owned)
                .collect();
            String::new()
        }
        _ => rest.word().to_owned(),
    };
    Some(Header {
        kind,
        name,
        keyword,
        first,
        public,
        attributes,
        names,
    })
}

/// A pair of braces of a scan: the scope it opens, when it holds items, and
/// the item whose body it is.
#[derive(Debug)]
struct Frame {
    /// The scope it opens.
    scope: Option<usize>,
    /// The item whose body it is.
    item: Option<usize>,
}

/// A scan of one file under way.
#[derive(Debug)]
struct Scanner<'a> {
    /// The file with its comments and literals blanked.
    code: &'a str,
    /// The file with all but its doc comments blanked.
    docs: &'a str,
    /// The offset at which each line starts.
    starts: &'a [usize],
    /// The items found.
    items: Vec<Item>,
    /// The scopes found, the file first.
    scopes: Vec<Scope>,
    /// Where each scope opens, at its brace or at the start of the file, and
    /// where its first item does.
    bounds: Vec<(usize, Option<usize>)>,
    /// The braces open at the place.
    frames: Vec<Frame>,
    /// Where the header of the next item starts.
    start: usize,
    /// How deep the place is in brackets of that header.
    depth: usize,
}

impl Scanner<'_> {
    /// The item whose header runs from `self.start` to `end`, in `scope`.
    fn item(&self, scope: usize, end: usize) -> Option<Item> {
        let found = header(self.code, self.start, end)?;
        let marked = |name: &str| found.attributes.iter().any(|inside| inside == name);
        Some(Item {
            kind: found.kind,
            line: rust::line_at(self.starts, found.keyword),
            span: found.first..end,
            public: found.public,
            build: found
                .attributes
                .iter()
                .map(|inside| build_of(inside))
                .max()
                .unwrap_or_default(),
            hidden: marked("doc(hidden)"),
            exported: marked("macro_export"),
            docs: doc_lines(self.docs, self.starts, self.start..found.keyword, "///"),
            scope,
            body: None,
            name: found.name,
            names: found.names,
        })
    }

    /// Opens the brace at `at` in `scope`, a scope that holds items: the body
    /// of the item whose header ends there, if one does.
    fn open(&mut self, scope: usize, at: usize) {
        let mut index = None;
        if self.depth == 0 {
            self.bounds[scope].1.get_or_insert(at);
            if let Some(item) = self.item(scope, at) {
                self.items.push(item);
                index = Some(self.items.len() - 1);
            }
        }
        let bears = index.is_some_and(|item| {
            matches!(self.items[item].kind, Kind::Mod | Kind::Trait | Kind::Impl)
        });
        let opened = bears.then(|| {
            self.scopes.push(Scope {
                item: index,
                parent: Some(scope),
                docs: Vec::new(),
            });
            self.bounds.push((at, None));
            self.scopes.len() - 1
        });
        if let (Some(item), Some(body)) = (index, opened) {
            self.items[item].body = Some(body);
        }
        self.frames.push(Frame {
            scope: opened,
            item: index,
        });
        self.start = at + 1;
        self.depth = 0;
    }

    /// Closes the brace at `at`, and the item whose body it ends.
    fn close(&mut self, at: usize) {
        if self.frames.len() > 1 {
            let frame = self.frames.pop().expect("a frame of braces");
            if let Some(item) = frame.item {
                self.items[item].span.end = at + 1;
            }
        }
        self.start = at + 1;
        self.depth = 0;
    }

    /// Reads the byte `byte` at `at`.
    fn step(&mut self, at: usize, byte: u8) {
        let frame = self.frames.last().expect("the frame of the file");
        let Some(scope) = frame.scope else {
            match byte {
                b'{' => self.frames.push(Frame {
                    scope: None,
                    item: None,
                }),
                b'}' => self.close(at),
                _ => {}
            }
            return;
        };
        match byte {
            b'(' | b'[' => self.depth += 1,
            b')' | b']' => self.depth = self.depth.saturating_sub(1),
            b';' if self.depth == 0 => {
                self.bounds[scope].1.get_or_insert(at);
                let item = self.item(scope, at + 1);
                self.items.extend(item);
                self.start = at + 1;
            }
            // The braces of a `use` tree are brackets of its header.
            b'{' if header(self.code, self.start, at)
                .is_some_and(|found| found.kind == Kind::Use) =>
            {
                self.depth += 1;
            }
            b'}' if self.depth > 0 => self.depth -= 1,
            b'{' => self.open(scope, at),
            b'}' => self.close(at),
            _ => {}
        }
    }
}

/// What the Rust file `source` declares.
pub fn scan(source: &str) -> Scan {
    let classes = rust::classify(source);
    let code = rust::only(source, &classes, Class::Code);
    let docs = rust::only(source, &classes, Class::Doc);
    let starts = rust::line_starts(source);
    let mut scanner = Scanner {
        code: &code,
        docs: &docs,
        starts: &starts,
        items: Vec::new(),
        scopes: vec![Scope {
            item: None,
            parent: None,
            docs: Vec::new(),
        }],
        bounds: vec![(0, None)],
        frames: vec![Frame {
            scope: Some(0),
            item: None,
        }],
        start: 0,
        depth: 0,
    };
    for (at, byte) in code.bytes().enumerate() {
        scanner.step(at, byte);
    }
    let Scanner {
        items,
        mut scopes,
        bounds,
        ..
    } = scanner;
    for (scope, (start, first)) in scopes.iter_mut().zip(bounds) {
        scope.docs = doc_lines(&docs, &starts, start..first.unwrap_or(code.len()), "//!");
    }
    Scan {
        items,
        scopes,
        classes,
    }
}

/// A Rust file of a crate, and where its module stands in the crate.
#[derive(Debug)]
pub struct Module<'a> {
    /// The file, relative to the repository's root.
    pub path: &'a str,
    /// What it declares.
    pub scan: Scan,
    /// The names of the modules from the crate root to it.
    pub names: Vec<String>,
    /// Whether the crate root of the library reaches it.
    pub library: bool,
    /// Whether a user of the library can name it: it is the root, or each
    /// module from the root to it is a bare `pub mod` in every build.
    pub public: bool,
    /// Whether only tests build it: its declaration, or one above it, is
    /// built for tests alone.
    pub test: bool,
    /// The rustdoc of its declaration.
    pub docs: Vec<(usize, String)>,
}

/// A file that a crate root reaches, before it is scanned.
#[derive(Debug)]
struct Reached {
    /// The file.
    path: String,
    /// The names of the modules from the root to it.
    names: Vec<String>,
    /// As [`Module::library`] says.
    library: bool,
    /// As [`Module::public`] says.
    public: bool,
    /// As [`Module::test`] says.
    test: bool,
    /// As [`Module::docs`] says.
    docs: Vec<(usize, String)>,
}

/// The files that the `mod` declarations of `scan`, the file that `reached`
/// names, reach among `files`. A root and a `mod.rs` hold their children in
/// their directory, and another file in the directory of its name.
fn children(
    reached: &Reached,
    root: bool,
    scan: &Scan,
    files: &BTreeMap<&str, &str>,
) -> Vec<Reached> {
    let file = Path::new(&reached.path);
    let stem = file
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("");
    let parent = file.parent().and_then(Path::to_str).unwrap_or("");
    let base = if root || stem == "mod" {
        parent.to_owned()
    } else {
        format!("{parent}/{stem}")
    };
    let mut found = Vec::new();
    for item in &scan.items {
        if item.kind != Kind::Mod || item.body.is_some() {
            continue;
        }
        let chain = scan.chain(item.scope);
        let mut directory = base.clone();
        for name in &chain.names {
            directory = format!("{directory}/{name}");
        }
        let candidates = [
            format!("{directory}/{}.rs", item.name),
            format!("{directory}/{}/mod.rs", item.name),
        ];
        let Some(path) = candidates
            .into_iter()
            .find(|candidate| files.contains_key(candidate.as_str()))
        else {
            continue;
        };
        let mut names = reached.names.clone();
        names.extend(chain.names.iter().cloned());
        names.push(item.name.clone());
        let build = chain.build.max(item.build);
        found.push(Reached {
            path,
            names,
            library: reached.library,
            public: reached.public && chain.public && item.public && build == Build::Every,
            test: reached.test || build == Build::Tests,
            docs: item.docs.clone(),
        });
    }
    found
}

/// The modules of the crate in `dir`, from `files`, its Rust files with
/// their texts. Those its roots reach come first, each where its module
/// stands. The roots are `src/lib.rs`, `src/main.rs`, and each file of
/// `src/bin/`. The other files under `src/` follow, neither public nor
/// tests. A file under `tests/` is no module of the crate.
pub fn crate_modules<'a>(dir: &str, files: &BTreeMap<&'a str, &'a str>) -> Vec<Module<'a>> {
    let src = format!("{dir}/src/");
    let mut roots = vec![format!("{src}lib.rs"), format!("{src}main.rs")];
    let bin = format!("{src}bin/");
    roots.extend(
        files
            .keys()
            .filter(|path| {
                path.strip_prefix(&bin)
                    .is_some_and(|rest| !rest.contains('/'))
            })
            .map(|path| (*path).to_owned()),
    );
    let mut queue: Vec<Reached> = roots
        .iter()
        .map(|root| Reached {
            library: root.ends_with("/lib.rs"),
            public: root.ends_with("/lib.rs"),
            path: root.clone(),
            names: Vec::new(),
            test: false,
            docs: Vec::new(),
        })
        .rev()
        .collect();
    let mut seen = BTreeSet::new();
    let mut modules = Vec::new();
    while let Some(reached) = queue.pop() {
        let Some((path, source)) = files.get_key_value(reached.path.as_str()) else {
            continue;
        };
        if !seen.insert(*path) {
            continue;
        }
        let scan = scan(source);
        let root = roots.contains(&reached.path);
        queue.extend(children(&reached, root, &scan, files).into_iter().rev());
        modules.push(Module {
            path,
            scan,
            names: reached.names,
            library: reached.library,
            public: reached.public,
            test: reached.test,
            docs: reached.docs,
        });
    }
    for (path, source) in files {
        if path.starts_with(&src) && !seen.contains(path) {
            modules.push(Module {
                path,
                scan: scan(source),
                names: Vec::new(),
                library: false,
                public: false,
                test: false,
                docs: Vec::new(),
            });
        }
    }
    modules
}

/// The crates of the workspace among `files`, each a path and its text:
/// every directory under `crates/` with a `Cargo.toml`, and the Rust files
/// under it.
pub fn workspace<'a>(files: &[(&'a str, &'a str)]) -> BTreeMap<String, BTreeMap<&'a str, &'a str>> {
    let mut crates: BTreeMap<String, BTreeMap<&'a str, &'a str>> = files
        .iter()
        .filter_map(|(path, _)| {
            let dir = path.strip_suffix("/Cargo.toml")?;
            let name = dir.strip_prefix("crates/")?;
            (!name.contains('/')).then(|| (dir.to_owned(), BTreeMap::new()))
        })
        .collect();
    for (path, text) in files {
        let Some(rest) = path.strip_prefix("crates/") else {
            continue;
        };
        let Some((name, _)) = rest.split_once('/') else {
            continue;
        };
        let source = Path::new(path)
            .extension()
            .is_some_and(|extension| extension == "rs");
        if let Some(files) = crates.get_mut(&format!("crates/{name}"))
            && source
        {
            files.insert(*path, *text);
        }
    }
    crates
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(source: &str) -> Vec<(Kind, String, bool)> {
        scan(source)
            .items
            .into_iter()
            .map(|item| (item.kind, item.name, item.public))
            .collect()
    }

    #[test]
    fn the_items_of_a_file_are_found() {
        let source = "pub struct A;\npub(crate) enum B { X }\npub fn c() {}\ntype D = u8;\n\
                      pub trait E {}\npub const F: u8 = 1;\npub mod g;\nuse h::I;";
        assert_eq!(
            items(source),
            [
                (Kind::Struct, "A".to_owned(), true),
                (Kind::Enum, "B".to_owned(), false),
                (Kind::Fn, "c".to_owned(), true),
                (Kind::Type, "D".to_owned(), false),
                (Kind::Trait, "E".to_owned(), true),
                (Kind::Value, "F".to_owned(), true),
                (Kind::Mod, "g".to_owned(), true),
                (Kind::Use, String::new(), false),
            ]
        );
    }

    #[test]
    fn an_item_inside_a_function_is_not_found() {
        assert_eq!(
            items("fn f() {\n    pub struct A;\n}"),
            [(Kind::Fn, "f".to_owned(), false)]
        );
    }

    #[test]
    fn a_field_or_a_variant_is_no_item() {
        let source = "pub struct A {\n    pub b: u8,\n}\npub enum C {\n    D { e: u8 },\n}";
        let found: Vec<String> = items(source).into_iter().map(|(_, name, _)| name).collect();
        assert_eq!(found, ["A", "C"]);
    }

    #[test]
    fn a_qualified_function_is_a_function() {
        let source = "pub const fn a() {}\npub async fn b() {}\npub unsafe extern \"C\" fn c() {}";
        let kinds: Vec<Kind> = items(source).into_iter().map(|(kind, ..)| kind).collect();
        assert_eq!(kinds, [Kind::Fn, Kind::Fn, Kind::Fn]);
    }

    #[test]
    fn a_semicolon_inside_brackets_ends_no_item() {
        let source = "impl A<[u8; 4]> {\n    pub fn b(c: [u8; 2]) {}\n}\npub struct C;";
        let found: Vec<String> = items(source).into_iter().map(|(_, name, _)| name).collect();
        assert_eq!(found, ["A", "b", "C"]);
    }

    #[test]
    fn an_impl_is_named_for_its_type() {
        let source = "impl<T> Feed<T> where T: Clone {}\nimpl fmt::Display for Feed {}";
        let found: Vec<String> = items(source).into_iter().map(|(_, name, _)| name).collect();
        assert_eq!(found, ["Feed", ""]);
    }

    #[test]
    fn the_methods_of_an_impl_are_in_its_scope() {
        let found = scan("impl A {\n    pub fn b() {}\n}");
        let method = &found.items[1];
        assert_eq!((method.kind, method.name.as_str()), (Kind::Fn, "b"));
        assert_eq!(found.chain(method.scope).container, Some(0));
    }

    #[test]
    fn a_macro_is_named_after_its_bang() {
        let found = scan("#[macro_export]\nmacro_rules! m {\n    () => {};\n}");
        assert_eq!(found.items.len(), 1);
        assert_eq!(
            (found.items[0].kind, found.items[0].name.as_str()),
            (Kind::Macro, "m")
        );
        assert!(found.items[0].exported);
    }

    #[test]
    fn the_rustdoc_of_an_item_is_its_outer_doc_lines() {
        let source = "//! The file.\n\n/// One.\n#[derive(Debug)]\n/// Two.\npub struct A;";
        let found = scan(source);
        let docs = &found.items[0].docs;
        assert_eq!(docs, &[(3, "One.".to_owned()), (5, "Two.".to_owned())]);
    }

    #[test]
    fn the_inner_rustdoc_of_a_file_comes_before_its_first_item() {
        let found = scan("//! The file.\n//! More.\n\nuse a::B;\n//! Not this.");
        let docs = &found.scopes[0].docs;
        assert_eq!(
            docs,
            &[(1, "The file.".to_owned()), (2, "More.".to_owned())]
        );
    }

    #[test]
    fn an_item_of_a_test_module_is_under_a_test() {
        let source = "pub fn a() {}\n#[cfg(test)]\nmod tests {\n    fn b() {}\n}\n";
        let found = scan(source);
        let inner = found.items.iter().find(|item| item.name == "b").unwrap();
        assert_eq!(found.chain(inner.scope).build, Build::Tests);
    }

    #[test]
    fn a_test_module_holds_its_items_in_its_span() {
        let source = "pub fn a() {}\n#[cfg(test)]\nmod tests {\n    fn b() {}\n}\n";
        let found = scan(source);
        let spans = found.test_spans();
        assert_eq!(spans.len(), 1);
        let inner = found.items.iter().find(|item| item.name == "b").unwrap();
        assert!(spans[0].contains(&inner.span.start));
    }

    #[test]
    fn an_item_behind_a_feature_has_no_test_span() {
        let found = scan("#[cfg(any(test, feature = \"x\"))]\npub fn a() {}");
        assert_eq!(found.test_spans(), []);
    }

    #[test]
    fn a_cfg_that_builds_for_tests_alone_is_found() {
        assert_eq!(build_of("cfg(test)"), Build::Tests);
        assert_eq!(build_of("cfg(doctest)"), Build::Tests);
        assert_eq!(build_of("cfg(all(test,unix))"), Build::Tests);
    }

    #[test]
    fn a_cfg_that_needs_a_feature_or_tests_is_gated() {
        assert_eq!(build_of("cfg(any(test,feature=))"), Build::Gated);
        assert_eq!(build_of("cfg(feature=)"), Build::Gated);
    }

    #[test]
    fn a_cfg_that_refuses_tests_holds_every_build() {
        assert_eq!(build_of("cfg(not(test))"), Build::Every);
        assert_eq!(build_of("cfg(unix)"), Build::Every);
    }

    #[test]
    fn the_fewest_builds_of_an_item_are_those_of_its_narrowest_cfg() {
        let found = scan("#[cfg(unix)]\n#[cfg(test)]\n#[cfg(feature = \"x\")]\npub fn a() {}");
        assert_eq!(found.items[0].build, Build::Tests);
    }

    #[test]
    fn a_hidden_item_is_marked() {
        assert!(scan("#[doc(hidden)]\npub fn a() {}").items[0].hidden);
    }

    #[test]
    fn a_use_gives_its_names() {
        let found = scan("pub use a::{B, c as D, self};");
        assert_eq!(found.items[0].names, ["a", "B", "c", "D"]);
    }

    /// The files of a crate at `crates/a`, each a path under it and a text.
    fn crate_of(files: &[(&'static str, &'static str)]) -> BTreeMap<&'static str, &'static str> {
        files.iter().copied().collect()
    }

    #[test]
    fn a_module_is_public_when_each_declaration_to_it_is_pub() {
        let files = crate_of(&[
            ("crates/a/src/lib.rs", "pub mod b;\nmod c;"),
            ("crates/a/src/b.rs", "pub mod d;"),
            ("crates/a/src/b/d.rs", ""),
            ("crates/a/src/c.rs", "pub mod e;"),
            ("crates/a/src/c/e.rs", ""),
        ]);
        let public: Vec<(&str, bool)> = crate_modules("crates/a", &files)
            .iter()
            .map(|module| (module.path, module.public))
            .collect();
        assert_eq!(
            public,
            [
                ("crates/a/src/lib.rs", true),
                ("crates/a/src/b.rs", true),
                ("crates/a/src/b/d.rs", true),
                ("crates/a/src/c.rs", false),
                ("crates/a/src/c/e.rs", false),
            ]
        );
    }

    #[test]
    fn a_module_behind_a_feature_is_not_public() {
        let files = crate_of(&[
            ("crates/a/src/lib.rs", "#[cfg(feature = \"x\")]\npub mod b;"),
            ("crates/a/src/b.rs", ""),
        ]);
        let modules = crate_modules("crates/a", &files);
        assert_eq!(modules[1].path, "crates/a/src/b.rs");
        assert!(!modules[1].public);
    }

    #[test]
    fn a_module_under_cfg_test_is_a_test() {
        let files = crate_of(&[
            ("crates/a/src/main.rs", "#[cfg(test)]\nmod tests;"),
            ("crates/a/src/tests.rs", "mod more;"),
            ("crates/a/src/tests/more.rs", ""),
        ]);
        let tests: Vec<(&str, bool)> = crate_modules("crates/a", &files)
            .iter()
            .map(|module| (module.path, module.test))
            .collect();
        assert_eq!(
            tests,
            [
                ("crates/a/src/main.rs", false),
                ("crates/a/src/tests.rs", true),
                ("crates/a/src/tests/more.rs", true),
            ]
        );
    }

    #[test]
    fn a_module_file_is_found_as_mod_rs() {
        let files = crate_of(&[
            ("crates/a/src/lib.rs", "pub mod b;"),
            ("crates/a/src/b/mod.rs", "pub mod c;"),
            ("crates/a/src/b/c.rs", ""),
        ]);
        let reached: Vec<(&str, bool)> = crate_modules("crates/a", &files)
            .iter()
            .map(|module| (module.path, module.library))
            .collect();
        assert_eq!(
            reached,
            [
                ("crates/a/src/lib.rs", true),
                ("crates/a/src/b/mod.rs", true),
                ("crates/a/src/b/c.rs", true),
            ]
        );
    }

    #[test]
    fn a_file_no_root_reaches_is_neither_public_nor_a_test() {
        let files = crate_of(&[("crates/a/src/lib.rs", ""), ("crates/a/src/b.rs", "")]);
        let modules = crate_modules("crates/a", &files);
        assert_eq!(modules[1].path, "crates/a/src/b.rs");
        assert!(!modules[1].public && !modules[1].test && !modules[1].library);
    }

    #[test]
    fn the_workspace_is_the_crates_with_a_manifest() {
        let files = [
            ("crates/a/Cargo.toml", ""),
            ("crates/a/src/lib.rs", ""),
            ("crates/a/README.md", ""),
            ("crates/b/src/lib.rs", ""),
            ("fuzz/Cargo.toml", ""),
        ];
        let crates = workspace(&files);
        let found: Vec<(&str, Vec<&str>)> = crates
            .iter()
            .map(|(dir, files)| (dir.as_str(), files.keys().copied().collect()))
            .collect();
        assert_eq!(found, [("crates/a", vec!["crates/a/src/lib.rs"])]);
    }

    #[test]
    fn an_item_of_a_private_inline_module_is_in_no_public_chain() {
        let found = scan("mod a {\n    pub fn b() {}\n}");
        assert!(!found.chain(found.items[1].scope).public);
    }

    #[test]
    fn an_impl_with_a_path_in_its_where_clause_is_named_for_its_type() {
        assert_eq!(impl_type("A where T: b::C "), Some("A".to_owned()));
    }

    #[test]
    fn a_for_inside_generic_arguments_makes_no_impl_of_a_trait() {
        assert_eq!(impl_type("A<for<'a> fn(&'a u8)> "), Some("A".to_owned()));
    }

    #[test]
    fn an_impl_of_a_trait_with_generic_arguments_has_no_name() {
        assert_eq!(impl_type("From<T> for B "), None);
    }

    #[test]
    fn an_arrow_inside_generic_arguments_closes_none() {
        assert_eq!(impl_type("From<fn() -> u8> for B "), None);
    }

    #[test]
    fn a_union_is_an_item() {
        assert_eq!(
            items("pub union A {\n    b: u8,\n}"),
            [(Kind::Union, "A".to_owned(), true)]
        );
    }

    #[test]
    fn the_word_macro_rules_without_its_bang_declares_no_macro() {
        assert_eq!(items("macro_rules m {}"), []);
    }

    #[test]
    fn a_mutable_static_is_named_after_its_mut() {
        assert_eq!(
            items("pub static mut A: u8 = 0;"),
            [(Kind::Value, "A".to_owned(), true)]
        );
    }

    #[test]
    fn a_static_keeps_its_whole_name() {
        assert_eq!(
            items("static ABCD: u8 = 0;"),
            [(Kind::Value, "ABCD".to_owned(), false)]
        );
    }

    #[test]
    fn a_closing_brace_with_no_opening_one_ends_nothing() {
        assert_eq!(items("}\nfn a() {}"), [(Kind::Fn, "a".to_owned(), false)]);
    }

    #[test]
    fn the_span_of_an_item_with_a_body_ends_after_its_brace() {
        assert_eq!(scan("mod a {}").items[0].span, 0..8);
    }

    #[test]
    fn the_span_of_an_item_without_a_body_ends_after_its_semicolon() {
        assert_eq!(scan("struct A;").items[0].span, 0..9);
    }

    #[test]
    fn an_item_after_a_block_inside_a_function_is_not_found() {
        assert_eq!(
            items("fn a() {\n    {}\n    struct B;\n}"),
            [(Kind::Fn, "a".to_owned(), false)]
        );
    }

    #[test]
    fn an_inline_module_reaches_no_file_of_its_name() {
        let files = crate_of(&[
            ("crates/a/src/lib.rs", "pub mod b {}"),
            ("crates/a/src/b.rs", ""),
        ]);
        let modules = crate_modules("crates/a", &files);
        assert!(!modules[1].library);
    }

    #[test]
    fn a_file_of_the_bin_directory_is_a_root() {
        let files = crate_of(&[
            ("crates/a/src/bin/b.rs", "#[cfg(test)]\nmod tests;"),
            ("crates/a/src/bin/tests.rs", ""),
        ]);
        let tests: Vec<(&str, bool)> = crate_modules("crates/a", &files)
            .iter()
            .map(|module| (module.path, module.test))
            .collect();
        assert_eq!(
            tests,
            [
                ("crates/a/src/bin/b.rs", false),
                ("crates/a/src/bin/tests.rs", true),
            ]
        );
    }
}
