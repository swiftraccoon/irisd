# irisd design decisions and research

The phase-0 research (2026-10-31) and the decisions taken since, for a Linux H.265 NVR daemon
whose record path never decodes, with a Linux GUI and a planned iOS and macOS app (08, phase 4).
Inputs: a hands-on probe of the first camera (01), five source-level research threads
(appendices A–E), and the measurements in 01. Everything below is either measured, read
from primary sources (specs, source code, crates.io), or marked as an open question.

## Decisions

| Area | Decision | Doc |
|---|---|---|
| Language | Rust, no C core; `panic=abort`, fuzzed parsers, `cargo audit/deny` | 03 |
| Runtime | one tokio `current_thread` runtime for every socket and timer, so a blocking call stalls every task and, with `panic = "abort"`, a panic ends the process; one blocking writer thread per configured root; one SQLite thread; no io_uring | 03, 07 |
| RTSP | retina 0.4.21 with feature `h265`, TCP interleaved, length-prefixed frames, 15 s setup timeout, 30 s inactivity timeout, TEARDOWN on every exit, backoff reconnect | 05 |
| Socket reads | timer-driven, 20 ms shared tick: drain without arming a wake-up, then sleep; plus `SO_RCVLOWAT` 32 KiB after `PLAY` through a patched retina (`third_party/`); together −53 % CPU against packet-driven reads; `0` restores either default | 05 §1, 12 |
| Timing | NVR clock is the authority; RTP for relative timing through a CFR regulariser; per-run wall offset via least-delayed frame with ≤ 500 ppm correction; RTCP SR diagnostic only | 04 §7, 01 §5 |
| On-disk | CMAF-style fMP4 (`hvc1`), one fragment per GOP (cap 2 s), `prft` + 24-byte CRC trailer per fragment, one file per elementary stream, rotate at 1 h / 1 GiB / parameter change / new run, sidecar fragment index | 04 |
| Catalog | SQLite WAL on SSD, segment and event rows only (a run is `segment.run_seq`; no per-fragment rows) | 04 §6 |
| I/O | preallocate 256 MiB ahead, one `pwritev` per fragment, `fdatasync` every 5 s staggered, bounded writer queue, whole-file retention with durable deletion intent, sidecar-tail recovery with full-scan fallback | 04 §8–9 |
| Serving | WebSocket per-frame fragments for live; HLS/LL-HLS as byte ranges; MP4 export assembled with `elst`; keyframe resource for client-side thumbnails; no transcoding anywhere | 04 §10, 06 |
| Audio | AAC as received, off by default: one `audio` stream per camera from the main stream's session, in files of the video's format on the video run's timeline, where time is the count of frames and a run is placed by least receipt delay; served as an HLS rendition whose playlist follows the video's, as the second track of an export, and as a live feed the viewer plays against one clock with the picture. A camera's `audio_steps` holds the sound to the picture by whole frames (`13-audio.md` §5.3); without it nothing bounds drift under the half second of the continuity rule | 13 |
| Events | Dahua CGI push feed and ONVIF PullPoint (`oxvif`); no metadata-track subscription or server-side detection in the 0.1 alpha | 05 §3 |
| Provisioning | configured `[[camera]]` entries are authoritative; `[provisioning]` (off by default) lets the API add and remove cameras within `max_cameras` and `connect_ports`, persisted in the state directory; the daemon's monitor loop owns every change | 07 §5 |
| Discovery | WS-Discovery and ONVIF stream lookup run in the daemon for the Add camera form, root token only, bounded and sanitised, every camera-advertised URL moved onto the operator's host | 05 §5 |
| HTTP | HTTP/1.1 only for the API; HTTP/2 and HTTP/3 stay out (a LAN API with few clients gains nothing that offsets the attack surface) | 07 §5 |
| Camera policy | Operators supply H.265 video with smart codec off, GOP ≤ 2× fps, CBR, and NTP; the third stream serves as the secondary stream on a camera whose sub stream stops beside a second session (01 §5b, 11) | 05 §4 |
| Planned Apple clients | one SwiftUI app for iOS and macOS over the Rust library crate `iris-client`, which the Linux viewer uses. Live: a sample-buffer renderer over WebSocket. Playback: `AVPlayer` + HLS, with the playlists handed over by a resource loader | 15 §4, 06 §2 |
| Linux GUI | gtk4-rs + gstreamer-rs (`gst-plugin-gtk4` as a crate), `vah265dec`/`nvh265dec` | 06 §3 |
| Soak timing | a soak window runs while the change rate is low; a ready build with features awaiting hardware checks closes an open window early, as the third window (25.2 h) was | 11 (Soak, third window) |

## Key measured facts about the test camera (01)

- 4K H.265 Main at 30 fps, 8 Mbps CBR, 2 s GOP (60 frames configured; 442 of 453 measured
  GOPs), single-slice, IDR only,
  VPS/SPS/PPS repeated before every IDR, no SEI/AUD/B-frames.
- RTP: fragmentation units only, 1,440-byte payloads, marker on every access unit, 707
  packets/s; RTCP sender reports every 5 s with sane NTP time.
- RTP timestamps are quantised to 10 ms, duplicated on 6 % of frames, bunched up to 100 ms;
  the camera clock is within a few ppm of the host, but the camera delivers ≈ 29.8 fps while
  declaring 30. ONVIF-reported UTC is
  off by exactly one hour. The camera forces extra IDRs when its own clip recording starts, so GOPs
  are not a constant 60 frames.
- Audio is offered as L16, G.711, or AAC depending on the camera and the stream (13).
- The sub stream stops 30–110 s after a second RTSP session opens on the same camera, on
  one camera model; its main and third streams are unaffected, so there the third stream is
  recorded as the secondary stream (01 §5b, 11).
- Container overhead is 0.03 % in every format; plain MP4 is unreadable after SIGKILL, fMP4
  and MKV survive; a buffered GOP is lost in either.
- Dahua CGI event feed validated with real motion (`VideoMotion` start/stop); ONVIF lists 23
  event topics; the ONVIF metadata track SETUPs but stayed silent through motion episodes.

## Reading order

1. `01-camera-probe.md` — what the hardware actually does.
2. `02-prior-art.md` — what others built and what to copy or avoid.
3. `03-language-runtime.md` — why Rust/tokio, crate list, hardening.
4. `04-storage-format.md` — the format, index, I/O, and time model (the core).
5. `05-ingest-events.md` — RTSP session policy, pipeline, events, provisioning.
6. `06-clients.md` — iOS and macOS, GUI, web.
7. `07-architecture.md` — crates, threads, API, budgets, benchmark and test plan.
8. `08-roadmap.md` — phases with exit criteria.
9. `09-phase1-results.md`, `10-phase2-results.md`, `11-multi-camera-results.md`,
   `12-ingest-cost.md` — dated measurement records.
10. `13-audio.md` — what the cameras offer as audio, the recording design that follows, and
    serving it with the picture: playlists, export, and the live view.
11. `14-drift.md` — the clocks a recording rests on, every pair that can part, the measure
    that shows each parting, and the checks against a clock fault of known size.
12. `15-apple-clients.md` — the planned iOS and macOS viewer: what Apple's decoders and players did
    with the daemon's output on macOS 27, the library crate `iris-client` that the Linux viewer
    uses, and what stays unverified.
13. `../design/phase2-interfaces.md` — the crate contracts the daemon is built on.
14. `appendix/` — research notes with citations as written on 2026-10-31 (A prior art, B storage,
    C clients, D camera integration, E language/runtime); each notes a later edit under its title.

## Top open questions (each has a planned test)

1. AVFoundation playback of byte-range segments spanning several on-disk fragments: they played on
   macOS 27 on 2026-11-15 (`15-apple-clients.md` §3.2, a dated observation); an iPhone or
   `mediastreamvalidator` run remains.
2. `prft` flag semantics and tool acceptance (the same run).
3. Camera backpressure tolerance and metadata-track feasibility (phase 5).
4. ext4 vs XFS under 12 writers over weeks (a seven-day soak).
