# irisd

[![check](https://github.com/swiftraccoon/irisd/actions/workflows/check.yml/badge.svg)](https://github.com/swiftraccoon/irisd/actions/workflows/check.yml)
[![rustdoc](https://github.com/swiftraccoon/irisd/actions/workflows/docs.yml/badge.svg)](https://swiftraccoon.github.io/irisd/)

An H.265 network video recorder for Linux whose record path never decodes video: RTSP in,
standard fragmented MP4 on disk, byte-range serving out to a Linux GUI and an iOS app.
Written in Rust; SQLite, linked through `rusqlite`, is the one C library in the daemon.

## What it does

- Records every configured camera stream over RTSP/TCP into CMAF-style fragmented MP4
  (`hvc1`), one fragment per GOP, each with a wall-clock `prft` box and a CRC-32C trailer,
  plus a sidecar fragment index per file. Files are playable by any MP4 player as written.
- Keeps the NVR clock as the authority for time: a constant-frame-rate regulariser turns
  camera RTP timestamps into exact frame durations and renders camera-dropped frames as holds.
- Catalogs files, runs, and camera events in SQLite; enforces retention by free space, a
  size quota, and per-camera age; recovers from a crash by scanning the open files.
- Serves byte-range HLS playlists, per-frame live fragments over WebSocket, MP4 export,
  snapshots, and events over an authenticated HTTP API.
- Subscribes to camera events (vendor push feeds and ONVIF pull-point) and stores them with
  both the camera's and the recorder's timestamps.
- Runs as a hardened systemd service: Landlock, a locked-down unit, watchdog pings gated on
  ingest progress, and a statistics line per stream, feed, writer, catalog, and root every
  minute.

Cost, measured with the replay harness: about 0.065 % of one core per Mbit/s of H.265 at
100 Mbit/s, about 90 MiB resident for fourteen streams (`docs/research/12-ingest-cost.md`).

## Layout

The crates and the rest of the tree are listed in [`crates/README.md`](crates/README.md).

## Build, test, run

Requires a stable Rust toolchain (`rust-toolchain.toml`), `just`, and for the viewer the
GTK 4 and GStreamer development libraries.

    just check          # format check, clippy with warnings as errors, tests, rustdoc
    just audit          # cargo audit and cargo deny
    just build          # release build of the daemon
    just doc            # rustdoc for every crate into target/doc

Configure cameras in a private copy of `config/cameras.example.toml`, then:

    irisd run --config /etc/irisd/irisd.toml

`packaging/README.md` describes the service user, the unit, and the drop-in for the
recording root. `irisd inspect FILE.mp4` lists a recording's fragments and timing;
`irisd recover` seals files left open by a crash; `irisd hls` and `irisd serve` exercise the
playlist and API paths without the daemon.

Benchmarks and fault injection need no hardware:

    cargo build --release -p irisd -p iris-replay -p iris-tools
    target/release/iris-tools replay-bench --file RECORDING.mp4 --cameras 14 --seconds 90 \
      --root SCRATCH --irisd target/release/irisd --replay target/release/iris-replay --replay-arg=--pace

## Documentation

- **rustdoc** — published at <https://swiftraccoon.github.io/irisd/> (the badge above links
  there; the daemon's crate opens first, with the other crates in the sidebar). The `docs`
  workflow builds `cargo doc --workspace --no-deps` on every push to `main` and deploys it
  to GitHub Pages, whose source must be set to GitHub Actions. Locally, `just doc` writes it
  to `target/doc/irisd/index.html`.
- **Design** — `docs/research/README.md` holds the decision table and the reading order;
  `04-storage-format.md` fixes the on-disk format and time model, `07-architecture.md` the
  crates, threads, budgets, and observability, `12-ingest-cost.md` the measured cost.
- **Operations** — `crates/iris-tools/README.md` covers the log summariser, the soak
  sampler and report, the benchmark driver, and the camera probes.
- **Standards** — `STANDARDS.md` digests the vendored style guides; `AGENTS.md` sets the
  rules for anyone, human or agent, changing the repository.

## License

MIT OR Apache-2.0. `third_party/` carries vendored code under its own licenses, listed in
`third_party/README.md`.
