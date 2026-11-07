# 05 — Camera ingest, timing, and events

Full research notes with citations: `appendix/D-camera-integration.md`. Measurements on cam1:
`01-camera-probe.md`.

## 1. RTSP session policy (retina 0.4.21, feature `h265`)

| Setting | Value | Reason |
|---|---|---|
| Transport | TCP interleaved | zero loss on a LAN (0 sequence gaps in 90 s on cam1), kernel retransmit, no reorder buffer needed; retina's UDP is experimental with no reorder buffer; Moonfire, go2rtc and Frigate all default to TCP. Head-of-line blocking only adds latency, irrelevant for recording |
| Frame format | 4-byte length-prefixed NALs, no parameter-set insertion | matches the `hvc1` sample format byte-for-byte; parameter sets come from `Stream::parameters()` and `VideoFrame::has_new_parameters()` |
| Control messages | 1 MiB maximum for one RTSP head plus body, enforced while parsing and reading | a setup deadline is not an allocation bound; every message is rejected before retained parser state or the connection read buffer crosses the declared limit |
| Keepalive | retina default: `GET_PARAMETER` every `min(timeout, 60)/2` s (cam1 timeout = 60 s) | ONVIF accepts any non-TEARDOWN request as keepalive; `SET_PARAMETER` breaks Tapo cameras |
| Teardown | `TeardownPolicy::Always` | every setup or playing-session outcome releases camera-side state |
| RTCP | `ReceiverReportPolicy::Never`; SRs consumed for diagnostics | TCP needs no RR; SR NTP is recorded per run, never used as the time base |
| Timestamps | `enforce_timestamps_with_max_jump_secs(10)` | large jumps become run boundaries instead of corrupt timelines |
| Inactivity | `tokio::time::timeout(30 s, stream.next())`; 15 s on DESCRIBE, SETUP, and PLAY; TEARDOWN on every exit | retina has no data-inactivity timeout of its own; a wedged camera once held DESCRIBE open for 130 s |
| Recorder clock | fresh realtime/monotonic correlations map packet receive instants; an offset change over 2 s flushes the pending fragment and ends the session | clock steps start a separate run instead of leaving video behind event timestamps |
| Session exit | cancellation is handled during setup and demuxing; after demux state exists, cancellation, inactivity, EOF, RTSP/demux failure, and frame, RTCP, or clock errors converge on one final flush before connected state is cleared | complete pending access units are delivered exactly once, and the final fragment uses the ordinary timing, gap, size, and sink accounting path |
| Socket reads | timer-driven: drain the demuxer without arming a wake-up, then sleep until the next tick of a clock shared by every stream (`ingest.read_interval_ms`, default 20) | one read per interval instead of one per packet and one runtime wake-up for all streams; about a quarter less CPU at the same input, 20 ms more live latency (12) |
| Detached socket | with timer-driven reads the socket leaves tokio's reactor once `PLAY` succeeds (`PlayOptions::detach_from_reactor` in the patched retina): reads that would block return `Pending` without a waker, keepalives and `TEARDOWN` write blocking | `epoll` is edge-triggered and fired once per arriving segment while a socket sat above the low-water mark, 5,400 unused wake-ups a second for fourteen streams; detached, 70 a second, and a third less CPU (12 §6) |
| Shared ticker | one `ReadTicker` task per runtime wakes every stream waiting for the next tick (`IngestOptions::ticker`) | a timer per stream cost one runtime unpark (an `eventfd` write) per stream per tick (12 §6) |
| Low-water mark | `SO_RCVLOWAT` 32 KiB on the RTSP socket once `PLAY` succeeds (`ingest.recv_low_water_kib`; `PlayOptions::recv_low_water` in the patched retina under `third_party/`) | the kernel reports the socket readable once per 32 KiB instead of once per packet; with timer reads the cost halves against packet-driven reads; a 1 Mbit/s stream waits up to a quarter second (12 §4) |
| Reconnect | 1 s → ×2 → 30 s cap, deterministic ±20 % jitter mixed from stream identity and retry attempt; reset after a 60 s healthy session; separate run; resume recording at the next IRAP | decorrelated streams and attempts avoid a synchronized retry burst; Moonfire retries after 1 s; Frigate uses a 10 s socket timeout |
| Credentials | passed to retina, never in the URL | Moonfire rule; URLs are logged |
| URLs | main `subtype=0`, sub `subtype=1`; keep `proto=Onvif` for Dahua compatibility | measured on cam1 |

What retina already handles so irisd does not have to: RFC 7798 single/AP/FU
depacketisation (SRST only, no DONL, which is what every IPC emits), AU assembly on marker
bit with timestamp-change fallback and the RFC "can end AU" NAL list, mid-AU loss
reporting (`VideoFrame::loss()`), IRAP detection (IDR in 0.4.21; CRA/BLA on main),
64-bit timestamp unwrapping, per-packet receipt times (`start_ctx()` since 0.4.18),
ring-buffer reads (0.4.20), and a long list of vendor quirks (Dahua NVR ssrc radix, Hikvision
`seq=0`, Reolink scaling lists, Tapo keepalive, empty FUs, NAL-type changes within an FU).
The patched copy under `third_party/` bounds pinned H.265 access-unit payload and projected
output at 64 MiB while depacketizing, releases partial state on a controlled error, and
applies the 1 MiB production control-message limit above. The application repeats the
64 MiB frame bound at fragment assembly as defense in depth.

cam1 specifics confirmed in 01: FUs only, AP never, marker on every AU, VPS/SPS/PPS as single
NAL packets before each IDR, no RTP extensions, no padding, session timeout 60 s, SR every 5 s.

## 2. Per-stream pipeline

```
retina Demuxed ──VideoFrame──▶ regulariser ──▶ fragment builder ──▶ live ring ──▶ WS / LL-HLS parts
                                                └──▶ writer queue ──▶ disk
               ──Rtcp SR──▶ run diagnostics (SR NTP − wall)
```

- **Regulariser** (04 §7.3): assigns durations; emits `(frame: Bytes, duration, is_irap,
  media_time, wall_us, raw_rtp)`. A changed parameter epoch with a different valid effective
  rate starts a fresh estimator at that rate without resetting decode continuity or cumulative
  diagnostics; unchanged, absent, and invalid rates preserve the estimator.
- **Fragment builder**: opens a fragment at an IRAP (or when the cap expires on long-GOP
  cameras), accumulates `Vec<Bytes>`, closes at the next IRAP or once the camera timeline
  exceeds `max_fragment_ms` by a quarter (so timestamp jitter never splits a GOP whose
  nominal length equals the cap). Receipt time and nominal sample time enforce the same
  cap when RTP stops advancing; hard limits of 4,096 frames and 64 MiB of pending payload
  apply independently of every clock and rate hint. The depacketizer's matching 64 MiB
  ingress bound prevents an oversized network access unit from reaching this stage. The
  assembler retains its own bound for direct inputs: it closes retained frames before
  discarding an oversized frame. Dependent recording frames are discarded until the next
  sync frame, which starts a separate run at decode time zero; the discarded interval
  remains a gap in wall time. Durations are finalized when the next frame arrives. At close,
  finalized durations update the per-run least-delay wall anchor (upward correction at most
  500 ppm), and `wall_us` is the anchor plus the fragment decode start. Emits
  `Fragment { samples, media_time, wall_us, starts_with_irap, new_params }`.
- **Live ring**: a fast-start cache from the most recent IRAP holds at most 600 frames and
  64 MiB of payload. Crossing either limit clears that run and waits for a later fitting
  IRAP. Subscriber fan-out stores each frame once for all subscribers and is bounded by its
  configured frame capacity and 64 MiB of payload per stream; with no subscribers it retains
  no fan-out payload. Queue eviction reports lag, discards dependent delivery, and resumes
  the subscriber at an IRAP. Payload `Bytes` are shared with recording and between the two
  live windows when they overlap.
- **Writer queue**: bounded by complete-fragment count (04 §8); the socket is never blocked
  on disk. A rejection ends recording continuity, records one gap for the consecutive loss
  epoch, preserves an already-pending sync frame when possible, and otherwise discards
  dependent recording frames until sync. Live publication continues.
- **Sender reports**: every RTCP SR's NTP time minus the NVR wall time at receipt is the
  camera clock offset reported in the `stream` statistics line, the cheapest continuous check
  that a camera's clock, and with it its event timestamps, stays aligned with the recorder.
- **Gaps**: the first frame after a reconnect logs `gap cause=reconnect` with the last frame
  time before the session ended; a regulariser hold of a second or more logs
  `gap cause=camera_hold` at the hold's start; the first rejected recording fragment in a
  loss epoch logs `gap cause=sink_rejected`. Each increments the stream's `gaps` counter,
  so "zero unexplained gaps" is a grep.
- **Parameter-set epochs**: hash VPS/SPS/PPS at every IRAP; on change seal the file, insert a
  `sample_entry`, push a replacement init segment to live clients, and apply the effective-rate rule
  above.
- **Audio**: unsupported in the 0.1 alpha. Configuration accepts only the `main` and `sub`
  video kinds for recording. An audio implementation needs AAC `mp4a`/`esds`, fixed
  1024-sample durations, its own file and fragment cadence, and alignment per 04 §7.5;
  G.711 and L16 are unsuitable for the HLS path.

## 3. Events

Normalised event record: `{camera_id, source, kind, state (start | stop | pulse),
camera_time_us, nvr_time_us, data JSON}` with `kind ∈ {motion, human, vehicle, line_cross,
intrusion, tamper, audio, face, video_loss, time_change, …}`. Configurable subscriptions are
`dahua_cgi` and `onvif_pull_point`; other enum values remain available for stored/test data but
configuration rejects them. Event-to-video alignment uses NVR receipt time directly, never the
camera's clock (cam1's ONVIF UTC is off by 1 h).
Camera-controlled event strings share one storage boundary: data is valid JSON text at most
64 KiB and an unmapped vendor kind code is at most 1 KiB. Invalid vendor text becomes a JSON
string, and a valid encoding beyond the data limit becomes `{"truncated":true}` instead of a
syntactically invalid prefix. Kind bounding stops at a UTF-8 boundary. Both paths rebuild bounded
strings before catalog notification, direct insertion, or WebSocket publication so a short value
cannot retain a large backing allocation.

Implemented sources for Dahua-family cameras:

1. **Dahua CGI push feed** — `GET /cgi-bin/eventManager.cgi?action=attach&codes=[All]&heartbeat=5`
   with HTTP digest (reuse the digest nonce across requests to avoid log spam). Attachment
   and authentication have a 10 s deadline through response headers; the body remains an
   indefinite stream bounded by an absolute deadline since the last completed part. Parse
   `multipart/x-mixed-replace` with the boundary from `Content-Type`; tolerate
   `Content-Length:` without a space and variable blank lines; bound each body to 256 KiB plus
   declared syntax overhead while appending, and retain scan cursors so one-byte fragmentation is
   linear; stall detection = 2× heartbeat + 5 s; reconnect with backoff. Codes seen in the wild:
   `VideoMotion`, `VideoMotionInfo`,
   `SmartMotionHuman`, `SmartMotionVehicle`, `CrossLineDetection`, `CrossRegionDetection`,
   `LeftDetection`, `TakenAwayDetection`, `FaceDetection`, `AudioMutation`, `AudioAnomaly`,
   `VideoLoss`, `VideoBlind`, `AlarmLocal`, `NewFile`, `TimeChange`, `NTPAdjustTime`; filter
   locally rather than passing long `codes=` lists (some firmware returns 400/500). Carries
   IVS object metadata in `data={}` JSON. Validated on cam1 with real motion: `VideoMotion`
   Start/Stop, `VideoMotionInfo` State pulses while active, `NewFile` when the camera's SD
   recording rolls (coincides with a forced IDR), `RtspSessionDisconnect` when a client vanishes
   without TEARDOWN (01 §7).
2. **ONVIF PullPoint** (vendor-neutral) via `oxvif` 0.17: `CreatePullPointSubscription`
   (10 min requested) → `PullMessages` (Timeout 5 s, MessageLimit 100) in a loop with a 0.75 s
   cooldown. `Renew` runs after 80 % of the camera-granted `CurrentTime` to `TerminationTime`
   duration, clamped between a 750 ms anti-spin floor and the measured 8 min cadence; its timer
   remains active during pulls and cooldowns. Zero/tiny grants use the floor, while missing,
   malformed, or reversed grants use the 8 min fallback. The pull timeout stays under the 10 s
   that the `oxvif` HTTP transport applies to every request: a 60 s long poll (Home Assistant's
   constant) failed on every quiet pull against hardware (11).
   WS-UsernameToken `Created` uses the camera's clock offset (01 §5), with host time as the
   fallback only for a structured clock-authentication fault from cameras that misreport UTC yet
   check tokens against their true clock. Fault classification uses code, subcode, and reason;
   free-form Detail never authorizes fallback or an authentication pause. HTTP 401 and structured
   WS-Security authentication failures wait at least 30 s before another session; unrelated
   network, parse, and SOAP faults do not trigger a second create attempt. Some cameras want HTTP
   digest on the subscription URL.
   Topics on cam1 include `tns1:VideoSource/MotionAlarm`,
   `tns1:RuleEngine/CellMotionDetector/Motion`, `FieldDetector/ObjectsInside`,
   `LineDetector/Crossed`, `TamperDetector/Tamper`, `AudioAnalytics/Audio/DetectedSound`.
   CGI and ONVIF both apply the endpoint's `tls_verify` policy to HTTPS requests. ONVIF service
   and subscription URLs must match the configured scheme, host, and effective port, with no
   userinfo or fragment; authenticated SOAP follows no redirects. The SOAP transport retains
   HTTP digest authentication and its 10 s request bound, caps every response status at 1 MiB,
   and the vendored XML parser independently caps depth, nodes, attributes, and accumulated text.
   Shutdown after a reference exists performs one unsubscribe attempt bounded to 5 s before
   clearing connected state. Standard `ElementItem` content is retained canonically and bounded,
   including child-adjacent mixed-content whitespace, without interpreting vendor analytics
   schemas. Property operations are handled before boolean state:
   inactive/unknown initialization is suppressed and deletion always produces Stop.
   Known Dahua quirks: creating a subscription may enable motion detection; `TerminationTime`
   advances ~10× the pull duration; messages without `Data` exist.
Phase-5 source candidates, not accepted by 0.1 configuration:

- **ONVIF metadata RTP track** (`vnd.onvif.metadata`, retina `MessageFrame`): XML
   `tt:MetadataStream` with analytics frames and events stamped on the video RTP clock, so
   frame-accurate alignment is possible. SETUP works on cam1 but the track stayed silent through
   four motion episodes, so on this firmware it apparently carries only IVS rule results
   (tripwire/intrusion); unvalidated until an IVS rule is enabled (01 §10).
- **RPC2 JSON-RPC polling** only as a last resort for firmware that 404s `/cgi-bin/`.

Each feed counts messages (heartbeats and empty pulls included) separately from events, so a
feed that is connected but silent shows as a growing idle time in the `events` statistics
line; ONVIF pulls carry a latency histogram and a renewal count. `errors` counts each failed
session once, while `reconnects` counts the corresponding retry/resubscribe cycle. Report tools
key these counters by camera and source. The latency from receipt to the committing catalog
transaction is measured in the catalog thread; with a 100 ms batch interval its p99 sits in
the 100 to 200 ms bucket.

Events drive catalog rows, `retain_until_us` (event-adjacent footage kept longer), WebSocket
push to clients, and timeline markers. The 0.1 alpha performs no server-side motion detection.
An optional analyzer belongs outside the record path and consumes sub-stream frames from the live
ring at no more than 5 fps.

Catalog admission is nonblocking and ordered with segment lifecycle changes, with hard command
count and owned-byte limits. Event notifications reject under overload while segment open/seal
transitions use reserved capacity; rejection and reconciliation-required counters expose both
outcomes. Event state is maintained incrementally in at most 1,000-row transactions. Inactive
rows beyond the recording horizon, 90 days, two million rows, or 512 MiB of logical strings are
removed oldest first. Active retention marks are never pruned and have separate 100,000-row and
128 MiB admission limits. Insertion performs no more than the same bounded pruning step and
omits or rejects an event that still cannot fit, without rolling back segment lifecycle work.
Catalog statistics expose event rows, logical bytes, active marks, pruned rows, and database/WAL
sizes.

## 4. Planned camera provisioning (phase 5)

The 0.1 alpha reads operator-supplied camera configuration and never changes device settings.
An explicit opt-in provisioning design may use the following policy:

Dahua CGI `configManager.cgi?action=setConfig&…` (ONVIF Media2 `SetVideoEncoderConfiguration`
for other vendors):

- `Encode[0].MainFormat[0].Video`: `Compression=H.265`, `GOP ≤ 2×FPS` (cam1: 60 at 30 fps,
  already right), `BitRateControl=CBR` (or VBR with a cap), `Profile=Main`, smart codec off
  (`SmartEncode[0].Enable=false`, already off on cam1). Smart/"+" modes produce multi-second
  irregular GOPs and long-term references, which stretch segment and seek granularity and
  break mid-GOP cuts; Blue Iris and SecuritySpy both tell users to disable them.
- Audio: `Audio.Compression=AAC` (cam1 ships L16 PCM at 1 Mbps on main and sub).
- Disable `ExtraFormat[1]` (the 1080p third stream) unless a client uses it or it serves as
  the secondary stream because the sub stream stops beside a second session (01 §5b); in that
  case lower its resolution and bitrate to what a grid tile needs.
- NTP: point cameras at the NVR host running chrony (`NTP.Address`, `UpdatePeriod`), prefer
  UTC, disable DST on the camera; check the offset at startup with `GetSystemDateAndTime`
  (compare **local** time because of the Dahua UTC/DST bug) or `global.cgi?action=getCurrentTime`,
  warn above 5 s.
- Record the camera's capabilities and current encoder settings in the catalog; alert when a
  camera's settings drift from the provisioned profile.

## 5. Planned discovery (phase 5)

The 0.1 alpha has no discovery path; camera entries come from TOML. A later discovery path may
use the following calls while keeping manual entries authoritative:

WS-Discovery probe to 239.255.255.250:3702 (`oxvif`), `GetCapabilities`/`GetServices`,
`GetProfiles` + `GetStreamUri` to derive main/sub URLs for non-Dahua cameras; manual TOML
entries always win. 3702/tcp is closed on cam1, as expected (discovery is UDP).

## 6. Open questions

1. Validate whether the ONVIF metadata track carries useful events and measure source latency.
2. Dahua CGI keys for `Video.Profile` and smart codec on current firmware (diff `getConfig`
   before/after a UI toggle).
3. Backpressure tolerance of the camera (how long a stalled reader keeps the session).
4. Whether cam1 ever changes SPS without a reconnect (resolution change test).
