# 11 — Multi-stream results (measurement record)

What running many streams through one daemon showed that one stream did not. Every figure is
from the daemon's own statistics lines, `iris-tools daemon-stats`, `iris-tools soak-report`,
or `irisd inspect`; a test load is described by its stream count and bit rate, which is what
the recorder's cost depends on.

## What ran (2026-10-31)

H.265 streams from test cameras, a main and a secondary stream from each, about 69 Mbit/s
together, with the vendor event feed of each camera, one daemon with the sandbox enforced
and the API on loopback, 96 one-minute samples.

## Secondary stream: a camera defect and the configuration that avoids it

On one camera model the sub stream stalled every 30–110 s while the main stream never did.
Isolation runs with the stand-alone probe and ffprobe, outside the recorder
(`01-camera-probe.md` §5b):

- the sub stream runs clean when it is the only session on the camera;
- it stops sending 30–110 s after any second RTSP session opens on the same camera, with
  the TCP connection still open and no RTSP error;
- the same happens over TCP and UDP transport and on both of the camera's URL forms;
- the main stream and the camera's third stream are unaffected;
- a camera of another model does not show the defect.

Decision: `sub_url` may name any stream of a camera, and on a camera with this defect it
names the third stream. The recorder does not care which profile it is given; the cost is
whatever bit rate that profile carries.

## Process (96 samples)

| Metric | Value |
|---|---|
| CPU | 10.83–12.33 % of one core, mean 11.65 % (≈ 0.17 % per Mbit/s; cost scales with bit rate) |
| RSS | 51.9 MiB at minute 1, 58.5 MiB from minute 60, 67.0 MiB in the final sample, taken while three `irisd inspect` processes read from the recording disk and the bounded writer queue held fragments |
| Threads | 3 (tokio current-thread runtime, catalog thread, one writer thread for the one root) |
| Written | 47,236 MiB in 36,237 fragments, 13,446 `fdatasync` calls (one per open file per 5 s), 0 drops, 0 write errors |
| Files | one per stream opened at start, and 40 rotations at the 1 GiB or the one-hour cap |
| Catalog | 40 sealed segments added, 4,857 events; the `/metrics` and `/api/*` endpoints answered throughout |

## Streams (median of per-minute rates)

Declared 30 fps on every stream; delivered medians from 28.3 to 30.00 fps, the same on both
streams of a camera. No stall interval (a one-minute sample under half the stream's median),
no reconnect; every session was playing within half a second of start.

Observations:

- A camera that delivers under its declared rate does so on both of its streams, so the
  shortfall is on the camera, not in the network or the recorder (see "Where the missing
  frames go" below). The regulariser absorbed it as frame-long holds: in a sealed segment of
  such a stream the sample durations are 3,000 ticks for the majority with corrections of at
  most 0.5 %, and the wall-clock drift over 1,298 s is −9.8 ms.
- Some main streams average 42 to 53 frames per fragment against a GOP of 60: their GOPs are
  not all whole (see below). Container overhead rises from 0.035 % to 0.041 %.
- Eleven of the twelve streams logged exactly one packet-loss marker during their first
  minute and none in the following 95 minutes; the sealed files contain no sequence gaps and
  no decode-time gaps. The marker is a session-start artefact, not network loss.
- Every event feed stayed connected with zero reconnects.

## Sealed files checked with `irisd inspect`

| File | Span | Fragments / samples | Payload rate | Wall-clock drift | Gaps |
|---|---|---|---|---|---|
| A stream delivering 28.4 fps | 1,297.8 s | 897 / 36,828 | 6.62 Mbit/s | −9.8 ms | 0 / 0 |
| A stream delivering 29.4 fps | 1,192.8 s | 597 / 35,097 | 7.20 Mbit/s | +83.4 ms | 0 / 0 |
| A low-rate stream (one-hour rotation) | 3,600.2 s | 1,800 / 108,000 | 1.28 Mbit/s | −79.6 ms | 0 / 0 |

Every fragment starts at a sync sample; every sidecar index agrees with the scanned file.

## Shutdown

"stopped" was logged 2.8 s after SIGTERM, after the catalog thread exited. Three of the
files open at that moment, inspected afterwards: clean end of file, sidecar index
agrees, wall-clock drift −8.4 ms, −66.8 ms, and +4.6 ms.

## Conclusions

- A load of ≈ 69 Mbit/s costs ≈ 12 % of one core and ≈ 60 MiB, with no recorder-side
  stall, drop, reconnect, or write error in 96 minutes. CPU was about six times the
  `07-architecture.md` §7 budget and the final RSS sample exceeded its 64 MB;
  `12-ingest-cost.md` records the changes that met the CPU budget.
- The one problem that more cameras exposed was per-camera, not cross-camera: on one model a
  second session on a camera stops its sub stream. Configuration avoids it.
- Camera-declared frame rates are not delivered rates; the hold-based regulariser keeps media
  time within 100 ms of the wall clock over 20–60-minute files regardless.

## A camera without the vendor event feed (ONVIF only)

A test camera that answers 404 on every vendor CGI path has its events from ONVIF pull-point
alone, so it tests that path on hardware.

With its streams added (≈ 84 Mbit/s in all): 13.2–14.3 % of one core,
62.7 MiB RSS, every stream at its declared rate, no stall, drop, or reconnect. The camera's
RTCP sender reports carry an SSRC that does not match its RTP stream, so retina ignores
them; harmless, since the recorder's clock is authoritative.

Two ONVIF findings, both fixed in the same change:

- The camera reports UTC one hour off (the DST fault of `01-camera-probe.md` §5) yet
  checks WS-Security `Created` against its true clock, so tokens signed with the measured
  offset fail with "Wsse authorized time check failed". The daemon retries with its own
  time; the other test cameras accept either.
- `PullMessages` with a 60 s timeout failed on every quiet pull: the `oxvif` HTTP transport
  applies a 10 s timeout to every request and has no setter. The camera itself honours long
  polls and closes every connection after its response. The pull timeout is 5 s, which the
  camera answers in 4.1 s.

With both changes, three minutes: one subscription, no resubscription, seven events
delivered, 13.6–14.5 % of one core, 63 MiB RSS, every stream clean.

## Retention quota

A storage quota (`quota_gib`) bounds the catalogued recordings on a disk shared with other
data. With 55 GB of recordings in the catalog and a 20 GiB quota, the first retention pass
deleted 91 sealed segments (36 GB) while fourteen streams kept recording, and the next pass
removed one more file as new data arrived; the root settled at the 20 GiB quota. In steady
state at ≈ 84 Mbit/s the quota deletes about one 1 GiB file every 1.7 minutes.

## Remaining phase-2 evidence

- One uninterrupted seven-day packaged-service window with every acceptance criterion clean.
- The filesystem comparison (`08-roadmap.md` phase-2 exit criteria).
- AVPlayer and `mediastreamvalidator` against the daemon's playlists.

## Serving checks against the running service (2026-11-01)

Read-only checks over the loopback API while the service recorded, with no camera session
opened:

- `GET /api/streams/<camera>/main/export.mp4` for a two-minute range returned 109 MB in
  0.09 s (page cache); `ffprobe` read 3,553 packets over 122.3 s with no decode-time gaps
  beyond the GOP-boundary holds described in 04 §7.3 (the stream delivered 29.29 fps under
  a 30 fps declaration; `04` §7.3 gives the rule for such a stream).
- `GET /api/streams/<camera>/main/hls/vod.m3u8` for a ten-minute range gave 98 media
  segments over one file with one `EXT-X-MAP`; `ffmpeg -c copy` consumed every byte range
  without an error.
- `irisd inspect` on a sealed 1 GiB file of the same stream (602 fragments, 35,656 samples,
  1217 s): 0 sequence gaps, 0 decode-time gaps, container overhead 0.036 %, wall clock drift
  against media time −11.4 ms over twenty minutes.

## Soak, first window (2026-11-01, 21.7 h on the pre-instrumentation record path)

The packaged service recorded the test cameras' streams (≈ 100 Mbit/s) for 21.7 h under a 200 GiB
quota, with the catalog on the recording disk, before being restarted on the build that
carries the timer-driven reads, the receive low-water mark, the CRC change, the rotation
fix, the regulariser's re-timing rule, and sidecar adoption (`12-ingest-cost.md`,
`04-storage-format.md §7`). `iris-tools soak-report` over the sampler's files:

| Item | Value |
|---|---|
| Process | 14.9 % of one core mean (user 6.6, sys 8.3), max 17.7 %; RSS 86.1–91.2 MiB, slope +2.2 MiB/day; loop lag max 4 ms; watchdog never withheld; 12 self-checks passed, 0 failed |
| Writer | 554,195 fragments, 820 GiB, 846 files, 0 write errors, 3 dropped fragments (the rotation defect fixed in the same change set), 18 slow syncs over 1 s (max 3.4 s), write p99 up to 200 ms |
| Catalog | 212 segments, 193.8 GiB, 57,569 events; transaction max 1.2 s, event commit max 1.3 s, query max 0.5 s |
| Streams | medians 29.6–30.05 fps, 0 stalls, 0 reconnects, 0 gaps; holds as `04 §7.3` describes on the streams that deliver under their declared rate |
| Events | 47,882 events, 0 reconnects, 0 errors; the ONVIF feed renewed 162 times |
| Retention | 752 passes, 807 files deleted (729 GiB), pass max 1.2 s, the root held at 199.0–200.0 GiB |

Conclusions: the record path is stable over a day at this load with no recorder-side
stall, drop beyond the fixed rotation case, or reconnect; a recording disk that stalls on
sync is the bottleneck for the catalog too, which is why the design keeps the catalog off it
(`04` §6); staggered per-stream syncs are not built.

## Soak, second window (2026-11-02 to 2026-11-07, 134.5 h)

The packaged service recorded the test cameras' streams with the catalog on separate storage and a
200 GiB quota on the recording root. The service instance stopped cleanly after 134.5 hours,
short of the 168-hour criterion. `iris-tools soak-report` over that instance:

| Item | Value |
|---|---|
| Process | 4.86 % of one core mean (user 2.38, system 2.48), loop lag max 4 ms; RSS 57.0–211.9 MiB with the final-hour mean 139.2 MiB; 78 self-checks passed, 0 failed |
| Writer | 3,398,635 fragments, 5,225 GiB of file growth, 5,329 files, 0 dropped fragments, 0 write errors; 293 writes or syncs over 1 s, sync max 6.1 s, queue peak 104 fragments / 159.6 MiB |
| Catalog | 0 failed batches; transaction p99 max 50 ms, event commit p99 max 500 ms; final 448,403 events and 54.8 MiB database |
| Retention | 5,286 successful passes, 5,349 files and 5,225 GiB removed; pass max 989 ms; resident catalogued media stayed at 199–200 GiB |
| Host | write amplification 1.0032, container overhead 1.0004 (files over payload, the report's form then: 0.04 %), no NIC errors, NTP offset at most 0.735 ms |

One camera became unreachable for about six minutes near the end: its two streams produced
seven reconnect gaps totaling about 11.5 minutes of stream-time, its event feed recorded
twelve failed sessions, and the watchdog was withheld once. The other streams and feeds had
no reconnect. The window therefore established multi-day writer, catalog, retention, and
self-check behavior but did not satisfy the uninterrupted seven-day exit criterion. The ten
parser fuzz targets also ran for 86,401 seconds each, and no artifact appeared.

## Failed diagnostic window (2026-11-07)

A run of the consolidated hardening build held every stream and feed at first, with zero
writer drops/errors, 2.76 % of one core mean, and 56.8–64.0 MiB RSS. After event maintenance
pruned exactly 25,000 event rows, a bounded materialized SQLite query spilled its temporary
B-tree into `/var/tmp`. The service sandbox denied that ambient path. Every retention pass
then failed, catalogued media reached 205.26 GiB against the 200 GiB quota, and later catalog
batches failed as well while basic HTTP health checks continued to pass.

That window is not soak evidence. The fix sets `temp_store=MEMORY` on the catalog connection,
which keeps the already-bounded temporary query state inside the process. The acceptance
report and sampled Prometheus counters also include catalog failures/rejections, retention
failures, reconciliation, watchdog/self-check state, stream/feed disconnections, and API
problems. A replacement window must advance beyond the 25,000-pruned-row trigger, complete
several later retention passes, restore the quota, and keep those counters at zero before its
seven-day clock begins.

## Soak, third window (2026-11-07 to 2026-11-08, 25.2 h, closed by decision)

The packaged service recorded the test cameras' streams (about 93 Mbit/s) on the hardening
build for 25.2 h and stopped cleanly for the build that carries camera provisioning, ONVIF
snapshots, discovery, and stream lookup. Soak windows run when the change rate is low
(`README.md`, decision table). `iris-tools soak-report` and `daemon-stats`
over the window:

| Item | Value |
|---|---|
| Process | 3.06 % of one core mean (user 1.55, system 1.50), max 4.55 %; loop lag max 4 ms; RSS 60.4–182.6 MiB with a 64.0 MiB first-hour and a 67.5 MiB last-hour mean (slope −1.9 MiB/day); each excursion above 100 MiB coincides with a logged slow sync on the recording disk and the fragments the writer queue holds meanwhile (peak 78 fragments, 130.9 MiB) |
| Writer | 636,247 fragments, 984 GiB, 254,221 syncs, 1,011 files, 0 dropped fragments, 0 write errors; 34 writes or syncs over 1 s (sync max 4.4 s, write max 1.1 s) |
| Catalog | 0 failed batches, 0 rejected commands, no reconciliation required; transaction p99 max 20 ms (max 33.8 ms), event commit p99 max 200 ms; 213 segments and 194.6 GiB catalogued at the end, 74,239 events, 81.4 MiB database |
| Streams | medians 29.93–30.05 fps, 0 stalls, 0 reconnects, 0 gaps; holds as `04 §7.3` describes on the streams that deliver under their declared rate |
| Events | 0 reconnects, 0 errors; the ONVIF feed renewed 189 times |
| Retention | 1,002 passes, 0 failed, 1,029 files and 984 GiB removed, pass max 1.3 s; catalogued media held at 199.0–200.0 GiB |
| Host | write amplification 1.0037, container overhead 1.0004 (files over payload, the report's form then: 0.04 %), no NIC errors, 0 RTSP retransmits, NTP offset at most 0.29 ms; 26 complete API checks with 0 problems |
| Journal | one warning, the state-directory permission repair at start |

Every acceptance counter stayed at zero for the whole window. The CPU figure is measured at
the live bit rate rather than the 56 Mbit/s the `07-architecture.md` §7 budget assumes, and
the RSS excursions are the writer queue absorbing sync stalls, which is its purpose.

## Provisioning and ONVIF lookups against the running service (2026-11-08)

The service restarted on the provisioning build with `[provisioning] enabled` and `multicast`
added to the unit's `IPAddressAllow=`; every check below went through the API with the root
token, and the viewer's Add camera form was driven under Xvfb against the same daemon.

| Check | Result |
|---|---|
| Startup | every stream and feed connected within a second, nothing recovered or adopted; `capability.key` created; the security score stayed 1.3 |
| ONVIF snapshot | `snapshot.jpg` of a camera without the vendor CGI answered 200 `image/jpeg` in 1.47 s through `GetSnapshotUri` and in 0.54 s with the URL cached |
| Discovery | `GET /api/onvif/devices?wait_ms=3000` listed the configured cameras and the other ONVIF devices that answered in 3.0 s; a device that serves HTTPS only was advertised with `https` and port 443 |
| Stream lookup | test cameras returned their profiles in 0.17 and 0.22 s, every URL on the configured host; an HTTPS-only camera answered 502 `onvif call failed` in 0.16 s because it answers ONVIF on port 443 with plaintext inside the TLS session (`01-camera-probe.md` §7) |
| Add and remove | `POST /api/cameras` answered 201 in 23 ms; the stream was playing after 0.2 s, its first file open after 3 s, its snapshot served, and `cameras.toml` written with mode 0600; `DELETE` answered 204 and the store returned to an empty list |
| Refusals | `DELETE` of a configured camera 409; a URL on a port outside `connect_ports` 400 |
| Viewer | Discover listed the devices with scheme, host, and port and filled the name from the picked one; "Fetch streams via ONVIF" filled the main and sub URLs from a camera's profiles and ticked ONVIF pull-point events |

The per-minute `stream` and `events` lines and the systemd status text follow cameras added
and removed through the API (`07-architecture.md` §9): the added camera's line appeared in
the first report after the add and was gone from the first report after the removal, with
the status text following.

## Low-latency HLS against a replayed camera (2026-11-02)

`iris-replay` serving a sealed main-stream recording at its own pace to a throwaway daemon;
`ffmpeg` and `curl` as clients:

- The first playlist request answered in 3.05 s, once the first GOP had completed, with
  one segment and five parts listed; later requests answer at once.
- `ffmpeg -i live.m3u8 -t 10 -f null -` decoded the HEVC with no error (it re-reads the init
  segment per media segment and says so); 180 frames in 6 s.
- A part named by `EXT-X-PRELOAD-HINT` and requested before it existed returned 202 KB
  after 84 ms, the time for its last frames to arrive.
- `_HLS_msn` two segments ahead blocked 5.5 s, then returned the playlist holding it.

Open: `mediastreamvalidator` and `AVPlayer` (`08-roadmap.md` phase-2 exit criteria).

## Low-latency HLS on the service (2026-11-10)

On the build that cuts parts by media time, with the test cameras' streams, over the loopback
with `curl`:

- `live.m3u8` of every stream fetched every six seconds for the first 100 s after a start,
  the window in which a stream is re-timed if it will be: 2,121 `EXT-X-PART` lines, each
  0.500 s against `PART-TARGET=0.500`, and 1,061 segment lines, each 2.000 s against
  `EXT-X-TARGETDURATION:3`. No stream was re-timed, so the case the change fixes did not occur.
- One segment and the four parts listed for it, fetched through the capability URIs of the
  playlist and each appended to `init.mp4`: both files scan clean with `irisd inspect`
  (60 fragments, 2.000 s, 1,224,429 payload bytes, no sequence or decode-time gap), and
  `ffprobe -count_frames` reads 60 frames from each. The same segment without a capability
  answers 401.
- The polling left the daemon at 174 MiB resident (`12-ingest-cost.md` §8).

## Timestamp snaps on the service (2026-11-09 to 2026-11-10)

Over a 19.7 h run (about 2.1 million frames a stream), every stream but two never snapped,
and those two snapped twice each. The per-minute lines around the first one's snaps show
the cause: its delivered rate had been near 29 fps and returned to 30, the error rose from
147 ms to 490 ms over two minutes, and the fragment snapped. Holds ran from 0 to 0.33 % of
frames per stream.

Two changes follow from it (`04-storage-format.md` §7.3). A snap takes the error carried
into it out of its fragment; before, each such snap left up to the 500 ms tolerance in the
timeline. And four fragments corrected by the full 1 % in one direction restart the rate
estimate. In a test of ten minutes on rate, ten at a slower rate, and ten on rate again, in
2 s GOPs stamped at 10 ms resolution:

| Slow phase | Before: snaps, worst error | After: snaps, worst error |
|---|---|---|
| 28.3 fps | 5, 479 ms | 0, 384 ms |
| 28.9 fps | 2, 491 ms | 0, 215 ms |
| 29.4 fps | 0, 351 ms | 0, 78 ms |

The replay of the ten-minute camera recording (`crates/iris-ingest/tests/replay.rs`) is
unchanged: locked, without a snap.

The service ran with both rules for 9.5 h (the same streams): no stream snapped,
and the largest timeline error on any per-minute line was 116 ms (under 30 ms on the stream
that had reached 490 ms). The figures are the `snaps` and `err_ms` fields of the journal's
`stream` lines.

### A camera whose slow rate varies by GOP (2026-11-10)

One test camera's secondary stream delivered 28.6 fps under its 30 fps declaration, and
the per-minute lines showed its nominal rate switching between
30.00 and about 28.5 fps with 40 to 65 holds a minute while at 30. An export of five minutes
(`/api/streams/{camera}/sub/export.mp4`, then `irisd inspect --samples`) gave the cause: each
of 144 fragments spanned 2.07 to 2.20 s for its 60 frames, a different amount in every GOP,
so the 1 % correction was often at its limit for four fragments running, and a restart
on saturation alone discarded the estimate each time. The declared rate then applied for the
sixteen fragments of another warm-up, and every GOP ended in a hold of two to five frames.

The corrected restart needs an error of at least an eighth of the tolerance after the four
fragments, keeps those four as the estimate, and keeps the warm-up
(`04-storage-format.md` §7.3). The 144 camera spans are a test
(`crates/iris-ingest/src/regularize.rs`); replayed six times over, 864 fragments:

| Restart rule | Switches between declared and delivered rate | Fragments ending in a hold | Held frames | Worst error |
|---|---:|---:|---:|---:|
| On saturation alone | 28 | 361 | 725 | 61 ms |
| No restart on saturation | 1 | 187 | 194 | 148 ms |
| Error condition, four fragments kept | 1 | 228 | 204 | 79 ms |

The slow-and-return table above is unchanged under the last rule (0 snaps; 384, 215, and
78 ms). About one GOP in four still ends in a one-frame hold: a GOP more than 1 % slower than
the estimate leads it by 0.6 frame or more, which the regulariser renders as a dropped frame.

On the service, the build before the change ran 83 minutes and the build with it the 45
minutes after (the `holds` and `fps` fields of the journal's `stream` lines; the three
streams that were delivering under 29 fps):

| Stream | Before: holds a minute | After: holds a minute | After: minutes at the declared rate |
|---|---:|---:|---:|
| A stream | 21 | 8.6 | 0 of 45 |
| Another | 27 | 6.2 | 0 of 45 |
| Another | 18 | 6.8 | 0 of 45 |

No stream snapped in either window; the largest timeline error after the change was 112 ms.

Not adopted, and measured by a temporary change on the same spans for the decision: holding only the
part of a lead that the correction cannot absorb leaves 108 fragments ending in a hold (138 frames)
at the 1 % limit, 36 at 2 %, 18 at 3 %, and 6 at 4 %, with the worst error at 79, 76, 56, and 37 ms.
A single dropped frame then stretches its GOP by 1 % instead of holding its last frame. A limit
above 1 % shortens ordinary fragments by more than a frame, which the live stream's provisional
decode times must allow for: taken from the declared duration alone they step backwards after such a
fragment, and LL-HLS answers a decode time that does not increase with a discontinuity. At 1 % that
took a fragment of 100 frames or more. The published decode times of the corrected code increase
inside a run whatever the correction (`crates/iris-ingest/src/session.rs`).

### Where the missing frames go (2026-11-10)

Read-only analysis of a recorder's journal and of three-minute exports through the loopback
API (`export.mp4`, then `irisd inspect --samples` and `ffprobe -show_entries
packet=size,flags`); no camera was contacted.

Delivered rate by hour, from the `frames` counters of the `stream` lines over 40 hours: the
test cameras of one model deliver their declared 30.00 fps while in night mode and 28.4 to
29.9 fps while in day mode, with the change at the switch between the modes. Test cameras of
other models deliver their declared rate in both.

| | Night mode | Day mode |
|---|---|---|
| Time per 60-frame GOP | 2.000 s (33.3 ms a frame) in 86 of 90 | 2.03–2.17 s: 61 to 65 frame slots |
| Payload rate against the configured constant rate | at the cap | 63–83 % of it |
| Keyframe size | 608–804 KB mean | 254–377 KB mean |
| Frame after the keyframe | ordinary | 430–520 KB, larger than the keyframe |
| Other frames | 4 and 54 KB alternating; none under 2 KB | 18–26 % under 2 KB, bursts of 50–110 KB |
| Extra keyframes | 1 odd GOP in 3 min | up to 10 single-frame GOPs in 3 min, 84 ms each |

What this rules out:

- The network and the recorder: both streams of a camera lose the same share, RTP sequence
  numbers have no gaps, and the rate is counted against the recorder's clock.
- Timestamp error: the frame count per second is lower, not only the camera's stamps.
- Keyframes too large to encode in a frame period: the night-mode keyframes are two to three
  times larger and lose nothing.
- Motion events: a camera that reports none loses frames like the others, and the extra
  keyframes of a clip fall between events.
- Scene complexity: the day-mode picture costs fewer bits than the night-mode one.

What it leaves: the model encodes differently in day mode (a small keyframe followed by a
larger frame, many near-empty frames, extra keyframes), and each GOP then takes one to three
frame slots more than its 60 frames. The pattern of a bit budget spent in bursts with frames
skipped after them is that of a rate control that drops input frames after an overshoot;
the recordings cannot show it, because the per-frame camera timestamps that would place the
missing slots inside the GOP are not kept. The per-frame log of the next day places them
("Where in the GOP the missing slots fall" below): at the keyframe, not after a burst.
Camera makers state that per-stream frame rates
are subject to a camera's total encoding capacity, and a recorder takes more than one stream
of a camera.

The low mean frame count per fragment on some main streams (above) comes from single-frame
GOPs: the GOPs of these clips are 60 frames.

### Where in the GOP the missing slots fall (2026-11-11)

The recorder logged every frame's step in camera time for every stream
(`iris_ingest::cadence`, `crates/iris-ingest/README.md`), and fifteen minutes of each mode
were read from the journal: for every 60-frame GOP, the camera time each part of it takes
beyond 33.33 ms a frame. Main streams of the test cameras of the model that delivers under
its declared rate; test cameras of other models show nothing in either mode.

| Camera time beyond the declared rate, per GOP | Night mode | Day mode, most units | Day mode, the other units |
|---|---|---|---|
| At the keyframe (its step from the frame before) | 5–8 ms | 48–53 ms | 42–43 ms |
| Frames 2–3 | −4 to −7 ms | 4–10 ms | 4–6 ms |
| Frames 4–10 | −1 to −9 ms | −21 to −31 ms | −15 to −16 ms |
| Frames 11–60 | 1–8 ms | 0.1–0.3 ms | 56–57 ms |
| Whole GOP | 0 (60 slots) | 33–36 ms (61 slots) | 87–91 ms (62–64 slots) |
| A keyframe alone, before the GOP's | none | 0 to 13 in 15 min | over 200 in 15 min, 83–86 ms each |
| Keyframe against the frame after it | two orders larger | less than half its size | half to two thirds of its size |

The missing slot is at the keyframe. In day mode this model stamps a keyframe a frame period
and a half after the frame before it, the next frame is larger than the keyframe, frames 4
to 10 make up half a period, and frames 11 to 60 are on the declared grid: sixty frames
and one empty slot, in every GOP. Some units lose a further period and a half inside the GOP
and send a keyframe on its own, two and a half periods long, before half or more of
their GOPs.
Nothing of this is in night mode, where long steps are matched by zero steps: frames
delayed, none missing.

So the cause is not a rate control that skips frames after an overshoot anywhere in the
GOP, which the frame sizes alone suggested (above). The encoder spends two frame periods on
the start of a GOP in day mode, and the frame count per second is lower by that one frame
in sixty or more.

For the timeline (`04-storage-format.md` §7.3) this is the measurement the choice between a
hold and a stretch lacked. The camera's cadence is sixty frames at the declared rate and a
gap of one period before the keyframe. A hold of the GOP's last frame for that period is
the cadence as the camera stamps it, and shows in playback as one repeated frame. Re-timing
to the delivered rate spreads the period over the sixty frames: no repeated frame, every
frame up to one period from its camera time, and motion 1.6 % slow between keyframes.

### What decides whether a GOP loses a slot (2026-11-12)

Read-only: each test camera's encoder and image configuration tables
(`configManager.cgi?action=getConfig`; no setting was written), the `frames` and `mib`
counters of the journal's `stream` lines over three days, and the sidecar indexes and packet
sizes (`ffprobe -show_entries packet=size,flags`) of some hours' recordings.

The configuration does not explain the loss. The units of the model that loses frames run
one firmware with the same encoder settings: H.265 Main, 3840×2160, 30 fps, a GOP of 60, and
8192 kbit/s, with the smart codec off and one
temporal layer. The day and night profiles hold no encoder setting; they differ in highlight
against backlight compensation, exposure compensation, sharpness, and noise reduction.

The loss follows what the picture costs, not the mode. Every night hour runs at the
configured bit rate and delivers 30.00 fps. A day hour delivers less the further its bit
rate falls under the configured one, and a day hour whose picture costs the configured rate
delivers 30.00 fps in day mode. Both streams of a camera lose the same share in the same
hours.

| Hourly bit rate, of the rate at night | Units that lose one slot a GOP at most | Units that lose more |
|---|---|---|
| 99–100 % | 29.93–30.00 fps | 30.00 fps (night hours only) |
| 92–96 % | 29.72–29.81 fps | 28.91–28.97 fps |
| 84–90 % | 29.60–29.67 fps | 28.66–28.70 fps |
| 75–83 % | 29.50–29.60 fps | 28.35–28.67 fps |

The same holds GOP by GOP. On one unit over five hours, a GOP of 2000 to 2199 KiB (the
configured rate over its two seconds) is followed by its successor's keyframe after 60 frame
slots in 84 % of cases, one of 1800 to 1999 KiB in 57 %, and one of 1600 to 1799 KiB in 13 %;
nearly all the rest take 61. Long and exact GOPs are mixed: in one file of 551 GOPs, 276
take more than 60 slots, and the two kinds change places 182 times.

The keyframe that ends the span is of one of two kinds, and the kind decides the slot:

| Slots from one keyframe to the next | Keyframe | Frame after it | The frame after is the larger |
|---|---|---|---|
| 60 (275 spans of one file) | 955 KiB median | 4 KiB | never |
| 61 (252 spans) | 191 KiB | 541 KiB | 91 % |
| 62 or more (24 spans) | 177 KiB | 518 KiB | always |

So this model's encoder sends the keyframe of a cheap picture as two frames, a small
keyframe and a larger frame after it, and takes two frame periods over them: the slot before
such a keyframe is empty. It sends the keyframe of a costly picture whole, in one period.
Why it splits a cheap keyframe is the encoder's own and **unverified**.

The units that lose more lose a further slot or two inside the GOP whatever its keyframe (a third of
their long spans end in a whole keyframe). No image setting separates those units from the others,
and the further loss follows the scene as well. Its cause is **unverified**, and no reading of a
camera can settle it.

For the timeline this fixes what a slow stream is: not a stream at a lower rate, but GOPs
of sixty frames on the declared grid, some of which are followed by one empty slot, mixed
from one GOP to the next. A stream whose GOPs are half of each kind is 0.8 % slow, at the
threshold of a rule that re-times a stream more than 1 % slow. Such a rule, over the file
above, re-times every GOP that is followed by an empty slot and also 134 of the 275 that
are not, which the camera sent on the declared grid, and keeps every keyframe within 58 ms
of its camera time; on a unit that loses more it re-times 303 long GOPs and holds the last
frame of 241, with keyframes within 40 ms.

The slots are whole, so the timeline needs no rate for them: a GOP's span to the next
keyframe, rounded to frame periods, says how many slots are empty, and the GOP's last frame
holds for them (`04-storage-format.md` §7.3). The declared rate stays in use while a
fragment lacks no more than an eighth of its slots. The sidecars of the five hours, replayed
through that rule (`crates/iris-ingest/tests/replay.rs`, the test that reads
`IRIS_SIDECARS`), about 9,000 GOPs a unit:

| | Units that lose one slot a GOP at most | A unit of either kind by the hour | Units that lose more |
|---|---|---|---|
| GOPs that hold no slot | 56–63 % | 19 % | 2 % |
| One slot (33 ms) | 34–43 % | 40 % | 6–12 % |
| Two | 1.5–2 % | 22 % | 33–35 % |
| Three | none | 16 % | 46–47 % |
| Four or more | none | 3 % | 7.5–10 % |
| GOPs sent on the grid whose frames keep the declared period | 87–89 % | 78 % | 72–73 % |
| Rate in use | declared, throughout | declared | declared |
| Snaps | 0 | 0 | 0 |
| Greatest distance of the timeline from the camera's | under 0.1 ms | 54 ms | 23–34 ms |

The GOPs on the grid that do not keep the period to a tick carry the 1 % correction of an
error left by the GOP before them.

On the service, the first thirteen minutes under the rule (2026-11-12, day mode, the `stream`
lines of every stream): no snap, an `err_ms` of 0 on every line, and the declared rate in
use on all. The streams of the units leaving slots empty at that hour held 820 to 890 slots
in about 22,550 frames (3.6 to 3.9 %); those of the units at their configured bit rate held
none to 40. A file of each kind recorded in those minutes: 24,900 of 27,000 frames at
exactly 3000 ticks and the rest within 15 ticks on a unit at its rate, and frames of 2975
to 3015 ticks, within 1 % of the period, on a unit losing three slots a GOP.

Over 5.2 h of one run of the service that passes from day into night (2026-11-12 to
2026-11-13, 311 `stream` lines of every stream): the declared rate in use on every line, no
snap, no reconnection, an `err_ms` of at most 13, and between none and 1.9 % of a stream's
slots held. Over 12.2 h of the next run, which passes from night into day (2026-11-13, 733
lines of every stream): the declared rate on every line, no snap, no reconnection, an
`err_ms` of at most 23, and between none and 0.8 % of a stream's slots held. Over 24.0 h of
one run through both changes of mode (2026-11-14, 1,443 lines of every stream): the
declared rate on every line, no snap, an `err_ms` of at most 23, between none and 2.2 % of
a stream's slots held, and no reconnection on any stream but those of one test camera,
which refused connections through fourteen minutes of the run.

### Generated stalls (2026-11-10)

Mutation testing of the regulariser (`cargo mutants` 27.1.0 on `regularize.rs`) showed that no
test depended on the rule that replaced a declared rate once holds passed 5 % of camera time.
Measured on generated cameras that stay on rate and stall now and then, the rule did harm. The
generator is the one of the assembler's property tests (30 fps, 10 ms stamps, repeated
stamps); each row is 300 timelines of 6,000 frames pushed through the assembler by a
temporary test, with the error measured at fragment boundaries. The columns were measured in
this order as the changes were made, and all three include the carried-error change described
below the table:

| Stalls | With the 5 % rule: snaps, worst error | Without it | Without it, median over frames |
|---|---|---|---|
| up to 2 s, about every 13 s | 2,431, 503 ms | 4, 378 ms | 0, 23 ms |
| up to 1.5 s, about every 10 s | 2,088, 503 ms | 0, 284 ms | 0, 23 ms |
| up to 4 s, about every 20 s | 5,170, 503 ms | 0, 409 ms | 0, 26 ms |
| up to 1 s, about every 7 s | 1,267, 503 ms | 62, 491 ms | 23, 490 ms |

Three changes follow (`04-storage-format.md` §7.3). The 5 % rule is gone; the comparison of
the typical fragment with the declared rate, which already caught a camera delivering 20 fps
under a 30 fps declaration, also runs against the cadence when no rate is declared. The
typical fragment is the median over frames instead of over fragments. And a snap in a
fragment whose span is shorter than the error carried into it keeps the rest carried: before,
such a fragment kept its nominal durations and the error was forgotten, which let the first
row's worst error reach 5.9 s.

## Report memory (2026-11-07)

Measured with debug builds on a copy of a soak sampler directory spanning 2026-10-31 to
2026-11-07 (`journal.log` 140,890,767 bytes in 268,713 lines, `host.jsonl` 20,031,867 bytes),
as the maximum resident set from `/usr/bin/time -f %M`, before and after the reports read their
inputs line by line; the reports were byte-identical. The `--since` window covers the last
3.1 hours.

| Command | Whole files read | Line by line |
|---|---:|---:|
| `iris-tools soak-report --since 2026-11-07T17:31:15Z DIR` | 826,784 KiB | 28,300 KiB |
| `iris-tools soak-report DIR` | 923,608 KiB | 923,540 KiB |
| `iris-tools daemon-stats DIR/journal.log` | 698,972 KiB | 695,324 KiB |

The whole-soak report and `daemon-stats` keep every statistics row of the window they
summarise, and those rows, not the input text, set their peak: memory follows the window, not
the soak.

## Soak report computations (2026-11-09)

From this date `soak-report` computes write amplification from the cgroup's bytes written and
the stream payload interpolated over the same accepted sample intervals, printing the share of
the sampled span they cover, and prints container overhead as a percentage from the daemon's
own file and payload counters. The windows above were reported by the earlier computation,
which compared the bytes of the accepted intervals with the payload of the whole span. Over the
sampler directory of 2026-10-31 to 2026-11-09 (12 service instances, 197.9 h) the two give:

| Figure | Earlier computation | From 2026-11-09 |
|---|---:|---:|
| Write amplification | 0.9982 | 1.0039 (99.1 % of the span) |
| Container overhead | 1.0004 (ratio) | 0.0389 % |

The command was `iris-tools soak-report DIR` with each build on the same copy of the directory.
