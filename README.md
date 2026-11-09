# irisd

[![check](https://github.com/swiftraccoon/irisd/actions/workflows/check.yml/badge.svg)](https://github.com/swiftraccoon/irisd/actions/workflows/check.yml)
[![rustdoc](https://github.com/swiftraccoon/irisd/actions/workflows/docs.yml/badge.svg)](https://swiftraccoon.github.io/irisd/)

An H.265 network video recorder for Linux whose record path never decodes video: RTSP in,
standard fragmented MP4 on disk, and authenticated serving to a Linux GUI and other clients.
Written in Rust; the daemon links two C libraries: SQLite (bundled through `rusqlite`) and
AWS-LC (the `rustls` crypto provider, through `aws-lc-rs`).

The `0.1.0` workspace is unreleased alpha software. It makes no compatibility promises:
APIs, configuration, command-line behavior, storage schemas, and formats may change in pursuit
of correctness, efficiency, readability, and simplicity.

## What it does

- Records every configured camera stream over RTSP/TCP into CMAF-style fragmented MP4
  (`hvc1`), one fragment per GOP, each with a wall-clock `prft` box and a CRC-32C trailer,
  plus a sidecar fragment index per file. ffmpeg and GStreamer play the files as written;
  the AVPlayer and `mediastreamvalidator` checks are open items in
  `docs/research/10-phase2-results.md`.
- Keeps the NVR clock as the authority for time: a constant-frame-rate regulariser turns
  camera RTP timestamps into exact frame durations and renders camera-dropped frames as holds.
- Catalogs files, runs, and camera events in SQLite; enforces retention by free space, a
  size quota, and per-camera age; recovers from a crash through validated sidecar tails with
  a bounded media scan as the fallback.
- Serves byte-range HLS playlists, per-frame live fragments over WebSocket, MP4 export,
  snapshots, and events over an authenticated HTTP API.
- Subscribes to camera events (vendor push feeds and ONVIF pull-point) and stores them with
  both the camera's and the recorder's timestamps.
- Runs as a hardened systemd service: Landlock, a locked-down unit, watchdog pings gated on
  ingest progress, and a statistics line per stream, feed, writer, catalog, and root every
  minute.

Cost, measured with the replay harness on 2026-11-08: about 0.024 % of one core per Mbit/s of
H.265 at 143 Mbit/s for fourteen replayed streams (`docs/research/12-ingest-cost.md` §6).

## Layout

The crates and the rest of the tree are listed in [`crates/README.md`](crates/README.md).

## Build, test, run

Requires a stable Rust toolchain (`rust-toolchain.toml`; the minimum supported version is
Rust 1.92), `just`, a C compiler (and a cross C compiler for `just cross-check`), and the
GTK 4 and GStreamer development libraries, which `just check` needs too because the viewer is
a workspace member.

    just check          # format check, clippy with warnings as errors, tests, rustdoc,
                        # and the vendored crates' tests
    just audit          # cargo audit and cargo deny
    just build          # release build of the daemon
    just doc            # rustdoc for every crate into target/doc

Configure cameras in a private copy of `config/cameras.example.toml`, then:

    irisd run --config /etc/irisd/irisd.toml

`packaging/README.md` describes the service user, the unit, the recording root, and the
network drop-in. `irisd inspect FILE.mp4` lists a recording's fragments and timing;
`irisd recover` seals files left open by a crash; `irisd hls` and `irisd serve` exercise the
playlist and API paths without the daemon.

Benchmarks and fault injection need no hardware:

    cargo build --release -p irisd -p iris-replay -p iris-tools
    target/release/iris-tools replay-bench --file RECORDING.mp4 --cameras 14 --seconds 90 \
      --read-interval-ms 20 --recv-low-water-kib 32 --root SCRATCH \
      --irisd target/release/irisd --replay target/release/iris-replay --replay-arg=--pace

The two ingest flags select the daemon's default reads; `replay-bench` itself defaults to
packet-driven reads without the low-water mark.

## Documentation

- **rustdoc** — published to GitHub Pages (the rustdoc badge above links there; the daemon's
  crate opens first, with the other crates in the sidebar). The `docs` workflow builds
  `cargo doc --workspace --no-deps` on every push to `main` and deploys it to GitHub Pages,
  whose source must be set to GitHub Actions. Locally, `just doc` writes it to
  `target/doc/irisd/index.html`.
- **Design** — `docs/research/README.md` holds the decision table and the reading order;
  `04-storage-format.md` fixes the on-disk format and time model, `07-architecture.md` the
  crates, threads, budgets, and observability, `12-ingest-cost.md` the measured cost.
- **Operations** — `crates/iris-tools/README.md` covers the log summariser, the soak
  sampler and report, the benchmark driver, and the camera probes.
- **Standards** — `STANDARDS.md` digests the vendored style guides; `AGENTS.md` sets the
  rules for anyone, human or agent, changing the repository.

## License

irisd's own code is licensed under either MIT (`LICENSE-MIT`) or Apache-2.0 (`LICENSE-APACHE`),
at your option. Vendored material keeps its own licenses: `third_party/` (see its README), the
reference pack in `.rust-reference/` (its `LICENSE` and `references/README.md`), and the IETF
documents in `docs/standards/` (its README).
