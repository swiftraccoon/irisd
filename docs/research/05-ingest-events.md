# 05 — Camera ingest, timing, and events

Full research notes with citations: `appendix/D-camera-integration.md`. Measurements on cam1:
`01-camera-probe.md`.

## 1. RTSP session policy (retina 0.4.21, feature `h265`)

| Setting | Value | Reason |
|---|---|---|
| Transport | TCP interleaved | zero loss on a LAN (0 sequence gaps in 90 s on cam1), kernel retransmit, no reorder buffer needed; retina's UDP is experimental with no reorder buffer; Moonfire, go2rtc and Frigate all default to TCP. Head-of-line blocking only adds latency, irrelevant for recording |
| Frame format | 4-byte length-prefixed NALs, no parameter-set insertion | matches the `hvc1` sample format byte-for-byte; parameter sets come from `Stream::parameters()` and `VideoFrame::has_new_parameters()` |
| Keepalive | retina default: `GET_PARAMETER` every `min(timeout, 60)/2` s (cam1 timeout = 60 s) | ONVIF accepts any non-TEARDOWN request as keepalive; `SET_PARAMETER` breaks Tapo cameras |
| Teardown | `TeardownPolicy::Auto` | avoids stale live555 sessions |
| RTCP | `ReceiverReportPolicy::Never`; SRs consumed for diagnostics | TCP needs no RR; SR NTP is recorded per run, never used as the time base |
| Timestamps | `enforce_timestamps_with_max_jump_secs(10)` | large jumps become run boundaries instead of corrupt timelines |
| Inactivity | `tokio::time::timeout(30 s, stream.next())`; 15 s on DESCRIBE, SETUP, and PLAY; TEARDOWN on every exit | retina has no data-inactivity timeout of its own; a wedged camera once held DESCRIBE open for 130 s |
| Socket reads | timer-driven: drain the demuxer without arming a wake-up, then sleep until the next tick of a clock shared by every stream (`ingest.read_interval_ms`, default 20) | one read per interval instead of one per packet and one runtime wake-up for all streams; about a quarter less CPU at the same input, 20 ms more live latency (12) |
| Detached socket | with timer-driven reads the socket leaves tokio's reactor once `PLAY` succeeds (`PlayOptions::detach_from_reactor` in the patched retina): reads that would block return `Pending` without a waker, keepalives and `TEARDOWN` write blocking | `epoll` is edge-triggered and fired once per arriving segment while a socket sat above the low-water mark, 5,400 unused wake-ups a second for fourteen streams; detached, 70 a second, and a third less CPU (12 §6) |
| Shared ticker | one `ReadTicker` task per runtime wakes every stream waiting for the next tick (`IngestOptions::ticker`) | a timer per stream cost one runtime unpark (an `eventfd` write) per stream per tick (12 §6) |
| Low-water mark | `SO_RCVLOWAT` 32 KiB on the RTSP socket once `PLAY` succeeds (`ingest.recv_low_water_kib`; `PlayOptions::recv_low_water` in the patched retina under `third_party/`) | the kernel reports the socket readable once per 32 KiB instead of once per packet; with timer reads the cost halves against packet-driven reads; a 1 Mbit/s stream waits up to a quarter second (12 §4) |
| Reconnect | 1 s → ×2 → 30 s cap, ±20 % jitter; new run; resume recording at the next IRAP | Moonfire retries after 1 s; Frigate uses a 10 s socket timeout |
| Credentials | passed to retina, never in the URL | Moonfire rule; URLs are logged |
| URLs | main `subtype=0`, sub `subtype=1`; keep `proto=Onvif` (enables the ONVIF metadata track and backchannel on Dahua) | measured on cam1 |

What retina already handles so irisd does not have to: RFC 7798 single/AP/FU
depacketisation (SRST only, no DONL, which is what every IPC emits), AU assembly on marker
bit with timestamp-change fallback and the RFC "can end AU" NAL list, mid-AU loss
reporting (`VideoFrame::loss()`), IRAP detection (IDR in 0.4.21; CRA/BLA on main),
64-bit timestamp unwrapping, per-packet receipt times (`start_ctx()` since 0.4.18),
ring-buffer reads (0.4.20), and a long list of vendor quirks (Dahua NVR ssrc radix, Hikvision
`seq=0`, Reolink scaling lists, Tapo keepalive, empty FUs, NAL-type changes within an FU).

cam1 specifics confirmed in 01: FUs only, AP never, marker on every AU, VPS/SPS/PPS as single
NAL packets before each IDR, no RTP extensions, no padding, session timeout 60 s, SR every 5 s.

## 2. Per-stream pipeline

```
retina Demuxed ──VideoFrame──▶ regulariser ──▶ fragment builder ──▶ live ring ──▶ WS / LL-HLS parts
               ──AudioFrame──▶ audio fragment builder ─────────────┘        └──▶ writer queue ──▶ disk
               ──MessageFrame (ONVIF metadata)──▶ event normaliser
               ──Rtcp SR──▶ run diagnostics (SR NTP − wall)
```

- **Regulariser** (04 §7.3): assigns durations; emits `(frame: Bytes, duration, is_irap,
  media_time, wall_us, raw_rtp)`.
- **Fragment builder**: opens a fragment at an IRAP (or when the cap expires on long-GOP
  cameras), accumulates `Vec<Bytes>`, closes at the next IRAP or once the camera timeline
  exceeds `max_fragment_ms` by a quarter (so timestamp jitter never splits a GOP whose
  nominal length equals the cap), finalising durations when the next frame arrives; emits `Fragment { samples,
  media_time, wall_us, starts_with_irap, new_params }`.
- **Live ring**: the last two complete fragments plus the frames of the one in progress,
  shared `Bytes` with the writer queue (no copies). Late joiners get the init segment and the
  most recent IRAP-started fragment.
- **Writer queue**: bounded by bytes (04 §8); the socket is never blocked on disk.
- **Sender reports**: every RTCP SR's NTP time minus the NVR wall time at receipt is the
  camera clock offset reported in the `stream` statistics line, the cheapest continuous check
  that a camera's clock, and with it its event timestamps, stays aligned with the recorder.
- **Gaps**: the first frame after a reconnect logs `gap cause=reconnect` with the last frame
  time before the session ended; a regulariser hold of a second or more logs
  `gap cause=camera_hold` at the hold's start. Both increment the stream's `gaps` counter,
  so "zero unexplained gaps" is a grep.
- **Parameter-set epochs**: hash VPS/SPS/PPS at every IRAP; on change seal the file, insert a
  `sample_entry`, push a new init segment to live clients.
- **Audio** (v1: AAC only): `mp4a` + `esds` from the SDP `config=` AudioSpecificConfig
  (cam1's third stream is AAC-LC at 64 kHz; main/sub must be switched from L16 PCM first);
  fixed 1024-sample durations; own file and fragment cadence; aligned per 04 §7.5. G.711 and
  L16 are not recorded in v1 (HLS cannot play them; L16 costs 1 Mbps).

## 3. Events

Normalised event record: `{camera_id, source, kind, state (start | stop | pulse),
camera_time_us, nvr_time_us, data JSON}` with `kind ∈ {motion, human, vehicle, line_cross,
intrusion, tamper, audio, face, video_loss, time_change, …}` and `source ∈ {dahua_cgi,
onvif_pullpoint, onvif_metadata, manual}`. Event-to-video alignment uses NVR receipt time
minus a per-camera latency estimate (default 300 ms, tunable after measurement), never the
camera's clock (cam1's ONVIF UTC is off by 1 h).

Source priority for Dahua-family cameras (all three verified present on cam1):

1. **Dahua CGI push feed** — `GET /cgi-bin/eventManager.cgi?action=attach&codes=[All]&heartbeat=5`
   with HTTP digest (reuse the digest nonce across requests to avoid log spam). Parse
   `multipart/x-mixed-replace` with the boundary from `Content-Type`; tolerate
   `Content-Length:` without a space and variable blank lines; stall detection = 2× heartbeat;
   reconnect with backoff. Codes seen in the wild: `VideoMotion`, `VideoMotionInfo`,
   `SmartMotionHuman`, `SmartMotionVehicle`, `CrossLineDetection`, `CrossRegionDetection`,
   `LeftDetection`, `TakenAwayDetection`, `FaceDetection`, `AudioMutation`, `AudioAnomaly`,
   `VideoLoss`, `VideoBlind`, `AlarmLocal`, `NewFile`, `TimeChange`, `NTPAdjustTime`; filter
   locally rather than passing long `codes=` lists (some firmware returns 400/500). Carries
   IVS object metadata in `data={}` JSON. Validated on cam1 with real motion: `VideoMotion`
   Start/Stop, `VideoMotionInfo` State pulses while active, `NewFile` when the camera's SD
   recording rolls (coincides with a forced IDR), `RtspSessionDisconnect` when a client vanishes
   without TEARDOWN (01 §7).
2. **ONVIF PullPoint** (vendor-neutral) via `oxvif` 0.17: `CreatePullPointSubscription`
   (10 min) → `PullMessages` (Timeout 5 s, MessageLimit 100) in a loop with a 0.75 s cooldown,
   `Renew` every 8 min, restart with backoff on error. The pull timeout stays under the 10 s
   that the `oxvif` HTTP transport applies to every request: a 60 s long poll (Home
   Assistant's constant) failed on every quiet pull against hardware (11).
   WS-UsernameToken `Created` uses the camera's clock offset (01 §5), with host time as the
   fallback for cameras that misreport UTC yet check tokens against their true clock; some
   cameras want HTTP digest on the subscription URL. Topics on cam1 include `tns1:VideoSource/MotionAlarm`,
   `tns1:RuleEngine/CellMotionDetector/Motion`, `FieldDetector/ObjectsInside`,
   `LineDetector/Crossed`, `TamperDetector/Tamper`, `AudioAnalytics/Audio/DetectedSound`.
   Known Dahua quirks: creating a subscription may enable motion detection; `TerminationTime`
   advances ~10× the pull duration; messages without `Data` exist.
3. **ONVIF metadata RTP track** (`vnd.onvif.metadata`, retina `MessageFrame`): XML
   `tt:MetadataStream` with analytics frames and events stamped on the video RTP clock, so
   frame-accurate alignment is possible. SETUP works on cam1 but the track stayed silent through
   four motion episodes, so on this firmware it apparently carries only IVS rule results
   (tripwire/intrusion); unvalidated until an IVS rule is enabled (01 §10).
4. **RPC2 JSON-RPC polling** only as a last resort for firmware that 404s `/cgi-bin/`.

Each feed counts messages (heartbeats and empty pulls included) separately from events, so a
feed that is connected but silent shows as a growing idle time in the `events` statistics
line; ONVIF pulls carry a latency histogram and a renewal count, and errors inside a session
are counted before it reconnects. The latency from receipt to the committing catalog
transaction is measured in the catalog thread; with a 100 ms batch interval its p99 sits in
the 100 to 200 ms bucket.

Events drive: catalog rows, `retain_until_us` (event-adjacent footage kept longer), clip
copies, WebSocket push to clients, and timeline markers. No server-side motion detection in
v1; an optional analyzer, if added, consumes sub-stream frames from the live ring at ≤ 5 fps and
never touches the record path.

## 4. Camera provisioning (explicit opt-in per camera)

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

## 5. Discovery

WS-Discovery probe to 239.255.255.250:3702 (`oxvif`), `GetCapabilities`/`GetServices`,
`GetProfiles` + `GetStreamUri` to derive main/sub URLs for non-Dahua cameras; manual TOML
entries always win. 3702/tcp is closed on cam1, as expected (discovery is UDP).

## 6. Open questions

1. Validate a real motion event on all three sources and measure camera latency.
2. Dahua CGI keys for `Video.Profile` and smart codec on current firmware (diff `getConfig`
   before/after a UI toggle).
3. `oxvif` against Dahua PullPoint (Renew semantics, digest on the subscription URL).
4. Backpressure tolerance of the camera (how long a stalled reader keeps the session).
5. Whether cam1 ever changes SPS without a reconnect (resolution change test).
