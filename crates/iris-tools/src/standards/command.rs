//! The `standards` command: gathers what the checks read, runs them, and
//! prints what they find.

use std::collections::BTreeSet;
use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::{env, fs};

use anyhow::Context;
use clap::{Parser, Subcommand};

use super::commits::{self, Change};
use super::config::Config;
use super::finding::{Finding, Rule};
use super::installation::{Own, Site};
use super::layout::{self, Repo};
use super::{markdown, rust};

/// Arguments of `standards`.
#[derive(Debug, Parser)]
pub struct Args {
    #[command(subcommand)]
    check: Check,
}

#[derive(Debug, Subcommand)]
enum Check {
    /// Check every file git tracks or would track.
    Tree,
    /// Check the commits of a range, oldest first: the form of each message,
    /// and what each says of its change.
    Commits {
        /// The range, as `git rev-list` reads it (`origin/main..HEAD`).
        range: String,
        /// Report what a message fails to say of its change without failing:
        /// for history that was made before the rule.
        #[arg(long)]
        lenient: bool,
    },
    /// Check the message of the commit being made against what is staged:
    /// the `commit-msg` hook.
    Message {
        /// The file git wrote the message to.
        file: PathBuf,
    },
    /// Write the `pre-commit` and `commit-msg` hooks into this clone's git
    /// directory, so git runs `tree` before a commit and `message` as one
    /// is made.
    Hooks,
}

/// The hooks and the command each runs. They are written into a clone and
/// not kept in the tree, which holds no executable file.
const HOOKS: [(&str, &str); 2] = [
    ("pre-commit", "standards tree"),
    ("commit-msg", "standards message \"$1\""),
];

/// The text of the hook that runs `command`.
fn hook_text(command: &str) -> String {
    format!("#!/bin/sh\nexec cargo run --quiet --locked -p iris-tools -- {command}\n")
}

/// Writes the hooks into the git directory of the clone at `root`, and
/// refuses to replace a hook of another content.
fn install_hooks(root: &Path) -> anyhow::Result<()> {
    let dir = git(root, &["rev-parse", "--git-path", "hooks"])?;
    let dir = root.join(dir.trim());
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    for (name, command) in HOOKS {
        let path = dir.join(name);
        let text = hook_text(command);
        match fs::read_to_string(&path) {
            Ok(present) => anyhow::ensure!(
                present == text,
                "{} holds another hook; remove it or add the command to it by hand",
                path.display()
            ),
            Err(_) => {
                fs::write(&path, &text).with_context(|| format!("writing {}", path.display()))?;
            }
        }
        fs::set_permissions(&path, Permissions::from_mode(0o755))
            .with_context(|| format!("making {} executable", path.display()))?;
    }
    Ok(())
}

/// The trees whose files are copies of published sources: only the local
/// list of names applies to them.
const VENDORED: [&str; 3] = [".rust-reference/", "docs/standards/", "third_party/"];

/// The checks' own sources, whose tests hold what the patterns refuse.
const OWN_SOURCES: &str = "crates/iris-tools/src/standards/";

/// The names no committed file may hold, in a file git ignores.
const DENYLIST: &str = "docs/local/denylist.txt";

/// The lockfiles whose packages a commit may add to.
const LOCKFILES: [&str; 2] = ["Cargo.lock", "fuzz/Cargo.lock"];

fn git(root: &Path, args: &[&str]) -> anyhow::Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .context("running git")?;
    anyhow::ensure!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    String::from_utf8(output.stdout).context("git output is not UTF-8")
}

/// The root of the repository the command runs in.
fn root() -> anyhow::Result<PathBuf> {
    let here = env::current_dir().context("the working directory")?;
    Ok(PathBuf::from(
        git(&here, &["rev-parse", "--show-toplevel"])?.trim(),
    ))
}

/// What the site's files may name: the local list, the configured mail
/// addresses, and the repository's own address from the root manifest.
fn site(repo: &Repo, config: &Config) -> Site {
    let denied = repo.read(DENYLIST).unwrap_or_default();
    let manifest = repo.read("Cargo.toml").unwrap_or_default();
    let own = manifest
        .lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix("repository = \"https://github.com/")
        })
        .map(|rest| rest.trim_end_matches(['"', '/']).to_lowercase());
    let own = own.map_or_else(Vec::new, |path| {
        let owner = path.split('/').next().unwrap_or("").to_owned();
        vec![path, format!("{owner}.github.io")]
    });
    Site {
        denied: denied
            .lines()
            .map(|line| line.trim().to_lowercase())
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .collect(),
        allowed: config
            .allowed
            .iter()
            .map(|allowed| allowed.text.clone())
            .collect(),
        own,
    }
}

/// Whether rule 9 lets `path` hold the repository's own address.
fn own_address(path: &str) -> Own {
    if path == "README.md" || path.starts_with(".github/") || path.ends_with("Cargo.toml") {
        Own::Allowed
    } else {
        Own::Refused
    }
}

/// The findings in one file of the tree.
fn file_findings(path: &str, text: &str, repo: &Repo, site: &Site) -> Vec<Finding> {
    let mut findings = Vec::new();
    let vendored = VENDORED.iter().any(|tree| path.starts_with(tree));
    for (index, line) in text.lines().enumerate() {
        let number = index + 1;
        let scanned = if vendored || path.starts_with(OWN_SOURCES) {
            site.names(line, Own::Refused)
        } else {
            site.scan(line, own_address(path))
        };
        for message in scanned {
            findings.push(Finding::new(Rule::Installation, path, number, message));
        }
        if !vendored && (line.starts_with("<<<<<<< ") || line.starts_with(">>>>>>> ")) {
            findings.push(Finding::new(
                Rule::Leftover,
                path,
                number,
                "a conflict marker",
            ));
        }
    }
    if vendored {
        return findings;
    }
    if Path::new(path).extension().is_some_and(|ext| ext == "md") {
        findings.extend(markdown::check(path, text, repo));
    } else if Path::new(path).extension().is_some_and(|ext| ext == "rs") {
        findings.extend(rust::check(path, text));
    }
    findings
}

/// The findings in the tree at `root`.
fn tree(root: &Path) -> anyhow::Result<Vec<Finding>> {
    let repo = Repo::open(root.to_owned());
    let config = match repo.read("standards.toml") {
        Some(text) => Config::parse(&text)?,
        None => Config::default(),
    };
    let site = site(&repo, &config);
    let listed = git(
        root,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
    )?;
    let mut findings = Vec::new();
    for path in listed.split('\0').filter(|path| !path.is_empty()) {
        // A file that is not text, or that is deleted and not yet staged, has no lines.
        if let Ok(text) = fs::read_to_string(root.join(path)) {
            findings.extend(file_findings(path, &text, &repo, &site));
        }
    }
    for entry in git(root, &["ls-files", "-s", "-z"])?.split('\0') {
        let Some((mode, path)) = entry
            .split_once('\t')
            .and_then(|(meta, path)| Some((meta.split(' ').next()?, path)))
        else {
            continue;
        };
        if mode != "100644" {
            let message =
                format!("mode {mode}: gh-commit publishes only regular files of mode 100644");
            findings.push(Finding::new(Rule::FileMode, path, 0, message));
        }
    }
    findings.extend(layout::crate_docs(&repo));
    findings.extend(layout::fuzz_targets(&repo));
    Ok(config.settle(&findings))
}

/// The packages `new` adds to the lockfiles of `old`, where each is a
/// revision or `""` for the index.
fn added_packages(root: &Path, old: &str, new: &str, paths: &[String]) -> Vec<String> {
    let mut added = BTreeSet::new();
    for lockfile in LOCKFILES
        .iter()
        .filter(|lock| paths.iter().any(|path| path == *lock))
    {
        let before = git(root, &["show", &format!("{old}:{lockfile}")]).unwrap_or_default();
        let after = git(root, &["show", &format!("{new}:{lockfile}")]).unwrap_or_default();
        let before = commits::packages(&before);
        added.extend(
            commits::packages(&after)
                .into_iter()
                .filter(|name| !before.contains(name))
                .map(str::to_owned),
        );
    }
    added.into_iter().collect()
}

/// What the commit `hash` changes.
fn change_of(root: &Path, hash: &str) -> anyhow::Result<Change> {
    let names = git(
        root,
        &[
            "diff-tree",
            "--no-commit-id",
            "--name-only",
            "-r",
            "--root",
            hash,
        ],
    )?;
    let paths: Vec<String> = names.lines().map(str::to_owned).collect();
    Ok(Change {
        added_packages: added_packages(root, &format!("{hash}^"), hash, &paths),
        paths,
    })
}

/// The tree with nothing in it, which a commit without a parent is compared
/// against.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// Whether the commit being made amends `HEAD`. Git gives a hook the author
/// date of the commit in the making as `@<seconds> <zone>`, and an amended
/// commit keeps the date of the commit it replaces; a commit made in the
/// same second as `HEAD` reads as an amendment of it.
fn amends_head(root: &Path, author_date: Option<&str>) -> bool {
    let seconds = author_date.and_then(|date| date.trim_start_matches('@').split(' ').next());
    seconds.is_some_and(|seconds| {
        git(root, &["log", "-1", "--format=%at", "HEAD"]).is_ok_and(|head| head.trim() == seconds)
    })
}

/// What the commit being made changes: what is staged against `HEAD`, or
/// against its parent when the commit amends it, so that the message answers
/// for everything the commit will hold. `author_date` is the hook's
/// `GIT_AUTHOR_DATE`.
fn staged_change(root: &Path, author_date: Option<&str>) -> Option<Change> {
    let base = if amends_head(root, author_date) {
        "HEAD^"
    } else {
        "HEAD"
    };
    let commit = format!("{base}^{{commit}}");
    let known = git(root, &["rev-parse", "--verify", "--quiet", &commit]).is_ok();
    let base = if known { base } else { EMPTY_TREE };
    let names = git(root, &["diff", "--cached", "--name-only", base]).ok()?;
    let paths: Vec<String> = names.lines().map(str::to_owned).collect();
    Some(Change {
        added_packages: added_packages(root, base, "", &paths),
        paths,
    })
}

/// The directories under `crates/`.
fn crates(repo: &Repo) -> Vec<String> {
    repo.entries("crates")
}

/// The findings in the commits of `range`, and how many commits it holds.
fn range_findings(root: &Path, range: &str) -> anyhow::Result<(Vec<Finding>, usize)> {
    let repo = Repo::open(root.to_owned());
    let site = site(&repo, &Config::default());
    let crates = crates(&repo);
    let hashes = git(root, &["rev-list", "--reverse", "--no-merges", range])?;
    let mut findings = Vec::new();
    let mut count = 0;
    for hash in hashes.lines() {
        count += 1;
        let message = git(root, &["log", "-1", "--format=%B", hash])?;
        let change = change_of(root, hash)?;
        let label = format!("commit {}", hash.get(..7).unwrap_or(hash));
        findings.extend(commits::check(
            &label,
            message.trim(),
            &crates,
            Some(&change),
            &site,
        ));
    }
    Ok((findings, count))
}

/// The findings in the message of the commit being made.
fn message_findings(
    root: &Path,
    file: &Path,
    author_date: Option<&str>,
) -> anyhow::Result<Vec<Finding>> {
    let repo = Repo::open(root.to_owned());
    let site = site(&repo, &Config::default());
    let raw = fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    let change = staged_change(root, author_date);
    let message = commits::cleaned(&raw);
    Ok(commits::check(
        "message",
        &message,
        &crates(&repo),
        change.as_ref(),
        &site,
    ))
}

/// The findings in `message` as the message of one commit that holds the
/// difference between the commits `base` and `head`.
fn publication_findings(
    root: &Path,
    message: &str,
    base: &str,
    head: &str,
) -> anyhow::Result<Vec<Finding>> {
    let repo = Repo::open(root.to_owned());
    let site = site(&repo, &Config::default());
    let names = git(root, &["diff", "--name-only", base, head])?;
    let paths: Vec<String> = names.lines().map(str::to_owned).collect();
    let change = Change {
        added_packages: added_packages(root, base, head, &paths),
        paths,
    };
    Ok(commits::check(
        "message",
        message.trim(),
        &crates(&repo),
        Some(&change),
        &site,
    ))
}

/// Holds a message to the rules of a commit before it is published as the
/// one commit of the difference between `base` and `head`: prints each
/// finding and fails when one stands. A commit made through an API passes no
/// git hook, so its publisher calls this.
pub fn check_publication(message: &str, base: &str, head: &str) -> anyhow::Result<()> {
    let findings = publication_findings(&root()?, message, base, head)?;
    for finding in &findings {
        println!("{finding}");
    }
    anyhow::ensure!(
        findings.is_empty(),
        "{} findings in the message; see STANDARDS.md §9",
        findings.len()
    );
    Ok(())
}

/// Whether a rule reads what a message says of its change, which history made
/// before the rule may fail.
fn of_substance(rule: Rule) -> bool {
    matches!(rule, Rule::Scope | Rule::Research | Rule::Dependency)
}

/// Runs the command: prints each finding and fails when one stands.
pub fn run(args: &Args) -> anyhow::Result<()> {
    let root = root()?;
    let (findings, checked, lenient) = match &args.check {
        Check::Hooks => {
            install_hooks(&root)?;
            println!(
                "standards: git checks the tree before a commit and the message as one is made"
            );
            return Ok(());
        }
        Check::Tree => (tree(&root)?, "the tree".to_owned(), false),
        Check::Commits { range, lenient } => {
            let (findings, count) = range_findings(&root, range)?;
            (findings, format!("{count} commits"), *lenient)
        }
        Check::Message { file } => (
            message_findings(&root, file, env::var("GIT_AUTHOR_DATE").ok().as_deref())?,
            "the message".to_owned(),
            false,
        ),
    };
    let mut failures = 0_usize;
    for finding in &findings {
        if lenient && of_substance(finding.rule) {
            println!("note: {finding}");
        } else {
            println!("{finding}");
            failures += 1;
        }
    }
    anyhow::ensure!(
        failures == 0,
        "{failures} findings in {checked}; see STANDARDS.md §1"
    );
    println!("standards: {checked} checked, nothing found");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_in(root: &Path, args: &[&str]) {
        let identity = ["-c", "user.name=Test", "-c", "user.email=test@example.com"];
        let status = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(identity)
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    /// A repository with one crate, its documents, and one commit.
    fn repository() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git_in(root, &["init", "-q", "-b", "main"]);
        fs::create_dir_all(root.join("crates/iris-a/src")).unwrap();
        fs::write(root.join("crates/iris-a/Cargo.toml"), "").unwrap();
        fs::write(root.join("crates/iris-a/src/lib.rs"), "//! A.\n").unwrap();
        fs::write(root.join("crates/iris-a/README.md"), "# a\n").unwrap();
        fs::write(root.join("crates/iris-a/AGENTS.md"), "# a\n").unwrap();
        fs::write(root.join("AGENTS.md"), "`crates/iris-a`\n").unwrap();
        fs::write(root.join("justfile"), "check:\n").unwrap();
        git_in(root, &["add", "-A"]);
        git_in(
            root,
            &["commit", "-q", "-m", "feat: start\n\nThe first tree."],
        );
        dir
    }

    #[test]
    fn a_clean_tree_has_no_findings_and_a_breach_in_any_file_is_found() {
        let dir = repository();
        let root = dir.path();
        assert_eq!(tree(root).unwrap(), []);
        // Untracked files are read too, so a breach is found before it is staged.
        fs::write(root.join("notes.md"), "The new camera is at 192.168.4.4.\n").unwrap();
        fs::write(root.join("crates/iris-a/src/b.rs"), "const A: u8 = 0xAB;\n").unwrap();
        let found: Vec<(String, Rule)> = tree(root)
            .unwrap()
            .into_iter()
            .map(|finding| (finding.path, finding.rule))
            .collect();
        assert_eq!(
            found,
            [
                ("crates/iris-a/src/b.rs".to_owned(), Rule::HexCase),
                ("notes.md".to_owned(), Rule::Timeless),
                ("notes.md".to_owned(), Rule::Installation),
            ]
        );
    }

    #[test]
    fn waivers_and_the_local_list_are_read_from_the_tree() {
        let dir = repository();
        let root = dir.path();
        fs::write(root.join("notes.md"), "The new index, by Acme.\n").unwrap();
        fs::write(
            root.join("standards.toml"),
            "[[waiver]]\nrule = \"timeless\"\npath = \"notes.md\"\ncount = 1\nreason = \"a title\"\n",
        )
        .unwrap();
        assert_eq!(tree(root).unwrap(), []);
        fs::create_dir_all(root.join("docs/local")).unwrap();
        fs::write(root.join(DENYLIST), "# makers\nacme\n").unwrap();
        fs::write(root.join(".gitignore"), "/docs/local/\n").unwrap();
        let found = tree(root).unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(
            (found[0].path.as_str(), found[0].rule),
            ("notes.md", Rule::Installation)
        );
    }

    #[test]
    fn a_range_is_checked_commit_by_commit_against_its_change() {
        let dir = repository();
        let root = dir.path();
        fs::write(root.join("crates/iris-a/src/lib.rs"), "//! A.\n//! More.\n").unwrap();
        git_in(root, &["commit", "-q", "-am", "feat(a): add more\n\nWhy."]);
        fs::write(root.join("AGENTS.md"), "`crates/iris-a`.\n").unwrap();
        git_in(root, &["commit", "-q", "-am", "Fix the notes"]);
        let (found, count) = range_findings(root, "HEAD~2..HEAD").unwrap();
        assert_eq!(count, 2);
        let rules: Vec<Rule> = found.iter().map(|finding| finding.rule).collect();
        assert_eq!(rules, [Rule::Research, Rule::Subject, Rule::Body]);
        assert!(found[0].path.starts_with("commit "));
    }

    #[test]
    fn hooks_are_written_once_and_another_hook_is_left_alone() {
        let dir = repository();
        let root = dir.path();
        install_hooks(root).unwrap();
        install_hooks(root).unwrap();
        let hook = root.join(".git/hooks/commit-msg");
        assert_eq!(
            fs::read_to_string(&hook).unwrap(),
            "#!/bin/sh\nexec cargo run --quiet --locked -p iris-tools -- standards message \"$1\"\n"
        );
        assert_eq!(
            fs::metadata(&hook).unwrap().permissions().mode() & 0o777,
            0o755
        );
        fs::write(root.join(".git/hooks/pre-commit"), "#!/bin/sh\nexit 0\n").unwrap();
        assert!(install_hooks(root).is_err());
        // The hooks are in the clone, not the tree, which stays publishable.
        assert_eq!(tree(root).unwrap(), []);
    }

    #[test]
    fn an_executable_file_in_the_index_is_found() {
        let dir = repository();
        let root = dir.path();
        let script = root.join("run.sh");
        fs::write(&script, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&script, Permissions::from_mode(0o755)).unwrap();
        git_in(root, &["add", "run.sh"]);
        let found = tree(root).unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(
            (found[0].path.as_str(), found[0].rule),
            ("run.sh", Rule::FileMode)
        );
    }

    #[test]
    fn a_message_to_publish_is_checked_against_the_whole_difference() {
        let dir = repository();
        let root = dir.path();
        fs::create_dir_all(root.join("crates/iris-b/src")).unwrap();
        fs::write(root.join("crates/iris-b/src/lib.rs"), "//! B.\n").unwrap();
        git_in(root, &["add", "-A"]);
        git_in(root, &["commit", "-q", "-m", "wip"]);
        fs::write(root.join("crates/iris-a/src/lib.rs"), "//! A.\n//! More.\n").unwrap();
        git_in(root, &["commit", "-q", "-am", "wip"]);
        // Two local commits become one published commit: its message answers for both.
        let found = |message: &str| -> Vec<Rule> {
            let found = publication_findings(root, message, "HEAD^^", "HEAD").unwrap();
            found.into_iter().map(|finding| finding.rule).collect()
        };
        assert_eq!(found("fix(a): say more\n\nWhy."), [Rule::Scope]);
        assert_eq!(found("fix(a): say more\n\nWhy, and what iris-b gains."), []);
        assert_eq!(found("Fix it"), [Rule::Subject, Rule::Body]);
    }

    #[test]
    fn the_message_being_written_is_checked_against_what_is_staged() {
        let dir = repository();
        let root = dir.path();
        fs::write(root.join("crates/iris-a/src/lib.rs"), "//! A.\n//! More.\n").unwrap();
        git_in(root, &["add", "-A"]);
        let file = root.join("MESSAGE");
        fs::write(&file, "fix: say more\n\nWhy.\n# a comment\n").unwrap();
        let found = message_findings(root, &file, None).unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].message, "the change is to one crate: `fix(a)`");
        fs::write(&file, "fix(a): say more\n\nWhy.\n").unwrap();
        assert_eq!(message_findings(root, &file, None).unwrap(), []);
    }

    #[test]
    fn an_amended_commit_answers_for_all_it_holds() {
        let dir = repository();
        let root = dir.path();
        fs::write(root.join("crates/iris-a/src/lib.rs"), "//! A.\n//! More.\n").unwrap();
        git_in(root, &["commit", "-q", "-am", "fix(a): say more\n\nWhy."]);
        fs::write(root.join("AGENTS.md"), "`crates/iris-a`.\n").unwrap();
        git_in(root, &["add", "-A"]);
        // As a commit of its own, the staged change is to no crate.
        let alone = staged_change(root, Some("@1 +0000")).unwrap();
        assert_eq!(alone.paths, ["AGENTS.md"]);
        // As an amendment, it joins what `HEAD` changes.
        let date = git(root, &["log", "-1", "--format=@%at +0000", "HEAD"]).unwrap();
        let amended = staged_change(root, Some(date.trim())).unwrap();
        assert_eq!(amended.paths, ["AGENTS.md", "crates/iris-a/src/lib.rs"]);
        // A first commit is compared against the empty tree.
        let first = tempfile::tempdir().unwrap();
        git_in(first.path(), &["init", "-q", "-b", "main"]);
        fs::write(first.path().join("a.md"), "A.\n").unwrap();
        git_in(first.path(), &["add", "-A"]);
        assert_eq!(staged_change(first.path(), None).unwrap().paths, ["a.md"]);
    }
}
