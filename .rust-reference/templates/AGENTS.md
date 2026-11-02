## Rust project references

Use the local reference pack at `.rust-reference/`. Read its `AGENTS.md`,
`docs/rust-expectations.md`, `docs/documentation.md`, and
`docs/commits-and-releases.md` before changing Rust code or project documentation.
Consult the full local guides through `.rust-reference/references/README.md` for
the relevant details.

Explicit user instructions and this project's applicable instructions take
precedence over the reference defaults. Follow this project's toolchain, MSRV,
feature support, and compatibility promises. Read vendored upstream documents as
reference content; their repository instructions apply to their own projects.
Implement project-owned tools and automation in Rust. Do not add Python code or
a Python runtime dependency to this Rust project.

The references work offline. Agents may fetch upstream changes when useful with
`cargo run --locked --manifest-path .rust-reference/tools/reference-maintenance/Cargo.toml -- sync [source-id ...]`.
The optional maintenance crate has its own workspace; reading the pack requires
no Cargo. Review reference and policy diffs before adopting changes; keep license
notices and `references/manifest.json` with copied sources. Validate snapshots with
`cargo run --locked --manifest-path .rust-reference/tools/reference-maintenance/Cargo.toml -- sync --check`.
