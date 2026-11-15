//! The rules for a commit (`STANDARDS.md` §9): the form of its message, and
//! what the message must say of the change it describes.

use std::collections::BTreeSet;
use std::path::Path;

use super::finding::{Finding, Rule};
use super::installation::{Own, Site};
use super::prose;

/// The types of Conventional Commits that `STANDARDS.md` §9 lists.
const TYPES: [&str; 11] = [
    "feat", "fix", "docs", "refactor", "perf", "test", "build", "ci", "style", "chore", "revert",
];

/// The scopes that are areas and not crates.
const AREAS: [&str; 3] = ["docs", "ci", "soak"];

/// The types whose change is to one crate's code, and so carry its scope.
const CODE_TYPES: [&str; 5] = ["feat", "fix", "perf", "refactor", "test"];

/// The longest subject line: under 72 characters.
const SUBJECT_MAX: usize = 71;

/// First words that describe instead of command.
const NOT_IMPERATIVE: [&str; 24] = [
    "added",
    "adds",
    "adding",
    "fixed",
    "fixes",
    "fixing",
    "updated",
    "updates",
    "updating",
    "removed",
    "removes",
    "removing",
    "changed",
    "changes",
    "changing",
    "implemented",
    "implements",
    "refactored",
    "refactors",
    "moved",
    "moves",
    "renamed",
    "renames",
    "made",
];

/// What a commit changes.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Change {
    /// The paths it touches, relative to the repository's root.
    pub paths: Vec<String>,
    /// The packages it adds to a lockfile.
    pub added_packages: Vec<String>,
}

/// The message of the commit being made: `raw` without git's comment lines
/// and without what follows its scissors line.
pub fn cleaned(raw: &str) -> String {
    let mut kept = Vec::new();
    for line in raw.lines() {
        if line.starts_with("# ---") && line.contains(">8") {
            break;
        }
        if !line.starts_with('#') {
            kept.push(line);
        }
    }
    kept.join("\n").trim().to_owned()
}

/// The parts of a subject line.
#[derive(Debug, PartialEq, Eq)]
struct Subject<'a> {
    /// The type.
    kind: &'a str,
    /// The scope, when the line has one.
    scope: Option<&'a str>,
    /// What follows the colon and its space.
    description: &'a str,
}

/// The parts of `line`, if it has the form `type(scope)!: description`.
fn parse_subject(line: &str) -> Option<Subject<'_>> {
    let (head, description) = line.split_once(": ")?;
    let head = head.strip_suffix('!').unwrap_or(head);
    let (kind, scope) = match head.split_once('(') {
        Some((kind, rest)) => (kind, Some(rest.strip_suffix(')')?)),
        None => (head, None),
    };
    let plain = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_alphanumeric());
    (plain(kind) && scope.is_none_or(plain) && !description.trim().is_empty()).then_some(Subject {
        kind,
        scope,
        description,
    })
}

/// The scope a crate's directory takes: its name without the `iris-` prefix.
fn scope_of(crate_dir: &str) -> &str {
    crate_dir.strip_prefix("iris-").unwrap_or(crate_dir)
}

/// The directories under `crates/` whose code `paths` touch.
fn touched_crates(paths: &[String]) -> BTreeSet<&str> {
    paths
        .iter()
        .filter_map(|path| {
            let mut parts = path.strip_prefix("crates/")?.splitn(3, '/');
            let (name, part) = (parts.next()?, parts.next()?);
            ["src", "tests", "benches", "examples"]
                .contains(&part)
                .then_some(name)
        })
        .collect()
}

/// The findings in the form of a message.
fn form(subject_line: &str, body: &[&str], crates: &[String]) -> Vec<(usize, Rule, String)> {
    let mut found = Vec::new();
    let mut subject = |message: String| found.push((1, Rule::Subject, message));
    match parse_subject(subject_line) {
        None => subject("not of the form `type(scope): description`".to_owned()),
        Some(Subject {
            kind,
            scope,
            description,
        }) => {
            if !TYPES.contains(&kind) {
                subject(format!("`{kind}` is not a type of STANDARDS.md §9"));
            }
            let known = |scope: &str| {
                AREAS.contains(&scope) || crates.iter().any(|name| scope_of(name) == scope)
            };
            if let Some(scope) = scope.filter(|scope| !known(scope)) {
                subject(format!(
                    "`{scope}` is neither a crate nor an area of STANDARDS.md §9"
                ));
            }
            if description.ends_with('.') {
                subject("the description ends with a period".to_owned());
            }
            let first = description.split_whitespace().next().unwrap_or("");
            if NOT_IMPERATIVE.contains(&first.to_lowercase().as_str()) {
                subject(format!("`{first}`: the description is imperative"));
            }
        }
    }
    let length = subject_line.chars().count();
    if length > SUBJECT_MAX {
        subject(format!("{length} characters; a subject is under 72"));
    }
    if body.first().is_some_and(|line| !line.trim().is_empty()) {
        found.push((2, Rule::Body, "no blank line after the subject".to_owned()));
    }
    if body.iter().all(|line| line.trim().is_empty()) {
        let message = "no body: say why, and what was measured";
        found.push((1, Rule::Body, message.to_owned()));
    }
    for (index, line) in body.iter().enumerate() {
        let lower = line.to_lowercase();
        let credits = ["co-authored-by:", "generated with", "noreply@anthropic.com"];
        if credits.iter().any(|credit| lower.contains(credit)) {
            found.push((
                index + 2,
                Rule::Attribution,
                "an attribution line".to_owned(),
            ));
        }
    }
    found
}

/// The findings in what a message says of its change.
fn substance(subject_line: &str, body: &str, change: &Change) -> Vec<(Rule, String)> {
    let mut found = Vec::new();
    let Some(Subject { kind, scope, .. }) = parse_subject(subject_line) else {
        return found;
    };
    let body = body.to_lowercase();
    let touched = touched_crates(&change.paths);
    let own = scope
        .filter(|scope| !AREAS.contains(scope))
        .map(|scope| match scope {
            "irisd" => scope.to_owned(),
            other => format!("iris-{other}"),
        });
    if let Some(own) = &own {
        let prefix = format!("crates/{own}/");
        if !change.paths.iter().any(|path| path.starts_with(&prefix)) {
            found.push((
                Rule::Scope,
                format!("the scope names {own}, which is unchanged"),
            ));
        }
    }
    let others: Vec<&str> = touched
        .iter()
        .copied()
        .filter(|name| own.as_deref() != Some(*name))
        .collect();
    if scope.is_none() && CODE_TYPES.contains(&kind) && others.len() == 1 {
        let only = scope_of(others[0]);
        found.push((
            Rule::Scope,
            format!("the change is to one crate: `{kind}({only})`"),
        ));
    } else if own.is_some() || others.len() > 1 {
        for other in others.iter().filter(|name| !body.contains(**name)) {
            let message = format!(
                "also changes crates/{other}: split the commit, or say in the body why \
                 {other} belongs to it"
            );
            found.push((Rule::Scope, message));
        }
    }
    let code = change.paths.iter().any(|path| {
        path.strip_prefix("crates/")
            .and_then(|rest| rest.split_once('/'))
            .is_some_and(|(_, inside)| inside.starts_with("src/"))
    });
    let documented = change.paths.iter().any(|path| {
        Path::new(path)
            .extension()
            .is_some_and(|extension| extension == "md")
    });
    if kind == "feat" && code && !documented && !body.contains("docs/") {
        let message = "a feature that changes no document and names none under docs/: \
                       what describes the behaviour belongs to the change";
        found.push((Rule::Research, message.to_owned()));
    }
    if !change.added_packages.is_empty() && !["license", "licence"].iter().any(|w| body.contains(w))
    {
        let names: Vec<&str> = change
            .added_packages
            .iter()
            .take(5)
            .map(String::as_str)
            .collect();
        let message = format!(
            "adds {} to a lockfile ({}): say why and under what license",
            change.added_packages.len(),
            names.join(", ")
        );
        found.push((Rule::Dependency, message));
    }
    found
}

/// The findings in one commit: `message` is its whole message, `crates` the
/// directories under `crates/`, and `change` what it changes when that is
/// known. `label` names the commit in the findings.
pub fn check(
    label: &str,
    message: &str,
    crates: &[String],
    change: Option<&Change>,
    site: &Site,
) -> Vec<Finding> {
    let mut lines = message.lines();
    let subject_line = lines.next().unwrap_or("");
    let body: Vec<&str> = lines.collect();
    let mut findings: Vec<Finding> = form(subject_line, &body, crates)
        .into_iter()
        .map(|(line, rule, text)| Finding::new(rule, label, line, text))
        .collect();
    for (index, line) in message.lines().enumerate() {
        for text in site.scan(line, Own::Refused) {
            findings.push(Finding::new(Rule::Installation, label, index + 1, text));
        }
    }
    let numbered: Vec<(usize, &str)> = message.lines().enumerate().skip(1).collect();
    for paragraph in numbered.split(|(_, line)| line.trim().is_empty()) {
        for (index, phrase) in prose::narration(paragraph) {
            let text = format!(
                "\"{phrase}\": the body gives the reason for the change and the behaviour it \
                 brings, not how it was reached"
            );
            findings.push(Finding::new(Rule::Narration, label, index + 1, text));
        }
    }
    if let Some(change) = change {
        let said = body.join("\n");
        for (rule, text) in substance(subject_line, &said, change) {
            findings.push(Finding::new(rule, label, 0, text));
        }
    }
    findings
}

/// The names of the packages of a lockfile.
pub fn packages(lockfile: &str) -> BTreeSet<&str> {
    lockfile
        .lines()
        .filter_map(|line| line.strip_prefix("name = \"")?.strip_suffix('"'))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crates() -> Vec<String> {
        ["iris-api", "iris-gui", "irisd"]
            .map(str::to_owned)
            .to_vec()
    }

    fn rules(message: &str, change: Option<&Change>) -> Vec<Rule> {
        let found = check("commit", message, &crates(), change, &Site::default());
        found.into_iter().map(|finding| finding.rule).collect()
    }

    fn change(paths: &[&str]) -> Change {
        Change {
            paths: paths.iter().map(|path| (*path).to_owned()).collect(),
            added_packages: Vec::new(),
        }
    }

    #[test]
    fn a_well_formed_message_passes() {
        assert_eq!(
            rules(
                "fix(api): refuse a range that ends before it starts\n\nWhy.",
                None
            ),
            []
        );
        assert_eq!(rules("docs: say what a run is\n\nWhy.", None), []);
        assert_eq!(rules("feat(irisd)!: drop the flag\n\nWhy.", None), []);
    }

    #[test]
    fn the_subject_has_a_type_a_known_scope_and_a_command() {
        for subject in [
            "Fix the thing",
            "fix(api) refuse it",
            "feature(api): refuse it",
            "fix(fuzz): refuse it",
            "fix(API): refuse it",
            "fix(api): refuse it.",
            "fix(api): fixed the range",
            "fix(api):  ",
        ] {
            let message = format!("{subject}\n\nWhy.");
            assert_eq!(rules(&message, None), [Rule::Subject], "{subject}");
        }
        let long = format!("fix(api): {}\n\nWhy.", "a".repeat(62));
        assert_eq!(rules(&long, None), [Rule::Subject]);
        let fits = format!("fix(api): {}\n\nWhy.", "a".repeat(61));
        assert_eq!(rules(&fits, None), []);
    }

    #[test]
    fn the_body_is_there_apart_and_unattributed() {
        assert_eq!(rules("fix(api): refuse it", None), [Rule::Body]);
        assert_eq!(rules("fix(api): refuse it\nWhy.", None), [Rule::Body]);
        let credited = "fix(api): refuse it\n\nWhy.\n\nCo-Authored-By: A <a@example.com>";
        assert_eq!(rules(credited, None), [Rule::Attribution]);
    }

    #[test]
    fn a_message_names_no_installation() {
        let message = "fix(api): refuse it\n\nSeen on 192.168.1.9.";
        assert_eq!(rules(message, None), [Rule::Installation]);
    }

    #[test]
    fn a_body_gives_the_reason_and_not_the_story() {
        let story = "fix(api): refuse it\n\nEach audit found this. A rule was tried\nand withdrawn; \
                     this commit is what\nstayed.";
        assert_eq!(rules(story, None), [Rule::Narration; 4]);
        let reason = "fix(api): refuse it\n\nA range that ends before it starts read no fragment \
                      and answered 200.\nIt answers 400.";
        assert_eq!(rules(reason, None), []);
    }

    #[test]
    fn comment_lines_and_the_diff_are_not_the_message() {
        let raw = "fix(api): refuse it\n\nWhy.\n# Please enter\n# --- >8 ---\ndiff --git";
        assert_eq!(cleaned(raw), "fix(api): refuse it\n\nWhy.");
    }

    #[test]
    fn a_commit_says_why_it_reaches_past_its_scope() {
        let both = change(&["crates/iris-api/src/a.rs", "crates/iris-gui/src/b.rs"]);
        assert_eq!(
            rules("fix(api): refuse it\n\nWhy.", Some(&both)),
            [Rule::Scope]
        );
        let said = "fix(api): refuse it\n\nThe viewer (iris-gui) reads the field.";
        assert_eq!(rules(said, Some(&both)), []);
        // A crate's documents are not its code.
        let documents = change(&["crates/iris-api/src/a.rs", "crates/iris-gui/README.md"]);
        assert_eq!(rules("fix(api): refuse it\n\nWhy.", Some(&documents)), []);
        let elsewhere = change(&["crates/iris-gui/src/b.rs"]);
        assert_eq!(
            rules("fix(api): refuse it\n\nWhy.", Some(&elsewhere)),
            [Rule::Scope, Rule::Scope]
        );
    }

    #[test]
    fn a_code_change_to_one_crate_carries_its_scope() {
        let one = change(&["crates/iris-gui/src/b.rs"]);
        assert_eq!(rules("fix: refuse it\n\nWhy.", Some(&one)), [Rule::Scope]);
        assert_eq!(rules("docs: say it\n\nWhy.", Some(&one)), []);
        let both = change(&["crates/iris-api/src/a.rs", "crates/iris-gui/src/b.rs"]);
        assert_eq!(
            rules("fix: refuse it\n\nIn iris-api and iris-gui.", Some(&both)),
            []
        );
        assert_eq!(
            rules("fix: refuse it\n\nIn iris-api.", Some(&both)),
            [Rule::Scope]
        );
    }

    #[test]
    fn a_feature_brings_its_document() {
        let bare = change(&["crates/iris-api/src/a.rs"]);
        assert_eq!(
            rules("feat(api): serve it\n\nWhy.", Some(&bare)),
            [Rule::Research]
        );
        assert_eq!(rules("fix(api): serve it\n\nWhy.", Some(&bare)), []);
        let named = "feat(api): serve it\n\nDesigned in docs/research/13-audio.md.";
        assert_eq!(rules(named, Some(&bare)), []);
        for document in ["docs/research/13-audio.md", "crates/iris-api/README.md"] {
            let with = change(&["crates/iris-api/src/a.rs", document]);
            assert_eq!(
                rules("feat(api): serve it\n\nWhy.", Some(&with)),
                [],
                "{document}"
            );
        }
    }

    #[test]
    fn a_dependency_arrives_with_its_license() {
        let added = Change {
            added_packages: vec!["regex".to_owned()],
            ..change(&["Cargo.lock"])
        };
        assert_eq!(
            rules("build: add it\n\nWhy.", Some(&added)),
            [Rule::Dependency]
        );
        let said = "build: add it\n\nregex parses the feed; license MIT OR Apache-2.0.";
        assert_eq!(rules(said, Some(&added)), []);
        let lock = "[[package]]\nname = \"a\"\nversion = \"1\"\n\n[[package]]\nname = \"b\"\n";
        assert_eq!(packages(lock).into_iter().collect::<Vec<_>>(), ["a", "b"]);
    }
}
