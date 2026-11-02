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
  points or at the cap, with the running payload checksum and hold accounting.

## How it is used

```rust
use std::sync::Arc;

use iris_ingest::{FragmentSink, IngestOptions, StreamSource, StreamStats, run_stream};
use tokio_util::sync::CancellationToken;

// `sink` implements FragmentSink (the daemon forwards to the store's writer queue).
let source = StreamSource {
    key: "cam1/main".parse().expect("key"),
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
and tears the session down with `TEARDOWN`.

## Invariants and limits

- **Timing.** The NVR clock is the authority; RTP timestamps give relative timing through
  the regulariser, which assigns the nominal frame duration, corrects by at most 1 % per
  fragment, renders camera-dropped frames as frame-long holds, re-times the stream to its delivered
  rate after warm-up when the typical fragment runs more than 1 % off the declared rate (or
  holes exceed 5 % of camera time), and snaps to the camera timeline when the error exceeds the tolerance
  (500 ms). Fixed by `docs/research/04-storage-format.md §7` and
  `05-ingest-events.md §2`; the constants live at the top of `src/regularize.rs`.
- **Fragments.** A fragment opens at a sync frame and closes at the next sync frame, at a
  parameter-set change, or once the camera timeline exceeds the cap by a quarter (default cap 2 s).
  Frames before the first sync frame are dropped. A discontinuity restarts the decode
  timeline at zero and starts a new run (`04 §7`, `05 §2`).
- **Sessions.** TCP interleaved only; 15 s setup timeout on DESCRIBE, SETUP, and PLAY;
  30 s inactivity timeout; `TEARDOWN` on every exit; timestamp jumps over 10 s end the
  session; backoff 1 s doubling to 30 s with ±20 % jitter, reset after a 60 s healthy
  session (`05 §1`).
- **Socket reads.** With `read_interval` set, a session drains the demuxer without arming a
  wake-up and waits for the next tick of the shared `ReadTicker` (one timer task for every
  stream; the caller creates it once per runtime and passes it in `IngestOptions::ticker`,
  else each stream runs its own timer at the same instant), and the socket leaves tokio's
  reactor once `PLAY` succeeds so arriving segments wake nothing; all streams
  are read in one runtime wake-up; `recv_low_water` sets `SO_RCVLOWAT` after `PLAY` so the
  kernel reports the socket readable once per that many bytes (`05 §1`,
  `12-ingest-cost.md §3–4`).
- **Sinks never block.** The sink is called on the ingest task; a rejected fragment is
  dropped and counted, never awaited (`04 §8`).
- **Live ring.** Frames are published with provisional durations before the fragment
  closes; a lagged subscriber resynchronises at the next sync frame
  (`docs/design/phase2-interfaces.md`).

## Tests and measurements

- Unit tests in `src/regularize.rs` (jitter, drift, gaps, backwards timelines, wrong
  declared rates, passthrough), `src/assembler.rs` (first-sync drop, cap splits,
  discontinuities, parameter-set changes, hold accounting), `src/session.rs` (declared-rate
  parsing, wall-clock mapping, the shared tick), `src/stats.rs`, and `src/live.rs`.
- `tests/replay.rs` replays the camera timeline of a real ten-minute recording (the sidecar
  in `tests/data/`) through the regulariser and checks that it stays locked without snaps.
- `tests/replay_server.rs` records from the `iris-replay` server in both read modes
  (packet-driven and timer-driven), checks that the fragments carry the served frames,
  and makes a replayed camera stall so the timer-driven session's inactivity timeout,
  teardown, and reconnect run on a detached socket; it
  byte for byte, with sender reports observed. No hardware is involved.
- Measurements: `docs/research/11-multi-camera-results.md` (six and seven live cameras) and
  `docs/research/12-ingest-cost.md` (CPU per megabit, the read policy, the low-water mark).

## Further reading

- `docs/research/05-ingest-events.md` §1 (session policy) and §2 (pipeline)
- `docs/research/04-storage-format.md` §7 (time model) and §8 (I/O policy, backpressure)
- `docs/research/12-ingest-cost.md` (where the CPU goes)
- `docs/research/01-camera-probe.md` (what cameras actually send)
- `docs/design/phase2-interfaces.md` (the live ring contract)
- `STANDARDS.md`
