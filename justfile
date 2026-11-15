# Common tasks. Run `just` to list them.

set shell := ["bash", "-euo", "pipefail", "-c"]

# A gitignored `.env` beside this file sets what differs from one machine to the next, such
# as the C cross compiler and sysroot that `cross-check` names.
set dotenv-load := true

default:
    @just --list

# Format all crates.
fmt:
    cargo fmt --all

# Verify formatting, lints (warnings are errors), docs, tests, the vendored crates' tests, and
# the mechanical rules of STANDARDS.md.
check: fmt-check clippy test doc vendor-test standards

# Everything the CI workflow runs (`.github/workflows/check.yml`), and the tests once more
# with their temporary files on ext4, which is what a CI runner's are on. Run before a push.
ci: check standards-commits msrv audit fuzz-clippy fuzz-build cross-check cross-test references-check ext4-test

# Build and test on the minimum supported Rust version, in a target directory of its own.
msrv:
    CARGO_TARGET_DIR=target/msrv cargo +1.92.0 test --workspace --locked

# Run the tests with their temporary directory on a scratch ext4 image, which reuses a freed
# inode number at once and holds files a tmpfs does not. Needs `mkfs.ext4` and root to mount
# the image (`sudo -n`); it fails where it cannot mount, and does not pass by skipping.
ext4-test:
    #!/usr/bin/env bash
    set -euo pipefail
    image="$PWD/target/ext4-test.img"
    mount="$PWD/target/ext4-test"
    mkdir -p "$mount"
    rm -f "$image"
    truncate -s 8G "$image"
    mkfs.ext4 -q -F "$image"
    sudo -n mount -o loop "$image" "$mount"
    trap 'sudo -n umount "$mount"; rm -f "$image"' EXIT
    sudo -n chown "$(id -u):$(id -g)" "$mount"
    TMPDIR="$mount" cargo test --workspace --locked

# Check every file git tracks or would track against the mechanical rules of STANDARDS.md and
# AGENTS.md. `standards.toml` holds the accepted exceptions.
standards:
    cargo run --quiet --locked -p iris-tools -- standards tree

# Check the commits of a range the same way: the form of each message, and what each says of
# its change.
standards-commits range="origin/main..HEAD":
    cargo run --quiet --locked -p iris-tools -- standards commits {{range}}

# Make git run the checks in this clone: the tree before a commit, the message as it is made.
# The hooks are written into the clone's git directory; the tree holds no executable file.
hooks:
    cargo run --quiet --locked -p iris-tools -- standards hooks

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

# The viewer's checks that need a display, which `just check` leaves out: on a virtual one
# (`xvfb-run`), with ffmpeg's libx264 and the GStreamer HLS and H.264 plugins. Run after any
# change to the viewer.
display-test:
    xvfb-run -a cargo test --locked -p iris-gui --bin iris-gui -- --ignored --test-threads=1
    cargo test --locked -p iris-gui --test playback_origin -- --ignored
    cargo test --locked -p iris-gui --test playback_snapshot -- --ignored

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

# Run the tests of the crates without C dependencies as 64-bit ARM code: natively on a
# 64-bit ARM host, and elsewhere under user-mode emulation, in release mode because emulated
# debug code is too slow for the timing tests. Needs `rustup target add
# aarch64-unknown-linux-musl` and, off ARM, a qemu-aarch64-static that implements openat2,
# which the store's tests call.
cross-test:
    #!/usr/bin/env bash
    set -euo pipefail
    export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld
    if [ "$(uname -m)" != aarch64 ]; then
        export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUNNER=qemu-aarch64-static
    fi
    cargo test --release --locked --target aarch64-unknown-linux-musl -p iris-core -p iris-h265 -p iris-mp4 -p iris-store -p iris-ingest -p iris-replay

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
