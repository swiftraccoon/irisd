# 11 — Multi-camera results (measurement record, 2026-10-31)

Six cameras, twelve H.265 streams, one daemon, 96 minutes. The question was whether more
than one camera introduces problems that one camera does not. The answer: one camera-side
defect, handled by configuration, and no recorder-side problem.

## What ran

| Item | Value |
|---|---|
| Cameras | five 4K units of one camera family (`cam1`–`cam5`) and one 4K PTZ of another family (`cam6`), all H.265 |
| Streams | main (3840×2160, 30 fps declared) and one secondary stream per camera: the 1920×1080 third stream on `cam1`–`cam5` (see §Secondary stream), the 704×480 sub stream on `cam6` |
| Events | the vendor CGI push feed on all six cameras |
| Daemon | `irisd run --config config/irisd.local.toml --stats-interval 60s`, API on loopback, Landlock sandbox enforced |
| Storage | one ext4 HDD root for video; the SQLite catalog on the same disk for this run (the design puts it on an SSD) |
| Host | an x86-64 host with other workloads running; irisd's own CPU is measured from its process counters |
| Window | started 19:36:12Z, SIGTERM at 21:12:45Z; the stats below cover the 96 one-minute samples from 19:37 to 21:12 |

Log analysis: `iris-tools daemon-stats daemon.log` (per-stream rate per
sample, stall intervals, process trend, every non-stats line deduplicated).

## Secondary stream: a camera defect and the configuration that avoids it

Earlier daemon runs showed the sub stream (`subtype=1`) of every unit in the five-unit family
stalling every 30–110 s while the main stream never did. Isolation runs with the stand-alone
probe and ffprobe, outside the recorder, pinned it down (details in `01-camera-probe.md`
§5b):

- the sub stream runs clean when it is the only session on the camera;
- it stops sending 30–110 s after any second RTSP session opens on the same camera (main +
  sub, or two sub sessions), with the TCP connection still open and no RTSP error;
- the same happens over TCP and UDP transport, on the `proto=Onvif` and native paths, and on
  two firmware builds;
- the main stream and the third stream (`subtype=2`) are unaffected: main + third ran clean
  for 130 s (3,758 third-stream frames, 5.65 Mbps);
- the PTZ unit of the other family does not show the defect: its sub stream ran the whole
  96 minutes beside its main stream at exactly 30.00 fps.

Decision: on the affected family, `sub_url` names the third stream. The cost is bitrate:
the five third streams wrote 16.1 GiB of the 43.7 GiB recorded in the first 91 minutes
(37 %); five sub streams at the PTZ's measured 1.28 Mbps would have written about 4.1 GiB.
Lowering the third stream's resolution and bitrate on the cameras (a 1280×720 tile at
2 Mbps is enough for a grid view) recovers most of that; the recorder does not care which
profile it is given.

## Process (96 samples)

| Metric | Value |
|---|---|
| CPU | 10.83–12.33 % of one core, mean 11.65 % (≈ 0.97 % per stream, ≈ 0.17 % per Mbps; one camera measured 1.5–1.7 % at ≈ 9 Mbps in `10-phase2-results.md`, so cost scales with bitrate) |
| RSS | 51.9 MiB at minute 1, 57.1 MiB at minute 30, 58.5 MiB from minute 60 through minute 95, then 67.0 MiB in the final sample at 21:12:11. During that minute three `irisd inspect` processes read 2.7 GB from the recording disk, so the likeliest cause of the 8.5 MiB step is the bounded writer queue holding fragments through the added write latency (0 drops), with the allocator keeping the pages afterwards; the `/metrics` scrapes of this record fell in the same minute. Not proven from an INFO-level log; the soak watches for a repeat |
| Threads | 3 (tokio current-thread runtime, catalog thread, one writer thread for the one root) |
| Written | 47,236 MiB in 36,237 fragments, 13,446 `fdatasync` calls (one per open file per 5 s), 0 drops, 0 write errors |
| Aggregate rate | ≈ 68.8 Mbps, 8.6 MB/s, ≈ 0.74 TB per day, ≈ 5.2 TB per seven days at these camera settings |
| Files | 52: 12 opened at start plus 40 rotations, every one at the 1 GiB cap (4K main ≈ 20–30 min, 1080p ≈ 27–37 min) except the 704×480 sub stream, which hit the 1 h cap at 578 MB |
| Catalog | 40 sealed segments added (110 rows, 42.27 GiB sealed in total with earlier runs), 4,857 events; the `/metrics` and `/api/*` endpoints answered throughout |

## Streams (median of per-minute rates; min and max in the log)

| Stream | fps (median) | Frames per fragment | Payload rate | Reconnects | Stall intervals |
|---|---|---|---|---|---|
| cam1 main / sub | 29.42 / 29.40 | 59.1 / 60.0 | 7.19 / 5.26 Mbps | 0 / 0 | 0 / 0 |
| cam2 main / sub | 29.46 / 29.43 | 52.7 / 60.0 | 6.82 / 5.65 Mbps | 0 / 0 | 0 / 0 |
| cam3 main / sub | 28.38 / 28.33 | 42.5 / 60.0 | 6.57 / 5.23 Mbps | 0 / 0 | 0 / 0 |
| cam4 main / sub | 28.39 / 28.32 | 43.6 / 59.9 | 6.54 / 5.23 Mbps | 0 / 0 | 0 / 0 |
| cam5 main / sub | 29.52 / 29.45 | 60.0 / 60.0 | 4.84 / 3.93 Mbps | 0 / 0 | 0 / 0 |
| cam6 main / sub | 30.00 / 30.00 | 60.0 / 60.0 | 10.26 / 1.28 Mbps | 0 / 0 | 0 / 0 |

A stall interval is a one-minute sample whose frame count is under half the stream's median.
There were none. All twelve sessions were playing within half a second of start.

Observations:

- `cam3` and `cam4` deliver 28.3–28.4 fps on both streams while declaring 30; the other
  three units of the family deliver 29.4–29.5 and the PTZ exactly 30.00. Both streams of a
  unit are affected equally, so the shortfall is on the camera (encoder load or exposure),
  not in the network or the recorder. The regulariser absorbed it as frame-long holds: in a
  sealed `cam3` main segment the sample durations are 3,000 ticks for the majority with
  corrections of at most 0.5 %, and the wall-clock drift over 1,298 s is −9.8 ms.
- `cam3` and `cam4` main streams average 42.5–43.6 frames per fragment against 60 everywhere
  else: their main-stream I-frame interval is set to about 45 frames on the camera. Harmless;
  container overhead rises from 0.035 % to 0.041 %.
- Eleven of the twelve streams logged exactly one packet-loss marker during their first
  minute and none in the following 95 minutes; the sealed files contain no sequence gaps and
  no decode-time gaps. The marker is a session-start artefact, not network loss.
- The six event feeds stayed connected with zero reconnects (186–2,003 events per camera).

## Sealed files checked with `irisd inspect`

| File | Span | Fragments / samples | Payload rate | Wall-clock drift | Gaps |
|---|---|---|---|---|---|
| cam3 main (28.4 fps unit) | 1,297.8 s | 897 / 36,828 | 6.62 Mbps | −9.8 ms | 0 / 0 |
| cam1 main | 1,192.8 s | 597 / 35,097 | 7.20 Mbps | +83.4 ms | 0 / 0 |
| cam6 sub (1 h rotation) | 3,600.2 s | 1,800 / 108,000 | 1.28 Mbps | −79.6 ms | 0 / 0 |

Every fragment starts at a sync sample; every sidecar index agrees with the scanned file.

## Shutdown

SIGTERM at 21:12:45.09Z; "stopped" logged at 21:12:47.89Z (2.8 s) after the catalog thread
exited. Three of the twelve files open at that moment, inspected afterwards: clean end of
file, sidecar index agrees, wall-clock drift −8.4 ms, −66.8 ms, and +4.6 ms.

## Conclusions

- Twelve streams at ≈ 69 Mbps cost ≈ 12 % of one core and ≈ 60 MiB, with no recorder-side
  stall, drop, reconnect, or write error in 96 minutes. CPU is about six times the
  `07-architecture.md` §7 budget (2 % of one core for six cameras, twelve streams), and the
  final RSS sample (67.0 MiB, 70.3 MB) exceeds its 64 MB; `12-ingest-cost.md` records the
  changes that met the CPU budget.
- The one problem that more cameras exposed was per-camera, not cross-camera: a second
  session on the same unit kills that unit's sub stream. Configuration avoids it.
- Camera-declared frame rates are not delivered rates on this family (28.3–29.5 fps against
  30); the hold-based regulariser keeps media time within 100 ms of the wall clock over
  20–60-minute files regardless.

## A seventh camera of a third family (ONVIF only)

The seventh device is a 4 MP turret from a third family. It has no vendor CGI at all (every
`cgi-bin` path answers 404), so events can only come from ONVIF pull-point, which made it the
first hardware test of that path. Its streams: main 2688×1520 H.265 at 30 fps with a 17.7 Mbps
cap (≈ 15.8 Mbps measured), a disabled 704×480 sub stream (RTSP 404), a 1080p third stream,
and a 720p fourth stream at `subtype=extra3`, which serves as its secondary stream.

Four minutes with all seven cameras (14 streams, ≈ 84 Mbps): 13.2–14.3 % of one core,
62.7 MiB RSS, every stream at its declared rate (the seventh at 30.05 fps on both streams),
no stall, drop, or reconnect. The device's RTCP sender reports carry an SSRC that does not
match its RTP stream, so retina ignores them; harmless, since the NVR clock is authoritative.

Two ONVIF findings, both fixed in the same change:

- The device reports UTC one hour ahead (the DST bug of `01-camera-probe.md` §5) yet checks
  WS-Security `Created` against its true clock, so tokens signed with the measured offset fail
  with "Wsse authorized time check failed". The daemon retries with host time; the first family
  accepts either.
- `PullMessages` with a 60 s timeout failed on every quiet pull: the `oxvif` HTTP transport
  applies a 10 s timeout to every request and has no setter. The device itself honours long
  polls (a 60 s pull returned after 24 s with an event, on a fresh connection; it closes every
  connection after its response). The pull timeout is 5 s, which the device answers in 4.1 s.

With both changes, three minutes with all seven cameras: one subscription, no resubscription,
seven events delivered, 13.6–14.5 % of one core, 63 MiB RSS, every stream clean.

## Retention quota

A storage quota (`quota_gib`) bounds the catalogued recordings on a disk shared with other
data. With 55 GB of recordings in the catalog and a 20 GiB quota, the first retention pass
deleted 91 sealed segments (36 GB) while fourteen streams kept recording, and the next pass
removed one more file as new data arrived; the root settled at the 20 GiB quota. In steady
state at ≈ 84 Mbps the quota deletes about one 1 GiB file every 1.7 minutes.

## Remaining phase-2 evidence

- One uninterrupted seven-day packaged-service window with every acceptance criterion clean.
- The ext4 versus XFS comparison (`08-roadmap.md` phase-2 exit criteria).
- AVPlayer and `mediastreamvalidator` against the daemon's playlists.

## Serving checks against the running service (2026-11-01, soak day one)

Read-only checks over the loopback API while the seven-camera service recorded, with no
camera session opened:

- `GET /api/streams/<camera>/main/export.mp4` for a two-minute range returned 109 MB in
  0.09 s (page cache); `ffprobe` read 3,553 packets over 122.3 s with no decode-time gaps
  beyond the GOP-boundary holds described in 04 §7.3 (that recording's camera delivered
  29.29 fps under a 30 fps declaration, which is what led to the regulariser's re-timing
  rule).
- `GET /api/streams/<camera>/main/hls/vod.m3u8` for a ten-minute range gave 98 media
  segments over one file with one `EXT-X-MAP`; `ffmpeg -c copy` consumed every byte range
  without an error.
- `irisd inspect` on a sealed 1 GiB file of the same stream (602 fragments, 35,656 samples,
  1217 s): 0 sequence gaps, 0 decode-time gaps, container overhead 0.036 %, wall clock drift
  against media time −11.4 ms over twenty minutes.

## Soak, first window (2026-11-01, 21.7 h on the pre-instrumentation record path)

The packaged service ran seven cameras (fourteen streams, ≈ 100 Mbit/s) from
2026-11-01T02:24Z to 2026-11-02T00:09Z under a 200 GiB quota on the ext4 HDD, with the
catalog on the same disk, before being restarted on the build that carries the timer-driven
reads, the receive low-water mark, the CRC change, the rotation fix, the regulariser's
re-timing rule, and sidecar adoption (`12-ingest-cost.md`, `04-storage-format.md §7`).
`iris-tools soak-report` over the sampler's files:

| Item | Value |
|---|---|
| Process | 14.9 % of one core mean (user 6.6, sys 8.3), max 17.7 %; RSS 86.1–91.2 MiB, slope +2.2 MiB/day; loop lag max 4 ms; watchdog never withheld; 12 self-checks passed, 0 failed |
| Writer | 554,195 fragments, 820 GiB, 846 files, 0 write errors, 3 dropped fragments (the rotation defect fixed in the same change set), 18 slow syncs over 1 s (max 3.4 s), write p99 up to 200 ms |
| Catalog | 212 segments, 193.8 GiB, 57,569 events; transaction max 1.2 s, event commit max 1.3 s, query max 0.5 s (on the HDD) |
| Streams | every stream at its declared rate (medians 29.6–30.05 fps), 0 stalls, 0 reconnects, 0 gaps; holds as `04 §7.3` describes on the units that deliver under 30 fps |
| Events | 47,882 events over seven feeds, 0 reconnects, 0 errors; the ONVIF feed renewed 162 times |
| Retention | 752 passes, 807 files deleted (729 GiB), pass max 1.2 s, the root held at 199.0–200.0 GiB |

Conclusions: the record path is stable over a day at this load with no recorder-side
stall, drop beyond the fixed rotation case, or reconnect; the HDD is the bottleneck for
fsync and for the catalog, which is why the second window runs with the catalog on an SSD
(the design's placement) and why staggered per-stream syncs remain on the list.

## Soak, second window (2026-11-02 to 2026-11-07, 134.5 h)

The packaged service recorded seven cameras and fourteen streams with the catalog on an SSD
and a 200 GiB quota on the ext4 recording root. The service instance stopped cleanly after
134.5 hours, short of the 168-hour criterion. `iris-tools soak-report` over that instance:

| Item | Value |
|---|---|
| Process | 4.86 % of one core mean (user 2.38, system 2.48), loop lag max 4 ms; RSS 57.0–211.9 MiB with the final-hour mean 139.2 MiB; 78 self-checks passed, 0 failed |
| Writer | 3,398,635 fragments, 5,225 GiB of file growth, 5,329 files, 0 dropped fragments, 0 write errors; 293 writes or syncs over 1 s, sync max 6.1 s, queue peak 104 fragments / 159.6 MiB |
| Catalog | 0 failed batches; transaction p99 max 50 ms, event commit p99 max 500 ms; final 448,403 events and 54.8 MiB database |
| Retention | 5,286 successful passes, 5,349 files and 5,225 GiB removed; pass max 989 ms; resident catalogued media stayed at 199–200 GiB |
| Host | write amplification 1.0032, container overhead 1.0004 (files over payload, the report's form then: 0.04 %), no NIC errors, NTP offset at most 0.735 ms |

One camera became unreachable for about six minutes near the end: its two streams produced seven
reconnect gaps totaling about 11.5 minutes of stream-time, its event feed recorded twelve failed
sessions, and the watchdog was withheld once. The other twelve streams and six event feeds had no
reconnect. The window therefore established multi-day writer, catalog, retention, and self-check
behavior but did not satisfy the uninterrupted seven-day exit criterion. The ten parser fuzz
targets also ran for 86,401 seconds each; no artifact appeared after the five earlier findings had
regression fixes.

## Failed diagnostic window (2026-11-07)

A restart on the consolidated hardening build initially held fourteen streams and seven feeds
with zero writer drops/errors, 2.76 % of one core mean, and 56.8–64.0 MiB RSS. After event
maintenance pruned exactly 25,000 event rows, a bounded materialized SQLite query spilled its
temporary B-tree into `/var/tmp`. The service sandbox denied that ambient path. Every retention
pass then failed, catalogued media reached 205.26 GiB against the 200 GiB quota, and later catalog
batches failed as well while basic HTTP health checks continued to pass.

That window is not soak evidence. The fix sets `temp_store=MEMORY` on the catalog connection,
which keeps the already-bounded temporary query state inside the process. The acceptance
report and sampled Prometheus counters also include catalog failures/rejections, retention
failures, reconciliation, watchdog/self-check state, stream/feed disconnections, and API
problems. A replacement window must advance beyond the 25,000-pruned-row trigger, complete
several later retention passes, restore the quota, and keep those counters at zero before its
seven-day clock begins.

## Soak, third window (2026-11-07 to 2026-11-08, 25.2 h, closed by decision)

The packaged service ran the seven cameras and fourteen streams (about 93 Mbit/s) on the
hardening build from 2026-11-07T21:46Z until 2026-11-08T23:00Z, when it was stopped cleanly to
deploy the build that carries camera provisioning, ONVIF snapshots, discovery, and stream lookup.
Verifying those features on hardware was judged worth more than the rest of the week-long window,
so soak windows wait until the change rate drops (`README.md`, decision table). `iris-tools
soak-report` and `daemon-stats` over the window:

| Item | Value |
|---|---|
| Process | 3.06 % of one core mean (user 1.55, system 1.50), max 4.55 %; loop lag max 4 ms; RSS 60.4–182.6 MiB with a 64.0 MiB first-hour and a 67.5 MiB last-hour mean (slope −1.9 MiB/day); each excursion above 100 MiB coincides with a logged slow sync on the recording HDD and the fragments the writer queue holds meanwhile (peak 78 fragments, 130.9 MiB), and ends within minutes; 14 self-checks passed, 0 failed; the watchdog was never withheld |
| Writer | 636,247 fragments, 984 GiB, 254,221 syncs, 1,011 files, 0 dropped fragments, 0 write errors; 34 writes or syncs over 1 s (sync max 4.4 s, write max 1.1 s) |
| Catalog | 0 failed batches, 0 rejected commands, no reconciliation required; transaction p99 max 20 ms (max 33.8 ms), event commit p99 max 200 ms; 213 segments and 194.6 GiB catalogued at the end, 74,239 events, 81.4 MiB database |
| Streams | medians 29.93–30.05 fps, 0 stalls, 0 reconnects, 0 gaps; holds as `04 §7.3` describes on the units that deliver under 30 fps |
| Events | seven feeds, 0 reconnects, 0 errors; the ONVIF feed renewed 189 times |
| Retention | 1,002 passes, 0 failed, 1,029 files and 984 GiB removed, pass max 1.3 s; catalogued media held at 199.0–200.0 GiB |
| Host | write amplification 1.0037, container overhead 1.0004 (files over payload, the report's form then: 0.04 %), no NIC errors, 0 RTSP retransmits, NTP offset at most 0.29 ms; 26 complete API checks with 0 problems |
| Journal | one warning, the state-directory permission repair at start |

Every acceptance counter stayed at zero for the whole window. The CPU figure is measured at the
live bitrate rather than the 56 Mbit/s the `07-architecture.md` §7 budget assumes, and the RSS
excursions are the writer queue absorbing HDD stalls, which is its purpose.

## Provisioning and ONVIF lookups against the running service (2026-11-08)

The service restarted on the provisioning build with `[provisioning] enabled` and `multicast`
added to the unit's `IPAddressAllow=`; every check below went through the API with the root
token, and the viewer's Add camera form was driven under Xvfb against the same daemon.

| Check | Result |
|---|---|
| Startup | fourteen streams and seven feeds connected within a second, nothing recovered or adopted; `capability.key` created; the security score stayed 1.3 |
| ONVIF snapshot | the CGI-less camera's `snapshot.jpg`, a 404 on the CGI path before, answered 200 `image/jpeg` (398 KB) in 1.47 s through `GetSnapshotUri` and in 0.54 s with the URL cached |
| Discovery | `GET /api/onvif/devices?wait_ms=3000` listed eight devices in 3.0 s: the seven configured cameras plus one unconfigured unit of the same platform; the HTTPS-only unit was advertised with `https` and port 443 |
| Stream lookup | the CGI-less camera returned four profiles in 0.22 s and the PTZ camera three in 0.17 s, every URL on the configured host; the HTTPS-only unit answered 502 `onvif call failed` in 0.16 s because its firmware answers ONVIF on port 443 with plaintext inside the TLS session (`01-camera-probe.md` §7) |
| Add and remove | `POST /api/cameras` with a second main-stream camera on the PTZ unit (its third session, the limit) answered 201 in 23 ms; the stream was playing after 0.2 s, its first file open after 3 s, its snapshot served, and `cameras.toml` written with mode 0600; `DELETE` answered 204, the store returned to an empty list, and the unit's sessions fell back to two |
| Refusals | `DELETE` of a configured camera 409; a URL on a port outside `connect_ports` 400 |
| Viewer | Discover listed the eight devices with scheme, host, and port and filled the name from the picked one; "Fetch streams via ONVIF" filled the main and sub URLs of the CGI-less camera from its four profiles and ticked ONVIF pull-point events |

One defect surfaced: the per-minute `stream` and `events` lines and the systemd status text came
from lists captured at start, so the added camera recorded without appearing in them (fourteen lines
for fifteen streams). The report adopts fresh lists at its next launch after a change
(`07-architecture.md` §9). On the redeployed build the added camera's line appeared in the first
report after the add and was gone from the first report after the removal, with the status text
following.

## Low-latency HLS against a replayed camera (2026-11-02)

`iris-replay` serving a sealed 4K main-stream recording at its own pace to a throwaway
daemon on the development host; `ffmpeg` and `curl` as clients:

- The first playlist request answered in 3.05 s, once the first GOP had completed, with
  one segment and five parts listed; later requests answer at once.
- `ffmpeg -i live.m3u8 -t 10 -f null -` decoded 3840×2160 HEVC with no error (it
  re-reads the init segment per media segment and says so); 180 frames in 6 s.
- A part named by `EXT-X-PRELOAD-HINT` and requested before it existed returned 202 KB
  after 84 ms, the time for its last frames to arrive.
- `_HLS_msn` two segments ahead blocked 5.5 s, then returned the playlist holding it.

Open: `mediastreamvalidator` and `AVPlayer`, which need Apple hardware
(`08-roadmap.md` phase-2 exit criteria).

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
