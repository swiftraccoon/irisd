# irisd

The daemon and its command line. `irisd run` records every configured camera stream into
fragmented MP4 files, keeps the SQLite catalog, serves the HTTP API, subscribes to camera
events, enforces retention, checks its own recordings, and reports to systemd. The other
subcommands work on recordings offline. Nothing in the record path decodes video.

## What it provides

- `irisd run --config PATH` — the daemon. Options: `--stats-interval` (default 60 s),
  `--selfcheck-cycle` (default 24 h), `--no-sandbox` (run without Landlock; without it the
  daemon refuses to start on a kernel lacking Landlock ABI 4).
- `irisd record --config PATH --root DIR [--duration 10m] [--streams main,sub] [--cameras ID,…]
  [--stats-interval 10s]` — record without the catalog, API, or events; the phase-1 spike and a
  quick capture tool. It runs until SIGINT, SIGTERM, or the duration; every `--cameras` id must
  be configured. A missing `DIR` is created through directory descriptors and a symlink in any
  component of it is refused; recordings are mode 0600 and directories 0700 whatever the umask.
  The configuration is validated in full, so a camera-only file still states its API
  authentication (a token, or `api.allow_unauthenticated = true` on the loopback default).
- `irisd inspect FILE [--verify] [--samples]` — scan a recording and report its fragments,
  timing, and, with `--verify`, every trailer checksum.
- `irisd recover FILE` — truncate a recording at its last valid fragment and rewrite its
  sidecar index, keeping the prior header's known run id and sample-entry hash. A FIFO or
  symlink in place of the sidecar is replaced without blocking or being followed, and a file
  holding more than 131,072 fragments is refused unchanged.
- `irisd hls FILE --out DIR [--fragments-per-segment 3] [--exclude-trailers] [--codecs …]
  [--resolution WxH]` — static byte-range HLS playlists for one recording. The playlists
  describe a scan of the media; a sidecar that disagrees with the media or cannot be read is
  reported and changes nothing. The output may be the source directory; linking preserves the
  recording itself, and a file at the link's name that is not a link to the recording fails the
  command unchanged. The command rejects reserved playlist basenames and generated-file aliases,
  percent-encodes the media basename, validates codec tokens, declares the RFC 8216 peak segment
  bit rate as `BANDWIDTH` and the mean as `AVERAGE-BANDWIDTH`, and publishes synced playlists
  (mode 0600) by atomic entry replacement. A recording holding more than 131,072 fragments is
  refused. `--exclude-trailers` emits one fragment per segment, including continuations, and
  independence is declared in the media playlist only when every segment starts at sync.
  `--codecs` and `--resolution` are declared as given; their defaults describe a 4K main stream.
- `irisd serve --dir DIR [--listen ADDR]` — an unauthenticated static file server with range
  support for playback tests; it listens on every interface at port 8080 unless `--listen` says
  otherwise and stops on SIGINT or SIGTERM.
- The configuration file (see `config/cameras.example.toml`): `[storage]` (video roots, state
  directory, free-space floor, size quota, sync interval, fragment cap, preallocation, rotation),
  `[ingest]` (socket read interval, receive low-water mark), `[api]` (listen address, bearer tokens,
  the development-only unauthenticated switch), `[provisioning]` (cameras added through the API: the
  switch, the limit, the TCP ports they may use), and one `[[camera]]` table per camera (RTSP URLs,
  credentials, vendor, streams to record, event sources,
  retention days, timestamp mode, snapshot method). Loading
  rejects storage values outside their documented ranges (`docs/research/07-architecture.md §10`),
  API tokens shorter than 32 visible ASCII characters or equal to the example placeholder, an empty
  token list without `api.allow_unauthenticated`, and that switch with tokens or on a non-loopback
  listener. A configuration that breaks these rules does not load (`STANDARDS.md` §9 keeps no
  compatibility path); generate a token with `openssl rand -hex 16`.
- Statistics lines once per `--stats-interval`: one `stream` line per stream, one `events`
  line per feed, and one `writer`, `catalog`, `root` (per video root), and `process` line;
  one-off `gap`, `retention pass`, `slow write`, `slow sync`, and `selfcheck` lines. The
  `catalog` line includes event state, rows and MiB in deletion intent, database/WAL sizes,
  command queue count/bytes and peaks, coalescing, admission rejections, and the
  reconciliation-required flag; its statistics fields read `-` when the catalog query failed.
  The `process` line includes maintenance timeouts per job and the sandbox enforcement level,
  and precedes the catalog and root lines so a stalled query cannot suppress it. The field
  reference is `docs/research/07-architecture.md §9`.
- `packaging/systemd/irisd.service` and `packaging/sysusers.d/irisd.conf`: a hardened
  `Type=notify` unit with a watchdog, and the service account; `packaging/README.md` is the
  installation guide.

## How it is used

```sh
cargo build --release -p irisd
target/release/irisd run --config /etc/irisd/irisd.toml
```

The daemon logs to standard error (the journal under systemd) with `RUST_LOG` controlling the
level; an unparsable `RUST_LOG` is reported on standard error and the default `info` level
applies. Lines are plain text unless standard error is a terminal and `NO_COLOR` is unset or
empty, so the journal holds no ANSI escape sequences. A per-minute `process` line looks like this:

```
process cpu_pct=6.40 cpu_user_pct=2.90 cpu_sys_pct=3.50 rss_mib=92.6 threads=3 fds=63 uptime_min=14 loop_lag_max_ms=1 watchdog_withheld=0 teardowns=0 retention_max_ms=3 selfcheck_ok=0 selfcheck_failed=0 selfcheck_skipped=0 report_timeouts=0 retention_timeouts=0 sandbox_enforced=2
```

`iris-tools daemon-stats` summarises a log; `iris-tools soak-sample` and `soak-report` run a
long soak beside the service; `iris-tools replay-bench` drives the daemon against replayed
cameras for cost measurements (`crates/iris-tools/README.md`).

## Invariants and limits

- Sandbox: before any thread exists, a Landlock ruleset restricts the daemon to reading its
  configuration directory, `/etc`, and `/proc`; reading, creating, writing, renaming,
  truncating, and removing regular files and directories beneath the video roots and the state
  directory (no execution, device nodes, sockets, FIFOs, or symbolic links there); connecting
  over TCP to the RTSP ports the camera URLs name, each camera's HTTP port, and
  `provisioning.connect_ports`; and binding the API port (`sandbox.rs`;
  `docs/research/07-architecture.md §6`). Filesystem rights (Landlock ABI 1) and TCP
  bind/connect rights (ABI 4) are required: when the kernel lacks them the daemon refuses to
  start unless `--no-sandbox` is given. The rights and scopes of ABI 5 to 7 (`ioctl` on
  devices denied, abstract Unix sockets and signals scoped to the daemon's own domain) apply
  when the kernel has them, and a partially enforced ruleset is a warning naming the missing
  ones. UDP, pathname Unix sockets, and signals are outside the ruleset's filesystem and TCP
  rules; the rules name ports for any address, and the unit's `IPAddressAllow=` list
  restricts peers. The `process` line reports `sandbox_enforced` (0 disabled, 1 partial, 2
  full).
- Camera provisioning: with `[provisioning] enabled`, `POST /api/cameras` adds a camera and
  `DELETE /api/cameras/{camera}` removes one the API added. Added cameras are kept in
  `cameras.toml` in the state directory (mode 0600, rewritten through a temporary file) and
  restart with the daemon; they may use only the TCP ports in `connect_ports`, which the
  sandbox allows at startup, and count against `max_cameras`. A camera whose stream URL does
  not parse or whose port is outside `connect_ports` is refused before anything changes; the
  ports are checked when the camera is added, and a later change to `connect_ports` or
  `max_cameras` does not remove stored cameras. The store is rewritten off the runtime thread.
  Removal rewrites the store first and changes nothing when that write fails; the removed
  camera's tasks are joined within 5 s by a retirement task beside the monitor loop, then
  aborted. Configured cameras are never removed through the API, and the recordings and
  catalog rows of a removed camera stay. The writer queue keeps room for two streams per
  allowed camera, so a daemon with provisioning enabled may start without any configured
  camera and receive them all through the API. The per-minute `stream` and `events` lines and
  the systemd status text follow added and removed cameras from the next report on. The same
  control carries the viewer's ONVIF lookups: `GET /api/onvif/devices` runs a WS-Discovery
  probe (UDP multicast to 239.255.255.250:3702, so the unit's `IPAddressAllow=` must include
  `multicast` for the probe to leave the host) and `POST /api/onvif/streams` asks one camera's
  Media service for its RTSP URLs, on a port in `connect_ports` only. At most four lookups run
  at a time, in tasks beside the monitor loop; a lookup still running at shutdown is cancelled
  and answers with a lifecycle error.
- Media capabilities: with API tokens configured, the first start creates `capability.key` in
  the state directory (32 random bytes as 64 hexadecimal characters and a newline, mode 0600)
  and signs every playlist capability with it, so a restart keeps outstanding capability URLs
  valid; removing the file invalidates them, and a malformed file fails startup naming its path.
- State and configuration permissions: before the catalog opens, the state directory (created 0700
  when missing) and the `irisd.db`, `-wal`, `-shm`, `capability.key`, and `cameras.toml` files must
  belong to the daemon's user and be a directory or a regular file with one link; symbolic links,
  hard links, and special files fail startup, and group or other permission bits are removed (0700
  and 0600) with a warning naming the old mode. The configuration file, which holds camera passwords
  and API tokens, draws a warning when users other than root and the daemon's account and group can
  read or write it.
- Descriptors: startup raises the soft `RLIMIT_NOFILE` to the hard limit and logs both values;
  the unit sets `LimitNOFILE=65536`.
- Watchdog: the systemd ping is withheld whenever an ingest task made no progress in the
  interval, so a wedged daemon is restarted within `WatchdogSec`.
- Readiness: startup binds the configured API listener before recording tasks start and before
  `READY=1`. An occupied address fails startup without a false-ready notification; the already
  bound listener is handed to the API task for graceful shutdown. A stop signal that arrives
  during start-up is acted on once the monitor loop runs; until then systemd's stop timeout
  bounds the daemon while the start-up heartbeats keep extending it.
- Start-up resolves each configured root against a durable `.irisd-root-id` marker and catalog
  registry before Landlock or threads. A known root with a missing or changed marker is rejected;
  the admitted physical directory is pinned for that daemon run and physical aliases are rejected.
  Every unregistered root in a catalog migrated from before the registry fails closed—even when no
  segment row remains—until an operator verifies the intended mount and durably installs a 128-bit
  marker readable by the service account. Start-up then reconciles in fixed catalog pages and a
  streaming no-follow directory walk: durable deletion intents under available roots are completed
  first; files that were open at the last stop are recovered (an open file holding no fragment
  enters deletion intent and is removed), and unknown files are adopted from one header scan of
  the media, a sidecar bound to that scan supplying the same facts and any other sidecar being
  reported and ignored, with progress reported to systemd and the start timeout extended while
  it runs. One heartbeat whose cadence holds across every await, short or long, covers catalog
  pages, directory enumeration, and media scans. A directory that cannot be read is reported and
  skipped; its files wait for a later start. Recovery stamps each open row's run and sample-entry
  hash
  into a rebuilt sidecar that lacks them, so recovered segments stay servable, and logs a binding
  that still differs. No reconciliation open blocks on a FIFO or device or follows a symlink: a
  special or symlinked sidecar is replaced by recovery and ignored by adoption, which scans the
  media instead, and removal unlinks it unopened. Files beyond the 131,072-fragment sidecar limit
  are left unchanged and are not adopted. Unavailable roots and filesystem lookup errors preserve
  pending deletion/open rows for a later start; only confirmed missing open files on available roots
  lose their rows. Confirmed absence is a missing media name or layout directory beneath the
  reverified root, whatever the row's fragment count; such a row enters deletion intent, its orphan
  sidecar is removed, and a warning counts the fragments lost.
- Retention runs every minute: bounded event maintenance, the free-space floor per root, the size
  quota, then per-camera age limits. A pass stops at 512 deletion attempts, 64 candidate pages,
  1,024 raw scanned candidate rows, or one second. Candidate queries cap raw indexed input before
  protection filtering and advance after empty filtered pages; indexed SQL and the atomic deletion
  transition are the protection authority. A root whose free-space probe fails is skipped for the
  pass: no row beneath it is marked, and the other roots' free-space, quota, and age steps run;
  rows of roots that were not admitted are left alone and counted in one warning per pass.
  Free-space deletion stays within the pressured root; the first root rotates between passes so one
  persistently pressured device cannot consume every global budget. Exact-root, quota, and
  stream-age cursors retain the last handled candidate at a bounded-pass exit and use a raw page end
  only after every returned candidate is handled. Satisfied root/quota policies reset; age wraps
  only after indexed input is exhausted. Pagination uses split start-time/id seeks so recordings
  behind large equal-time or protected groups remain reachable. Each deletion atomically checks for
  active 30-second event protection and commits FULL deletion intent. Only `Marked` permits pair
  removal; the directory is synced before the row is removed. Counts and quota progress advance only
  after that transition completes. A row whose path is not in its root's
  `<camera>/<kind>/<date>/<file>.mp4` layout is never marked. After event maintenance, each pass
  first retries up to 16 earlier deletion intents on approved roots (or 100 ms, continuing where the
  previous pass stopped and wrapping after the last), so a cleared fault or a tombstone whose files
  are already absent stops counting toward the quota without a restart; rows on unavailable roots
  wait for startup.
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
- The self-check visits one stream per cycle divided by the stream count, scans its newest sealed
  file with CRC verification, and compares both wall boundaries and the media extent with the
  catalog row; a fragment without a `prft` fails the check. A camera added or removed through the
  API re-points it at the streams that record
  afterwards and re-times the cycle over them. Its scan is cancellation-aware and participates in
  ordered shutdown. It opens the file without following a final symlink or blocking on a FIFO, and a
  file beyond the 131,072-fragment limit fails the check.
- All CLI/config timer, capacity, and time conversions plus signal/monitor state are validated or
  armed before roots, listeners, workers, or readiness. Shutdown has one absolute 25-second
  service budget: producers, camera retirements, and ONVIF lookups share one grace interval,
  then writer, API, and catalog cleanup all run best-effort against the remaining deadlines, with
  every task outcome observed.
- Threads: the tokio current-thread runtime, one writer thread per root, and one catalog thread
  are long-lived; beside them run the counted maintenance threads (root probes, retention
  removals), the self-check's scan thread, tokio's blocking pool during start-up reconciliation,
  and the writer and catalog shutdown helpers, one call each. Budgets and measured costs are in
  `docs/research/07-architecture.md §7` and `12-ingest-cost.md`.
- The statistics line field names are parsed by `iris-tools daemon-stats`
  and `soak-report`; a renamed field changes those tools and `07-architecture.md §9`.
- Catalog lifecycle rejection sets `reconcile_required` and logs that startup filesystem
  reconciliation is required; event/progress rejection and query overload have separate fields.

## Tests and measurements

Unit tests cover the endpoint and sandbox derivation from configuration, the sandbox decision for
every kernel status, the rights a sandboxed thread keeps and loses (on a kernel with Landlock),
state and configuration permission checks and repairs, the descriptor-limit raise, start-up
validation up to the monitor initializer (including a provisioning-only daemon without cameras),
maintenance timeouts that keep state, raw calls that defer the next job and escalate, the lookup
permit bound and cancellation, camera retirement, report-list refresh, ordered shutdown within its
budget with first-error precedence, URL redaction, root admission, aliases, and fallback recording,
retention policy loops against admitted roots (free-space floor, quota, age, rotation, protected
prefixes, a skipped root after a failed probe), in-pass deletion-intent retries, layout checks
before deletion, start-up reconciliation (deletion intents, recovery, adoption, the no-follow
walk, heartbeat cadence), the self-check comparison and its cancellation, the process sampler
parsers, and the offline subcommands' argument checks. `tests/startup.rs` runs the binary:
readiness only after the listener binds, an occupied port, invalid periods, no sources, and a
signal right after readiness. Recording end to end is measured rather than tested: six and seven
live cameras over hours and the systemd soak in `docs/research/11-multi-camera-results.md`, kill
and recovery in `10-phase2-results.md`, and CPU per megabit against replayed cameras in
`12-ingest-cost.md`.

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
