# irisd research (phase 0) — 2026-10-31

Research for a Linux H.265 NVR daemon whose record path never decodes, with a Linux GUI and
an iOS app to follow. Inputs: a hands-on probe of the first camera (01), five source-level
research threads (appendices A–E), and the measurements in 01. Everything below is either
measured on cam1, read from primary sources (specs, source code, crates.io), or marked as an
open question.

## Decisions

| Area | Decision | Doc |
|---|---|---|
| Language | Rust, no C core; `panic=abort`, fuzzed parsers, `cargo audit/deny` | 03 |
| Runtime | tokio `current_thread` (opt-in 2 workers); one blocking writer thread per disk; one SQLite thread; no io_uring | 03, 07 |
| RTSP | retina 0.4.21 with feature `h265`, TCP interleaved, length-prefixed frames, 15 s setup timeout, 30 s inactivity timeout, TEARDOWN on every exit, backoff reconnect | 05 |
| Socket reads | timer-driven, 20 ms shared tick: drain without arming a wake-up, then sleep; plus `SO_RCVLOWAT` 32 KiB after `PLAY` through a patched retina (`third_party/`); together −53 % CPU against packet-driven reads; `0` restores either default | 05 §1, 12 |
| Timing | NVR clock is the authority; RTP for relative timing through a CFR regulariser; per-run wall offset via least-delayed frame with ≤ 500 ppm correction; RTCP SR diagnostic only | 04 §7, 01 §5 |
| On-disk | CMAF-style fMP4 (`hvc1`), one fragment per GOP (cap 2 s), `prft` + 24-byte CRC trailer per fragment, one file per elementary stream, rotate at 1 h / 1 GiB / parameter change / new run, sidecar fragment index | 04 |
| Catalog | SQLite WAL on SSD, segment/run/event rows only (no per-fragment rows) | 04 §6 |
| I/O | preallocate 256 MiB ahead, one `pwritev` per fragment, `fdatasync` every 5 s staggered, bounded writer queue, whole-file retention by free space, size quota, and age, scan-based recovery | 04 §8–9 |
| Serving | WebSocket per-frame fragments for live; HLS/LL-HLS as byte ranges; MP4 export assembled with `elst`; keyframe resource for client-side thumbnails; no transcoding anywhere | 04 §10, 06 |
| Events | Dahua CGI push feed → ONVIF PullPoint (`oxvif`) → ONVIF metadata track; no server-side detection in v1 | 05 §3 |
| Camera policy | H.265, smart codec off, GOP ≤ 2× fps, CBR, AAC audio, NTP to the NVR; the third stream serves as the secondary stream on the family whose sub stream stops beside a second session (01 §5b, 11), otherwise it is disabled | 05 §4 |
| iOS | `AVSampleBufferDisplayLayer` over WebSocket for live; `AVPlayer` + HLS for playback/AirPlay | 06 §2 |
| Linux GUI | gtk4-rs + gstreamer-rs (`gst-plugin-gtk4` as a crate), `vah265dec`/`nvh265dec` | 06 §3 |

## Key measured facts about the test camera (01)

- 4K H.265 Main at 30 fps, 8 Mbps CBR, 2 s GOP of exactly 60 frames, single-slice, IDR only,
  VPS/SPS/PPS repeated before every IDR, no SEI/AUD/B-frames.
- RTP: fragmentation units only, 1,440-byte payloads, marker on every access unit, 707
  packets/s; RTCP sender reports every 5 s with sane NTP time.
- RTP timestamps are quantised to 10 ms, duplicated on 6 % of frames, bunched up to 100 ms;
  the camera clock is within a few ppm of the host, but the camera delivers ≈ 29.8 fps while
  declaring 30. ONVIF-reported UTC is
  off by exactly one hour. The camera forces extra IDRs when its SD-card clips start, so GOPs
  are not a constant 60 frames.
- Audio is raw L16 PCM at 64 kHz (1 Mbps) on main and sub; the third stream (1080p, 6 Mbps,
  AAC) is on by default.
- The sub stream stops 30–110 s after a second RTSP session opens on the same camera, on
  every unit of the family and on two firmware builds; main and third streams are unaffected,
  so the third stream is recorded as the secondary stream (01 §5b, 11).
- Container overhead is 0.03 % in every format; plain MP4 is unreadable after SIGKILL, fMP4
  and MKV survive; a buffered GOP is lost in either.
- Dahua CGI event feed validated with real motion (`VideoMotion` start/stop); ONVIF lists 23
  event topics; the ONVIF metadata track SETUPs but stayed silent through four motion episodes.

## Reading order

1. `01-camera-probe.md` — what the hardware actually does.
2. `02-prior-art.md` — what others built and what to copy or avoid.
3. `03-language-runtime.md` — why Rust/tokio, crate list, hardening.
4. `04-storage-format.md` — the format, index, I/O, and time model (the core).
5. `05-ingest-events.md` — RTSP session policy, pipeline, events, provisioning.
6. `06-clients.md` — iOS, GUI, web.
7. `07-architecture.md` — crates, threads, API, budgets, benchmark and test plan.
8. `08-roadmap.md` — phases with exit criteria.
9. `09-phase1-results.md`, `10-phase2-results.md`, `11-multi-camera-results.md`,
   `12-ingest-cost.md` — dated measurement records.
10. `../design/phase2-interfaces.md` — the crate contracts the daemon is built on.
11. `appendix/` — unedited research notes with citations (A prior art, B storage, C clients,
    D camera integration, E language/runtime).

## Top open questions (each has a planned test)

1. AVFoundation playback of byte-range segments spanning several on-disk fragments (phase 1).
2. `prft` flag semantics and tool acceptance (phase 1).
3. Camera backpressure tolerance and real-event validation on all three event sources (phase 1–2).
4. ext4 vs XFS under 12 writers over weeks (phase 2 soak).
5. Fragment cap default: 2 s vs 0.5 s (phase 1 measurements).
