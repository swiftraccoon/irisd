# 13 — Audio

Recording AAC is behind a configuration switch that is off by default. §6 lists what is
built and what stays open.

## 1. What the standards require

Local text is under `docs/standards/ietf/`; clause numbers are theirs.

| Topic | Source | What it says | Consequence |
|---|---|---|---|
| AAC over RTP | RFC 3640 §3.3.6 | `mode=AAC-hbr`: one or more whole AAC frames or one fragment per packet, each with an AU header (13-bit size, 3-bit index); frames may be interleaved and "receivers MUST support de-interleaving" | The depacketizer is the RTSP library's; whether it de-interleaves is unverified |
| Frame timing | RFC 3640 §2.6, §3.2.3.2 | The RTP timestamp "MUST carry the sampling instant of the first AU"; with constant-duration AUs timing is reconstructed from the timestamp and the AU index | Frames of one stream have one duration: time is the count of frames |
| The timestamp clock | RFC 3550 §5.1 | The sampling instant "MUST be derived from a clock that increments monotonically and linearly"; for periodic packets "the nominal sampling instant ... is to be used, not a reading of the system clock" | The cameras break this (§3): their audio timestamps cannot place single frames |
| Audio with its picture | RFC 3550 §5.1, §6.4.1 | Timestamps of different media are related only through the NTP and RTP timestamp pair of each stream's RTCP sender reports | Sender reports are the standard basis for lip sync; receipt time is not |
| HLS renditions | draft-pantos-hls-rfc8216bis-22 §6.2.4 | "Matching content in Variant Streams and Renditions MUST have matching timestamps" and "matching Discontinuity Sequence Numbers"; every Media Playlist "MUST have the same Target Duration" | An audio rendition shares the video's timeline and its discontinuities |
| Apple devices | HLS Authoring Specification for Apple Devices 2.1, 2.3, 9.5, 8.2, 8.3, 8.15, 8.22, 7.3 | Audio in fMP4 or elementary; stereo AAC "MUST be provided"; "You SHOULD deliver video and audio as separate streams"; audio and video playlists "MUST" share target duration and content duration; "MUST have discontinuities at the same points in time"; segment boundaries "SHOULD" coincide (MUST on tvOS); `tfdt` consistent with the previous segment | Separate audio files fit; audio segments are cut where the video's are |
| Encoder delay | Apple QuickTime File Format, appendix G. It says that its mechanisms "are specified in ISO MPEG-4 standards (ISO/IEC 14496-12, 2008)"; that text was not read | AAC "encoders add at least 1024 samples of silence" before the sound, the priming. A track "uses an edit list to indicate the placement of the source signal in the time represented by the encoded AAC packets". A `roll` sample group states what a decoder needs first: "the typical value is `-1`, meaning one preceding AAC packet" | A recording that joins a running encoder has no priming to trim; the first frame of a run decodes imperfectly unless a `roll` group says so |
| Track timescale | ISO/IEC 14496-12 (`mdhd`) and ISO/IEC 23000-19 (CMAF) | Neither text was read (`docs/standards/README.md`) | Whether either text constrains an audio track's timescale to the sampling rate is **unverified**. FFmpeg and GStreamer decode an audio track with a 90 kHz timescale (§3) |

The Apple specification states no sampling-rate limit; Apple's guide to preparing audio says
rates "are normally 44.1 kHz or 48 kHz". On macOS 27, `AVAudioConverter` decoded 400 frames of the
test daemon's 64 kHz AAC-LC with no error (`15-apple-clients.md` §3.1, one session on 2026-11-15, a
dated observation). Whether the decoders of iOS accept it is **unverified**.

## 2. How others do it

- Moonfire NVR has no audio. Its open design issue names the two problems this document
  meets: building the sample entry, and timing, with the intention to leave audio durations
  untouched, never re-encode, and buffer audio so it "lands in the correct recording" when
  a keyframe and the audio before it arrive out of order.
- MediaMTX records AAC, Opus, G.711, and LPCM into fMP4 as received, in parts whose length
  is the recovery point; a release note records a playback fix to "adjust speed to
  compensate discrepancies between NTP and DTS".
- Frigate records through `ffmpeg -c copy` and documents re-encoding audio to AAC in go2rtc
  when a camera's audio does not play.
- Audio sender reports are measured in §5.1.

## 3. What cameras send (measured 2026-11-11)

From the `offered, not recorded` lines of running sessions and, with audio recording on, the
cadence log's audio groups of 125 frames over 188 s on test cameras; no camera
session beyond the recorder's own was opened.

Offers seen beside H.265 video: AAC (`mpeg4-generic`) at 64 kHz; L16 at 32 and at 64 kHz;
G.711 A-law at 8 kHz. One camera may offer different codecs on its two streams. Every
description also offers an ONVIF metadata stream, and none declares a channel count, which
RFC 3551 and RFC 3640 read as one channel.

| Measured on the AAC streams | Test cameras that stamp in 10 ms steps | A test camera that does not |
|---|---|---|
| Camera time per 125 frames (nominal 2,000 ms) | mean 1,999.7–1,999.9 ms; single groups 1,973–2,027 | mean 2,000.3; 1,960–2,040 |
| Step between consecutive timestamps (nominal 16 ms) | 16 ms for 60 %; the rest from −18 to +76 ms in a 10 ms pattern | pairs of −24 and +56 ms around 16 ms |
| Arrival | 37–41 % of frames in the same socket read as the one before; gaps up to 160–280 ms | 81 % in the same read; gaps up to 380 ms |

These cameras stamp audio from a clock with 10 ms steps, not from the sampling clock, so
consecutive timestamps step backwards about once a second; averaged over two seconds the rate
is the nominal one within 0.02 %. Frames arrive in bursts, behind the video's keyframes on
the same connection.

Decoders, on a 120 s export of a recording (AAC-LC, 64 kHz, mono, 124.5 kbps, in a track
with a 90 kHz timescale): `ffprobe` reads it, `ffmpeg -f null` decodes it without a warning,
and GStreamer's `qtdemux ! aacparse ! avdec_aac` decodes it. AVPlayer, Safari, and Chromium
are **unverified**.

## 4. Decisions and what settles each

| Decision | Basis | Check |
|---|---|---|
| AAC as received; L16 and G.711 are not recorded | HLS and Apple carry AAC, not PCM or G.711 in this form; the record path does not transcode | Apple 2.1–2.3; decoders above |
| One audio stream per camera, in files of its own | Apple 9.5 (separate streams); video `mdat` ranges stay contiguous (`04`) | whole-daemon test; real files decode |
| Audio from the main stream's session | One session's streams share sender reports; the main stream is the one always recorded | RFC 3550 §6.4.1 |
| Time within a run is the count of frames | RFC 3640 §2.6; measured rate within 0.02 % of nominal; timestamps unusable per frame | §3 |
| A run continues while a timestamp is within half a second of the count | Measured steps of −24 to +76 ms; packet loss or a larger step is a real break | §3; the bound itself is a choice, **unverified** against a camera that drops audio |
| The timestamp policy of the RTSP library covers video only | A backward audio step ended the whole session six times in 103 s (§7) | regression test fails on the library as published |
| 90 kHz media timescale for audio | One time model for the sidecar, catalog, playlists, and export; exact for 8, 16, 32, 48, 64, 96 kHz, refused for the 44.1 kHz family | FFmpeg and GStreamer decode. On macOS 27, AVPlayer played a multivariant playlist with its audio track enabled, and AVFoundation opened an export with its audio at the 90 kHz timescale (`15-apple-clients.md` §3.2, dated observations of 2026-11-15). The CMAF text and iOS are **unverified** |

## 5. Audio against its picture

### 5.1 Measured (2026-11-11, 20 min, cadence log with sender reports)

Each stream's position on a clock was estimated two ways: from its RTCP sender reports (the
camera's clock, RFC 3550 §6.4.1) and from the least receipt delay of each two-second group
(the recorder's clock, the estimate the video path uses, `04` §7).

| | Test cameras whose sender reports the library accepts | A test camera whose reports it rejects |
|---|---|---|
| Sender reports | both streams, every 5 s; a stream's position by single reports scatters 28–96 ms (standard deviation 5–13 ms) | none the RTSP library accepts (`11-multi-camera-results.md`) |
| Least delay per group | scatters 10–25 ms (standard deviation 1.5–4.7 ms) | 37–94 ms (standard deviation 7–8 ms) |
| Audio against video, sender reports less least delay | −14.7 to +0.3 ms | not comparable |
| Audio frames per second, by count | 62.4999–62.5003 against a nominal 62.5 | 62.5015 |
| Audio count against the video timestamp clock | within 5 ppm, at the resolution of 20 minutes | within 6 ppm |

Two results. Placing audio by the least receipt delay of a group agrees with the sender
reports within 15 ms, is steadier than single reports, and works where a camera's reports are
unusable; one frame's receipt time, which the built code uses, is up to 380 ms late (§3).
And the audio sampling clock and the video timestamp clock of a camera differ by a few parts
per million: a millisecond or two in the 20 to 60 minutes of a file, and tens of
milliseconds in a day-long run. §5.1a measures a run of many
hours: the clocks are closer than that, and the drift has another cause.

### 5.1a The many-hour run (2026-11-11, 5.8 h)

| | Result |
|---|---|
| A camera's audio and video timestamp clocks against the recorder's | the two within 0.2 ppm of each other on every camera; cameras between 9.8 ppm slow and 8.5 ppm fast of the recorder |
| Audio frames per second, by count | 62.4995–62.5004 |
| Drift (a fragment's placement less its count), test cameras that change mode | within 7 ms for the first four hours; then, within an hour, 40–90 ms as those cameras changed to the mode in which they deliver fewer frames than they declare (`11`) |
| Drift, a test camera that does not | 72 ms at the run's second fragment, 70–81 ms from there on |

So the drift is not a difference of clocks, which the first two rows put under a millisecond
an hour. It has two causes, one an error of placement and one of the measure:

- **A run placed too early.** A run is placed when its first fragment is cut, against the
  wall time of the video fragment it accompanies. That is the first video fragment of a
  session, whose wall time comes from the video's least-delay estimate after a few frames;
  two seconds later the estimate had moved 72 ms on one camera, and the audio kept the
  first value for the whole run. Placed against the video fragment that closed last, which
  the cut has in hand (design item 1), the same camera's first fragments of a session show
  0 to 3 ms.
- **The video's estimate moving, not the recording.** The drift logged is the audio's
  placement, recomputed for each fragment from the least receipt delays, less its count.
  Where a camera sends sender reports for both streams they say where the audio is against
  the video on the camera's own clock (RFC 3550 §6.4.1), whatever either stream's delay.
  Set against them, through the fragments' stored RTP timestamps and decode times, the
  recording of one such camera has its audio 10 ms before the camera's place for it
  (median over a file) early in the run, after four and a half hours, and after the change
  of mode, when its logged drift was near 90 ms. In that mode the least delay of the
  video's frames falls by tens of milliseconds, in steps, as frames with less delay
  arrive, and the logged drift is that estimate moving. A run restarted on it would have
  moved the audio 80 ms off. In a run started in that mode, the audio of such cameras is
  between 30 ms before and 14 ms after the camera's place (a median for each camera): what
  the least-delay placement is good for at a run's start.

So the audio stays counted from its run's start, and the logged drift decides nothing.
What remains is the difference of a camera's two clocks, under 0.2 ppm here: 45 ms in
about sixty hours of one run.

### 5.2 Design

1. **Placement.** An audio run is placed against its video run by the least receipt delay
   of both, estimated over groups as the video path does, not by one frame's receipt.
   Sender reports stay a cross-check. The video's side of the placement is the video
   fragment that closed last, not the one the audio is cut for: a session's first video
   fragment takes its wall time before the video's estimate has settled (§5.1a).
2. **One timeline.** Audio decode times are expressed on the video run's timeline: the
   first audio frame of a run takes the video media time its placement gives it, and from
   there the count of frames carries it. HLS §6.2.4 requires matching timestamps, and the
   serving path hands out byte ranges of the files, so the files themselves must match.
3. **Runs.** An audio run starts and ends with its video run, and a break in either starts
   a run in both, so the two renditions have their discontinuities at the same points
   (HLS §6.2.4, Apple 8.15).
4. **Segments.** An audio fragment is closed when the video fragment it accompanies is
   closed and holds the frames that start inside that fragment's span, so audio and video
   segments start within one audio frame of each other (Apple 8.22) and cover the same
   duration within a frame (Apple 8.3).
5. **Drift.** Frame durations are never changed (`04` §7.5). A run's audio is counted from
   its start, so it parts from the video as the camera's two clocks part (under 0.2 ppm
   measured, §5.1a: 60 ms in about 83 hours of one run). Neither of two measures of it holds,
   and the rule of §5.3 acts on a third:
   - The difference between a fragment's placement and its count follows the video's delay
     estimate, not the recording (§5.1a).
   - The audio's own timestamps against its count are not a clock to measure a few parts
     per million against. On a test camera of one behaviour the median over a minute's
     fragments wanders 40 to 110 ms from one minute to the next for hours at a time, with
     no change in its frame count (2026-11-11). A rule that ends a run when that median
     has moved 60 ms from the run's first minute ended 33 runs of that camera in under
     three hours, each a discontinuity in its audio and video. Two hours of recorded
     fragments do not show the wander: over them the same median stays within 30 ms.

   Without the steps of §5.3, what bounds the error is the continuity rule: a run ends when one
   frame's timestamp is half a second from the count, which is no bound for a run of days. Sender
   reports read fragment by fragment give no tighter one (ten hours of one run, 2026-11-11). A test
   camera whose audio timestamps wander sends none on any stream. On the test cameras that send
   them, the audio's epoch less the video's, as a median over ten minutes, moves in steps of 10 ms:
   by nothing on one camera and by up to 70 ms on another, which is coarser than those cameras'
   timestamps against their count. Ending a run would also cost what it buys: by the median of a
   file a run is placed within 30 ms of the camera's place for it (§5.1a), which is the drift of
   about forty hours at 0.2 ppm, and by the reading by pairs of sender reports one run stands 56 ms
   off (§5.3.4). How AVPlayer treats a discontinuity that moves audio by tens of milliseconds is
   **unverified**.

   A measure that may bound it is the least of a window. A camera stamps a frame when it
   packs it, late by a varying time, so the frame of a window that is stamped earliest
   against the count is the one that waited least. From the first frame of each recorded
   fragment over the last four hours of a seventeen-hour run (2026-11-12, daylight), in
   ten-minute windows against the first: the least moves between −6 and +10 ms on every
   test camera, and between −10 and +10 ms, in steps of 10 ms, on the camera whose median
   moves 0 to 30 ms in those hours; the greatest moves between −130 and +80 ms. Through
   an evening of that camera (2026-11-12, 27 windows) the least moves between +10 and
   −30 ms, and by −100 ms in one window, while on every test camera that sends sender
   reports it stays within 3 ms (`14-drift.md` §3). The daemon logs the measure for every
   audio stream (`audio timestamps against the count`: over every frame of each ten
   minutes of a run, the least and the greatest distance of a timestamp from the count,
   and how far the least has moved since the run's first window). The log decides nothing.
   `14-drift.md` sets this measure beside the others of a recording's clocks: the one that
   reads the sound against the picture directly, by the sender reports of both streams and
   by when the frames of each arrive, and the checks of each against a clock fault of
   known size.

   A drift that the measure shows is taken out one frame at a time (§5.3).
6. **The first frame of a run** carries a `roll` sample group entry with distance −1 if a
   player is found to need it; **unverified**.

Retention of audio follows its camera's main stream.

### 5.3 Taking a drift out

The measures of `14-drift.md` §3 show how far the recorded sound stands from the recorded
picture. This section fixes what moves it back. A camera's `audio_steps` setting, off unless
set, decides whether the rule's steps are made on its recording or only said; §5.3.6 gives
the order of building and what each stage showed.

#### 5.3.1 What can move the sound

| Way | What it does | Taken |
|---|---|---|
| Re-time the frames | each frame lasts a little more or less | no: a frame is 1,024 samples at the sampling rate, and a duration that says otherwise is false to every player (`04` §7.5) |
| End the run in both streams | a discontinuity in sound and picture, and the audio placed again | no: a placement is good to 30 ms by the median of a file (§5.1a) and stood 56 ms off in one run (§5.3.4), no better than what it would remove, and the picture pays with a discontinuity |
| Leave a gap in the audio's decode times | the next fragment starts later than the last ended | no: it moves the sound one way only, since an overlap has no meaning; what AVPlayer plays over a gap is **unverified**; hls.js fills such a gap with frames (§5.3.3), which is the next way done by the player |
| One frame less, or one frame twice | every later frame is one frame earlier, or later; decode times stay a count without a gap | yes |

A frame less or twice changes no timestamp: the audio's decode times go on as a count. The
HLS draft requires `EXT-X-DISCONTINUITY` for a change of file format, of tracks, or of
"timestamp sequence", and says it should be present for a change of "encoding parameters"
or "encoding sequence" (§4.4.4.3). The stream stays one encoding with one frame absent or
twice, so no discontinuity is declared; a discontinuity in the audio would owe one in the
video (§6.2.4). That reading is **unverified** on AVPlayer.

#### 5.3.2 What a frame less or twice does to the sound (2026-11-12)

The frames of a recording of a test camera (AAC-LC, 64 kHz, one channel), 6,000 of them,
written as ADTS three times: as recorded, with eleven frames left out, and with eleven
frames twice. Each was decoded by four decoders (FFmpeg's own, FFmpeg with libfdk-aac, and
GStreamer's `faad` and `fdkaacdec`), and the decoded sound set against the decoded
original on both sides of every splice.

| | A frame less | A frame twice |
|---|---|---|
| Decoded length | 1,024 samples shorter a frame, exactly | 1,024 longer, exactly |
| Samples at a splice that are neither the sound before it nor the sound after | 127 or 128 (2 ms) | 126 or 128 |
| Greatest step from one sample to the next in that stretch, against the greatest in the fourteen frames around | below it at ten splices of eleven, 1.2 times it at one | below it at all eleven |
| Decoder errors | none | none |

The four decoders agree in every figure. The same splices in a stream from FFmpeg's AAC
encoder (pink noise at the same rate) disturb 1,019 to 1,024 samples, a whole frame, with
steps at most 1.07 times those around: that encoder overlaps its frames by a frame, the
test camera's by an eighth of one. So a correction costs 16 ms of sound left out or said
twice and a transition of 2 to 16 ms with no step harder than the sound's own. How it
sounds is **unverified**: no listening test was made.

#### 5.3.3 Prior art

hls.js, remuxing AAC beside video, holds the audio to its place by whole frames
(`src/remux/mp4-remuxer.ts`): "If the audio track is missing samples, the frames seem to
get "left-shifted" within the resulting mp4 segment, causing sync issues and leaving gaps
at the end of the audio segment. In an effort to prevent this from happening, we inject
frames here where there are gaps. When possible, we inject a silent frame; when that's not
possible, we duplicate the last" frame. It does so once the audio is `maxAudioFramesDrift`
frames from its place. Apple's guidance for fMP4 gives the audio's place to the fragment's
decode time, "to permit the alignment of the audio timeline with the video timeline"
(Preparing Audio for HTTP Live Streaming).

A frame said twice is chosen over a silent one: a silent frame must be built for the
stream's coding, which hls.js does from a table by codec and channel count, falling back
to the duplicate; a duplicate needs nothing of the bitstream, and the transition measured
above is the same length either way round.

#### 5.3.4 The rule

- **The quantity.** *E* is how late the recording has the sound against the picture over a
  decision window (`14-drift.md` §3, measure 5), negative when it has the sound early. For
  a camera that sends sender reports for both streams it is the reading by those reports.
  A camera that sends none has two readings, by arrival and by the two streams' timestamps
  alone, and each shows how far the sound has moved and not where the run was placed. Each
  is taken as the median of its last three windows less the median of the run's first three,
  and *E* is the lesser of the two where they stand on the same side of zero, and zero
  where they do not: the reading by arrival alone moves where a camera changes mode, and
  the reading by timestamps alone where a camera stamps frames early (`14-drift.md` §3).
  What a camera does in a run's first two windows is in where the run started, by either
  reading. A run is judged the one way or the other from its first window on.
- **The step.** One frame, *F* (16 ms at 64 kHz), toward zero. With the sound late the next
  frame received is not recorded; with the sound early it is recorded twice. The count that
  gives decode times moves by a frame. The count that the camera's timestamps are held
  against, for the continuity rule and for measure 3, does not, so measure 3 goes on
  reading the camera and measure 5 reads the result.
- **When.** When *E* is past ¾ *F* on the same side in two decision windows running. After
  a step the next window is not judged, since it spans the step. A step of one frame
  leaves *E* within ¼ *F* of zero on the other side at most, and a step of several, taken to
  the nearest frame, within ½ *F*, so a second step the other way needs the measure to be
  wrong by a quarter of a frame or more twice running. A run without sender reports is judged from
  its third window on and steps as soon as *E* is past ¾ *F*, which two of its last three
  windows have then been; after a step three windows fill the median again.
- **A step of a run without sender reports** may be of up to three frames at once, since
  what it takes out is sound a camera left out at once (below).
- **A run's first step** may be of up to three frames at once where the run is judged by
  sender reports, because it takes out where the run was placed and not a drift. By the
  median of a file, runs were placed within 30 ms (§5.1a); by the reading by pairs, the
  runs of 2026-11-13 stand from 9 ms early to 21 ms late, and one run of one test camera
  52 to 58 ms late (`14-drift.md` §3).
- **As frames are received.** A step is made on a frame as it arrives, before it is held for
  its fragment or published, so the live ring carries what is recorded (§10.3). A step of
  several frames takes the frames received next, one after another: each is left out, or
  each is recorded twice, the second record a frame after the first. Frames left out in a
  row make one splice of the kind measured in §5.3.2, and frames recorded twice in a row
  one such splice each.
- **A run that is placed anew holds no step.** At a break of the audio and at a run the
  video starts, the audio is placed by its frames' receipt as at a session's start. What
  is left of a step is not made, a second record of a frame still held is dropped, and
  every frame received from there on is recorded where its receipt places it; frames held
  from before frames that were left out stand that much late in the run that starts, whose
  fragment with the frame after them keeps the mark.
- **The bound.** A step is made once *E* has stood past ¾ *F* for two windows, so *E* stays within ¾
  *F*, what the sound moves in the windows until the step, and what a window's reading wanders: 12
  ms and those at 64 kHz. A stream of longer frames is held more coarsely: 16 ms at 48 kHz, 96 ms at
  8 kHz.
- **When the rule stops.** A drift does not turn round within hours, so a step the other
  way within six windows of the last, or more than four steps in an hour, means the
  measure is at fault and not the recording. The first step of a run judged by sender
  reports is not among the four; every step of a run without them is. The rule then makes
  no further step in that run, says so once, and the measures go on.
- **The reading by arrival is no check on the reading by reports.** It moves by 11 to
  65 ms and stays there where a test camera changes mode, and by up to 34 ms from one
  window to the next before that (`14-drift.md` §3). A rule that also stopped when the two
  readings had moved more than a frame apart in two windows running stopped on every test
  camera that sends reports within eleven hours of one run, and on most of them within two
  hours of the next (2026-11-13).
- **Sound a camera leaves out.** A test camera that sends no reports stepped its audio by
  0.1 s twice in a day (2026-11-13). The second step is sound left out, by a check of the
  sound, and its recording has the sound that much early from there on; what the first was
  is **unverified** (`14-drift.md` §5). A rule that judged that camera by arrival alone, a
  frame a step,
  named seven frames in 3.2 hours after the first and stopped for a step due the other
  way, and in another run named a frame where nothing but the reading had moved. The rule
  as above, over that day's logged windows of both readings: no step in the seven hours
  before the first or in the two runs without one; after each, three frames as the second
  window after it closes and three more half an hour later; and one more frame 2.7 hours
  after those of the first. A camera that sends every later frame 0.1 s later instead
  moves both readings the same way, and the step would be wrong; nothing in the stream
  tells the two apart (`14-drift.md` §5).

  On the service, saying only (2026-11-14, 24 hours), with a run's start taken as the
  median of its first six windows and the reading by timestamps taken by the least of the
  audio's: that camera stepped by 0.1 s five times, in two audio runs (`14-drift.md` §5).
  The first step fell in the first run's first half hour and went into the median of six,
  so the rule named nothing for it; the median of the first three stands 21 ms from where
  the run began. For the second the rule named three frames as the second window after it
  closed and three more half an hour later. It named one frame twice besides, 1.3 and
  14.4 hours in, each for a reading 12 to 15 ms off. The third step ended the run by the
  continuity rule. In the run after, the fourth moved the reading by arrival by 0.1 s and
  the reading by timestamps by 20 ms at most through the ten windows to the fifth: frames
  that the camera stamps 0.1 s early held the least, which is why that reading takes the
  median (`14-drift.md` §3). The rule named one frame for it. At the fifth the least moved
  by 0.2 s in one window, and the rule named three frames in each of four windows half an
  hour apart: thirteen frames in that run, 208 ms, for a sound 0.2 s early, the last of
  them 1.7 hours after the fifth step and 3.4 hours after the fourth. With three windows
  and the median, on the service: **unverified**.

The decision window is the measure's, ten minutes. What it needs of the measure is a
reading that does not stand a quarter of a frame off in two windows running: a step
leaves *E* within ¼ *F* of zero, or ½ *F* where it takes several frames, and the threshold
is ¾ *F*. The reading by pairs of reports stands at
most 6 ms off its run's median over 15 hours of the test cameras and a change of mode; two
other ways to read the reports stand up to 37 and 39 ms off (`14-drift.md` §3). The two
runs of those hours, the second begun 12.7 hours after the first, read 0.5 to 14 ms apart
on a camera: the difference is where each run was placed, which a run's first step takes
out. Through 24 hours of one run and both changes of mode (2026-11-14, 143 windows) the
reading spans 4 to 7 ms on every one of those cameras.

#### 5.3.5 What records a step

A log line at `info` for every step (`a step of the audio is made`), with the stream, the
direction, the frames, and *E* before it; counts of the frames left out and of the frames
recorded twice in the stream's statistics (`StreamStats::sound_steps_made`); and two flag
bits of the sidecar record of the fragment that holds the step (`04` §5): one on a fragment
with a sample that follows frames left out, one on a fragment that holds a second record
of a frame. `irisd clocks` counts the marked fragments of each window, so a reader of the
files can tell a step from a fault. A sidecar rebuilt from a scan alone loses the marks, as
it loses the camera's timestamps. Where the rule only says what it would do, the line
reads `a step of the audio would be made`, no flag is set, and nothing counts as made. The
frames the rule names are counted either way (`StreamStats::sound_steps_said`).

#### 5.3.6 The order of building

1. The rule computes *E*, decides, and says what it would do, and makes no step: it counts
   what it has said against the measure, as if each step had been made
   (`crates/iris-ingest/src/steer.rs`). Against the replay server's camera
   (`crates/iris-ingest/tests/replay_server.rs`): with a sample clock 5,000 ppm fast and
   sender reports it names two frames in the eight seconds after the first window; with
   the same clock and no audio reports, three to five frames within twenty seconds; with
   112 ms of sound left out and no audio reports, three frames; and none with exact
   clocks. It
   runs on the service through a day and both changes of mode. It passes when, on every
   test camera that sends reports, a run's first step would come within its first hour, no
   more than one step a day would follow, and none would turn round. On a camera that
   leaves sound out the steps follow what it leaves out, 0.1 s being seven frames: there
   it passes when it names frames only after a step of the camera's that both readings
   show, has named what was left out to within a frame two hours after, and none turns
   round.

   Two runs of 12.2 and 3.3 hours on the service (2026-11-13), the first through one
   change of mode, with the reports read by each stream's lower tenth and the rule also
   stopping on readings apart: on most of the test cameras that send reports the rule
   named one or two frames in a run's second or third window, and on one more a frame
   1.8 hours into the first run. It then stopped for the readings apart, on every such
   camera in the first run and on most in the second. From the reports those runs logged
   one by one, the reading by pairs and the rule without that stop give: one frame in the
   run's second window on every camera placed more than ¾ *F* off, no step after it, and
   none turning round. On the daemon, over 5.8 hours of a third run through the other
   change of mode (2026-11-13): three frames in the second window on the camera placed
   56 ms late, one
   on a camera placed 13 ms late, and no step after on any camera that sends reports.

   Through 24.0 hours of one run and both changes of mode (2026-11-14, 143 windows): one frame in
   the second window on the camera placed 16 ms late, no step after on any camera that sends
   reports, and none on the others, which the run placed from 4 ms early to 10 ms late. Those
   cameras pass. On a camera that sends no reports the rule that takes a run's start over six
   windows and reads the timestamps by their least does not: it named nothing for one step of the
   camera's, a frame twice where the camera had not stepped, and what the camera left out in its
   second run only 3.4 hours after (§5.3.4). With three windows and the median that camera's day is
   **unverified**.
2. The step itself, behind a setting of each camera that is off by default, with a recording
   of the replay server's camera under a fast sample clock held within the bound, the live
   ring equal to the recording, and files with steps decoded by FFmpeg and by the viewer.

   Built: `audio_steps` of a camera (`crates/iris-ingest/src/audio.rs`). The setting makes the steps
   of a run whichever way it is judged; the judging of a run without sender reports has not passed
   step 1. Against the replay server's camera over twenty windows of two seconds
   (`crates/iris-ingest/tests/replay_server.rs`, one run of 2026-11-15): with a sample clock 2,000
   ppm fast, which moves the sound 80 ms later over them, five frames are left out and the last
   window reads 17 ms late; with the clock as slow, four frames are recorded twice and it reads 12
   ms early. A rule that takes its steps off a place that holds them ends those runs 49 ms late and
   43 ms early. A step is made when the reading has stood past ¾ *F* for two windows, by which a
   drift of 4 ms a window has it at 20 ms. In both recordings the decode times stay a count through
   every step, each fragment that holds a step is marked and no other, and the live ring carried the
   frames as recorded.

   On the service (2026-11-15), a run placed a test camera 21 ms late, and a frame was left out as
   the second window closed. Its file: the sidecar marks one fragment of 652, `irisd clocks --audio`
   counts it, no fragment's decode time leaves the count, and the file holds one frame less than 125
   a fragment. FFmpeg decodes it without a warning, and GStreamer's `qtdemux ! decodebin3`, the
   viewer's chain, decodes the same number of samples. The greatest step from one sample to the next
   in the marked fragment is 64, between the 43 to 113 of the four fragments on either side. How it
   sounds is **unverified**, and so is a day of steps made.
3. AVPlayer on a playlist whose audio holds a step: **unverified**.

## 6. What is built, and where it disagrees with the above

Built, behind `audio` in a camera's `record` list (off by default): the init segment of a
sound track (`iris_mp4::init`), the session's AAC stream assembled beside its video
(`crates/iris-ingest/README.md`), and recording through the daemon with a segment listing
(`crates/irisd/tests/recording.rs`). Of serving (§8), the multivariant VOD playlist is built:
`GET /api/cameras/{camera}/hls/vod.m3u8` declares recorded audio as a rendition of the main
stream, and the viewer plays the main stream with its audio (`crates/iris-gui/README.md`):
run headless against a recording of a test camera with the audio discarded, its player
parses the 64 kHz audio configuration and plays without an error; no listening test was
made. An export of a camera carries the audio as a second track (§9), and the audio is a live
stream on the WebSocket that the viewer's focused view plays with its picture (§10). Not built:
audio in the low-latency playlist.

Of §5.2, items 1 to 4 are built (`crates/iris-ingest/src/audio.rs`): placement by least
receipt delay, decode times on the video run's timeline, runs tied to the video's, and
fragments cut with the video's. Checked on a recording of a test camera (2026-11-11): one
minute exported as video and as audio holds 31 fragments of each, every audio fragment
starts 10.5 ms after its video fragment, the audio decodes in FFmpeg without a warning, and
the logged drift of the first two minutes stays within 10 ms on every stream. Of item 5,
the logged drift of a fragment's placement is not the recording's (§5.1a), and nothing
restarts on it. The rule of §5.3 holds the sound to the picture by
whole frames on a camera whose `audio_steps` is set, and on any other says what step it
would make and makes none. Item 6 is not built, and it is the one open point of the audio's
recording format.

## 7. The RTSP library's timestamp policy

With the RTSP library as published, a session that carries AAC beside its video ends at the
first backward step of the audio timestamps (§3), because the library's timestamp policy
covers every stream: six session ends in 103 s on a test camera (2026-11-11), each taking
the picture with it. The vendored copy applies the policy to video only, a run is kept by
frame count, and the replay server stamps audio as measured, so the audio test fails on the
library as published.

## 8. Serving audio with its picture

A media playlist (`vod.m3u8`, `live.m3u8`) serves one stream. Audio that plays with a
picture needs a multivariant playlist; what it has to satisfy, from the HLS
draft (`docs/standards/ietf/`) and Apple's authoring specification:

| Requirement | Source | Consequence here |
|---|---|---|
| A separate audio rendition is declared with `EXT-X-MEDIA` `TYPE=AUDIO`, a `GROUP-ID`, a `NAME`, and the `URI` of its media playlist; the variant names the group in `AUDIO=` | draft §4.4.6.1, §4.4.6.2.1; Apple 8.9 | a multivariant playlist per camera and range: one variant (the video media playlist) and one audio rendition |
| `LANGUAGE` "MUST be included in every `EXT-X-MEDIA` tag that doesn't have `TYPE=VIDEO`" | Apple 8.10 | `LANGUAGE="und"` (RFC 5646), since a camera's sound has no declared language |
| The variant's `CODECS` "lists every sample format present in any Rendition in the Group" | draft §4.4.6.1.1 | `CODECS="hvc1…,mp4a.40.2"` in the daemon's playlist (`crates/iris-mp4/src/hls.rs`). The Linux viewer's local multivariant playlist (`iris_client::vod::multivariant_playlist`) declares no `CODECS` |
| `BANDWIDTH` covers every playable combination | draft §4.4.6.2.1 | video peak plus audio peak in the daemon's playlist, which also declares the sum of the two averages as `AVERAGE-BANDWIDTH`. The viewer's local playlist declares the sum of the two average rates as `BANDWIDTH`, against the MUST of RFC 8216 §4.3.4.2. The viewer plays it (§6); any other player is **unverified** |
| `CHANNELS` and `SAMPLE-RATE` are optional and only for audio | draft §4.4.6.1 | `CHANNELS` from the sample entry's channel count in the daemon's playlist; `SAMPLE-RATE` not declared. The viewer's local playlist declares no `CHANNELS` |
| A rendition meets the constraints of a variant: matching timestamps and discontinuity sequence numbers, the same target duration, the same `EXT-X-PLAYLIST-TYPE` | draft §6.2.4 | the audio playlist is built from the video playlist's segment boundaries and discontinuities, not from the audio files alone |
| The init segment has `ftyp` compatible with `iso6`, a `trak` for every `traf` with matching `track_ID`, zero durations in `mvhd` and `tkhd`, and `mvex`; every `traf` has a `tfdt` | draft §3.1.2 | met by the audio files as written (§6) |
| Segment boundaries of renditions coincide (SHOULD; MUST on tvOS) | Apple 8.22 | audio fragments are cut with the video's (§5.2), so the same grouping of fragments into segments gives boundaries within one audio frame |

What the design settles:

- Audio and video fragments do not pair one to one, and their files rotate apart; the audio
  playlist takes its segments from the video's (§8.1).
- `SAMPLE-RATE` is not declared. `CHANNELS` is: the count in the `channelcount` field of
  the first audio file's sample entry, which the RTSP library fills from the channel
  configuration of the stream's `config` (RFC 3640 §4.1). Check: the playlist of a test
  camera's one-channel audio says `CHANNELS="1"`, the count `ffprobe` reports for the file.
- A change of the video's parameter sets is a discontinuity in both playlists (§8.1).
- Whether players accept an audio rendition whose segments start up to one audio frame
  after the video's. Checked on 2026-11-11 with a multivariant playlist written by hand over
  a 30 s video export and the audio export of the same range: FFmpeg's HLS demuxer decodes
  both renditions and reports the audio starting 9.3 ms after the video, and GStreamer's
  `uridecodebin3` plays both to the end. hls.js and AVPlayer are **unverified**. The
  multivariant playlist of the running service, for a minute of a test camera, plays in
  FFmpeg with the audio starting 3.8 ms after the video on one timeline (1,860 frames, no
  error), and both media playlists list the same number of segments with the same target
  duration and no discontinuity.
- The export: one file with two tracks (§9).
- The live WebSocket and the low-latency playlist: audio fragments exist one video fragment
  late (§5.2), which a low-latency audio rendition cannot afford.

### 8.1 The audio playlist follows the video's

The audio stream's VOD playlist, grouping its own fragments three to a segment, meets the
constraints of §6.2.4 only while audio and video files rotate together and fragments pair
one to one. They do not: a video file rotates by size and restarts the grouping where the
audio's runs on, a video fragment can have no audio fragment, a change of the video's
parameter sets is a discontinuity in the video alone, and a segment of 376 audio frames
(6.016 s) rounds a target duration up where the video's 6.000 s does not. So the playlist of
an `audio` stream is built from the main stream's of the same range:

| Rule | From | Check |
|---|---|---|
| An audio media segment holds the audio fragments, contiguous in one file, that start inside one video media segment (from that segment's first wall time up to the next one's) | Apple 8.22; draft §6.2.4 "matching timestamps" | the two playlists of a range that crosses a video file rotation list segments of the same durations within one audio frame |
| `EXT-X-DISCONTINUITY` precedes an audio segment when the video playlist has one between that segment and the audio segment before it, or when the audio's sample entry changes. Audio time is the video run's (§5.2), so the audio's timeline breaks where the video's does and nowhere else | draft §6.2.4 "matching Discontinuity Sequence Numbers" | a test with a video parameter-set change; FFmpeg and GStreamer play the service's playlists |
| The target duration is the video playlist's, or the audio's longest segment rounded to the nearest second when that is larger | draft §6.2.4 "the same Target Duration"; §4.4.3.1 (a segment's duration "rounded to the nearest integer" is bounded by it) | equal `EXT-X-TARGETDURATION` in both playlists |
| An `audio` range with no main-stream recording keeps the audio's own grouping | — | — |

Built (`crates/iris-api/src/hls.rs`, `audio_vod_playlist`). Checked on 2026-11-11 against
the running service with a test camera's recordings: over 25 minutes in which the video file
rotated and the audio file did not, both playlists list 251 segments with no duration more
than one audio frame apart, and at the rotation both list the 4 s segment that ends the video
file, where the audio's own grouping would have listed 6 s and stayed a fragment off from
there. Over 36 minutes with three restarts of the recorder inside, both list 361 segments,
three discontinuities before the same segments, and the same target duration; the only
durations more than a frame apart are the segments that end at a stop of the recorder, where
the audio ends up to 52 ms before the video. FFmpeg plays the camera's multivariant playlist
across the rotation to the end without an error, and the viewer (GStreamer `hlsdemux2`) opens
75 minutes holding rotations and restarts without a warning.

Not handled: video with no audio at all between two of its discontinuities leaves the audio
playlist one discontinuity short, and a stretch of video without audio is not declared with
`EXT-X-GAP` (draft §4.4.4.7, §6.2.1), so the audio playlist is then shorter than the video's
while its media timestamps still match. Whether a player needs the gap declared is
**unverified**.

## 9. An export with sound

The export of a stream is one file with one track: stored fragments rebased onto a
continuous timeline from zero, the gaps between runs closed, with an edit list for the
range's start (`crates/iris-mp4/src/export.rs`). An export that plays with sound in any
player is one file with two tracks. What that takes, and what checks it:

| Part | Design | Check |
|---|---|---|
| Initialization | `ftyp` and one `moov` with the video `trak` as track 1 and the audio `trak` as track 2, one `trex` for each in `mvex`, built from the two sample entries. HLS draft §3.1.2: "The Movie Box MUST contain a Track Box ('trak') for every Track Fragment Box ('traf') in the fMP4 Segment, with matching track_ID." | `ffprobe` lists an `hevc` and an `aac` stream |
| Fragments | Each stored fragment stays one `moof` with one `traf`; an audio fragment's `tfhd` track id becomes 2. Sequence numbers count over both tracks | `irisd inspect --verify` reports each track, no sequence gap, every checksum; `ffmpeg` decodes both to the end without an error |
| Order | A video fragment, then the audio fragments that start inside its span | players read without seeking back |
| Timeline | A video fragment keeps the export's continuous time. An audio fragment is as far into the export as into the video: the difference of the stored decode times, which share the video run's timeline (§5.2), or of the wall times when the two disagree by more than a millisecond | no decode-time gap in either track in `irisd inspect` |
| Start | The video track carries the edit list that hides its frames before the range's start. The audio track has no such edit: its frames that start before the range does are left out, and its timeline is zero at the range's start | `ffprobe`: the audio stream starts within one audio frame after zero |
| End | The final video fragment is trimmed at the range's end; audio that starts at or after that end is left out and the fragment that crosses it is trimmed there | `ffprobe`: the audio stream's duration is the range's |
| Sync samples | Every sample of an audio fragment is flagged a sync sample: the `tfhd` default says so | `ffprobe -show_packets`: every audio packet is a key packet |

Built: `GET /api/cameras/{camera}/export.mp4` (`crates/iris-api/src/export.rs`), with the
init segment, the track move, the per-track edit lists, and the cut of a fragment's start
in `crates/iris-mp4`. `irisd inspect` reports a file with two tracks track by track.

### 9.1 What a real file showed (2026-11-11)

Twenty seconds of a test camera's recording (H.265 at 30 frames a second, AAC at 64 kHz)
were exported from the running service and read with FFmpeg 8.1 (`ffprobe`, `ffmpeg -f null`)
and GStreamer 1.28 (`qtdemux` into `avdec_h265` and `avdec_aac`). Two things
that no test against the project's own reader shows decide the design above.

**An edit list on the audio track loses sound in FFmpeg.** An export whose two tracks carry the edit
list of the range's start, a media time of 1.963 s in that export: `ffprobe` reported the audio
stream starting 0.804 s after zero: 2.760 s of sound discarded, not 1.963 s. FFmpeg does not apply
edit lists to fragmented files as edits; for an AAC track with one edit it takes the edit's media
time, a value in the track's timescale, as the number of audio samples to skip (`libavformat/mov.c`:
`sc->start_pad = start_time`, later `sti->skip_samples = sc->start_pad`). That is right only when
the timescale is the sampling rate. The audio here has the video's 90 kHz timescale (§4), so FFmpeg
skips 90,000/64,000 times too much. The stored files have no edit lists and are not affected; an
export of an `audio` stream alone with an edit list is, in the same way. With the start cut instead,
the same range exports with the audio stream starting 14.5 ms after zero and lasting 20.000 s, and
every frame from the first is decoded.

**Audio frames need the sync default of their fragment.** With the `tfhd` default that suits a group
of pictures, `ffprobe` shows 12 key packets among 1,373 audio packets, against
`04-storage-format.md`, which says every audio sample is a sync sample. A player that seeks by key
packets can then land only on fragment starts, and a fragment's start cannot be cut. The encoder
writes a sync default when every sample of a fragment is a sync sample, and all 1,250 audio packets
of the export are key packets. A fragment that flags a sample as not a sync sample cannot lose its
start; an export that starts inside one leaves that fragment out and starts its sound with the next.
Checked on a recording whose audio fragments carry the other default: an export from inside such a
fragment completes, with its sound starting 1.4 s into the range.

Both exports decode to the end in FFmpeg and in GStreamer without an error, and
`irisd inspect --verify` finds eleven fragments in each track, no gap, and every checksum.

The viewer exports a main stream through this route (`crates/iris-transport/src/export.rs`). A
minute that the viewer exported from the running service held both tracks. No date or command is
recorded for that export, so it is **unverified**. An export that ends at the present ends its sound
up to one fragment before its picture, because an audio fragment is written when the video fragment
after it closes (§5.2).

**Unverified**: ISO/IEC 14496-12 was not read, so the two-track layout rests on §3.1.2 of the HLS
draft, quoted in the table above, and on what FFmpeg and GStreamer accept. FFmpeg shifts the video's
hidden frames into view when it remuxes or plays with its tools, as it does for an export without
sound. On macOS 27, AVFoundation opened an export with two tracks and took 2112 samples (33 ms) off
the start of its sound (`15-apple-clients.md` §3.2, one file on 2026-11-15, a dated observation).
QuickTime Player, AVPlayer, iOS, and browsers are **unverified**.

## 10. Sound in the live view

### 10.1 What exists and what was measured

The video's live path: ingest publishes every video frame to a ring as it is
received, with a provisional decode time on its run's timeline; a WebSocket
(`GET /api/streams/{camera}/{kind}/live`) sends the init segment and then one fragment of
one sample for each frame, each with its decode time (`tfdt`) and wall time (`prft`); the
viewer pushes the bytes into a demuxer and renders each picture when it is decoded, with no
clock delay (`docs/design/phase2-interfaces.md`, `crates/iris-gui/src/tile.rs`).

Audio frames last 16 ms each and do not arrive 16 ms apart. Measured on 2026-11-11 from the
arrival step of every audio frame that test cameras' main-stream sessions delivered in
twelve minutes (45,000 frames of each, from the `iris_ingest::cadence` log):

| Arrival step | Cameras of one behaviour | A camera of another |
|---|---|---|
| under a millisecond (the frame came with the one before) | 36 to 42 % | 80 % |
| 40 ms or more | 12 to 14 % | 11 % |
| largest | 180 to 320 ms | 361 ms |

So sound played as it arrives would stutter: a player needs about 0.4 s of audio in hand,
and a picture shown the instant it is decoded would then lead its sound by that much.
ITU-R BT.1359-1 puts the detectability of sound after picture near 125 ms (quoted from
memory, as in the sources); 0.4 s is far past it. Sound and picture are therefore played
against one clock with the same delay, or the sound is visibly late.

### 10.2 Prior art

- go2rtc, which Frigate serves live through, sends one muxed fragmented MP4 over a
  WebSocket into a Media Source buffer, keeps a five-second window, and holds the delay by
  setting the playback rate to the seconds it is behind the live edge, never under 0.1
  (`www/video-rtc.js`): the browser's media element does the synchronising, and the player
  only steers the delay.
- Moonfire NVR's live WebSocket sends media segments of one or more video frames with
  their media time range; it has no audio (`ref/api.md`).
- RTP receivers recover the sender's clock in a jitter buffer and play both media against
  it after a configured latency (RFC 3550 §6.4.1 gives the mapping between the media).

### 10.3 Design

| Decision | Reason | Check |
|---|---|---|
| The audio of a camera is a live stream of its own, `GET /api/streams/{camera}/audio/live`, in the protocol of the video streams: the `mp4a` init segment, then one fragment for each frame | the grid's tiles play no sound, a viewer asks for sound for one camera at a time, and a client that wants both opens both | a client of a running service receives the init segment and frames, and their concatenation decodes in FFmpeg |
| A live audio frame carries the decode time the recording gives it, on the video run's timeline (§5.2), and the wall time of that position | a client places sound against picture by the decode times of the two feeds, with no second estimate of its own; the wall times agree as the recording's do (§5.1) | the decode times of live frames equal those of the same frames in the recorded file |
| Audio frames are published when their run has been placed on the video's timeline, which the close of the second video fragment after the run's start does (the first gives the span, the second cuts the audio of it); the frames before that are not sent live, and a frame received as the video starts another run keeps the time of the run that ended | an exact time later is worth more to a player than a guessed one at once; a run starts with a session, so the cost is the first seconds of sound after a reconnect | the first live frame of a session is at or after the start of its third video fragment |
| The ring of the audio stream is the kind the video streams have, with every frame a sync frame, so a client that falls behind resumes at any frame | — | the WebSocket handler's tests run against an audio ring |

Built: the session publishes placed audio frames to a ring of the camera's `audio` stream
(`crates/iris-ingest/src/session.rs`, `AudioAssembler::last_pushed`), the daemon registers
it, and the WebSocket serves it; the low-latency playlist refuses `audio`. Checked on
2026-11-11 against the running service: eight seconds of a test camera's audio feed are one
init segment and 489 fragments of one frame, contiguous in decode time, that FFmpeg decodes
without an error as 64 kHz AAC, and every one of the 489 has the bytes of the recorded
sample at the same decode time in the camera's audio file. The daemon's integration test
hears forty frames from a replayed camera and finds their decode times in the file it
recorded.

Not built: audio in the low-latency playlist (§10.5).

### 10.4 The viewer's sound

Two facts about the installed GStreamer (1.28) were checked on six seconds of a test
camera's live audio and sub-stream feeds, captured from the running service
(2026-11-11), because the design rests on them:

- Its MP4 demuxer, fed the feed's bytes, stamps each frame with the fragment's decode time
  (a frame at 23,585,436 ticks has the presentation time 262.0604 s) and starts its segment
  at the first fragment's time, so a frame's running time is its media time less the first
  frame's.
- It does not pass on the fragment's wall time: the reference timestamp it can attach to a
  buffer comes from a `cstb` box, not from `prft` (`gst/isomp4/qtdemux.c`). A viewer that
  wants the wall time reads it from the fragment itself.

The sub stream's session has a timeline of its own; only the main stream shares one with
the audio. The viewer therefore places each feed by itself, in the way the recorder places
audio against video (§5.1):

| Decision | Reason | Check |
|---|---|---|
| The focused live view plays the camera's sound when it has any, as playback does; `--mute` discards it. The grid plays none | one camera is listened to at a time | — |
| Each feed keeps the least of arrival time less media time over the last ten seconds, and a frame plays at its media time plus that least plus half a second, on the pipeline's clock | audio arrives in steps of up to 0.36 s (§10.1), so half a second holds it; the least receipt delay is the estimate that places recorded audio against video within 15 ms of the sender reports (§5.1), and both feeds reach the viewer over one path, so the same estimate there gives the same relation, on any pair of timelines | two feeds whose frames were captured together, one arriving in bursts, reach their sinks within 20 ms of each other and half a second after the least-delay arrival |
| The picture is held only once the audio feed delivers: until then, and for a camera without audio, it shows as it is decoded | no delay where nothing needs it | a camera without audio answers 404 and its picture is never held |
| The mapping is a running-time offset on the pad after the demuxer, and it moves only when a feed's estimate has moved more than 80 ms from what is applied, both feeds then together | between moves each medium plays at its own rate; a camera's clock drifts from the viewer's by parts per million, which takes tens of minutes to reach 80 ms | the offsets move once at the start and not again in a steady test |

Built: the rule is `iris_client::sound_clock` (`crates/iris-client/src/sound_clock.rs`,
`15-apple-clients.md` §5). The Linux viewer applies it to the pads after its demuxers
(`crates/iris-gui/src/sound.rs`, `tile.rs`). Two things go beyond the table. A feed's first estimate
is one frame's delay, and an error under the threshold then stays (sound 64 ms late in a test run).
So while a feed's first half second is held the mapping follows the estimate, and the threshold
applies after that. A picture sink that drops late frames shows nothing for the seconds a decoder
needs to work through the frames a feed starts with. So a late picture is shown at once, not
dropped.

Checked on 2026-11-11. In a pipeline of the real elements with two feeds of frames captured
together, on timelines 4,000 s apart, the sound sent in groups of five frames 64 ms apart
(`feeds_captured_together_reach_their_sinks_together`): in three runs every picture and
every sound frame reached its sink 520 or 521 ms after its capture, which is the 20 ms of
transit plus the half second. Against the running service, headless with the audio
discarded into a clocked sink, for sixteen seconds after the start had settled: with a test
camera's sub stream, 477 pictures and 995 audio frames, none late; with its main stream
decoded and drawn in software on the test machine under other load, 994 audio frames, none
late, and
442 pictures of which 129 were late by 11 ms on average and 39 ms at most, where the
decoder was the limit. Nothing moved the mapping after the first half second.

The branch that plays on an output was run with the output confined to a null device: the
audio sink then reported that the branch
could not hold what its latency asks for, which a queue before the sink settles, and with
the queue the view runs without a warning. A null device keeps no time, so how a real
output's clock is followed is **unverified**.

**Unverified**: the delay and the threshold are chosen from the twelve-minute measurement
and BT.1359's figures; no viewing and listening test was made.

### 10.5 Audio in the low-latency playlist (not built)

The low-latency playlist of a stream is built by one feeder from that stream's ring: a
segment for each group of pictures, cut into parts of a fixed frame count
(`crates/iris-api/src/llhls.rs`). It refuses `audio`, whose every frame is a sync frame and
would start a segment. Audio as a low-latency rendition is more than a second feeder:

- A rendition's segments and discontinuities match its variant's (draft §6.2.4), so the
  audio's segments are cut where the video's groups of pictures are, with the video's media
  sequence numbers: the audio feeder follows the video feeder, as the VOD playlist of
  `audio` follows the main stream's (§8.1).
- "Each Media Playlist MUST contain one EXT-X-RENDITION-REPORT tag for each Media Playlist
  (Rendition) in the Multivariant Playlist" other than itself (draft appendix B.1, the
  low-latency server configuration profile; §4.4.5.4), so each feeder reports the other's
  last segment and part.
- A live multivariant playlist names the two, as `vod.m3u8` of a camera does for
  recordings.
- Audio frames are published once their run is placed (§10.3) and arrive in bursts
  (§10.1), so audio parts close later than the video parts of the same time; the part
  hold-back a player needs is to be measured.

No client of the project needs it: the viewer plays live from the WebSocket
(`08-roadmap.md`), and a check of a low-latency rendition against the profile needs
`mediastreamvalidator` or AVPlayer (open items in `10`).

## Sources

- RFC 3550, RFC 3551, RFC 3640, draft-pantos-hls-rfc8216bis-22: `docs/standards/ietf/`.
- HLS Authoring Specification for Apple Devices:
  <https://developer.apple.com/documentation/http-live-streaming/hls-authoring-specification-for-apple-devices>
- Preparing audio for HTTP Live Streaming:
  <https://developer.apple.com/documentation/http-live-streaming/preparing-audio-for-http-live-streaming>
- QuickTime File Format, appendix G (audio priming) and the pages under it, read on 2026-11-18:
  <https://developer.apple.com/documentation/quicktime-file-format/appendix_g_audio_priming_handling_encoder_delay_in_aac>
- Moonfire NVR, issue 34 (audio support): <https://github.com/scottlamb/moonfire-nvr/issues/34>
- Moonfire NVR, API reference (live stream):
  <https://github.com/scottlamb/moonfire-nvr/blob/master/ref/api.md>
- go2rtc, the browser player (`www/video-rtc.js`):
  <https://github.com/AlexxIT/go2rtc/blob/master/www/video-rtc.js>
- MediaMTX, recording: <https://mediamtx.org/docs/usage/record>
- hls.js, the MP4 remuxer (`src/remux/mp4-remuxer.ts`, audio frames injected and held to the
  video): <https://github.com/video-dev/hls.js/blob/master/src/remux/mp4-remuxer.ts>
- FFmpeg, `libavformat/mov.c` at tag n8.0 (edit lists in fragmented files, `start_pad`):
  <https://github.com/FFmpeg/FFmpeg/blob/n8.0/libavformat/mov.c>
- Frigate, go2rtc troubleshooting: <https://docs.frigate.video/troubleshooting/go2rtc>
- ITU-R BT.1359-1 (relative timing of sound and vision): its detectability figures are
  quoted from memory and were not re-read.
- ISO/IEC 14496-12 and ISO/IEC 23000-19 were not read; statements about them rest on the
  secondary sources above and are marked unverified where a decision depends on them.
