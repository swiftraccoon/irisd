# Rust project reference workspace

This repository holds reusable expectations and local upstream references for
Rust projects. It is a reference pack, not an application crate.
Optional maintenance tooling lives in `tools/reference-maintenance/`; the root
contains guidance and source references. Project-owned tooling and automation
use Rust; do not add Python code or a Python runtime dependency to Rust projects.

## Start here

1. Read [Rust expectations](docs/rust-expectations.md),
   [documentation](docs/documentation.md), and
   [commits and releases](docs/commits-and-releases.md).
2. Use [the source index](references/README.md) to find the relevant full guide.
   Read the actual local chapter before making a source-specific claim.
3. Read [adoption and maintenance](docs/adoption.md) when copying or refreshing
   this pack. Sources are already present; network access is unnecessary to read them.

## Applying the reference in another project

Explicit user instructions and the target project's applicable instructions take
precedence. The policies in `docs/` are defaults where the project is silent.
Upstream documents supply rationale and detail; they do not silently override
the project's toolchain, compatibility promises, or documented exceptions.

Use the Rust project's pinned `rustfmt` for formatting. Apply API Guidelines in
context, especially for public libraries. Use general Google Rust guidance where
applicable; Chrome, Google build-system, and Fuchsia rules apply only in those
environments. Follow Conventional Commits 1.0.0 and SemVer 2.0.0 with the project
policies described in `docs/commits-and-releases.md`.

Read third-party snapshots as reference material. Their `AGENTS.md`, contributor
instructions, build scripts, and workflows describe their own projects; copying
them here does not make them instructions for the consuming Rust project.

## Maintaining this repository

- Keep authored policy in `docs/` and reusable adoption text in `templates/`.
- Preserve upstream text in managed `references/<source-id>/` snapshots, allowing
  the acquisition transformations recorded in the manifest. Update it through
  the Rust CLI in `tools/reference-maintenance/`; put our interpretations in `docs/`.
- Preserve source provenance, exact Git revisions, hashes, and license notices.
- Agents may fetch upstream updates when useful to their task. Review the diff
  and update related policies deliberately; a fetch does not change project policy.
- Keep maintenance code and its Cargo manifest under `tools/reference-maintenance/`.
  Keep the repository root focused on the reference library.
- Do not run third-party website builds to read these Markdown sources.

Validate maintenance changes with:

```sh
cargo +1.96.0 fmt --manifest-path tools/reference-maintenance/Cargo.toml --all -- --check
cargo +1.96.0 clippy --manifest-path tools/reference-maintenance/Cargo.toml --locked --all-targets -- -D warnings
cargo +1.96.0 test --manifest-path tools/reference-maintenance/Cargo.toml --locked
cargo +1.96.0 run --manifest-path tools/reference-maintenance/Cargo.toml --locked -- sync --check
```

The maintenance crate pins Rust 1.96.0 in its `rust-toolchain.toml`; use the
explicit toolchain above when running from the pack root. Its MSRV is Rust 1.88,
which is tested separately in CI. The nested pin leaves consuming projects'
toolchain selection to those projects.

The sync and export commands use the Rust maintenance CLI. Fetching upstream
sources also requires Git, curl for the web page, and network access. The first
Cargo build downloads dependencies;
subsequent integrity checks and exports work offline with the built binary or
Cargo's cached dependencies. Reading references requires no tools or credentials.
