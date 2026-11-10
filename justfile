# Common tasks. Run `just` to list them.

set shell := ["bash", "-euo", "pipefail", "-c"]

default:
    @just --list

# Format all crates.
fmt:
    cargo fmt --all

# Verify formatting, lints (warnings are errors), docs, tests, and the vendored crates' tests.
check: fmt-check clippy test doc vendor-test

# Lint the fuzz targets with the pinned nightly; `just fuzz-build` builds them.
fuzz-clippy:
    cd fuzz && cargo clippy --locked --bins -- -D warnings

fmt-check:
    cargo fmt --all -- --check
    cargo fmt --manifest-path fuzz/Cargo.toml --all -- --check

clippy:
    cargo clippy --workspace --all-targets --locked -- -D warnings

test:
    cargo test --workspace --locked

doc:
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked

# On a machine whose cargo cache lacks these dependencies, run
# `cargo fetch --locked --manifest-path <manifest>` before this recipe.
#
# Test the vendored crates offline through their own manifests and lockfiles.
vendor-test:
    cargo test --offline --locked --manifest-path third_party/oxvif/Cargo.toml
    cargo test --offline --locked --manifest-path third_party/retina/Cargo.toml

# Supply-chain checks of every lockfile (install with `cargo install cargo-audit cargo-deny`).
audit:
    cargo audit
    cargo audit --file fuzz/Cargo.lock
    cargo audit --file third_party/oxvif/Cargo.lock
    cargo audit --file third_party/retina/Cargo.lock
    cargo audit --file .rust-reference/tools/reference-maintenance/Cargo.lock
    cargo deny check
    cargo deny --manifest-path fuzz/Cargo.toml check

# Build every fuzz target with the nightly toolchain pinned in fuzz/rust-toolchain.toml.
fuzz-build:
    cd fuzz && cargo fuzz build

# Mutation-test the given source files with cargo-mutants (`cargo install cargo-mutants`):
# a mutant that survives is logic no test pins down. Runs on a copy of the committed tree,
# because cargo-mutants copies the tree it runs in once per worker, build directories
# included. The lists of caught and missed mutants land in target/mutants/mutants.out, and
# the recipe fails when a mutant is missed.
mutants +files:
    #!/usr/bin/env bash
    set -euo pipefail
    copy=$(mktemp -d)
    trap 'rm -rf "$copy"' EXIT
    git archive HEAD | tar -x -C "$copy"
    out="$PWD/target/mutants"
    cd "$copy"
    cargo mutants -j 2 -o "$out" $(printf -- '-f %s ' {{files}})

# Run the tests of the crates without C dependencies as 64-bit ARM code under user-mode
# emulation, in release mode because emulated debug code is too slow for the timing tests.
# Needs `rustup target add aarch64-unknown-linux-musl` and qemu-aarch64-static.
cross-test:
    CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUNNER=qemu-aarch64-static cargo test --release --locked --target aarch64-unknown-linux-musl -p iris-core -p iris-h265 -p iris-mp4 -p iris-store -p iris-ingest -p iris-replay

# Lint every crate but the GUI (which needs a GTK sysroot) for 64-bit ARM. Needs
# `rustup target add aarch64-unknown-linux-gnu` and a C cross compiler named by
# CC_aarch64_unknown_linux_gnu; CFLAGS_aarch64_unknown_linux_gnu may add a --sysroot.
cross-check target="aarch64-unknown-linux-gnu":
    cargo clippy --workspace --exclude iris-gui --all-targets --locked --target {{target}} -- -D warnings

# Release build of the daemon.
build:
    cargo build --release -p irisd

# Record the configured cameras for a given duration (example: just record 10m).
record duration="2m" root="data/recordings":
    cargo run --release -p irisd -- record --config config/cameras.local.toml --root {{root}} --duration {{duration}}

# Verify the reference pack's snapshots against its manifest and run the maintenance tool's tests.
references-check:
    cargo run --locked --manifest-path .rust-reference/tools/reference-maintenance/Cargo.toml -- sync --check
    cargo test --locked --manifest-path .rust-reference/tools/reference-maintenance/Cargo.toml
