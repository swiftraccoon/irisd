# 04 — On-disk format, index, I/O policy, and time model

Full research notes with citations: `appendix/B-storage.md`. Measurements: `01-camera-probe.md`.

## 1. Requirements

| # | Requirement | Target |
|---|---|---|
| R1 | Zero decode; bytes on disk ≈ bytes from camera | container overhead < 0.1 % |
| R2 | Few, large, sequential writes; no small files; no metadata churn | 1 write per fragment per stream; ≤ 2 files per stream-hour |
| R3 | Crash and power-loss safe; recovery is a local scan | loss ≤ one fragment + sync interval; restart recovery < 5 s |
| R4 | Serve live, timeline playback, export, thumbnails without transcoding | byte-range serving; MP4 export assembled, not re-muxed |
| R5 | Retention is O(1) per deleted unit; no fragmentation growth | delete whole files; preallocated extents |
| R6 | Wall-clock-correct, drift-corrected, searchable by time | ≤ 1 s absolute error, ≤ 500 ppm drift correction |
| R7 | Optional audio (AAC) | separate elementary-stream file |
| R8 | Files readable by standard tools | `ffprobe`, `mpv`, AVPlayer open them directly |

## 2. Options considered

| Option | Overhead | Power-loss recovery | Zero-copy serving | Standard tools read it | Verdict |
|---|---|---|---|---|---|
| (a) Raw samples + external index (Moonfire, SentryShot) | 0 | needs private per-frame framing to be scannable; Moonfire discards the last minute by design | yes (moof synthesised per request in µs) | no | strong, but needs a private format for R3 and fails R8 |
| **(b) CMAF-style fMP4 fragments on disk + sidecar index** (mediamtx, crumbvms, UniFi-like) | 0.034 % measured | self-describing; each fragment validated by trailer CRC; loss = in-progress fragment | yes: segments are byte ranges that already contain their `moof` | yes | **chosen** |
| (c) Matroska | 0.027 % measured | resilient to truncation | no (no byte-range HLS, no Apple playback) | yes | rejected |
| (d) Plain MP4 per N minutes (Frigate, Viseron) | 0.033 % measured | **unreadable** until `moov` is written (measured with SIGKILL) | partial | yes | rejected |
| (e) Raw-disk block ring (Hikvision/Dahua) | 0 | custom | custom | no | rejected: preallocated files on ext4/XFS give the same sequential behaviour |

(b) differs from (a) in one real way: a `moof` precedes its `mdat`, so a fragment is buffered
in RAM until complete (≤ 2 MB for 4K at 2 s), and a crash loses the in-progress fragment.
Writes are batched per fragment anyway for HDD efficiency, so (a)'s per-frame durability is
theoretical unless one also writes per frame. (b) is readable and recoverable by anyone with
ffmpeg, which matters for a long-lived archive.

## 3. File layout

```
<video_root>/<camera_id>/<stream>/<YYYY-MM-DD>/<HHMMSS>Z.mp4    # stream ∈ main | sub | audio
<video_root>/<camera_id>/<stream>/<YYYY-MM-DD>/<HHMMSS>Z.idx    # sidecar index (cache)
<state_dir>/irisd.db                                             # SQLite catalog, on SSD/NVMe
```

One file per **elementary stream**: video main, video sub, and (if enabled) audio. Keeping
audio out of the video file keeps every video `mdat` contiguous (video-only serving is pure
byte ranges) and makes audio optional per client. HLS serves demuxed audio as an
`EXT-X-MEDIA` rendition; MP4 export and WebSocket live interleave fragments from both files.

A file (a *segment* in the catalog) is sealed and a new one started at the first IRAP after
any of: 1 hour elapsed, 1 GiB written, parameter-set (VPS/SPS/PPS) change, or a new *run*
(reconnect or timing discontinuity). Dahua cam1 at 8 Mbps rotates every ≈ 18 min by size; the
D1 sub stream hourly by time.

## 4. Byte-level layout of a video file

```
ftyp  major_brand 'iso6', minor 0, compatible 'iso6' 'cmfc' 'mp41'                 ≈ 28 B
moov                                                                                ≈ 700 B
  mvhd v1 (timescale 1000, duration 0)
  trak
    tkhd v1 (track_ID 1, duration 0, width/height from SPS)
    mdia
      mdhd v1 (timescale 90000, duration 0)
      hdlr 'vide'
      minf  vmhd · dinf/dref · stbl
        stsd  'hvc1' sample entry (width, height) + 'hvcC' (VPS/SPS/PPS arrays,
              array_completeness = 1, lengthSizeMinusOne = 3) [+ 'colr' nclx from VUI]
        stts/stsc/stsz/stco  (empty)
  mvex  trex (track 1, default_sample_flags 0x01010000)

repeat per fragment (one GOP, ≤ max_fragment_ms):
  prft  v1, flags 0x18, reference_track_ID 1, ntp_timestamp = NVR wall time for      32 B
        media_time, media_time = tfdt of this fragment
  moof                                                                      ≈ 96 + 8·N B
    mfhd  sequence_number (monotonic per file)
    traf
      tfhd  track 1, flags default-base-is-moof | default-sample-flags-present,
            default_sample_flags 0x01010000 (non-sync, depends)
      tfdt  v1, baseMediaDecodeTime (90 kHz, continuous within a run)
      trun  v0 flags data-offset | first-sample-flags | sample-duration | sample-size,
            first_sample_flags 0x02000000 when the fragment starts with an IRAP,
            N × { duration u32, size u32 }
  mdat  N access units, each NAL 4-byte-length-prefixed; VPS/SPS/PPS/AUD stripped,
        SEI kept
  free  size u32 = 24, 'free', 'IRX1' magic, crc32c u32 over [prft .. end of mdat],  24 B
        fragment_len u32 (prft start → end of this trailer), flags u32
```

- Sample entry `hvc1` with parameter sets only in `hvcC`: Apple's HLS authoring spec says
  fMP4 is a MUST for HEVC and parameter sets in the sample description (`hvc1`) is a SHOULD
  (Frigate's `force_record_hvc1` exists for the same reason). cam1's parameter sets are
  byte-identical across GOPs; the writer hashes them at every IRAP and seals the file on
  change.
- The `CODECS` string is derived from the SPS `profile_tier_level` per ISO/IEC 14496-15
  Annex E (cam1 is expected to be `hvc1.1.6.L150.B0`; verify once the writer exists).
- Version-1 (64-bit) `mvhd`/`tkhd`/`mdhd`/`tfdt` avoid the 13.26 h overflow of 32-bit
  90 kHz fields. `trun` per-sample durations are 32-bit, which is fine.
- `prft` (version 1, flags 0x18, the value ffmpeg writes for `-write_prft wallclock`) makes
  each fragment self-describing in wall time. The flag semantics in ISO/IEC 14496-12:2022
  §8.16.5 are unverified against the paywalled text (§12); matching ffmpeg keeps the files
  readable by the tools that already parse `prft`.
- The trailer is a standard, universally ignored 24-byte `free` box: size, `free`, the magic
  `IRX1`, a CRC-32C over `prft` through the end of `mdat`, `fragment_len` (from the start of
  `prft` to the end of the trailer), and reserved flags. `fragment_len` lets recovery and
  reverse scans walk backwards (the UniFi/DHAV back-pointer idea); the CRC catches torn
  writes into preallocated (zero-filled) space and bit rot.
- B-frame streams (not produced by Dahua/Hikvision IPCs) would need `trun` v1 composition
  offsets; deferred, but the layout accommodates it.

Measured cost on cam1: ≈ 700 B per 2 MB fragment (0.03 %); the audio file adds its own
`moof` per fragment (≈ 1 % of a 64 kbps AAC stream, still negligible).

## 5. Sidecar index (`.idx`)

Fixed-size records, appended per fragment, synced on the same cadence as the data. The file
is a cache: it is fully rebuildable by scanning the `.mp4` (forward via box sizes, backward
via trailers).

```
header (64 B): magic 'IRIX', version u16 = 1, record_size u16 = 40, timescale u32 (90000),
               flags u32, start_wall i64 (µs since the Unix epoch, first fragment),
               run_id u64, init_len u32 (ftyp + moov), sample_entry_hash u32 (CRC-32C of
               the sample entry), reserved to 64 bytes
record (40 B): offset u64 (prft start), decode_time u64 (tfdt), wall_us i64,
               len u32 (prft → trailer end), duration u32 (sum of sample durations),
               sample_count u16,
               flags u8 (bit0 starts-with-sync, bit1 discontinuity-before, bit2 new-parameters),
               reserved u8, raw_rtp u32 (low 32 bits of the camera RTP timestamp of the
               first sample)
```

All integers are big-endian. A trailing partial record (a crash between the data write and
the index append) is ignored on read. Per hour at 2 s fragments this is 1,800 × 40 B = 72 KB;
the whole index for 12 streams × 90 days is ≈ 1.9 GB on disk but only the open files' indexes
are held in RAM (≈ 72 KB each). Time → fragment lookup: catalog finds the segment by wall-time
range, then a binary search over the sidecar (loaded or `mmap`ed) finds the fragment; no
per-fragment rows in SQLite (47 million rows at 90 days was the alternative).

## 6. Catalog (SQLite, WAL, `synchronous=NORMAL`, on SSD)

```sql
CREATE TABLE camera   (id TEXT PRIMARY KEY, name TEXT, vendor TEXT, config TEXT);
CREATE TABLE stream   (id INTEGER PRIMARY KEY, camera_id TEXT, kind TEXT, url TEXT,
                       record INTEGER, UNIQUE(camera_id, kind));
CREATE TABLE sample_entry (id INTEGER PRIMARY KEY, hash BLOB UNIQUE, codec TEXT,
                       width INTEGER, height INTEGER, config BLOB /* hvcC or esds */,
                       codecs TEXT /* RFC 6381 */);
CREATE TABLE run      (id INTEGER PRIMARY KEY, stream_id INTEGER, started_us INTEGER,
                       wall_minus_media_us INTEGER, sr_ntp_minus_wall_us INTEGER,
                       ended_us INTEGER, end_reason TEXT);
CREATE TABLE segment  (id INTEGER PRIMARY KEY, stream_id INTEGER, run_id INTEGER,
                       sample_entry_id INTEGER, path TEXT, wall_start_us INTEGER,
                       wall_end_us INTEGER, media_start INTEGER, media_end INTEGER,
                       init_len INTEGER, durable_len INTEGER, fragments INTEGER,
                       bytes INTEGER, state TEXT /* writing | sealed | deleting */);
CREATE INDEX segment_time ON segment(stream_id, wall_start_us);
CREATE TABLE event    (id INTEGER PRIMARY KEY, camera_id TEXT, source TEXT, kind TEXT,
                       wall_start_us INTEGER, wall_end_us INTEGER, camera_time_us INTEGER,
                       data TEXT, retain_until_us INTEGER);
CREATE INDEX event_time ON event(camera_id, wall_start_us);
CREATE TABLE clip     (id INTEGER PRIMARY KEY, event_id INTEGER, path TEXT, bytes INTEGER);
CREATE TABLE setting  (key TEXT PRIMARY KEY, value TEXT);
```

Write rate: one `segment` update per stream per sync interval (`durable_len`, `fragments`,
`wall_end_us`) batched into a single transaction every 5 s, plus a few rows per hour. A
checkpoint thread keeps the WAL bounded. Alternatives (redb, fjall, sled) offer nothing at this
write rate and lose `sqlite3` inspection and ad-hoc queries.

Durability order (Moonfire's invariants): data written → `fdatasync` data and sidecar →
catalog row updated; seal: `ftruncate` → `fsync` file → `fsync` directory → `state=sealed`.
Delete: `state=deleting` → `unlink` both files → `fsync` directory → delete row.

## 7. Time model

Units: wall time in microseconds since the Unix epoch (NVR `CLOCK_REALTIME`); media time in
90 kHz ticks, continuous within a run; RTP timestamps unwrapped to i64 by retina.

1. **The NVR clock is the authority** for search, retention, and `prft`. cam1 proves why: its
   ONVIF UTC is off by one hour while its RTCP NTP time is right; other cameras do the
   opposite. Camera clocks are recorded (`run.sr_ntp_minus_wall_us`) but never trusted.
2. **Run anchoring**: `wall_minus_media = min over frames (receipt_wall − media_time)` with
   a slow upward allowance of 500 ppm (Moonfire's least-delayed-frame rule and its clamp,
   applied continuously rather than once per minute). Receipt time is retina's per-packet
   `start_ctx()` wall clock of the first packet of the access unit, the least-delayed signal
   available (a 650 KB IDR arrives over ≈ 50 ms on cam1's uplink).
3. **Frame durations come from a CFR regulariser**, not from raw RTP deltas, because cam1
   stamps frames at 10 ms resolution with 4 % duplicates and 90–100 ms bunches (01 §5):
   - nominal duration = declared rate (SDP `a=framerate`, SPS VUI) until a measured estimate
     over ≥ 16 fragments and ≥ 480 frames exists; the delivered rate replaces the declared
     one while the typical (median) fragment runs more than 1 % off it, which is what a
     uniformly slow encoder does (the probed cameras declare 30 fps and deliver 28.3 to 30
     depending on scene load), whereas bursty drops leave the median on rate;
   - camera-dropped frames leave whole-frame holes in the camera timeline (cam1 drops about
     one frame in 170, in bursts of more than one per GOP); each hole is rendered as a
     frame-long hold on the fragment's last sample, which is what the viewer saw and keeps
     the assigned timeline locked no matter the drop rate;
   - the residual (IDR timestamp steps of 10–40 ms, sub-frame jitter) is corrected by at most
     1 % of the fragment per fragment, spread as ±1-tick deltas: a slow lock that absorbs a
     30 ms step within the next two seconds without a visible rate change;
   - a declared rate is also replaced when more than 5 % of camera time would be holds (a
     camera declaring 30 fps and delivering 20); the two rules together mean a hold never
     recurs at every GOP boundary, which would stutter on playback;
   - if the camera timeline falls behind the assigned one by more than `tolerance` (default
     500 ms, a backwards step), durations snap to the camera's span and the estimate resets;
     any jump > `max_jump_secs` (10 s) ends the session and starts a new run;
   - `timestamp_mode = "passthrough"` keeps raw RTP deltas for cameras with trustworthy clocks
     or for diagnostics. The raw RTP timestamp of each fragment's first sample is kept in the
     sidecar in both modes, so the camera's timeline is never lost.
4. **Discontinuities**: reconnect; RTP jump; `|wall − (prev_wall + media_delta)| > 2 s`
   (NVR clock step, detected by watching `CLOCK_REALTIME − CLOCK_MONOTONIC`; camera reboot) →
   seal the file, new run, `EXT-X-DISCONTINUITY` in playlists. Moonfire lists NVR clock steps
   as unhandled; RTC-less SBCs make this common.
5. **Audio/video alignment**: both tracks are anchored by their own receipt-based run offsets
   (≈ 50 ms accuracy); when RTCP SRs are present for both (cam1: every 5 s, sane), the
   SR-derived RTP↔NTP pairs refine the relative offset. Media timestamps are never stretched
   (Moonfire's lesson: audio cannot be).
6. Leap seconds are assumed smeared by the NVR's NTP configuration (chrony default).

## 8. I/O policy

- **One blocking writer thread per disk** (`std::thread`), fed by a bounded channel of
  completed fragments (`Vec<Bytes>` per fragment, no copies: the same buffers feed the live
  ring and the socket read path). It writes each fragment with one `pwritev` (prft + moof
  header, the sample iovecs, trailer), appends the sidecar record, and tracks dirty files.
  Size and age rotation waits for the next fragment that starts with a sync sample; only a
  fragment that needs a fresh file without one (no file open, a new run, a parameter-set
  change) is dropped and counted, because a file cannot start mid-GOP.
  The trailer's CRC-32C runs over the whole fragment in this thread with a
  carry-less-multiply implementation (`iris_mp4::crc`); the table-driven one cost more than
  the RTSP depacketizer (12 §5).
  It records write and `fdatasync` latency histograms, queue depth and queued bytes with
  their peaks, and warns about any write or sync over 1 s. On a shared HDD a single
  `fdatasync` has been measured at 1.2 s while another writer saturated the disk; the queue
  absorbs such stalls, and the peak queued bytes field shows their size.
- **Preallocation**: `fallocate(FALLOC_FL_KEEP_SIZE)` 256 MiB ahead when < 64 MiB remain;
  `ftruncate` to the real length at seal. On ext4/XFS this creates unwritten extents whose
  conversion costs some journal I/O (a 2026 ext4 series measured ≈ 25 % write-path gain from
  deferring it) but it guarantees large contiguous extents under 12 concurrent writers.
- **Sync cadence**: `fdatasync` every open data and sidecar file every `sync_interval`
  (default 5 s), staggered across streams (each stream at its own sixteenth of the
  interval, in order of first appearance, kept across rotations); then one catalog
  transaction. Power-loss window
  ≤ one fragment (2 s) + 5 s; daemon-crash window ≤ one fragment (the page cache survives).
  `sync_file_range` is not used (it does not flush disk caches).
- **Page cache**: default is ordinary buffered writes; sealed files get
  `posix_fadvise(DONTNEED)` after their final fsync. On ≤ 2 GB hosts enable
  `uncached_writes = true` → `pwritev2(RWF_DONTCACHE)` (ext4 since 6.17, XFS since 6.15);
  recent footage is then served from the live ring or re-read from disk (a 2 MB read is ≈ 20 ms
  on an HDD). `O_DIRECT` is rejected: alignment rules, no `O_SYNC` guarantee, and it fights
  buffered reads of the same file.
- **Filesystem**: ext4 (`noatime,lazytime`, default `commit=5`) or XFS with an inherited
  `extsize=256m` on the video root. Both are adequate; decide after the soak test in 08. No
  disk spin-down: continuous recording makes it pointless and load/unload cycles are finite.
- **Backpressure**: the channel holds at most `max_queue_bytes` (default 64 MB) per stream.
  If the disk stalls, the stream task keeps reading the socket (so the camera does not drop
  the session), discards complete fragments beyond the cap, and records a gap. Reads of the
  camera socket are never blocked on disk.
- **Retention**: when free space on a video root drops below `min_free`, when the catalogued
  recordings exceed an optional size quota (`quota_gib`, for a disk shared with other data),
  or when a per-camera age limit passes, delete the oldest sealed segments across streams in
  wall order, skipping segments overlapped by events whose `retain_until_us` is in the
  future (the event's time widened by 30 s on each side, `RETAIN_MARGIN_US` in `irisd`'s
  retention; `POST /api/events/{id}/retain` sets or clears the time, and a pass moves its
  cursor past a run of protected segments rather than stalling on it). Event clips that
  must outlive continuous retention are copied once into `clip/` at event end (a bounded,
  write-once cost) rather than pinning 1 GiB files. Deletion of a 1 GiB file is a handful of
  extent frees; FIFO churn in multi-GB units keeps fragmentation bounded (verify with
  `filefrag`/`xfs_bmap` in the soak test).
- **Disk full / I/O error**: stop writing that root, keep recording to others if configured,
  raise an alert event, keep the camera sessions alive.

## 9. Recovery

At startup, for every `segment` with `state = writing`: load the sidecar; validate the last
indexed fragment (box sizes + trailer magic + CRC); scan forward from its end parsing
`prft/moof/mdat/free` headers; stop at the first invalid or zero-filled region; `ftruncate`
there; seal. Files on disk that the catalog does not know (catalog lost or restored from
backup) are adopted from their sidecar index when it describes the file exactly (records
contiguous from the init segment to the end of the file, a trailer at the end that matches
the last record), which reads kilobytes per file; a file without such an index is adopted
by the same scan: the path encodes camera/stream/start time, the `moov` carries the sample
entry, each `prft` carries wall time. Full re-index of a multi-terabyte disk reads only box
headers (≈ 1,800 fragments per file), not payload, and the daemon reports progress to
systemd and extends its start timeout while it runs.

A rotating self-check guards against silent corruption after the fact: once per cycle
(default 24 h) divided by the stream count, the daemon scans the newest sealed file of the
next stream with CRC verification and compares fragments, bytes, init length, media span, and
wall start with the catalog row, logging the result with every mismatch. `prft` round trips
are exact to the microsecond because the NTP fraction is rounded, not truncated, on the way
back.

## 10. Serving from the format

- **HLS VOD / timeline**: `EXT-X-MAP:URI="…/segment/<id>",BYTERANGE="<init_len>@0"`, media
  segments `#EXT-X-BYTERANGE:<len>@<offset>` covering 1–3 fragments (2–6 s; Apple recommends 6 s
  target duration), `EXT-X-INDEPENDENT-SEGMENTS`, `EXT-X-DISCONTINUITY` at run boundaries and
  a new `EXT-X-MAP` at sample-entry changes. Segments spanning several fragments include the
  24-byte trailers; ISO BMFF permits `free` boxes between fragments, but AVFoundation must be
  tested (open question).
- **LL-HLS live**: parts (`EXT-X-PART`, default 0.5 s) are mini-fragments built from the
  in-RAM live ring, blocking playlist reload, `EXT-X-PRELOAD-HINT`. As built (`iris-api`'s
  `llhls`): one feeder per requested stream keeps the frames of the last four GOPs; each GOP
  is a media segment numbered when its sync frame arrives, cut into parts of a fixed frame
  count (the part target over the frame duration) so a part never changes once listed;
  segments and parts are served from memory as runs of single-sample fragments (the
  WebSocket feed's encoding), the init segment from the sample entry, and a feeder idle
  for 30 s stops. Full segments are therefore served from memory too, not from the file on
  disk: about four GOPs per watched stream, nothing for streams nobody watches.
- **WebSocket live**: init segment, then one `moof+mdat` per frame (≈ 100 B overhead) from the
  live ring; the lowest-latency path for iOS and the GUI.
- **MP4 export**: `ftyp` + fresh `moov` with a full `stbl` and a version-1 `elst` for
  sub-GOP start trimming + one `mdat` whose bytes are sent as file ranges (Moonfire's
  slice technique); for muxed audio+video exports the fragments of both files are interleaved
  as `moof/mdat` pairs instead.
- **Export as served**: `iris-api`'s export streams the init segment followed by the whole
  fragments of the requested range, rewritten into a stand-alone recording: the init
  segment gains an `edts`/`elst` (version 1, one entry) whose media time skips the frames
  of the first fragment before `from` and whose duration ends the presentation at `to`;
  each fragment's `tfdt` and `prft` media time are rebased onto a continuous timeline from
  zero (gaps between runs collapse, as in the VOD playlist), `mfhd` sequence numbers run
  from one, and each trailer is recomputed over the rewritten bytes, so the export scans
  and verifies like any recording (`iris_mp4::export`). Sample bytes are copied verbatim.
  The trims are derived from the fragments' wall times, which media time tracks within
  milliseconds (§7), so they are frame-exact at 30 fps. This replaces the fresh `moov` with
  a full `stbl` above, which would only buy the ability to seek without the fragment
  index; players seek fragmented files fine.
- **Keyframe / thumbnail**: `GET …/keyframe?t=` returns init + one single-sample fragment
  (sub stream by default, ≈ 20–40 KB) for client-side hardware decode; the daemon never decodes.

## 11. Budgets (per 4K stream unless noted)

| Item | Budget |
|---|---|
| RAM | fragment under construction ≤ 2 MB + live ring (2 fragments) 4 MB; sub stream ≈ 1 MB; sidecar of open file ≈ 72 KB |
| Disk writes | 1 `pwritev` per fragment (2 s); `fdatasync` every 5 s; container overhead 0.034 % |
| Write amplification | bytes written / bytes received ≤ 1.01 (journal + sidecar + catalog) |
| Files | 2 per stream-hour (plus daily directories) |
| Crash loss | ≤ 2 s (daemon crash) · ≤ 7 s (power loss) |
| Recovery | < 5 s for 12 open files |

## 12. Open questions and tests before freezing the format

1. AVFoundation/AVPlayer with byte-range segments that span `free` trailers; if rejected,
   serve one fragment per segment or omit trailers between fragments of a segment.
2. `prft` flag semantics per ISO/IEC 14496-12:2022 (irisd writes 0x18 like ffmpeg) and
   acceptance by ffprobe/AVFoundation.
3. `RWF_DONTCACHE` exposure in `rustix` (else `libc::pwritev2`) and ext4 6.17 behaviour on
   partial-page tails.
4. ext4 vs XFS multi-week soak at 12 streams: extent counts, unlink latency, journal writes.
5. Default fragment cap: 2 s (one GOP, fewest writes) vs 0.5 s (smaller loss window, LL-HLS
   parts straight from disk). Both are supported by the format.
6. How long cam1 tolerates a stalled TCP reader (sizes `max_queue_bytes`).
