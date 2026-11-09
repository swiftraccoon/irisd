# 01 — Test camera probe: cam1 (Dahua-platform 4K dome, reseller-branded)

Measured 2026-10-31 from the dev host on the camera's network segment. Every number here was
produced by the commands in §11; nothing is quoted from a datasheet. The camera's identity
(model, firmware string, addresses) and credentials live in gitignored files:
`docs/local/cam1-identity.md` and `config/cameras.local.toml`.

## 1. Identity and services

| Item | Value |
|---|---|
| Device type | Dahua platform, sold under a reseller brand (exact model in `docs/local/`) |
| Firmware | a 2021 build (string in `docs/local/`) |
| SoC | SigmaStar |
| Open TCP ports | 80 (HTTP CGI + ONVIF), 554 (RTSP), 1935 (RTMP), 37777 (Dahua private). 443, 2020, 3702/tcp, 8000 closed |
| RTSP server | `Rtsp Server/3.0`: OPTIONS, DESCRIBE, ANNOUNCE, SETUP, PLAY, RECORD, PAUSE, TEARDOWN, SET_PARAMETER, GET_PARAMETER |
| ONVIF services | device 18.12, media 18.06, media2 18.12, events 2.60, analytics 18.12, imaging 18.12, deviceIO 18.12, recording 18.06, search 18.12, replay 17.06 |
| ONVIF media profiles | `MediaProfile00000` (main, audio declared G711), `00001` (sub, G711), `00002` (third, AAC) |
| NTP | enabled, pointed at the LAN NTP server, update period 1 min, a time zone with DST |
| Detection | `MotionDetect[0].Enable=true` (Dejitter 30 s); SMD (`SmartMotionDetect`) off; IVS/face rules off; `SmartEncode` off |

Encoder configuration (`configManager.cgi?action=getConfig&name=Encode`):

| Stream | RTSP `subtype` | Codec | Resolution | fps | Rate control | GOP | Audio |
|---|---|---|---|---|---|---|---|
| main `MainFormat[0]` | 0 | H.265 Main | 3840×2160 | 30 | CBR 8192 kbps | 60 | "PCM" = L16/64000 mono |
| sub `ExtraFormat[0]` | 1 | H.265 Main | 704×480 (D1) | 30 | CBR 1280 kbps | 60 | "PCM" = L16/64000 mono |
| third `ExtraFormat[1]` | 2 | H.265 Main | 1920×1080 | 30 | CBR 6144 kbps | 60 | AAC-LC 64 kHz, 64 kbps |

`MainFormat[1]` (motion) and `[2]` (alarm) are identical to `[0]`, so no event-triggered
encoder switching happens on this unit. The third stream is enabled by default and spends
encoder capacity for nothing; disable it unless a client uses it.

ffprobe on main and sub: `hevc`, profile Main, level 150 (5.0), `yuv420p`, `color_range=tv`,
`has_b_frames=0`, `pts == dts` for all packets, `time_base 1/90000`. The reported
`r_frame_rate 100/1` is an artefact of the 10 ms timestamp quantisation (§5).

## 2. SDP (main stream, parameter-set payloads shortened)

```
v=0
o=- 2251938229 2251938229 IN IP4 0.0.0.0
s=Media Server
c=IN IP4 0.0.0.0
t=0 0
a=control:*
a=packetization-supported:DH
a=rtppayload-supported:DH
a=range:npt=now-
m=video 0 RTP/AVP 98
a=control:trackID=0
a=framerate:30.000000
a=rtpmap:98 H265/90000
a=fmtp:98 profile-id=1;sprop-sps=QgEBAWAAAAMAAAMAAAMAAAMAlqAB4CACHFja5JMmuWcAgAAB9IAAOqcE;sprop-pps=RAHgdrAmQA==;sprop-vps=QAEMAf//AWAAAAMAAAMAAAMAAAMAlqwJ
a=recvonly
m=audio 0 RTP/AVP 97
a=control:trackID=1
a=rtpmap:97 L16/64000
a=recvonly
m=application 0 RTP/AVP 107
a=control:trackID=4
a=rtpmap:107 vnd.onvif.metadata/90000
a=recvonly
```

- `Content-Base` is the request URL plus a trailing slash; track controls are relative, so
  the SETUP URL is `<Content-Base>trackID=0` with the query string in the middle of the path.
  ffmpeg and retina both handle this.
- No `sprop-max-don-diff` → single-stream SRST mode, no DONL fields (RFC 7798 §7.1).
- SETUP: `Session: <id>;timeout=60`, `Transport: RTP/AVP/TCP;unicast;interleaved=0-1;ssrc=5933D9C2`
  (hex ssrc). PLAY: `RTP-Info` with `seq`/`rtptime` for video and metadata tracks.
- `a=packetization-supported:DH` advertises Dahua private packetisation; standard RFC 7798
  is used unless a client asks otherwise.

## 3. Elementary-stream structure (30 s raw capture, 889 access units)

| NAL type | Count | Mean size |
|---|---|---|
| 1 TRAIL_R | 884 | 24,005 B |
| 19 IDR_W_RADL | 15 | 675,565 B |
| 32 VPS / 33 SPS / 34 PPS | 15 each | 25 / 43 / 8 B |

- No SEI, AUD, CRA/BLA, or multi-slice pictures: every picture is exactly one NAL.
- Every IDR is preceded in-band by VPS, SPS, PPS (byte-identical across GOPs in the capture).
- GOP is 60 frames = 2.00 s (44/44 over 90 s). Over 15 min, 442 of 453 GOPs were 60 frames;
  11 were shortened (2–51 frames) by extra IDRs the camera inserts when it starts a motion
  clip on its SD card (`NewFile` events, §7). Fragmenting must therefore be IRAP-driven, never
  frame-counted.
- Annex-B start codes cost 3,776 B per 30 s; switching to 4-byte length prefixes (hvc1
  sample format) is size-neutral.

## 4. RTP packetisation (90 s over TCP interleaved, 63,534 packets)

| Metric | Value |
|---|---|
| Payload structures | FU (type 49): 63,399 · single NAL: 135 (= 45 × VPS/SPS/PPS) · AP (type 48): 0 |
| Max RTP payload | 1,440 B |
| Marker bit | on the last packet of every AU (2,682 of 2,682) |
| Extension / padding | none |
| Sequence gaps | 0 |
| Rate | 707 packets/s at 8.03 Mbps (CBR target 8,192 kbps) |
| Packets per 4K IDR / P frame | ≈ 450 / ≈ 16 |
| Audio | L16/64000 mono 16-bit: 129 KB/s = 1.03 Mbps in 1,365-byte packets (the SDP is right, the ONVIF profile's "G711" is wrong) |

## 5. Timing behaviour

RTP timestamps are quantised to 900 ticks (10 ms) and reflect encoder-output time, not
capture time. Delta histogram over 2,681 intervals:

| Δ ticks | Δ ms | Count |
|---|---|---|
| 2700 | 30 | 1,540 |
| 3600 | 40 | 756 |
| 0 | 0 | 108 |
| 1800 | 20 | 69 |
| 8100 | 90 | 68 |
| 900 | 10 | 66 |
| 4500 / 9000 / 6300 / 7200 | 50 / 100 / 70 / 80 | 21 / 15 / 15 / 14 |

- The camera declares 30 fps (SDP `a=framerate:30`, SPS VUI 30000/1001 ≈ 29.97) but delivers
  ≈ 29.8 fps: per-GOP spans between IDR timestamps are 180,000 + k × 900 ticks and never
  shorter, which is ≈ 3017 ticks per delivered frame. Per-frame jitter against host arrival
  time: stdev 48 ms, max 297 ms. 4 % of frames repeat the previous timestamp.
- RTP clock vs host `CLOCK_MONOTONIC`: slope 1.000050 (+50 ppm) over the 90 s run but only
  1.000002 (+2 ppm) over the 15 min run (§5a); the short-run figure is jitter-dominated. The
  SR-implied rate over 15 min agrees (90,000.5 Hz, +6 ppm). Budget ±100 ppm for other units.
- RTCP: Sender Report + SDES every 5.0 s (19 in 90 s). SR NTP time is sane: −12 … −93 ms
  relative to the host wall clock (the tail is the SR queued behind IDR bursts on the camera
  uplink). The SR-implied RTP rate (90,021 Hz over 89 s) disagrees with the +50 ppm above
  because the camera wall clock is stepped by NTP every minute (`NTPAdjustTime`, §7). SR
  packet/octet counters are garbage (1.6×10⁹ packets).
- ONVIF `GetSystemDateAndTime` reports UTC = true UTC **+ 1 h** while `LocalDateTime` is
  right (DST mishandled; the TZ string the camera reports pairs a fixed offset with a
  mismatched DST abbreviation and a non-POSIX week/day rule encoding, so it is not valid
  POSIX). WS-UsernameToken `Created` must be computed from the camera's clock
  (measured offset +3,600 s); `iris-tools onvif-call` does this.

Consequences for the time model (see 04): the NVR clock is the only trustworthy wall clock;
RTP timestamps give relative timing but need constant-frame-rate regularisation; RTCP SRs
are a coarse cross-check and an audio/video alignment aid, nothing more.

### 5a. Fifteen-minute run (26,789 frames, 623,596 packets, 7.86 Mbps)

| Metric | Value |
|---|---|
| Frames / keyframes | 26,789 at 29.77 fps / 454 IDR; marker on every AU; 0 sequence gaps |
| Mean sizes | IDR 556 KB, P 23.7 KB |
| Δ histogram (ticks: count) | 2700: 15,492 · 3600: 7,526 · **0: 1,607 (6 %)** · 9000: 668 · 1800: 498 · 4500: 249 · 900: 211 · 7200: 153 · 6300: 142 · 8100: 120 |
| RTP vs host monotonic | slope 1.000002 (+2 ppm): the 90 kHz clock is accurate; the ≈ 29.8 fps delivery rate is a frame-period property, not clock drift. Residual stdev 46 ms, max 366 ms |
| GOP lengths | 60 × 442; 2, 3, 4, 6, 20, 24, 25, 37, 38, 43, 51 × 1 each |
| RTCP | 181 SR + 181 SDES, every 5.0 s; SR NTP − host wall: mean −18 ms, min −141 ms, max 0 ms |
| ONVIF metadata track | 0 packets, although four `VideoMotion` events occurred (§7) |

### 5b. Concurrent sessions: the sub stream stops

Isolation runs with `iris-tools rtsp-probe` and ffprobe, outside the recorder, on five
units of this family across two firmware builds:

- The sub stream (`subtype=1`) runs clean when it is the only session on the camera.
- It stops sending 30–110 s after any second RTSP session opens on the same camera (main +
  sub, or two sub sessions). The TCP connection stays open and no RTSP error is sent; the
  camera's own `RtspSessionDisconnect` events name the sub-stream session.
- The same happens over TCP and UDP transport and on both the `proto=Onvif` and native
  paths.
- The main stream and the third stream (`subtype=2`) are unaffected: main + third ran clean
  for 130 s (3,758 third-stream frames at 5.65 Mbps).
- A 4K PTZ unit of a different family does not show the defect.

Consequence for irisd: on this family, `sub_url` names the third stream, at ≈ 5 Mbps per
camera instead of ≈ 1.3 Mbps unless the third stream is reconfigured on the camera. The
six-camera run that validated the choice is `11-multi-camera-results.md`.

## 6. Container experiment (same 30 s, 889 frames)

| Format | Bytes | Overhead vs raw | After SIGKILL mid-write |
|---|---|---|---|
| Raw Annex-B | 29,884,675 | — | readable, 301/301 received frames |
| fMP4, fragment per keyframe, `hvc1` | 29,894,925 | +10,250 B (0.034 %, ≈ 683 B per 2 s GOP incl. ftyp/moov) | readable, 241 frames (the buffered partial GOP is lost) |
| MKV | 29,892,611 | +7,936 B (0.027 %) | readable with "file ended prematurely", 240 frames |
| Plain MP4 (moov at end) | 29,894,618 | +9,943 B (0.033 %) | **unreadable**: `moov atom not found` |

A graceful SIGTERM lets ffmpeg write the moov, which made plain MP4 look fine in a first
attempt; only SIGKILL (≈ power loss) shows the real behaviour.

## 7. Event sources

Dahua CGI push feed, `GET /cgi-bin/eventManager.cgi?action=attach&codes=[All]&heartbeat=5`
(HTTP digest), works:

```
--myboundary
Content-Type: text/plain
Content-Length:9

Heartbeat
--myboundary
Content-Type: text/plain
Content-Length:135

Code=TimeChange;action=Pulse;index=0;data={
   "BeforeModifyTime" : "2026-10-31 12:00:00",
   "ModifiedTime" : "2026-10-31 12:00:00"
}
```

`Content-Length:` has no space after the colon and blank lines between parts vary; parse
defensively. Times in `data` are camera-local (values above altered). `NTPAdjustTime` and
`TimeChange` pulses arrive every minute (NTP period 1 min).
`eventManager.cgi?action=getEventIndexes&code=VideoMotion` returns `Error: No Events` when idle.

A 15-minute capture of the same feed (heartbeat 30 s) around midday caught real activity:

| Code | action | Count | Meaning |
|---|---|---|---|
| `VideoMotion` | Start / Stop | 4 / 3 | motion detection state (one episode still open at the end) |
| `VideoMotionInfo` | State | 201 | pulses while motion is active (no `data=` on this firmware) |
| `NewFile` | Pulse | 17 | the camera's own SD-card motion clips rolling; each coincides with a forced IDR (§3) |
| `NTPAdjustTime` / `TimeChange` | Pulse | 15 / 15 | once per minute |
| `RtspSessionDisconnect` | Start / Stop | 1 / 1 | emitted when the SIGKILLed ffmpeg of §6 vanished without TEARDOWN |

So the CGI feed is validated for motion on this firmware. The ONVIF metadata track stayed
silent through all four motion episodes, so on this firmware it apparently carries only IVS
rule results (tripwire/intrusion, both disabled here), not plain motion.

ONVIF `GetEventProperties` lists 23 topics, including `tns1:VideoSource/MotionAlarm`,
`tns1:RuleEngine/CellMotionDetector/Motion`, `tns1:RuleEngine/FieldDetector/ObjectsInside`,
`tns1:RuleEngine/LineDetector/Crossed`, `tns1:RuleEngine/ObjectDetector/Face`,
`tns1:RuleEngine/TamperDetector/Tamper`, `tns1:AudioAnalytics/Audio/DetectedSound`,
`tns1:VideoSource/GlobalSceneChange/AnalyticsService`.

The ONVIF metadata RTP track (`vnd.onvif.metadata`, trackID=4) SETUPs fine but delivered 0
packets in 90 s and again 0 packets in 15 min that included four motion episodes.

ONVIF over HTTPS: a unit of this platform with HTTPS enabled advertises every ONVIF service with an
`https` address, in WS-Discovery and in `GetCapabilities` alike, yet its port 443 answers an ONVIF
request inside the TLS session with plaintext HTTP, so clients see a TLS record error (OpenSSL
"wrong version number", rustls "InvalidContentType") after sending the request, while a plain `GET`
of its web interface over the same port works and port 80 answers the same ONVIF requests correctly.
Its CGI over HTTPS is unaffected. Units without HTTPS enabled keep port 443 closed (measured
2026-11-08).

## 8. Storage arithmetic at measured rates

| Stream | Mbps | GB/day/camera | GB/day for 6 cameras |
|---|---|---|---|
| main 4K H.265 | 8.03 | 86.7 | 520 |
| sub D1 H.265 | 1.28 | 13.8 | 83 |
| audio L16/64000 | 1.03 | 11.1 | 67 |
| third 1080p H.265 | 6.14 | 66.3 | 398 |

Six cameras, main + sub, no audio: 603 GB/day, so a 10 TB disk holds 16 days. Raw PCM
audio adds 11 %; AAC at 64 kbps adds 0.7 GB/day/camera.

## 9. Implications for irisd

1. `hvc1` packaging is straightforward: single-slice pictures, IDR-only random access,
   stable in-band parameter sets (the writer compares each fragment's sample entry with the
   open file's; a change seals the file).
2. A fragment per GOP is 2 MB at 4K every 2 s. Container overhead is negligible in every
   format, so the storage decision is about crash safety, zero-copy serving, and tool
   compatibility, not bytes.
3. RTP timestamps must be regularised; durations must never be used verbatim (6 % of frames
   carry a 0 ms duration and 100 ms gaps are routine; players stutter and some muxers reject
   the stream). The clock is accurate (+2 ppm) but the declared 30 fps is 0.6 % off the
   delivered ≈ 29.8 fps, so the regulariser must measure the rate rather than trust the
   declaration, and a fragment cap equal to the nominal GOP length needs slack.
4. GOP length is not a constant: the camera forces IDRs when its own clip recording starts.
5. Camera wall clocks are untrustworthy (ONVIF UTC off by 1 h). Use NVR receipt time; use
   SRs only as a cross-check.
6. irisd sets up only the video track (04 R7). An audio path would need AAC: L16 costs
   1 Mbps and HLS cannot play it.
7. Event ingestion can start with the CGI feed; ONVIF PullPoint and the metadata track exist
   on this firmware for the vendor-neutral path.
8. The camera uplink behaves like 100 Mbps (a 650 KB IDR takes ≈ 50 ms to arrive). Six such
   cameras need a gigabit NVR uplink, nothing more.
9. ONVIF endpoints of an HTTPS-enabled unit are addressed over HTTP (§7): irisd never downgrades
   an endpoint on its own, so a lookup or pull-point subscription that names the HTTPS port fails
   and is made again with `https = false`.

## 10. Open validation items

- Motion is validated on the CGI feed (§7). Open: ONVIF PullPoint delivery of the same
  episodes, whether enabling an IVS rule makes the metadata track emit, and event-to-video
  latency (timestamp the feed lines against the video wall clock).
- An in-band parameter-set change while recording, when an operator changes the main-stream
  resolution: whether the RTSP session survives (irisd never changes camera settings).
- Stall the TCP reader deliberately to measure how long the camera tolerates backpressure
  before dropping the session (sizes the writer queue).
- Not measured: the maximum number of concurrent RTSP sessions. `AGENTS.md` rule 6 caps tests
  at three sessions per camera, and §5b shows a second session already stops the sub stream on
  this family.
- AAC packetisation (`MPEG4-GENERIC` AU headers) on a stream that carries AAC, such as the
  third stream.

## 11. Reproduction

```sh
CAM=<camera-address>
# identity, encoder, detection, NTP; curl prompts for the password, which stays out of shell
# history and process listings
curl -s --digest -u user "http://$CAM/cgi-bin/magicBox.cgi?action=getSystemInfo"
curl -s --digest -u user "http://$CAM/cgi-bin/configManager.cgi?action=getConfig&name=Encode"
curl -s --digest -u user "http://$CAM/cgi-bin/configManager.cgi?action=getConfig&name=MotionDetect"
# stream parameters and packet-level GOP analysis; ffmpeg's tools take the credentials only
# inside the URL ("rtsp://..." below) and print it, so prefer iris-tools rtsp-probe, further
# down, which reads the password from the environment
ffprobe -rtsp_transport tcp -i "rtsp://..." -show_streams -of json
ffprobe -v verbose -rtsp_transport tcp -i "rtsp://..." 2>&1 | grep -E '^(m=|a=)'          # SDP
ffprobe -v error -rtsp_transport tcp -i "rtsp://..." -select_streams v:0 \
  -show_entries packet=pts,dts,size,flags -of csv=p=0 -read_intervals '%+#450'
# 30 s into four containers from one session, then SIGKILL variants
ffmpeg -rtsp_transport tcp -i "rtsp://..." -map 0:v -c copy -t 30 -f hevc raw.hevc \
  -map 0:v -c copy -t 30 -movflags +frag_keyframe+empty_moov+default_base_moof -tag:v hvc1 -f mp4 frag.mp4 \
  -map 0:v -c copy -t 30 -f matroska out.mkv -map 0:v -c copy -t 30 -tag:v hvc1 -f mp4 plain.mp4
# RTP/RTCP/timing/metadata probe (iris-tools); the password comes from the environment
read -rs CAMERA_PASSWORD && export CAMERA_PASSWORD
iris-tools rtsp-probe --password-env CAMERA_PASSWORD $CAM user 90 0 probe/meta.xml
# ONVIF with WS-UsernameToken (clock-offset compensated)
iris-tools onvif-call --password-env CAMERA_PASSWORD user http://$CAM/onvif/event_service \
  '<GetEventProperties xmlns="http://www.onvif.org/ver10/events/wsdl"/>' 60000
# Dahua event feed
curl -s -g --digest -u user "http://$CAM/cgi-bin/eventManager.cgi?action=attach&codes=[All]&heartbeat=5"
```
