# irisd

The daemon and its command line. `irisd run` records every configured camera stream into
fragmented MP4 files, keeps the SQLite catalog, serves the HTTP API, subscribes to camera
events, enforces retention, checks its own recordings, and reports to systemd. The other
subcommands work on recordings offline. Nothing in the record path decodes video.

## What it provides

- `irisd run --config PATH` — the daemon. Options: `--stats-interval` (default 60 s),
  `--selfcheck-cycle` (default 24 h), `--no-sandbox` (debugging on kernels without Landlock).
- `irisd record --config PATH --root DIR [--duration 10m] [--streams main,sub] [--cameras ID,…]`
  — record without the catalog, API, or events; the phase-1 spike and a quick capture tool.
- `irisd inspect FILE [--verify] [--samples]` — scan a recording and report its fragments,
  timing, and, with `--verify`, every trailer checksum.
- `irisd recover FILE` — truncate a recording at its last valid fragment and rewrite its
  sidecar index.
- `irisd hls FILE --out DIR` — static byte-range HLS playlists for one recording.
  The output may be the source directory; linking preserves the recording itself.
  The command rejects reserved playlist basenames and generated-file aliases, percent-encodes
  the media basename, validates codec tokens, and publishes synced playlists by atomic entry
  replacement. Oversized or media-mismatched sidecars fall back to a media scan. `--exclude-trailers` emits one
  fragment per segment, including continuations, and independence is declared in the media
  playlist only when every segment starts at sync.
- `irisd serve --dir DIR [--listen ADDR]` — a static file server with range support for
  playback tests.
- The configuration file (see `config/cameras.example.toml`): `[storage]` (video roots, state
  directory, free-space floor, size quota, sync interval, fragment cap), `[ingest]` (socket
  read interval, receive low-water mark), `[api]` (listen address, bearer tokens), and one
  `[[camera]]` table per camera (RTSP URLs, credentials, vendor, streams to record, event
  sources, retention days, timestamp mode).
- Statistics lines once per `--stats-interval`: one `stream` line per stream, one `events`
  line per feed, and one `writer`, `catalog`, `root` (per video root), and `process` line;
  one-off `gap`, `retention pass`, `slow write`, `slow sync`, and `selfcheck` lines. The
  `catalog` line includes event state, database/WAL sizes, command queue count/bytes and peaks,
  coalescing, admission rejections, and the reconciliation-required flag. The field reference
  is `docs/research/07-architecture.md §9`.
- `packaging/systemd/irisd.service` and `packaging/sysusers.d/irisd.conf`: a hardened
  `Type=notify` unit with a watchdog, and the service account; `packaging/README.md` is the
  installation guide.

## How it is used

```sh
cargo build --release -p irisd
target/release/irisd run --config /etc/irisd/irisd.toml
```

The daemon logs to standard error (the journal under systemd) with `RUST_LOG` controlling the
level. A per-minute `process` line looks like this:

```
process cpu_pct=6.4 cpu_user_pct=2.9 cpu_sys_pct=3.5 rss_mib=92.6 threads=3 fds=63 uptime_min=14 loop_lag_max_ms=1 watchdog_withheld=0 retention_max_ms=3 selfcheck_ok=0 selfcheck_failed=0 selfcheck_skipped=0
```

`iris-tools daemon-stats` summarises a log; `iris-tools soak-sample` and `soak-report` run a
long soak beside the service; `iris-tools replay-bench` drives the daemon against replayed
cameras for cost measurements (`crates/iris-tools/README.md`).

## Invariants and limits

- Sandbox: before any thread exists, a Landlock ruleset restricts the daemon to reading its
  configuration directory, `/etc`, and `/proc`, reading and writing the video roots and the
  state directory, connecting to the RTSP ports the camera URLs name and each camera's HTTP
  port, and binding the API port (`sandbox.rs`; `docs/research/07-architecture.md §6`).
- Watchdog: the systemd ping is withheld whenever an ingest task made no progress in the
  interval, so a wedged daemon is restarted within `WatchdogSec`.
- Readiness: startup binds the configured API listener before recording tasks start and before
  `READY=1`. An occupied address fails startup without a false-ready notification; the already
  bound listener is handed to the API task for graceful shutdown.
- Start-up resolves each configured root against a durable `.irisd-root-id` marker and catalog
  registry before Landlock or threads. A known root with a missing or changed marker is rejected;
  the admitted physical directory is pinned for that daemon run and physical aliases are rejected.
  Every unregistered root in a catalog migrated from before the registry fails closed—even when
  no segment row remains—until an operator verifies the intended mount and durably
  installs a 128-bit marker readable by the service account. Start-up then reconciles in fixed catalog pages and a streaming no-follow
  directory walk: durable deletion intents under available roots are completed first;
  files that were open at the last stop are recovered,
  and unknown files are adopted from their sidecar index (or by a scan when the index does
  not match), with progress reported to systemd and the start timeout extended while it
  runs. Elapsed-time heartbeats cover catalog pages, directory enumeration, and media scans.
  Unavailable roots and filesystem lookup errors preserve pending deletion/open rows
  for a later start; only confirmed missing open files on available roots lose their rows.
- Retention runs every minute: bounded event maintenance, the free-space floor per root,
  the size quota, then per-camera age limits. A pass stops at 512 deletion attempts, 64
  candidate pages, 1,024 raw scanned candidate rows, or one second. Candidate queries cap raw
  indexed input before protection filtering and advance after empty filtered pages. The
  payload-free retained-event projection stops at 2,048 rows or 100 ms, continues by
  `(retain_until, event id)`, and serves only as a filtering optimization; indexed SQL and the
  atomic deletion transition remain authoritative.
  Free-space deletion stays within the pressured root; the first root rotates between passes
  so one persistently pressured device cannot consume every global budget. Exact-root, quota,
  and stream-age cursors retain the last handled candidate at a bounded-pass exit and use a raw
  page end only after every returned candidate is handled. Satisfied root/quota policies reset;
  age wraps only after indexed input is exhausted. Pagination uses split start-time/id seeks so
  recordings behind large equal-time or protected groups remain reachable. Each deletion
  atomically checks for active 30-second event protection and commits FULL deletion intent. Only `Marked`
  permits pair removal; the directory is synced before the row is removed. Counts and quota
  progress advance only after that transition completes.
- Periodic reporting, retention, and self-checks are owned single-flight jobs rather than work
  awaited inline by the monitor. Filesystem probes run off the current-thread runtime; timeouts
  terminate the daemon rather than permitting detached work to accumulate. One in-kernel
  filesystem call may remain on a detached standard-library thread until process exit because
  Linux provides no safe cancellation for that syscall; no replacement job is launched, the
  thread cannot hold process exit, and durable deletion intent makes a late removal retry-safe.
- The self-check visits one stream per cycle divided by the stream count, scans its newest
  sealed file with CRC verification, and compares both wall boundaries and the media extent with
  the catalog row. Its scan is cancellation-aware and participates in ordered shutdown.
- All CLI/config timer, capacity, and time conversions plus signal/monitor state are validated or
  armed before roots, listeners, workers, or readiness. Shutdown has one absolute 25-second
  service budget: producers share one grace interval, then writer, API, and catalog cleanup all
  run best-effort against the remaining deadlines, with every task outcome observed.
- Threads: the tokio current-thread runtime, one writer thread per root, one catalog thread;
  budgets and measured costs are in `docs/research/07-architecture.md §7` and
  `12-ingest-cost.md`.
- The statistics line field names are parsed by `iris-tools daemon-stats`
  and `soak-report`; a renamed field changes those tools and `07-architecture.md §9`.
- Catalog lifecycle rejection sets `reconcile_required` and logs that startup filesystem
  reconciliation is required; event/progress rejection and query overload have separate fields.

## Tests and measurements

Unit tests cover the endpoint and sandbox derivation from configuration, retention
predicates (free-space floor, quota, age cutoff), the self-check comparison, the process
sampler parsers, and start-up path parsing. End-to-end behaviour is measured rather than
unit-tested: six and seven live cameras over hours and the systemd soak in
`docs/research/11-multi-camera-results.md`, kill and recovery in `10-phase2-results.md`,
and CPU per megabit against replayed cameras in `12-ingest-cost.md`.

## Further reading

- [`docs/research/07-architecture.md`](../../docs/research/07-architecture.md) — crates,
  threads, API, security, budgets, observability.
- [`docs/research/08-roadmap.md`](../../docs/research/08-roadmap.md) — phases and exit
  criteria.
- [`docs/design/phase2-interfaces.md`](../../docs/design/phase2-interfaces.md) — the contracts
  the daemon is built on (`irisd run` section).
- [`packaging/README.md`](../../packaging/README.md) — installation as a service.
- [`config/cameras.example.toml`](../../config/cameras.example.toml) — the configuration
  format.
- [`STANDARDS.md`](../../STANDARDS.md).
