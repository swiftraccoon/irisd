//! One breach of a rule, and the rules there are.

use std::fmt;

/// A rule the checks enforce, named as `standards.toml` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Rule {
    /// Markdown prose wraps at 100 columns (`STANDARDS.md` §7).
    ProseWidth,
    /// Prose is timeless (`STANDARDS.md` §7).
    Timeless,
    /// A text says what is, not how it came to be (`STANDARDS.md` §9,
    /// `AGENTS.md` rule 9).
    Narration,
    /// What a scripted edit or a merge leaves behind.
    Leftover,
    /// Committed files describe no installation (`AGENTS.md` rule 9).
    Installation,
    /// A count of cameras describes an installation (`AGENTS.md` rule 9).
    DeviceCount,
    /// One `use` per module, no `::` inside braces (`STANDARDS.md` §2).
    UseBraces,
    /// Lowercase hex literals (`STANDARDS.md` §2).
    HexCase,
    /// Lint exceptions are `#[expect(lint, reason = "…")]` (`STANDARDS.md` §1).
    LintException,
    /// `TODO` carries an owner or an issue (`STANDARDS.md` §7).
    Todo,
    /// Getters have no `get_` prefix (`STANDARDS.md` §3).
    GetPrefix,
    /// Enums over `bool` parameters (`STANDARDS.md` §4).
    BoolParameter,
    /// No bare integers for units in a public signature (`STANDARDS.md` §5).
    BareTuple,
    /// Every crate has its documents (`AGENTS.md` rule 10).
    CrateDocs,
    /// Every fuzz target is registered and described (`AGENTS.md` rule 8).
    FuzzTarget,
    /// A document names only recipes and paths that exist.
    Reference,
    /// Every tracked file is a regular file that is not executable: the API
    /// that `gh-commit` publishes through preserves no other mode.
    FileMode,
    /// The subject line of a commit (`STANDARDS.md` §9).
    Subject,
    /// The body of a commit (`STANDARDS.md` §9).
    Body,
    /// No attribution lines in a commit.
    Attribution,
    /// A commit is one coherent change (`STANDARDS.md` §9).
    Scope,
    /// A feature changes a document in the same commit (`AGENTS.md` rules 4, 10, and 11).
    Research,
    /// A dependency arrives with its reason and license (`AGENTS.md`, definition of done).
    Dependency,
}

impl Rule {
    /// Every rule, in the order a report lists them.
    pub const ALL: [Self; 23] = [
        Self::ProseWidth,
        Self::Timeless,
        Self::Narration,
        Self::Leftover,
        Self::Installation,
        Self::DeviceCount,
        Self::UseBraces,
        Self::HexCase,
        Self::LintException,
        Self::Todo,
        Self::GetPrefix,
        Self::BoolParameter,
        Self::BareTuple,
        Self::CrateDocs,
        Self::FuzzTarget,
        Self::Reference,
        Self::FileMode,
        Self::Subject,
        Self::Body,
        Self::Attribution,
        Self::Scope,
        Self::Research,
        Self::Dependency,
    ];

    /// The rule's name in reports and in `standards.toml`.
    pub fn name(self) -> &'static str {
        match self {
            Self::ProseWidth => "prose-width",
            Self::Timeless => "timeless",
            Self::Narration => "narration",
            Self::Leftover => "leftover",
            Self::Installation => "installation",
            Self::DeviceCount => "device-count",
            Self::UseBraces => "use-braces",
            Self::HexCase => "hex-case",
            Self::LintException => "lint-exception",
            Self::Todo => "todo",
            Self::GetPrefix => "get-prefix",
            Self::BoolParameter => "bool-parameter",
            Self::BareTuple => "bare-tuple",
            Self::CrateDocs => "crate-docs",
            Self::FuzzTarget => "fuzz-target",
            Self::Reference => "reference",
            Self::FileMode => "file-mode",
            Self::Subject => "subject",
            Self::Body => "body",
            Self::Attribution => "attribution",
            Self::Scope => "scope",
            Self::Research => "research",
            Self::Dependency => "dependency",
        }
    }

    /// The rule called `name`.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|rule| rule.name() == name)
    }
}

/// One breach: where it is and what it is.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Finding {
    /// The file, relative to the repository's root, or `commit <hash>`.
    pub path: String,
    /// The line, from 1; 0 when the breach is of the whole file.
    pub line: usize,
    /// The rule.
    pub rule: Rule,
    /// What is wrong, in a sentence.
    pub message: String,
}

impl Finding {
    /// A finding of `rule` at `line` of `path`.
    pub fn new(rule: Rule, path: &str, line: usize, message: impl Into<String>) -> Self {
        Self {
            path: path.to_owned(),
            line,
            rule,
            message: message.into(),
        }
    }
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            path,
            line,
            rule,
            message,
        } = self;
        if *line == 0 {
            write!(f, "{path}: [{}] {message}", rule.name())
        } else {
            write!(f, "{path}:{line}: [{}] {message}", rule.name())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_rule_has_its_own_name() {
        for rule in Rule::ALL {
            assert_eq!(Rule::from_name(rule.name()), Some(rule));
        }
        assert_eq!(Rule::from_name("taste"), None);
    }

    #[test]
    fn a_finding_prints_as_a_compiler_message() {
        let finding = Finding::new(Rule::HexCase, "src/a.rs", 3, "0xFF");
        assert_eq!(finding.to_string(), "src/a.rs:3: [hex-case] 0xFF");
        let whole = Finding::new(Rule::CrateDocs, "crates/x", 0, "no README.md");
        assert_eq!(whole.to_string(), "crates/x: [crate-docs] no README.md");
    }
}
