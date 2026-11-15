# 01 — Camera behaviour the design rests on

Measured on test cameras from 2026-10-31 on, with the commands in §11; nothing is quoted from
a datasheet. Which devices were measured is not part of this repository (`AGENTS.md` rule 9);
what is recorded here is behaviour a recorder has to expect from cameras of this kind.

## 1. Services and streams

A test camera serves RTSP (OPTIONS, DESCRIBE, ANNOUNCE, SETUP, PLAY, RECORD, PAUSE, TEARDOWN,
SET_PARAMETER, GET_PARAMETER), a vendor HTTP CGI interface with digest authentication, and
ONVIF (device, media, media2, events, analytics, imaging, recording, search, replay) on its
HTTP port. It offers up to three video streams, selected by a query parameter of the RTSP
URL: a main stream, a low-resolution sub stream, and a third stream between the two.

The figures below are from a main stream of 3840×2160 H.265 Main at a declared 30 fps,
constant bit rate 8,192 kbps, GOP 60, with the camera's bit-saving encoder mode off.

ffprobe: `hevc`, profile Main, level 150 (5.0), `yuv420p`, `color_range=tv`,
`has_b_frames=0`, `pts == dts` for all packets, `time_base 1/90000`. The reported
`r_frame_rate 100/1` is an artefact of the 10 ms timestamp quantisation (§5).

## 2. SDP

The description of a stream has this shape (identifiers and parameter sets left out):

```
v=0
s=Media Server
c=IN IP4 0.0.0.0
t=0 0
a=control:*
a=range:npt=now-
m=video 0 RTP/AVP 98
a=control:trackID=0
a=framerate:30.000000
a=rtpmap:98 H265/90000
a=fmtp:98 profile-id=1;sprop-sps=…;sprop-pps=…;sprop-vps=…
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
- SETUP: `Session: <id>;timeout=60`, `Transport: RTP/AVP/TCP;unicast;interleaved=0-1;ssrc=…`
  (hex ssrc). PLAY: `RTP-Info` with `seq`/`rtptime` for video and metadata tracks.
- A camera may advertise a private packetisation beside the standard one; RFC 7798 is used
  unless a client asks otherwise.
- The audio a description offers differs by camera and by stream (`13-audio.md`).

## 3. Elementary-stream structure (30 s raw capture, 889 access units)

| NAL type | Count | Mean size |
|---|---|---|
| 1 TRAIL_R | 884 | 24,005 B |
| 19 IDR_W_RADL | 15 | 675,565 B |
| 32 VPS / 33 SPS / 34 PPS | 15 each | 25 / 43 / 8 B |

- No SEI, AUD, CRA/BLA, or multi-slice pictures: every picture is exactly one NAL.
- Every IDR is preceded in-band by VPS, SPS, PPS (byte-identical across GOPs in the capture).
- GOP is 60 frames = 2.00 s (44/44 over 90 s). Over 15 min, 442 of 453 GOPs were 60 frames;
  11 were shortened (2–51 frames) by extra IDRs the camera inserts when it starts a clip of
  its own (`NewFile` events, §7). Fragmenting must therefore be IRAP-driven, never
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
| Packets per IDR / P frame | ≈ 450 / ≈ 16 |
| Audio, where a stream carries L16 at 64 kHz | 129 KB/s = 1.03 Mbps in 1,365-byte packets (the SDP is right where an ONVIF profile says otherwise) |

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
  shorter, which is ≈ 3017 ticks per delivered frame. Per-frame jitter against arrival time:
  stdev 48 ms, max 297 ms. 4 % of frames repeat the previous timestamp.
- RTP clock against the recorder's `CLOCK_MONOTONIC`: slope 1.000050 (+50 ppm) over the 90 s
  run but only 1.000002 (+2 ppm) over the 15 min run (§5a); the short-run figure is
  jitter-dominated. The SR-implied rate over 15 min agrees (90,000.5 Hz, +6 ppm). Budget
  ±100 ppm for other cameras.
- RTCP: Sender Report + SDES every 5.0 s (19 in 90 s). SR NTP time is sane: −12 … −93 ms
  relative to the recorder's wall clock (the tail is the SR queued behind IDR bursts). The
  SR-implied RTP rate (90,021 Hz over 89 s) disagrees with the +50 ppm above because the
  camera steps its wall clock at every NTP update (`NTPAdjustTime`, §7). SR packet and octet
  counters are garbage (1.6×10⁹ packets).
- ONVIF `GetSystemDateAndTime` can report UTC one hour off while `LocalDateTime` is right:
  a daylight-saving fault (the TZ string such a camera reports pairs a fixed offset with a
  mismatched DST abbreviation and a non-POSIX rule encoding, so it is not valid POSIX).
  WS-UsernameToken `Created` must be computed from the camera's clock; `iris-tools
  onvif-call` does this.

Consequences for the time model (see 04): the recorder's clock is the only trustworthy wall
clock; RTP timestamps give relative timing but need constant-frame-rate regularisation; RTCP
SRs are a coarse cross-check and an audio/video alignment aid, nothing more.

### 5a. Fifteen-minute run (26,789 frames, 623,596 packets, 7.86 Mbps)

| Metric | Value |
|---|---|
| Frames / keyframes | 26,789 at 29.77 fps / 454 IDR; marker on every AU; 0 sequence gaps |
| Mean sizes | IDR 556 KB, P 23.7 KB |
| Δ histogram (ticks: count) | 2700: 15,492 · 3600: 7,526 · **0: 1,607 (6 %)** · 9000: 668 · 1800: 498 · 4500: 249 · 900: 211 · 7200: 153 · 6300: 142 · 8100: 120 |
| RTP vs recorder monotonic | slope 1.000002 (+2 ppm): the 90 kHz clock is accurate; the ≈ 29.8 fps delivery rate is a frame-period property, not clock drift. Residual stdev 46 ms, max 366 ms |
| GOP lengths | 60 × 442; 2, 3, 4, 6, 20, 24, 25, 37, 38, 43, 51 × 1 each |
| RTCP | 181 SR + 181 SDES, every 5.0 s; SR NTP − recorder wall: mean −18 ms, min −141 ms, max 0 ms |
| ONVIF metadata track | 0 packets, although four `VideoMotion` events occurred (§7) |

### 5b. Concurrent sessions: the sub stream stops

Isolation runs with `iris-tools rtsp-probe` and ffprobe, outside the recorder, on several
cameras of one model:

- The sub stream runs clean when it is the only session on the camera.
- It stops sending 30–110 s after any second RTSP session opens on the same camera (main +
  sub, or two sub sessions). The TCP connection stays open and no RTSP error is sent; the
  camera's own `RtspSessionDisconnect` events name the sub-stream session.
- The same happens over TCP and UDP transport and on both of the camera's URL forms.
- The main stream and the third stream are unaffected.
- A camera of another model does not show the defect.

Consequence for irisd: `sub_url` may name any stream, and on such a camera it names the third
stream (`11-multi-camera-results.md`).

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

The vendor CGI push feed, `GET /cgi-bin/eventManager.cgi?action=attach&codes=[All]&heartbeat=5`
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
defensively. Times in `data` are camera-local (the values above are made up). `NTPAdjustTime`
and `TimeChange` pulses arrive at every NTP update.
`eventManager.cgi?action=getEventIndexes&code=VideoMotion` returns `Error: No Events` when idle.

Codes seen on the feed, with what each means to a recorder:

| Code | action | Meaning |
|---|---|---|
| `VideoMotion` | Start / Stop | motion detection state |
| `VideoMotionInfo` | State | pulses while motion is active (no `data=`) |
| `NewFile` | Pulse | the camera's own clip recording rolling; each coincides with a forced IDR (§3) |
| `NTPAdjustTime` / `TimeChange` | Pulse | at every NTP update |
| `RtspSessionDisconnect` | Start / Stop | emitted when a client vanishes without TEARDOWN |

So the CGI feed is validated for motion. The ONVIF metadata track stayed silent through
motion episodes, so it apparently carries only analytics rule results (tripwire, intrusion),
not plain motion.

ONVIF `GetEventProperties` lists 23 topics, including `tns1:VideoSource/MotionAlarm`,
`tns1:RuleEngine/CellMotionDetector/Motion`, `tns1:RuleEngine/FieldDetector/ObjectsInside`,
`tns1:RuleEngine/LineDetector/Crossed`, `tns1:RuleEngine/ObjectDetector/Face`,
`tns1:RuleEngine/TamperDetector/Tamper`, `tns1:AudioAnalytics/Audio/DetectedSound`,
`tns1:VideoSource/GlobalSceneChange/AnalyticsService`.

The ONVIF metadata RTP track (`vnd.onvif.metadata`) SETUPs fine but delivered 0 packets in
90 s and again 0 packets in 15 min that included motion.

ONVIF over HTTPS: a camera with HTTPS enabled advertises every ONVIF service with an `https`
address, in WS-Discovery and in `GetCapabilities` alike, yet its port 443 answers an ONVIF
request inside the TLS session with plaintext HTTP, so clients see a TLS record error
(OpenSSL "wrong version number", rustls "InvalidContentType") after sending the request,
while a plain `GET` of its web interface over the same port works and port 80 answers the
same ONVIF requests correctly. Its CGI over HTTPS is unaffected (measured 2026-11-08).

## 8. Storage arithmetic at measured rates

| Stream | Mbps | GB/day |
|---|---|---|
| H.265 at 8.03 Mbps | 8.03 | 86.7 |
| H.265 at 6.14 Mbps | 6.14 | 66.3 |
| H.265 at 1.28 Mbps | 1.28 | 13.8 |
| audio L16 at 64 kHz | 1.03 | 11.1 |
| audio AAC at 64 kbps | 0.064 | 0.7 |

Raw PCM audio adds 11 % to an 8 Mbps main stream with a 1.3 Mbps sub stream; AAC adds under
1 %.

## 9. Implications for irisd

1. `hvc1` packaging is straightforward: single-slice pictures, IDR-only random access,
   stable in-band parameter sets (the writer compares each fragment's sample entry with the
   open file's; a change seals the file).
2. A fragment per GOP is 2 MB at 8 Mbps every 2 s. Container overhead is negligible in every
   format, so the storage decision is about crash safety, zero-copy serving, and tool
   compatibility, not bytes.
3. RTP timestamps must be regularised; durations must never be used verbatim (6 % of frames
   carry a 0 ms duration and 100 ms gaps are routine; players stutter and some muxers reject
   the stream). The clock is accurate (+2 ppm) but the declared 30 fps is 0.6 % off the
   delivered ≈ 29.8 fps, so the regulariser must measure the rate rather than trust the
   declaration, and a fragment cap equal to the nominal GOP length needs slack.
4. GOP length is not a constant: a camera forces IDRs when its own clip recording starts.
5. Camera wall clocks are untrustworthy (ONVIF UTC off by 1 h). Use the recorder's receipt
   time; use SRs only as a cross-check.
6. Audio is covered in `13-audio.md`: L16 costs 1 Mbps and HLS cannot play it.
7. Event ingestion can start with the CGI feed; ONVIF PullPoint and the metadata track exist
   for the vendor-neutral path.
8. A 650 KB IDR takes ≈ 50 ms to arrive, as on a 100 Mbps link: a camera's keyframes arrive
   in bursts.
9. ONVIF endpoints of an HTTPS-enabled camera are addressed over HTTP (§7): irisd never
   downgrades an endpoint on its own, so a lookup or pull-point subscription that names the
   HTTPS port fails and is made again with `https = false`.

## 10. Open validation items

- Motion is validated on the CGI feed (§7). Open: ONVIF PullPoint delivery of the same
  episodes, whether enabling an analytics rule makes the metadata track emit, and
  event-to-video latency (timestamp the feed lines against the video wall clock).
- An in-band parameter-set change while recording, when a stream's resolution is changed on
  the camera: whether the RTSP session survives (irisd never changes camera settings).
- Stall the TCP reader deliberately to measure how long a camera tolerates backpressure
  before dropping the session (sizes the writer queue).
- Not measured: the maximum number of concurrent RTSP sessions. `AGENTS.md` rule 6 caps tests
  at three sessions per camera, and §5b shows a second session already stops a sub stream on
  one model.

## 11. Reproduction

```sh
CAM=<camera-address>
# encoder and detection configuration; curl prompts for the password, which stays out of
# shell history and process listings
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
# vendor event feed
curl -s -g --digest -u user "http://$CAM/cgi-bin/eventManager.cgi?action=attach&codes=[All]&heartbeat=5"
```
