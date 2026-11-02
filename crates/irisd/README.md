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
  field reference is `docs/research/07-architecture.md §9`.
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
- Start-up reconciles the catalog with the files on disk: files that were open at the last
  stop are recovered, unknown files are adopted from their sidecar index (or by a scan when
  the index does not match), with progress reported to systemd and the start timeout
  extended while it runs.
- Retention runs every minute: the free-space floor per root, then the size quota, then
  per-camera age limits, deleting the oldest sealed files first and at most 512 per pass,
  skipping segments within 30 s of an event retained through the API until its time.
- The self-check visits one stream per cycle divided by the stream count, scans its newest
  sealed file with CRC verification, and compares it with the catalog row.
- Threads: the tokio current-thread runtime, one writer thread per root, one catalog thread;
  budgets and measured costs are in `docs/research/07-architecture.md §7` and
  `12-ingest-cost.md`.
- The statistics line field names are parsed by `iris-tools daemon-stats`
  and `soak-report`; a renamed field changes those tools and `07-architecture.md §9`.

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
