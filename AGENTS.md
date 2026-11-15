# AGENTS.md — working in this repository

irisd is an H.265 network video recorder for Linux whose record path never decodes video:
RTSP in, standard fragmented MP4 out, and authenticated serving to a Linux GUI and other clients.
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
crates/iris-catalog  SQLite catalog, bounded admission, retention queries, migrations
crates/iris-events   bounded Dahua CGI and ONVIF PullPoint event subscriptions
crates/iris-api      authenticated HTTP/WebSocket serving, HLS, export, metrics
crates/irisd         the binary: run (the daemon), record, inspect, clocks, recover, hls, serve
crates/iris-replay   RTSP replay server: recordings as synthetic cameras, for benchmarks and
                     fault injection
crates/iris-tls      shared TLS client trust: the platform's roots plus an optional CA file
crates/iris-gui      Linux viewer: GTK 4 live grid, focus, playback, timeline, export
crates/iris-tools    the tools binary: log summaries, soak sampler and report, benchmark driver,
                     camera probes, gh-commit
fuzz/                cargo-fuzz targets for the parsers (rule 8), with their own workspace,
                     lockfile, and pinned nightly
config/              cameras.example.toml (committed); *.local.toml copies (gitignored secrets)
standards.toml       the accepted exceptions to `just standards`, each with its reason
packaging/           systemd unit, sysusers file, and the install guide
docs/research/       research and design, including every dated measurement
docs/design/         the cross-crate interfaces of the phase-2 crates
.rust-reference/     the Rust project reference pack (policies, vendored guides, its own Rust CLI)
docs/standards/ietf/ the RTSP, RTP, RTP audio and video profile, MPEG-4 and HEVC payload,
                     HLS, and codecs documents, as published
third_party/         vendored dependencies with local patches (retina, oxvif); see its README
.github/             workflows/check.yml (format, clippy, tests, rustdoc, vendored tests, MSRV,
                     audit, fuzz build, aarch64 clippy and tests, references), workflows/docs.yml
                     (rustdoc to GitHub Pages), dependabot.yml (weekly action updates)
```

## Commands

- `just check` — format check, clippy with warnings as errors, tests, docs, the vendored
  crates' tests, and `just standards`. Run before finishing any change.
- `just ci` — everything the CI workflow runs (`just check`, the commit messages of the
  unpushed range, the minimum supported Rust version, `just audit`, the fuzz targets' lint and
  build, `just cross-check`, `just cross-test`, the reference pack) and `just ext4-test`, the
  tests once more with their temporary files on a scratch ext4 image, which a CI runner's are
  on. Run it before a push; `just check` alone is one of the workflow's seven jobs. A
  gitignored `.env` holds what a machine needs set, such as the C cross compiler's sysroot.
- `just standards` — the rules of `STANDARDS.md` and of this file that a program can decide,
  over every file git tracks or would track (`STANDARDS.md` §1 lists them; `standards.toml`
  holds the accepted exceptions). `just standards-commits` checks the commits that are not on
  the remote branch. `just hooks`, once per clone, makes git run both: the tree before a
  commit and the message as it is made. A hook that refuses is fixed, never bypassed.
  `iris-tools gh-commit` holds the message it publishes to the same rules.
- `just fmt`, `just test`, `just clippy`, `just doc`, `just vendor-test` (the vendored crates' own
  test suites, offline), `just audit` (cargo-audit over every lockfile and cargo-deny), `just
  fuzz-build` (every fuzz target on the pinned nightly), `just cross-check` (clippy for 64-bit ARM;
  needs the target and a C cross compiler), `just cross-test` (tests of the crates without C
  dependencies as ARM code, on an ARM host or under `qemu-aarch64-static`), `just mutants FILE...`
  (mutation testing of the named source files; needs cargo-mutants), `just display-test` (the
  viewer's checks that need a display, on a virtual one; run after any change to `crates/iris-gui`,
  since `just check` leaves them out).
- `just record 2m` — record the configured cameras for two minutes into the default root.
- `cargo run -p irisd -- inspect <file.mp4>` — list fragments and timing of a recording.

## Rules

1. Follow `STANDARDS.md`. Highlights: no `unsafe` (workspace `forbid`); `thiserror` in
   libraries, `anyhow` in the binary; newtypes for units and ids; one `use` per module, no
   glob imports outside tests; every public item documented; timeless prose (no
   *currently, now, new, soon, latest, existing, eventually*).
2. Never put credentials in URLs, logs, docs, commits, or test fixtures. Camera secrets
   live only in gitignored `config/*.local.toml` files.
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
9. Committed files are about the software, never about the people who work on it or the
   place it runs. They contain no person's name, email, or username; no hostnames, LAN
   addresses, disk names, or local paths; no camera makers, models, serial numbers,
   firmware strings, device names, or addresses; no credentials. They describe no
   installation either: not how many cameras a site has or of what kinds, not the
   specifications of its machines, disks, cameras, or phones, not its location, its light,
   its sounds, its schedule, or what happens there, and not what anyone there did or
   decided. A measurement states the conditions its claim needs in general terms (a test
   camera, an H.265 stream at a stated resolution and bit rate, the test machine) and
   nothing that tells one installation from another. Records of a particular installation
   go in gitignored files: `AGENTS.override.md`, `config/*.local.toml`, and `docs/local/`.
   Media, logs, and events taken from an installation are never committed, and copies made
   for an analysis are deleted when it ends. Example addresses use RFC 5737 ranges
   (`192.0.2.0/24`). Exceptions are the repository's own address in the badge and workflow
   links of `README.md` and `.github/` and in the `repository` key of the Cargo manifests, and
   legally required attribution, provenance, and synthetic fixtures retained from published
   third-party source. Vendored material must never contain locally captured identifiers or
   secrets.
10. Documentation is part of every change: rustdoc on every public item (intra-doc links,
    an example where cheap), the crate's `README.md` and `AGENTS.md` when its surface or
    invariants move, and the design document that fixes the behaviour. CI builds rustdoc
    with warnings as errors.
11. Research comes before code. A change that adds a media format, a codec, a protocol
    path, or a serving surface starts from a document in `docs/research/` that exists
    before the first line of it is written: the governing standards read and cited by
    clause, with their text under `docs/standards/`; how comparable recorders and players
    handle the same thing; what each player and tool the files must work in requires; and
    for every design decision the external check that settles it (a clause, a real decoder,
    a real device, a measurement on a camera) or the word *unverified*. A test against a
    fake written for the change shows that the code matches its author's assumption, not
    that the assumption holds; it is never the only check. Urgency does not waive this rule.

## Definition of done for a change

`just check` passes; new behaviour has a test; docs updated; a new format, codec, protocol
path, or serving surface has its research document (rule 11), named in the change
description; no new dependency without a line in the change description saying why and
what license it carries.

## Rust project references

Use the local reference pack at `.rust-reference/`. Read its `AGENTS.md`,
`.rust-reference/docs/rust-expectations.md`, `.rust-reference/docs/documentation.md`, and
`.rust-reference/docs/commits-and-releases.md` before changing Rust code or project
documentation.
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
