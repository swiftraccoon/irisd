//! What the tree must hold beside its code: each crate's documents, each
//! fuzz target's registration, and the recipes and paths documents name.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use super::finding::{Finding, Rule};
use super::names::Names;

/// The repository a check reads.
#[derive(Debug, Default)]
pub struct Repo {
    /// Its root directory.
    pub root: PathBuf,
    /// The names of the `justfile`'s recipes.
    pub recipes: BTreeSet<String>,
    /// The names of the tree's code, which the names in its prose are
    /// checked against; nothing when the code was not read, and then the
    /// rule `unknown-name` checks nothing.
    pub names: Option<Names>,
}

impl Repo {
    /// The repository at `root`, with the recipes of its `justfile`.
    pub fn open(root: PathBuf) -> Self {
        let justfile = fs::read_to_string(root.join("justfile")).unwrap_or_default();
        Self {
            recipes: recipes(&justfile),
            root,
            names: None,
        }
    }

    /// Whether `relative` names a file or a directory of the tree.
    pub fn exists(&self, relative: &str) -> bool {
        self.root.join(relative).exists()
    }

    /// Whether git ignores `relative`: a document may name a file that each
    /// clone makes for itself.
    pub fn ignores(&self, relative: &str) -> bool {
        Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(["check-ignore", "-q", "--", relative])
            .status()
            .is_ok_and(|status| status.success())
    }

    /// The contents of `relative`, or nothing when it cannot be read as text.
    pub fn read(&self, relative: &str) -> Option<String> {
        fs::read_to_string(self.root.join(relative)).ok()
    }

    /// The names of the entries of the directory `relative`, sorted.
    pub fn entries(&self, relative: &str) -> Vec<String> {
        let Ok(entries) = fs::read_dir(self.root.join(relative)) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect();
        names.sort();
        names
    }
}

/// The names of the recipes of a `justfile`: the first word of every line
/// that starts in the first column with a name and holds a colon.
pub fn recipes(justfile: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for line in justfile.lines() {
        let name_len = line
            .bytes()
            .take_while(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(byte))
            .count();
        let (name, rest) = line.split_at(name_len);
        let declares = rest.starts_with(':') || rest.starts_with(' ');
        if !name.is_empty() && name != "set" && declares && rest.contains(':') {
            names.insert(name.to_owned());
        }
    }
    names
}

/// Every crate has a `README.md`, an `AGENTS.md`, crate-level rustdoc, and a
/// line in the layout of the root `AGENTS.md` (`AGENTS.md` rule 10).
pub fn crate_docs(repo: &Repo) -> Vec<Finding> {
    let mut findings = Vec::new();
    let layout = repo.read("AGENTS.md").unwrap_or_default();
    for name in repo.entries("crates") {
        let dir = format!("crates/{name}");
        if !repo.exists(&format!("{dir}/Cargo.toml")) {
            continue;
        }
        let mut miss = |message: String| {
            findings.push(Finding::new(Rule::CrateDocs, &dir, 0, message));
        };
        for document in ["README.md", "AGENTS.md"] {
            if !repo.exists(&format!("{dir}/{document}")) {
                miss(format!("no {document}"));
            }
        }
        let root_file = ["src/lib.rs", "src/main.rs"]
            .iter()
            .find_map(|file| repo.read(&format!("{dir}/{file}")));
        if !root_file.is_some_and(|text| text.trim_start().starts_with("//!")) {
            miss("the crate root does not open with `//!` documentation".to_owned());
        }
        if !layout.contains(&dir) {
            miss("the layout in the root AGENTS.md does not name it".to_owned());
        }
    }
    findings
}

/// Every fuzz target is a `[[bin]]` of `fuzz/Cargo.toml` and has a row in
/// `fuzz/README.md`.
pub fn fuzz_targets(repo: &Repo) -> Vec<Finding> {
    let manifest = repo.read("fuzz/Cargo.toml").unwrap_or_default();
    let readme = repo.read("fuzz/README.md").unwrap_or_default();
    let mut findings = Vec::new();
    for file in repo.entries("fuzz/fuzz_targets") {
        let Some(target) = file.strip_suffix(".rs") else {
            continue;
        };
        let path = format!("fuzz/fuzz_targets/{file}");
        if !manifest.contains(&format!("name = \"{target}\"")) {
            let message = "fuzz/Cargo.toml has no [[bin]] of this name";
            findings.push(Finding::new(Rule::FuzzTarget, &path, 0, message));
        }
        if !readme.contains(&format!("`{target}`")) {
            let message = "fuzz/README.md does not describe it";
            findings.push(Finding::new(Rule::FuzzTarget, &path, 0, message));
        }
    }
    findings
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recipes_are_the_names_in_the_first_column() {
        let names = recipes(
            "set shell := [\"bash\"]\n# note: x\ncheck: fmt test\n    cargo test: no\n\
             mutants +files:\ncross-check target=\"a\":\n@quiet:\n",
        );
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        assert_eq!(names, ["check", "cross-check", "mutants"]);
    }

    #[test]
    fn a_crate_without_its_documents_is_found() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("crates/a/src")).unwrap();
        fs::write(root.join("crates/a/Cargo.toml"), "").unwrap();
        fs::write(root.join("crates/a/src/lib.rs"), "fn f() {}\n").unwrap();
        fs::write(root.join("crates/a/README.md"), "").unwrap();
        fs::write(root.join("AGENTS.md"), "crates/b\n").unwrap();
        let found = crate_docs(&Repo::open(root.to_owned()));
        let messages: Vec<&str> = found.iter().map(|f| f.message.as_str()).collect();
        assert_eq!(
            messages,
            [
                "no AGENTS.md",
                "the crate root does not open with `//!` documentation",
                "the layout in the root AGENTS.md does not name it",
            ]
        );
    }

    #[test]
    fn an_unregistered_fuzz_target_is_found() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("fuzz/fuzz_targets")).unwrap();
        fs::write(root.join("fuzz/fuzz_targets/scan.rs"), "").unwrap();
        fs::write(root.join("fuzz/fuzz_targets/live.rs"), "").unwrap();
        fs::write(root.join("fuzz/Cargo.toml"), "name = \"scan\"\n").unwrap();
        fs::write(root.join("fuzz/README.md"), "| `scan` | x |\n").unwrap();
        let found = fuzz_targets(&Repo::open(root.to_owned()));
        assert_eq!(found.len(), 2);
        assert!(found.iter().all(|f| f.path == "fuzz/fuzz_targets/live.rs"));
    }
}
