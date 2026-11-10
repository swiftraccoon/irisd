# 07 — Daemon architecture (irisd)

Integrates 03–06 into one design. Numbers refer to cam1 measurements (01).

## 1. Data flow

```
              ┌──────── per configured camera and stream ────────┐
 RTSP/TCP ──▶ retina session ──▶ frame ──┬──▶ fragment assembler ──▶ bounded writer queue
                                         │    (the regulariser assigns           │
                                         │     durations when a fragment closes) │
                                         ▼                                       ▼
              live ring (Bytes, 64 MiB fast-start          writer thread per recording root:
                         + 64 MiB fan-out caps)            pwritev fragment, append sidecar,
                    │                 │                    fallocate ahead, fdatasync/5 s
 WS per-frame ◀─────┘                 └─────▶ LL-HLS parts               │
 fragments                                                               ▼
 HTTP ──▶ Dahua CGI event feed ──┐                         catalog thread (SQLite): segment
 SOAP ──▶ ONVIF PullPoint ───────┴──▶ event normaliser ──▶ and event rows; retention queries
                                                                         │
 HTTP/1.1 (axum) ◀── HLS playlists (byte ranges), MP4 export, keyframes, JSON API ◀───┘
```

Frames reach the live ring one at a time as they arrive, before the fragment they belong to
closes; the regulariser runs inside the assembler when a fragment closes.

Every byte of video is copied at most once in user space (socket read buffer → `Bytes` slice
shared by the live ring, the writer, and the WebSocket senders). The record path never
allocates per packet, decodes, or re-encodes video.

## 2. Workspace layout

```
irisd/
  Cargo.toml                 # workspace, profile release: lto=fat, codegen-units=1, panic=abort
  crates/
    iris-core/               # ids, config (TOML), time types (WallUs, MediaTicks), errors
    iris-h265/               # NAL classification, length-prefixed access units, parameter sets
    iris-mp4/                # fMP4 writer (ftyp/moov/prft/moof/mdat/trailer), reader/scanner, fragment rewrite export, mini-fragments
    iris-ingest/             # retina session supervisor, regulariser, fragment builder, live ring
    iris-store/              # writers, preallocation, sync, sidecar appends, recovery, removal
    iris-catalog/            # SQLite schema, migrations, queries (single owning thread)
    iris-events/             # Dahua CGI feed, ONVIF PullPoint, media lookups, discovery (oxvif)
    iris-api/                # axum: JSON, HLS/LL-HLS, WebSockets, export, auth
    irisd/                   # daemon with the retention policy; offline record, inspect,
                             # recover, HLS, and serve commands
    iris-tls/                # shared TLS client trust: platform roots plus an optional CA file
    iris-replay/             # bounded RTSP replay and synthetic-camera server
    iris-gui/                # GTK/GStreamer live, playback, thumbnails, and export client
    iris-tools/              # soak, replay-benchmark, probe, report, and commit tooling
  fuzz/                      # cargo-fuzz targets for the parsers (excluded from the workspace)
  docs/research/             # this research
```

The `hvcC` sample entry and the `CODECS` string come from retina (`mp4_sample_entry`,
`rfc6381_codec`). `iris-h265`, `iris-mp4`, and the event parsers in `iris-events` are pure,
allocation-light
libraries with bounded regression fixtures. `cargo-fuzz` targets (`fuzz/README.md`) cover their
parsers, the ONVIF notification normaliser, and the WS-Discovery answer mapping, the `iris-api`
timestamp, `Range`, and pre-authentication token parsers, the `iris-replay` RTSP request parser,
the `iris-tools` probe and log parsers, the viewer's VOD playlist rules, the vendored oxvif
SOAP/XML parser, and the vendored retina RTSP message parser, RTP
header validation, H.265 depacketization, and `DESCRIBE` interpretation with its SDP parsing.
`iris-store` has no fuzz target of its own; its recovery parses recordings and sidecars through
the fuzzed `iris-mp4` scanner and index codec. Retina's RTCP compound validation and its
`SETUP` and `PLAY` interpretation have no fuzz target; retina's own test suite
(`just vendor-test`) and the `iris-ingest` integration tests, which run sessions against
loopback replay servers and fault-injecting peers, exercise them.

## 3. Threads and tasks

| Executor | Work | Count |
|---|---|---|
| tokio `current_thread` | all sockets: RTSP sessions, event feeds, HTTP/WS server; timers: one shared read ticker for the timer-driven reads, per-session setup and keepalive deadlines, per-feed stall, renewal, and pull deadlines, and per-connection HTTP deadlines | 1 thread |
| `std::thread` writer per video root | `pwritev`, `fallocate`, `fdatasync`, sidecar appends | 1 per configured root |
| `std::thread` catalog | owns the SQLite connection; batches notifications for at most 100 ms; retention scans | 1 |
| bounded maintenance threads | root probes and retention removals (counted by `RawCalls`), the self-check scan, start-up reconciliation on tokio's blocking pool, and the writer and catalog shutdown helpers | single-flight per job type; one call each |

The daemon runs one such runtime, and the release profile sets `panic = "abort"`. A panic in any
task or thread therefore ends the process, which the unit restarts (`Restart=always`), and a
blocking call in any task stalls every task on the runtime: the camera sessions that feed
recording, the API, the event feeds, and the watchdog ping.

Backpressure rules: camera sockets are never blocked on disk or DB; the writer queue is
bounded by complete-fragment count. A rejected recording fragment ends continuity and
records one gap for the consecutive loss epoch; dependent recording frames are discarded
until a sync frame can start a run at decode time zero. Live publication continues.
The live fast-start cache holds at most 600 frame references and 64 MiB of payload from one
sync frame, and abandons that run on overflow. Subscriber fan-out stores a frame once for
all subscribers and is bounded by both its configured frame count and 64 MiB of payload per
stream. A subscriber that falls behind receives a lag result and resumes only at a sync
frame; without subscribers the fan-out retains no payload. The two live windows share
`Bytes` allocations where they overlap.
Configured roots have bounded queues and disk workers in an ordered fallback chain
sharing the writer statistics. Unavailable roots with a fallback remain retired; the
final root retries a sync fragment after a one-second cooldown and starts a discontinuity.
Shutdown propagates through the chain after all preceding forwarded fragments.
The catalog's single ordered admission domain is bounded to 2,048 commands and 16 MiB of
estimated owned input, including work in progress. Segment open/seal transitions reserve 128
commands and 1 MiB. Events reject on overload, same-path progress coalesces only in the
unqueried tail, and queries return an overload error without waiting behind the tail. Lifecycle
eviction cannot cross an admitted query. Reserve exhaustion latches a reconciliation-required
metric; explicit shutdown or dropping the final handle drains only the bounded accepted work
and joins the thread.
The GUI live input has 16 MiB and 512-buffer queue budgets checked before each nonempty payload
copy. Its WebSocket frame and assembled-message limits are the 16 MiB payload plus one protocol
type byte. Overflow, empty input, or an unknown type ends that feed session and reconnects from
initialization data and a sync frame without blocking a runtime thread or dropping arbitrary bytes
inside a media fragment.
Fragment assembly also has camera-independent bounds: 4,096 frames and 64 MiB pending
payload, with nominal and receipt-time caps when RTP timestamps stop advancing. The patched
H.265 depacketizer enforces separate 64 MiB pinned-payload and projected-output bounds while
an access unit is assembled, and production RTSP control messages have a 1 MiB head-plus-body
limit.

## 4. Core types

```rust
struct WallUs(i64);          // µs since Unix epoch, NVR CLOCK_REALTIME
struct MediaTicks(i64);      // 90 kHz, continuous within a run
struct CameraId(String); struct StreamKey { camera: CameraId, kind: StreamKind }
struct SegmentId(i64);       // iris_catalog

struct Frame { data: Bytes /* length-prefixed NALs, no parameter sets */, raw_ts: i64,
               wall: WallUs, is_sync: bool, new_params: bool }
struct Sample { data: Bytes, duration: u32, is_sync: bool }
struct Fragment { samples: Vec<Sample>, decode_time: u64, wall: WallUs, raw_rtp: u32,
                  new_run: bool, sample_entry: Arc<SampleEntry> }
struct SampleEntry { bytes: Bytes /* complete hvc1 entry */, codecs: String,
                     width: u16, height: u16, hash: u32 /* CRC-32C */ }
struct Event { camera: CameraId, source: EventSource, kind: EventKind, state: EventState,
               camera_time: Option<WallUs>, nvr_time: WallUs, data: Option<String> }
```

Event data is valid JSON text capped at 64 KiB: invalid vendor text is quoted and an over-limit
encoding becomes `{"truncated":true}`. An unmapped vendor kind code is capped at 1 KiB on a
UTF-8 boundary. Catalog maintenance bounds inactive event state by recording horizon, age,
rows, and logical string bytes while preserving capacity-limited active retention marks.

## 5. API surface (0.1 alpha)

| Method | Path | Notes |
|---|---|---|
| GET | `/api/cameras`, `/api/cameras/{camera}` | the running cameras (`provisioned` marks those the API added) and per-stream health |
| POST | `/api/cameras` | adds a camera described like a `[[camera]]` table; needs `[provisioning] enabled` and the root token (201) |
| DELETE | `/api/cameras/{camera}` | stops and forgets a camera the API added (204); a configured camera answers 409 |
| GET | `/api/onvif/devices` | the cameras a bounded WS-Discovery probe finds, for the Add camera form; root token, `[provisioning] enabled` |
| POST | `/api/onvif/streams` | the RTSP URLs of one camera's ONVIF profiles, moved onto the given host; same gating, port in `connect_ports` |
| GET | `/api/streams/{camera}/{kind}/segments?from=&to=` | catalog listing (wall ranges, runs, sample entries) |
| GET | `/api/streams/{camera}/{kind}/hls/live.m3u8` (+ `_HLS_msn/_HLS_part`) | LL-HLS playlist and in-memory child resources |
| GET | `/api/streams/{camera}/{kind}/hls/vod.m3u8?from=&to=` | byte-range playlist across segments |
| GET | `/api/segments/{id}/file.mp4` (Range) | one bounded snapshot of the on-disk file |
| GET | `/api/streams/{camera}/{kind}/export.mp4?from=&to=` | assembled MP4 |
| GET | `/api/streams/{camera}/{kind}/keyframe?t=` | init + one single-sample fragment |
| GET | `/api/cameras/{camera}/snapshot.jpg` | proxied from the camera: the Dahua CGI endpoint, or the URL its ONVIF Media service names (`snapshot`, default by event source); one request per camera at a time |
| WS | `/api/streams/{camera}/{kind}/live` | init + per-frame fragments, each message ≤ 16 MiB + 1 byte |
| WS | `/api/events` | push of normalised events |
| GET/POST | `/api/events?from=&to=&after_us=&after_id=`, `/api/events/{id}/retain` | bounded `(time,id)` event pages, pin |
| GET | `/api/health`, `/metrics` | liveness `{status, uptime_s, version}`, Prometheus text (root token unless `[api] public_metrics`) |

The camera list and detail routes describe the cameras the daemon runs, configured or added through
the API; catalog rows for removed cameras remain available through historical recording routes.
Camera provisioning is off unless `[provisioning] enabled`. An added camera is validated like a
configured one, must use TCP ports in `provisioning.connect_ports` (the sandbox allows those at
startup, §6), counts against `provisioning.max_cameras`, is registered in the catalog, persisted to
`<state_dir>/cameras.toml` (mode 0600, rewritten through a temporary file) so it restarts with the
daemon, and runs its streams and event feeds under a cancellation token of its own; its stream URLs
must parse and its ports must be allowed before anything changes, and the store is written off the
runtime thread. Removal rewrites the store first (a failed write changes nothing), cancels that
token, hands the tasks to a retirement task that joins them within 5 s and aborts the rest, forgets
the camera in the live registries, and keeps its recordings and catalog rows. Configured cameras are
never removed through the API. Only the root bearer authorizes these routes, media capabilities
never do, and request bodies on protected routes are limited to 64 KiB. The writer queue is sized
for the configured streams plus two per allowed camera, so a daemon with provisioning enabled may
start without any configured camera. The Add camera form's ONVIF lookups (discovery and stream URLs,
`05 §5`) relay through the same control and run in tasks beside the loop, at most four at a time,
the daemon enforcing `connect_ports` on the lookup's port; a lookup still running at shutdown is
cancelled and answers with a lifecycle error. `live_viewers` is the per-stream count of external
live WebSockets and excludes the shared LL-HLS feeder. Its drop guard removes a viewer on close,
timeout, cancellation, and send failure. The segments listing includes an unsealed segment only once
it has a durable fragment and treats it as extending to the earlier of now and 30 s past its last
durable progress, while each row reports its recorded `wall_end_us`.

Request time parameters accept Unix microseconds or ASCII RFC 3339. Non-ASCII timestamps
are rejected before fixed-position parsing. Dates follow proleptic Gregorian month and leap-year
rules; clock and numeric-offset fields use RFC 3339 bounds. Leap seconds are rejected because the
recorder has no leap-second table. Checked subtraction enforces the seven-day range limit even at
signed-integer boundaries.
Keyframe requests reject timestamps whose two-minute lookback or exclusive end cannot
be represented, returning HTTP 400 instead of overflowing endpoint arithmetic.
VOD includes complete sync preroll and one ignored
`#IRIS-FRAGMENT:WALL-US=<microseconds>,DURATION-TICKS=<90000-Hz ticks>` comment per served
fragment. The GUI fetches one immutable playlist for both timing metadata and playback,
serving its body through a private loopback capability URL while resolving media URIs
against the original API URL. It accumulates media ticks before conversion, so window
boundaries and gaps agree with the player. Exports reject ranges spanning
different codec configurations before streaming begins, verify source checksums, and
trim sync preroll with an edit list. LL-HLS carries discontinuity markers and a
discontinuity sequence across reconnects and sliding-window eviction.
Exports also omit samples starting at or after the requested end and shorten a crossing
sample's duration, regenerating only the final fragment's boxes and checksum. Retained
sample bytes are unchanged; unwanted tail frames are absent even in players that do not
enforce the edit-list end.
Positive export tails round up to media ticks, with edit durations rounded up to
milliseconds from the serialized media span, preserving even sub-tick requests.
Disk-backed routes open a row's media and sidecar together beneath the approved recording root
that owns its path, refusing symlinks and mount crossings, and validate the bounded sidecar read
from its descriptor against catalog, header, and physical-media facts, capturing one complete
published extent and the media's device and inode. A row outside every approved root is not
served (HTTP 404, counted). Raw full/range bodies come from the validated descriptor; exports and
keyframes reopen beneath the root and require the same inode and extent; an early end of file
aborts a raw body with an error. Full/range bodies and VOD ranges never read past that
snapshot. A sidecar header whose run identifier or sample-entry hash is zero, as crash recovery
rebuilds it, is unknown rather than a mismatch. Serving admits at most 4,096 catalog rows,
131,072 sidecar records per file, 100,000 selected records, a 16 MiB playlist, a 4 MiB init,
and a 66 MiB fragment. Media responses are admitted process-wide in two classes: streaming
(whole or open-ended files, ranges above 1 MiB, exports; `[api] max_streaming_media`, default 8)
and bounded (VOD playlists, keyframes, the memory-served LL-HLS resources, ranges of at most
1 MiB; `[api] max_bounded_media`, default 32). A full class answers HTTP 503 with `Retry-After: 1`
without affecting the other. Admission remains held through detached sidecar/read/encode work and
final response-buffer/body drop, and a client that stops reading loses it at the 30 s write-stall
deadline; range-capable players resume with `Range`. Raw bodies reserve their capacity-one
channel before each bounded read and attach that admission to every emitted chunk. An export
reserves its sole queue slot before materializing the next bounded fragment.
Snapshot responses stream through an 8 MiB ceiling, accept a body that starts with `FF D8 FF` and
has an end-of-image marker within its last 64 KiB, are served through that marker, and use a
fixed non-sniffable, non-cacheable media type. One snapshot request per camera is in flight; a
second answers HTTP 503 with `Retry-After: 1`. The camera request never follows redirects,
including same-origin redirects, and errors returned to clients omit the camera URL. Live and event
WebSocket writes observe shutdown and stop after a two-second send deadline; client accounting uses
drop guards so cancellation and peer failures release it. At most 64 live sessions (16 per
stream) and 32 event sessions are admitted, with HTTP 503 and `Retry-After: 1` beyond the caps.
Clients send no data messages: inbound messages and frames are limited to 4 KiB, and a data
message or a violation closes the session with 1008. A live message never exceeds one type byte
plus 16 MiB, the viewer's limit; an oversized dependent frame resumes at the next sync frame, and
an oversized sync frame closes the socket with 1009.

The listener serves HTTP/1.1 only, at most `[api] max_connections` connections at once (default
256, upgraded WebSockets included); the accept loop takes a slot first, so excess clients wait in
the kernel backlog instead of consuming descriptors. A request head must arrive within 10 s,
which also closes idle kept-alive connections, and a response write without progress for 30 s
closes its connection. HTTP 500 bodies carry only `"internal error"` and a `request_id` that
matches the logged detail.

Auth: bearer tokens protect every route except `/api/health`, and `/metrics` unless `[api]
public_metrics` is set. The server speaks plain HTTP; bind it to loopback or a trusted private
network and use a VPN or TLS reverse proxy for remote access. The Linux viewer works through such a
proxy: an `https` API URL gives `wss` live feeds, both verified with one `iris-tls` configuration
(the platform's trust store plus the authorities of an optional `--ca-file`, as the soak sampler's
API checks use too), and the viewer refuses plain HTTP to a host other than loopback unless started
with `--allow-insecure-http`. Authenticated playlists place only bounded tamper-resistant
capabilities in media URIs. Each capability is limited to its exact path, `GET`/`HEAD`, and expiry;
the root bearer is never copied into a playlist URL. A capability is an HMAC-SHA256 tag under a
32-byte secret the daemon creates once at `<state_dir>/capability.key` (mode 0600) and reuses across
restarts; the root tokens take no part in it, so a captured URL reveals nothing about a token, and
every token issues interchangeable capabilities. Rolling live-HLS capabilities last 15 minutes.
Immutable VOD children cover the selected span plus a 15-minute playback margin, capped at seven
days. Configuration admits bearer tokens of at least 32 visible ASCII characters (0x21 to 0x7e)
other than the example placeholder; an API without tokens needs `api.allow_unauthenticated = true`
and a loopback listener, and the switch together with tokens is rejected.

## 6. Security model

- Cameras are untrusted peers: every parser (RTSP, SDP, RTP, H.265 headers, multipart event
  feed, SOAP XML, JSON) has size caps, and a stream's errors end only its own session, which
  reconnects with jittered backoff. §2 lists the parsers with fuzz targets, and §3 describes
  the limits of one shared runtime. RTSP control messages are capped at 1 MiB
  for head and body together; in-progress H.265 access-unit payload and projected output are
  each capped at 64 MiB before a completed frame allocation.
- systemd hardening set + Landlock (03 §5): the process reads its configuration directory,
  `/etc`, and `/proc`; reads, creates, writes, renames, truncates, and removes regular files and
  directories beneath the video roots and the state directory (no execution, device nodes,
  sockets, FIFOs, or symbolic links there); connects over TCP to the camera and provisioning
  ports; and binds the API port. Landlock filesystem rights (ABI 1) and TCP bind/connect rights
  (ABI 4) are required: on a kernel without them the daemon refuses to start unless
  `--no-sandbox` is given. The rights and scopes of later ABIs (device `ioctl` denied, abstract
  Unix sockets and signals scoped to the daemon's domain) are applied when available, with a
  warning naming any missing ones. UDP (the WS-Discovery probe), pathname Unix sockets
  (`sd_notify`), and signals are outside the ruleset. Landlock filters TCP ports for every
  destination address; peers are restricted by the unit's `IPAddressDeny=any` and
  `IPAddressAllow=`, which allows only the loopback until a drop-in adds the camera subnet
  (and the API clients' subnet when the API listens on the LAN).
- Secrets (camera passwords, API tokens) live in the configuration file read at startup,
  never in URLs or logs. The packaging installs it 0640 root:irisd in a 0750 directory, and
  the daemon warns when users other than root and its account and group can read or write it.
  `CameraConfig`'s `Debug` output redacts the password.
  Configuration parse errors retain a generic syntax/schema description and byte span;
  they discard source text and value-bearing parser messages before crossing the core API. The
  camera store `cameras.toml` is parsed the same way, so a malformed store names the file and a
  byte location, never its text.
- State at rest: the unit's `StateDirectoryMode=0700` and a startup check keep the catalog
  (camera metadata, recording paths, events), the capability secret, and the credentials of
  cameras added through the API (`cameras.toml`) private to the service account. Before SQLite
  opens the catalog, the daemon requires the state directory, the `irisd.db`, `-wal`, and `-shm`
  files, `capability.key`, and `cameras.toml` to belong to its user, refuses
  symbolic links, hard links, and special files, and
  removes group and other permissions with a warning; SQLite copies the database file's mode
  to new WAL and shared-memory files, so repairing the database file keeps them private.
- API clients are bounded before and after authentication: the connection cap, the 10 s
  request-head deadline, and the 30 s write-stall deadline hold for unauthenticated peers;
  media classes, WebSocket socket caps with 4 KiB inbound limits, and per-camera snapshot
  single-flight hold for authenticated ones. Media bytes come only from descriptors opened
  beneath approved recording roots, and HTTP 500 bodies carry no internal detail.
- Supply chain: `cargo audit` on every committed lockfile and `cargo deny` (licenses +
  advisories) on the root and fuzz graphs (`just audit`), lockfiles committed, minimal
  feature sets.
- The Linux viewer's Rust code never parses video, but GStreamer's native demuxers, parsers,
  decoders and drivers parse camera-originated media inside the viewer process without a
  sandbox, outside Cargo's auditing.

## 7. Efficiency budgets and benchmark harness

Targets for 6 cameras × (main 4K 8 Mbps + sub D1 1.3 Mbps) ≈ 56 Mbps, ≈ 5,000 packets/s:

| Metric | Budget | How measured |
|---|---|---|
| CPU | ≤ 2 % of one core steady state (≤ 0.15 % per stream) | `perf stat -e task-clock`, cgroup `cpu.stat` over 10 min |
| RSS | ≤ 64 MB | `/proc/<pid>/status` VmRSS, `smem` |
| Syscalls | ≤ 1,500/s | `perf trace -s` / `strace -c` sample |
| Wake-ups | ≤ 1,000/s | `powertop`, `perf sched` |
| Write amplification | ≤ 1.01 | `/proc/diskstats` sectors written vs bytes received (RTP payload counter) |
| Container overhead | ≤ 0.1 % | bytes on disk vs payload bytes |
| Live latency | ≤ 300 ms WS, ≤ 3 s LL-HLS | camera pointed at a millisecond clock |
| Seek | < 1 s to any time in 90 days | API timing |
| Crash loss | ≤ 2.5 s plus the writer queue (daemon), ≤ 7.5 s (power) | `kill -9` and power-cycle tests, count missing frames |
| Recovery | < 5 s with 12 open files | startup timing |

The RSS figure is a steady-state operating target, not the sum of adversarial ceilings.
The per-stream fragment and live-ring limits make camera- and subscriber-controlled payload
retention finite even when a stream reaches its declared worst case.

Measured against this budget on 2026-11-01 (12-ingest-cost.md): 0.147 % per Mbit/s with
packet-driven reads on replayed cameras (0.166 % live), 0.113 % with the 20 ms timer-driven
reads, 0.069 % with the 32 KiB receive low-water mark on top, and 0.064 % with the SIMD
CRC-32C; the remaining kernel items are listed in 12 §7. Measured again on
2026-11-02 after one shared read ticker and sockets detached from the reactor (12 §6):
0.021 % per Mbit/s at 132 Mbit/s, under the budget, with 70 wake-ups and 930 system
calls per second for fourteen streams. Measured on 2026-11-08 with the build that bounds every
camera-supplied diagnostic (12 §6): 0.024 to 0.025 % per Mbit/s at 143 Mbit/s. RSS measured on
2026-11-10 (12 §8): 46 to 47 MiB for six replayed streams at 61.5 Mbit/s, inside the budget,
and about 12 MiB plus 0.55 MiB per Mbit/s across one to fourteen streams.

Harness (`iris-replay` and `iris-tools replay-bench`):

1. **Replay server**: `iris-replay`, a minimal RTSP server in the workspace that serves
   recordings (or synthetic streams) as any number of cameras over TCP-interleaved RTP,
   re-packetized per RFC 7798 with in-band parameter sets, RTCP sender reports, and
   decode-time pacing; `--drop-every` and `--stall-after` inject faults.
   Mount/media/timeline/MTU admission is bounded and fail-closed, and each RTSP session is
   bound to its selected mount, negotiated interleaved channels, and issued Session value.
   `iris-tools replay-bench` drives the daemon against N replayed cameras.
2. **Baselines on the same box**: `ffmpeg -c copy` to fMP4, Moonfire NVR 0.7.33, Frigate
   record-only; same inputs, same metrics.
3. **Real cameras**: the six units, 24 h soak with metrics scraped every 10 s.
4. **Chaos**: SIGKILL mid-fragment, disk full, slow disk (`dm-delay`), camera reboot, NVR
   clock step, parameter-set change via CGI, network partition; each with a pass criterion
   (no corrupt files, gap recorded, recovery time, catalog consistent).
5. **Fuzzing**: `cargo-fuzz` corpora seeded from the captured packets and SDP/SOAP/multipart
   samples.

## 8. Testing strategy

- Unit tests beside the code, among them the regulariser's scenario tests (duplicate stamps,
  gaps, drift, wrap-around, resets) and a regression test that replays a committed sidecar of
  a real recording (`crates/iris-ingest/tests/data/`, timing only).
- Recovery tests over truncated and zero-filled tails (`crates/iris-store/tests/store.rs`
  and the `iris-mp4` reader's tests).
- Integration tests that run ingest sessions against loopback replay servers and
  fault-injecting RTSP peers (`crates/iris-ingest/tests/`), and the API's routes, limits, and
  WebSocket lifetimes against a test catalog (`crates/iris-api/tests/`).
- The daemon binary end to end (`crates/irisd/tests/`): start-up, readiness, and shutdown
  order, and one run that records a loopback camera served by the replay server, lists the
  open recording through the authenticated API, and on SIGTERM leaves a recording that
  scans clean, a sidecar bound to it, and a sealed catalog row with the same length and
  fragment count.
- Fuzz targets for every parser that faces the network (§2, `fuzz/README.md`).
- Recordings checked with `ffprobe`, `irisd inspect`, and GStreamer playback.
- The writer on a filesystem that fills up: a small tmpfs in a user namespace returns the real
  failed preallocations, short writes, and `ENOSPC` (`crates/iris-store/tests/full_disk.rs`).
- Seeded property tests of the regulariser (`crates/iris-ingest`) and of generated recordings
  through the encoder, scanner, memory parser, and sidecar record
  (`crates/iris-mp4/tests/properties.rs`), and of recovery after generated damage to a written
  recording and its sidecar (`crates/iris-store/tests/recover_properties.rs`), of the writer
  over generated fragment sequences through rotation, of the fragment
  assembler over generated camera timelines, of the LL-HLS part and segment targets under
  changing frame durations, of exports of generated recordings over generated ranges, and
  model tests of every retention statement's event protection and of the event pages and
  segment listings (`crates/iris-catalog/tests/retention_model.rs`, `query_model.rs`).
- Mutation testing on demand (`just mutants FILE...`): a mutant of the format, timing, or
  parsing code that every test still passes names logic no test pins down. The first runs
  (2026-11-10) led to the removal of a regulariser rule that no test needed and that
  measurements then showed to do harm (`11-multi-camera-results.md`).
- The tests of the crates without C dependencies as 64-bit ARM code under user-mode
  emulation (`just cross-test`, CI).
- Not implemented: writer tests with injected read and write errors (`EIO`), captured-camera
  golden tests, and `mediastreamvalidator` and AVPlayer conformance (`08-roadmap.md`, phase-2
  exit criteria).

## 9. Observability

`tracing` with structured `key=value` fields to journald. Once per `--stats-interval` (default 60 s)
the daemon writes one line per stream, one per event feed, one `writer`, one `catalog`, one `root`
per video root, and one `process` line. The stream and event-feed lists follow cameras added or
removed through the API from the next report on. Latency fields are per-interval p50/p99 upper
bounds from fixed logarithmic histogram buckets (`iris_core::metrics`) plus the interval maximum, so
the hot paths only increment atomics. Counts and percentile ranks come from the published bucket
samples. Open-bucket percentiles use a cumulative maximum, published before each bucket increment
and independent of interval maximum resets. One reporter owns those resets. Snapshots are
approximate across concurrent updates, so interval cutoffs and means can be skewed; percentile
bounds still cover their observed bucket samples.

| Line | Fields |
|---|---|
| `stream` | connected, frames, fragments, payload MiB, reconnects, snaps, loss markers, dropped, regulariser holds and hold seconds, nominal fps, timeline error ms, peak fragment KiB and seconds, RTCP sender reports and camera-minus-NVR clock offset ms, gaps, seconds since the last frame, codec |
| `events` | camera and source identity; connected, events, messages (CGI parts including heartbeats; ONVIF successful pulls, empty ones included), failed-session retry cycles (`reconnects`), renewals, failed sessions (`errors`), seconds since the last message, pull p99/max ms (ONVIF), last event time |
| `writer` | fragments, file MiB, syncs, dropped, write errors, files, queue depth and peak, peak queued MiB, write p50/p99/max ms, sync p50/p99/max ms, slow I/O count |
| `catalog` | segments and sealed GiB; rows and MiB in deletion intent (`deleting_segments`, `deleting_mib`); event rows, logical MiB, active marks, and pruned rows; database/WAL MiB (these statistics fields read `-` when the catalog query failed, which is logged); live/peak command count and owned MiB; coalesced and total/per-kind rejected commands; reconciliation-required flag; batches, changes, peak batch, failures; transaction, query, queue-wait, and event receipt-to-commit p99/max ms |
| `root` | free GiB and percent per video root |
| `process` | CPU total, user, and system percent, RSS, threads, fds, uptime, loop lag max ms (how late a 250 ms timer fired on the runtime thread), watchdog pings withheld, background `TEARDOWN` tasks running (`teardowns`, at most 64), retention pass max ms, self-check ok, failed, and skipped counts, statistics-report and retention-pass timeouts since start (`report_timeouts`, `retention_timeouts`), Landlock enforcement (`sandbox_enforced`: 0 disabled, 1 partial, 2 full); written before the `catalog` and `root` lines, whose queries may stall |

One-off lines mark the facts a soak must explain: `gap` (`cause=reconnect` with the last
frame time before the session ended and the first after it, `cause=camera_hold` for a
regulariser hold of a second or more, or `cause=sink_rejected` for the first writer rejection
in a loss epoch), `retention pass` (deleted, earlier deletion intents finished, freed, duration,
the smallest free percent any probe of the pass saw, catalogued GiB after the quota step against
the quota, and `roots_unavailable`, the roots skipped after a failed probe, with `-` for a value the
pass could not know; a pass over 2 s is a warning),
`slow write` and `slow sync` (over 1 s),
and `selfcheck ok` or `selfcheck failed`. The self-check visits one
stream per `--selfcheck-cycle` (default 24 h) divided by the stream count, scans that
stream's newest sealed file with CRC verification on a blocking thread, and compares
fragments, bytes, init length, media span, and both wall boundaries with the catalog row; a
fragment without a `prft` fails the check. A camera
added or removed through the API re-points it at the streams that record afterwards and
re-times the cycle over them.

`/api/health` and `/metrics` (Prometheus text: API requests, clients, skipped event-WebSocket
messages, API admission rejections by media class, WebSocket cap, and snapshot single-flight, rows
outside the approved roots, oversized live closes, uptime, catalog queue, rejection and failed-batch
state, deletion intent, event capacity/pruning, and database/WAL sizes) serve pollers; `/metrics`
requires the root bearer token unless `[api] public_metrics` is set. Uptime uses a monotonic process
instant. Metric scrapes share one cache-owned catalog-aggregate refresh at most once per second;
concurrent scrapes coalesce and request cancellation cannot abandon the refresh. A failure serves
the last snapshot or omits cold aggregates until the next cadence. The `systemd` watchdog ping is
withheld whenever an ingest task made no progress in the interval. Figures the daemon cannot see
from inside its sandbox (disk sectors written for write amplification, pressure stall information,
NIC and TCP counters, NTP offset, the service's cgroup CPU and memory split) come from `iris-tools
soak-sample`, which runs from a timer beside the service. Its bounded API check also captures the
catalog failure, rejection, and reconciliation metrics so a healthy HTTP response cannot hide failed
persistence. Soak reports recognize the ordered startup prefix (the configuration-exposure and
unauthenticated-API warnings, the descriptor limit, state-mode repairs, root availability, the
sandbox decision, catalog open) before reconciliation as a run boundary, including after crashes
without a `stopped` line; `iris-tools` matches those messages by their text, so a reworded message
changes there in the same change. Reconciliation remains a fallback marker when the prefix is absent. Startup warnings belong to
the restarted run and cannot extend the prior run's last observation. Without a clean stop or
process identity in the exported log, the interval between runs remains unobserved / possible
downtime; the report cannot infer the exact crash time. An API task whose result was consumed by the
monitor is removed before shutdown; a bind failure therefore still drains the writer and catalog
without polling it twice. Startup reconciles storage and then binds the API listener before
recording tasks and `READY=1`. The already-bound listener is passed to the API task, so an occupied
address cannot produce a false-ready daemon.

## 10. Configuration sketch (TOML)

```toml
[storage]
video_roots = ["/srv/irisd/video"]
state_dir = "/var/lib/irisd"
min_free_percent = 5
sync_interval_ms = 5000
max_fragment_ms = 2000
preallocate_bytes = 268435456
rotate_bytes = 1073741824
rotate_secs = 3600
max_queued_fragments = 32

[ingest]
read_interval_ms = 20
recv_low_water_kib = 32

[api]
listen = "127.0.0.1:8554"
tokens = ["replace-with-a-long-random-token"]   # rejected; use `openssl rand -hex 16`

[provisioning]
enabled = false
max_cameras = 16
connect_ports = [554, 80, 443]

[[camera]]
id = "cam1"
name = "Example"
rtsp_url = "rtsp://192.0.2.10:554/cam/realmonitor?channel=1&subtype=0&unicast=true&proto=Onvif"
# subtype=2, the third stream, on a family whose sub stream stops beside a second session (01 §5b)
sub_url  = "rtsp://192.0.2.10:554/cam/realmonitor?channel=1&subtype=1&unicast=true&proto=Onvif"
username = "..."
password = "..."
vendor = "dahua"
record = ["main", "sub"]
http_port = 80
https = false
tls_verify = true
retention_days = 21
timestamp_mode = "regularize"
events = ["dahua_cgi", "onvif_pull_point"]  # each source appears at most once per camera
```

Configuration admission accepts `max_fragment_ms` in `1..=60,000`,
`max_queued_fragments` in `0..=4,096` (zero means one usable queue slot), and
`retention_days` in `1..=106,751,991`. The storage knobs whose out-of-range values silently
break durability, stop recording, or delete every recording are bounded too:
`sync_interval_ms` in `100..=60,000` (it bounds the power-loss window), `rotate_secs` in
`60..=86,400`, `rotate_bytes` in 64 MiB..=64 GiB (at least the 64 MiB fragment cap),
`preallocate_bytes` up to the smaller of `rotate_bytes` and 4 GiB (reserved space counts as
used for the free-space floor), `quota_gib` at least 1 when set, and `min_free_percent` in
`0..=50`. `[provisioning]` needs `max_cameras` in `1..=64`, no zero port, and a nonempty
`connect_ports` when enabled; a daemon with it enabled may have no `[[camera]]` at all. The API
section needs a bearer token of at least 32 visible ASCII characters other than the example
placeholder, or `allow_unauthenticated = true` with no token on a loopback listener (§5). Each
rejection names the key and its accepted range; a configuration that breaks a rule does not load
(`STANDARDS.md` §9 keeps no compatibility path). Derived queue and
media-tick arithmetic is checked before roots, listeners, writers, or tasks are created.
Repeated stream selectors in the standalone `record` command are rejected rather than
starting two producers for one key.

Before the daemon creates a worker thread, it opens the catalog synchronously, resolves durable
legacy-registry state, and admits each recording root against the durable `.irisd-root-id` marker
plus its process-local physical directory identity. Known or ambiguous legacy roots are never
initialized when missing or replaced—even if a migrated catalog has no remaining segment
rows—physical aliases are rejected, and subsequent reconciliation/removal remains
descriptor-relative beneath the exact approved root. A missing fresh root is created through
directory descriptors from the deepest ancestor that exists without following a link, and admission
continues from the created descriptor. Every open of a marker, recording, or sidecar beneath a root
is nonblocking, no-follow, and regular-file only, so a planted FIFO or device fails that open at
once instead of stalling admission, the writer, reconciliation, or shutdown. Startup catalog scans
use fixed pages and the root walk streams no-follow entries; an elapsed heartbeat extends the
service start deadline through all pages, enumeration, and media scans.

The process monitor owns one reporting job, one retention job, and one self-check job. They are
single-flight and cancellation-aware, while blocking filesystem probes/removal run outside the
current-thread runtime on standard-library threads that each owner counts. Their completions are
selected alongside signals and watchdog ticks. A report or retention job that exceeds its
30-second deadline is dropped and its state kept for the next launch; the timeout is counted in
the `process` line and recording continues. Awaiting the catalog is cancellable, but an in-kernel
`statvfs`, unlink, or directory sync is not, so its thread may outlive the job: while such a call
is in flight the owner starts no further job (warning at most once a minute), which keeps at most
one raw call per owner and never races a late removal, and it relaunches once the call returns.
A call still in the kernel ten minutes after the timeout stops the daemon so systemd restarts it;
a task failure or an unexpected cancellation also stops it. The thread cannot hold process exit,
and deletion intent makes a late removal retry-safe in a later pass or at the next startup.
SIGTERM/SIGINT shutdown shares one 25-second absolute budget across all producers (camera
retirements and ONVIF lookups included), writer sealing, API drain, and catalog closure; aborted
tasks are awaited and all later cleanup stages run even when an earlier one fails.
