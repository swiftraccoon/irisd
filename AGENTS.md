# AGENTS.md — working in this repository

irisd is an H.265 network video recorder for Linux whose record path never decodes video:
RTSP in, standard fragmented MP4 out, byte-range serving to a Linux GUI and an iOS app.
Rust workspace. Read this file first, then `STANDARDS.md`, then `docs/research/README.md`.

## Read before changing anything

| Topic | Where |
|---|---|
| Engineering standards (formatting, naming, API design, docs, commits, tests) | `STANDARDS.md`; verbatim sources in the reference pack `.rust-reference/` and `docs/standards/ietf/` |
| Design decisions and measured facts | `docs/research/README.md` (decision table), `04-storage-format.md` (format and time model), `07-architecture.md` (crates, threads, budgets) |
| Test camera facts and quirks | `docs/research/01-camera-probe.md` |
| Phase plan and exit criteria | `docs/research/08-roadmap.md` |
| Rules specific to one crate | `crates/<crate>/AGENTS.md`; each crate's `README.md` states what it provides and its invariants |
| Local, machine-specific notes (not committed) | `AGENTS.override.md`, `docs/local/` |

## Layout

```
crates/iris-core     ids, time units (WallUs, MediaTicks), TOML config
crates/iris-h265     NAL classification, length-prefixed access-unit helpers
crates/iris-mp4      fMP4 init segment, fragment encoder, scanner, sidecar index, HLS playlists
crates/iris-ingest   retina RTSP sessions, timestamp regulariser, fragment assembly
crates/iris-store    segment files, preallocation, sync policy, writer thread, recovery
crates/irisd         the binary: record, inspect, recover, hls, serve
crates/iris-replay   RTSP replay server: recordings as synthetic cameras for benchmarks and fault injection
crates/iris-gui      Linux viewer: GTK 4 grid of live tiles decoded by GStreamer (phase 3)
crates/iris-tools    the tools binary: log summaries, soak sampler and report, benchmark driver, camera probes, gh-commit
config/              cameras.example.toml (committed), cameras.local.toml (gitignored secrets)
docs/research/       research and design (dated measurements live only there and in CHANGELOG)
.rust-reference/     the Rust project reference pack (policies, vendored guides, its own Rust CLI)
docs/standards/ietf/ the RTSP, RTP, HEVC payload, HLS, and codecs documents, as published
third_party/         vendored dependencies with local patches (retina + SO_RCVLOWAT); see its README
.github/workflows/   CI: check (format, clippy, tests, rustdoc, audit) and docs (rustdoc to GitHub Pages)
```

## Commands

- `just check` — format check, clippy with warnings as errors, tests, docs. Run before
  finishing any change.
- `just fmt`, `just test`, `just clippy`, `just doc`, `just audit`.
- `just record 2m` — record the configured cameras for two minutes into the default root.
- `cargo run -p irisd -- inspect <file.mp4>` — list fragments and timing of a recording.

## Rules

1. Follow `STANDARDS.md`. Highlights: no `unsafe` (workspace `forbid`); `thiserror` in
   libraries, `anyhow` in the binary; newtypes for units and ids; one `use` per module, no
   glob imports outside tests; every public item documented; timeless prose (no
   *currently, now, new, soon, latest, existing, eventually*).
2. Never put credentials in URLs, logs, docs, commits, or test fixtures. Camera secrets
   live only in `config/cameras.local.toml`.
3. Performance claims need numbers from the benchmark harness or `irisd record` stats,
   recorded in `docs/` with the date and the command used.
4. A behaviour change updates the affected design document in the same change.
5. Do not commit or push unless asked. Commits follow `STANDARDS.md §9` (Conventional
   Commits, created through the GitHub API so they are signed). Never edit files under
   `.rust-reference/references/` or `docs/standards/ietf/` by hand.
6. The test cameras are live devices on a private network: do not change camera
   configuration, do not open more than three concurrent RTSP sessions per camera, and
   tear sessions down (TEARDOWN) when finished.
7. Storage roots on the host hold other data; irisd only touches the recording root named
   in `AGENTS.override.md`.
8. Parsers that face the network never panic on input and get a fuzz target.
9. Committed files never contain identifying details: no person's name, email, or
   username; no hostnames, LAN addresses, disk names, or local paths; no camera models,
   serial numbers, firmware strings, device names, or addresses; no credentials. Hardware
   is described by class only (an x86-64 host, an NVIDIA GPU with NVDEC, an ext4 HDD).
   Specifics that work needs go in gitignored files: `AGENTS.override.md`,
   `config/*.local.toml`, and `docs/local/`. Example addresses use RFC 5737 ranges
   (`192.0.2.0/24`). The one exception is the repository's own address in the badge and
   workflow links of `README.md` and `.github/`.
10. Documentation is part of every change: rustdoc on every public item (intra-doc links,
    an example where cheap), the crate's `README.md` and `AGENTS.md` when its surface or
    invariants move, and the design document that fixes the behaviour. CI builds rustdoc
    with warnings as errors.

## Definition of done for a change

`just check` passes; new behaviour has a test; docs updated; no new dependency without a
line in the change description saying why and what license it carries.

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
