//! The rule of examples: each item that a user of a library crate can name
//! has an example in its rustdoc, or a sentence that links the example that
//! shows it (`STANDARDS.md` §7).

use std::collections::BTreeSet;

use super::finding::{Finding, Rule};
use super::modules::{Build, Item, Kind, Module};
use super::{prose, text};

/// Whether `sentence` says "example" and links something: the word, or its
/// plural, in any letter case outside code spans, and a pair of brackets, as
/// an intra-doc or a Markdown link has.
fn links_an_example(sentence: &str) -> bool {
    let prose = text::without_code_spans(sentence);
    let lower = prose.to_lowercase();
    let word = text::has_word(&lower, "example") || text::has_word(&lower, "examples");
    let link = prose
        .find('[')
        .is_some_and(|open| prose[open + 1..].find(']').is_some_and(|close| close > 0));
    word && link
}

/// Whether the rustdoc `docs` holds an example: a fenced code block, or a
/// sentence that [`links_an_example`].
fn has_example(docs: &[(usize, String)]) -> bool {
    let fenced = |line: &str| line.starts_with("```") || line.starts_with("~~~");
    if docs.iter().any(|(_, line)| fenced(line)) {
        return true;
    }
    docs.split(|(_, line)| line.is_empty()).any(|block| {
        let lines: Vec<(usize, &str)> = block
            .iter()
            .map(|(number, line)| (*number, line.as_str()))
            .collect();
        prose::sentences(&lines)
            .iter()
            .any(|sentence| links_an_example(sentence))
    })
}

/// The names that the `pub use` declarations of the library give in the
/// build that the documentation is made from.
fn reexported(modules: &[&Module]) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for module in modules {
        for item in &module.scan.items {
            let build = item.build.max(module.scan.chain(item.scope).build);
            if item.kind == Kind::Use && item.public && build == Build::Every {
                names.extend(item.names.iter().cloned());
            }
        }
    }
    names
}

/// Whether a user of the crate can name `item` of `module` where it is
/// declared: the module and each inline module around the item are public,
/// or a `pub use` gives its name.
fn nameable(module: &Module, item: &Item, reexported: &BTreeSet<String>) -> bool {
    (module.public && module.scan.chain(item.scope).public) || reexported.contains(&item.name)
}

/// Whether `item` of `module` is left out of its documentation. It is when it
/// is hidden, or when a `cfg` on it or on an item around it needs tests or a
/// feature.
fn left_out(module: &Module, item: &Item) -> bool {
    let chain = module.scan.chain(item.scope);
    item.hidden || item.build.max(chain.build) != Build::Every
}

/// The names of the types and traits of the library that a user can name.
fn nameable_types(modules: &[&Module], reexported: &BTreeSet<String>) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for module in modules {
        for item in &module.scan.items {
            let typed = matches!(
                item.kind,
                Kind::Struct | Kind::Enum | Kind::Union | Kind::Type | Kind::Trait
            );
            let in_module = module.scan.chain(item.scope).container.is_none();
            if typed
                && in_module
                && item.public
                && !left_out(module, item)
                && nameable(module, item, reexported)
            {
                names.insert(item.name.clone());
            }
        }
    }
    names
}

/// How a finding names `item`, or nothing when the rule does not cover it.
/// The rule covers a type, a trait, a type alias, and a function declared
/// with a bare `pub` in a module, and a `#[macro_export]` macro. It covers a
/// method with a bare `pub` in an `impl` of a type a user can name, and each
/// function of a public trait a user can name.
fn covered(
    module: &Module,
    item: &Item,
    reexported: &BTreeSet<String>,
    types: &BTreeSet<String>,
) -> Option<String> {
    if left_out(module, item) {
        return None;
    }
    let chain = module.scan.chain(item.scope);
    let Some(owner) = chain.container.map(|owner| &module.scan.items[owner]) else {
        let what = match item.kind {
            Kind::Macro if item.exported => return Some(format!("the macro `{}!`", item.name)),
            Kind::Struct => "struct",
            Kind::Enum => "enum",
            Kind::Trait => "trait",
            Kind::Type => "type",
            Kind::Fn => "fn",
            _ => return None,
        };
        let named = item.public && nameable(module, item, reexported);
        return named.then(|| format!("`{what} {}`", item.name));
    };
    let method = match owner.kind {
        Kind::Impl => item.public && !owner.name.is_empty() && types.contains(&owner.name),
        Kind::Trait => {
            owner.public && !left_out(module, owner) && nameable(module, owner, reexported)
        }
        _ => false,
    };
    (item.kind == Kind::Fn && method).then(|| format!("`{}::{}`", owner.name, item.name))
}

/// The message of a finding on what `what` names.
fn message(what: &str) -> String {
    format!(
        "{what} has no example: write one in a code block, or a sentence that says \"example\" \
         and links the example that shows it"
    )
}

/// The findings of `missing-example` in the modules of a crate, as
/// [`crate_modules`](super::modules::crate_modules) gives them. Only the
/// modules that the root of a library reaches are read, and of those not the
/// ones built for tests alone. A public module needs an example in its `//!`
/// docs or the `///` docs of its declaration, and each item [`covered`] needs
/// one in its rustdoc. An item that `#[doc(hidden)]` leaves out of the
/// documentation needs none, and neither does a `const` or a `static`.
pub fn findings(modules: &[Module]) -> Vec<Finding> {
    let library: Vec<&Module> = modules
        .iter()
        .filter(|module| module.library && !module.test)
        .collect();
    let reexported = reexported(&library);
    let types = nameable_types(&library, &reexported);
    let mut found = Vec::new();
    for module in &library {
        let scan = &module.scan;
        if module.public {
            let docs: Vec<(usize, String)> = module
                .docs
                .iter()
                .chain(&scan.scopes[0].docs)
                .cloned()
                .collect();
            if !has_example(&docs) {
                let line = scan.scopes[0].docs.first().map_or(1, |(line, _)| *line);
                let name = if module.names.is_empty() {
                    "the crate".to_owned()
                } else {
                    format!("the module `{}`", module.names.join("::"))
                };
                let finding = Finding::new(Rule::MissingExample, module.path, line, message(&name));
                found.push(finding);
            }
        }
        for item in &scan.items {
            let inline = item.kind == Kind::Mod && item.public && item.body.is_some();
            if inline && !left_out(module, item) && nameable(module, item, &BTreeSet::new()) {
                let body = item.body.expect("the body of an inline module");
                let docs: Vec<(usize, String)> = item
                    .docs
                    .iter()
                    .chain(&scan.scopes[body].docs)
                    .cloned()
                    .collect();
                if !has_example(&docs) {
                    let what = format!("the module `{}`", item.name);
                    found.push(Finding::new(
                        Rule::MissingExample,
                        module.path,
                        item.line,
                        message(&what),
                    ));
                }
                continue;
            }
            let Some(what) = covered(module, item, &reexported, &types) else {
                continue;
            };
            if !has_example(&item.docs) {
                found.push(Finding::new(
                    Rule::MissingExample,
                    module.path,
                    item.line,
                    message(&what),
                ));
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::super::modules;
    use super::*;

    /// The findings in a crate at `crates/a` of `files`, each a path under
    /// `crates/a/src/` and a text.
    fn found(files: &[(&str, &str)]) -> Vec<(String, usize, String)> {
        let paths: Vec<(String, &str)> = files
            .iter()
            .map(|(path, text)| (format!("crates/a/src/{path}"), *text))
            .collect();
        let files: BTreeMap<&str, &str> = paths
            .iter()
            .map(|(path, text)| (path.as_str(), *text))
            .collect();
        let modules = modules::crate_modules("crates/a", &files);
        findings(&modules)
            .into_iter()
            .map(|finding| {
                let path = finding.path.trim_start_matches("crates/a/src/").to_owned();
                (path, finding.line, finding.message)
            })
            .collect()
    }

    /// The names that the findings in `files` give, as their messages start.
    fn named(files: &[(&str, &str)]) -> Vec<String> {
        found(files)
            .into_iter()
            .map(|(_, _, message)| {
                message
                    .split(" has no example")
                    .next()
                    .unwrap_or("")
                    .to_owned()
            })
            .collect()
    }

    /// Crate documentation with an example.
    const ROOT: &str = "//! A crate.\n//!\n//! ```\n//! ```\n";

    #[test]
    fn a_public_function_without_an_example_is_found() {
        let lib = format!("{ROOT}\n/// Does it.\npub fn act() {{}}\n");
        assert_eq!(
            found(&[("lib.rs", &lib)]),
            [(
                "lib.rs".to_owned(),
                7,
                "`fn act` has no example: write one in a code block, or a sentence that says \
                 \"example\" and links the example that shows it"
                    .to_owned()
            )]
        );
    }

    #[test]
    fn an_item_with_a_code_block_passes() {
        let lib = format!(
            "{ROOT}\n/// Does it.\n///\n/// ```\n/// a::act();\n/// ```\npub fn act() {{}}"
        );
        assert_eq!(named(&[("lib.rs", &lib)]), [""; 0]);
    }

    #[test]
    fn an_item_with_a_sentence_that_links_an_example_passes() {
        let lib = format!(
            "{ROOT}\n/// Does it. The example of the\n/// [module](self) acts twice.\n\
             pub fn act() {{}}\n/// Does it. The EXAMPLES on [`act`] show it.\n\
             pub fn again() {{}}"
        );
        assert_eq!(named(&[("lib.rs", &lib)]), [""; 0]);
    }

    #[test]
    fn a_sentence_with_the_word_but_no_link_is_found() {
        let lib = format!("{ROOT}\n/// The example of the module acts.\npub fn act() {{}}");
        assert_eq!(named(&[("lib.rs", &lib)]), ["`fn act`"]);
    }

    #[test]
    fn a_link_in_one_sentence_with_the_word_in_another_is_found() {
        let lib = format!("{ROOT}\n/// See [`b`]. It is an example.\npub fn act() {{}}");
        assert_eq!(named(&[("lib.rs", &lib)]), ["`fn act`"]);
    }

    #[test]
    fn the_word_in_a_code_span_is_no_word() {
        let lib = format!("{ROOT}\n/// See [`example`].\npub fn act() {{}}");
        assert_eq!(named(&[("lib.rs", &lib)]), ["`fn act`"]);
    }

    #[test]
    fn each_kind_of_public_item_is_found() {
        let lib = format!(
            "{ROOT}\npub struct A;\npub enum B {{}}\npub trait C {{}}\npub type D = u8;\n\
             #[macro_export]\nmacro_rules! e {{\n    () => {{}};\n}}"
        );
        assert_eq!(
            named(&[("lib.rs", &lib)]),
            [
                "`struct A`",
                "`enum B`",
                "`trait C`",
                "`type D`",
                "the macro `e!`"
            ]
        );
    }

    #[test]
    fn a_const_a_static_or_a_union_is_not_covered() {
        let lib =
            format!("{ROOT}\npub const A: u8 = 1;\npub static B: u8 = 1;\npub union C {{ a: u8 }}");
        assert_eq!(named(&[("lib.rs", &lib)]), [""; 0]);
    }

    #[test]
    fn a_method_of_a_public_type_is_found() {
        let lib =
            format!("{ROOT}\n/// ```\n/// ```\npub struct A;\nimpl A {{\n    pub fn b() {{}}\n}}");
        assert_eq!(named(&[("lib.rs", &lib)]), ["`A::b`"]);
    }

    #[test]
    fn a_method_of_a_trait_impl_is_not_covered() {
        let lib = format!(
            "{ROOT}\n/// ```\n/// ```\npub struct A;\n\
             impl Clone for A {{\n    fn clone(&self) -> Self {{ A }}\n}}"
        );
        assert_eq!(named(&[("lib.rs", &lib)]), [""; 0]);
    }

    #[test]
    fn a_method_of_a_crate_type_is_not_covered() {
        let lib = format!("{ROOT}\npub(crate) struct A;\nimpl A {{\n    pub fn b() {{}}\n}}");
        assert_eq!(named(&[("lib.rs", &lib)]), [""; 0]);
    }

    #[test]
    fn a_function_of_a_public_trait_is_found() {
        let lib = format!("{ROOT}\n/// ```\n/// ```\npub trait A {{\n    fn b(&self);\n}}");
        assert_eq!(named(&[("lib.rs", &lib)]), ["`A::b`"]);
    }

    #[test]
    fn an_item_of_restricted_visibility_is_not_covered() {
        let lib = format!("{ROOT}\npub(crate) fn a() {{}}\npub(super) fn b() {{}}\nfn c() {{}}");
        assert_eq!(named(&[("lib.rs", &lib)]), [""; 0]);
    }

    #[test]
    fn an_item_of_a_private_module_no_pub_use_names_is_skipped() {
        let lib = format!("{ROOT}\nmod b;\n");
        assert_eq!(
            named(&[("lib.rs", &lib), ("b.rs", "pub fn hidden() {}")]),
            [""; 0]
        );
    }

    #[test]
    fn an_item_of_a_private_module_that_a_pub_use_names_is_found() {
        let lib = format!("{ROOT}\nmod b;\n\npub use b::shown;\n");
        let files = [
            ("lib.rs", lib.as_str()),
            ("b.rs", "pub fn shown() {}\npub fn kept() {}"),
        ];
        assert_eq!(
            found(&files),
            [("b.rs".to_owned(), 1, message("`fn shown`"))]
        );
    }

    #[test]
    fn a_public_module_without_an_example_is_found() {
        let lib = format!("{ROOT}\npub mod b;\n");
        let files = [("lib.rs", lib.as_str()), ("b.rs", "//! The module.\n")];
        assert_eq!(
            found(&files),
            [("b.rs".to_owned(), 1, message("the module `b`"))]
        );
    }

    #[test]
    fn a_module_whose_declaration_links_an_example_passes() {
        let lib = format!("{ROOT}\n/// The example of [`b`] shows it.\npub mod b;\n");
        assert_eq!(
            named(&[("lib.rs", &lib), ("b.rs", "//! The module.\n")]),
            [""; 0]
        );
    }

    #[test]
    fn a_crate_without_an_example_is_found() {
        assert_eq!(named(&[("lib.rs", "//! A crate.\n")]), ["the crate"]);
    }

    #[test]
    fn a_private_module_needs_no_example() {
        let lib = format!("{ROOT}\nmod b;\n");
        assert_eq!(
            named(&[("lib.rs", &lib), ("b.rs", "//! The module.\n")]),
            [""; 0]
        );
    }

    #[test]
    fn an_inline_public_module_without_an_example_is_found() {
        let lib = format!("{ROOT}\n/// A module.\npub mod b {{}}\n");
        assert_eq!(named(&[("lib.rs", &lib)]), ["the module `b`"]);
    }

    #[test]
    fn an_item_of_a_test_module_is_skipped() {
        let lib = format!(
            "{ROOT}\n#[cfg(test)]\nmod tests {{\n    pub fn a() {{}}\n}}\n#[cfg(test)]\npub mod b;"
        );
        assert_eq!(
            named(&[("lib.rs", &lib), ("b.rs", "pub fn c() {}")]),
            [""; 0]
        );
    }

    #[test]
    fn an_item_built_for_tests_alone_is_skipped() {
        let lib = format!("{ROOT}\n#[cfg(test)]\npub fn a() {{}}");
        assert_eq!(named(&[("lib.rs", &lib)]), [""; 0]);
    }

    #[test]
    fn an_item_behind_a_feature_is_skipped() {
        let lib = format!(
            "{ROOT}\n#[cfg(any(test, feature = \"support\"))]\npub fn a() {{}}\n\
             #[cfg(feature = \"support\")]\npub mod b;\n"
        );
        assert_eq!(
            named(&[("lib.rs", &lib), ("b.rs", "pub fn c() {}")]),
            [""; 0]
        );
    }

    #[test]
    fn an_item_of_a_module_behind_a_feature_is_skipped() {
        let lib = format!("{ROOT}\n#[cfg(feature = \"x\")]\npub mod b {{\n    pub fn c() {{}}\n}}");
        assert_eq!(named(&[("lib.rs", &lib)]), [""; 0]);
    }

    #[test]
    fn a_pub_use_behind_a_feature_names_nothing() {
        let lib = format!("{ROOT}\nmod a;\n#[cfg(feature = \"x\")]\npub use a::act;\n");
        assert_eq!(
            named(&[("lib.rs", &lib), ("a.rs", "pub fn act() {}")]),
            [""; 0]
        );
    }

    #[test]
    fn an_item_behind_a_cfg_that_refuses_tests_is_found() {
        let lib =
            format!("{ROOT}\n#[cfg(not(test))]\npub fn a() {{}}\n#[cfg(unix)]\npub fn b() {{}}");
        assert_eq!(named(&[("lib.rs", &lib)]), ["`fn a`", "`fn b`"]);
    }

    #[test]
    fn a_hidden_item_is_skipped() {
        let lib = format!("{ROOT}\n#[doc(hidden)]\npub fn a() {{}}");
        assert_eq!(named(&[("lib.rs", &lib)]), [""; 0]);
    }

    #[test]
    fn a_crate_with_only_a_main_is_skipped() {
        assert_eq!(named(&[("main.rs", "//! A tool.\npub fn a() {}")]), [""; 0]);
    }

    #[test]
    fn a_pub_use_of_a_binary_names_nothing_of_a_library() {
        let main = "//! A tool.\nmod a;\npub use a::act;\n";
        assert_eq!(
            named(&[("main.rs", main), ("a.rs", "pub fn act() {}")]),
            [""; 0]
        );
    }

    #[test]
    fn an_item_of_a_test_module_file_is_skipped_though_a_pub_use_gives_its_name() {
        let lib = format!("{ROOT}\nmod a;\npub use a::act;\n#[cfg(test)]\nmod tests;\n");
        let files = [
            ("lib.rs", lib.as_str()),
            ("a.rs", "/// ```\n/// ```\npub fn act() {}"),
            ("tests.rs", "pub fn act() {}"),
        ];
        assert_eq!(named(&files), [""; 0]);
    }

    #[test]
    fn empty_brackets_link_no_example() {
        assert!(!links_an_example("The example of [] shows it."));
    }

    #[test]
    fn a_macro_that_is_not_exported_needs_no_example() {
        let lib = format!("{ROOT}\nmacro_rules! m {{\n    () => {{}};\n}}\n");
        assert_eq!(named(&[("lib.rs", &lib)]), [""; 0]);
    }

    #[test]
    fn a_function_of_a_private_trait_needs_no_example() {
        let lib = format!("{ROOT}\ntrait A {{\n    fn b();\n}}\n");
        assert_eq!(named(&[("lib.rs", &lib)]), [""; 0]);
    }
}
