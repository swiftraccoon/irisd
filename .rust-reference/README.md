# Rust project reference

A reusable reference library for Rust projects. It includes project
expectations, complete local source documents, and templates. Read the guides
offline; agents can copy them or fetch upstream changes when needed.

The repository root holds the reference library. Optional Rust maintenance
tooling lives in [`tools/reference-maintenance/`](tools/reference-maintenance/)
as an independent Cargo workspace. Reading the references requires no Cargo.

The authored guidance, templates, and maintenance tooling use the
[BSD Zero Clause License (0BSD)](LICENSE), which permits reuse without
attribution or notice retention. The upstream snapshots in `references/`
retain their own licenses and file-specific notices; see the
[license inventory](references/README.md#licenses-and-attribution).
Exports include the root license and upstream notices.

Start with [AGENTS.md](AGENTS.md). The working policies are:

- [Rust expectations](docs/rust-expectations.md): design, style, errors, safety,
  dependencies, and validation.
- [Documentation](docs/documentation.md): rustdoc and timeless product writing.
- [Commits and releases](docs/commits-and-releases.md): Conventional Commits and
  Semantic Versioning, including Rust compatibility considerations.
- [Adoption and maintenance](docs/adoption.md): use this pack in another project.

## Local sources

| Source | Local entry point |
| --- | --- |
| Rust Style Guide | [Book source](references/rust-style-guide/src/doc/style-guide/src/README.md) |
| Rust API Guidelines | [Chapter index](references/rust-api-guidelines/src/SUMMARY.md) |
| Google styleguide repository | [Repository README](references/google-styleguide/README.md) |
| Google Rust Style Guide | [Rust guide source](references/google-styleguide/rustguide.md) |
| Google timeless documentation | [Readable snapshot](references/google-timeless-documentation/timeless-documentation.md) |
| Conventional Commits 1.0.0 | [English specification](references/conventional-commits/content/v1.0.0/index.md) |
| Semantic Versioning 2.0.0 | [Specification](references/semver/spec/v2.0.0.md) |

The Google repository includes the source for its rendered Rust guide. The Rust
compiler repository is limited to its style-guide subtree and license notices;
the other Git sources contain their tracked repository files. The timeless
documentation snapshot includes the original HTML and a readable Markdown copy.
SemVer's tracked symlink is stored as a regular copy of its target; this
transformation is recorded in the manifest so the pack needs no symlinks.
See [source provenance and licenses](references/README.md).

## Copy into a Rust project

From the reference repository root, run:

```sh
cargo run --locked --manifest-path tools/reference-maintenance/Cargo.toml -- export /absolute/path/to/rust-project
```

This creates `rust-project/.rust-reference/` containing a self-contained copy,
without Git metadata or caches. Merge the provided
[AGENTS.md fragment](templates/AGENTS.md) into that project's root `AGENTS.md` so
future agents know to use the reference. The exporter leaves existing project
instructions untouched, refuses to overwrite an existing reference pack, and
warns when the project's Git ignore rules would drop part of the pack. The
copied `.gitattributes` keeps snapshot bytes exact on every platform.

You can also keep this repository beside your projects and add its absolute path
to their agent instructions. [Adoption examples](docs/adoption.md) cover both approaches.

## Verify or refresh

Maintenance tool requirements: Rust 1.88+ and Cargo; Git, curl, and network
access for upstream updates. Project-owned tools and tests are Rust. The first
build downloads Cargo dependencies; the reference documents themselves are
readable without any tools.

Maintenance development uses Rust 1.96.0, pinned in
[`tools/reference-maintenance/rust-toolchain.toml`](tools/reference-maintenance/rust-toolchain.toml).
Run Cargo from that directory to select it automatically, or use
`cargo +1.96.0` from the pack root. The nested pin leaves the consuming project's
toolchain selection untouched. CI tests Rust 1.88 and 1.96 on Linux, macOS, and
Windows; formatting, Clippy, and documentation checks use 1.96.

```sh
# Validate the checked-in snapshots offline.
cargo run --locked --manifest-path tools/reference-maintenance/Cargo.toml -- sync --check

# Fetch upstream changes and replace the snapshots; then review the diff.
cargo run --locked --manifest-path tools/reference-maintenance/Cargo.toml -- sync

# Refresh only selected sources.
cargo run --locked --manifest-path tools/reference-maintenance/Cargo.toml -- sync rust-api-guidelines conventional-commits semver

# Restore the exact revisions recorded in the manifest; matching snapshots stay unchanged.
cargo run --locked --manifest-path tools/reference-maintenance/Cargo.toml -- sync --locked

# Test the maintenance tools without network access.
cargo test --locked --manifest-path tools/reference-maintenance/Cargo.toml
```

[references/manifest.json](references/manifest.json) records source URLs, revisions,
retrieval times, selections, and SHA-256 hashes. Fetchable Git caches live in
ignored `.cache/upstreams/`; reading or copying the references does not depend on
those caches. No submodule initialization is required.

After building, `tools/reference-maintenance/target/debug/rust-project-reference`
can run `sync --check` and `export` offline. Use `--root /path/to/reference-pack`
before the command to select a different pack; the default is the reference root,
two directories above the maintenance crate's manifest used to build the tool.

The nested maintenance crate has its own workspace boundary, so copying the pack
inside another Cargo workspace does not change that project's members. The
consuming project chooses its crate layout, toolchain, MSRV, runtime, and
dependencies.
