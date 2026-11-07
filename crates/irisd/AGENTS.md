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
- Retention order and bounds (event maintenance, free-space floor, quota, age; oldest sealed
  first; at most 512 attempts, 64 pages, 1,024 raw scanned candidate rows, or one second per pass) follow
  `docs/research/04-storage-format.md §8`. Protection projection is separately bounded to
  2,048 minimal rows and 100 ms and continues by `(retain_until, event id)`.
- Free-space retention selects files beneath the pressured root; quota retention spans
  roots. Indexed candidate SQL caps raw input before active-protection filtering and returns
  scan progress even for an empty filtered page. Pagination uses `(wall_start, segment id)`;
  the atomic mutation repeats protection authorization. The first root rotates between passes
  so a root cannot monopolize the shared work budget. Separate exact-root, quota, and
  stream-age cursors advance through handled candidates and adopt the raw page end only after
  complete candidate consumption. Budget exits retain the last handled candidate, satisfied
  root/quota policies reset, and age wraps only after raw-query exhaustion. Policy
  reconciliation includes the free-space floor and prunes or resets bounded state.
- Startup resolves durable root identities synchronously before the sandbox or threads, rejects
  marker/physical aliases, and only visits `ApprovedRoot`s. Catalog rows are paged; root walks
  are streaming and no-follow; one elapsed heartbeat covers every reconciliation phase. A writing
  row on an unavailable root or behind a filesystem lookup error is preserved; only confirmed
  absence under its exact admitted persisted root permits cleanup.
- Startup registers configured roots and completes durable deletion intents on available roots
  before writing recovery and adoption. Retention unlinks only after
  `mark_deleting_if_unprotected` returns `Marked`; `Protected` and `NotFound` do not mutate the
  filesystem. It removes the row only after the directory barrier; failed transitions count
  neither deletion nor quota progress.
- Empty writing rows enter durable deletion intent before descriptor-relative pair removal.
  Every destructive startup/retention operation uses its exact approved root and keeps a row or
  tombstone until both names are absent and the directory barrier succeeds.
- Shutdown has one absolute budget: cancel once, await all producers against one shared grace,
  abort and observe every remainder, then attempt writer, API, and catalog closure exactly once
  while preserving the first failure. Periodic maintenance and self-check owners participate in
  the same bound.
- The API listener is bound successfully before recording tasks and `READY=1`; serving consumes
  that same listener. A bind failure must not emit readiness or leave recording work running.
- Offline HLS treats a bounded sidecar only as a cache physically bound to the selected media,
  rejects generated-file/source aliases and reserved basenames, validates playlist text, and
  publishes through exclusive temporaries plus atomic rename. It excludes each trailer separately;
  continuation segments cannot claim independent decoding.
- An API task consumed by the monitor is removed before ordered shutdown; a completed
  join handle cannot be polled twice.
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

- Anything awaited inside the monitor loop delays signals and watchdog ticks. Reporting,
  retention, and self-check work stays single-flight, separately owned, cancellable, and bounded;
  blocking filesystem calls run off the current-thread runtime. An uninterruptible raw filesystem
  call may detach only while its single-flight owner forces bounded process exit; never relaunch
  that owner, and preserve deletion intent so late completion is retry-safe.
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
