# Documentation expectations

Write documentation that lets a reader use the supported behavior without
knowing the project's development history. These are this workspace's adopted
defaults, informed by Google's locally stored
[Timeless documentation](../references/google-timeless-documentation/timeless-documentation.md),
the [Rust API documentation guidelines](../references/rust-api-guidelines/src/documentation.md),
and [Google's Rust documentation guidance](../references/google-styleguide/rustguide.md#writing-doc).

## Durable prose

Describe the product's behavior directly. Avoid relative claims such as
"currently," "new," "latest," "not yet," and "soon" in reference material.
When a version distinction matters, name the version or date and explain its
effect on the reader. A changelog or dated announcement can describe change;
an API reference should explain the API.

| Avoid | Prefer |
| --- | --- |
| The parser now accepts comments. | The parser accepts comments. |
| Use the latest Rust release. | Use the toolchain specified in `rust-toolchain.toml`. |
| This option is not yet supported. | This option is unsupported. |
| The new API replaces the old parser. | Version 2 uses `Parser::parse`; see the migration guide. |

Do not promise planned features as product behavior. State limitations precisely
and keep roadmap discussions in their own clearly identified context. Reference
refresh timestamps and pinned revisions are provenance; retain them even though
product prose avoids unnecessary relative time.

Use direct sentences and concrete names. Explain prerequisites before commands,
show expected results where they help, and identify placeholders. Comments
should explain intent, invariants, or a surprising choice. Remove comments that
only narrate obvious syntax.

## Rustdoc

- Use `//!` for crate or module documentation and `///` for items. Keep internal
  implementation notes in ordinary comments.
- Explain a crate's purpose, main concepts, and a representative use case.
  Document the behavior and contract of public APIs, including meaningful
  constraints on arguments, return values, and side effects.
- Include `# Errors`, `# Panics`, and `# Safety` sections when applicable.
  An unsafe function's safety section must tell callers what they must uphold;
  a block's `SAFETY:` comment explains why its implementation upholds those
  obligations at that call site.
- Provide useful examples for the public workflow. Share and cross-link an
  example when several items participate in the same operation. Exercise the
  reason to use the API rather than showing a trivial invocation alone.
- Compile examples as doctests. Prefer `?` for recoverable failures, using hidden
  setup or a `Result`-returning wrapper when needed. Use `no_run`, `compile_fail`,
  or `ignore` only when the example's behavior requires it, with an explanation
  for ignored examples.
- Use rustdoc intra-doc links for related types and methods. Avoid exposing
  irrelevant implementation details or linking consumers to private items.
  Check rendered documentation and resolve broken links.

## Project documentation

A project's README should explain its purpose, setup, ordinary use, and how to
run its checks. Record supported Rust versions, platforms, feature combinations,
and meaningful operating constraints where applicable. Public crates need
accurate package metadata and license information.

Update documentation in the same change as the behavior it describes. Record
significant release changes and migration steps in release notes. Explain
architectural choices when they constrain later work; keep those records close
to the code or contributor guidance.

Link to the local reference copies when directing agents to style guidance.
Preserve upstream attribution and license files when copying them. Put project
decisions and adaptations in project documentation; leave managed upstream
snapshots unchanged so refreshes remain reviewable.
