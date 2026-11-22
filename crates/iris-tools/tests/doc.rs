//! `iris-tools doc` as a process, with a script in place of cargo: the
//! commands it runs, the status it exits with, the lines it prints, and the
//! environment it refuses.

#![expect(
    clippy::unwrap_used,
    reason = "a helper of a test fails as the test does, by panicking"
)]

use std::ffi::OsString;
use std::fs::{self, Permissions};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Mutex;
use std::{env, io};

use tempfile::TempDir;

/// The script that stands for cargo. It appends the arguments of each call
/// to the file `calls` beside it. `metadata` prints a document that names a
/// target directory beside the script and one package with the library
/// `iris_a`. `doc` writes `RUSTDOCFLAGS` to the file `rustdocflags` and the
/// fingerprint file to the target directory, and it writes the index page of
/// the library unless `FAKE_PAGES` is `none`. `metadata`, `check`, and `doc`
/// end as `FAKE_METADATA`, `FAKE_CHECK`, and `FAKE_DOC` say: with that exit
/// code, or stopped by SIGKILL for `kill`.
const CARGO: &str = r#"#!/bin/sh
here=$(dirname "$0")
echo "$*" >> "$here/calls"
end=0
case "$1" in
    metadata)
        printf '{"target_directory": "%s/target", "packages": [{"targets": [' "$here"
        printf '{"name": "iris_a", "kind": ["lib"], "doc": true}]}]}'
        end=${FAKE_METADATA:-0}
        ;;
    check) end=${FAKE_CHECK:-0} ;;
    doc)
        echo "$RUSTDOCFLAGS" > "$here/rustdocflags"
        mkdir -p "$here/target/doc/iris_a"
        echo "the rustc of the script" > "$here/target/.rustdoc_fingerprint.json"
        if [ "$FAKE_PAGES" != none ]; then
            echo "the index page" > "$here/target/doc/iris_a/index.html"
        fi
        end=${FAKE_DOC:-0}
        ;;
esac
if [ "$end" = kill ]; then
    kill -KILL $$
fi
exit "$end"
"#;

/// The line the command prints when it refuses `CARGO_BUILD_TARGET`.
const BUILD_TARGET_REFUSED: &str = "Error: CARGO_BUILD_TARGET is set, so Cargo would write the \
    documentation under the directory of that target, where this command makes no directories; \
    run it without CARGO_BUILD_TARGET\n";

/// The line the command prints when it refuses `CARGO_ENCODED_RUSTDOCFLAGS`.
const ENCODED_RUSTDOCFLAGS_REFUSED: &str = "Error: CARGO_ENCODED_RUSTDOCFLAGS is set, so Cargo \
    would ignore RUSTDOCFLAGS and rustdoc's warnings would not be errors; run it without \
    CARGO_ENCODED_RUSTDOCFLAGS\n";

/// Held while a test writes the script and runs the command. Linux refuses
/// to execute a file that a process holds open for writing, and a child
/// forked by one test thread inherits the script another thread is writing.
/// So the runs take turns.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

/// What a run of `iris-tools doc` against the script left.
struct Run {
    /// The exit status and the output streams of `iris-tools`.
    output: Output,
    /// The arguments of each call of the script, in order.
    calls: Vec<String>,
    /// The `RUSTDOCFLAGS` the `doc` call saw, with its line end.
    rustdocflags: String,
    /// The output directory of the workspace the script describes.
    output_directory: PathBuf,
}

impl Run {
    /// What `iris-tools` wrote to its standard error.
    fn standard_error(&self) -> String {
        String::from_utf8_lossy(&self.output.stderr).into_owned()
    }
}

/// Runs `iris-tools doc` in a directory without a fingerprint file.
fn run_doc(variables: &[(&str, &str)]) -> Run {
    run_doc_in(&tempfile::tempdir().unwrap(), variables)
}

/// Runs `iris-tools doc` in a directory whose fingerprint file names another
/// rustc than the script writes, so that the script's `doc` rewrites it.
fn run_doc_after_another_rustc(variables: &[(&str, &str)]) -> Run {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target");
    fs::create_dir(&target).unwrap();
    fs::write(target.join(".rustdoc_fingerprint.json"), "another rustc\n").unwrap();
    run_doc_in(&dir, variables)
}

/// Writes the script to `cargo` and runs `command`, with `ONE_AT_A_TIME`
/// held from before the write until the command ends. No step under the lock
/// can panic: a step that fails is returned as an error. So no test poisons
/// the lock for the tests that follow.
fn run_in_turn(cargo: &Path, command: &mut Command) -> io::Result<Output> {
    use std::os::unix::fs::PermissionsExt as _;

    let _turn = ONE_AT_A_TIME.lock().expect("one at a time");
    fs::write(cargo, CARGO)?;
    fs::set_permissions(cargo, Permissions::from_mode(0o755))?;
    command.output()
}

/// Runs `iris-tools doc` in `dir` with the script first on the search path,
/// without the variables the command refuses or those that add a backtrace
/// to its error line, and then with `variables` set.
fn run_doc_in(dir: &TempDir, variables: &[(&str, &str)]) -> Run {
    let mut path = OsString::from(dir.path());
    path.push(":");
    path.push(env::var_os("PATH").unwrap_or_default());
    let mut command = Command::new(env!("CARGO_BIN_EXE_iris-tools"));
    command
        .arg("doc")
        .current_dir(dir.path())
        .env("PATH", path)
        .env_remove("CARGO_BUILD_TARGET")
        .env_remove("CARGO_ENCODED_RUSTDOCFLAGS")
        .env_remove("RUST_BACKTRACE")
        .env_remove("RUST_LIB_BACKTRACE")
        .envs(variables.iter().copied());
    let output = run_in_turn(&dir.path().join("cargo"), &mut command).unwrap();
    let calls = fs::read_to_string(dir.path().join("calls")).unwrap_or_default();
    Run {
        output,
        calls: calls.lines().map(str::to_owned).collect(),
        rustdocflags: fs::read_to_string(dir.path().join("rustdocflags")).unwrap_or_default(),
        output_directory: dir.path().join("target/doc"),
    }
}

#[test]
fn a_run_makes_the_three_cargo_calls_in_order_with_their_arguments() {
    let run = run_doc(&[]);
    assert_eq!(
        run.calls,
        [
            "metadata --format-version 1 --no-deps --locked",
            "check --workspace --locked",
            "doc --workspace --no-deps --locked --jobs 1",
        ]
    );
}

#[test]
fn the_documentation_pass_denies_warnings() {
    let run = run_doc(&[]);
    assert_eq!(run.rustdocflags, "-D warnings\n");
}

#[test]
fn a_run_whose_every_step_passes_exits_with_success() {
    let run = run_doc(&[]);
    assert_eq!(run.output.status.code(), Some(0));
}

#[test]
fn a_failed_check_exits_with_the_status_of_cargo() {
    let run = run_doc(&[("FAKE_CHECK", "3")]);
    assert_eq!(run.output.status.code(), Some(3));
}

#[test]
fn a_failed_documentation_pass_exits_with_the_status_of_cargo() {
    let run = run_doc(&[("FAKE_DOC", "7")]);
    assert_eq!(run.output.status.code(), Some(7));
}

#[test]
fn a_check_stopped_by_a_signal_exits_with_128_plus_the_signal() {
    let run = run_doc(&[("FAKE_CHECK", "kill")]);
    assert_eq!(run.output.status.code(), Some(128 + 9));
}

#[test]
fn a_check_stopped_by_a_signal_is_reported_in_one_line() {
    let run = run_doc(&[("FAKE_CHECK", "kill")]);
    assert_eq!(run.standard_error(), "doc: cargo was stopped by signal 9\n");
}

#[test]
fn a_check_stopped_by_a_signal_runs_no_documentation_pass() {
    let run = run_doc(&[("FAKE_CHECK", "kill")]);
    assert!(
        !run.calls.iter().any(|call| call.starts_with("doc ")),
        "cargo ran: {:?}",
        run.calls
    );
}

#[test]
fn a_documentation_pass_stopped_by_a_signal_exits_with_128_plus_the_signal() {
    let run = run_doc(&[("FAKE_DOC", "kill")]);
    assert_eq!(run.output.status.code(), Some(128 + 9));
}

#[test]
fn a_documentation_pass_stopped_by_a_signal_is_reported_in_one_line() {
    let run = run_doc(&[("FAKE_DOC", "kill")]);
    assert_eq!(run.standard_error(), "doc: cargo was stopped by signal 9\n");
}

#[test]
fn a_cargo_metadata_stopped_by_a_signal_exits_with_128_plus_the_signal() {
    let run = run_doc(&[("FAKE_METADATA", "kill")]);
    assert_eq!(run.output.status.code(), Some(128 + 9));
}

#[test]
fn a_cargo_metadata_stopped_by_a_signal_is_reported_in_one_line() {
    let run = run_doc(&[("FAKE_METADATA", "kill")]);
    assert_eq!(run.standard_error(), "doc: cargo was stopped by signal 9\n");
}

#[test]
fn a_passed_documentation_pass_that_writes_no_index_page_fails() {
    let run = run_doc(&[("FAKE_PAGES", "none")]);
    let text = run.standard_error();
    let expected = format!(
        "the library iris_a has no index.html in {};",
        run.output_directory.display()
    );
    assert!(
        run.output.status.code() == Some(1) && text.contains(&expected),
        "{text}"
    );
}

#[test]
fn a_second_documentation_pass_is_announced_in_one_line() {
    let run = run_doc_after_another_rustc(&[]);
    assert_eq!(
        run.standard_error(),
        format!(
            "doc: Cargo rewrote the rustdoc fingerprint and emptied {}; documenting again\n",
            run.output_directory.display()
        )
    );
}

#[test]
fn an_output_directory_emptied_after_a_failed_pass_is_announced_in_one_line() {
    let run = run_doc_after_another_rustc(&[("FAKE_DOC", "7")]);
    assert_eq!(
        run.standard_error(),
        format!(
            "doc: Cargo rewrote the rustdoc fingerprint and emptied {} before the pass that \
             failed; removed the pages of that pass\n",
            run.output_directory.display()
        )
    );
}

#[test]
fn a_build_target_in_the_environment_is_refused() {
    let run = run_doc(&[("CARGO_BUILD_TARGET", "aarch64-unknown-linux-gnu")]);
    assert_eq!(
        (run.output.status.code(), run.standard_error().as_str()),
        (Some(1), BUILD_TARGET_REFUSED)
    );
}

#[test]
fn an_empty_build_target_in_the_environment_is_refused() {
    let run = run_doc(&[("CARGO_BUILD_TARGET", "")]);
    assert_eq!(
        (run.output.status.code(), run.standard_error().as_str()),
        (Some(1), BUILD_TARGET_REFUSED)
    );
}

#[test]
fn a_refused_build_target_runs_no_cargo() {
    let run = run_doc(&[("CARGO_BUILD_TARGET", "aarch64-unknown-linux-gnu")]);
    assert!(run.calls.is_empty(), "cargo ran: {:?}", run.calls);
}

#[test]
fn encoded_rustdoc_flags_in_the_environment_are_refused() {
    let run = run_doc(&[("CARGO_ENCODED_RUSTDOCFLAGS", "--cap-lints=warn")]);
    assert_eq!(
        (run.output.status.code(), run.standard_error().as_str()),
        (Some(1), ENCODED_RUSTDOCFLAGS_REFUSED)
    );
}

#[test]
fn empty_encoded_rustdoc_flags_in_the_environment_are_refused() {
    let run = run_doc(&[("CARGO_ENCODED_RUSTDOCFLAGS", "")]);
    assert_eq!(
        (run.output.status.code(), run.standard_error().as_str()),
        (Some(1), ENCODED_RUSTDOCFLAGS_REFUSED)
    );
}

#[test]
fn refused_encoded_rustdoc_flags_run_no_cargo() {
    let run = run_doc(&[("CARGO_ENCODED_RUSTDOCFLAGS", "--cap-lints=warn")]);
    assert!(run.calls.is_empty(), "cargo ran: {:?}", run.calls);
}
