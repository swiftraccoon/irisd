# Appendix A — Prior-art survey notes (unedited, 2026-10-31)

Research notes produced by a source-level survey thread. Tags: **VP** verified-primary
(source/docs read), **S** secondary, **I** inferred. Summarised in `../02-prior-art.md`.

## 1. Moonfire NVR — v0.7.33 (2026-10-04), Rust, H.265 since v0.7.19 (2025-01-28; README still says "H.264", stale)
1. **Ingest: native retina, no ffmpeg** (ffmpeg path removed v0.7.3, 2022). TCP/UDP per stream. —
   CHANGELOG, VP.
2. **On-disk = raw sample files + SQLite.** Sample file = concatenation of timestamped samples (the
   `mdat` payload, *no framing*), one per ~1-minute recording, named as 16 lowercase hex digits of a
   composite id (high 32 bits stream id "which improves locality", low 32 = `cum_recordings`); dir
   holds a 512-byte protobuf `meta`. SQLite `recording` row: `start_time_90k`, `wall_duration_90k`,
   `media_duration_delta_90k`, `video_samples`, `video_sync_samples`, `sample_file_bytes`,
   `end_reason`; `recording_playback.video_index` = 2 varints/sample (zigzag
   duration-delta<<1|keyframe, size-delta vs last same-type frame) ≈4 KB/min at 30 fps;
   `video_sample_entry` stores avcC/hvcC + RFC 6381 string. Durability: write → fsync file →
   fsync dir → insert row; delete = `garbage` row → unlink → fsync dir → drop row; last
   minute after crash is *not* recovered ("simpler to just keep recording time short enough"). —
   design/schema.md, server/db/schema.sql, server/db/dir/mod.rs, VP.
3. **Why not MP4/fMP4 per file:** schema.md gives no head-to-head; stated reasons are sub-ms
   metadata random access on flash with no HDD seeks ("not take precious disk time fraction away"),
   ACID, 1-chunk-per-file implicit `stsc`, <1% disk time for seeks; "direct access to stored data
   with standard tools" is an explicit non-goal, as are B-frames and PIR. — VP.
4. **MP4 is synthesized, never written to disk.** `mp4.rs` builds a virtual file of "slices": `moov`
   (stts/stsz/stss from `video_index`, cached in a `OnceLock`) then one 64-bit `mdat` whose byte
   ranges are read straight from sample files; HTTP range requests served without materialization;
   keyframe-aligned start with `elst` trim; `InitSegment`/`MediaSegment` (moof/tfdt/trun, <4 GiB,
   Safari-small `mdat`) for MSE; optional `tx3g` wall-clock subtitle track; BLAKE3 ETag. Live view =
   WebSocket delivering fMP4 segments (chosen over multipart because Chrome allows 6 HTTP/1.1 vs 256
   WS connections). — server/src/mp4.rs, ref/api.md, VP.
5. **Time model:** media duration from RTP timestamps (camera clock); wall duration = media +
   clamp(local_start − start, ±media/2000) i.e. ≤500 ppm correction; run start = min over
   frames of (local receive time − preceding durations) ("least delayed frame wins"); RTCP SR
   start used only if within 5 s of local; sample durations are never stretched (pre-schema-6 did,
   abandoned because audio can't be stretched); NVR clock steps are *not* handled (wrong timestamps
   until restart, common on RTC-less boards); leap seconds unresolved. — design/time.md,
   guide/troubleshooting.md, VP.
6. **CPU:** README: six 1080p/30 streams on a Raspberry Pi 2 at "<10% of total CPU" (old); v0.7.26
   (2026-02-17) "roughly halved CPU" by giving each sample dir a 2-thread I/O pool and buffering
   frames in RAM, writing ~once per GOP; v0.7.21 mimalloc; retina 0.4.20 ring-buffer reads. RAM:
   wiki says runs on 1 GiB; no per-camera figure (third-party ~50–150 MB, unverified). —
   CHANGELOG VP; wiki S.
7. **Pain points / open issues:** v0.7.29 regression (DB not updated on full deletion → unbounded
   SQLite growth); v0.7.30 "severe timestamp drift" introduced by the batched-writer rework;
   #153/#214 lowering retention locks DB "for a minute or more"; #116 retention ignores in-progress
   recording; #43 redundant SPS/PPS stored per keyframe; #322 cameras that send identical RTP
   timestamps are rejected; #231 B-frames → "pts not monotonically increasing", stream drops;
   "Stream ended" when NVR falls behind; full hash verification 8 h at 100 MB/s. H.265 issues #333
   (SPS parse) and #344 (FU NAL type) fixed in retina. — github.com/scottlamb/moonfire-nvr/issues,
   VP.

## 2. retina — v0.4.21 (2026-10-04)
8. **H.265 since 0.4.12 (2025-01-28), followed by a long tail of camera-specific SPS/PPS fixes**
   (Tenda, Xiaomi, Reolink scaling lists, PTL sub-layers, TrailN); unreleased: CRA/BLA IRAP as
   random-access points. — CHANGELOG, VP.
9. **Transport:** TCP interleaved default; UDP "experimental", LAN-only, *no reorder buffer*
   ("out-of-order packets are all dropped"), no RTCP RR sent. — docs.rs `Transport`, README, VP.
10. **Timestamp policy:** 64-bit extension of 32-bit RTP; delta > i32::MAX treated as backward jump
    and *allowed by default* (opt-in `enforce_timestamps_with_max_jump_secs`);
    `InitialTimestampPolicy` (Default = Require if multi-stream else Ignore);
    `UnknownRtcpSsrcPolicy` default DropPackets; RTCP SR received but "only uses the timestamp";
    0.4.18 exposes per-packet wall/monotonic receive times; `VideoFrame` gives `loss()`,
    `has_new_parameters()`, `is_random_access_point()`; `FrameFormat` = 4-byte-length (MP4) or Annex
    B with parameter-set insertion. Keepalive GET_PARAMETER (0.4.21, ffmpeg-like); TEARDOWN before
    close for live555. — docs.rs, VP.
11. **Quirks list is the real value:** Dahua NVR ssrc radix mismatch (0.4.21), Hikvision
    `seq=0`/SSRC whitespace, Reolink trailing SPS bytes, Tapo keepalive, RTP MARK on SEI, Tiandy
    NAL-type changes within FU. Limits: no RTSP/2.0, SRTP, backchannel, replay; "lightly / unevenly
    tested, most depacketizers have no tests"; direct packet access copies each packet (0.4.20). —
    VP.

## 3. go2rtc — v1.9.15 (2026-10-25), Go
12. **Claims "zero-dependency", "zero-delay", "on-the-fly transcoding only if necessary via FFmpeg";
    no "zero-copy" claim exists** (README and web search) — treat that phrase as folklore. Filters
    "do not create a new codec, they only select"; multi-source codec negotiation. No recording of
    its own (Frigate/HA record its `?mp4` output). — README, VP.
13. **H.265 to browsers (README table):** Chrome 136+ WebRTC/MSE H.265; desktop/iPad Safari and
    iPhone iOS 17.1+ MSE H.265; iPhone Safari <17.1 MSE "no!"; HLS H.265 on Safari only ("HLS is the
    worst technology for live streaming, it still exists only because of iPhones"); Firefox H.264
    only. — VP.
14. **Resources:** no official numbers; blog figures ~5% CPU/stream passthrough on RPi4, ~40% when
    transcoding H.265→H.264, ~50 MB RAM/stream; anecdotal reports of CPU creeping to 200%+. — S,
    unverified.

## 4. Frigate — v0.18.2 (2026-10-15), Python + ffmpeg subprocesses
15. **Record path:** ffmpeg `-f segment -segment_time 10 -segment_format mp4 -reset_timestamps 1
    -strftime 1 -c copy` → `/tmp/cache` (tmpfs). `RecordingMaintainer` (~5 s loop) then ffprobes
    each segment (semaphore 4: "probing every cached segment at once starves the camera and
    detection processes"), runs a second per-packet ffprobe for a keyframe index, decides
    keep/discard (all/motion/active_objects), and **re-muxes with `ffmpeg -c copy -movflags
    +faststart`** into `/media/frigate/recordings/YYYY-MM-DD/HH/cam/MM.SS.mp4` → every kept
    segment is written ≥3× and probed 2×; SQLite `Recordings` row
    (start/end/duration/motion/objects/dBFS/keyframes/audio_duration). Segment start is derived from
    file mtime − probed duration, snapped to previous end within 0.5 s — stream timestamps are
    not trusted for wall-clock. — ffmpeg_presets.py, record/maintainer.py, const.py, VP.
16. **Failure modes:** `MAX_SEGMENTS_IN_CACHE=6` → "Unable to keep up with recording segments in
    cache"; tmpfs fills → ENOSPC; `MAX_PLAYLIST_SECONDS=7200` "to account for cameras with
    inconsistent segment times" (copy-mode segments split only at keyframes); H.265 needs `-tag:v
    hvc1` (`force_record_hvc1`) for Safari. — VP.
17. **Playback:** nginx-vod-module (`vod_mode mapped`, fMP4 HLS, keyframe-aligned) over the MP4s;
    export via ffmpeg; 180p preview WebP frames at 1–2 fps re-encoded hourly. go2rtc is bundled
    because "Frigate uses go2rtc to provide its restream and MSE/WebRTC capabilities" and to hold a
    single camera connection. — nginx.conf, output/preview.py, docs, VP.
18. **CPU:** detect dominates: ~0.9 core/camera `frigate.process` + ~0.7 core ffmpeg decode without
    hwaccel; record `-c copy` ≈18% of an RK3566 core for 1080p20 vs ≈85% with `-c:a aac`.
    "Decompressing video streams takes a significant amount of CPU power." — discussions #2186,
    #11498, hardware docs, S (user-measured).

## 5. Scrypted, ZoneMinder, Shinobi, Viseron, motionEye
19. **Scrypted:** TypeScript; prebuffer-mixin uses its *own* RTSP client by default ("Scrypted
    (TCP)" parser; ffmpeg only if selected), 10 s in-RAM prebuffer, rebroadcast RTSP server,
    recommends 4 s IDR; H.265 branch unimplemented in that code ("Video codec is not h264…"). NVR
    plugin is not in the public repo (no `plugins/nvr`); storage format undocumented — docs only
    give ext4/xfs, ≥1 TB, 15%/10% free thresholds, "Large" vs "Fast" disks for main vs sub
    streams. — plugins/prebuffer-mixin/src/main.ts VP; storage I/unverified.
20. **ZoneMinder:** C++ `zmc` links the FFmpeg libraries in-process (TCP default,
    `reorder_queue_size`); Video Writer = Disabled / Encode / Camera Passthrough ("assumes… an
    h.264 stream", no post-processing); events as mp4 (Auto)/mkv/webm + optional JPEGs; MySQL;
    `Decoding` None/OnDemand/KeyFrames/Always governs CPU (None kills live view/thumbnails). —
    readthedocs, VP; forum S.
21. **Shinobi:** Node + ffmpeg subprocess; `-f segment -segment_atclocktime 1 -reset_timestamps 1
    -strftime 1 -segment_list pipe:8`, `movflags=faststart`, `-vcodec copy` only if configured
    (**default re-encodes libx264 for mp4**), `-fflags +genpts+igndts`; MariaDB index; detector gets
    PAM gray frames via pipe. — libs/ffmpeg/builders.js (GitLab), VP.
22. **Viseron:** Python + ffmpeg/gstreamer subprocess; 5 s mp4 segments, `codec: copy` default
    ("practically no resources"), detection via `-f rawvideo -pix_fmt nv12 pipe:1`, PostgreSQL,
    tiered storage with optional tmpfs first tier. — docs, VP.
23. **motion/motionEye:** C, libavformat in-process; `movie_passthrough` writes camera packets
    directly but "Decoding of the image will still occur… for motion detection"; "should reduce
    CPU usage but does increase memory"; `movie_max_time` default 120 s. — motion docs, VP.

## 6. UniFi Protect, Synology
24. **UniFi `.ubv` (reverse-engineered):** big-endian records = 8-byte header + size + payload + pad
    + *back-size* ("efficient reverse seeking"); file = partitions per session, each opening with a
    Clock Sync packet (DTS→UTC ms); length-prefixed H.264/H.265 NALs, AAC ADTS; per-track 16-bit
    sequence counter (gaps = drops); no B-frames; seek-index records; **1.0 GB pre-allocated files**
    `video/yyyy/mm/dd/<mac>_0_rotating_<ms>.ubv`; "designed to be easily written sequentially" to
    survive power loss in a "limited and highly contended IO environment". `ubnt_ubvinfo` was
    dropped from recent releases. — github.com/petergeneric/unifi-protect-remux/wiki, VP (RE).
25. **Synology:** per-camera folders in the `surveillance` share, truncation 1–240 min, downloads
    MP4/AVI, "Recording indexing synchronizes recordings listed in Surveillance Station with actual
    files in File Station" ⇒ standard files + DB index (I); encrypted shares block SMB/File
    Station; H.265 playback depends on client GPU. — synology.com spec, S.

## 7. Hikvision / Dahua embedded NVRs
26. **Hikvision:** no filesystem; Han, Jeong & Lee 2015 (ICDF2C) describe four sections — master
    sector at 0x200 (256 B, "HIKVISION@HANGZHOU", holds capacity, offsets, block size/count), system
    logs, fixed-size video data blocks (reported "typically 1 GB", raw NALs, one channel per block),
    and HIKBTREE index (channel, start/end time → block). Frames hit disk in real time, index
    "updated on a separate schedule"; circular overwrite. — paper S (PDF 403'd); rossmanngroup.com
    S.
27. **Dahua:** DHFS 4.1 proprietary index; frames self-delimited as `DHAV`…`dhav` with a 24-byte
    header (type 0xfd video/0xf0 audio, channel, frame no., length, packed date, timestamp, TLV ext
    with codec 0xc=HEVC, resolution, fps) and a footer back-offset enabling backward scans — this
    is why forensic carving works without the index. — FFmpeg libavformat/dhav.c VP (frame
    format); DHFS layout S. (Rzayeva 2025 MDPI *Information* and DFRWS 2026 papers exist; both
    403'd.)

## 8. Other Rust / native projects
28. **SentryShot** (365★, Rust, pushed 2026-04): ingest via a *git fork* of retina (branch `dts`
    → B-frame/DTS support), no ffmpeg; per-recording `<id>.meta` (version, startTimeNS, w/h,
    extradata, then 25-byte samples: flags, pts i64 ns, dts_offset i32, duration u32, mdat offset,
    size) + `<id>.mdat` raw samples + `.json` + `.jpeg`; **no database** — crawler walks
    `Year/Month/Day/Monitor`; stated requirements "valid in case of system failure… readable as
    soon as written… binary search… Must support B-frames"; audio unsupported; HLS live + vod.
    — src/recording/readme.txt, src/recdb/crawler.rs, VP.
29. **OasisNvr** (4★): GStreamer ingest; pre-allocated 512 MB ring-buffer pool files, RAM index
    rebuilt from embedded headers, single sequential writer ("HDD head only moves forward");
    H.264/AV1 only. **crumbvms** (30★, active): Rust, clock-aligned keyframe fMP4 segments +
    PostgreSQL index, H.265 passed through to libmpv/Media3. Dead/small: rrtsp_client (2022), nvr-rs
    (2023), ClusterVMS/rtsp-recorder (archived), keeppeek (alpha), satori. Crates: retina 0.4.21;
    retina-record 0.2.2 (2024-04, CLI); rtsp-types 0.1.3 (parser only); oddity-rtsp-server (server,
    on ffmpeg); video-rs 0.12.0 (ffmpeg wrapper); gstreamer-rs 0.25.4. — GitHub/crates.io APIs,
    VP.
30. **mediamtx** (Go): native fMP4/MPEG-TS recording with `recordPartDuration` parts ("When a system
    failure occurs, the last part gets lost"), H.265 OK, playback `/get?start&duration` returns fMP4
    or mp4 for arbitrary ranges. — mediamtx.org docs, VP.

## Cross-cutting
**(a) Zero-decode, no ffmpeg pipe:** Moonfire, SentryShot, mediamtx, UniFi, Hikvision/Dahua,
OasisNvr (GStreamer lib, no decode), Scrypted ingest (own client; NVR storage unknown).
ffmpeg-subprocess copy (no decode but a pipe + remux): Frigate, Viseron, Shinobi (if configured).
In-process libav passthrough: ZoneMinder, motion (both still decode for detection).

**(b) Reported footprint**
| Project | CPU | RAM | Conf. |
|---|---|---|---|
| Moonfire | 6×1080p30 <10% of a Pi 2; halved again in 0.7.26 | runs on 1 GiB total | VP / S |
| go2rtc | ~5%/stream Pi 4 passthrough; ~40% transcoding | ~50 MB/stream | S |
| Frigate record (copy) | ~18% of 1 RK3566 core @1080p20 (85% with aac) | tmpfs cache | S |
| Frigate detect | ~0.9 core + ~0.7 core decode/camera (no hwaccel) | — | S |
| Viseron copy | "practically no resources" | — | VP (qualitative) |
| Scrypted/ZM/Shinobi/UniFi/Synology/Hik/Dahua | none published | — | — |

**(c) Camera-stream pitfalls**
- *Timestamp jumps/backwards:* retina allows by default (opt-in max-jump enforcement); Moonfire
  panicked on extreme PTS jumps (#113, #346 fixed) and rejects non-monotonic pts (B-frames); Frigate
  sidesteps with `-use_wallclock_as_timestamps 1`, `-reset_timestamps 1`, mtime-based segment times;
  SentryShot stores pts+dts_offset.
- *Identical RTP timestamps every frame:* Moonfire #322 open (proposal: derive from frame
  rate/arrival).
- *Parameter-set changes mid-stream:* retina `has_new_parameters()`; Moonfire closes the recording
  on `video_sample_entry_id` change and supports it in live view only since 0.7.31; cameras resend
  SPS/PPS each keyframe (Moonfire #43 wasted space).
- *Keyframe spacing:* Moonfire buffers a whole GOP in RAM; Frigate copy segments drift (2 h playlist
  tolerance); Scrypted asks for 4 s IDR; vod aligns to keyframes.
- *TCP stalls/reboots:* Moonfire logs "Stream ended"/"getting next packet took PT…S" when the
  consumer lags (backpressure kills streams); retina sends TEARDOWN to dodge live555 stale sessions,
  keepalive at half timeout; Frigate 10 s socket timeout.
- *RTCP absence:* Moonfire uses local time unless SR within 5 s; retina drops unknown-SSRC RTCP by
  default (Tapo).
- *Clock drift:* Moonfire ±500 ppm wall correction, no step handling; UniFi periodic Clock Sync
  packets; Hik/Dahua rely on their own RTC.

**(d) Lessons:** copy Moonfire's split (sequential raw samples + compact index + synthesized
MP4/fMP4), its explicit durability invariants, and retina's quirk handling; copy UniFi/Dahua
self-delimiting records with back-pointers and periodic clock-sync; avoid Frigate's
write-3×/probe-2× pipeline, tmpfs staging, and ffprobe-per-segment; avoid SQLite row-per-minute
without bounded deletion (0.7.29, #153); avoid rejecting B-frames/odd timestamps outright
(SentryShot forked retina for DTS).

## Recommendations for irisd
- Use retina (TCP) — it is the only maintained Rust client with H.265 and a 5-year camera-quirk
  corpus; budget for upstreaming Dahua fixes.
- Store raw samples in large pre-allocated, sequentially written files (UniFi/OasisNvr style, e.g. 1
  GB per stream) with a compact self-delimiting per-frame header + back-size, so recovery works
  without the index; keep the index (SQLite or own B-tree) on the NVMe/SSD, not the HDDs.
- Write once per GOP with `writev`/O_DIRECT-sized buffers; fdatasync on a timer, not per frame;
  never rewrite (no faststart, no remux, no tmpfs staging).
- Synthesize MP4/fMP4 on demand from the index (Moonfire mp4.rs); serve HLS-fMP4 for iOS and MSE for
  desktop; tag HEVC `hvc1`.
- Store pts *and* dts per sample (i64 ns) so B-frame cameras are not rejected; never stretch media
  timestamps — keep a separate wall mapping with periodic clock-sync records and ±500 ppm
  correction.
- Treat parameter-set changes as a new "sample entry" epoch, dedupe repeated SPS/PPS.
- Bound deletion work: fixed-size blocks make retention O(1) and avoid DB lock storms.
- Use NVDEC only for playback/thumbnails/analytics, never in the record path; drive motion from
  camera events (ONVIF/Dahua) or sub-stream keyframes.
- Measure your own CPU/RSS; no project publishes trustworthy per-camera numbers.

## Open questions
- Hikvision block size (1 GB) and index-update cadence are vendor-blog claims; the 2015/2023/2026
  papers were inaccessible (403).
- Scrypted NVR on-disk format and Synology native container remain unverified.
- Whether retina's lack of a UDP reorder buffer matters on your LAN (TCP avoids it).
- go2rtc per-stream RAM/CPU and whether its H.265 MSE path works on iOS <17.1 (README says no).
