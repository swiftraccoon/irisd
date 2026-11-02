# AGENTS.md — irisd

Scope: this crate only. Read the root `AGENTS.md` first.

## Invariants

- Statistics line field names (`stream`, `events`, `writer`, `catalog`, `root`, `process`,
  and the one-off `gap`, `retention pass`, `slow write`, `slow sync`, `selfcheck` lines) are
  parsed by `iris-tools daemon-stats` and `iris-tools soak-report` and documented in
  `docs/research/07-architecture.md §9`; a change updates all three.
- The sandbox spec (`sandbox_spec` in `run.rs`) is derived from the configuration and applied
  before any thread is spawned; its paths and ports are the ones in
  `docs/research/07-architecture.md §6` and `packaging/README.md`.
- The watchdog ping is gated on every stream having made progress since the last check
  (`all_streams_progressed`); never ping unconditionally.
- Retention order and bounds (free-space floor, quota, age; oldest sealed first; at most 512
  deletions per pass) follow `docs/research/04-storage-format.md §8`.
- Shutdown order is ingest tasks, writer, API, catalog, with every open file sealed.
- Configuration keys are the ones in `config/cameras.example.toml`; a change to the schema
  updates that file and `docs/research/07-architecture.md §10`.

## Where things are

- `main.rs` — command line and subcommand dispatch.
- `run.rs` — the daemon: start-up, task spawning, the monitor loop, statistics lines, shutdown.
- `record.rs` — `irisd record`, ingest to disk without the catalog or API.
- `inspect.rs`, `recover.rs`, `hls.rs`, `serve.rs` — the offline subcommands.
- `adopt.rs` — start-up reconciliation of the catalog with the files on disk.
- `catalog_observer.rs` — store notifications and camera events into the catalog.
- `retention.rs` — the retention pass and its report.
- `selfcheck.rs` — the rotating CRC check of sealed files.
- `sandbox.rs` — the Landlock ruleset.
- `systemd.rs` — readiness, watchdog, and stopping notifications.
- `proc.rs` — CPU, RSS, thread, and descriptor sampling from `/proc/self`.

## How to verify a change

```sh
cargo fmt -p irisd
cargo clippy -p irisd --all-targets -- -D warnings
cargo test -p irisd
RUSTDOCFLAGS="-D warnings" cargo doc -p irisd --no-deps
```

Run the daemon against the replay server instead of live cameras:

```sh
cargo build --release -p irisd -p iris-replay -p iris-tools
target/release/iris-tools replay-bench --file RECORDING --cameras 7 --seconds 90 \
  --root SCRATCH --irisd target/release/irisd --replay target/release/iris-replay --replay-arg=--pace
```

A change to the statistics lines is checked by feeding a log to
`iris-tools daemon-stats`. A live-camera run follows the limits in the root `AGENTS.md`.

## Pitfalls

- Anything awaited inside the monitor loop delays the statistics, watchdog, and retention
  ticks; long work goes to `spawn_blocking` or its own task (the self-check does this).
- The select arms in `monitor` borrow disjoint fields of `Daemon`; a helper that takes the
  whole struct will not compile.
- `start_recording` opens two RTSP sessions per camera; the limit is three per camera, and
  the family of cameras described in `docs/research/01-camera-probe.md §5b` drops its sub
  stream when another session opens.
- A non-standard RTSP port must be allowed by the sandbox (`CameraConfig::rtsp_ports`), or
  connections fail with "Permission denied".
- Counters reset on restart; tools that sum them handle restarts by positive deltas.

## Related documents

- `docs/research/07-architecture.md`, `docs/research/04-storage-format.md` §8 and §9,
  `docs/research/08-roadmap.md`, `docs/research/10-phase2-results.md`,
  `docs/research/11-multi-camera-results.md`, `docs/research/12-ingest-cost.md`,
  `docs/design/phase2-interfaces.md`, `packaging/README.md`.
