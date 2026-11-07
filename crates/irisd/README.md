# irisd

The daemon and its command line. `irisd run` records every configured camera stream into
fragmented MP4 files, keeps the SQLite catalog, serves the HTTP API, subscribes to camera
events, enforces retention, checks its own recordings, and reports to systemd. The other
subcommands work on recordings offline. Nothing in the record path decodes video.

## What it provides

- `irisd run --config PATH` — the daemon. Options: `--stats-interval` (default 60 s),
  `--selfcheck-cycle` (default 24 h), `--no-sandbox` (run without Landlock; without it the
  daemon refuses to start on a kernel lacking Landlock ABI 4).
- `irisd record --config PATH --root DIR [--duration 10m] [--streams main,sub] [--cameras ID,…]`
  — record without the catalog, API, or events; the phase-1 spike and a quick capture tool.
  A missing `DIR` is created through directory descriptors and a symlink in any component of
  it is refused; recordings are mode 0600 and directories 0700 whatever the umask.
  The configuration is validated in full, so a camera-only file still states its API
  authentication (a token, or `api.allow_unauthenticated = true` on the loopback default).
- `irisd inspect FILE [--verify] [--samples]` — scan a recording and report its fragments,
  timing, and, with `--verify`, every trailer checksum.
- `irisd recover FILE` — truncate a recording at its last valid fragment and rewrite its
  sidecar index, keeping the prior header's known run id and sample-entry hash. A FIFO or
  symlink in place of the sidecar is replaced without blocking or being followed, and a file
  holding more than 131,072 fragments is refused unchanged.
- `irisd hls FILE --out DIR` — static byte-range HLS playlists for one recording.
  The output may be the source directory; linking preserves the recording itself.
  The command rejects reserved playlist basenames and generated-file aliases, percent-encodes
  the media basename, validates codec tokens, and publishes synced playlists by atomic entry
  replacement. Oversized, media-mismatched, or non-regular sidecars fall back to a media scan,
  and a recording holding more than 131,072 fragments is refused. `--exclude-trailers` emits one
  fragment per segment, including continuations, and independence is declared in the media
  playlist only when every segment starts at sync.
- `irisd serve --dir DIR [--listen ADDR]` — a static file server with range support for
  playback tests.
- The configuration file (see `config/cameras.example.toml`): `[storage]` (video roots, state
  directory, free-space floor, size quota, sync interval, fragment cap, preallocation,
  rotation), `[ingest]` (socket read interval, receive low-water mark), `[api]` (listen
  address, bearer tokens, the development-only unauthenticated switch), and one `[[camera]]`
  table per camera (RTSP URLs, credentials, vendor, streams to record, event sources,
  retention days, timestamp mode). Loading rejects storage values outside their documented
  ranges (`docs/research/07-architecture.md §10`), API tokens shorter than 32 visible ASCII
  characters or equal to the example placeholder, an empty token list without
  `api.allow_unauthenticated`, and that switch with tokens or on a non-loopback listener.
  Under the alpha policy (`STANDARDS.md` §9) a configuration that broke these rules stops
  loading; generate a token with `openssl rand -hex 16`.
- Statistics lines once per `--stats-interval`: one `stream` line per stream, one `events`
  line per feed, and one `writer`, `catalog`, `root` (per video root), and `process` line;
  one-off `gap`, `retention pass`, `slow write`, `slow sync`, and `selfcheck` lines. The
  `catalog` line includes event state, rows and MiB in deletion intent, database/WAL sizes,
  command queue count/bytes and peaks, coalescing, admission rejections, and the
  reconciliation-required flag. The `process` line includes maintenance timeouts per job and
  the sandbox enforcement level. The field reference is `docs/research/07-architecture.md §9`.
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
process cpu_pct=6.4 cpu_user_pct=2.9 cpu_sys_pct=3.5 rss_mib=92.6 threads=3 fds=63 uptime_min=14 loop_lag_max_ms=1 watchdog_withheld=0 retention_max_ms=3 selfcheck_ok=0 selfcheck_failed=0 selfcheck_skipped=0 report_timeouts=0 retention_timeouts=0 sandbox_enforced=2
```

`iris-tools daemon-stats` summarises a log; `iris-tools soak-sample` and `soak-report` run a
long soak beside the service; `iris-tools replay-bench` drives the daemon against replayed
cameras for cost measurements (`crates/iris-tools/README.md`).

## Invariants and limits

- Sandbox: before any thread exists, a Landlock ruleset restricts the daemon to reading its
  configuration directory, `/etc`, and `/proc`, reading and writing the video roots and the
  state directory, connecting to the RTSP ports the camera URLs name and each camera's HTTP
  port, and binding the API port (`sandbox.rs`; `docs/research/07-architecture.md §6`).
  Filesystem rights (Landlock ABI 1) and TCP bind/connect rights (ABI 4) are required: when
  the kernel lacks them the daemon refuses to start unless `--no-sandbox` is given. Newer
  rights up to ABI 7 apply when available, and a partially enforced ruleset is a warning
  naming the missing rights. The rules name ports for any address; the unit's
  `IPAddressAllow=` list restricts peers. The `process` line reports `sandbox_enforced`
  (0 disabled, 1 partial, 2 full).
- State and configuration permissions: before the catalog opens, the state directory (created
  0700 when missing) and the `irisd.db`, `-wal`, and `-shm` files must belong to the daemon's
  user and be a directory or a regular file with one link; symbolic links, hard links, and
  special files fail startup, and group or other permission bits are removed (0700 and 0600)
  with a warning naming the old mode. The configuration file, which holds camera passwords
  and API tokens, draws a warning when users other than the daemon's account and group can
  read or write it.
- Descriptors: startup raises the soft `RLIMIT_NOFILE` to the hard limit and logs both values;
  the unit sets `LimitNOFILE=65536`.
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
  Recovery stamps each open row's run and sample-entry hash into a rebuilt sidecar that lacks
  them, so recovered segments stay servable, and logs a binding that still differs. No
  reconciliation open blocks on a FIFO or device or follows a symlink: a special or symlinked
  sidecar is replaced by recovery and ignored by adoption, which scans the media instead, and
  removal unlinks it unopened. Files beyond the 131,072-fragment sidecar limit are left
  unchanged and are not adopted.
  Unavailable roots and filesystem lookup errors preserve pending deletion/open rows
  for a later start; only confirmed missing open files on available roots lose their rows.
  Confirmed absence is a missing media name or layout directory beneath the reverified root,
  whatever the row's fragment count; such a row enters deletion intent, its orphan sidecar is
  removed, and a warning counts the fragments lost.
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
  progress advance only after that transition completes. A row whose path is not in its
  root's `<camera>/<kind>/<date>/<file>.mp4` layout is never marked. After event maintenance,
  each pass first retries up to 16 earlier deletion intents on approved roots (or 100 ms,
  continuing where the previous pass stopped and wrapping after the last), so a cleared fault
  or a tombstone whose files are already absent stops counting toward the quota without a
  restart; rows on unavailable roots wait for startup.
- Periodic reporting, retention, and self-checks are owned single-flight jobs rather than work
  awaited inline by the monitor. Filesystem probes and removals run off the current-thread
  runtime on counted standard-library threads. A report or retention job that exceeds its
  30-second deadline is dropped, keeps its state, counts a timeout (`report_timeouts`,
  `retention_timeouts`), and does not stop recording. Linux provides no safe cancellation for
  an in-kernel `statvfs`, unlink, or directory sync, so such a call may outlive its job; the
  owner starts no further job while it is in flight (warning at most once a minute) and
  relaunches once it returns, keeping at most one raw call per owner. A call still in the
  kernel ten minutes after the timeout stops the daemon so systemd restarts it. The thread
  cannot hold process exit, and durable deletion intent makes a late removal retry-safe.
  A task failure or an unexpected cancellation still stops the daemon.
- The self-check visits one stream per cycle divided by the stream count, scans its newest
  sealed file with CRC verification, and compares both wall boundaries and the media extent with
  the catalog row. Its scan is cancellation-aware and participates in ordered shutdown. It opens
  the file without following a final symlink or blocking on a FIFO, and a file beyond the
  131,072-fragment limit fails the check.
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

Unit tests cover the endpoint and sandbox derivation from configuration, the sandbox
decision for every kernel status (without touching Landlock), state and configuration
permission checks and repairs, the descriptor-limit raise, maintenance timeouts that keep
state, raw calls that defer the next job and escalate, retention predicates (free-space
floor, quota, age cutoff), in-pass deletion-intent retries, layout checks before deletion,
the self-check comparison, the process sampler parsers, and start-up path parsing.
End-to-end behaviour is measured rather than
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
