# AGENTS.md — iris-ingest

Scope: this crate only. Read the root `AGENTS.md` first.

## Invariants

- The regulariser's rules (nominal durations, 1 % per-fragment correction, holds for
  dropped frames, override after warm-up, snap beyond the tolerance, restart of a stale
  estimate after four saturated corrections) and its constants in
  `src/regularize.rs` implement `docs/research/04-storage-format.md §7`; change them only
  with that section and `tests/replay.rs`.
- Fragment boundaries (sync frame, new parameter sets, cap plus a quarter measured by
  RTP, receipt time, or nominal samples) and hard 4,096-frame/64 MiB bounds implement
  `05-ingest-events.md §2`. An oversized frame presented directly to the assembler flushes
  retained frames before marking a discontinuity. Frames are discarded until the next sync
  frame, which starts a separate run at decode time zero and retains the gap in wall time.
- The patched Retina H.265 depacketizer enforces the same 64 MiB limit while accumulating
  pinned RTP payload and projected output, before it constructs a `VideoFrame`. Production
  RTSP connections limit each control message's head plus body to 1 MiB. Keep both limits
  aligned with `05-ingest-events.md §1–2` and the provenance note in `third_party/README.md`.
- Session policy (TCP interleaved, 15 s setup timeout, 30 s inactivity, `TEARDOWN` always,
  10 s jump limit, backoff) implements `05-ingest-events.md §1`.
- Nothing a camera controls may block the runtime thread or reach another host. The patched
  Retina never blocks on a detached write, bounds camera-supplied fields, sends a fresh
  `TEARDOWN` only to the session's own peer address, stops a background `TEARDOWN` after
  min(session timeout, 60 s), and runs at most 64 of them per process. These are local
  contracts listed in `third_party/README.md`; keep `05 §1` in step with them.
- Camera-supplied text reaches a log line only through `iris_core::text::LogSafe` (escaped, at
  most 512 characters) or as one of Retina's bounded excerpts; never log a `retina::Error`, an SDP,
  or a header value with `%` or `?` directly. `StreamSource`'s `Debug` stays redacting.
- The read policy (noop-waker drain, the shared `ReadTicker`, the socket detached from
  the reactor after `PLAY`) and the low-water mark are measured decisions in
  `12-ingest-cost.md §3–4, §6`; a change needs measurements from
  `iris-tools replay-bench`, including the syscall mix (`perf record -e
  raw_syscalls:sys_enter -p PID`).
- `FragmentSink::send` must not block the ingest task. Rejection ends recording continuity,
  counts one gap for the loss epoch, and resumes only from a sync frame in a run whose
  decode time starts at zero; it does not stop live publication.
- Every terminal path after session setup flushes complete pending frames through the same
  accounting and delivery path exactly once before the reconnect loop clears connection
  state. The outer loop must not race cancellation against the session future.
- Fresh realtime/monotonic correlations detect recorder clock steps over 2 s and end the
  run. Sealed-fragment wall placement uses finalized media durations and a per-run
  least-delay anchor whose upward correction is limited to 500 ppm.
- The live fast-start cache and subscriber fan-out have independent 64 MiB payload-reference
  budgets in addition to their frame-count limits. Overflow abandons fast start or reports
  lag, respectively; neither path resumes delivery before a sync frame.
- The names of `StreamStats` accessors are read by `irisd`'s statistics line, by
  `iris-tools daemon-stats`, and by the API's camera detail; renaming one changes the log
  format and the HTTP answer.

## Where things are

- `src/session.rs` — `run_stream`, the reconnect loop, one `run_session` per RTSP session,
  the demux loop with both read modes, RTCP sender-report handling, gap logging.
- `src/assembler.rs` — `Frame` → `Fragment` assembly, hold and snap accounting.
- `src/regularize.rs` — the regulariser and its constants.
- `src/source.rs` — `StreamSource` and `IngestOptions` with their defaults.
- `src/sink.rs` — `FragmentSink` and `SinkError`.
- `src/stats.rs` — `StreamStats` counters and their `pub(crate)` mutators.
- `src/live.rs` — the live ring.
- `tests/replay.rs`, `tests/data/` — the recorded-timeline regression test.
- `tests/replay_server.rs` — end-to-end recording from `iris-replay` in both read modes.
- `tests/adversarial_rtsp.rs` — bounded loopback regressions for terminal flushes, wall
  placement, network allocation limits, and sink-loss continuity in both read modes, and
  hostile cameras that stop reading or advertise another host.
- `tests/log_bounds.rs` — a subscriber configured like the daemon's against hostile
  `DESCRIBE` answers;
  `tests/teardown_bound.rs` — the process-wide bound on background `TEARDOWN`s. Each owns
  process-wide state, so each is a test binary of its own.

## How to verify a change

```
cargo fmt -p iris-ingest
cargo clippy -p iris-ingest --all-targets -- -D warnings
cargo test -p iris-ingest
RUSTDOCFLAGS="-D warnings" cargo doc -p iris-ingest --no-deps
```

The integration tests need no hardware: `tests/replay_server.rs` serves a synthetic
recording from `iris-replay` on a loopback port. Never point a test at a live camera
(root rules 6 and 9). For a cost claim, run `iris-tools replay-bench` with
`--replay-arg=--pace` and record the numbers in `docs/research/12-ingest-cost.md`.

## Pitfalls

- Timer-driven reads poll the demuxer with a no-op waker: readiness is retained by tokio
  until a read returns `WouldBlock`, which is why the drain loop must run to `Pending`
  before sleeping. Polling with the task's real waker reintroduces one wake-up per packet.
- `aligned_tick` derives every stream's deadline from one process-wide epoch; a per-stream
  epoch loses the single shared wake-up. A timer per stream, even at the same instant,
  costs one runtime unpark per stream per tick: use the ticker.
- A detached socket (`PlayOptions::detach_from_reactor`) must only be polled by the
  timer loop: an ordinary `.await` on the demuxer would never be woken, neither for a read
  nor for the rest of a keepalive the camera has not yet accepted. The packet-driven mode
  never detaches.
- The hostile-camera tests run their cameras on OS threads with blocking sockets so they
  keep going if the runtime under test blocks; a camera closes its stalled connection after
  a fixed hold, which also releases a blocked writer, so a regression fails instead of
  hanging. Keep that shape for any test whose peer stops reading.
- The inactivity timeout in timer mode is measured from the last item, not from the last
  readiness; keep `last_item` updated for every item kind, RTCP included.
- Hold accounting is a delta against the assembler's totals (`holds_seen`); reading the
  totals twice double-counts.
- Receipt wall times remain raw observations for fragment caps and clock-step detection;
  sealed fragment walls come from the run anchor plus finalized decode time.
- Parameter sets are stripped from samples (`hvc1`); a new set closes the pending fragment
  with the old entry before the new one applies. Reset the regulariser's estimation history
  only when the parameter epoch supplies a different valid effective frame rate; preserve
  decode continuity and cumulative diagnostics.
- The live ring is fed before the fragment closes, so `LiveFrame::duration` is provisional.
- Camera-side defects (sub-stream stalls, timestamp quantisation) are documented in
  `docs/research/01-camera-probe.md` and `11-multi-camera-results.md`; do not "fix" them by
  changing camera settings.

## Related documents

- `docs/research/05-ingest-events.md`, `04-storage-format.md §7–8`, `12-ingest-cost.md`,
  `01-camera-probe.md`, `docs/design/phase2-interfaces.md`.
- Depends on `iris-core`, `iris-h265`, `iris-mp4`, and `retina` (patched copy under
  `third_party/retina`). Depended on by `irisd` and `iris-api` (live ring, statistics);
  `iris-replay` is a dev-dependency.
