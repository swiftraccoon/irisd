//! `doc`: the workspace's documentation, warnings as errors, made so that the
//! links from one crate of the workspace to another resolve.
//!
//! The command documents with `--no-deps`. rustdoc then links an item of
//! another crate by a relative path only when that crate's directory is in
//! the output directory as rendering starts. When the directory is absent,
//! an intra-doc link to the other crate does not resolve, and it takes one
//! of three forms. A path in backticks and square brackets stays as code in
//! square brackets. A bare path in square brackets stays as that text. A
//! label with a path as its target becomes an anchor whose address is the
//! path as written. A type of the other crate in a signature is a fourth
//! case: rustdoc prints it without a link. rustdoc reports none of the four,
//! even with `-D warnings`, and it exits with success.
//!
//! Cargo removes a crate's directory when that crate's documentation unit
//! starts. rustdoc makes the directory again only when it starts to render,
//! after it has analysed the whole crate. With jobs in parallel, a dependant
//! rendered in between gets links that do not resolve. So the command
//! documents the crates one at a time, with `--jobs 1`.
//!
//! One job would also compile the dependencies one at a time. So the command
//! first runs `cargo check --workspace --locked`, which builds what
//! `cargo doc` needs of the dependencies with jobs in parallel. The check
//! changes neither the output directory nor the fingerprint file described
//! below. When the check fails, the command exits with the check's status
//! and leaves the output directory as it was.
//!
//! A page from before the run can hold links that do not resolve.
//! `cargo doc` with jobs in parallel writes such pages. So does
//! `cargo doc -p` for some of the crates from an empty output directory. So
//! does a run of this command that a signal stops after Cargo emptied the
//! output directory. Cargo keeps such a page until it documents the crate
//! again, and nothing in the page says that a link is missing. So the command
//! keeps no page from before the run. After the check passes, it removes the
//! output directory and everything in it. That has three costs. Every run
//! documents every crate of the workspace. A run removes the pages of
//! dependencies that a developer built into the output directory. And a
//! documentation pass that fails leaves only the pages that it wrote before
//! the failure.
//!
//! Cargo documents the workspace's crates in no fixed order, so with one job
//! a crate can be documented before a crate it links to. So the command
//! makes the directory of each library of the workspace in the emptied
//! output directory before it documents. Then it runs
//! `cargo doc --workspace --no-deps --locked --jobs 1` with
//! `RUSTDOCFLAGS=-D warnings`.
//!
//! Cargo names the rustc that made the output in `.rustdoc_fingerprint.json`
//! in the build directory. That is the target directory unless
//! `build.build-dir` names another. When the file names another rustc, or
//! Cargo cannot parse it, Cargo empties the output directory before it
//! documents a crate, and so removes the directories made first. It writes
//! the file again whether or not rustdoc then succeeds. The pages of that
//! pass can hold links that do not resolve. When the file is missing, Cargo
//! writes it and empties nothing.
//!
//! So the command reads the file twice: before it empties the output
//! directory, and after the first pass. A file that it cannot read stops it
//! with an error at either read. Cargo emptied the output directory when it
//! rewrote the file that the command found. The command treats a file that
//! it found and that is gone after the pass as a rewritten file. After a
//! first pass that succeeded, the command then empties the output directory,
//! makes the directories again, and documents once more. After a first pass
//! that failed, it empties the output directory and exits with the failure,
//! so that it leaves no page whose links do not resolve.
//!
//! The target directory, the build directory, and the libraries come from
//! one document, the output of
//! `cargo metadata --format-version 1 --no-deps --locked`, read through
//! serde. The build directory is the document's `build_directory`. A Cargo
//! release without the `build.build-dir` setting prints no such key and keeps
//! the fingerprint file in the target directory, so the command reads it
//! there. A library's output directory is `doc` in the target directory,
//! joined with the library target's name with `-` written as `_`. Cargo
//! does not document a library target whose `doc` is `false` in the
//! document, so the command makes no directory for it. A link from another
//! crate to such a library does not resolve, and rustdoc reports nothing. In
//! a workspace without a library, the command makes no directory and
//! documents the binaries, which have no links from one crate to another.
//!
//! With a build target configured, Cargo writes the documentation to
//! `<triple>/doc` in the target directory, and `cargo metadata` does not
//! give the triple. So the command refuses to run while `CARGO_BUILD_TARGET`
//! is set. It cannot see `build.target` in a Cargo configuration file. So it
//! checks the result of the last documentation pass, when that pass
//! succeeded: each library directory that the command made must hold
//! `index.html`, the page rustdoc writes for the crate. With `build.target`
//! in a configuration file, no library directory holds that page. The
//! command then fails with an error that names the first such library and
//! the output directory. The pages stay in the directory Cargo wrote them
//! to, and their links to other crates of the workspace can be unresolved.
//!
//! Cargo ignores `RUSTDOCFLAGS` while `CARGO_ENCODED_RUSTDOCFLAGS` is set,
//! even to nothing, and warnings are then not errors. So the command refuses
//! to run while that variable is set.
//!
//! The command exits with cargo's status when the check or the documentation
//! fails, and with 1 when a library directory holds no `index.html`. When a
//! signal stopped `cargo metadata`, the check, or a documentation pass, the
//! command prints one line that names the signal and exits with 128 plus the
//! signal's number.

use std::io::ErrorKind;
use std::path::PathBuf;
use std::process::{self, Command, ExitStatus, Output};
use std::{env, fs};

use anyhow::Context;
use serde::Deserialize;

/// The file in the build directory that names the rustc of the output.
const FINGERPRINT: &str = ".rustdoc_fingerprint.json";

/// The page rustdoc writes in a crate's directory for the crate.
const INDEX: &str = "index.html";

/// The environment variable that configures a build target.
const BUILD_TARGET: &str = "CARGO_BUILD_TARGET";

/// The environment variable whose flags Cargo passes to rustdoc in place of
/// the flags of `RUSTDOCFLAGS`.
const ENCODED_RUSTDOCFLAGS: &str = "CARGO_ENCODED_RUSTDOCFLAGS";

/// The kinds of the targets that are not libraries. The JSON format in the
/// manual of `cargo metadata` gives every target a `kind` array. A library
/// lists the crate types of the manifest. A binary, an example, an
/// integration test, a benchmark, and a build script each list one of these.
const OTHER_KINDS: [&str; 5] = ["bin", "example", "test", "bench", "custom-build"];

/// The fields of `cargo metadata --format-version 1` the command reads.
#[derive(Debug, Deserialize)]
struct Metadata {
    /// The target directory; Cargo writes the documentation in `doc` inside
    /// it.
    target_directory: PathBuf,
    /// The directory Cargo keeps the fingerprint file in. A Cargo release
    /// without the `build.build-dir` setting prints no such key.
    build_directory: Option<PathBuf>,
    /// The workspace's packages, under `--no-deps`.
    packages: Vec<Package>,
}

/// A package of the workspace.
#[derive(Debug, Deserialize)]
struct Package {
    /// The package's library, binaries, tests, and other targets.
    targets: Vec<Target>,
}

/// A target of a package.
#[derive(Debug, Deserialize)]
struct Target {
    /// The target's name; a library's is its crate name.
    name: String,
    /// A library's crate types, or one of `OTHER_KINDS`.
    kind: Vec<String>,
    /// Whether Cargo documents the target: `false` for `doc = false` in the
    /// manifest, and `true` when the document gives the target no such key.
    #[serde(default = "documented")]
    doc: bool,
}

/// The `doc` of a target for which the document has no such key: Cargo
/// documents a target unless its manifest says otherwise.
fn documented() -> bool {
    true
}

impl Target {
    /// Whether the target is a library: none of its kinds is one of
    /// `OTHER_KINDS`. Cargo prints `bin` beside a crate type only for a
    /// library whose manifest lists both, and `cargo check` fails on that
    /// library, so the command stops before it makes a directory. For every
    /// other target, "none of its kinds" and "not every kind" are the same
    /// test.
    fn is_library(&self) -> bool {
        !self
            .kind
            .iter()
            .any(|kind| OTHER_KINDS.contains(&kind.as_str()))
    }
}

/// The directories the command documents in.
#[derive(Debug)]
struct Workspace {
    /// The target directory.
    target: PathBuf,
    /// The build directory, where Cargo keeps the fingerprint file.
    build: PathBuf,
    /// The name of the directory in the output directory of each library
    /// that Cargo documents.
    libraries: Vec<String>,
}

impl Workspace {
    /// Reads the document `cargo metadata --format-version 1 --no-deps`
    /// prints.
    fn parse(document: &[u8]) -> anyhow::Result<Self> {
        let metadata: Metadata = serde_json::from_slice(document)
            .context("decoding the document cargo metadata printed")?;
        let mut libraries = Vec::new();
        for package in &metadata.packages {
            for target in &package.targets {
                if target.is_library() && target.doc {
                    libraries.push(target.name.replace('-', "_"));
                }
            }
        }
        let build = metadata
            .build_directory
            .unwrap_or_else(|| metadata.target_directory.clone());
        Ok(Self {
            target: metadata.target_directory,
            build,
            libraries,
        })
    }

    /// The directory rustdoc writes every crate's pages to.
    fn output_directory(&self) -> PathBuf {
        self.target.join("doc")
    }

    /// Makes the directory of each library in the output directory.
    fn make_directories(&self) -> anyhow::Result<()> {
        for library in &self.libraries {
            let directory = self.output_directory().join(library);
            fs::create_dir_all(&directory)
                .with_context(|| format!("creating {}", directory.display()))?;
        }
        Ok(())
    }

    /// Removes the output directory and everything in it.
    fn clear(&self) -> anyhow::Result<()> {
        let directory = self.output_directory();
        match fs::remove_dir_all(&directory) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).with_context(|| format!("removing {}", directory.display())),
        }
    }

    /// Fails when the directory of a library in the output directory holds
    /// no index page, and names the first such library. rustdoc writes that
    /// page for every crate it documents.
    fn check_index_pages(&self) -> anyhow::Result<()> {
        let output = self.output_directory();
        for library in &self.libraries {
            anyhow::ensure!(
                output.join(library).join(INDEX).is_file(),
                "cargo doc succeeded, but the library {library} has no {INDEX} in {}; Cargo \
                 writes the documentation elsewhere when a build target is configured, as with \
                 `build.target` in a configuration file; run this command without a build target",
                output.display()
            );
        }
        Ok(())
    }

    /// The bytes of the fingerprint file, or `None` when it is missing.
    fn fingerprint(&self) -> anyhow::Result<Option<Vec<u8>>> {
        let path = self.build.join(FINGERPRINT);
        match fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
        }
    }
}

/// What the command does after the first pass of `cargo doc`.
#[derive(Debug, PartialEq, Eq)]
enum AfterFirstPass {
    /// End with the first pass's status: Cargo emptied nothing.
    Finish(ExitStatus),
    /// Empty the output directory and end with the first pass's failure.
    ClearAndFail(ExitStatus),
    /// Empty the output directory, make the directories, and document again.
    DocumentAgain,
}

/// The step after a first pass that ended with `first`, from the fingerprint
/// file's bytes before and after that pass (`None` for a missing file). A
/// file that the command found and that is rewritten or gone after the pass
/// means that the output directory was emptied.
#[must_use]
fn after_first_pass(
    before: Option<&[u8]>,
    after: Option<&[u8]>,
    first: ExitStatus,
) -> AfterFirstPass {
    let emptied = before.is_some() && before != after;
    if !emptied {
        AfterFirstPass::Finish(first)
    } else if first.success() {
        AfterFirstPass::DocumentAgain
    } else {
        AfterFirstPass::ClearAndFail(first)
    }
}

/// Runs `cargo metadata` in the working directory and collects its output.
fn metadata() -> anyhow::Result<Output> {
    Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--no-deps", "--locked"])
        .output()
        .context("running cargo metadata")
}

/// The document a finished `cargo metadata` printed, or its failure with
/// cargo's standard error. The warnings of a success are dropped:
/// `cargo doc` prints them again.
fn metadata_document(output: &Output) -> anyhow::Result<&[u8]> {
    anyhow::ensure!(
        output.status.success(),
        "cargo metadata failed ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(&output.stdout)
}

/// Runs `cargo check` over the workspace with jobs in parallel, its output
/// on the terminal.
fn check() -> anyhow::Result<ExitStatus> {
    Command::new("cargo")
        .args(["check", "--workspace", "--locked"])
        .status()
        .context("running cargo check")
}

/// Runs one pass of `cargo doc` over the workspace, one crate at a time and
/// with warnings as errors, its output on the terminal.
fn document() -> anyhow::Result<ExitStatus> {
    Command::new("cargo")
        .args(["doc", "--workspace", "--no-deps", "--locked", "--jobs", "1"])
        .env("RUSTDOCFLAGS", "-D warnings")
        .status()
        .context("running cargo doc")
}

/// Checks and documents `workspace`, and returns the status the command ends
/// with. `check` runs `cargo check` and `document` runs one pass of
/// `cargo doc`; the module documentation says why each step is there. It is
/// an error when the last pass succeeded and a library has no index page.
fn check_and_document(
    workspace: &Workspace,
    check: impl FnOnce() -> anyhow::Result<ExitStatus>,
    mut document: impl FnMut() -> anyhow::Result<ExitStatus>,
) -> anyhow::Result<ExitStatus> {
    let checked = check()?;
    if !checked.success() {
        return Ok(checked);
    }
    let before = workspace.fingerprint()?;
    workspace.clear()?;
    workspace.make_directories()?;
    let first = document()?;
    let after = workspace.fingerprint()?;
    let last = match after_first_pass(before.as_deref(), after.as_deref(), first) {
        AfterFirstPass::Finish(status) => status,
        AfterFirstPass::ClearAndFail(status) => {
            workspace.clear()?;
            eprintln!(
                "doc: Cargo rewrote the rustdoc fingerprint and emptied {} before the pass that \
                 failed; removed the pages of that pass",
                workspace.output_directory().display()
            );
            status
        }
        AfterFirstPass::DocumentAgain => {
            eprintln!(
                "doc: Cargo rewrote the rustdoc fingerprint and emptied {}; documenting again",
                workspace.output_directory().display()
            );
            workspace.clear()?;
            workspace.make_directories()?;
            document()?
        }
    };
    if last.success() {
        workspace.check_index_pages()?;
    }
    Ok(last)
}

/// The signal that stopped a process, or `None` when the process exited.
fn signal(status: ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt as _;

    status.signal()
}

/// Prints the line that names the signal that stopped cargo, and exits with
/// 128 plus the signal's number.
fn exit_as_stopped(signal: i32) -> ! {
    eprintln!("doc: cargo was stopped by signal {signal}");
    process::exit(128 + signal);
}

/// Runs the command, and exits with cargo's status when the check or the
/// documentation fails.
pub fn run() -> anyhow::Result<()> {
    anyhow::ensure!(
        env::var_os(BUILD_TARGET).is_none(),
        "{BUILD_TARGET} is set, so Cargo would write the documentation under the directory of \
         that target, where this command makes no directories; run it without {BUILD_TARGET}"
    );
    anyhow::ensure!(
        env::var_os(ENCODED_RUSTDOCFLAGS).is_none(),
        "{ENCODED_RUSTDOCFLAGS} is set, so Cargo would ignore RUSTDOCFLAGS and rustdoc's \
         warnings would not be errors; run it without {ENCODED_RUSTDOCFLAGS}"
    );
    let output = metadata()?;
    if let Some(signal) = signal(output.status) {
        exit_as_stopped(signal);
    }
    let workspace = Workspace::parse(metadata_document(&output)?)?;
    let status = check_and_document(&workspace, check, document)?;
    if status.success() {
        return Ok(());
    }
    if let Some(signal) = signal(status) {
        exit_as_stopped(signal);
    }
    process::exit(status.code().unwrap_or(1));
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::os::unix::process::ExitStatusExt as _;
    use std::path::Path;

    use serde_json::json;
    use tempfile::TempDir;

    use super::*;

    /// A fingerprint file that names one rustc.
    const ONE_RUSTC: &[u8] = br#"{"rustc_vv":"rustc 1.99.1"}"#;

    /// A fingerprint file that names another rustc.
    const ANOTHER_RUSTC: &[u8] = br#"{"rustc_vv":"rustc 1.98.0"}"#;

    /// The directory names of a workspace without a library.
    const NO_DIRECTORY: [&str; 0] = [];

    fn passed() -> ExitStatus {
        ExitStatus::from_raw(0)
    }

    /// The status of a process that exited with code 101, as cargo does.
    fn failed() -> ExitStatus {
        ExitStatus::from_raw(101 << 8)
    }

    /// The directory names `Workspace::parse` gives for a workspace of one
    /// package with one target of the given kinds.
    fn libraries_of(kinds: &[&str]) -> Vec<String> {
        let document = json!({
            "target_directory": "/t",
            "packages": [{"targets": [{"name": "iris_a", "kind": kinds}]}],
        });
        Workspace::parse(document.to_string().as_bytes())
            .unwrap()
            .libraries
    }

    /// A workspace with the library `iris_a` whose target directory is also
    /// its build directory.
    fn workspace_in(target: &Path) -> Workspace {
        Workspace {
            target: target.to_path_buf(),
            build: target.to_path_buf(),
            libraries: vec!["iris_a".to_owned()],
        }
    }

    /// The directory of the library `iris_a` in the output directory.
    fn library_directory(workspace: &Workspace) -> PathBuf {
        workspace.output_directory().join("iris_a")
    }

    /// The index page of the library `iris_a`.
    fn library_page(workspace: &Workspace) -> PathBuf {
        library_directory(workspace).join("index.html")
    }

    /// Writes `page` and the directories above it.
    fn write_page(page: &Path) {
        fs::create_dir_all(page.parent().unwrap()).unwrap();
        fs::write(page, "page").unwrap();
    }

    /// Does what a pass of `cargo doc` that succeeds does for the library
    /// `iris_a`: writes its index page. Returns the status of that pass.
    fn document_the_library(workspace: &Workspace) -> ExitStatus {
        write_page(&library_page(workspace));
        passed()
    }

    /// Does what Cargo does when the fingerprint file names another rustc:
    /// empties the output directory and writes the file again.
    fn empty_as_cargo_does(workspace: &Workspace) {
        let output = workspace.output_directory();
        if output.exists() {
            fs::remove_dir_all(&output).unwrap();
        }
        fs::create_dir_all(&output).unwrap();
        fs::write(workspace.build.join(FINGERPRINT), ONE_RUSTC).unwrap();
    }

    /// A target directory whose fingerprint file names another rustc than
    /// the one `empty_as_cargo_does` writes.
    fn target_of_another_rustc() -> TempDir {
        let target = tempfile::tempdir().unwrap();
        fs::write(target.path().join(FINGERPRINT), ANOTHER_RUSTC).unwrap();
        target
    }

    /// A target directory with the fingerprint file that a pass of
    /// `cargo doc` leaves when it empties nothing.
    fn target_with_a_fingerprint() -> TempDir {
        let target = tempfile::tempdir().unwrap();
        fs::write(target.path().join(FINGERPRINT), ONE_RUSTC).unwrap();
        target
    }

    #[test]
    fn a_fingerprint_written_by_a_pass_finishes_with_the_pass() {
        let next = after_first_pass(None, Some(ONE_RUSTC), passed());
        assert_eq!(next, AfterFirstPass::Finish(passed()));
    }

    #[test]
    fn a_fingerprint_written_by_a_failed_pass_finishes_with_the_failure() {
        let next = after_first_pass(None, Some(ONE_RUSTC), failed());
        assert_eq!(next, AfterFirstPass::Finish(failed()));
    }

    #[test]
    fn a_fingerprint_missing_throughout_a_pass_finishes_with_the_pass() {
        let next = after_first_pass(None, None, passed());
        assert_eq!(next, AfterFirstPass::Finish(passed()));
    }

    #[test]
    fn a_fingerprint_missing_throughout_a_failure_finishes_with_the_failure() {
        let next = after_first_pass(None, None, failed());
        assert_eq!(next, AfterFirstPass::Finish(failed()));
    }

    #[test]
    fn an_unchanged_fingerprint_after_a_pass_finishes_with_the_pass() {
        let next = after_first_pass(Some(ONE_RUSTC), Some(ONE_RUSTC), passed());
        assert_eq!(next, AfterFirstPass::Finish(passed()));
    }

    #[test]
    fn an_unchanged_fingerprint_after_a_failure_finishes_with_the_failure() {
        let next = after_first_pass(Some(ONE_RUSTC), Some(ONE_RUSTC), failed());
        assert_eq!(next, AfterFirstPass::Finish(failed()));
    }

    #[test]
    fn a_rewritten_fingerprint_after_a_pass_documents_again() {
        let next = after_first_pass(Some(ANOTHER_RUSTC), Some(ONE_RUSTC), passed());
        assert_eq!(next, AfterFirstPass::DocumentAgain);
    }

    #[test]
    fn a_rewritten_fingerprint_after_a_failure_fails_leaving_the_output_empty() {
        let next = after_first_pass(Some(ANOTHER_RUSTC), Some(ONE_RUSTC), failed());
        assert_eq!(next, AfterFirstPass::ClearAndFail(failed()));
    }

    #[test]
    fn a_removed_fingerprint_after_a_pass_documents_again() {
        let next = after_first_pass(Some(ONE_RUSTC), None, passed());
        assert_eq!(next, AfterFirstPass::DocumentAgain);
    }

    #[test]
    fn a_removed_fingerprint_after_a_failure_fails_leaving_the_output_empty() {
        let next = after_first_pass(Some(ONE_RUSTC), None, failed());
        assert_eq!(next, AfterFirstPass::ClearAndFail(failed()));
    }

    #[test]
    fn a_target_directory_decodes_its_json_escape_sequences() {
        let document = br#"{"packages": [], "target_directory": "/w/a \"b\" \\c/target"}"#;
        let workspace = Workspace::parse(document).unwrap();
        assert_eq!(workspace.target, PathBuf::from(r#"/w/a "b" \c/target"#));
    }

    #[test]
    fn a_build_directory_in_the_document_is_the_build_directory() {
        let document = br#"{"packages": [], "target_directory": "/t", "build_directory": "/b"}"#;
        assert_eq!(
            Workspace::parse(document).unwrap().build,
            PathBuf::from("/b")
        );
    }

    #[test]
    fn a_build_directory_in_the_document_does_not_move_the_target_directory() {
        let document = br#"{"packages": [], "target_directory": "/t", "build_directory": "/b"}"#;
        assert_eq!(
            Workspace::parse(document).unwrap().target,
            PathBuf::from("/t")
        );
    }

    #[test]
    fn a_document_without_a_build_directory_gives_the_target_directory() {
        let document = br#"{"packages": [], "target_directory": "/t"}"#;
        assert_eq!(
            Workspace::parse(document).unwrap().build,
            PathBuf::from("/t")
        );
    }

    #[test]
    fn a_document_without_a_target_directory_is_refused() {
        assert!(Workspace::parse(br#"{"packages": []}"#).is_err());
    }

    #[test]
    fn a_document_without_packages_is_refused() {
        assert!(Workspace::parse(br#"{"target_directory": "/t"}"#).is_err());
    }

    #[test]
    fn the_library_of_each_package_names_a_directory() {
        let document = br#"{"target_directory": "/t", "packages": [
            {"targets": [
                {"name": "iris-a", "kind": ["bin"]},
                {"name": "iris_a", "kind": ["lib"]}
            ]},
            {"targets": [{"name": "iris_b", "kind": ["lib"]}]}
        ]}"#;
        let workspace = Workspace::parse(document).unwrap();
        assert_eq!(workspace.libraries, ["iris_a", "iris_b"]);
    }

    #[test]
    fn a_workspace_without_a_library_names_no_directory() {
        let document = br#"{"target_directory": "/t", "packages": [
            {"targets": [{"name": "iris-b", "kind": ["bin"]}]}
        ]}"#;
        assert_eq!(Workspace::parse(document).unwrap().libraries, NO_DIRECTORY);
    }

    #[test]
    fn a_library_with_doc_true_names_a_directory() {
        let document = br#"{"target_directory": "/t", "packages": [
            {"targets": [{"name": "iris_a", "kind": ["lib"], "doc": true}]}
        ]}"#;
        assert_eq!(Workspace::parse(document).unwrap().libraries, ["iris_a"]);
    }

    #[test]
    fn a_library_with_doc_false_names_no_directory() {
        let document = br#"{"target_directory": "/t", "packages": [
            {"targets": [{"name": "iris_a", "kind": ["lib"], "doc": false}]}
        ]}"#;
        assert_eq!(Workspace::parse(document).unwrap().libraries, NO_DIRECTORY);
    }

    #[test]
    fn a_library_without_the_doc_key_names_a_directory() {
        assert_eq!(libraries_of(&["lib"]), ["iris_a"]);
    }

    #[test]
    fn a_proc_macro_target_is_a_library() {
        assert_eq!(libraries_of(&["proc-macro"]), ["iris_a"]);
    }

    #[test]
    fn an_sdylib_target_is_a_library() {
        assert_eq!(libraries_of(&["sdylib"]), ["iris_a"]);
    }

    #[test]
    fn a_target_of_several_crate_types_is_a_library() {
        let kinds = ["rlib", "dylib", "cdylib", "staticlib"];
        assert_eq!(libraries_of(&kinds), ["iris_a"]);
    }

    #[test]
    fn a_binary_is_not_a_library() {
        assert_eq!(libraries_of(&["bin"]), NO_DIRECTORY);
    }

    #[test]
    fn an_example_is_not_a_library() {
        assert_eq!(libraries_of(&["example"]), NO_DIRECTORY);
    }

    #[test]
    fn an_integration_test_is_not_a_library() {
        assert_eq!(libraries_of(&["test"]), NO_DIRECTORY);
    }

    #[test]
    fn a_benchmark_is_not_a_library() {
        assert_eq!(libraries_of(&["bench"]), NO_DIRECTORY);
    }

    #[test]
    fn a_build_script_is_not_a_library() {
        assert_eq!(libraries_of(&["custom-build"]), NO_DIRECTORY);
    }

    #[test]
    fn a_dash_in_a_library_name_is_written_as_an_underscore() {
        let document = br#"{"target_directory": "/t", "packages": [
            {"targets": [{"name": "iris-a", "kind": ["lib"]}]}
        ]}"#;
        assert_eq!(Workspace::parse(document).unwrap().libraries, ["iris_a"]);
    }

    #[test]
    fn a_failed_metadata_run_reports_cargo_standard_error() {
        let output = Output {
            status: failed(),
            stdout: Vec::new(),
            stderr: b"error: the lock file needs to be updated\n".to_vec(),
        };
        let error = metadata_document(&output).unwrap_err();
        assert_eq!(
            error.to_string(),
            "cargo metadata failed (exit status: 101): error: the lock file needs to be updated"
        );
    }

    #[test]
    fn a_finished_metadata_run_gives_its_standard_output() {
        let output = Output {
            status: passed(),
            stdout: br#"{"packages": []}"#.to_vec(),
            stderr: b"warning: an unused manifest key\n".to_vec(),
        };
        let document = metadata_document(&output).unwrap();
        assert_eq!(document, br#"{"packages": []}"#);
    }

    #[test]
    fn the_output_directory_is_doc_in_the_target_directory() {
        let workspace = workspace_in(Path::new("/t"));
        assert_eq!(workspace.output_directory(), PathBuf::from("/t/doc"));
    }

    #[test]
    fn the_fingerprint_is_read_from_its_file_in_the_build_directory() {
        let target = tempfile::tempdir().unwrap();
        let build = tempfile::tempdir().unwrap();
        fs::write(build.path().join(".rustdoc_fingerprint.json"), ONE_RUSTC).unwrap();
        let workspace = Workspace {
            build: build.path().to_path_buf(),
            ..workspace_in(target.path())
        };
        let fingerprint = workspace.fingerprint().unwrap();
        assert_eq!(fingerprint.as_deref(), Some(ONE_RUSTC));
    }

    #[test]
    fn a_missing_fingerprint_file_is_none() {
        let target = tempfile::tempdir().unwrap();
        let fingerprint = workspace_in(target.path()).fingerprint().unwrap();
        assert_eq!(fingerprint, None);
    }

    #[test]
    fn a_fingerprint_that_cannot_be_read_is_an_error() {
        let target = tempfile::tempdir().unwrap();
        fs::create_dir(target.path().join(FINGERPRINT)).unwrap();
        assert!(workspace_in(target.path()).fingerprint().is_err());
    }

    #[test]
    fn each_library_gets_a_directory_in_the_output_directory() {
        let target = tempfile::tempdir().unwrap();
        workspace_in(target.path()).make_directories().unwrap();
        assert!(target.path().join("doc/iris_a").is_dir());
    }

    #[test]
    fn clearing_removes_an_output_directory_that_holds_pages() {
        let target = tempfile::tempdir().unwrap();
        let workspace = workspace_in(target.path());
        workspace.make_directories().unwrap();
        fs::write(library_directory(&workspace).join("index.html"), "page").unwrap();
        workspace.clear().unwrap();
        assert!(!target.path().join("doc").exists());
    }

    #[test]
    fn clearing_leaves_the_rest_of_the_target_directory() {
        let target = tempfile::tempdir().unwrap();
        let workspace = workspace_in(target.path());
        workspace.make_directories().unwrap();
        fs::create_dir(target.path().join("debug")).unwrap();
        workspace.clear().unwrap();
        assert!(target.path().join("debug").is_dir());
    }

    #[test]
    fn clearing_a_missing_output_directory_succeeds() {
        let target = tempfile::tempdir().unwrap();
        assert!(workspace_in(target.path()).clear().is_ok());
    }

    #[test]
    fn a_failed_check_is_the_final_status() {
        let target = tempfile::tempdir().unwrap();
        let workspace = workspace_in(target.path());
        let status = check_and_document(&workspace, || Ok(failed()), || Ok(passed())).unwrap();
        assert_eq!(status, failed());
    }

    #[test]
    fn a_failed_check_documents_nothing() {
        let target = tempfile::tempdir().unwrap();
        let workspace = workspace_in(target.path());
        let passes = Cell::new(0);
        let document = || {
            passes.set(passes.get() + 1);
            Ok(passed())
        };
        check_and_document(&workspace, || Ok(failed()), document).unwrap();
        assert_eq!(passes.get(), 0);
    }

    #[test]
    fn a_failed_check_leaves_the_pages_in_the_output_directory() {
        let target = tempfile::tempdir().unwrap();
        let workspace = workspace_in(target.path());
        let page = library_page(&workspace);
        write_page(&page);
        check_and_document(&workspace, || Ok(failed()), || Ok(passed())).unwrap();
        assert!(page.exists());
    }

    #[test]
    fn the_check_runs_before_the_first_pass() {
        let target = target_with_a_fingerprint();
        let workspace = workspace_in(target.path());
        let steps = RefCell::new(Vec::new());
        let check = || {
            steps.borrow_mut().push("check");
            Ok(passed())
        };
        let document = || {
            steps.borrow_mut().push("doc");
            Ok(document_the_library(&workspace))
        };
        check_and_document(&workspace, check, document).unwrap();
        assert_eq!(*steps.borrow(), ["check", "doc"]);
    }

    #[test]
    fn a_page_from_before_the_run_is_gone_as_the_first_pass_starts() {
        let target = tempfile::tempdir().unwrap();
        let workspace = workspace_in(target.path());
        let page = library_page(&workspace);
        write_page(&page);
        let kept = Cell::new(true);
        let document = || {
            kept.set(page.exists());
            Ok(document_the_library(&workspace))
        };
        check_and_document(&workspace, || Ok(passed()), document).unwrap();
        assert!(!kept.get());
    }

    #[test]
    fn a_page_outside_the_library_directories_is_gone_as_the_first_pass_starts() {
        let target = tempfile::tempdir().unwrap();
        let workspace = workspace_in(target.path());
        let page = workspace.output_directory().join("iris_b/index.html");
        write_page(&page);
        let kept = Cell::new(true);
        let document = || {
            kept.set(page.exists());
            Ok(document_the_library(&workspace))
        };
        check_and_document(&workspace, || Ok(passed()), document).unwrap();
        assert!(!kept.get());
    }

    #[test]
    fn an_output_directory_that_cannot_be_removed_is_an_error() {
        let target = tempfile::tempdir().unwrap();
        let workspace = Workspace {
            libraries: Vec::new(),
            ..workspace_in(target.path())
        };
        // Removing a directory and everything in it fails on a file.
        fs::write(workspace.output_directory(), "a file").unwrap();
        let result = check_and_document(&workspace, || Ok(passed()), || Ok(passed()));
        assert!(result.is_err());
    }

    #[test]
    fn a_fingerprint_that_cannot_be_read_before_the_first_pass_documents_nothing() {
        let target = tempfile::tempdir().unwrap();
        let workspace = workspace_in(target.path());
        // Reading a file fails on a directory.
        fs::create_dir(workspace.build.join(FINGERPRINT)).unwrap();
        let passes = Cell::new(0);
        let document = || {
            passes.set(passes.get() + 1);
            Ok(passed())
        };
        let _error = check_and_document(&workspace, || Ok(passed()), document);
        assert_eq!(passes.get(), 0);
    }

    #[test]
    fn a_fingerprint_that_cannot_be_read_after_the_first_pass_is_an_error() {
        let target = tempfile::tempdir().unwrap();
        let workspace = workspace_in(target.path());
        let document = || {
            fs::create_dir(workspace.build.join(FINGERPRINT)).unwrap();
            Ok(failed())
        };
        let result = check_and_document(&workspace, || Ok(passed()), document);
        assert!(result.is_err());
    }

    #[test]
    fn the_library_directories_stand_as_the_first_pass_starts() {
        let target = target_with_a_fingerprint();
        let workspace = workspace_in(target.path());
        let made = Cell::new(false);
        let document = || {
            made.set(library_directory(&workspace).is_dir());
            Ok(document_the_library(&workspace))
        };
        check_and_document(&workspace, || Ok(passed()), document).unwrap();
        assert!(made.get());
    }

    #[test]
    fn a_first_pass_that_leaves_the_fingerprint_is_the_only_pass() {
        let target = target_with_a_fingerprint();
        let workspace = workspace_in(target.path());
        let passes = Cell::new(0);
        let document = || {
            passes.set(passes.get() + 1);
            Ok(document_the_library(&workspace))
        };
        check_and_document(&workspace, || Ok(passed()), document).unwrap();
        assert_eq!(passes.get(), 1);
    }

    #[test]
    fn the_pages_of_the_only_pass_stand_as_the_command_ends() {
        let target = target_with_a_fingerprint();
        let workspace = workspace_in(target.path());
        let page = library_directory(&workspace).join("fn.f.html");
        let document = || {
            write_page(&page);
            Ok(document_the_library(&workspace))
        };
        check_and_document(&workspace, || Ok(passed()), document).unwrap();
        assert!(page.exists());
    }

    #[test]
    fn a_failed_only_pass_is_the_final_status() {
        let target = tempfile::tempdir().unwrap();
        let workspace = workspace_in(target.path());
        let status = check_and_document(&workspace, || Ok(passed()), || Ok(failed())).unwrap();
        assert_eq!(status, failed());
    }

    #[test]
    fn a_failed_only_pass_keeps_the_pages_it_wrote() {
        let target = tempfile::tempdir().unwrap();
        let workspace = workspace_in(target.path());
        let page = library_page(&workspace);
        let document = || {
            write_page(&page);
            Ok(failed())
        };
        check_and_document(&workspace, || Ok(passed()), document).unwrap();
        assert!(page.exists());
    }

    #[test]
    fn a_pass_that_writes_the_index_page_of_every_library_ends_with_success() {
        let target = tempfile::tempdir().unwrap();
        let workspace = Workspace {
            libraries: vec!["iris_a".to_owned(), "iris_b".to_owned()],
            ..workspace_in(target.path())
        };
        let document = || {
            write_page(&workspace.output_directory().join("iris_a/index.html"));
            write_page(&workspace.output_directory().join("iris_b/index.html"));
            Ok(passed())
        };
        let status = check_and_document(&workspace, || Ok(passed()), document).unwrap();
        assert_eq!(status, passed());
    }

    #[test]
    fn a_pass_that_writes_no_index_page_is_an_error() {
        let target = tempfile::tempdir().unwrap();
        let workspace = workspace_in(target.path());
        let result = check_and_document(&workspace, || Ok(passed()), || Ok(passed()));
        assert!(result.is_err());
    }

    #[test]
    fn the_error_for_a_missing_index_page_names_the_output_directory() {
        let target = tempfile::tempdir().unwrap();
        let build = tempfile::tempdir().unwrap();
        let workspace = Workspace {
            build: build.path().to_path_buf(),
            ..workspace_in(target.path())
        };
        let error = check_and_document(&workspace, || Ok(passed()), || Ok(passed())).unwrap_err();
        assert_eq!(
            error.to_string(),
            format!(
                "cargo doc succeeded, but the library iris_a has no index.html in {}/doc; Cargo \
                 writes the documentation elsewhere when a build target is configured, as with \
                 `build.target` in a configuration file; run this command without a build target",
                target.path().display()
            )
        );
    }

    #[test]
    fn the_error_names_the_first_library_without_an_index_page() {
        let target = tempfile::tempdir().unwrap();
        let workspace = Workspace {
            libraries: vec![
                "iris_a".to_owned(),
                "iris_b".to_owned(),
                "iris_c".to_owned(),
            ],
            ..workspace_in(target.path())
        };
        let document = || Ok(document_the_library(&workspace));
        let error = check_and_document(&workspace, || Ok(passed()), document).unwrap_err();
        let text = error.to_string();
        assert!(
            text.contains("the library iris_b has no index.html"),
            "{text}"
        );
    }

    #[test]
    fn a_workspace_without_a_library_is_not_checked_for_index_pages() {
        let target = tempfile::tempdir().unwrap();
        let workspace = Workspace {
            libraries: Vec::new(),
            ..workspace_in(target.path())
        };
        let status = check_and_document(&workspace, || Ok(passed()), || Ok(passed())).unwrap();
        assert_eq!(status, passed());
    }

    #[test]
    fn a_failed_pass_that_writes_no_index_page_is_not_an_error() {
        let target = tempfile::tempdir().unwrap();
        let workspace = workspace_in(target.path());
        let result = check_and_document(&workspace, || Ok(passed()), || Ok(failed()));
        assert!(result.is_ok());
    }

    #[test]
    fn a_first_pass_in_which_cargo_empties_the_output_is_followed_by_a_second() {
        let target = target_of_another_rustc();
        let workspace = workspace_in(target.path());
        let passes = Cell::new(0);
        let document = || {
            passes.set(passes.get() + 1);
            if passes.get() == 1 {
                empty_as_cargo_does(&workspace);
            }
            Ok(document_the_library(&workspace))
        };
        check_and_document(&workspace, || Ok(passed()), document).unwrap();
        assert_eq!(passes.get(), 2);
    }

    #[test]
    fn the_library_directories_stand_as_the_second_pass_starts() {
        let target = target_of_another_rustc();
        let workspace = workspace_in(target.path());
        let passes = Cell::new(0);
        let made = Cell::new(false);
        let document = || {
            passes.set(passes.get() + 1);
            if passes.get() == 1 {
                empty_as_cargo_does(&workspace);
            } else {
                made.set(library_directory(&workspace).is_dir());
            }
            Ok(document_the_library(&workspace))
        };
        check_and_document(&workspace, || Ok(passed()), document).unwrap();
        assert!(made.get());
    }

    #[test]
    fn the_second_pass_starts_without_the_pages_of_the_first() {
        let target = target_of_another_rustc();
        let workspace = workspace_in(target.path());
        let page = workspace.output_directory().join("index.html");
        let passes = Cell::new(0);
        let kept = Cell::new(true);
        let document = || {
            passes.set(passes.get() + 1);
            if passes.get() == 1 {
                empty_as_cargo_does(&workspace);
                fs::write(&page, "page").unwrap();
            } else {
                kept.set(page.exists());
            }
            Ok(document_the_library(&workspace))
        };
        check_and_document(&workspace, || Ok(passed()), document).unwrap();
        assert!(!kept.get());
    }

    #[test]
    fn the_pages_of_the_second_pass_stand_as_the_command_ends() {
        let target = target_of_another_rustc();
        let workspace = workspace_in(target.path());
        let page = library_directory(&workspace).join("fn.f.html");
        let passes = Cell::new(0);
        let document = || {
            passes.set(passes.get() + 1);
            if passes.get() == 1 {
                empty_as_cargo_does(&workspace);
            } else {
                write_page(&page);
            }
            Ok(document_the_library(&workspace))
        };
        check_and_document(&workspace, || Ok(passed()), document).unwrap();
        assert!(page.exists());
    }

    #[test]
    fn a_first_pass_that_a_second_follows_is_not_checked_for_index_pages() {
        let target = target_of_another_rustc();
        let workspace = workspace_in(target.path());
        let passes = Cell::new(0);
        let document = || {
            passes.set(passes.get() + 1);
            if passes.get() == 1 {
                empty_as_cargo_does(&workspace);
                Ok(passed())
            } else {
                Ok(document_the_library(&workspace))
            }
        };
        let result = check_and_document(&workspace, || Ok(passed()), document);
        assert!(result.is_ok());
    }

    #[test]
    fn a_second_pass_that_writes_no_index_page_is_an_error() {
        let target = target_of_another_rustc();
        let workspace = workspace_in(target.path());
        let passes = Cell::new(0);
        let document = || {
            passes.set(passes.get() + 1);
            if passes.get() == 1 {
                empty_as_cargo_does(&workspace);
                Ok(document_the_library(&workspace))
            } else {
                Ok(passed())
            }
        };
        let result = check_and_document(&workspace, || Ok(passed()), document);
        assert!(result.is_err());
    }

    #[test]
    fn a_failed_second_pass_is_the_final_status() {
        let target = target_of_another_rustc();
        let workspace = workspace_in(target.path());
        let passes = Cell::new(0);
        let document = || {
            passes.set(passes.get() + 1);
            if passes.get() == 1 {
                empty_as_cargo_does(&workspace);
                Ok(passed())
            } else {
                Ok(failed())
            }
        };
        let status = check_and_document(&workspace, || Ok(passed()), document).unwrap();
        assert_eq!(status, failed());
    }

    #[test]
    fn a_failed_second_pass_keeps_the_pages_it_wrote() {
        let target = target_of_another_rustc();
        let workspace = workspace_in(target.path());
        let page = library_page(&workspace);
        let passes = Cell::new(0);
        let document = || {
            passes.set(passes.get() + 1);
            if passes.get() == 1 {
                empty_as_cargo_does(&workspace);
                Ok(passed())
            } else {
                write_page(&page);
                Ok(failed())
            }
        };
        check_and_document(&workspace, || Ok(passed()), document).unwrap();
        assert!(page.exists());
    }

    #[test]
    fn a_failed_first_pass_in_which_cargo_empties_the_output_leaves_no_output() {
        let target = target_of_another_rustc();
        let workspace = workspace_in(target.path());
        let document = || {
            empty_as_cargo_does(&workspace);
            Ok(failed())
        };
        check_and_document(&workspace, || Ok(passed()), document).unwrap();
        assert!(!workspace.output_directory().exists());
    }

    #[test]
    fn a_failed_first_pass_in_which_cargo_empties_the_output_is_the_only_pass() {
        let target = target_of_another_rustc();
        let workspace = workspace_in(target.path());
        let passes = Cell::new(0);
        let document = || {
            passes.set(passes.get() + 1);
            empty_as_cargo_does(&workspace);
            Ok(failed())
        };
        check_and_document(&workspace, || Ok(passed()), document).unwrap();
        assert_eq!(passes.get(), 1);
    }

    #[test]
    fn a_failed_first_pass_in_which_cargo_empties_the_output_is_the_final_status() {
        let target = target_of_another_rustc();
        let workspace = workspace_in(target.path());
        let document = || {
            empty_as_cargo_does(&workspace);
            Ok(failed())
        };
        let status = check_and_document(&workspace, || Ok(passed()), document).unwrap();
        assert_eq!(status, failed());
    }
}
