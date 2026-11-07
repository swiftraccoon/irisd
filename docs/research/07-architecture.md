# 07 — Daemon architecture (irisd)

Integrates 03–06 into one design. Numbers refer to cam1 measurements (01).

## 1. Data flow

```
                 ┌──────── per configured camera ────────┐
 RTSP/TCP ──▶ retina session (main) ──▶ regulariser ──▶ fragment builder ──┐
 RTSP/TCP ──▶ retina session (sub)  ──▶ regulariser ──▶ fragment builder ──┤
 HTTP     ──▶ Dahua CGI event feed ─────────────────────────────┐          │
 SOAP     ──▶ ONVIF PullPoint ──────────────────────────────────┼─▶ event normaliser ──▶ catalog, WS push
                                                                           ▼
                                                     live ring (Bytes, 64 MiB fast-start
                                                                + 64 MiB fan-out caps)
                                                        │                 │
                        WS per-frame fragments ◀────────┤                 ├──▶ bounded writer queue
                        LL-HLS parts            ◀────────┘                 │
                                                                           ▼
                                                  writer thread per recording root: pwritev fragment,
                                                  append sidecar, fallocate ahead, fdatasync/5 s
                                                                           │
                                                                           ▼
                                     catalog thread (SQLite): segment/run/event rows, retention
                                                                           │
 HTTP/1.1+2 (axum) ◀── HLS playlists (byte ranges), MP4 export, keyframes, JSON API ◀──┘
```

Every byte of video is copied at most once in user space (socket read buffer → `Bytes` slice
shared by the live ring, the writer, and the WebSocket senders). The record path never
allocates per packet, decodes, or re-encodes video.

## 2. Workspace layout

```
irisd/
  Cargo.toml                 # workspace, profile release: lto=fat, codegen-units=1, panic=abort
  crates/
    iris-core/               # ids, config (TOML), time types (WallUs, MediaTicks), errors
    iris-h265/               # NAL header + VPS/SPS/PPS/slice-header readers, hvcC builder, CODECS string
    iris-mp4/                # fMP4 writer (ftyp/moov/prft/moof/mdat/trailer), reader/scanner, fragment rewrite export, mini-fragments
    iris-ingest/             # retina session supervisor, regulariser, fragment builder, live ring
    iris-store/              # writer threads, preallocation, sync, sidecar index, recovery, retention
    iris-catalog/            # SQLite schema, migrations, queries (single owning thread)
    iris-events/             # Dahua CGI feed, ONVIF PullPoint (oxvif), normalisation
    iris-api/                # axum: JSON, HLS/LL-HLS, WebSockets, export, auth
    irisd/                   # daemon and offline record, inspect, recover, HLS, and serve commands
    iris-replay/             # bounded RTSP replay and synthetic-camera server
    iris-gui/                # GTK/GStreamer live, playback, thumbnails, and export client
    iris-tools/              # soak, replay-benchmark, probe, report, and commit tooling
  fuzz/                      # cargo-fuzz targets for the parsers (excluded from the workspace)
  docs/research/             # this research
```

`iris-h265`, `iris-mp4`, and the event parsers in `iris-events` are pure, allocation-light
libraries with bounded regression fixtures. `cargo-fuzz` targets (`fuzz/README.md`) cover their
parsers, the `iris-api` timestamp parser, the `iris-replay` RTSP request parser, the `iris-tools`
probe parsers, the vendored oxvif SOAP/XML parser, and the vendored retina RTSP message parser.
`iris-store` has no fuzz target of its own; its recovery parses recordings and sidecars through
the fuzzed `iris-mp4` scanner and index codec. Retina's SDP parsing, RTP depacketization, and
RTSP session state have no fuzz target; retina's own test suite (`just vendor-test`) and the
`iris-ingest` integration tests, which run sessions against loopback replay servers and
fault-injecting peers, exercise them.

## 3. Threads and tasks

| Executor | Work | Count |
|---|---|---|
| tokio `current_thread` | all sockets: RTSP sessions, event feeds, HTTP/WS server, timers (≤ 20 total) | 1 thread |
| `std::thread` writer per video root | `pwritev`, `fallocate`, `fdatasync`, sidecar appends | 1 per configured root |
| `std::thread` catalog | owns the SQLite connection; batches notifications for at most 100 ms; retention scans | 1 |
| bounded maintenance threads | root probes, retention removal, startup recovery, and self-check scans | single-flight per job type |

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
struct SegmentId(i64);

struct Frame { data: Bytes /* length-prefixed NALs, no parameter sets */, raw_ts: i64,
               wall: WallUs, is_sync: bool, new_params: bool }
struct Sample { data: Bytes, duration: u32, is_sync: bool }
struct Fragment { samples: Vec<Sample>, decode_time: u64, wall: WallUs, raw_rtp: u32,
                  new_run: bool, sample_entry: Arc<SampleEntry> }
struct SampleEntry { bytes: Bytes /* complete hvc1 entry */, codecs: String,
                     width: u16, height: u16, hash: u32 /* CRC-32C */ }
struct Event { camera: CameraId, source: EventSource, kind: EventKind, state: State,
               camera_time: Option<WallUs>, nvr_time: WallUs, data: Option<String> }
```

Event data is valid JSON text capped at 64 KiB: invalid vendor text is quoted and an over-limit
encoding becomes `{"truncated":true}`. An unmapped vendor kind code is capped at 1 KiB on a
UTF-8 boundary. Catalog maintenance bounds inactive event state by recording horizon, age,
rows, and logical string bytes while preserving capacity-limited active retention marks.

## 5. API surface (0.1 alpha)

| Method | Path | Notes |
|---|---|---|
| GET | `/api/cameras`, `/api/cameras/{camera}` | configured cameras and per-stream health |
| GET | `/api/streams/{camera}/{kind}/segments?from=&to=` | catalog listing (wall ranges, runs, sample entries) |
| GET | `/api/streams/{camera}/{kind}/hls/live.m3u8` (+ `_HLS_msn/_HLS_part`) | LL-HLS playlist and in-memory child resources |
| GET | `/api/streams/{camera}/{kind}/hls/vod.m3u8?from=&to=` | byte-range playlist across segments |
| GET | `/api/segments/{id}/file.mp4` (Range) | one bounded snapshot of the on-disk file |
| GET | `/api/streams/{camera}/{kind}/export.mp4?from=&to=` | assembled MP4 |
| GET | `/api/streams/{camera}/{kind}/keyframe?t=` | init + one single-sample fragment |
| GET | `/api/cameras/{camera}/snapshot.jpg` | proxied from the camera, one request per camera at a time |
| WS | `/api/streams/{camera}/{kind}/live` | init + per-frame fragments, each message ≤ 16 MiB + 1 byte |
| WS | `/api/events` | push of normalised events |
| GET/POST | `/api/events?from=&to=&after_us=&after_id=`, `/api/events/{id}/retain` | bounded `(time,id)` event pages, pin |
| GET | `/api/health`, `/metrics` | watchdog data, Prometheus text (root token unless `[api] public_metrics`) |

The camera list and detail routes describe only IDs in the running configuration; catalog rows for
removed cameras remain available through historical recording routes. `live_viewers` is the
per-stream count of external live WebSockets and excludes the shared LL-HLS feeder. Its drop guard
removes a viewer on close, timeout, cancellation, and send failure.
The segments listing includes an unsealed segment only once it has a durable fragment and treats
it as extending to the earlier of now and 30 s past its last durable progress, while each row
reports its recorded `wall_end_us`.

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

Auth: bearer tokens protect every route except `/api/health`, and `/metrics` unless
`[api] public_metrics` is set. The server speaks
plain HTTP; bind it to loopback or a trusted private network and use a VPN or TLS reverse proxy
for remote access. The Linux viewer works through such a proxy: an `https` API URL gives `wss`
live feeds, both verified against the platform's trust store, and the viewer refuses plain
HTTP to a host other than loopback unless started with `--allow-insecure-http`.
Authenticated playlists place only bounded tamper-resistant capabilities in media URIs. Each
capability is limited to its exact path, `GET`/`HEAD`, and expiry; the root bearer is never copied
into a playlist URL. Rolling live-HLS capabilities last 15 minutes. Immutable VOD children cover
the selected span plus a 15-minute playback margin, capped at seven days.
Configuration admits bearer tokens of at least 32 visible ASCII characters (0x21 to 0x7e) other
than the example placeholder; an API without tokens needs `api.allow_unauthenticated = true` and a
loopback listener, and the switch together with tokens is rejected.

## 6. Security model

- Cameras are untrusted peers: every parser (RTSP, SDP, RTP, H.265 headers, multipart event
  feed, SOAP XML, JSON) has size caps, and a stream's errors end only its own session, which
  reconnects with jittered backoff. §2 lists the parsers with fuzz targets, and §3 describes
  the limits of one shared runtime. RTSP control messages are capped at 1 MiB
  for head and body together; in-progress H.265 access-unit payload and projected output are
  each capped at 64 MiB before a completed frame allocation.
- systemd hardening set + Landlock (03 §5): the process can read/write only the video roots
  and state dir, connect only to camera ports, bind only the API port. Landlock filesystem
  rights (ABI 1) and TCP bind/connect rights (ABI 4) are required: on a kernel without them
  the daemon refuses to start unless `--no-sandbox` is given, and rights from later ABIs are
  applied when available, with a warning naming any missing ones. Landlock filters TCP ports
  for every destination address; peers are restricted by the unit's `IPAddressDeny=any` and
  `IPAddressAllow=`, which allows only the loopback until a drop-in adds the camera subnet
  (and the API clients' subnet when the API listens on the LAN).
- Secrets (camera passwords, API tokens) live in the configuration file read at startup,
  never in URLs or logs. The packaging installs it 0640 root:irisd in a 0750 directory, and
  the daemon warns when users other than its account and group can read it. `CameraConfig`'s
  `Debug` output redacts the password.
  Configuration parse errors retain a generic syntax/schema description and byte span;
  they discard source text and value-bearing parser messages before crossing the core API.
- State at rest: the unit's `StateDirectoryMode=0700` and a startup check keep the catalog
  (camera metadata, recording paths, events) private to the service account. Before SQLite
  opens it, the daemon requires the state directory and the `irisd.db`, `-wal`, and `-shm`
  files to belong to its user, refuses symbolic links, hard links, and special files, and
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
| Crash loss | ≤ 2 s daemon, ≤ 7 s power | `kill -9` and power-cycle tests, count missing frames |
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
calls per second for fourteen streams.

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

- Golden tests: 30 s cam1 captures (raw NALs, RTP packet dumps) checked into a test-data
  store (not git) with expected fragment boundaries, hashes, and timing.
- Property tests for the regulariser (duplicate stamps, gaps, drift, wrap-around, resets).
- Writer tests against a loop device with injected errors; recovery tests over truncated and
  zero-filled tails.
- Playback conformance: produced files pass `ffprobe`, `mp4dump`, Apple's `mediastreamvalidator`
  for HLS playlists, and play in AVPlayer/mpv/GStreamer.

## 9. Observability

`tracing` with structured `key=value` fields to journald. Once per `--stats-interval`
(default 60 s) the daemon writes one line per stream, one per event feed, one `writer`, one
`catalog`, one `root` per video root, and one `process` line. Latency fields are per-interval
p50/p99 upper bounds from fixed logarithmic histogram buckets (`iris_core::metrics`) plus the
interval maximum, so the hot paths only increment atomics.
Counts and percentile ranks come from the published bucket samples. Open-bucket
percentiles use a cumulative maximum, published before each bucket increment and independent
of interval maximum resets. One reporter owns those resets. Snapshots are approximate
across concurrent updates, so interval cutoffs and means can be skewed; percentile bounds
still cover their observed bucket samples.

| Line | Fields |
|---|---|
| `stream` | connected, frames, fragments, payload MiB, reconnects, snaps, loss markers, dropped, regulariser holds and hold seconds, nominal fps, timeline error ms, peak fragment KiB and seconds, RTCP sender reports and camera-minus-NVR clock offset ms, gaps, seconds since the last frame, codec |
| `events` | camera and source identity; connected, events, messages (heartbeats and empty pulls included), failed-session retry cycles (`reconnects`), renewals, failed sessions (`errors`), seconds since the last message, pull p99/max ms (ONVIF), last event time |
| `writer` | fragments, file MiB, syncs, dropped, write errors, files, queue depth and peak, peak queued MiB, write p50/p99/max ms, sync p50/p99/max ms, slow I/O count |
| `catalog` | segments and sealed GiB; rows and MiB in deletion intent (`deleting_segments`, `deleting_mib`); event rows, logical MiB, active marks, and pruned rows; database/WAL MiB; live/peak command count and owned MiB; coalesced and total/per-kind rejected commands; reconciliation-required flag; batches, changes, peak batch, failures; transaction, query, queue-wait, and event receipt-to-commit p99/max ms |
| `root` | free GiB and percent per video root |
| `process` | CPU total, user, and system percent, RSS, threads, fds, uptime, loop lag max ms (how late a 250 ms timer fired on the runtime thread), watchdog pings withheld, retention pass max ms, self-check ok, failed, and skipped counts, statistics-report and retention-pass timeouts since start (`report_timeouts`, `retention_timeouts`), Landlock enforcement (`sandbox_enforced`: 0 disabled, 1 partial, 2 full) |

One-off lines mark the facts a soak must explain: `gap` (`cause=reconnect` with the last
frame time before the session ended and the first after it, `cause=camera_hold` for a
regulariser hold of a second or more, or `cause=sink_rejected` for the first writer rejection
in a loss epoch), `retention pass` (deleted, earlier deletion intents finished, freed,
duration, free percent, catalogued GiB against the quota; a pass over 2 s is a warning),
`slow write` and `slow sync` (over 1 s),
and `selfcheck ok` or `selfcheck failed`. The self-check visits one
stream per `--selfcheck-cycle` (default 24 h) divided by the stream count, scans that
stream's newest sealed file with CRC verification on a blocking thread, and compares
fragments, bytes, init length, media span, and wall start with the catalog row.

`/api/health` and `/metrics` (Prometheus text: API requests, clients, skipped event-WebSocket
messages, API admission rejections by media class, WebSocket cap, and snapshot single-flight,
rows outside the approved roots, oversized live closes, uptime, catalog queue, rejection and
failed-batch state, deletion intent, event capacity/pruning, and database/WAL sizes) serve pollers; `/metrics`
requires the root bearer token unless `[api] public_metrics` is set. Uptime uses a monotonic
process instant. Metric scrapes share one
cache-owned catalog-aggregate refresh at most once per second; concurrent scrapes coalesce and
request cancellation cannot abandon the refresh. A failure serves the last snapshot or omits cold
aggregates until the next cadence. The `systemd`
watchdog ping is withheld whenever an ingest
task made no progress in the interval. Figures the daemon cannot see from inside its
sandbox (disk sectors written for write amplification, pressure stall information, NIC and
TCP counters, NTP offset, the service's cgroup CPU and memory split) come from
`iris-tools soak-sample`, which runs from a timer beside the service. Its bounded API check also
captures the catalog failure, rejection, and reconciliation metrics so a healthy HTTP response
cannot hide failed persistence.
Soak reports recognize the ordered startup prefix (root availability, sandbox setup,
catalog open) before reconciliation as a run boundary, including after crashes without
a `stopped` line. Reconciliation remains a fallback marker when the prefix is absent.
Startup warnings belong to the restarted run and cannot extend the prior run's last observation.
Without a clean stop or process identity in the exported log, the interval between runs
remains unobserved / possible downtime; the report cannot infer the exact crash time.
An API task whose result was consumed by the monitor is removed before shutdown; a
bind failure therefore still drains the writer and catalog without polling it twice.
Startup reconciles storage and then binds the API listener before recording tasks and `READY=1`.
The already-bound listener is passed to the API task, so an occupied address cannot produce a
false-ready daemon.

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

[[camera]]
id = "cam1"
name = "Example"
rtsp_url = "rtsp://192.0.2.10:554/cam/realmonitor?channel=1&subtype=0&unicast=true&proto=Onvif"
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
`0..=50`. The API section needs a bearer token of at least 32 visible ASCII characters other
than the example placeholder, or `allow_unauthenticated = true` with no token on a loopback
listener (§5). Each rejection names the key and its accepted range; under the alpha policy
(`STANDARDS.md` §9) a configuration that broke a rule stops loading. Derived queue and
media-tick arithmetic is checked before roots, listeners, writers, or tasks are created.
Repeated stream selectors in the standalone `record` command are rejected rather than
starting two producers for one key.

Before the daemon creates a worker thread, it opens the catalog synchronously, resolves durable
legacy-registry state, and admits each recording root against the durable `.irisd-root-id` marker
plus its process-local physical directory identity. Known or ambiguous legacy roots are never
initialized when missing or replaced—even if a migrated catalog has no remaining segment rows—physical aliases are rejected, and subsequent reconciliation/removal remains
descriptor-relative beneath the exact approved root. A missing fresh root is created through
directory descriptors from the deepest ancestor that exists without following a link, and
admission continues from the created descriptor. Every open of a marker, recording, or sidecar
beneath a root is nonblocking, no-follow, and regular-file only, so a planted FIFO or device fails
that open at once instead of stalling admission, the writer, reconciliation, or shutdown.
Startup catalog scans use fixed pages and
the root walk streams no-follow entries; an elapsed heartbeat extends the service start deadline
through all pages, enumeration, and media scans.

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
SIGTERM/SIGINT shutdown shares one 25-second absolute budget
across all producers, writer sealing, API drain, and catalog closure; aborted tasks are awaited and
all later cleanup stages run even when an earlier one fails.
