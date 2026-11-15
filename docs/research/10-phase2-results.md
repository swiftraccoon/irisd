# 10 — Phase 2 results (measurement record, 2026-10-31)

Dated record of the first integration of the phase-2 daemon against one test camera. Build:
`cargo build --release -p irisd`
from the working tree committed as the phase-2 change (12.2 MB binary). Configuration: one
camera, main + sub streams, the vendor CGI event feed, API on loopback with one bearer token.

## What ran

`irisd run --config <local toml>` for about five minutes, then `kill -9`, then a restart on
a rebuilt binary for another five minutes. Startup logged Landlock "fully enforced" on this
kernel, the catalog open, both streams recording within 2 s, the event feed connected within
60 ms, and the API listening.

## Endpoint checks (all against the live daemon)

| Endpoint | Result |
|---|---|
| `GET /api/health` | `{"status":"ok","uptime_s":…,"version":"0.1.0"}` without a token |
| any `/api/*` without or with a wrong token | 401 |
| `GET /api/cameras` | the configured camera with `has_snapshot: true` |
| `GET /api/streams/<camera>/main/segments` | rows with wall range, fragment count, bytes, sealed flag, codecs, dimensions; no file paths |
| `GET /api/streams/<camera>/main/hls/vod.m3u8` | byte-range playlist, `EXT-X-MAP` per segment; ffprobe over HTTP reads `hvc1` 3840×2160, ffmpeg decodes 180 frames in 6 s through its HLS demuxer (after the segment URL gained a `.mp4` extension, which ffmpeg's `allowed_segment_extensions` requires) |
| `GET /api/streams/<camera>/sub/export.mp4` | 4.0 MB streamed fMP4, 1,067 frames, 36.8 s, `hvc1` |
| `GET /api/segments/{id}/file.mp4` with `Range: bytes=0-767` | 206, 768 bytes, `video/mp4` |
| `GET /api/cameras/<camera>/snapshot.jpg` | 200, `image/jpeg`, 1.36 MB (4K still proxied from the camera with digest auth) |
| WebSocket `/api/streams/<camera>/sub/live` | one `0x01` init message, then `0x02` per-frame fragments at the stream rate (204 in 6 s) |
| `GET /api/events` | 32 events after ten minutes: `time_change` pulses every minute, the camera's `NewFile` and `VideoMotionInfo` codes kept as `other`, three `disconnect` start/stop pairs, and one real `motion` start/stop pair |
| `GET /metrics` | Prometheus text with request, client, catalog, and uptime series |

## Crash and restart

| Step | Observed |
|---|---|
| `kill -9` with two files open (main 147 fragments, sub 12) | — |
| restart | `recovered open segment … truncated=false` for both within 0.3 s; `catalog reconciled recovered=2 adopted=0`; recording resumed in new files 3 s after start |
| `GET …/segments` after restart | the crashed file listed as sealed with its full fragment count; the file opened after it unsealed and growing |

The test machine stopping under the service, with what the kernel had not written lost
(2026-11-13, a segment of every recorded stream open, 46 to 737 fragments each): at the
next start every segment is recovered, four in five whole and the rest with a torn tail cut
off (`truncated=true`). The sidecars of the torn ones, whose last records point past the
data, are rebuilt from the scan (`ignoring unreadable prior sidecar records`). The
recovered ends lie within 8.1 s of each other. A recovery takes 0.4 to 11 s, all of them
together 60 s, and the first stream connects 67 s after the start. A watchdog period counted
from before the recovery would have its first check as the streams are spawned and withhold
a ping; the period therefore runs from the start of recording
(`crates/irisd/README.md`). How much of each stream's tail was lost is not measured: the
journal lost its own tail with it.

## Resource use (daemon with API, catalog, and event feed, one camera)

| Metric | Value |
|---|---|
| CPU | 1.47–1.70 % of one core (30 s samples) |
| RSS | 16.1–16.2 MiB (phase-1 recorder alone: 11–12 MiB) |
| Catalog | 9 segment rows and 32 events after ten minutes; batched notifications, no measurable cost |

## Camera-side observations

- The camera's sub stream stalled every 30–60 s for the whole session ("no frames for
  10 s"), while the main stream never did; the camera's own `RtspSessionDisconnect` events
  name the sub-stream sessions, so the stall is camera-side. Each stall cost one reconnect
  and a new run file; nothing was dropped. The root cause (a second session on the same
  camera stops its sub stream) and the decision to record the third stream instead are in
  `01-camera-probe.md` §5b and `11-multi-camera-results.md`.
- The Dahua `TimeChange` payload renders camera-local time in 12-hour form without a
  meridiem marker (`02:46:06` for 14:46); only `nvr_time` is used for placement.

## Supply chain

`cargo deny check`: advisories, bans, licenses, and sources all pass after allowing
`CDLA-Permissive-2.0` for the webpki root-certificate bundle pulled in by rustls.
`cargo audit`: no advisories against the 319 crates in the lock file.

## Fuzzing (2026-11-01 to 2026-11-02)

Ten targets each ran for 24 h of libFuzzer time (`cargo fuzz run <target> --
-max_total_time=86400` on the pinned nightly, niced beside the soak) without a finding. The
five crashes the targets had found in their first minutes on 2026-11-01 (two in the ONVIF
timestamp parser, two in the recording scanner, one in the replay server's RTSP parser) were
fixed before the run, and their artifacts did not reproduce on 2026-11-02.

| Target | Runs in 24 h |
|---|---|
| `access_unit` | 4.95 × 10⁹ |
| `box_header` | 1.80 × 10¹⁰ |
| `dahua_part` | 2.38 × 10⁹ |
| `digest_challenge` | 1.46 × 10⁹ |
| `index` | 5.22 × 10⁹ |
| `multipart` | 4.62 × 10⁸ |
| `onvif_utc` | 6.14 × 10⁹ |
| `rtsp_request` | 1.96 × 10⁹ |
| `scan` | 7.29 × 10⁹ |
| `trailer` | 1.93 × 10¹⁰ |

## Fuzzing (2026-11-08 to 2026-11-09)

Six targets added after the first campaign ran the same command for 24 h each from
2026-11-08T17:18Z, without a finding. They were built from the tree of that hour, before the
standards audit of 2026-11-09 changed the event, export, and query-time code that
`onvif_soap`, `export_rebase`, and `api_time` reach, and `tools_protocol` held three parsers
then.

| Target | Runs in 24 h |
|---|---|
| `api_time` | 1.05 × 10¹⁰ |
| `describe_diagnostics` | 1.85 × 10⁸ |
| `export_rebase` | 4.81 × 10⁹ |
| `onvif_soap` | 2.73 × 10⁸ |
| `rtsp_message` | 3.02 × 10⁷ |
| `tools_protocol` (three parsers) | 3.59 × 10⁹ |

## Fuzzing (2026-11-09 to 2026-11-10)

Seven more targets ran the same command for 24 h each from 2026-11-09T05:07Z, without a
finding, on a tree that held the audit's changes to the code they reach. The civil-date
arithmetic the ONVIF timestamp parser calls moved to `iris-core` afterwards, unchanged.

| Target | Runs in 24 h |
|---|---|
| `api_auth` | 9.88 × 10⁹ |
| `api_range` | 1.47 × 10¹⁰ |
| `depacketize_h265` | 1.03 × 10⁸ |
| `discovery_parts` | 5.63 × 10⁷ |
| `onvif_notification` | 1.20 × 10⁷ |
| `rtp_packet` | 1.71 × 10¹⁰ |
| `vod_playlist` | 7.49 × 10⁷ |

## Fuzzing (2026-11-10 to 2026-11-11)

Six targets ran the same command for 24 h each from 2026-11-10T01:29Z, without a finding:
the two that had no completed run, and four whose code the audit had changed or whose
harness had been extended.

| Target | Runs in 24 h |
|---|---|
| `api_time` | 6.85 × 10⁹ |
| `export_rebase` | 2.76 × 10⁹ |
| `onvif_soap` | 4.29 × 10⁸ |
| `rtsp_request` | 7.00 × 10⁸ |
| `tools_logs` | 9.76 × 10⁷ |
| `tools_protocol` | 4.12 × 10⁸ |

Nine targets of the first campaign ran again for 24 h each from 2026-11-10T05:09Z on the
audited tree, without a finding:

| Target | Runs in 24 h |
|---|---|
| `access_unit` | 9.50 × 10⁹ |
| `box_header` | 1.81 × 10¹⁰ |
| `dahua_part` | 2.60 × 10⁸ |
| `digest_challenge` | 4.86 × 10⁸ |
| `index` | 3.23 × 10⁹ |
| `multipart` | 3.56 × 10⁷ |
| `onvif_utc` | 3.94 × 10⁹ |
| `scan` | 5.04 × 10⁹ |
| `trailer` | 1.40 × 10¹⁰ |

The audio work of 2026-11-11 then changed code that three targets reach: the fragment reader
(`scan`, `export_rebase`) and the viewer's playlist rules (`vod_playlist`). `export_rebase`
gained the track move, the cut of a fragment's start, the per-track edit lists, and the
sample entry reader, and encodes a fragment from its input so that those paths are reached
past the checksum; a two-minute run of it made 6.2 × 10⁵ executions without a finding.

## Fuzzing (2026-11-11 to 2026-11-12)

Four targets ran the same command for 24 h each from 2026-11-11T05:13Z, without a finding:
`describe_diagnostics` and `rtsp_message`, which had no completed run on the audited tree,
and `scan` and `export_rebase` on the tree with audio, the second with a harness that
encodes a fragment from its input to reach the export's cuts past the checksum.

| Target | Runs in 24 h |
|---|---|
| `describe_diagnostics` | 1.12 × 10⁸ |
| `export_rebase` | 3.62 × 10⁸ |
| `rtsp_message` | 1.82 × 10⁷ |
| `scan` | 9.73 × 10⁹ |

## Fuzzing (2026-11-12 to 2026-11-13)

Two targets ran the same command for 24 h each from 2026-11-12T00:43Z on the tree with audio,
without a finding: the viewer's playlist rules and its reading of a live fragment's media
time.

| Target | Runs in 24 h |
|---|---|
| `live_fragment` | 2.96 × 10¹⁰ |
| `vod_playlist` | 6.40 × 10⁷ |

## Mutation testing (2026-11-10)

`just mutants FILE...` (cargo-mutants 27.1.0, two workers, each crate's own tests) over the
format, timing, parsing, storage, catalog, API, and daemon code. A mutant that hangs the tests
counts as detected. The figures are of the first run over each set, before the tests written
from it; the second round reran the first two sets and added the export and LL-HLS handlers.

| Code | Mutants | Caught | Timed out | Missed | Unviable |
|---|---:|---:|---:|---:|---:|
| ingest: regulariser, assembler | 279 | 223 | 0 | 48 | 8 |
| mp4: fragment, index, reader, export | 502 | 407 | 12 | 58 | 25 |
| mp4: boxes, init, hls, crc | 113 | 89 | 1 | 19 | 4 |
| second round, with api export and llhls | 783 | 635 | 13 | 102 | 33 |
| mp4: export, init, fragment, with the two-track export (2026-11-11) | 260 | 232 | 9 | 12 | 7 |
| api: export and hls, with audio (2026-11-11) | 351 | 293 | 4 | 42 | 12 |
| ingest audio; gui sound (2026-11-11) | 109 | 98 | 0 | 4 | 7 |
| h265 | 51 | 46 | 0 | 3 | 2 |
| core: time, id, event | 294 | 239 | 1 | 36 | 18 |
| core: config | 139 | 125 | 0 | 10 | 4 |
| events: onvif | 191 | 153 | 0 | 24 | 14 |
| events: feed, dahua, multipart, digest, backoff | 237 | 174 | 6 | 41 | 16 |
| store: recover, segment | 148 | 80 | 0 | 44 | 24 |
| store: writer, stream, root worker, layout, policy | 159 | 117 | 0 | 31 | 11 |
| catalog: db | 308 | 209 | 3 | 50 | 46 |
| catalog: handle; replay: synth | 200 | 120 | 2 | 41 | 37 |
| api: auth, query, segments | 267 | 199 | 7 | 30 | 31 |
| api: hls, keyframe, live, admission | 247 | 179 | 0 | 49 | 19 |
| api: state, events, cameras, provision, snapshot, websocket, server, metrics, health, error, onvif | 240 | 106 | 10 | 35 | 89 |
| ingest: live, stats, source, session; tls | 270 | 166 | 17 | 64 | 23 |
| irisd: retention | 122 | 80 | 0 | 38 | 4 |
| irisd: adopt, inspect, hls | 235 | 117 | 0 | 101 | 17 |
| irisd: run, record, selfcheck, capability, permissions, catalog observer, provision | 432 | 207 | 0 | 131 | 94 |
| replay: rtp, rtsp, sdp, media | 280 | 239 | 3 | 24 | 14 |
| replay: server, binary; tools: time, logs, probe parser, stats, summary, secret | 772 | 593 | 13 | 142 | 24 |
| tools: report, sample | 694 | 372 | 3 | 303 | 16 |
| ingest: audio, cadence | 83 | 76 | 0 | 2 | 5 |
| ingest: regulariser, with empty slots held (2026-11-12) | 202 | 181 | 0 | 18 | 3 |
| ingest: drift rule, clock measures (2026-11-13) | 115 | 105 | 0 | 6 | 4 |

What the missed mutants showed, and what followed the same day:

- A regulariser rule no test needed, which measurement then showed to do harm
  (`11-multi-camera-results.md`, generated stalls); it was removed.
- Whole paths that no test ran: the Dahua feed task, the ONVIF retry loop, the daemon's
  recording path from camera to sealed catalog row, the offline `hls` and `inspect` commands,
  and every field of the initialization segment. Each has an end-to-end or field-by-field
  test.
- Settings that could be inverted unnoticed: certificate verification in the Dahua feed and
  snapshot clients (the ONVIF client was covered), and the watchdog's progress condition.
- Facts checked only in combination or only at one end: sidecar binding and fragment checks,
  capability expiry and path limits, the free-space floor, event maintenance at its age and
  recording-horizon boundaries, `tfhd` and `trun` layouts of other muxers.

Left as they are: mutants equivalent to the original (a second loop exit, flags that share no
bit, log-only branches), limits of 64 MiB and more that a test would have to allocate, and
error handling that needs injected faults (transient resource errors, `AlreadyExists` retries,
directory sync), which the full-filesystem test covers only for `ENOSPC`. In the replay server
and the tools that also leaves the entry points, the sender report interval, and an empty
password variable, which a test cannot set without `unsafe`. Of the 303 missed in the soak
report and sampler, the tests written cover the arithmetic the reports rest on (slope,
interpolation, growth across resets, write amplification); the rest is rendering of report
sections and reads of `/proc`, the journal, and the API. `iris-gui` and the tools' benchmark
driver, probes, and `gh-commit` were not run.

The run of 2026-11-11 over the format crate, after the two-track export, the cut of a
fragment's start, and the sync flags of audio fragments, missed twelve mutants, none in that
code: flags that share no bit, a version-0 `tfdt` no test writes, and bounds that a validated
fragment cannot reach.

The run of the same day over the API's export and playlist code missed 42. Those in the
audio code were rules no test held, and each has a test: sound placed by stored decode times
that agree with the wall times, sound in two files, sound before the first video fragment or
in a gap of the video, a range inside one fragment, sound cut only where the export ends, and
the audio playlist served through its route. The rest are size limits, constants, and
sidecar faults in code that the audio work did not change. Over the audio assembler and the
viewer's sound module, on the tree with those tests, four were missed, all equivalent under
test: the switch that discards audio, which every test has on, a formatter, and a window
kept as one entry for each arrival.

The run of 2026-11-12 over the regulariser, with a fragment's empty slots held at the
declared rate, missed 18, none in that rule: the 1 % at which the delivered rate takes over,
the cadence an undeclared rate is held against, the 20 % past which a fragment is left out
of the estimate, the tolerance at which a fragment snaps, what a snap reports, and the three
snaps after which a declared rate is forgotten. Each has a test, and the run with them
misses seven, all equivalent: a hold or a snap's hold of zero ticks added, a window of no
frames, which is never stored, a reference rate of zero, the frame duration a shortening
snap starts from, which the spread that follows overrides, and a spread of nothing.

The run of 2026-11-13 over the drift rule that says what step it would make and the
measures of the audio against the video (`steer.rs`, `sync.rs`) missed six: the threshold
on the early side at exactly three quarters of a frame, the distance that puts a step
inside the hour, the words of a reason to stop, and the median of the video's timestamps
against its decode times. Each has a test, and the run with them catches all 111 that
build. A run of the same day over the same two files, with the audio's place read by pairs
of sender reports and a rule without a stop for readings apart, catches all 114 that build
of 118. A run of 2026-11-14, with a run without sender reports judged by two readings,
missed three of 152: the sign of what the rule has said, taken off each reading, and a
comparison between two equal readings. With a test that has the readings apart each way
round and the lesser of the two taken without a comparison, all 145 that build of 149 are
caught. With a window keeping at most 1,024 pairs and a window that lacks a reading kept
out of both medians (2026-11-14), all 151 that build of 155 are caught. With a run without
reports judged from three windows and by medians (2026-11-14), one of 167 was missed: the
index of the median, which its mutant leaves the same for three values. With a test of the
median for other counts, all 163 that build are caught. With the steps made on arriving
frames (2026-11-15), the audio assembler and the rule together: one of 175 was missed, a
choice of which record of a frame takes its bytes, which records the same either way.
With the bytes taken once, all 160 that build of 172 are caught.

## Open items (phase-2 exit criteria in `08-roadmap.md`)

ONVIF PullPoint events, open on 2026-11-02, ran against hardware later
(`11-multi-camera-results.md`, a camera without the vendor event feed).

- Seven-day soak: run the daemon as the packaged systemd service.
- `mediastreamvalidator` on the daemon's playlists and AVPlayer (needs a `listen` address
  the device can reach).
- Fuzzing: the exit criterion is judged on a 24 h run of every target on the release tree.
