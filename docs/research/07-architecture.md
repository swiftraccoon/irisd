# 07 — Daemon architecture (irisd)

Integrates 03–06 into one design. Numbers refer to cam1 measurements (01).

## 1. Data flow

```
                 ┌──────────── per camera (×6) ────────────┐
 RTSP/TCP ──▶ retina session (main) ──▶ regulariser ──▶ fragment builder ──┐
 RTSP/TCP ──▶ retina session (sub)  ──▶ regulariser ──▶ fragment builder ──┤
 HTTP     ──▶ Dahua CGI event feed ─────────────────────────────┐          │
 SOAP     ──▶ ONVIF PullPoint ──────────────────────────────────┼─▶ event normaliser ──▶ catalog, WS push
 RTP      ──▶ ONVIF metadata (via retina) ──────────────────────┘          │
                                                                           ▼
                                                     live ring (Bytes, 2 fragments + current)
                                                        │                 │
                        WS per-frame fragments ◀────────┤                 ├──▶ bounded writer queue
                        LL-HLS parts            ◀────────┘                 │
                                                                           ▼
                                                  writer thread per disk: pwritev fragment,
                                                  append sidecar, fallocate ahead, fdatasync/5 s
                                                                           │
                                                                           ▼
                                     catalog thread (SQLite): segment/run/event rows, retention
                                                                           │
 HTTP/1.1+2 (axum) ◀── HLS playlists (byte ranges), MP4 export, keyframes, JSON API ◀──┘
```

Every byte of video is copied at most once in user space (socket read buffer → `Bytes` slice
shared by the live ring, the writer, and the WebSocket senders). The record path never
allocates per packet, never decodes, never re-muxes.

## 2. Workspace layout

```
irisd/
  Cargo.toml                 # workspace, profile release: lto=fat, codegen-units=1, panic=abort
  crates/
    iris-core/               # ids, config (TOML), time types (WallUs, MediaTicks), errors
    iris-h265/               # NAL header + VPS/SPS/PPS/slice-header readers, hvcC builder, CODECS string
    iris-mp4/                # fMP4 writer (ftyp/moov/prft/moof/mdat/trailer), reader/scanner, moov+elst export, mini-fragments
    iris-ingest/             # retina session supervisor, regulariser, fragment builder, live ring, audio
    iris-store/              # writer threads, preallocation, sync, sidecar index, recovery, retention
    iris-catalog/            # phase 2: SQLite schema, migrations, queries (single owning thread)
    iris-events/             # phase 2: Dahua CGI feed, ONVIF PullPoint (oxvif), metadata-track parser, normaliser
    iris-api/                # phase 2: axum: REST, HLS/LL-HLS, WebSocket live + events, export, auth
    iris-camera/             # phase 2: vendor adapters: Dahua CGI (config/provisioning/snapshot), ONVIF media
    irisd/                   # binary: record, inspect, recover, hls, serve; later supervisor, systemd notify/watchdog, landlock, metrics
  fuzz/                      # cargo-fuzz targets for the parsers (excluded from the workspace)
  iris-tools/                # tools binary: log summaries, soak sampler and report, bench driver, camera probes, gh-commit
  apps/iris-gtk/             # later: Linux GUI (gtk4-rs + gstreamer-rs)
  ios/                       # later: Swift app (separate Xcode project)
  docs/research/             # this research
```

`iris-h265`, `iris-mp4`, `iris-store` and the event parsers are pure, allocation-light
libraries with `cargo-fuzz` targets and golden tests built from captured cam1 data.

## 3. Threads and tasks

| Executor | Work | Count |
|---|---|---|
| tokio `current_thread` (opt-in 2 workers) | all sockets: RTSP sessions, event feeds, HTTP/WS server, timers (≤ 20 total) | 1 thread |
| `std::thread` writer per video root | `pwritev`, `fallocate`, `fdatasync`, sidecar appends | 1 per disk |
| `std::thread` catalog | owns the SQLite connection; batched transactions every 5 s; retention scans; WAL checkpoints | 1 |
| `std::thread` maintenance (low priority) | recovery scans at startup, clip copies, re-index | 1, idle |

Backpressure rules: camera sockets are never blocked on disk or DB; the writer queue is
bounded by bytes; WebSocket subscribers that fall behind are dropped (slow consumer), not
buffered without limit.

## 4. Core types

```rust
struct WallUs(i64);          // µs since Unix epoch, NVR CLOCK_REALTIME
struct MediaTicks(i64);      // 90 kHz, continuous within a run
struct RunId(u64); struct SegmentId(u64); struct StreamId(u32); struct CameraId(String);

struct Frame { data: Bytes /* length-prefixed NALs, no parameter sets */, duration: u32,
               is_irap: bool, media: MediaTicks, wall: WallUs, raw_rtp: i64 }
struct Fragment { stream: StreamId, run: RunId, samples: Vec<Frame>, media_start: MediaTicks,
                  wall_start: WallUs, starts_with_irap: bool, sample_entry: SampleEntryId }
struct SampleEntry { hash: [u8;16], codec: Codec, width: u16, height: u16, config: Bytes /* hvcC/esds */, codecs: String }
struct Event { camera: CameraId, source: EventSource, kind: EventKind, state: State,
               camera_time: Option<WallUs>, nvr_time: WallUs, data: serde_json::Value }
```

## 5. API surface (v1)

| Method | Path | Notes |
|---|---|---|
| GET | `/api/cameras`, `/api/cameras/{id}` | config, capabilities, health, current stream params |
| GET | `/api/streams/{id}/segments?from=&to=` | catalog listing (wall ranges, runs, sample entries) |
| GET | `/api/streams/{id}/hls/live.m3u8` (+ `_HLS_msn/_HLS_part`) | LL-HLS |
| GET | `/api/streams/{id}/hls/vod.m3u8?from=&to=` | byte-range playlist across segments |
| GET | `/api/segments/{id}` (Range) | the on-disk file; init = first `init_len` bytes |
| GET | `/api/streams/{id}/export.mp4?from=&to=&audio=` | assembled MP4 |
| GET | `/api/streams/{id}/keyframe?t=` | init + one single-sample fragment |
| GET | `/api/cameras/{id}/snapshot.jpg` | proxied from the camera |
| WS | `/api/streams/{id}/live` | init + per-frame fragments; `?from=` for ring catch-up |
| WS | `/api/events` | push of normalised events |
| GET/POST | `/api/events?from=&to=`, `/api/events/{id}/retain` | query, pin |
| GET | `/api/health`, `/metrics` | watchdog data, Prometheus text |
| POST | `/api/cameras/{id}/provision` | opt-in encoder/NTP/audio provisioning |

Auth: bearer tokens (per client, revocable) over TLS (rustls, optional on a trusted LAN);
remote access via VPN rather than exposure. Unauthenticated: nothing.

## 6. Security model

- Cameras are untrusted peers: every parser (RTSP, SDP, RTP, H.265 headers, multipart event
  feed, SOAP XML, JSON) has size caps, fuzz targets, and runs under `panic = "abort"` with
  per-camera task isolation and backoff restart.
- systemd hardening set + Landlock (03 §5): the process can read/write only the video roots
  and state dir, connect only to camera ports, bind only the API port.
- Secrets (camera passwords, API tokens) in a mode-0600 TOML or systemd credentials
  (`LoadCredential=`), never in URLs or logs.
- Supply chain: `cargo audit`, `cargo deny` (licenses + advisories), lockfile committed,
  minimal feature sets.

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

| Line | Fields |
|---|---|
| `stream` | connected, frames, fragments, payload MiB, reconnects, snaps, loss markers, dropped, regulariser holds and hold seconds, nominal fps, timeline error ms, peak fragment KiB and seconds, RTCP sender reports and camera-minus-NVR clock offset ms, gaps, seconds since the last frame, codec |
| `events` | connected, events, messages (heartbeats and empty pulls included), reconnects, renewals, errors, seconds since the last message, pull p99/max ms (ONVIF), last event time |
| `writer` | fragments, file MiB, syncs, dropped, write errors, files, queue depth and peak, peak queued MiB, write p50/p99/max ms, sync p50/p99/max ms, slow I/O count |
| `catalog` | segments, sealed GiB, events, database and WAL MiB, batches, changes, peak batch, failures, transaction, query, and queue-wait p99 and max ms, event receipt-to-commit p99 and max ms |
| `root` | free GiB and percent per video root |
| `process` | CPU total, user, and system percent, RSS, threads, fds, uptime, loop lag max ms (how late a 250 ms timer fired on the runtime thread), watchdog pings withheld, retention pass max ms, self-check ok, failed, and skipped counts |

One-off lines mark the facts a soak must explain: `gap` (`cause=reconnect` with the last
frame time before the session ended and the first after it, or `cause=camera_hold` for a
regulariser hold of a second or more), `retention pass` (deleted, freed, duration, free
percent, catalogued GiB against the quota; a pass over 2 s is a warning), `slow write` and
`slow sync` (over 1 s), and `selfcheck ok` or `selfcheck failed`. The self-check visits one
stream per `--selfcheck-cycle` (default 24 h) divided by the stream count, scans that
stream's newest sealed file with CRC verification on a blocking thread, and compares
fragments, bytes, init length, media span, and wall start with the catalog row.

`/api/health` and `/metrics` (Prometheus text: API requests, live and event clients, uptime,
catalog totals) serve pollers. The `systemd` watchdog ping is withheld whenever an ingest
task made no progress in the interval. Figures the daemon cannot see from inside its
sandbox (disk sectors written for write amplification, pressure stall information, NIC and
TCP counters, NTP offset, the service's cgroup CPU and memory split) come from
`iris-tools soak-sample`, which runs from a timer beside the service.

## 10. Configuration sketch (TOML)

```toml
[storage]
video_roots = ["/srv/irisd/video"]
state_dir = "/var/lib/irisd"
min_free_percent = 5
sync_interval_ms = 5000
max_fragment_ms = 2000
max_queue_bytes = 67108864
uncached_writes = false

[api]
listen = "0.0.0.0:8554"
tls = { cert = "...", key = "..." }   # optional

[[camera]]
id = "cam1"
name = "Example"
rtsp_url = "rtsp://192.0.2.10:554/cam/realmonitor?channel=1&subtype=0&unicast=true&proto=Onvif"
sub_url  = "rtsp://192.0.2.10:554/cam/realmonitor?channel=1&subtype=1&unicast=true&proto=Onvif"
username = "..."
password = "..."
vendor = "dahua"
record = ["main", "sub"]
audio = false
retention_days = 21
timestamp_mode = "regularize"
events = ["dahua_cgi", "onvif_pullpoint"]
```
