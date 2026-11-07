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
| R7 | Video-only alpha record path | reject unsupported audio configuration |
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
<video_root>/.irisd-root-id                                          # 128-bit storage identity
<video_root>/<camera_id>/<stream>/<YYYY-MM-DD>/<HHMMSS.ffffff>Z.mp4  # stream ∈ main | sub
<video_root>/<camera_id>/<stream>/<YYYY-MM-DD>/<HHMMSS.ffffff>Z.idx  # sidecar index (cache)
<state_dir>/irisd.db                                                 # SQLite catalog, on SSD/NVMe
```

`.irisd-root-id` contains exactly 32 lowercase hexadecimal characters encoding 128 random
bits. The catalog binds each configured root spelling to this durable identity. A known root is
available only when a no-follow directory open reaches the same marker; a missing or changed
marker is not initialized or used as a replacement. During one daemon admission the opened
device/inode pins that physical directory, detects pathname replacement, and rejects physical
aliases; device/inode is not persisted as cross-restart identity.

For a catalog created before the root registry, every unregistered configured root is ambiguous,
including a root with no catalog row whose pathname is absent or was recreated
empty. The durable migration bit keeps that rule across retries. Migration requires an operator to verify that the
intended storage is mounted, generate 16 bytes from a cryptographically secure random source,
encode them as exactly 32 lowercase hexadecimal characters, create `.irisd-root-id` exclusively,
make it readable by the daemon account, sync the file, and sync the root directory. Startup then adopts that marker into the catalog. An
unregistered root in a catalog created directly at v9 can be initialized automatically only when
it has no catalog ownership and is absent or empty, using the same durable order.

One file per configured video stream: main and sub. The 0.1 alpha rejects audio recording
configuration. A later audio format can use a separate elementary-stream file so video `mdat`
ranges remain contiguous, but no audio layout or serving contract is part of this format.

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

File and memory fragment readers enforce optional `prft`, one `moof`, one adjacent `mdat`,
then one trailer. Native sample addressing requires `tfhd.default-base-is-moof` and an
explicit signed `trun.data_offset` resolving to the first payload byte. Every declared
optional trun field must fit its box, including skipped composition offsets.

The length-prefixed helpers preserve each six-bit NAL type code and classify all codes
below 32 as VCL, including reserved codes 24..=31. A framing error reports the start of
the offending length prefix even when fewer than four prefix bytes remain; the walk stops
after that error. These helper contracts do not add codec decoding or alter sync decisions.

Measured cost on cam1: ≈ 700 B per 2 MB fragment (0.03 %).

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

All integers are big-endian. Complete index records must describe contiguous byte ranges
from `init_len`, each longer than the trailer with a non-overflowing end. A malformed range
rejects the cache; offline HLS falls back to scanning media. A trailing partial record
(a crash between the data write and the index append) is ignored on read.
Per hour at 2 s fragments this is 1,800 × 40 B = 72 KB;
the whole index for 12 streams × 90 days is ≈ 1.9 GB on disk. Time → fragment lookup:
the catalog finds the segment by wall-time range, then a request loads a bounded sidecar snapshot
and binary-searches it; no per-fragment rows live in SQLite (47 million rows at 90 days was the
alternative).

## 6. Catalog (SQLite, WAL, `synchronous=NORMAL`, on SSD)

The connection uses `temp_store=MEMORY`: bounded sorters and materialized query pages stay
inside the process, so SQLite never needs an ambient temporary directory outside the daemon's
filesystem sandbox.

```sql
CREATE TABLE camera (id TEXT PRIMARY KEY, name TEXT NOT NULL, vendor TEXT);
CREATE TABLE sample_entry (id INTEGER PRIMARY KEY, hash INTEGER NOT NULL UNIQUE,
                           codecs TEXT NOT NULL, width INTEGER NOT NULL,
                           height INTEGER NOT NULL, config BLOB NOT NULL);
CREATE TABLE segment (id INTEGER PRIMARY KEY, camera_id TEXT NOT NULL, kind TEXT NOT NULL,
                      path TEXT NOT NULL UNIQUE, root TEXT, run_seq INTEGER NOT NULL,
                      discontinuity INTEGER NOT NULL, wall_start_us INTEGER NOT NULL,
                      wall_end_us INTEGER NOT NULL, media_start INTEGER NOT NULL,
                      media_end INTEGER NOT NULL, init_len INTEGER NOT NULL,
                      durable_len INTEGER NOT NULL, fragments INTEGER NOT NULL,
                      bytes INTEGER NOT NULL, sealed INTEGER NOT NULL,
                      deleting INTEGER NOT NULL, sample_entry_id INTEGER NOT NULL);
CREATE INDEX segment_stream_time ON segment(camera_id, kind, wall_start_us, id);
CREATE INDEX segment_camera_horizon ON segment(camera_id, wall_start_us)
    WHERE deleting = 0;
CREATE INDEX segment_retention_time ON segment(wall_start_us, id)
    WHERE sealed = 1 AND deleting = 0;
CREATE INDEX segment_retention_root_time ON segment(root, wall_start_us, id)
    WHERE sealed = 1 AND deleting = 0;
CREATE INDEX segment_retention_stream_time ON segment(camera_id, kind, wall_start_us, id)
    WHERE sealed = 1 AND deleting = 0;
CREATE TABLE event (id INTEGER PRIMARY KEY, camera_id TEXT NOT NULL, source TEXT NOT NULL,
                    kind TEXT NOT NULL, kind_other TEXT, state TEXT NOT NULL,
                    nvr_time_us INTEGER NOT NULL, camera_time_us INTEGER, data TEXT,
                    retain_until_us INTEGER, storage_bytes INTEGER NOT NULL);
CREATE INDEX event_camera_time ON event(camera_id, nvr_time_us, id);
CREATE INDEX event_time ON event(nvr_time_us, id);
CREATE INDEX event_retained_time ON event(nvr_time_us, id)
    WHERE retain_until_us IS NOT NULL;
CREATE INDEX event_inactive_time ON event(nvr_time_us, id)
    WHERE retain_until_us IS NULL;
CREATE INDEX event_retain_until ON event(retain_until_us, id)
    WHERE retain_until_us IS NOT NULL;
CREATE INDEX event_active_camera_deadline_time
    ON event(camera_id, retain_until_us, nvr_time_us, id)
    WHERE retain_until_us IS NOT NULL;
CREATE TABLE catalog_state (singleton INTEGER PRIMARY KEY, event_rows INTEGER NOT NULL,
                            event_bytes INTEGER NOT NULL, pruned_events INTEGER NOT NULL);
CREATE TABLE recording_root (path TEXT PRIMARY KEY, identity TEXT NOT NULL UNIQUE);
CREATE TABLE recording_root_state (singleton INTEGER PRIMARY KEY,
                                   pre_identity_catalog INTEGER NOT NULL);
CREATE TABLE schema_version (version INTEGER NOT NULL);
```

Schema version 9 includes transactional triggers that maintain `catalog_state` on event insert
and delete. Migrations add event retention in v2, durable deletion intent in v3, a separate
`kind_other` payload in v4, root/global ordering indexes in v5, and event-capacity accounting
in v6, followed by the partial camera-horizon index in v7 and explicit tie-key plus retention
access paths in v8, then the durable recording-root identity registry in v9. Open reads the
exact single version and validates the persisted
`segment.sealed` and `segment.deleting` domains before persistent pragmas or writes; it rejects
a higher version. Row decoding checks ids, enum spellings, booleans, integer conversions, and
aggregates; malformed persisted values fail the operation as corruption.

The catalog thread batches admitted notifications within 100 ms, at 1,000 changes, or before
the next query. Its one ordered ingress holds at most 2,048 commands and 16 MiB estimated owned
input, including commands in progress, with 128 commands and 1 MiB reserved for segment
open/seal transitions. Events reject under overload, same-path progress may coalesce only in
the unqueried tail, lifecycle eviction cannot cross a query, and queries return a bounded
overload error. Exhausting the lifecycle reserve requires startup filesystem reconciliation.
Explicit shutdown or dropping the final handle closes admission, drains only this bounded
tail, and joins the thread. Alternatives (redb, fjall, sled) lose `sqlite3` inspection and
ad-hoc queries.

Event listing uses separate camera/global indexed statements and an exclusive
`(nvr_time_us, id)` cursor. A call materializes at most 10,000 rows and 8 MiB of aggregate
approximate owned-row bytes, including fixed row storage plus camera, kind, and payload
strings. Equal-time continuation merges same-time/id and later-time index seeks. Retention
reads payload-free protection pages of at most 512 rows, ordered and continued by
`(retain_until_us, id)` with the same split seek while projecting receipt time for overlap
checks. Event
data is capped at 64 KiB and unmapped vendor kind codes at 1 KiB, both on UTF-8 boundaries.
Incremental maintenance spends at most 1,000 row-work units per transaction, carries progress
across inactive pages that delete nothing, and applies the recording-horizon, 90-day age,
two-million-row, or 512 MiB logical-string limits. Active marks remain
protected and are admitted up to 100,000 rows or 128 MiB. An event insertion performs at most
the same bounded pruning work; it is rejected or omitted if it still cannot fit, without
rolling back segment lifecycle changes in the batch. Expired marks are cleared incrementally.
String bounding releases excess backing capacity as well as truncating content.

`synchronous=NORMAL` applies to ordinary catalog work. The deletion-intent transaction
atomically checks active same-camera event protection and, only when unprotected, changes the
sealed row to deleting under `synchronous=FULL`. It returns `Marked`, `Protected`, or
`NotFound`, then restores `NORMAL`. Only `Marked` permits either member of the recording pair
to be unlinked. Its deadline-leading partial access path skips inactive and expired event
history before checking receipt-time overlap.

Durability order (Moonfire's invariants): data written → `fdatasync` data and sidecar →
catalog row updated; seal: `ftruncate` → `fsync` file → `fsync` directory → `state=sealed`.
Delete: `state=deleting` → `unlink` both files → `fsync` directory → delete row.
Before the initial catalog row, a created configured recording root, its exclusively created
identity marker, and each added layout directory are synced through their parents. The identity
is registered before reconciliation or writer startup. Each approved writer pins a freshly
reverified root descriptor before rotation, creates every layout component beneath it without
following links or crossing nested mounts, and exclusively claims both final entries through the
pinned leaf descriptor. Their initialization bytes and leaf directory are synced. Failure removes
and syncs only entries claimed by that attempt through the same descriptor. Filenames retain
numeric exclusive-create suffixes for a true equal-time collision.

## 7. Time model

Units: wall time in microseconds since the Unix epoch (NVR `CLOCK_REALTIME`); media time in
90 kHz ticks, continuous within a run; RTP timestamps unwrapped to i64 by retina.

1. **The NVR clock is the authority** for search, retention, and `prft`. cam1 proves why: its
   ONVIF UTC is off by one hour while its RTCP NTP time is right; other cameras do the
   opposite. Camera clocks are recorded (`run.sr_ntp_minus_wall_us`) but never trusted.
2. **Run anchoring**: after a fragment's frame durations are final, its anchor candidate is
   `min over frames (receipt_wall − cumulative_media_time_at_frame)`. A lower candidate is
   adopted immediately; upward movement is limited to 500 ppm of finalized media elapsed
   since the preceding fragment closed (Moonfire's least-delayed-frame rule and clamp).
   The sealed fragment wall is the resulting per-run `wall_minus_media` plus its decode
   start, so transport backlog changes the delay observation rather than the recording
   timeline. Receipt time is retina's per-packet `start_ctx()` wall clock of the first packet
   of the access unit, the least-delayed signal available (a 650 KB IDR arrives over ≈ 50 ms
   on cam1's uplink). Raw receipt observations still enforce receipt-time fragment caps.
   Monotonic packet instants are mapped through fresh recorder realtime/monotonic
   correlations. A run boundary resets the anchor. An offset step over two seconds flushes
   the pending fragment and ends the RTSP session; reconnect anchors a separate run at a sync
   frame.
3. **Frame durations come from a CFR regulariser**, not from raw RTP deltas, because cam1
   stamps frames at 10 ms resolution with 4 % duplicates and 90–100 ms bunches (01 §5):
   - nominal duration = declared rate (SDP `a=framerate`, SPS VUI) until a measured estimate
     over ≥ 16 fragments and ≥ 480 frames exists; the delivered rate replaces the declared
     one while the typical (median) fragment runs more than 1 % off it, which is what a
     uniformly slow encoder does (the probed cameras declare 30 fps and deliver 28.3 to 30
     depending on scene load), whereas bursty drops leave the median on rate;
   - a parameter-set epoch carrying a different valid effective frame rate retargets the
     nominal immediately and clears the estimator's history and current error. Decode time
     and cumulative hold, hole, and snap diagnostics continue. Repeated parameter bytes, an
     unchanged rate, or an absent or invalid rate preserve the estimator;
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
     any jump > `max_jump_secs` (10 s) ends the session and starts a separate run;
   - `timestamp_mode = "passthrough"` keeps raw RTP deltas for cameras with trustworthy clocks
     or for diagnostics. The raw RTP timestamp of each fragment's first sample is kept in the
     sidecar in both modes, so the camera's timeline is never lost.
4. **Discontinuities**: reconnect; RTP jump; writer-queue rejection; or
   `|wall − (prev_wall + media_delta)| > 2 s` (NVR clock step, detected by watching
   `CLOCK_REALTIME − CLOCK_MONOTONIC`; camera reboot) → seal the file, separate run,
   `EXT-X-DISCONTINUITY` in playlists. Moonfire lists NVR clock steps as unhandled; RTC-less
   SBCs make this common.
5. **Audio extension**: audio is outside the 0.1 alpha format. Any later implementation must
   define receipt-based alignment and may use RTCP SR pairs diagnostically; it must not stretch
   audio timestamps to hide clock disagreement.
6. Leap seconds are assumed smeared by the NVR's NTP configuration (chrony default).

## 8. I/O policy

- **One blocking writer thread per configured root** (`std::thread`), fed by a bounded channel of
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
- **Page cache**: writes and reads use the ordinary buffered path. `O_DIRECT` is rejected:
  alignment rules, no `O_SYNC` guarantee, and it fights buffered reads of the same file.
- **Filesystem**: ext4 (`noatime,lazytime`, default `commit=5`) or XFS with an inherited
  `extsize=256m` on the video root. Both are adequate; decide after the soak test in 08. No
  disk spin-down: continuous recording makes it pointless and load/unload cycles are finite.
- **Backpressure**: each root's queue holds at most `max_queued_fragments` (default 32)
  times the number of configured streams, in complete fragments.
  If the disk stalls, the stream task keeps reading the socket (so the camera does not drop
  the session), discards complete fragments beyond the cap, and records a gap. The first
  rejected fragment in a consecutive loss epoch ends recording continuity. If the assembler
  has already accepted a sync frame for the next fragment it keeps that frame; otherwise it
  discards dependent recording frames until the next sync. The next accepted fragment starts
  a run at decode time zero. Live publication is independent and continues while recording
  waits for sync. Reads of the camera socket are never blocked on disk.
  A loss inside an ordered forwarding edge follows the same rule: the edge remembers the
  affected stream, drops dependent fragments, and marks the first sync fragment it can
  successfully forward as a new run.
- **Retention**: when free space on a video root drops below `min_free`, when the catalogued
  recordings exceed an optional size quota (`quota_gib`, for a disk shared with other data),
  or when a per-camera age limit passes, delete the oldest sealed segments across streams in
  wall order, skipping segments overlapped by events whose `retain_until_us` is later than
  the pass time (the event's time widened by 30 s on each side, `RETAIN_MARGIN_US` in `irisd`;
  `POST /api/events/{id}/retain` sets or clears the time). Retention pins overlapping segment
  rows; it does not create clip copies. Deletion of a 1 GiB file is a handful of
  extent frees; FIFO churn in multi-GB units keeps fragmentation bounded (verify with
  `filefrag`/`xfs_bmap` in the soak test).
  The free-space pass selects by exact configured root in indexed SQL; the quota pass spans
  all roots. Candidate SQL materializes a bounded raw indexed page before excluding active
  protection and returns the last scanned position even when the filtered page is empty.
  Pages order and continue by `(wall_start_us, segment id)` so tied timestamps cannot hide
  later candidates. A pass stops at 512 deletion attempts, 64 candidate pages, 1,024 scanned
  raw candidate rows, or one
  second. Its payload-free protection projection is separately bounded to 2,048 rows and
  100 ms and continues by `(retain_until_us, event id)`; it is an optimization rather than the
  deletion authority. The first root rotates between passes so sustained pressure on one root
  cannot starve another. Exact-root, global-quota, and per-stream age cursors survive pass
  budgets. They retain only the last handled candidate when a budget interrupts a page and
  adopt the raw page end only after every returned candidate is handled; an empty filtered page
  is therefore complete. Satisfied free-space and quota policies reset their cursors, while age
  wraps only after true indexed exhaustion. Configuration reconciliation includes the
  free-space floor and bounds cursor memory to configured roots plus three stream keys per
  age-limited camera.
  Each attempt atomically rechecks current protection and commits deleting state under FULL
  synchronization. `Protected` and `NotFound` leave the filesystem untouched; `Marked` permits
  idempotent removal of both pair members (either may already be absent), the directory sync,
  and row removal. Failed transitions remain deleting for startup to finish and advance
  neither deletion counts nor quota arithmetic.
- **Disk full / I/O error**: stop writing that root, keep recording to others if configured,
  raise an alert event, keep the camera sessions alive.
  One bounded writer queue and worker per configured root form an ordered fallback
  chain with shared statistics. Space is checked through a freshly identity-verified root
  descriptor before every fragment and cached for one second; the identity check remains active
  when the configured floor is zero. A pressured root seals its files, and an I/O failure abandons
  them for recovery.
  Only an incomplete media write is retried on the fallback. Once the whole media
  fragment is written, a sidecar append or subsequent preallocation error acknowledges
  that fragment once and retires the root before another queued fragment is processed.
  Recovery rebuilds the sidecar from the complete media without duplicating it elsewhere.
  Fragments advance to the next root in queue order, with dependent fragments discarded
  until a sync fragment can open a file. Upstream roots stay retired after failover to
  preserve ordering. The final root retries at a sync fragment after a one-second
  cooldown, rechecks space, and marks a discontinuity so transient pressure or I/O faults
  do not stop recording permanently. Shutdown drains forwarding before stopping the
  next worker. A root that cannot be created at startup is skipped if another is usable.

## 9. Recovery

The scanner publishes a nonzero initialization length only after accepting complete `ftyp`
and `moov` boxes. Recovery rejects missing, wrong or incomplete initialization without
changing media or sidecar bytes; a complete init remains a valid prefix before a torn fragment.

At startup, for every `segment` with `state = writing` under an available configured root:
CRC-scan every complete fragment from the initialization, parsing `prft/moof/mdat/free` headers;
stop at the first invalid or zero-filled region; `ftruncate` there even when it equals the current
logical length, releasing any `KEEP_SIZE` reservation; seal. A bounded sidecar may preserve only
flags and raw-RTP metadata for authoritative media records at matching offsets; its prefix,
timing, and length facts never authorize publication or catalog sealing. Files on disk that the catalog does not know (catalog lost or restored from
backup) are adopted from their sidecar index when it describes the file exactly (records
contiguous from the init segment to the end of the file, a trailer at the end that matches
the last record), which reads kilobytes per file; a file without such an index is adopted
by the same scan: the path encodes camera/stream/start time, the `moov` carries the sample
entry, each `prft` carries wall time. Full re-index of a multi-terabyte disk reads only box
headers (≈ 1,800 fragments per file), not payload, and the daemon reports progress to
systemd and extends its start timeout while it runs.
Recovery first builds and syncs an exclusive temporary index, atomically replaces the sidecar,
and syncs its directory before truncating and syncing the valid media prefix. Publication
failure therefore leaves media untouched, while the published cache describes only a verified
prefix and remains safe if a crash occurs before media truncation. The catalog marks the file
sealed only after the media sync succeeds. Complete
fragments found in the page cache after a process crash still need that media sync.
Rows on unavailable roots or behind filesystem lookup errors remain for a later startup.
Only confirmed absence under an available root permits deleting a missing file's row.
Before writing recovery, startup finishes every durable deleting row under an available root;
rows on unavailable roots or behind a filesystem error remain for a later start.

A rotating self-check guards against silent corruption after the fact: once per cycle
(default 24 h) divided by the stream count, the daemon scans the newest sealed file of the
next stream with CRC verification and compares fragments, bytes, init length, media span, and
both wall boundaries with the catalog row, logging the result with every mismatch. A missing final
`prft` cannot verify `wall_end` and is reported as a failed check, including when the catalog value
is zero. `prft` round trips
are exact to the microsecond because the NTP fraction is rounded, not truncated, on the way
back. Era-0 NTP values before the Unix epoch clamp to Unix zero, including their fractional
seconds, when read for diagnostics or from `prft`.

## 10. Serving from the format

- **HLS VOD / timeline**: `EXT-X-MAP:URI="…/segment/<id>",BYTERANGE="<init_len>@0"`, media
  segments `#EXT-X-BYTERANGE:<len>@<offset>` covering 1–3 fragments (2–6 s; Apple recommends 6 s
  target duration), `EXT-X-INDEPENDENT-SEGMENTS`, `EXT-X-DISCONTINUITY` at run boundaries and
  a new `EXT-X-MAP` at sample-entry changes. Segments spanning several fragments include the
  24-byte trailers; ISO BMFF permits `free` boxes between fragments, but AVFoundation must be
  tested (open question).
  A range beginning inside a non-sync continuation includes all preceding fragments
  back to its sync fragment. VOD exposes the exact served order, including this preroll,
  as `#IRIS-FRAGMENT:WALL-US=<microseconds>,DURATION-TICKS=<90000-Hz ticks>` comments;
  clients accumulate ticks before converting playlist positions to wall time. Standard
  HLS players ignore these comments. Offline HLS generation preserves the source file
  when the output directory is the recording's own directory or an alias of it. Generated
  playlists and media links use exclusive sibling temporaries. A media-link temporary keeps
  its symlink inode pinned through identity verification and atomic publication, so unlinking
  and recreating its pathname cannot substitute a different entry.
  API serving first takes a bounded sidecar snapshot and binds its 90 kHz timescale,
  initialization length, known run and sample-entry facts, start wall/media time, record ranges,
  and final complete extent to the catalog row and physical file. That captured complete extent is
  the immutable upper bound for raw full/range bodies and every VOD range, even if the writer
  appends later. A skipped middle row with an unreadable, invalid, or empty selected sidecar emits
  exactly one discontinuity before the next usable row; a skipped leading row emits none.
  Offline `--exclude-trailers` emits one range per fragment even for continuations, ending
  each range before its own trailer. Media playlists assert independent segments only if
  every emitted segment starts at sync; the offline master leaves this assertion to them.
- **LL-HLS live**: parts (`EXT-X-PART`, default 0.5 s) are mini-fragments built from the
  in-RAM live ring, blocking playlist reload, `EXT-X-PRELOAD-HINT`. As built (`iris-api`'s
  `llhls`): one feeder per requested stream keeps the frames of the last four GOPs; each GOP
  is a media segment numbered when its sync frame arrives, cut into parts of a fixed frame
  count (the part target over the frame duration) so a part never changes once listed;
  segments and parts are served from memory as runs of single-sample fragments (the
  WebSocket feed's encoding), the init segment from the sample entry, and a feeder idle
  for 30 s stops. Full segments are therefore served from memory too, not from the file on
  disk: about four GOPs per watched stream, nothing for streams nobody watches.
  Non-increasing decode timestamps, codec resets, and ring lag start a discontinuity
  epoch. Dependent frames are discarded until sync, and the playlist carries both
  `EXT-X-DISCONTINUITY` and the sliding window's `EXT-X-DISCONTINUITY-SEQUENCE`.
- **WebSocket live**: init segment, then one `moof+mdat` per frame (≈ 100 B overhead) from the
  live ring; the lowest-latency path for iOS and the GUI.
- **Export as served**: `iris-api`'s export streams the init segment followed by the whole
  fragments of the requested range, rewritten into a stand-alone recording: the init
  segment gains an `edts`/`elst` (version 1, one entry) whose media time skips the frames
  of the first fragment before `from` and whose duration ends the presentation at `to`;
  each fragment's `tfdt` and `prft` media time are rebased onto a continuous timeline from
  zero (gaps between runs collapse, as in the VOD playlist), `mfhd` sequence numbers run
  from one, and each trailer is recomputed over the rewritten bytes, so the export scans
  and verifies like any recording (`iris_mp4::export`). Sample bytes are copied verbatim.
  Each original checksum is verified before rewriting, so corrupt samples are rejected
  instead of being given a fresh checksum. Actual initialization bytes are compared
  before streaming, including adopted files without known catalog hashes; differing
  configurations produce HTTP 400 before the response starts.
  Edit-list insertion accepts ordinary-size `moov` and `trak` headers and rejects
  extended-size parents with an error.
  Sync preroll is included for decoding and removed from presentation by the edit list.
  At the trailing boundary, samples starting at or after `to` are physically omitted;
  a sample crossing the boundary has its duration shortened. The final fragment's boxes
  and checksum are regenerated without changing retained sample bytes or decoding video.
  Tail exclusion therefore does not depend on a player's edit-list support.
  A positive exclusive tail rounds up to a 90 kHz media tick. The edit duration is
  derived from the serialized media span and rounds up to a millisecond, so even a
  positive sub-tick request yields nonempty media and a positive presentation duration.
  The trims are derived from the fragments' wall times, which media time tracks within
  milliseconds (§7), so they are frame-exact at 30 fps. Keeping the fragmented structure
  avoids materializing a second full sample table; players seek fragmented files directly.
- **Keyframe / thumbnail**: `GET …/keyframe?t=` returns init + one single-sample fragment
  (sub stream by default, ≈ 20–40 KB) for client-side hardware decode; the daemon never decodes.
  The source fragment's trailer CRC and parsed timing/sample/sync facts must match its sidecar
  record. Search never admits a sync fragment before the two-minute lower bound. An
  unrepresentable lookback or exclusive end is rejected with HTTP 400. An implicit newest lookup
  is mutable and uses `private, no-store`; an explicit historical time may cache privately.

## 11. Budgets (per 4K stream unless noted)

| Item | Budget |
|---|---|
| RAM | hard references per stream: fragment under construction ≤ 64 MiB / 4,096 frames; live fast-start cache ≤ 64 MiB / 600 frames; live subscriber fan-out ≤ 64 MiB / configured frame capacity; these owners share `Bytes` when their windows overlap; sidecar of open file ≈ 72 KB |
| API disk serving | ≤ 4,096 catalog rows; ≤ 131,072 records per sidecar (about 5 MiB); ≤ 100,000 selected records; ≤ 16 MiB rendered playlist; ≤ 4 MiB init; ≤ 66 MiB stored fragment; eight concurrent disk-backed responses process-wide, with admission held through detached work and final response-buffer/body drop; raw body reads reserve a capacity-one slot before I/O and emitted chunks share admission; one export fragment materialized only after reserving its queue slot |
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
3. ext4 vs XFS multi-week soak at 12 streams: extent counts, unlink latency, journal writes.
4. How long cam1 tolerates a stalled TCP reader (sizes `max_queue_bytes`).
