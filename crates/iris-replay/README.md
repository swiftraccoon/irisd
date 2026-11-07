# iris-replay

A minimal RTSP server that serves irisd recordings, or synthetic ones, as any number of
H.265 cameras over TCP-interleaved RTP. It exists so the daemon can be benchmarked and
fault-tested without hardware: cost measurements, chaos runs, and the integration tests of
`iris-ingest` all use it.

## What it provides

- The `iris-replay` binary: `--mount NAME=PATH` serves a recording at
  `rtsp://<listen>/NAME`; `--synth NAME[=GOPS]` serves a synthetic 1080p stream built from
  real parameter sets and dummy slices; `--listen` (default `127.0.0.1:8554`), `--mtu`,
  `--once`, `--burst`, `--pace`, `--drop-every N`, `--stall-after DURATION`,
  `--stall-for DURATION`.
- `media` — `Media` loads a recording into memory, extracts VPS, SPS, and PPS from its
  `hvcC`, and indexes every frame with its decode time (relative to the file's first
  fragment, since stored decode times run on across the files of a run) and sync flag.
- `rtp` — `Packetizer` returns a fallible streaming iterator of RFC 7798 packets (single
  NAL units and fragmentation units, marker bit on the last) and builds RTCP sender
  reports; fallible `interleave` frames one packet for a TCP channel.
- `rtsp` — a request parser and response builder for `OPTIONS`, `DESCRIBE`, `SETUP`, `PLAY`,
  `TEARDOWN`, `GET_PARAMETER`, and `SET_PARAMETER`. `PAUSE` is deliberately unsupported:
  replay is start-once so its advertised RTP state remains truthful.
- `sdp` — the session description with `sprop-vps`, `sprop-sps`, `sprop-pps`.
- `server` — `Server` binds a listener and runs one session per connection; `Options`
  carries the delivery and fault settings.
- `synth` — `Plan` and fallible `recording` build a bounded, valid irisd recording in
  memory.

## How it is used

```sh
cargo build --release -p iris-replay
target/release/iris-replay --mount main=recording.mp4 --pace
ffprobe -rtsp_transport tcp rtsp://127.0.0.1:8554/main
```

Every connection gets its own paced session of the same mount, so one process stands in for
as many cameras as there are connections. `iris-tools replay-bench` starts the server
and the daemon together and reports the daemon's cost; see `crates/iris-tools/README.md`. From
code, `Server::bind` on `127.0.0.1:0` gives a port for tests, as `tests/loopback.rs` and
the two-mode recording test in `iris-ingest` do.

## Invariants and limits

- Packetization follows RFC 7798 §4.4.1 and §4.4.3 (single NAL unit packets and
  fragmentation units; no aggregation packets, no DONL), parameter sets are sent in-band
  before every sync sample as cameras do, and a sender report goes out every five seconds
  with the stream's own SSRC. RTP MTU is admitted only in `256..=65535`; packets are
  produced one at a time and remain representable by RTSP interleaving. RTCP packet/octet
  fields use the low 32 bits of the continuing totals, rolling over instead of freezing.
- Frames are paced by decode time. Each RTP packet is written with its own system call by
  default, and `--pace` spreads a frame's packets over the frame interval; this reproduces
  the receiver's per-packet cost, which `--burst` understates by four times
  (`docs/research/12-ingest-cost.md §2`).
- A dropped packet (`--drop-every`) is simply not sent; a stall (`--stall-after`) pauses
  once and then sends the paused frames in a burst with their original timestamps, as a
  network stall would deliver them. Sender reports sample wall and RTP clocks at the same
  instant; this does not change the intentional catch-up burst.
- Only TCP interleaved transport is accepted (`SETUP` without it gets `461`); there is no
  authentication. A connection must select one mount, complete a valid `SETUP`, and return
  the issued `Session` before `PLAY`; cross-mount and repeated/finished plays fail closed,
  and `TEARDOWN` preserves prior control replies but bypasses stale queued media on a bounded,
  delivery-acknowledged terminal path.
- Mount names are one URI segment and must be unique. Recordings must be CRC-clean irisd
  files through exact EOF, start with a truthful sync sample, contain completely framed
  access units, have a nonoverlapping representable timeline, and carry a version-1
  four-byte-length `hvcC` with VPS, SPS, and PPS. Valid forward timeline gaps are retained.
- Synthetic plans are checked before generation: at most 100,000 GOPs, 65,535 frames per
  GOP, 1,000,000 frames and 1 GiB of sample payload in total; durations and NAL units must
  be nonzero/representable. The constructors return diagnostics instead of panicking.

## Tests and measurements

Unit tests cover strict `hvcC`, CRC/EOF/AU and timeline admission; RTP MTU, streaming
fragmentation, rollover and interleaving boundaries; RTSP mount/session/channel/state and
TEARDOWN behavior; grouped SDP parameter sets; and every synthetic plan ceiling.
`tests/loopback.rs` serves a synthetic recording and receives it through the recorder's own
RTSP client, checking bytes, sync flags, and timestamps. The fidelity of the replay against
live cameras and the measurements it enabled are in `docs/research/12-ingest-cost.md`.

## Further reading

- [`docs/research/12-ingest-cost.md`](../../docs/research/12-ingest-cost.md) — harness
  fidelity and the measurements.
- [`docs/research/07-architecture.md`](../../docs/research/07-architecture.md) §7 — the
  benchmark harness plan.
- [`docs/research/05-ingest-events.md`](../../docs/research/05-ingest-events.md) §1 — what
  the client expects of a camera.
- [`crates/iris-tools/README.md`](../iris-tools/README.md) — the driver.
- [`STANDARDS.md`](../../STANDARDS.md).
