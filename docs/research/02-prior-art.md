# 02 — Prior art: what other NVRs do and what irisd takes from them

Condensed from a source-level survey (full notes with citations: `appendix/A-prior-art.md`).
Versions are as of 2026-10-31.

## 1. Survey

| Project | Language / ingest | On-disk format | Index | Playback | Verdict for irisd |
|---|---|---|---|---|---|
| **Moonfire NVR** 0.7.33 (H.265 since 0.7.19) | Rust; native `retina`, no ffmpeg | Raw sample bytes (the `mdat` payload, no framing), one file per ≈1-minute recording | SQLite: per-recording row + 2-varint-per-frame `video_index` blob; `video_sample_entry` (avcC/hvcC) table | MP4 synthesised on the fly (moov + file slices, `elst` trim), fMP4 over WebSocket for live | The closest ancestor. Copy: zero-decode ingest, synthesised MP4, explicit durability order, NVR-clock-authoritative time model, ±500 ppm wall correction. Avoid: 1-minute file granularity, index only in SQLite (last minute lost on crash by design), lock storms on retention, rejection of odd timestamps (#322) |
| **retina** 0.4.21 | Rust RTSP client used by Moonfire | — | — | — | Use it. TCP default, UDP experimental (no reorder buffer), 64-bit timestamp unwrapping, per-packet receipt wall/monotonic time (0.4.18), ring-buffer reads (0.4.20, 4.6 GiB/s), five years of camera-quirk fixes (Dahua, Hikvision, Reolink, Tapo…). H.265 is a cargo feature. No data-inactivity timeout: wrap `next()` in `tokio::time::timeout` |
| **go2rtc** 1.9.15 | Go; native clients, ffmpeg only for transcoding | none (restreamer) | — | WebRTC/MSE/HLS/RTSP fan-out | Good reference for codec negotiation and browser support tables. No "zero-copy" claim exists in its docs; GC-based, ≈ 50 MB/stream anecdotal. Not a recorder |
| **Frigate** 0.18.2 | Python; ffmpeg subprocess per camera | ffmpeg `-f segment -segment_time 10 -c copy` into tmpfs cache, then `ffprobe` ×2 per segment and a **re-mux with `+faststart`** into `recordings/…/MM.SS.mp4` | SQLite `Recordings` row per 10 s segment | nginx-vod-module HLS over the MP4s | Every kept segment is written ≥ 3× and probed 2×; `MAX_SEGMENTS_IN_CACHE=6` → "unable to keep up"; needs go2rtc for live. Exactly the pipeline to avoid |
| **SentryShot** (Rust, 2026-04) | Rust; git fork of retina (`dts` branch for B-frames) | `<id>.meta` (25-byte sample records: flags, pts ns, dts offset, duration, mdat offset, size) + `<id>.mdat` raw samples + json + jpeg | none (directory crawler) | HLS live + VOD | Confirms "raw samples + compact index" works in Rust; stores pts and dts. No audio, no DB |
| **UniFi Protect** (`.ubv`, reverse-engineered) | native | 1 GB **pre-allocated** files; records = header + size + payload + pad + **back-size**; Clock-Sync packets per session; length-prefixed NALs | seek-index records in-file | proprietary; fMP4 over WebSocket API | The design built "to survive power loss in a limited and highly contended IO environment": large sequential pre-allocated files, self-delimiting records with back-pointers, periodic clock sync |
| **Hikvision / Dahua embedded NVRs** | C firmware | Raw disk, no filesystem: master sector, fixed-size (≈1 GB) video blocks written in real time, circular overwrite; Dahua `DHAV` frames self-delimited (24-byte header + footer back-offset) | HIKBTREE / DHFS index updated on a separate schedule | proprietary | Same lesson: big sequential blocks, self-describing frames, index as a cache. Raw-disk is unnecessary on Linux with preallocated files on ext4/XFS |
| **mediamtx** (Go) | native | native fMP4 or MPEG-TS, `recordPartDuration` parts ("when a system failure occurs, the last part gets lost") | filesystem | `/get?start&duration` → fMP4/MP4 | Validates fMP4-on-disk with part granularity = loss window |
| ZoneMinder | C++, libavformat in-process | per-event MP4/MKV (passthrough or re-encode) | MySQL | — | Decodes for motion detection; parser CVE history |
| Shinobi | Node + ffmpeg | `-f segment`, **re-encodes to H.264 by default** | MariaDB | — | avoid |
| Viseron / motionEye | Python/C + ffmpeg or libav | 5 s MP4 segments / passthrough | PostgreSQL / — | — | still decode for detection |
| Scrypted | TypeScript; own RTSP parser | undocumented | — | — | nothing to copy; storage unknown |
| OasisNvr / crumbvms (small Rust) | GStreamer / Rust | 512 MB preallocated ring files / clock-aligned fMP4 + PostgreSQL | RAM / PostgreSQL | — | ring-buffer-of-preallocated-files idea; H.264 only |

## 2. Reported efficiency numbers (nobody publishes rigorous per-camera figures)

| Project | CPU | RAM | Confidence |
|---|---|---|---|
| Moonfire | six 1080p30 streams < 10 % of a Raspberry Pi 2 (README figure from an earlier release); "roughly halved" again in 0.7.26 by buffering a GOP in RAM and writing once per GOP | runs on 1 GiB boxes | primary / secondary |
| go2rtc | ≈ 5 %/stream passthrough on a Pi 4; ≈ 40 % when transcoding H.265→H.264 | ≈ 50 MB/stream | anecdotal |
| Frigate record (`-c copy`) | ≈ 18 % of one RK3566 core for 1080p20 (85 % with `-c:a aac`) | tmpfs cache | user-measured |
| Frigate detect | ≈ 0.9 core + ≈ 0.7 core decode per camera without hwaccel | — | user-measured |

irisd must therefore measure its own numbers (07 §7) rather than compare against published ones.

## 3. Camera-stream pitfalls and how projects cope

| Pitfall | Seen in | Handling |
|---|---|---|
| Timestamp jumps / backwards | retina allows by default (opt-in max-jump); Moonfire panicked on extreme jumps (#113, #346, fixed); Frigate uses `-use_wallclock_as_timestamps 1 -reset_timestamps 1` | irisd: regulariser + run boundaries (04 §7) |
| Identical RTP timestamp on consecutive frames | Moonfire #322 (open; proposal: derive from frame rate/arrival) — and **our cam1 does this on 4 % of frames** | irisd: CFR regulariser |
| Parameter sets change mid-stream | retina `has_new_parameters()`; Moonfire seals the recording on `video_sample_entry_id` change | irisd: new sample entry → seal file, new init segment |
| Parameter sets repeated at every IDR | Moonfire #43 (wasted space) | irisd: strip into `hvcC` (hvc1) |
| Long / irregular GOP (smart codec) | Scrypted asks for 4 s IDR; Frigate tolerates 2 h playlist drift | irisd: provision cameras (GOP ≤ 2× fps, smart codec off); fragment cap 2 s |
| Consumer falls behind → camera drops session | Moonfire "Stream ended" / "getting next packet took…"; Frigate 10 s socket timeout | irisd: bounded writer queue, drop whole fragments with a logged gap, reconnect with backoff |
| RTCP SR absent, late, or wrong clock | Moonfire uses SR start only if within 5 s of local; retina drops unknown-SSRC RTCP | irisd: SRs diagnostic only |
| NVR clock steps (no RTC on SBCs) | Moonfire: unhandled (wrong until restart) | irisd: monotonic vs realtime watchdog → new run |
| B-frames | Moonfire rejects (pts not monotonic); SentryShot forked retina for DTS | irisd 0.1 alpha: unsupported (no composition offsets; 04 §4, 08 deferred list); Dahua/Hik IPCs emit P-only streams |

## 4. Lessons adopted

1. **Zero decode, no subprocess, no pipe**: the daemon touches only RTP payload bytes.
2. **Big sequential preallocated files** (UniFi, Hik/Dahua, OasisNvr) written once per
   fragment; never re-mux, never stage in tmpfs, never `faststart`.
3. **Self-describing on-disk records with back-pointers** (UniFi, Dahua DHAV) so recovery is a
   local scan, plus a rebuildable index. irisd achieves this with standard fMP4 fragments and a
   24-byte trailer rather than a private format (04).
4. **Synthesise MP4/fMP4/HLS from byte ranges** (Moonfire) instead of writing playback files.
5. **NVR clock is the authority, camera clock supplies durations** (Moonfire time.md), with
   the ±500 ppm correction and "least-delayed frame" anchoring, extended with a CFR
   regulariser because real cameras (cam1) emit 10 ms-quantised, duplicated timestamps.
6. **Explicit durability order** (write → fsync file → fsync dir → catalog row; delete = mark →
   unlink → fsync dir → drop row) and bounded deletion work.
7. **Parameter-set epochs** as first-class objects (sample entries), deduplicated.
8. **Events from the camera** (ONVIF / vendor feed) rather than server-side decoding; on-NVR
   analytics, if added, runs on the secondary stream at low fps and never in the record path.

## 5. What irisd does that none of the surveyed projects do together

- Standard CMAF fMP4 on disk **and** zero-copy byte-range serving **and** crash loss of at most
  one fragment plus the writer queue (2.5 s per fragment at the default cap) **and** index
  rebuildable by local scan.
- Measured CPU/RSS/write-amplification budgets with a reproducible benchmark harness (07 §7).
- Regularised constant-frame-rate timing with the raw camera timeline preserved per fragment.
- One camera connection shared by all clients (live ring → WebSocket per-frame fragments, LL-HLS
  parts, HLS segments, exports) with no transcoding anywhere.
