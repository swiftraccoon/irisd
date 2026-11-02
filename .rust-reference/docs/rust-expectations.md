# Rust project expectations

These are the defaults adopted by this reference workspace. They combine the
local upstream guides with practical project conventions; they are not a claim
that every upstream guide requires every rule below. Explicit user requirements
and the consuming project's documented decisions take precedence. Record a
departure and its reason in that project's guidance when it affects later work.

Read the [Rust style guide](../references/rust-style-guide/src/doc/style-guide/src/README.md),
the [API checklist](../references/rust-api-guidelines/src/checklist.md), and
[Google's Rust guide](../references/google-styleguide/rustguide.md) for details.
Use the [reference index](../references/README.md) for provenance and updates.

## Toolchain and formatting

- Implement project-owned tools, automation, and maintenance commands in Rust.
  Do not add Python code or a Python runtime dependency to Rust projects.
- Use stable Rust unless the project has a documented requirement for an
  unstable feature. Preserve its edition, minimum supported Rust version (MSRV),
  and formatter configuration during unrelated work.
- For a new project, choose and record the Rust edition, MSRV, and development
  toolchain. Put the MSRV in `package.rust-version`; use
  `workspace.package.rust-version` with member inheritance for a shared value.
  A development toolchain pin and an MSRV express different commitments.
- Run the project's `cargo fmt` with its selected toolchain. Prefer default
  rustfmt formatting, four spaces, and a 100-column width. Avoid unstable
  formatter options merely to reproduce a guide's illustrative formatting.
  Explain narrow `rustfmt::skip` exceptions where needed.
- A source snapshot can discuss unreleased syntax or contain outdated release
  statements. The target toolchain and tested compatibility determine what the
  project can use. Updating references does not authorize an edition migration.

## Structure, names, and imports

Use conventional Cargo layout: `src/lib.rs` for a library, `src/main.rs` for a
binary, and modules organized around cohesive responsibilities. Keep application
entry points focused on configuration and orchestration. Split crates when the
boundary serves ownership, reuse, or build needs; avoid creating a crate for
every source file.

Follow Rust naming: `snake_case` modules, functions, and variables;
`UpperCamelCase` types and traits; `SCREAMING_SNAKE_CASE` constants. Prefer clear
domain names to abbreviations. Follow the standard `as_`, `to_`, and `into_`
conversion meanings, and omit a redundant `get_` on ordinary getters.

Keep visibility as narrow as practical. Prefer explicit imports; a local
`use super::*` in tests is reasonable. Import types directly, qualify external
free functions by their module, and avoid combining imports from multiple
modules inside one nested brace group. Existing consistent project conventions
can refine these choices. Root modules should primarily explain the structure
and expose the public facade when implementations have their own modules.

## Ownership, APIs, and traits

Borrow data when the operation does not need ownership. Prefer `&str`, `&[T]`,
and `&T` over references to their owning containers. Choose owned values when
the API stores or transfers them. A clone or shared owner is appropriate when
it makes the ownership model clearer and its cost is acceptable; do not add
lifetime complexity solely to remove a harmless copy.

For public libraries, review the API checklist before exposing new items:

- Use newtypes for meaningful distinctions and enums for choices whose boolean
  arguments would be ambiguous. Validate inputs at the boundary.
- Prefer established standard traits to equivalent custom methods. Derive
  traits where their semantics fit. Consider useful interoperability traits for
  public types, without making application internals implement unused APIs or
  adding a dependency just to satisfy a checklist.
- Expose the flexibility callers need. Choose slices, iterators, generics, or
  trait objects deliberately. Keep private application code direct and avoid
  speculative abstraction.
- Protect invariants and representation with private fields. Consider
  `#[non_exhaustive]` for public enums and field-bearing structs intended to
  evolve; account for the downstream matching and construction restrictions.
- Treat public signatures, trait implementations, feature behavior, and error
  types as compatibility commitments. Explain breaking changes and migrations
  in release notes. Do not rely on another crate's hidden documentation items.

The upstream [interoperability](../references/rust-api-guidelines/src/interoperability.md),
[type safety](../references/rust-api-guidelines/src/type-safety.md), and
[future proofing](../references/rust-api-guidelines/src/future-proofing.md)
chapters provide the full reasoning. Library recommendations should not turn
private application helpers into a general-purpose framework.

## Errors and implementation

Use `Option` for an expected absence or simple outcome that needs no diagnostic
detail. Use `Result` when callers need to distinguish or explain failures.
Propagate errors with `?` and add useful context at boundaries. Public library
errors should support callers' recovery needs and implement the appropriate
standard error traits; applications can use the project's chosen reporting
approach.

Reserve panics for bugs and violated invariants. Avoid `unwrap` or `expect` on
untrusted input and ordinary I/O failures. When a proven invariant justifies
`expect`, explain what makes failure a bug. Tests can fail directly with useful
assertions. Prefer matching and `if let` to checking a variant and then unwrapping
it. Use loops when they express control flow more clearly than combinator chains.

Do not swallow errors, conceal incomplete behavior behind a success return, or
introduce broad lint allowances. A lint exception should cover the smallest
relevant scope and explain its reason.

## Safety and concurrency

Prefer safe implementations. Any necessary unsafe code needs a technical
justification, a small reviewable boundary, and `SAFETY:` comments explaining
the actual invariants. Document caller obligations on unsafe APIs. Use tests and
appropriate analysis tools for the risks involved; measure performance when
performance is the justification for unsafe code.

Use concurrency only when the workload needs it. Choose the runtime, ownership,
synchronization, cancellation, and shutdown model explicitly. Respect the chosen
runtime's blocking rules and avoid holding locks across suspension points.

Google's rules marked `[chrome]` or `[Fuchsia]` belong to those environments.
Its Chromium flat file layout, restrictions on async and standard library
threads, Chromium import macro, and Fuchsia mutex choice do not apply to an
ordinary Cargo project by default. Adopt them only when the target environment
requires them.

## Dependencies and verification

Prefer the standard library when it meets the requirement. Before adding a
crate, consider its maintenance, license compatibility, MSRV, supported targets,
feature costs, and whether an existing dependency already solves the problem.
Keep dependencies and enabled features focused. Follow the project's lockfile
policy and include intentional lockfile changes with dependency updates.

Run the consuming project's documented checks. A typical starting point is:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo test --workspace --doc
cargo doc --workspace --no-deps
```

Use these commands in the consuming Cargo workspace and adjust them for
supported packages, targets, feature sets, and project lint policy. This pack's
own maintenance checks are listed in [AGENTS.md](../AGENTS.md).
Add `--locked` where the workflow maintains a lockfile.
Do not assume `--all-features` is valid when features are mutually exclusive.
Test the declared MSRV and meaningful supported feature combinations in CI.

Write tests for observable behavior, failure paths, boundaries, and regressions.
Keep them deterministic; avoid external network and timing dependencies unless
the behavior under test requires them. Review doctests and
[documentation](documentation.md) with the implementation. Report the checks
run and any material gaps when delivering a change.
