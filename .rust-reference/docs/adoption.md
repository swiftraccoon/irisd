# Adopt and maintain the reference

## Copy a self-contained pack

Run this command from the reference repository root, with an existing Rust
project as the destination:

```sh
cargo run --locked --manifest-path tools/reference-maintenance/Cargo.toml -- export /absolute/path/to/rust-project
```

It creates `.rust-reference/` in the project. It includes authored guidance,
upstream snapshots and provenance, templates, and the maintenance tools. It omits
Git metadata, build output, and caches, verifies the copied snapshots, and refuses
to replace an existing `.rust-reference/`. The pack includes the optional Rust
maintenance crate and lockfile under `tools/reference-maintenance/`, with an
independent Cargo workspace boundary. Reading the guides requires no Cargo.
The pack's root `.gitattributes` stops Git from converting line endings under
`references/`, so snapshot hashes verify on Windows checkouts, including those
made with `core.autocrlf=true`. Keep that file with the pack.

Authored guidance, templates, and tooling use [0BSD](../LICENSE), which permits
reuse without attribution or notice retention. Exports include `LICENSE`.
Upstream snapshots retain their own licenses, including the exceptions in the
[source license inventory](../references/README.md#licenses-and-attribution).
Preserve those upstream notices when copying or modifying the pack.

Merge `.rust-reference/templates/AGENTS.md` into the project's root `AGENTS.md`.
If the project has no `AGENTS.md`, copy the template there. A nested reference
pack alone does not instruct agents working elsewhere in the project to read it.
Preserve any existing project instructions.

Commit the pack to the consuming repository when every checkout needs offline
access. The exporter warns when the project's Git ignore rules would exclude
part of the pack; adjust `.gitignore` before committing so the committed pack
is complete. For a publishable crate, add `exclude = [".rust-reference/"]` to
its `[package]` table so the pack stays out of the published package.
Otherwise, keep the reference workspace in a shared local checkout and
record its absolute path in the consuming project's `AGENTS.md`, for example:

```markdown
## Rust references

Read `/absolute/path/to/rust-proj-init/AGENTS.md` and its linked local policies.
Use `/absolute/path/to/rust-proj-init/references/README.md` to find full guides.
This project's instructions, toolchain, and compatibility promises take precedence.
```

The shared-checkout approach depends on that path being available. A copied pack
has no such dependency. Keep the source directories and manifest together when
copying by other means. Preserve hidden upstream files and license notices;
copying only a README or a list of links does not carry the reference material.

## Establish the consuming project's decisions

Record these in its Cargo manifests, toolchain configuration, CI, and project
instructions as appropriate:

- Application, library, or both; intended users and supported platforms.
- Rust edition, formatting style edition, pinned development toolchain, and the
  minimum supported Rust version (MSRV). Development toolchain and MSRV can differ.
- Public compatibility surface, initial version, release process, and pre-1.0 policy.
- Supported default and optional feature combinations and test commands.
- Dependency, runtime, unsafe-code, and lint policies; explain justified exceptions.

Do not infer a compiler version or runtime from a vendored guide. Use the
project's own decisions when validating it.

## Refresh upstream sources

From either the original reference root or a copied pack's `.rust-reference/`
directory:

```sh
cargo run --locked --manifest-path tools/reference-maintenance/Cargo.toml -- sync --check
cargo run --locked --manifest-path tools/reference-maintenance/Cargo.toml -- sync
cargo run --locked --manifest-path tools/reference-maintenance/Cargo.toml -- sync --check
```

Pass source IDs to limit an operation:

```sh
cargo run --locked --manifest-path tools/reference-maintenance/Cargo.toml -- sync rust-style-guide google-styleguide
cargo run --locked --manifest-path tools/reference-maintenance/Cargo.toml -- sync --check conventional-commits semver
```

The source IDs are `rust-style-guide`, `rust-api-guidelines`, `google-styleguide`,
`google-timeless-documentation`, `conventional-commits`, and `semver`.
The Google Rust guide refreshes with `google-styleguide`.

The Rust CLI maintains ignored Git caches under `.cache/upstreams/`, fetches
upstream revisions, stages snapshots, and records the selected commits and
file hashes in `references/manifest.json`. It protects locally modified managed
snapshots; move intended policy changes into `docs/` before replacing source
files. Use `--force` only when you intend to discard changes in those snapshots.
Desktop metadata files such as `.DS_Store` are ignored by the checks and skipped
by the exporter. A selected path that no longer exists upstream fails the
refresh instead of silently shrinking the snapshot.

`sync --check` also compares each manifest entry's kind, URL, branch, selection,
and license with the source definitions compiled into the tool. After changing
a source's kind, URL, branch, or selection in
`tools/reference-maintenance/src/sources.rs`, refresh that source so the manifest
records the new values. A locked restore retains those recorded fields and the
pinned revision; it updates the license description from the tool's definition.
For a change to the license description alone, `sync --locked SOURCE` records
the corrected description without accepting different upstream content.

The CLI rejects unsupported source IDs in the manifest. When deliberately
removing a source definition, review and remove its manifest entry and snapshot
directory together.

Review the manifest and source diffs, then reconcile adopted policies and run:

```sh
cargo +1.96.0 run --locked --manifest-path tools/reference-maintenance/Cargo.toml -- sync --check
cargo +1.96.0 fmt --manifest-path tools/reference-maintenance/Cargo.toml --all -- --check
cargo +1.96.0 clippy --locked --manifest-path tools/reference-maintenance/Cargo.toml --all-targets -- -D warnings
cargo +1.96.0 test --locked --manifest-path tools/reference-maintenance/Cargo.toml
git diff --check
```

A refreshed upstream guide is evidence to evaluate. Updating it does not change
the consuming project's edition, dependency choices, or compatibility commitments.
The full source text may contain historical or environment-specific advice; use
the appropriate section in context.

## Restore recorded revisions

```sh
cargo run --locked --manifest-path tools/reference-maintenance/Cargo.toml -- sync --locked
```

For each source, `--locked` compares the snapshot with its manifest entry. A
snapshot that already matches is reported as unchanged and left alone, without
network access. Missing files and directories are restored from the cached
objects, or by fetching the recorded commit by hash when no cache exists, so a
renamed upstream branch does not block a restore. Modified or added files are
local work: `--locked` refuses to discard them unless `--force` is given, which
also re-exports matching snapshots and rewrites their manifest entries. A web
page has no Git revision; its exact HTML must already exist in the local cache
or intact snapshot to be restored byte for byte. Fetching the live page is a
refresh, not a reproducible restore. The committed snapshots themselves are the
portable offline record.

From the consuming project's directory, select the copied crate explicitly:

```sh
cargo run --locked --manifest-path .rust-reference/tools/reference-maintenance/Cargo.toml -- sync --check
```

The CLI requires Rust 1.88+ and Cargo. Build dependencies must be downloaded once;
after building, use `tools/reference-maintenance/target/debug/rust-project-reference`
directly from the pack root for checks and exports without network access. Its
default reference root is two directories above the maintenance manifest used
to build it; pass `--root /path/to/reference-pack` before the command to select
another pack. Fetching upstream changes still needs network access and Git, and
the web page is downloaded with the system `curl`.
Project-owned maintenance tools and tests use Rust. Maintenance development uses
Rust 1.96.0, pinned in
`tools/reference-maintenance/rust-toolchain.toml`; the minimum supported version
is Rust 1.88. Run Cargo from that crate's directory to select the pin
automatically, or use `cargo +1.96.0` from the pack root. `--manifest-path`
selects a Cargo manifest without selecting its directory's Rustup toolchain.
The nested pin does not select a toolchain for the consuming project.

To update an existing copied pack from the central workspace, first review any
local changes in that pack. Export into a temporary project directory, compare
the two copies, and replace or merge deliberately. The exporter never overwrites
an existing pack, and refreshing only updates upstream snapshots, not our policies.
