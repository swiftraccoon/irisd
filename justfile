# Common tasks. Run `just` to list them.

set shell := ["bash", "-euo", "pipefail", "-c"]

default:
    @just --list

# Format all crates.
fmt:
    cargo fmt --all

# Verify formatting, lints (warnings are errors), docs, and tests.
check: fmt-check clippy test doc

fmt-check:
    cargo fmt --all -- --check

clippy:
    cargo clippy --workspace --all-targets --locked -- -D warnings

test:
    cargo test --workspace --locked

doc:
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked

# Supply-chain checks (install with `cargo install cargo-audit cargo-deny`).
audit:
    cargo audit
    cargo deny check

# Release build of the daemon.
build:
    cargo build --release -p irisd

# Record the configured cameras for a given duration (example: just record 10m).
record duration="2m" root="data/recordings":
    cargo run --release -p irisd -- record --config config/cameras.local.toml --root {{root}} --duration {{duration}}

# Verify the reference pack's snapshots against its manifest.
references-check:
    cargo run --locked --manifest-path .rust-reference/tools/reference-maintenance/Cargo.toml -- sync --check
