# Engineering standards

irisd follows six published standards. Verbatim copies live in the reference pack under
`.rust-reference/references/` (provenance, hashes, and licenses in
`.rust-reference/references/README.md` and `manifest.json`; the pack's own policies in
`.rust-reference/docs/`), and the IETF documents the protocols come from under
`docs/standards/ietf/`. This file is the digest of what we adopt and how it is enforced.
Where the sources conflict, the order of precedence is:

1. The Rust Style Guide (`.rust-reference/references/rust-style-guide/src/doc/style-guide/src/`)
   for formatting. rustfmt enforces it; `rustfmt.toml` sets `style_edition = "2024"` and the
   stable options below.
2. The Rust API Guidelines (`.rust-reference/references/rust-api-guidelines/src/`). Binding
   for the library crates (`iris-*`); a strong recommendation for the `irisd` binary, where
   simplicity and directness win over generality.
3. The Google Rust style guide (`.rust-reference/references/google-styleguide/rustguide.md`)
   for everything the first two leave open: imports, module layout, ownership, error
   reporting, documentation, and implementation habits.
4. Google's timeless-documentation guidance
   (`.rust-reference/references/google-timeless-documentation/timeless-documentation.md`)
   for all prose: READMEs, design documents, rustdoc, and CLI help.
5. Conventional Commits 1.0.0
   (`.rust-reference/references/conventional-commits/content/v1.0.0/index.md`) for commit
   messages, with the house style in §9.
6. Semantic Versioning 2.0.0 (`.rust-reference/references/semver/spec/v2.0.0.md`) for
   published versions, with the alpha policy in §9.

## 1. Enforcement

| Check | Command | Status |
|---|---|---|
| Formatting | `cargo fmt --all -- --check` | required |
| Lints | `cargo clippy --workspace --all-targets -- -D warnings` with `clippy::all` and `clippy::pedantic` enabled in `Cargo.toml` (`[workspace.lints]`) | required |
| Unsafe code | `#![forbid(unsafe_code)]` workspace-wide (`unsafe_code = "forbid"`) | required |
| Public docs | `missing_docs`, and clippy's `missing_errors_doc` and `missing_panics_doc` (pedantic); `cargo doc` with `RUSTDOCFLAGS=-D warnings` | required |
| Tests | `cargo test --workspace` and the vendored crates' suites (`just vendor-test`) | required |
| Supply chain | `cargo audit` on every lockfile; `cargo deny check` (`deny.toml`) for the root and `fuzz/` graphs (`just audit`) | required before a release |
| Fuzz build | `just fuzz-build` (the nightly pinned in `fuzz/rust-toolchain.toml`) | CI |
| 64-bit ARM portability | `just cross-check` (clippy with warnings as errors on every crate but the GUI for `aarch64-unknown-linux-gnu`) | CI |
| MSRV | `cargo +1.92.0 test --workspace --locked` (`rust-version` in `Cargo.toml`) | CI |
| Reference pack | `just references-check` (the snapshots match `manifest.json`) | CI |

`just check` runs the required set. The `check`, cross-check, and fuzz jobs and the docs
workflow treat warnings as errors; the MSRV job builds and tests without denying them, because
lints differ between toolchains.

`Cargo.toml` allows five pedantic lints across the workspace, each with its reason beside it:
`cast_possible_truncation`, `cast_possible_wrap`, `cast_precision_loss`, `cast_sign_loss`, and
`module_name_repetitions`. Any other exception is an `#[expect(lint, reason = "…")]` on the
smallest item that needs it. `clippy.toml` raises `too-many-arguments-threshold` to 8 (default
7) and `type-complexity-threshold` to 300 (default 250).

## 2. Formatting (Rust Style Guide)

- rustfmt with `style_edition = "2024"`, `max_width = 100`, `use_field_init_shorthand`,
  `use_try_shorthand`, Unix newlines. Never `#[rustfmt::skip]` for taste; only for
  deliberate alignment such as matrices or box layouts.
- Options the Google guide recommends but that stable rustfmt does not support
  (`imports_granularity = "Module"`, `hex_literal_case = "Lower"`,
  `format_macro_matchers`) are followed by hand and checked in review: one `use` per module
  (no `::` inside braces), lowercase hex literals.
- `Cargo.toml` follows
  `.rust-reference/references/rust-style-guide/src/doc/style-guide/src/cargo.md`: `[package]`
  first, with `name` and `version` leading and `description` last; keys version-sorted within
  every other section; a table that does not fit on its key's line moved to a section of its
  own; workspace inheritance for shared fields.

## 3. Naming (API Guidelines C-CASE, C-CONV, C-GETTER, C-ITER, C-WORD-ORDER; Google)

- RFC 430 casing. Acronyms are words: `RtspSession`, `NalType`, `Mp4Writer`, `HlsPlaylist`.
- Conversions use `as_` (cheap borrow), `to_` (expensive), `into_` (consuming) with the
  matching `self` form. Getters have no `get_` prefix; `foo()`, `foo_mut()`, `set_foo()`,
  `take_foo()`, `replace_foo()`, `with_foo()` as in the Google guide.
- Constructors: `new`, `new_with_x` when there are several, `try_new` when fallible,
  `from_x` paired with a `From` impl when it converts one value (a constructor that takes
  several inputs, such as `IndexWriter::from_claimed_file`, is not a conversion). Iterators:
  `iter`, `iter_mut`, `into_iter`.
- Extension traits end in `Ext`. Lifetimes and generic parameters get descriptive names
  unless single letters express an obvious bound (`T`, `R: Read`).
- No abbreviations outsiders would not know. Domain acronyms with a Wikipedia page are fine
  (`Rtp`, `Sps`, `Idr`, `Hls`); `cfg`, `ctx`, `buf`, `len`, `idx` are accepted Rust idiom.

## 4. Code organization (Google)

- Small crates with one concern; a leaf module and its file are named after the primary
  item it defines; non-leaf modules only declare and re-export.
- No glob imports except `use super::*` in `mod tests` and, in `iris-gui` alone, the GTK and
  GStreamer preludes (`gtk::prelude::*`, `gst::prelude::*`): crate-encouraged preludes that the
  Google guide leaves to the reviewer's discretion. Types are imported and used unqualified. A
  free function is called qualified by its module (`mem::take`, `fs::read_dir`, `reader::scan`)
  unless its name carries its module's meaning on its own (`remove_segment_beneath`,
  `admit_recording_root`), which may be imported; a name that reads as generic at the call site
  (`scan`, `free_space`, `index_path`) is never imported.
- Prefer `&[T]`, `&str`, `&T`, `Option<&T>` parameters; return iterators from library code
  when callers only iterate; application code may return `Vec`.
- Avoid `Rc`/`Arc` unless the alternative is unreasonably complex. Documented exception:
  video payloads travel as `bytes::Bytes` so one socket read feeds the writer, the live ring,
  and every client without copying.
- Prefer enums over `bool` parameters; if a `bool` is used, name it at the call site with a
  comment.

## 5. Types and APIs (API Guidelines C-NEWTYPE, C-COMMON-TRAITS, C-SERDE, C-GOOD-ERR, C-BUILDER, C-VALIDATE, C-STRUCT-PRIVATE, C-DEBUG)

- Newtypes for units and identifiers (`WallUs`, `MediaTicks`, `CameraId`, `StreamKey`);
  never a bare integer for an instant or an id in a public signature. A duration is a newtype
  (`MediaTicks`, `std::time::Duration`) or an integer whose unit is in the parameter or field
  name (`micros`, `_ms`, `_secs`, `ticks`). Codec structs that mirror a box or record field by
  field (`iris-mp4`'s `Sample`, `FragmentHeader`, `Fragment`, `FragmentInfo`, `IndexHeader`,
  `IndexRecord`) carry the field's integer type with the unit in the field's doc: the box value
  is the datum, and the functions that write them take those values.
- Derive eagerly: `Debug` on every public type; `Clone`, `Copy`, `PartialEq`, `Eq`, `Hash`,
  `Default`, `PartialOrd`, `Ord` wherever semantically right; `Serialize`/`Deserialize` on
  configuration and data-transfer types.
- Errors: `thiserror` enums in libraries (`Error + Debug + Display + Send + Sync + 'static`,
  meaningful variants, sources preserved); `anyhow` only in the binary. `Option` for simple
  absence, `Result` for recoverable failures, `panic!` only for violated invariants.
- Public structs keep private fields, except passive data (reports, configuration, transfer
  types) whose fields carry no invariant; `#[non_exhaustive]` on public error enums and on
  enums of protocol values that can grow (a closed set such as a configuration mode stays
  exhaustive, so every consumer handles each value); `#[must_use]` on functions whose result
  must not be ignored; builders for anything with more than three optional inputs; functions
  validate arguments.
- A library error's `Display` is self-contained and may repeat its `#[source]`, because the
  daemon logs errors through `Display`; the source chain stays available to callers.
- Implement standard traits instead of ad hoc methods (`FromStr`, `Display`, `From`,
  `AsRef`, `Extend`, `FromIterator`); operator overloads only for arithmetic meaning.

## 6. Robustness

- `unsafe` is forbidden. If a need appears, it goes in its own crate with a `SAFETY:`
  comment and a benchmark that justifies it. The one such need, a `setsockopt` call, sits in
  the vendored retina beside the published crate's own two `unsafe` blocks
  (`third_party/README.md`).
- Release profile: `panic = "abort"`, `lto = "fat"`, `codegen-units = 1`.
- Parsers that face the network (RTSP, SDP, RTP, NAL headers, MP4 boxes, multipart event
  feeds, SOAP) never panic on input, bound every allocation by a declared maximum, and have a
  `cargo-fuzz` target seeded from captured data.
- `expect("<invariant>")` over `unwrap()`; pattern matching (`if let`, `let … else`) over
  check-then-unwrap; context strings on `assert!`; explicit types where inference would
  make a reader guess; no long side-effecting iterator chains; `format!`/`write!` for
  string building. Acquiring a mutex is the one place the message may name the lock
  instead (`expect("snapshot registry")`): poisoning is its only failure, and the release
  profile aborts before a lock can be poisoned.
- Never read a poisoned mutex; no `catch_unwind`; no code before `main`.

## 7. Documentation (API Guidelines C-CRATE-DOC, C-EXAMPLE, C-FAILURE, C-LINK, C-METADATA, C-RELNOTES; Google timeless documentation)

- Every crate has `//!` docs with a short example; every public item has `///` docs that
  add information beyond the name; errors, panics, and invariants are documented.
- Prose is timeless: do not write *currently, now, new, soon, latest, existing, eventually,
  old, future, presently, as of this writing, does not yet*. State what the software does.
  Dates and versions appear only where the record is inherently time-stamped: research
  measurements, the changelog, and benchmark results, and then as absolute dates or version
  numbers.
- Markdown prose wraps at 100 columns like code; tables, headings, and lines made of one link
  or one command are exempt.
- `Cargo.toml` carries description, license, repository; `CHANGELOG.md` records significant
  changes per release.
- Comments explain why, not what; `TODO` carries an owner or an issue reference.

## 8. Testing

- Unit tests live in `mod tests` with `use super::*`; golden tests use captured camera media
  kept outside git (the one committed capture,
  `crates/iris-ingest/tests/data/cam1-main-10min.idx`, is a timing-only sidecar: header and
  records, no payload or text); seeded property tests cover the timestamp regulariser, and the
  `index` fuzz target checks the record codec's round trip; chaos tests (SIGKILL, truncation,
  zero-filled tails) cover the store; fuzz targets cover parsers.
- A test asserts one behaviour and names it: `rejects_truncated_trun`, not `test1`.

## 9. Commits, versions, and changes

- Commit messages follow Conventional Commits 1.0.0: `type(scope): description`, with a
  lowercase type and scope, an imperative description without a trailing period and under
  72 characters, and a body that explains the reason and what was measured. `!` and a
  `BREAKING CHANGE:` footer describe incompatibility with a published compatibility promise;
  differences only from unpublished alpha history do not use them. Types: `feat`, `fix`,
  `docs`, `refactor`, `perf`, `test`, `build`, `ci`, `style`, `chore`, `revert`. Scopes are
  crate names without the `iris-` prefix (`ingest`, `store`, `api`, `gui`, `replay`, `mp4`,
  `catalog`, `events`, `core`, `h265`, `tls`, `tools`), `irisd` for the binary crate, or an
  area (`docs`, `ci`, `soak`).
- Keep each commit coherent; split independent changes. Commits are created through the
  GitHub API (`gh api graphql`, `createCommitOnBranch`) so GitHub signs them.
  `iris-tools gh-commit -m HEADLINE --body TEXT` does this for the difference between
  `origin/<branch>` and a clean local `HEAD`.
- Semantic Versioning governs published artifacts. The workspace remains version `0.1.0` while
  the initial alpha is unreleased. Until `1.0.0`, exported Rust APIs, configuration,
  command-line behavior, storage schemas and formats, HTTP and WebSocket protocols, supported
  platforms, and toolchains carry no compatibility promise. Prefer direct improvements to
  correctness, efficiency, readability, and simplicity over compatibility layers; update every
  project caller, fixture, and document in the same change. Retain a migration only when it
  protects useful operator data without compromising the design.
- `CHANGELOG.md` records differences between published releases. Before the first release it
  contains alpha status only, without `Added`, `Changed`, or `Fixed` sections. A published
  `0.y.z` entry records the delivered behavior and any operator-data migration that remains
  useful; `1.0.0` establishes the stable compatibility commitment and ordinary SemVer policy.
- Behaviour changes update the affected document in `docs/` in the same change.
