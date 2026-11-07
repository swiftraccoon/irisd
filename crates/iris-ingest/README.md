# iris-ingest

RTSP ingest for irisd. One task per stream holds a `retina` session over TCP, receives
H.265 access units as length-prefixed NAL units (the `hvc1` sample format, no decoding),
gives every frame a regularised duration, groups frames from one random access point to the
next into fragments, and hands each fragment to a sink. In the record path (RTSP in → fMP4 on
disk → byte-range serving) this crate is the "RTSP in" stage: it owns the camera sessions
and the timing model, and it is the only place that talks to cameras.

## What it provides

- `run_stream` — records one stream until shutdown, reconnecting after every failure with
  jittered exponential backoff; the only entry point the daemon uses.
- `StreamSource` — the stream to record: its `StreamKey`, RTSP `Url` without credentials, and
  the credentials passed separately.
- `IngestOptions` — the policy: fragment cap, timestamp mode, inactivity and jump limits,
  user agent, the live ring, the socket read interval, the shared read ticker, and the
  receive low-water mark.
- `FragmentSink` and `SinkError` — where finished fragments go; the daemon's sink is the
  store's writer queue, which rejects instead of blocking.
- `StreamStats` — lock-free counters the daemon reads once a minute for its `stream` line:
  frames, bytes, fragments, reconnects, snaps, holds, loss markers, the measured nominal,
  the timeline error, fragment peaks, RTCP sender reports and the camera clock offset, gaps.
- `LiveRing`, `LiveSubscription`, `LiveFrame`, `LiveError` — fan-out of frames to live
  viewers without copying, with the init segment and the frames since the last sync frame
  for fast joins.
- `regularize::Regularizer` and `regularize::Assignment` — the constant-frame-rate timestamp
  regulariser, usable on its own (the replay regression test drives it from a recorded
  sidecar).
- `assembler::FragmentAssembler` and `assembler::Frame` — fragment assembly at random access
  points or at the cap, with byte and frame limits and hold accounting.

## How it is used

```rust
use std::sync::Arc;

use iris_core::id::{CameraId, StreamKey, StreamKind};
use iris_ingest::{FragmentSink, IngestOptions, StreamSource, StreamStats, run_stream};
use tokio_util::sync::CancellationToken;

// `sink` implements FragmentSink (the daemon forwards to the store's writer queue).
let source = StreamSource {
    key: StreamKey {
        camera: CameraId::try_new("cam1").expect("camera id"),
        kind: StreamKind::Main,
    },
    url: "rtsp://192.0.2.10:554/cam/realmonitor?channel=1&subtype=0".parse().expect("url"),
    username: "user".into(),
    password: "secret".into(),
};
let stats = Arc::new(StreamStats::new());
let shutdown = CancellationToken::new();
tokio::spawn(run_stream(source, IngestOptions::default(), sink, Arc::clone(&stats), shutdown.clone()));
```

`run_stream` never returns an error: a failed or ended session is logged, counted in
`StreamStats::reconnects`, and retried. Cancelling the token flushes the pending fragment
and tears the session down with `TEARDOWN`. After a session is established, cancellation,
inactivity, EOF, demux failure, and frame or clock errors all finalize complete pending
access units exactly once before reconnect or shutdown.

## Invariants and limits

- **Timing.** The NVR clock is the authority; RTP timestamps give relative timing through
  the regulariser, which assigns the nominal frame duration, corrects by at most 1 % per
  fragment, renders camera-dropped frames as frame-long holds, re-times the stream to its delivered
  rate after warm-up when the median fragment runs more than 1 % off the declared rate (or
  holes exceed 5 % of camera time), and snaps to the camera timeline when the error exceeds the tolerance
  (500 ms). A parameter-set epoch carrying a different valid frame rate starts a fresh
  timing estimate at that rate while preserving decode-time continuity and cumulative
  diagnostics; an unchanged, absent, or invalid rate preserves the estimator. Fixed by
  `docs/research/04-storage-format.md §7` and `05-ingest-events.md §2`; the constants live
  at the top of `src/regularize.rs`.
- **Fragments.** A fragment opens at a sync frame and closes at the next sync frame, at a
  parameter-set change, or once camera time, receipt time, or nominal sample time exceeds
  the cap by a quarter (default cap 2 s). Each pending fragment is also bounded at 4,096
  frames and 64 MiB of payload, so stalled RTP timestamps cannot grow it indefinitely.
  The patched H.265 depacketizer independently limits both pinned payload and projected
  frame output to 64 MiB while an access unit is assembled; exceeding either limit ends
  the session with a controlled error before the frame allocation. The assembler retains
  its 64 MiB check as defense in depth: an oversized frame closes retained frames with
  nominal durations and is discarded. Recording resumes at the next sync frame in a
  separate run at decode time zero, leaving the discarded interval as a gap in wall time.
  The regulariser retains its rate estimate.
  Frames before the first sync frame are dropped. A discontinuity restarts the decode
  timeline at zero and starts a new run (`04 §7`, `05 §2`).
- **Sessions.** TCP interleaved only; 15 s setup timeout on DESCRIBE, SETUP, and PLAY;
  30 s inactivity timeout; `TEARDOWN` on every exit; RTSP control messages are limited to
  1 MiB for head and body together; timestamp jumps over 10 s end the session; backoff 1 s
  doubling to 30 s with deterministic ±20 % jitter from stream identity and retry attempt,
  reset after a 60 s healthy session (`05 §1`). The patched retina bounds camera-supplied
  fields (URLs 2,048 bytes, `Session` id 512, `WWW-Authenticate` 4,096, 64 SDP media
  sections) and fails the session on a longer one.
- **Teardown.** A `TEARDOWN` on a fresh connection goes to the address the session's
  connection reached, never to a host the camera advertised, so credentials reach only the
  configured camera; a background `TEARDOWN` gives up after the session timeout or 60 s,
  whichever is shorter; at most 64 run in the process, and a session dropped beyond that is
  not torn down (retina logs a warning) (`05 §1`).
- **Camera text in logs.** A session error is logged through `LogSafe` (`src/session.rs`):
  line breaks, other control characters, and non-ASCII are escaped and the text stops at
  512 characters, so a camera cannot split a journal entry, forge a priority prefix, or
  flood the log. Retina quotes camera text only as escaped excerpts of at most 512 bytes
  and logs at most one warning per `DESCRIBE` (`05 §1`). `StreamSource`'s `Debug` output
  redacts the credentials.
- **Wall placement and recorder clock.** Receive instants use fresh realtime/monotonic
  correlations. Finalized frame durations feed a per-run least-delay wall-minus-media
  anchor: lower-delay observations take effect immediately, upward movement is limited to
  500 ppm, and each sealed fragment is placed from that anchor plus its decode start. Raw
  receipt times still enforce the fragment cap. A change over 2 s in the recorder clock
  offset flushes the pending fragment and ends the session, so reconnect starts a separate
  run and anchor.
- **Socket reads.** With `read_interval` set, a session drains the demuxer without arming a
  wake-up and waits for the next tick of the shared `ReadTicker` (one timer task for every
  stream; the caller creates it once per runtime and passes it in `IngestOptions::ticker`,
  else each stream runs its own timer at the same instant), and the socket leaves tokio's
  reactor once `PLAY` succeeds so arriving segments wake nothing; all streams
  are read in one runtime wake-up; `recv_low_water` sets `SO_RCVLOWAT` after `PLAY` so the
  kernel reports the socket readable once per that many bytes (`05 §1`,
  `12-ingest-cost.md §3–4`). Writes on the detached socket never block the runtime thread:
  a keepalive the camera does not read stays queued in the session, and the session fails
  at the next keepalive deadline (`12-ingest-cost.md §6`).
- **Sinks never block.** The sink is called on the ingest task; a rejected fragment is
  dropped and counted, never awaited. The first rejection in a loss epoch records one gap
  and ends recording continuity. A sync frame already pending is retained; otherwise
  dependent recording frames are discarded until the next sync frame, which begins a run
  at decode time zero. Live publication continues throughout (`04 §8`).
- **Live ring.** Frames are published with provisional durations before the fragment
  closes. The fast-start cache retains at most 600 frames and 64 MiB of payload from the
  most recent sync frame; crossing either limit abandons that run until a later fitting sync.
  Subscriber fan-out retains each frame once, bounded by the configured frame capacity
  and 64 MiB of payload per stream, and retains no fan-out payload without subscribers.
  Overflow reports lag and delivery resumes at the next sync frame
  (`docs/design/phase2-interfaces.md`).

## Tests and measurements

- Unit tests in `src/regularize.rs` (jitter, drift, gaps, backwards timelines, wrong
  declared rates, passthrough), `src/assembler.rs` (first-sync drop, cap splits,
  discontinuities, oversized-frame gaps, parameter-set changes, hold accounting),
  `src/session.rs` (declared-rate parsing, injected realtime steps, wall-clock mapping, the
  shared tick), `src/stats.rs`, and `src/live.rs`.
- `tests/replay.rs` replays the camera timeline of a real ten-minute recording (the sidecar
  in `tests/data/`) through the regulariser and checks that it stays locked without snaps.
- `tests/replay_server.rs` records from the `iris-replay` server in both read modes
  (packet-driven and timer-driven), checks that the fragments carry the served frames,
  and makes a replayed camera stall so the timer-driven session's inactivity timeout,
  teardown, and reconnect run on a detached socket. Cancellation checks account for every
  received frame and clear connection state in both read modes. No hardware is involved.
- `tests/adversarial_rtsp.rs` uses bounded loopback peers in both read modes to cover
  terminal tail flushes, transport-backlog wall placement, RTSP and H.265 allocation
  limits, reconnect recovery, and recording continuity after sink rejection. Hostile cameras
  on OS threads check that a camera which stops reading never stalls the runtime thread
  (a task beside the session ticks with no gap over 500 ms, through the keepalive and the
  `TEARDOWN`) and that a `TEARDOWN` never contacts an advertised host (Basic, Digest, and a
  challenged `TEARDOWN`).
- `tests/log_bounds.rs` installs the daemon's log subscriber and checks that hostile
  `DESCRIBE` answers (1 MiB of invalid SDP, 18,000 media sections, unusable sections beside
  the video) log only escaped lines of at most 1 KiB, one per ended session plus retina's
  one summary. `tests/teardown_bound.rs` drops 100 sessions against a camera that never
  answers `TEARDOWN` and checks that 64 background tasks remain. Both bind process-wide
  state, so each is a test binary of its own.
- Measurements: `docs/research/11-multi-camera-results.md` (six and seven live cameras) and
  `docs/research/12-ingest-cost.md` (CPU per megabit, the read policy, the low-water mark).

## Further reading

- `docs/research/05-ingest-events.md` §1 (session policy) and §2 (pipeline)
- `docs/research/04-storage-format.md` §7 (time model) and §8 (I/O policy, backpressure)
- `docs/research/12-ingest-cost.md` (where the CPU goes)
- `docs/research/01-camera-probe.md` (what cameras actually send)
- `docs/design/phase2-interfaces.md` (the live ring contract)
- `STANDARDS.md`
