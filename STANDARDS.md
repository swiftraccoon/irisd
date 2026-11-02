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
   versions, with the pre-1.0 policy in §9.

## 1. Enforcement

| Check | Command | Status |
|---|---|---|
| Formatting | `cargo fmt --all -- --check` | required |
| Lints | `cargo clippy --workspace --all-targets -- -D warnings` with `clippy::all` and `clippy::pedantic` enabled in `Cargo.toml` (`[workspace.lints]`) | required |
| Unsafe code | `#![forbid(unsafe_code)]` workspace-wide (`unsafe_code = "forbid"`) | required |
| Public docs | `missing_docs` lint; `cargo doc` with `RUSTDOCFLAGS=-D warnings` | required |
| Tests | `cargo test --workspace` | required |
| Supply chain | `cargo audit`, `cargo deny check` (`deny.toml`) | required before a release |

`just check` runs the required set. CI treats warnings as errors.

## 2. Formatting (Rust Style Guide)

- rustfmt with `style_edition = "2024"`, `max_width = 100`, `use_field_init_shorthand`,
  `use_try_shorthand`, Unix newlines. Never `#[rustfmt::skip]` for taste; only for
  deliberate alignment such as matrices or box layouts.
- Options the Google guide recommends but that stable rustfmt does not support
  (`imports_granularity = "Module"`, `hex_literal_case = "Lower"`,
  `format_macro_matchers`) are followed by hand and checked in review: one `use` per module
  (no `::` inside braces), lowercase hex literals.
- `Cargo.toml` follows `.rust-reference/references/rust-style-guide/src/doc/style-guide/src/cargo.md`: `[package]` first, keys
  in the conventional order, dependencies sorted, workspace inheritance for shared fields.

## 3. Naming (API Guidelines C-CASE, C-CONV, C-GETTER, C-ITER, C-WORD-ORDER; Google)

- RFC 430 casing. Acronyms are words: `RtspSession`, `NalType`, `Mp4Writer`, `HlsPlaylist`.
- Conversions use `as_` (cheap borrow), `to_` (expensive), `into_` (consuming) with the
  matching `self` form. Getters have no `get_` prefix; `foo()`, `foo_mut()`, `set_foo()`,
  `take_foo()`, `replace_foo()`, `with_foo()` as in the Google guide.
- Constructors: `new`, `new_with_x` when there are several, `try_new` when fallible,
  `from_x` paired with a `From` impl. Iterators: `iter`, `iter_mut`, `into_iter`.
- Extension traits end in `Ext`. Lifetimes and generic parameters get descriptive names
  unless single letters express an obvious bound (`T`, `R: Read`).
- No abbreviations outsiders would not know. Domain acronyms with a Wikipedia page are fine
  (`Rtp`, `Sps`, `Idr`, `Hls`); `cfg`, `ctx`, `buf`, `len`, `idx` are accepted Rust idiom.

## 4. Code organization (Google)

- Small crates with one concern; a leaf module and its file are named after the primary
  item it defines; non-leaf modules only declare and re-export.
- No glob imports except `use super::*` in `mod tests`. Types are imported and used
  unqualified; free functions are called qualified by module (`mem::take`, `fs::read_dir`).
- Prefer `&[T]`, `&str`, `&T`, `Option<&T>` parameters; return iterators from library code
  when callers only iterate; application code may return `Vec`.
- Avoid `Rc`/`Arc` unless the alternative is unreasonably complex. Documented exception:
  video payloads travel as `bytes::Bytes` so one socket read feeds the writer, the live ring,
  and every client without copying.
- Prefer enums over `bool` parameters; if a `bool` is used, name it at the call site with a
  comment.

## 5. Types and APIs (API Guidelines C-NEWTYPE, C-COMMON-TRAITS, C-SERDE, C-GOOD-ERR, C-BUILDER, C-VALIDATE, C-STRUCT-PRIVATE, C-DEBUG)

- Newtypes for units and identifiers (`WallUs`, `MediaTicks`, `CameraId`, `StreamId`);
  never bare `i64` for a time or an id in a public signature.
- Derive eagerly: `Debug` on every public type; `Clone`, `Copy`, `PartialEq`, `Eq`, `Hash`,
  `Default`, `PartialOrd`, `Ord` wherever semantically right; `Serialize`/`Deserialize` on
  configuration and data-transfer types.
- Errors: `thiserror` enums in libraries (`Error + Debug + Display + Send + Sync + 'static`,
  meaningful variants, sources preserved); `anyhow` only in the binary. `Option` for simple
  absence, `Result` for recoverable failures, `panic!` only for violated invariants.
- Public structs keep private fields; `#[non_exhaustive]` on public enums and structs that
  may grow; `#[must_use]` on functions whose result must not be ignored; builders for
  anything with more than three optional inputs; functions validate arguments.
- Implement standard traits instead of ad hoc methods (`FromStr`, `Display`, `From`,
  `AsRef`, `Extend`, `FromIterator`); operator overloads only for arithmetic meaning.

## 6. Robustness

- `unsafe` is forbidden. If a need appears, it goes in its own crate with a `SAFETY:`
  comment and a benchmark that justifies it.
- Release profile: `panic = "abort"`, `lto = "fat"`, `codegen-units = 1`.
- Parsers that face the network (RTSP, SDP, RTP, NAL headers, MP4 boxes, multipart event
  feeds, SOAP) never panic on input, bound every allocation by a declared maximum, and have a
  `cargo-fuzz` target seeded from captured data.
- `expect("<invariant>")` over `unwrap()`; pattern matching (`if let`, `let … else`) over
  check-then-unwrap; context strings on `assert!`; explicit types where inference would
  make a reader guess; no long side-effecting iterator chains; `format!`/`write!` for
  string building.
- Never read a poisoned mutex; no `catch_unwind`; no code before `main`.

## 7. Documentation (API Guidelines C-CRATE-DOC, C-EXAMPLE, C-FAILURE, C-LINK, C-METADATA, C-RELNOTES; Google timeless documentation)

- Every crate has `//!` docs with a short example; every public item has `///` docs that
  add information beyond the name; errors, panics, and invariants are documented.
- Prose is timeless: do not write *currently, now, new, soon, latest, existing, eventually,
  old, future, presently, as of this writing, does not yet*. State what the software does.
  Dates and versions appear only where the record is inherently time-stamped: research
  measurements, the changelog, and benchmark results, and then as absolute dates or version
  numbers.
- `Cargo.toml` carries description, license, repository; `CHANGELOG.md` records significant
  changes per release.
- Comments explain why, not what; `TODO` carries an owner or an issue reference.

## 8. Testing

- Unit tests live in `mod tests` with `use super::*`; golden tests use captured camera data
  kept outside git; property tests cover the timestamp regulariser and index codecs; chaos
  tests (SIGKILL, truncation, zero-filled tails) cover the store; fuzz targets cover parsers.
- A test asserts one behaviour and names it: `rejects_truncated_trun`, not `test1`.

## 9. Commits, versions, and changes

- Commit messages follow Conventional Commits 1.0.0: `type(scope)!: description`, with a
  lowercase type and scope, an imperative description without a trailing period and under
  72 characters, a body that explains the reason and what was measured, and a
  `BREAKING CHANGE:` footer (with `!`) for an incompatible change. Types: `feat`, `fix`,
  `docs`, `refactor`, `perf`, `test`, `build`, `ci`, `style`, `chore`, `revert`. Scopes are
  crate names without the `iris-` prefix (`ingest`, `store`, `api`, `gui`, `replay`, `mp4`,
  `catalog`, `events`, `core`, `h265`) or an area (`docs`, `ci`, `tools`, `soak`).
- Keep each commit coherent; split independent changes. Commits are created through the
  GitHub API (`gh api graphql`, `createCommitOnBranch`) so GitHub signs them.
  `iris-tools gh-commit -m HEADLINE --body TEXT` does this for the difference between
  `origin/<branch>` and a clean local `HEAD`.
- Versions follow Semantic Versioning 2.0.0. Before 1.0.0: `0.y.(z+1)` for compatible
  fixes, `0.(y+1).0` for new functionality or an incompatible change, breaking commits still
  marked and their migration described in `CHANGELOG.md`. The compatibility surface is the
  configuration file, the storage format (`docs/research/04-storage-format.md`), the HTTP
  and WebSocket API, the command line, and the supported platform (Linux, x86-64 and
  aarch64) and toolchain (`rust-version` in `Cargo.toml`).
- Behaviour changes update the affected document in `docs/` in the same change.
