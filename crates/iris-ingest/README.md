# iris-ingest

RTSP ingest for irisd. One task per stream holds a `retina` session over TCP, receives
H.265 access units as length-prefixed NAL units (the `hvc1` sample format, no decoding),
gives every frame a regularised duration, groups frames from one random access point to the
next into fragments, and hands each fragment to a sink. In the record path (RTSP in → fMP4 on
disk → byte-range serving) this crate is the "RTSP in" stage: it owns the camera sessions
and the timing model, and it is the only place that opens RTSP sessions to cameras.

## What it provides

- `run_stream` — records one stream until shutdown, reconnecting after every failure with
  jittered exponential backoff; the only entry point the daemon uses.
- `StreamSource` — the stream to record: its `StreamKey`, RTSP `Url` without credentials, and
  the credentials passed separately.
- `IngestOptions` — the policy: fragment cap, timestamp mode, inactivity and jump limits,
  user agent, the live ring, audio recording with its live ring, the socket read interval,
  the shared read ticker, the receive low-water mark, the window of the watches of parted
  clocks, and whether the drift rule's steps are made (`audio_steps`). `Default` is the
  packet-driven policy without a low-water mark, audio, or steps;
  the daemon fills the read policy from `[ingest]`.
- `ReadTicker` — the read clock every timer-driven session shares; the daemon spawns one per
  runtime and passes it in `IngestOptions::ticker`.
- `FragmentSink` and `SinkError` — where finished fragments go; the daemon's sink is the
  store's writer queue, which rejects instead of blocking. `deliver_fragment` is the
  delivery and accounting seam `run_stream` uses, public for the daemon's sink tests.
- `StreamStats` — atomic counters, most of which the daemon reads once a minute for its `stream`
  line (the codec description sits behind a mutex held only to copy a short string): frames,
  access-unit bytes, fragments, reconnects, snaps, holds, loss markers, the nominal in use, the
  timeline error, fragment peaks, RTCP sender reports and the camera clock offset, gaps; the
  measures of the watches of parted clocks; `SoundSteps` named and made by the drift rule; and
  `beats`, the task's own sign of life, which the daemon's watchdog reads: it counts at every item
  read, within every 5 s of a wait on a quiet camera or between attempts, and after each step of a
  session's set-up, so it stands still only when the task is stuck.
- `background_teardowns` and `MAX_BACKGROUND_TEARDOWNS` — the running background `TEARDOWN`
  tasks and their bound, the daemon's `teardowns` gauge.
- `Url` — the URL type of `StreamSource`, re-exported so callers need not depend on `url`.
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

// `sink` is an `Arc<dyn FragmentSink>` (the daemon's forwards to the store's writer queue).
// `IngestOptions::default()` reads packet by packet; the daemon sets `read_interval`,
// `ticker`, and `recv_low_water` from its `[ingest]` table.
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

- **Timing.** The NVR clock is the authority; RTP timestamps give relative timing through the
  regulariser, which assigns the nominal frame duration, corrects by at most 1 % per fragment,
  renders the slots a camera leaves empty, its dropped frames, and its stalls as holds at the
  declared rate, re-times the stream to its delivered rate after warm-up only when the typical
  fragment (the median over frames) runs faster than declared by more than 1 % or lacks more than
  an eighth of its slots, and snaps to the camera timeline when the error exceeds the tolerance
  (500 ms). A
  parameter-set epoch carrying a different valid frame rate starts a fresh timing estimate at that
  rate while preserving decode-time continuity and cumulative diagnostics; an unchanged, absent, or
  invalid rate preserves the estimator. Fixed by `docs/research/04-storage-format.md §7` and
  `05-ingest-events.md §2`; the constants live at the top of `src/regularize.rs`.
- **Fragments.** A fragment opens at a sync frame and closes at the next sync frame, at a
  parameter-set change, or once camera time, receipt time, or nominal sample time exceeds
  the cap by a quarter (default cap 2 s). A gap the camera left is added to the last frame's
  duration when the fragment closes, so a fragment's duration is at most the cap and a quarter
  plus that gap, which the 10 s jump limit bounds. Each pending fragment is also bounded at 4,096
  frames and 64 MiB of payload, so stalled RTP timestamps cannot grow it indefinitely.
  The patched H.265 depacketizer independently limits both pinned payload and projected
  frame output to 64 MiB while an access unit is assembled; exceeding either limit ends
  the session with a controlled error before the frame allocation. The assembler retains
  its 64 MiB check as defense in depth: an oversized frame closes the retained frames as a
  fragment and is discarded. Recording resumes at the next sync frame in a
  separate run at decode time zero, leaving the discarded interval as a gap in wall time.
  The regulariser retains its rate estimate.
  Frames before the first sync frame are dropped. A discontinuity restarts the decode
  timeline at zero and starts a new run (`04 §7`, `05 §2`).
- **Sessions.** TCP interleaved only; 15 s setup timeout on DESCRIBE, SETUP, and PLAY;
  a 30 s inactivity timeout that any RTSP item resets, RTCP sender reports included;
  `TEARDOWN` on every exit; RTSP control messages are limited to
  1 MiB for head and body together; timestamp jumps over 10 s end the session; backoff 1 s
  doubling to 30 s with deterministic ±20 % jitter from stream identity and retry attempt,
  reset after a 60 s healthy session (`05 §1`). The patched retina bounds camera-supplied
  fields (URLs 2,048 bytes, `Session` id 512, `WWW-Authenticate` 4,096, 64 SDP media
  sections) and fails the session on a longer one.
- **Teardown.** A `TEARDOWN` on a fresh connection goes to the address the session's
  connection reached, never to a host the camera advertised, so credentials reach only the
  configured camera; a background `TEARDOWN` gives up after the session timeout or 60 s,
  whichever is shorter; at most 64 run in the process, and a session dropped beyond that is
  not torn down (retina logs a warning); the daemon's `process` line reports the running count as
  `teardowns` (`05 §1`).
- **Audio.** With `IngestOptions::audio`, a session also sets up the description's AAC
  stream (`mpeg4-generic`) and delivers it under the camera's `audio` stream key, on the
  video's timeline (`docs/research/13-audio.md` §5.2). Each frame is a sync sample of its own
  duration in the 90 kHz timescale (1,440 ticks at 64 kHz), and time within a run is the
  count of frames: cameras stamp audio a fraction of a frame early or late, backwards too,
  so a run goes on while a frame's timestamp is within half a second of where the count puts
  it. The first frame of a run takes the video media time at which it was received, by the
  least receipt delay of the frames (the video path's own estimate), against the video
  fragment of its run that closed last; a fragment holds the frames that start inside one video
  fragment's span and is cut when the next video fragment
  closes, so the two streams are cut at the same points within a frame; a video run starts
  an audio run, and lost audio packets or a timestamp off the count start a run in both. With
  `IngestOptions::audio_live`, each frame is also published to that live ring once its run is
  placed, with the decode time and wall time its recording gets (§10.3).
  The difference between a fragment's placement and its count is logged as `drift_us` on
  the cadence target and changes nothing: it follows the video's delay estimate, not the
  recording (`docs/research/13-audio.md` §5.1a). Every ten minutes of a run, the least and
  the greatest distance of the frames' timestamps from the count, and how far the least has
  moved since the run's first window, are logged at `info` as `audio timestamps against the
  count`; the line decides nothing (§5.2, item 5).
- **Watches of parted clocks.** Once a window (`IngestOptions::watch_window`, ten minutes) a session
  logs its measures at `info` and keeps each in `StreamStats` (`docs/research/14-drift.md` §3):
  `timestamps against the recorder's clock` for the video (`clock_moved_us`) and, logged only, for
  the audio, `audio timestamps against the count` (`audio_stamps_moved_us`), and where the recording
  has the audio against the video, read two ways: `audio against video by sender reports`
  (`audio_sync_us`, `audio_sync_moved_us`), from the video's epoch less the audio's by the lower
  tenth of the window's pairs of reports (one of each stream, received within a second of each
  other), retaining at most 1,024 pairs and thinning a larger sample uniformly in arrival order, and
  `audio against video by arrival` (`audio_arrival_moved_us`), from the least receipt time less
  decode time of each stream's frames, with how far the two streams' timestamps alone have the sound
  moved (`stamps_moved_us`). A window with fewer than eight pairs is read by arrival alone, said
  once as `audio against video is not read by sender reports: too few pairs of them`. The audio's
  place goes to the drift rule; no other measure decides anything.
- **The drift rule.** Each window of the audio's place goes to the rule of
  `docs/research/13-audio.md` §5.3.4 (`steer.rs`), which judges a run by its sender reports
  or, without them, by its arrival and its timestamps together, and names a step: frames to
  leave out where the recording has the sound late, frames to record twice where it has it
  early. With `IngestOptions::audio_steps` the step is made on the frames received next
  (`audio.rs`): the decode times stay a count without a gap, every later frame of the run
  stands that many frames earlier or later, the fragment that holds the step is marked
  (`iris_mp4::fragment::FrameSteps`), and the live ring carries what is recorded. The
  session logs `a step of the audio is made` with the frames and the error judged, and
  counts the frames named (`StreamStats::sound_steps_said`) and the frames left out or
  recorded twice (`StreamStats::sound_steps_made`). Without the option no frame is left out
  or recorded twice, and the line reads `a step of the audio would be made`. A rule that
  stops says so once, as `no further step of the audio is made in this run` or `would be
  made`, with its reason.
- **Audio that is not recorded.** A stream with another codec, without parameters, or at a
  sampling rate whose frames are not whole ticks (`iris_mp4::init::audio_frame_ticks`)
  records no audio, said once as `audio not recorded`; audio never ends a session, and a
  fragment the sink rejects is dropped. A fragment's wall time is its place on the video's
  timeline.
- **Other offered streams.** A session sets up the H.265 video stream and, when asked, the
  AAC one. At its start it logs up to four other streams of the description (audio,
  metadata) as `offered, not recorded` lines with the media type, encoding name, clock
  rate, and channel count; the camera's text is clipped to 32 characters and escaped.
- **Camera cadence.** With the `iris_ingest::cadence` log target at `debug` (for example
  `RUST_LOG=info,iris_ingest::cadence=debug`), each group of pictures is logged at its end as
  one `group` line: the stream, the frame count, and for every frame its step in camera time
  (`camera_ms`), its arrival step on the recorder's clock (`arrival_ms`), and its size
  (`kib`), up to 600 frames, and the group's least delay (`least_delay_us`: the smallest
  receipt time less camera time among its frames). Audio is logged the same way in groups
  of 125 frames. Without the target nothing is collected. With the `iris_ingest::reports`
  target at `debug`, each RTCP sender report is a `sender report` line with the instant on
  the camera's clock at which the stream's RTP timestamp is zero (`epoch_us`) and how long
  after its NTP time it was received (`late_us`).
- **Camera text in logs.** A session error is logged through `iris_core::text::LogSafe`:
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
  closes, at decode times that increase inside a run even when the regulariser shortens a
  fragment by more than a frame. The fast-start cache retains at most 600 frames and 64 MiB
  of payload from the most recent sync frame; crossing either limit abandons that run until a
  later fitting sync.
  Subscriber fan-out retains each frame once, bounded by the configured frame capacity
  and 64 MiB of payload per stream, and retains no fan-out payload without subscribers.
  Overflow reports lag and delivery resumes at the next sync frame
  (`docs/design/phase2-interfaces.md`).

## Tests and measurements

- Unit tests in `src/regularize.rs` (jitter, drift, gaps, backwards timelines, wrong
  declared rates, passthrough, and generated timelines checked for positive durations, a
  bounded error, and exact snaps), `src/assembler.rs` (first-sync drop, cap splits,
  discontinuities, oversized-frame gaps, parameter-set changes, hold accounting, and generated
  camera timelines with quantised, duplicated, and skipped stamps that must come out whole and
  continuous, within 100 ms of the camera at every fragment for a camera dropping half a
  percent of its frames, over 100 s and over an hour, and within the snap tolerance for one
  dropping three times that in bursts),
  `src/session.rs` (declared-rate parsing, injected realtime steps, wall-clock mapping, the
  shared tick, a pause between attempts that beats every slice), `src/audio.rs` (placement
  on the video's timeline, runs, the continuity rule, the stamp watch, and steps: frames
  left out and recorded twice with decode times that stay a count), `src/sync.rs` (each
  reading of the audio's place and its bounded samples), `src/steer.rs` (the drift rule's
  thresholds, first steps, stops, both ways of judging a run, and steps made against steps
  said), `src/stats.rs`, and `src/live.rs`.
- `tests/replay.rs` replays the camera timeline of a real ten-minute recording (the sidecar
  in `tests/data/`) through the regulariser and checks that it stays locked without snaps.
- `tests/replay_server.rs` records from the `iris-replay` server in both read modes
  (packet-driven and timer-driven), checks that the fragments carry the served frames,
  and makes a replayed camera stall so the timer-driven session's inactivity timeout,
  teardown, and reconnect run on a detached socket. Cancellation checks account for every
  received frame and clear connection state in both read modes. With the server's audio it
  checks the audio on the video's timeline and in the live ring, a lost audio packet, each
  watch of parted clocks under the server's clock faults, what the drift rule says for each,
  the steps made holding a fast and a slow sample clock, and a task's signs of life while
  its camera is silent or down. No hardware is involved.
- `tests/adversarial_rtsp.rs` uses bounded loopback peers in both read modes to cover
  terminal tail flushes, transport-backlog wall placement, RTSP and H.265 allocation
  limits, reconnect recovery, and recording continuity after sink rejection. Hostile cameras
  on OS threads check that a camera which stops reading never stalls the runtime thread
  (a task beside the session ticks with no gap over 500 ms, through the keepalive and the
  `TEARDOWN`) and that a `TEARDOWN` never contacts an advertised host (Basic, Digest, and a
  challenged `TEARDOWN`).
- `tests/log_bounds.rs` installs a log subscriber configured like the daemon's (`info`, no
  target) with ANSI colours off, so the byte checks see only what a camera contributed, and
  checks that hostile
  `DESCRIBE` answers (1 MiB of invalid SDP, 18,000 media sections, unusable sections beside
  the video) log only escaped lines of at most 1 KiB, one per ended session plus retina's
  one summary. `tests/teardown_bound.rs` drops 100 sessions against a camera that never
  answers `TEARDOWN` and checks that 64 background tasks remain. Both bind process-wide
  state, so each is a test binary of its own.
- Measurements: `docs/research/11-multi-camera-results.md` (live cameras) and
  `docs/research/12-ingest-cost.md` (CPU per megabit, the read policy, the low-water mark).

## Further reading

- `docs/research/05-ingest-events.md` §1 (session policy) and §2 (pipeline)
- `docs/research/04-storage-format.md` §7 (time model) and §8 (I/O policy, backpressure)
- `docs/research/12-ingest-cost.md` (where the CPU goes)
- `docs/research/01-camera-probe.md` (what cameras actually send)
- `docs/design/phase2-interfaces.md` (the live ring contract)
- `STANDARDS.md`
