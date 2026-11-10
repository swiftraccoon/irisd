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
<video_root>/<camera_id>/<stream>/<YYYY-MM-DD>/<HHMMSS.ffffff>Z-<n>.mp4  # n = 1..99 when the name is taken
<state_dir>/irisd.db                                                 # SQLite catalog, on SSD/NVMe
<state_dir>/capability.key                                           # media-capability secret
<state_dir>/cameras.toml                                             # cameras added through the API
```

irisd writes `.irisd-root-id` as exactly 32 lowercase hexadecimal characters encoding 128 random
bits, without a terminator. Readers also accept those 32 characters followed by one line feed, the
form `echo` and text editors write, and reject every other framing (CR LF, a second line feed, any
further byte, leading space, uppercase) with an error that reports the length found. The catalog
binds each configured root spelling to this durable identity. A known root is available only when a
no-follow directory open reaches the same marker; a missing or changed marker is not initialized or
used as a replacement. During one daemon admission the opened device/inode pins that physical
directory, detects pathname replacement, and rejects physical aliases; device/inode is not persisted
as cross-restart identity. A configured root path must hold no symbolic link in any component;
resolution refuses links everywhere, not only at the end, so an operator configures the resolved
physical path. The marker, media files, and sidecars must be regular files. Every open of a
marker, media file, or sidecar already beneath an approved recording root is nonblocking, follows
no link, crosses no mount, and checks the opened descriptor's type before any read, so a FIFO,
socket, device, or symlink planted in a root fails that open at once instead of stalling admission,
the writer's free-space probe, recovery, adoption, removal, or the self-check; every creation
beneath an approved root is exclusive (`O_CREAT | O_EXCL`) through the same resolution. The
standalone writer of `irisd record` has no admitted root and resolves its root and layout
directories by name. Pair removal unlinks a special file or symlink in place of either member
without opening or following it. Recordings, sidecars, recovery temporaries, and the marker are
created mode 0600 and layout directories 0700; the process umask can only narrow those modes. A
missing fresh root is created through directory descriptors from the
deepest ancestor that exists without following a link, and admission continues from the created
descriptor, so a symlinked or swapped ancestor fails creation instead of placing directories
elsewhere.

For a catalog created before the root registry, every unregistered configured root is ambiguous,
including a root with no catalog row whose pathname is absent or was recreated empty. The durable
migration bit keeps that rule across retries. Migration requires an operator to verify that the
intended storage is mounted, generate 16 bytes from a cryptographically secure random source, encode
them as 32 lowercase hexadecimal characters, create `.irisd-root-id` exclusively, make it readable
by the daemon account, sync the file, and sync the root directory. Run as the daemon account (`ROOT`
is the configured root):

```sh
umask 077
set -o noclobber
od -An -N16 -tx1 /dev/urandom | tr -d ' \n' > ROOT/.irisd-root-id
sync ROOT/.irisd-root-id ROOT
```

Startup then adopts that marker into the catalog. An
unregistered root in a catalog created with the root registry (v9 or later) can be initialized
automatically only when
it has no catalog ownership and is absent or empty, using the same durable order.

One file per configured video stream: main and sub. The 0.1 alpha rejects audio recording
configuration. An audio extension could use a separate elementary-stream file so video `mdat`
ranges remain contiguous, but no audio layout or serving contract is part of this format.

A file (a *segment* in the catalog) is sealed and a new one started at the first IRAP after
any of: 1 hour elapsed, 1 GiB written, 126,976 fragments (4,096 below the 131,072-record
ceiling of every sidecar reader), parameter-set (VPS/SPS/PPS) change, or a new *run*
(reconnect or timing discontinuity). A parameter-set change is decided by comparing the sample
entry bytes; the CRC-32C is a hint and consistency check, so two entries that share a CRC still
rotate. A file that holds 131,072 fragments takes no further fragment: dependent fragments are
dropped and counted until the next IRAP opens a new file as a new run, so the writer never
produces a recording its own readers reject. Dahua cam1 at 8 Mbps rotates every ≈ 18 min by
size; on that family the secondary stream recorded is the third stream (01 §5b), about 6 Mbps,
which rotates by size roughly every 24 min, and a D1 sub stream at 1.3 Mbps rotates hourly by
time.

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
              array_completeness = 1, lengthSizeMinusOne = 3)
        stts/stsc/stsz/stco  (empty)
  mvex  trex (track 1, default_sample_flags 0x01010000)

repeat per fragment (one GOP, ≤ 1.25 × max_fragment_ms of frames):
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
  mdat  N access units, each NAL 4-byte-length-prefixed; VPS/SPS/PPS stripped (they
        live in hvcC), AUD and SEI kept
  free  size u32 = 24, 'free', 'IRX1' magic, crc32c u32 over [prft .. end of mdat],  24 B
        fragment_len u32 (prft start → end of this trailer), reserved u32 = 0
```

- Sample entry `hvc1` with parameter sets only in `hvcC`: Apple's HLS authoring spec says
  fMP4 is a MUST for HEVC and parameter sets in the sample description (`hvc1`) is a SHOULD
  (Frigate's `force_record_hvc1` exists for the same reason). cam1's parameter sets are
  byte-identical across GOPs; the writer compares each fragment's sample entry with the open
  file's and seals the file when the bytes change.
- The `CODECS` string comes from retina's `rfc6381_codec()`, which derives it from the SPS
  `profile_tier_level` per ISO/IEC 14496-15 Annex E; it is stored with each sample entry and in
  the catalog's `codecs` column, and cam1's main stream reports `hvc1.1.6.L150.00` (the
  `stream` line's `codec` field).
- Version-1 (64-bit) `mvhd`/`tkhd`/`mdhd`/`tfdt` avoid the 13.26 h overflow of 32-bit
  90 kHz fields. `trun` per-sample durations are 32-bit, which is fine.
- `prft` (version 1, flags 0x18, the value ffmpeg writes for `-write_prft wallclock`) makes
  each fragment self-describing in wall time. The flag semantics in ISO/IEC 14496-12:2022
  §8.16.5 are unverified against the paywalled text (§12); matching ffmpeg keeps the files
  readable by the tools that parse `prft`.
- The trailer is a standard, universally ignored 24-byte `free` box: size, `free`, the magic
  `IRX1`, a CRC-32C over `prft` through the end of `mdat`, `fragment_len` (from the start of
  `prft` to the end of the trailer), and four reserved bytes that are zero. They lie outside
  the CRC, so a reader rejects a trailer with any of them set: no byte of a fragment goes
  unchecked, and a flag a reader does not know is never ignored. `fragment_len` lets recovery and
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
caches what the fragment boxes say plus facts only the writer knows: `raw_rtp`, `run_id`, and
the discontinuity and new-parameter flags. A scan of the `.mp4` (forward via box sizes,
backward via trailers) rebuilds the box-derived fields; a rebuild leaves `raw_rtp` zero and
keeps the writer-only facts only from a prior sidecar whose records match (§9).

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

A zero `run_id` or `sample_entry_hash` means unknown. The sample-entry CRC-32C is a hint and a
consistency check against the catalog, not an identity: the writer compares entry bytes, the
catalog keys sample entries by CRC-32C and bytes together, and VOD playlists place
`EXT-X-DISCONTINUITY` by the catalog row, never by the CRC alone.

All integers are big-endian. Complete index records must describe contiguous byte ranges
from `init_len`, each longer than the trailer with a non-overflowing end. A malformed range
rejects the cache; offline HLS describes the media scan whatever the sidecar holds and reports a
sidecar that disagrees. A trailing partial record
(a crash between the data write and the index append) is ignored on read.
Per hour at 2 s fragments this is 1,800 × 40 B = 72 KB;
the whole index for 12 streams × 90 days is ≈ 1.9 GB on disk. Time → fragment lookup:
the catalog finds the segment by wall-time range, then a request loads a bounded sidecar snapshot
and binary-searches it; no per-fragment rows live in SQLite (47 million rows at 90 days was the
alternative).

## 6. Catalog (SQLite, WAL, `synchronous=NORMAL`, on SSD)

The connection uses `temp_store=MEMORY`: bounded sorters and materialized query pages stay
inside the process, so SQLite never needs an ambient temporary directory outside the daemon's
filesystem sandbox. Opening creates missing parent directories with mode 0700 and a missing
database file with mode 0600; SQLite gives the WAL and shared-memory files the database file's
mode, and the daemon checks and repairs all three before it opens the catalog.

The listing below gives the version-10 schema as `sqlite_master` holds it, constraints
included; the migration history (§6 text) explains the column order.

```sql
CREATE TABLE camera (id TEXT PRIMARY KEY, name TEXT NOT NULL, vendor TEXT);
CREATE TABLE sample_entry (id INTEGER PRIMARY KEY, hash INTEGER NOT NULL,
                           codecs TEXT NOT NULL, width INTEGER NOT NULL,
                           height INTEGER NOT NULL, config BLOB NOT NULL,
                           UNIQUE(hash, config));
CREATE TABLE segment (id INTEGER PRIMARY KEY, camera_id TEXT NOT NULL, kind TEXT NOT NULL,
                      path TEXT NOT NULL UNIQUE, run_seq INTEGER NOT NULL,
                      discontinuity INTEGER NOT NULL, wall_start_us INTEGER NOT NULL,
                      wall_end_us INTEGER NOT NULL, media_start INTEGER NOT NULL,
                      media_end INTEGER NOT NULL, init_len INTEGER NOT NULL,
                      durable_len INTEGER NOT NULL, fragments INTEGER NOT NULL,
                      bytes INTEGER NOT NULL, sealed INTEGER NOT NULL,
                      sample_entry_id INTEGER NOT NULL REFERENCES sample_entry(id),
                      deleting INTEGER NOT NULL DEFAULT 0, root TEXT);
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
                    kind TEXT NOT NULL, state TEXT NOT NULL,
                    nvr_time_us INTEGER NOT NULL, camera_time_us INTEGER, data TEXT,
                    retain_until_us INTEGER, kind_other TEXT,
                    storage_bytes INTEGER NOT NULL DEFAULT 0 CHECK(storage_bytes >= 0));
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
CREATE TABLE catalog_state (singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
                            event_rows INTEGER NOT NULL CHECK(event_rows >= 0),
                            event_bytes INTEGER NOT NULL CHECK(event_bytes >= 0),
                            pruned_events INTEGER NOT NULL CHECK(pruned_events >= 0));
CREATE TRIGGER event_account_insert AFTER INSERT ON event ...;  -- adds the row's storage_bytes
CREATE TRIGGER event_account_delete AFTER DELETE ON event ...;  -- subtracts them, counts a prune
CREATE TABLE recording_root (path TEXT PRIMARY KEY,
                             identity TEXT NOT NULL UNIQUE CHECK(length(identity) = 32));
CREATE TABLE recording_root_state (singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
                                   pre_identity_catalog INTEGER NOT NULL
                                       CHECK(pre_identity_catalog IN (0, 1)));
CREATE TABLE schema_version (version INTEGER NOT NULL);
```

Schema version 10 includes transactional triggers that maintain `catalog_state` on event insert
and delete. Migrations add event retention in v2, durable deletion intent in v3, a separate
`kind_other` payload in v4, root/global ordering indexes in v5, and event-capacity accounting
in v6, followed by the partial camera-horizon index in v7 and explicit tie-key plus retention
access paths in v8, then the durable recording-root identity registry in v9, then sample
entries unique by CRC-32C and bytes together in v10 (a rebuild that keeps every id, with the
foreign keys checked before it commits), so colliding CRCs keep separate rows and the unknown
placeholder never merges with a real entry. Open reads the
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
tail, and joins the thread. A drop or eviction latches an overload that refuses queries until
the outstanding work falls below half the ordinary bound; notifications are admitted
throughout. Every write runs in a transaction that begins `IMMEDIATE`: a deferred transaction
that reads first cannot wait for another connection's write lock (SQLite skips the busy
handler when a reader upgrades), so a second writer such as an operator shell, a checkpoint,
or a backup would fail the batch at once; with `IMMEDIATE` the 5 s busy timeout covers lock
acquisition. Deletion intent and recording-root registration commit under a scoped
`synchronous = FULL`. Open confirms that SQLite entered WAL mode and fails otherwise, since
the durability statements above hold only there. Path columns hold text, and a path that is
not UTF-8 is refused rather than converted lossily. Alternatives (redb, fjall, sled) lose
`sqlite3` inspection and ad-hoc queries.

Range listings return the segments of one stream overlapping `[from, to)`. A sealed row covers
its recorded interval. An unsealed row is listed only once it has a durable fragment, and it
extends to the earlier of now and the open-segment grace past its last durable progress. The
grace defaults to 30 s (`OPEN_SEGMENT_GRACE`: two default sync intervals, one default fragment
at its cap plus a quarter, and margin); the daemon passes `2 × sync_interval_ms +
1.25 × max_fragment_ms + 10 s`, never less than the default, so a healthy open file with a
long sync interval never leaves the listings between two syncs. An open file whose writer
stopped reporting, after a camera outage, an abandoned file, or a row recovery could not
reach, therefore drops out of later windows; every listed row keeps its recorded `wall_end`.

Event listing uses separate camera/global indexed statements and an exclusive
`(nvr_time_us, id)` cursor. A call materializes at most 10,000 rows and 8 MiB of aggregate
approximate owned-row bytes, including fixed row storage plus camera, kind, and payload
strings. Equal-time continuation merges same-time/id and later-time index seeks. Event protection
is applied in SQL by the retention candidate pages and the deletion mark (§8); no query projects
the active marks. Event data is capped at 64 KiB and unmapped vendor kind codes at 1 KiB, both on
UTF-8 boundaries.
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
   opposite. Camera clocks are observed (each RTCP sender report's NTP time minus its receipt
   wall time, reported as the `stream` line's `cam_offset_ms` and the camera detail's
   `camera_offset_ms`) but never stored or trusted.
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
   stamps frames at 10 ms resolution with 6 % duplicates over 15 minutes and 90–100 ms bunches
   (01 §5):
   - nominal duration = declared rate (SDP `a=framerate`, SPS VUI) until a measured estimate
     over ≥ 16 fragments and ≥ 480 frames exists; the delivered rate replaces the declared
     one while the typical fragment runs more than 1 % off it, which is what a uniformly
     slow encoder does (the probed cameras declare 30 fps and deliver 28.3 to 30 depending
     on scene load), whereas bursty drops leave the typical fragment on rate. Typical is
     the median over frames, so the few-frame remainder that a GOP longer than the cap
     leaves, whose span per frame is mostly stamp quantisation, does not count like a full
     fragment. Without a declared rate the first fragment seeds the estimate and the same
     comparison runs against the cadence (holds excluded);
   - a parameter-set epoch carrying a different valid effective frame rate retargets the
     nominal immediately and clears the estimator's history and current error. Decode time
     and cumulative hold, hole, and snap diagnostics continue. Repeated parameter bytes, an
     unchanged rate, or an absent or invalid rate preserve the estimator;
   - camera-dropped frames leave whole-frame holes in the camera timeline (cam1 drops about
     one frame in 170, in bursts of more than one per GOP); each hole is rendered as a
     frame-long hold on the fragment's last sample, which is what the viewer saw and keeps
     the assigned timeline locked no matter the drop rate;
   - a lead of the camera timeline over the assigned one of 0.6 frame or more is read as
     dropped frames and rendered as whole-frame holds (above); the residual (IDR timestamp
     steps of 10–40 ms, sub-frame jitter, backward steps) is corrected by at most 1 % of the
     fragment per fragment, spread evenly over its frames (±30 ticks each on a 60-frame
     fragment): a slow lock that absorbs a 30 ms step within two fragments without a visible
     rate change;
   - the share of camera time spent in holds does not decide the rate: a camera that stalls
     for a second now and then spends a tenth of its time in holds while every other
     fragment is on rate, and re-timing it to the mean would put those fragments off by
     that tenth and snap every few of them. A camera declaring 30 fps and delivering 20 is
     caught by its typical fragment, so a hold still never recurs at every GOP boundary,
     which would stutter on playback; a fragment whose average frame span deviates more
     than 20 % from the nominal is left out of the estimate;
   - when the error that remains after holds and the correction exceeds the tolerance
     (500 ms) in either direction, the fragment snaps: it lasts its camera span less the
     error carried into it, so the assigned timeline is back on the camera's. A camera that
     paused gets the difference as a hold on its last frame, an assigned timeline that ran
     ahead has the shorter time spread over the frames, and a backwards step keeps nominal
     durations and takes the position the camera stepped to as the reference. A fragment whose span
     is shorter than the error carried into it (a few frames closed by the cap) cannot
     absorb all of it: its frames last one tick each and the rest stays carried into the
     fragments after it. Three consecutive snaps also restart the rate estimate. The error
     therefore never exceeds the tolerance and one fragment's correction, except across a
     backwards step;
   - four consecutive fragments corrected by the full 1 % in one direction, with at least an
     eighth of the tolerance (62 ms) of error left after them, also restart the estimate,
     because the nominal itself is then off: a camera that slowed for minutes and returns to
     its declared rate would otherwise run against the two-minute average of the slower one
     until it snapped. The estimate restarts from those four fragments, so it is what the
     camera delivers at that moment, and the error that builds meanwhile stays under the
     tolerance (380 ms from 28.3 fps back to 30). The error condition is there for a camera
     whose GOPs each run a few percent faster or slower than its mean: its corrections are
     often at the limit several fragments running while the error stays small, and
     restarting on those would keep re-centring a good estimate. A restart never returns to
     the declared rate: a camera measured 5 % off it would end every GOP in a hold of
     several frames until the next warm-up. Any jump > `max_jump` (10 s) ends the session
     and starts a separate run;
   - `timestamp_mode = "passthrough"` keeps raw RTP deltas for cameras with trustworthy clocks
     or for diagnostics. The raw RTP timestamp of each fragment's first sample is kept in the
     sidecar in both modes (never in the media), so the camera's timeline survives as long
     as the sidecar does; a sidecar rebuilt from a scan carries zero (§5).
4. **Discontinuities**: reconnect; RTP jump; writer-queue rejection; or
   `|wall − (prev_wall + media_delta)| > 2 s` (NVR clock step, detected by watching
   `CLOCK_REALTIME − CLOCK_MONOTONIC`; camera reboot) → seal the file, separate run,
   `EXT-X-DISCONTINUITY` in playlists. Moonfire lists NVR clock steps as unhandled; RTC-less
   SBCs make this common.
5. **Audio extension**: audio is outside the 0.1 alpha format. An audio implementation must
   define receipt-based alignment and may use RTCP SR pairs diagnostically; it must not stretch
   audio timestamps to hide clock disagreement.
6. Leap seconds are assumed smeared by the NVR's NTP configuration (chrony default).

## 8. I/O policy

- **One blocking writer thread per configured root** (`std::thread`), fed by a bounded channel of
  completed fragments (`Vec<Bytes>` per fragment, no copies: the same buffers feed the live
  ring and the socket read path). It writes each fragment with one vectored write (prft + moof
  header, the sample iovecs, trailer; a `pwritev` takes at most 1,024 slices, so a fragment of
  more than 1,022 samples continues in further calls), appends the sidecar record, and tracks
  dirty files.
  Size, age, and fragment-count rotation waits for the next fragment that starts with a sync
  sample; only a fragment that needs a fresh file without one (no file open, a new run, a
  parameter-set change, an open file holding 131,072 fragments) is dropped and counted,
  because a file cannot start mid-GOP.
  The trailer's CRC-32C runs over the whole fragment in this thread with a
  carry-less-multiply implementation (`iris_mp4::crc`); the table-driven one cost more than
  the RTSP depacketizer (12 §5).
  It records write and `fdatasync` latency histograms, queue depth and queued bytes with
  their peaks, and warns about any write or sync over 1 s. On the soak's HDD single syncs took
  up to 6.1 s (`11-multi-camera-results.md`, second soak window); the queue absorbs such
  stalls, and the peak queued bytes field shows their size.
- **Preallocation**: `fallocate(FALLOC_FL_KEEP_SIZE)` 256 MiB ahead when < 64 MiB remain;
  `ftruncate` to the real length at seal. On ext4/XFS this creates unwritten extents whose
  conversion costs some journal I/O (a 2026 ext4 series measured ≈ 25 % write-path gain from
  deferring it) but it guarantees large contiguous extents under 12 concurrent writers.
- **Sync cadence**: `fdatasync` every open data and sidecar file every `sync_interval`
  (default 5 s), staggered across streams (each stream at its own sixteenth of the
  interval, in order of first appearance, kept across rotations); then one catalog
  transaction. A daemon crash loses the fragment under construction (at most 1.25 ×
  `max_fragment_ms` of frames, 2.5 s at the default) and any fragments still queued for the
  writer; the page cache survives. A power loss adds up to one `sync_interval` (5 s).
  `sync_file_range` is not used (it does not flush disk caches).
- **Page cache**: writes and reads use the ordinary buffered path. `O_DIRECT` is rejected:
  alignment rules, no `O_SYNC` guarantee, and it fights buffered reads of the same file.
- **Filesystem**: ext4 (`noatime,lazytime`, default `commit=5`) or XFS with an inherited
  `extsize=256m` on the video root. Both are adequate; decide after the soak test in 08. No
  disk spin-down: continuous recording makes it pointless and load/unload cycles are finite.
- **Backpressure**: each root's queue holds at most `max_queued_fragments` (default 32)
  times the configured streams plus two per camera the API may add (`provisioning.max_cameras`
  when provisioning is enabled), in complete fragments.
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
- **Retention**: when free space on a video root drops below `min_free_percent`, when the catalogued
  recordings exceed an optional size quota (`quota_gib`, for a disk shared with other data),
  or when a per-camera age limit passes, delete the oldest sealed segments across streams in
  wall order, skipping segments overlapped by events whose `retain_until_us` is later than
  the pass time (the event's time widened by 30 s on each side, `RETAIN_MARGIN` in `irisd`;
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
  second. The candidate SQL and the atomic mark are the only protection checks; the daemon keeps
  no projection of retained events. A root whose free-space probe fails is skipped for the
  pass: no row beneath it is marked, the other roots' free-space, quota, and age steps run, and
  the pass counts the root (`roots_unavailable`); rows of roots that were not admitted are left
  alone and counted in one warning. The first root rotates between passes so sustained pressure
  on one root cannot starve another. Exact-root, global-quota, and per-stream age cursors survive
  pass
  budgets. They retain only the last handled candidate when a budget interrupts a page and
  adopt the raw page end only after every returned candidate is handled; an empty filtered page
  is therefore complete. Satisfied free-space and quota policies reset their cursors, while age
  wraps only after true indexed exhaustion. Configuration reconciliation includes the
  free-space floor and bounds cursor memory to configured roots plus three stream keys per
  age-limited camera.
  Each attempt atomically rechecks current protection and commits deleting state under FULL
  synchronization. A row whose path is not in its root's four-component layout is never
  marked, because neither pair removal nor startup could finish it. `Protected` and
  `NotFound` leave the filesystem untouched; `Marked` permits idempotent removal of both pair
  members (either may already be absent), the directory sync, and row removal. Failed
  transitions remain deleting and advance neither deletion counts nor quota arithmetic. A
  deleting row still counts toward the quota total, so every pass, right after event
  maintenance, retries one page of deletion intents beneath approved roots (16 rows or
  100 ms, continuing after the previous page and wrapping after the last) before the quota
  arithmetic; a cleared fault, or a tombstone whose files are already gone, is finished
  without a restart and cannot push the quota onto newer footage. Rows on unavailable roots
  wait for startup.
- **Disk full / I/O error**: stop writing that root, keep recording to others if configured,
  log a warning naming the root and count the failure in the `writer` line's `write_errors`,
  keep the camera sessions alive.
  One bounded writer queue and worker per configured root form an ordered fallback
  chain with shared statistics. Space is checked through a freshly identity-verified root
  descriptor before every fragment and cached for one second; the identity check remains active
  when the configured floor is zero. A pressured root seals its files, and an I/O failure abandons
  them for recovery. A probe that fails because the process lacks a resource (`EMFILE`,
  `ENFILE`, `ENOMEM`, `EAGAIN`, `EINTR`) is not a root failure: the open files stay in use, the
  failure is logged, and the probe runs again after the one-second cadence. Any other probe
  failure, such as `ENOENT`, `EIO`, `ESTALE`, `ENOTDIR`, or an identity mismatch, abandons the
  root's files like an I/O error.
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

At startup, for every `segment` with `state = writing` under an available configured root: CRC-scan
every complete fragment from the initialization, parsing `prft/moof/mdat/free` headers; stop at the
first invalid or zero-filled region; `ftruncate` there even when it equals the current logical
length, releasing any `KEEP_SIZE` reservation; seal. A bounded sidecar may preserve only flags and
raw-RTP metadata for authoritative media records at matching offsets; its prefix, timing, and length
facts never authorize publication or catalog sealing. The media records neither the run id nor the
sample-entry hash, so recovery preserves both in the rebuilt header: each comes from the prior
sidecar header when that header decodes and holds a nonzero value, else from the catalog row
(`run_seq` and the row's sample-entry hash), else stays zero, which readers treat as unknown. A
recovered file therefore stays bound to its catalog row and servable. The sidecar is opened through
the pinned directory descriptor without following a link or blocking; a missing, symlinked, or
special sidecar counts as absent and is replaced by the rebuilt regular file, a sidecar that cannot
be read for lack of a process resource fails recovery so the row is retried at a later start, any
other read failure is logged and counts as absent, and a sidecar that is the media inode itself is
rejected. A file holding more than 131,072 complete fragments is refused without changing its media
or sidecar; every scan stops after that many fragments, which bounds its memory. Files on disk that
the catalog does not know (catalog lost or restored from backup) are adopted from one header scan of
the media; a sidecar bound to that scan (records contiguous from the init segment to the end of the
file, a trailer at the end that matches the last record) supplies the same facts, and a sidecar that
disagrees or cannot be opened as a regular file is reported and ignored. The path supplies the
camera and stream, each `prft` supplies wall time (zero when the file has none), and the sample
entry stays unknown, so exports compare initialization bytes (§10). A file beyond the fragment
limit is skipped with a warning, and so is a directory the walk cannot read, whose files wait for a
later start. Full re-index of a multi-terabyte disk reads only box headers (≈ 1,800 fragments per
file), not payload, and the daemon reports progress to systemd on one heartbeat whose cadence holds
across every await, extending its start timeout while it runs. Recovery first builds and syncs an
exclusive temporary index, atomically replaces the sidecar, and syncs its directory before
truncating and `fsync`ing the valid media prefix. A crash between creating that temporary
(`.<sidecar>.irisd-recover-<pid>-<n>.tmp`) and the rename leaves it in the directory; nothing
removes it. Publication failure therefore leaves media untouched, while the published cache
describes only a verified prefix and remains safe if a crash occurs before media truncation. The
catalog marks the file sealed only after the media sync succeeds. Complete fragments found in the
page cache after a process crash still need that media sync. Rows on unavailable roots or behind
filesystem lookup errors remain for a later startup. Only confirmed absence under an available root
permits deleting a missing file's row. Confirmed absence is a missing media name or layout directory
beneath the reopened and verified root, whatever the row's fragment count; a missing or replaced
root is not absence. Such a row enters durable deletion intent, any orphan sidecar is removed, the
row is deleted, and a warning counts the fragments lost. Pair removal treats a missing layout
directory as both members absent and syncs the deepest ancestor that exists. Before writing
recovery, startup finishes every durable deleting row under an available root; rows on unavailable
roots or behind a filesystem error remain for a later start, and while the daemon runs each
retention pass retries a bounded page of them (§8).

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

- **HLS VOD / timeline**: one `EXT-X-MAP:URI="/api/segments/<id>/file.mp4",BYTERANGE="<init_len>@0"`
  per file, media segments `#EXT-X-BYTERANGE:<len>@<offset>` that start at a sync fragment and
  run until the first sync fragment after their third fragment (continuation fragments always
  join the open segment: about 6 s at a 2 s GOP, one whole GOP at longer ones; Apple recommends
  a 6 s target duration), `EXT-X-INDEPENDENT-SEGMENTS`, and `EXT-X-DISCONTINUITY` at run
  boundaries, after a skipped row, and where the sample entry changes. Segments spanning several
  fragments include the
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
  API serving first opens the media file and its sidecar together beneath the approved recording
  root that owns the row, without following symlinks or crossing mounts, takes a bounded sidecar
  snapshot from that descriptor, and binds its 90 kHz timescale, initialization length, known
  run and sample-entry facts, start wall/media time, record ranges, and final complete extent to
  the catalog row and to the open file, identified by device and inode. Raw bodies read that
  descriptor; exports and keyframes reopen beneath the root and require the same inode. That
  captured complete extent is the immutable upper bound for raw full/range bodies and every VOD
  range, even if the writer appends later. A skipped middle row with an unreadable, invalid, or
  empty selected sidecar emits
  exactly one discontinuity before the next usable row; a skipped leading row emits none.
  Offline `--exclude-trailers` emits one range per fragment even for continuations, ending
  each range before its own trailer. Media playlists assert independent segments only if
  every emitted segment starts at sync; the offline master leaves this assertion to them.
- **LL-HLS live**: parts (`EXT-X-PART`, up to 0.5 s) are mini-fragments built from the
  in-RAM live ring, blocking playlist reload, `EXT-X-PRELOAD-HINT`. As built (`iris-api`'s
  `llhls`, after `docs/standards/ietf/draft-pantos-hls-rfc8216bis-22.txt`): one feeder per
  requested stream starts from the frames since the ring's last sync frame, so its first
  segment completes within one GOP, and keeps complete GOPs until the complete GOPs after
  them cover three target durations (§6.2.2), at most 12; each GOP is a media segment
  numbered when its sync frame arrives. `PART-TARGET` is pinned from the first frame's
  duration (what the whole frames that fit 0.5 s add up to, at least one frame), and parts are
  cut by media time against it: a part ends before a frame that would take it past the target
  and after a frame when another like it would. No listed part outlasts the target when the
  regulariser re-times the stream under a running feeder, a part never changes once listed,
  and with constant durations every non-final part lasts the target exactly (§4.4.4.9).
  `EXT-X-TARGETDURATION` is pinned when the first GOP completes, its length rounded up to
  whole seconds plus one second of headroom and at most the 10 s GOP budget, and never changes
  (§6.2.1); a GOP that would outlast it, or exceed 16 MiB or 600 frames, is cut where it is
  with a discontinuity, so cameras whose GOP length varies by more than a second get cut
  segments. Segments and parts are served from memory as runs of single-sample fragments (the
  WebSocket feed's encoding), the init segment from the sample entry, and a feeder idle for
  30 s stops. Full segments are therefore served from memory too, not from the file on disk:
  about five GOPs per watched stream at a 2 s GOP, nothing for streams nobody watches. A
  blocking reload waits three target durations (12 s before the first segment completes) and
  then answers 503; a directive more than two segments, or three seconds of parts, ahead is
  400 (§6.2.5.2). Non-increasing decode timestamps, ring lag, and budget breaches end the open
  segment where it is and start a discontinuity epoch; a new sample entry starts the window
  over. Dependent frames are discarded until sync, and the playlist carries
  `EXT-X-DISCONTINUITY`, `EXT-X-PROGRAM-DATE-TIME` after it, and the sliding window's
  `EXT-X-DISCONTINUITY-SEQUENCE`; its version is 6.
- **WebSocket live**: init segment, then one single-sample `prft+moof+mdat`+trailer fragment
  per frame (≈ 170 B overhead) from the live ring; a sample-entry change sends a fresh init
  segment in-band. It is the lowest-latency path for iOS and the GUI.
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
  Only the final fragment is cut: the wall anchor can step down inside a run (§7), so an
  earlier fragment's wall interval may reach past `to`, and it is exported whole because
  the fragments after it need its last samples to decode.
  A positive exclusive tail rounds up to a 90 kHz media tick. The edit duration is
  derived from the serialized media span and rounds up to a millisecond, so even a
  positive sub-tick request yields nonempty media and a positive presentation duration.
  The trims are derived from the fragments' wall times, which media time tracks within
  milliseconds (§7), so they are frame-exact at 30 fps. Keeping the fragmented structure
  avoids materializing a second full sample table; players seek fragmented files directly.
- **Keyframe / thumbnail**: `GET …/keyframe?t=` returns init + one single-sample fragment
  (sub stream by default, ≈ 20–40 KB) for client-side hardware decode; the daemon never decodes.
  The source fragment's trailer CRC and parsed timing/sample/sync facts must match its sidecar
  record. Search never admits a sync fragment before the two-minute lower bound, so a GOP longer
  than that answers 404. An unrepresentable lookback or exclusive end is rejected with HTTP 400.
  An implicit newest lookup, or an explicit time within 30 s of the present, is mutable and uses
  `private, no-store`; an explicit historical time may cache privately for an hour.

## 11. Budgets (per 4K stream unless noted)

| Item | Budget |
|---|---|
| RAM | hard references per stream: fragment under construction ≤ 64 MiB / 4,096 frames; live fast-start cache ≤ 64 MiB / 600 frames; live subscriber fan-out ≤ 64 MiB / configured frame capacity; LL-HLS feeder of a watched stream ≤ 12 complete GOPs of ≤ 16 MiB / 600 frames / the pinned target each (about five GOPs, 15 to 30 MiB, at a 2 s 4K GOP), plus one encoded GOP per bounded-class response in flight; these owners share `Bytes` when their windows overlap; sidecar of open file ≈ 72 KB |
| API disk serving | ≤ 4,096 catalog rows; ≤ 131,072 records per sidecar (about 5 MiB), a bound the writer guarantees by rotating at 126,976 fragments and never appending past 131,072; ≤ 100,000 selected records; ≤ 16 MiB rendered playlist; ≤ 4 MiB init; ≤ 66 MiB stored fragment; media responses in two process-wide classes, 8 streaming (whole or open-ended files, ranges above 1 MiB, exports) and 32 bounded (VOD playlists, keyframes, the memory-served LL-HLS resources, ranges of at most 1 MiB) by default, a full class answering 503 with `Retry-After: 1`, with admission held through detached work and final response-buffer/body drop or until a client that stops reading hits the 30 s write-stall deadline; raw body reads reserve a capacity-one slot before I/O and emitted chunks share admission; one export fragment materialized only after reserving its queue slot |
| Disk writes | 1 `pwritev` per fragment (2 s); `fdatasync` every 5 s; container overhead 0.034 % |
| Write amplification | bytes written / bytes received ≤ 1.01 (journal + sidecar + catalog) |
| Files | 2 per stream-hour (plus daily directories) |
| Crash loss | ≤ 2.5 s plus the writer queue (daemon crash) · ≤ 7.5 s (power loss) |
| Recovery | < 5 s for 12 open files |

## 12. Open questions and tests before freezing the format

1. AVFoundation/AVPlayer with byte-range segments that span `free` trailers; if rejected,
   serve one fragment per segment or omit trailers between fragments of a segment.
2. `prft` flag semantics per ISO/IEC 14496-12:2022 (irisd writes 0x18 like ffmpeg) and
   acceptance by ffprobe/AVFoundation.
3. ext4 vs XFS multi-week soak at 12 streams: extent counts, unlink latency, journal writes.
4. How long cam1 tolerates a stalled TCP reader (sizes `max_queued_fragments`).
