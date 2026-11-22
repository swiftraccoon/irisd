# AGENTS.md — iris-replay

Scope: this crate only. Read the root `AGENTS.md` first.

## Invariants

- Delivery must stay camera-like: one TCP segment per RTP packet by default, in-band parameter
  sets before sync samples, RTCP sender reports at `Options::report_interval` (five seconds by
  default), decode-time pacing. `docs/research/12-ingest-cost.md §2` records why; a change that
  alters delivery re-runs the fidelity comparison there.
- RFC 7798 layout is pinned by the `rtp` tests (header bits, start and end flags, marker on
  the last packet of an access unit). MTU admission is `256..=65535`; packetization and
  interleaving are fallible and packetization stays streaming.
- Optional synthetic audio is mono AAC-hbr over RFC 3640 on `trackID=1`, one frame every 1,024
  samples. `Options` controls its sample rate, timestamp wander, lost or omitted frames, the
  relative camera and sample-clock rates, the clock that stamps audio, and whether and how
  audio sender reports are made. Every fault is a fixed function of a frame's or a report's
  number, so ingest tests name the readings they expect; the video's SSRC and the instants
  of the reports differ from run to run.
- The server is a test instrument, not a product: no authentication, TCP only, bounded to
  what the recorder's client needs. The binary listens outside loopback only with
  `--allow-remote` (`Server::bind` binds what it is given), serves at most
  `Options::max_connections` connections (closing the rest at accept), and closes a
  connection whose request is incomplete after `request_timeout` before `PLAY` (interleaved
  frames do not extend it) or that stays silent for `idle_timeout` after it, and one whose
  reply cannot be queued within the deadline in force. Keep `idle_timeout` well above the
  recorder's 30-s keepalive, or long benchmarks lose their sessions.
- The library's errors are `thiserror` types (`MediaError`, `RtpError`, `SynthError`,
  `ServerError`, `InvalidMountName`); `anyhow` belongs to `main.rs` alone.
  `MediaError::ParameterSets` carries the `HvccError` of the `hvcC` parser of `iris-mp4`
  (`iris_mp4::decoder_config`). Units and ids are newtypes: `Ticks`, `RtpTimestamp`, `Ssrc`,
  `MountName`.
- Client-controlled header parsing lives in `rtsp.rs` (`parse`, `mount_name`,
  `interleaved_transport`, `session_id`), where the `rtsp_request` fuzz target reaches it;
  keep it there rather than in `server.rs`.
- Synthetic recordings are real irisd files (scanner and CRC clean); `synth` must track the
  format in `iris-mp4`. Its public builders are fallible and enforce their documented GOP,
  frame, payload, NAL, duration, and clock bounds before allocating the recording.
- Normal mounts fail closed unless the recording is CRC-clean through exact EOF, with samples in
  every fragment, complete nonempty access units, an initial sync sample, and a representable
  nonoverlapping timeline. The recording's `hvcC` must be version 1 with four-byte lengths and carry
  a VPS, an SPS, and a PPS. An `hvcC` with more than 16 VPSs, 16 SPSs, or 64 PPSs is refused
  (`HvccError::TooManyParameterSets`). `Media` keeps that state private, so every frame it lists has
  a sample.
- The RTSP surface is one DESCRIBE/PLAY lifecycle bound to one mount, the issued Session, and
  negotiated TCP channels: video has one required `SETUP`; offered audio permits one further
  `SETUP` after video and before `PLAY`, on the same mount and in the same Session. `PAUSE`
  and replaying a started/finished session stay unsupported; `TEARDOWN` must preserve
  preceding control responses, discard stale queued media, and acknowledge its terminal
  response only after the socket write completes.

## Where things are

- `main.rs` — the command line.
- `media.rs` — recording loader and frame table; it reads the parameter sets with
  `iris_mp4::decoder_config::parse_parameter_sets` (`crates/iris-mp4/src/decoder_config.rs`).
- `rtp.rs` — packetizer, sender reports, interleaving.
- `rtsp.rs` — request parsing, responses, mount names.
- `sdp.rs` — session description and base64.
- `server.rs` — listener, per-connection state machine, the paced sender and fault injection.
- `synth.rs` — synthetic recordings and the shared parameter sets.
- `tests/loopback.rs` — end-to-end through the recorder's RTSP client.

## How to verify a change

```sh
cargo fmt -p iris-replay
cargo clippy -p iris-replay --all-targets -- -D warnings
cargo test -p iris-replay
RUSTDOCFLAGS="-D warnings" cargo doc -p iris-replay --no-deps
cargo test -p iris-ingest --test replay_server --test adversarial_rtsp --test teardown_bound --test log_bounds
just fuzz-clippy   # after a change to rtsp.rs
```

A delivery change is measured with `iris-tools replay-bench` in `--burst`, default,
and `--pace` modes and compared with the live figures in `docs/research/12-ingest-cost.md`.

## Pitfalls

- `--stall-after` resumes with a burst on purpose; do not "fix" it by shifting the clock.
- Sender reports still sample their wall/RTP clock pair at one instant across that stall
  (`Clocks::late_reports` is the one fault that parts them);
  do not derive the SR RTP field from the delayed media frame.
- Over loopback a whole frame written at once arrives as one segment; per-packet writes are
  what make the harness representative.
- The RTSP parser must skip interleaved frames from the client (RTCP receiver reports) or a
  session wedges after `PLAY`.
- Loop playback keeps timestamps continuous by adding the file's total duration. The loader
  rejects a file whose first sample is not a sync sample, since the recorder could not start
  from it.
- Never point the replay at, or confuse it with, a live camera: it listens on loopback by
  default for that reason.

## Related documents

- `docs/research/12-ingest-cost.md`, `docs/research/07-architecture.md` §7,
  `docs/research/05-ingest-events.md` §1, `crates/iris-tools/README.md`, RFC 7798 under
  `docs/standards/ietf/`.
