# 14 — Drift: the clocks a recording rests on, and what shows them parting

A frame is right in a recording when it plays at the instant it was taken: against the frames
around it, against the sound, against the clock on the wall, and against the events. Each of
those is kept by a different clock, and no two clocks run alike. This document lists the
clocks, every pair whose parting changes what a recording or a view shows, the measure that
shows each parting, what that measure resolves, and the check of each measure against a
parting of known size. What no measure shows is listed last (§5).

Every measure here is reported. Measure 5 is also what the rule that takes a drift out of a
recording judges (`13-audio.md` §5.3); no other measure decides anything.

## 1. The clocks and the timelines

| | Clock | Runs in | The recorder reads it as |
|---|---|---|---|
| C1 | the camera's wall clock | the camera | the NTP time of its sender reports; the UTC of its ONVIF events |
| C2 | the camera's media clock | the camera | the RTP timestamps of the video, 90 kHz |
| C3 | the frame cadence | the camera's encoder | which frame slots hold a frame |
| C4 | the audio sample clock | the camera's audio converter | the count of audio frames, 1,024 samples each |
| C5 | the audio timestamp clock | the camera | the RTP timestamps of the audio |
| C6 | the recorder's realtime clock | the recorder | the receipt time of every packet |
| C7 | the recorder's monotonic clock | the recorder | the instant of every packet |
| C8 | true time | the recorder's NTP source | through C6 |

The recorder builds three timelines from them (`04-storage-format.md` §7):

- **T1**, the video timeline of a run: every frame lasts the declared period, a slot the
  camera left empty is a hold, each fragment is corrected by at most 1 % toward C2, and an
  error past 500 ms is a snap.
- **T2**, the audio on T1: the count of frames (C4), placed once a run by the least receipt
  delay.
- **T3**, the wall time of each fragment: T1 plus the run's least-delay anchor on C6, which
  `EXT-X-PROGRAM-DATE-TIME` carries to a player (HLS draft §4.4.4.6: the first sample of a
  segment, "to at least millisecond accuracy").

## 2. What the standards make of the clocks

RTP defines a timestamp as "the sampling instant of the first octet in the RTP data packet",
"derived from a clock that increments monotonically and linearly in time", and for a periodic
source "the nominal sampling instant as determined from the sampling clock is to be used, not
a reading of the system clock" (RFC 3550 §5.1). By that rule C5 is C4: an audio frame's
timestamp is its sample count.

The same clause says how two streams are related: "RTP timestamps from different media
streams may advance at different rates and usually have independent, random offsets", so
"directly comparing RTP timestamps from different media is not effective for
synchronization. Instead, for each medium the RTP timestamp is related to the sampling
instant by pairing it with a timestamp from a reference clock (wallclock)", sent in sender
reports, and "the reference clock is shared by all media to be synchronized". A report's RTP
timestamp "corresponds to the same time as the NTP timestamp", and "this correspondence may
be used for intra- and inter-media synchronization" (§6.4.1). By the standard, then, the one
thing that relates a camera's sound to its picture is the pair of sender reports.

Cameras do otherwise in two ways that matter here. Test cameras stamp audio from their media
clock, late by a varying time, so that C5 is C2 with noise and not C4 (`13-audio.md` §5.2);
and a test camera sends no sender reports for any stream. A measure that rests on one
behaviour misses the other, so each pair below has its own.

## 3. Every pair, and what shows it parting

The measures are log lines of the daemon at `info`, one a window of ten minutes for each
stream, and values of `iris_ingest::StreamStats`; the `stream` line is the per-minute
statistics line (`07-architecture.md` §9).

| | Pair | What parts it | What that does | The measure |
|---|---|---|---|---|
| 1 | C2 and T1 | stamp noise, empty slots, steps of the camera's clock | frames play off their instants | `err_ms`, `snaps`, `holds`, and `hold_s` of the `stream` line |
| 2 | C3 and C2 | slots the encoder leaves empty | fewer frames than declared | `holds`; `fps` is the rate in use |
| 3 | C4 and C5 | the sample clock against the clock that stamps | on a camera that stamps from its media clock: the sound parts from the picture | `audio timestamps against the count` |
| 4 | C5 and C2 | on a camera that stamps by the sample count: the sample clock against the media clock | the sound parts from the picture | inside measure 5, by either reading |
| 5 | T2 and T1 | 1, 3, and 4 together, and where the run was placed | what is heard against what is seen | `audio against video by sender reports`; `audio against video by arrival` |
| 6 | C2 or C5 and C6 | the camera's oscillator against the recorder's; a change in how long a frame takes to arrive | T3 and T1 differ in length over a run | `timestamps against the recorder's clock`; `cam_offset_ms` |
| 7 | C1 and C6 | a camera's wall clock set wrong | nothing in a recording: C1 is never stored | `cam_offset_ms` of the `stream` line |
| 8 | C6 and C8 | the recorder's own clock | every wall time | the host's NTP client; a step of C6 against C7 ends the session (`ClockStep`) |

**Measure 1** is the regulariser's own account: the assigned timeline less the camera's at
the last fragment, in milliseconds, with the count of snaps and of held slots. It is exact
to a tick, and its rule is checked by unit and property tests, by replays of recorded
sidecars, and by a mutation run (`10-phase2-results.md`, `11-multi-camera-results.md`).

**Measure 3** takes, over every frame of a window, the least distance of a timestamp from
where the count puts it, and reports how far that least has moved since the first window
of the audio's count (`moved_us`), with the greatest (`greatest_us`). The count starts over
at a break of the audio and not at a run the video starts. A camera stamps a frame when it packs
it, so the frame stamped earliest waited least. On the test cameras the least holds within
10 ms over four daylight hours where the greatest moves by 130 ms (`13-audio.md` §5.2). Over
27 windows of one run that pass from day into night (2026-11-12), the least of every test
camera that sends sender reports stays within 3 ms of the first window's while the greatest
lies between 10 and 190 ms; that of a test camera that sends none moves between +10 and
−30 ms in steps of 10 ms, and by −100 ms in one window. On a camera that stamps by the
sample count it reads zero whatever the clocks do.

**Measure 5** is the direct one, read three ways. The readings by sender reports and by
arrival share nothing but the recording; the reading by timestamps alone is the reports'
without the epochs, for a camera that sends none.

*By sender reports* (`audio against video by sender reports`). A report gives a stream's
epoch: the instant on the camera's clock at which the stream's timestamp is zero, the
report's NTP time less its RTP timestamp. With both epochs, the camera's instant for the
picture at a place in the recording less its instant for the sound at that place is how late
the recording has the sound (`late_us`); `moved_us` is how far that has moved since the run's
first window. It holds whichever clock stamps the audio, because a sender report reads that
clock. Three quantities enter it, and a window stands by a different value of each, by the
kind of noise each has:

- **The video's epoch less the audio's, by the value a tenth of the way up the window's
  pairs of reports.** A test camera makes a report of each stream every 5 s, the two
  together: the recorder receives them within 0.3 s of each other, and reports received
  within a second of each other are a pair. A pair gives the difference without what its
  two reports share, and they share two things. A stream's epochs scatter over a band about
  as wide as the time between two frames (33 ms for the video and 20 to 30 ms for the audio
  over twelve minutes of test cameras, 2026-11-13). And a camera's clock moves against its
  media clocks: by 130 ms over twelve hours on one test camera, and by 45 ms and back
  within ten minutes. Over 15 hours of the test cameras that send reports, in two runs
  (2026-11-13, 11,131 pairs a camera), against the lower tenth of a camera's pairs in a run:
  58 to 88 % of the pairs lie from 3 ms under it to 10 ms over; 12 to 40 % lie further
  over, up to 136 ms, the video's report late; 0.2 to 1.3 % lie more than 5 ms under, up to
  91 ms. Over the 92 windows of ten minutes, which hold a change of mode on every camera,
  the lower tenth of a window stays from 4 ms under to 6 ms over the median of its run's
  windows on every camera. The median of a window's pairs moves with its share of late
  ones and stands up to 39 ms off. A window retains at most 1,024 pairs. Past that bound it
  keeps every second, then every fourth, and so on in arrival order; the pair count logged
  is the number received. A ten-minute window of a test camera holds 120 pairs and keeps
  every one: the bound is for reports that arrive while no window closes.
- **The audio's timestamps against its decode times, by the least.** Until a step is made
  this is measure 3; a step moves the decode times and not the count (`13-audio.md` §5.3.4),
  so the reading holds the steps and measure 3 does not.
- **The video's timestamps against its decode times, by the median of the window's
  fragments.** The regulariser holds a fragment's start to its timestamp: over the 27
  windows of measure 3's run the median is zero in every window of every test camera, with
  single fragments up to 45 ms under it.

A window with fewer than eight pairs of reports is not read, and the daemon says so once
(`audio against video is not read by sender reports: too few pairs of them`).

Two other ways to read the reports give the same quantity with more of their noise in it:

- Each fragment's two first frames set against the last report of each stream, with the
  median of a window's fragments standing for it. Over the 27 windows of measure 3's run
  the median of one test camera moves by up to 39 ms from one window to the next, and its
  level by about 30 ms where the camera changes mode, while the timestamps of both streams
  at the same fragments stay within 7 ms of where they stood against the recording.
- The lower tenth of each stream's epochs over a window, and the difference of the two.
  The camera's clock moves both streams' epochs within a window and not the two lower
  tenths alike: over the 92 windows above this reading stands up to 37 ms off its run's
  median, in two windows running on one camera, where the pairs' stands 6 ms off at most.
  It reads the sound 3 to 6 ms later than the pairs' lower tenth does on every camera;
  which of the two is nearer the camera's own place for the sound is **unverified**.

Over those two runs the reading by pairs spans 2 to 7 ms within a run on every test camera
(as computed from the reports logged one by one and the windows the daemon logged with the
reading above). Between the two runs it differs by 0.5 to 14 ms on a camera: where each run
was placed (`13-audio.md` §5.3.4). As the daemon reads it, over 5.8 hours of a third run
through the other change of mode (2026-11-13, 35 windows), it spans 4 to 6 ms on every test
camera, with no step at the change. Over the three runs a camera's reading stands from
9 ms early to 21 ms late, and in one run of one camera 52 to 58 ms late throughout.

*By arrival* (`audio against video by arrival`). Over every frame of a window, the least
receipt time less decode time of the video's frames, less that of the audio's. Its value
holds how much longer the camera takes to send a picture than a sound (`apart_us`) and says
nothing by itself; how far it has moved since the run's first window (`moved_us`) is how far
the sound has moved against the picture, whatever the camera stamps and whether it reports.
Its noise is that of measure 6. Over the same 27 windows, measure 6 of the audio less
measure 6 of the video stays within 11 ms on every test camera but one and within 19 ms on it,
in steps of 10 ms, with no trend and no step at the change of mode, a camera without
sender reports among them.

The reading itself over 12.2 hours of one run and 3.3 of the next (2026-11-13): on the
test cameras that send reports it moves by up to 34 ms from one window to the next, and
where a camera changes mode it moves by 11 to 65 ms and stays there, while the reading by
reports holds, so it does not check the reading by reports to within a frame. On a test
camera that sends no reports it stays from 8 ms under to 15 ms over its first window for
seven hours and then moves by 100 ms together with measure 3 (§5).

Most of that is not the recorder's reading of its sockets every 20 ms. With every packet
read as it arrives (`ingest.read_interval_ms` and `ingest.recv_low_water_kib` at zero, the
5.8 hours of the third run, 2026-11-13; the `cpu_pct` of the daemon's `process` line reads
16 to 18 against 3): at the other change of mode the reading moves by 7 to 53 ms the other
way and stays; before it, on the test cameras that send reports, it moves by 9 to 27 ms
from one window to the next. On the camera that sends none it spans 8 ms over eleven
windows, against 19 and 20 ms over the first eleven of the two runs read every 20 ms.
Whether what is left is the camera's or the path's is **unverified**.

*By timestamps alone* (`stamps_moved_us` of the same line). The video's timestamps against
its decode times less the audio's against its own, the reading by reports without the
epochs and by the median of each: each stream's timestamps start where the camera chose, so
it says only how far the sound has moved since the run's first window. It moves with the
sound on a camera that stamps both streams by one clock and reads zero on a camera that
stamps its audio by the sample count. Frames stamped early move it and not the reading by
arrival; a change of mode moves the reading by arrival and not it.

The audio's half is a median, over a sample of at most 1,024 of a window's frames thinned
as the pairs are, because a test camera that sends no reports stamps a share of its frames
early. The least of measure 3 on it moves by 100 ms in single windows and in the first
three windows of one run (2026-11-13), where the reading by arrival holds. Over the ten
windows after one of its steps of 0.1 s (2026-11-14, the first frame of each fragment by
its sidecars), the least stays where it stood before the step in every window, while the
median stands 80 to 110 ms over that in nine windows and 10 ms in one.

Through 24.0 hours of one run of the service and both changes of mode (2026-11-14, 143
windows, 120 or 121 pairs in each), on the test cameras that send reports: the reading by
pairs spans 4 to 7 ms on every camera, moves by at most 6 ms from one window to the next,
and does not step at either change; a camera's stands from 4 ms early to 20 ms late. The
reading by arrival moves by 7 to 66 ms at the one change and by 5 to 49 ms back at the
other (the medians of the ten windows before and after). The reading by timestamps alone,
its audio's half then taken by the least, stays within 3 ms of zero.

On a camera that sends no reports, which stepped five times in that day (§5), the
reading by arrival moves with every step in the window of it. The reading by timestamps
with the least moves with two of them in their window, with one two windows late, and with
one only at the next step, ten windows after. That camera's day by the median is
**unverified**.

**Measure 6** takes, over every frame of a window, the least receipt time less timestamp and
reports how far it has moved since the session's first window (`moved_us`) and the window's
spread. Receipt is late by a varying time and never early, so the least follows the camera's
clock against the recorder's: a camera 10 ppm fast moves 6 ms a window. It also follows any
change in how long a frame takes to arrive, which it cannot tell from a step of a clock.

## 4. Each measure against a parting of known size

The replay server's synthetic camera takes clock faults (`iris_replay::server::Clocks`): a
media clock fast against true time, an audio sample clock fast against the media clock, audio
stamped by the sample clock or by the media clock, sender reports for the audio or none, and
sender reports stamped in the ways a camera's are (§3), the early ones and the clock's
movement more often than measured: the two of a pair behind their clocks together by up to
the time between two video frames, the video's by up to 6 ms more, by 60 ms more in one
pair of five, and 20 ms ahead in one of fifteen, and one pair in three with a wall time
50 ms behind. It also leaves sound out without losing a packet
(`Options::audio_leave_out`). The daemon's session records it for five windows of two
seconds, ten where the table says so, and the last window is read
(`crates/iris-ingest/tests/replay_server.rs`). Three runs of each (2026-11-13), measure 5
since the first window:

| Fault | Measure 6 | Measure 3 | Measure 5 by reports | Measure 5 by arrival |
|---|---|---|---|---|
| none | −0.1 to 0.1 ms | 0 | 0.000 to 0.001 ms (0.7 to 1.0 ms from the camera's place) | −0.5 to −0.1 ms |
| none, reports stamped as a camera's | 0.0 to 0.1 ms | 0 | 0.000 to 0.002 ms (0.3 to 1.3 ms from the camera's place) | −0.5 to 0.0 ms |
| sample clock 5,000 ppm fast, stamps from the media clock, reports stamped as a camera's | −0.1 to 0.1 ms | −40.1 ms for −40 | 35.0 ms for 35 | 36.0 ms |
| the same fault, stamps from the sample clock | −0.1 to 0.2 ms | 0 | 35.0 ms for 35 | 35.9 to 36.5 ms |
| the same fault, stamps from the media clock, no audio reports, ten windows | −0.2 to 0.1 ms | −90.3 ms for −90 | reads nothing | 89.2 to 92.8 ms |
| media clock 5,000 ppm fast | −41.8 to −40.9 ms for −40 | 0 | −0.002 to 0.002 ms | 0.5 to 1.2 ms |
| seven frames of sound left out in the seventh window, stamps from the media clock, no audio reports, ten windows | −0.1 to 0.1 ms | 112.0 ms for 112 | reads nothing | −112.1 to −111.6 ms |

The fault of the third to fifth rows is the one a camera's separate audio oscillator makes.
It shows by arrival on every kind of camera, by reports where the camera sends them,
whichever clock stamps the audio, and in measure 3 only on a camera that stamps from its
media clock.

### 4.1 The same measures from the sidecars

`irisd clocks [--audio] [--frame-samples N] [--window DURATION] SIDECAR...` reads the sidecars of
one stream and reports, for each run and each window (ten minutes unless set), how many of its
fragments are marked as holding a step of the audio, the least and the greatest of the camera's
timestamps less the recording's timeline over the first frames of the fragments, how far the least
has moved since the run's first window, and how far the wall time has moved against the timeline.
For a video stream that is measure 1 as recorded; for an audio stream, whose timeline is the frame
count, it is measure 3 from one frame in each fragment; `--bound-ms` fails a run that moved further.
It is a second path to the same quantities, from the files and not from the session. On recordings
of the test cameras (2026-11-12, three runs of four streams, 1,800 to 4,900 fragments a run): an
audio stream whose daemon watch read the least within 10 ms over an hour reads within 10 ms by its
sidecars; a video stream reads 0.0 ms in both runs recorded with a fragment's empty slots held
(`04-storage-format.md` §7.3), and 30 to 45 ms in the run before, when a stream more than 1 % slow
was re-timed.

## 5. What no measure shows

- **Where a run's audio was placed, on a camera without sender reports.** Measure 3 and
  measure 5 by arrival show movement since the run's first window, not the placement
  itself. On the test cameras that have reports a run's placement stands up to 58 ms off
  by the reading by pairs (§3), and within 30 ms by the median of a file (`13-audio.md`
  §5.1a).
- **An error that every report of a stream shares.** Measure 5 by reports takes the
  camera's reports at their word once their scatter is out; a camera whose every report
  of a stream is late by the same time is off by that time.
- **Delay inside the camera before a frame is stamped.** A camera that stamps sound and
  picture after different delays is off by their difference in every measure's zero.
- **Sound a camera leaves out against sound it sends later.** A camera that leaves 100 ms
  of sound out has moved the recorded sound against the picture; a camera that holds every
  later frame 100 ms longer before it stamps and sends it has not. Both read alike in
  every measure on a camera that stamps a frame as it packs it, with sender reports or
  without: a report says where the stamps stand on the camera's clock and not when the
  sound was taken. The sound itself tells them apart.

  A test camera that sends no reports did the one or the other twice in a day
  (2026-11-13), 7.3 hours into one run and 1.9 hours into another. The least of measure 3
  rose by 100 ms and stayed (−10 to −30 ms before, 50 to 90 ms after), inside one fragment
  by its sidecars, and measure 5 by arrival moved by −92 to −109 ms in the same windows and
  stayed. The video's measures show nothing then, and the audio run did not break, so no
  packet was lost. The second time its recorded sound was set against another test
  camera's recording of the same sounds, by the loudness of each in steps of 5 ms matched
  over windows of two minutes. Through the 40 minutes before, the one stands −13 to 22 ms
  from the other, moving 5 ms in about five minutes as sample clocks of separate cameras
  do; in the six minutes after it stands 127 to 132 ms from it. The camera left 105 ms of
  sound out, and its recording has the sound that much early from there on.

  Over the 24 hours after one start of the service (2026-11-14) it stepped five times, by
  0.1 s each: 0.4 and 8.3 hours in; 18.5 hours in, where its timestamps came to half a
  second from the count and the continuity rule ended the audio run; and 0.8 and
  2.5 hours into the run after. The step at 8.3 hours, set against the same other camera's
  sound in the same way, moved its recorded sound by 115 ms (79 to 89 ms from the other
  before, 204 to 214 ms after). Through the first of those runs the recording had the
  sound 0.11 s early for eight hours and 0.22 s early for ten.
- **Events against recordings.** An event takes the recorder's time at its receipt, later
  than the instant by the camera's delay in sending it; ONVIF events carry the camera's own
  time beside it, on C1. Nothing measures the delay.
- **One camera against another.** Each camera's T3 rests on its own least delay; a
  difference between the least delay of one camera and of another is in neither's measures.
- **A view on a real output.** The viewer paces sound and picture from each feed's least
  delay (`13-audio.md` §10) and is checked in a pipeline with null sinks, not on a device
  that plays.
- **Measure 5 for a recording whose log is gone.** Its sidecars keep the first timestamp of
  every fragment of both streams, and `irisd clocks` recomputes measures 1, 3, and the
  wall-clock side of 6 from them (§4.1); the sender reports are not kept, so the sound
  against the picture by them is not recomputed.

## 6. Sources

- RFC 3550 §5.1 and §6.4.1; `draft-pantos-hls-rfc8216bis-22` §4.4.4.6
  (`docs/standards/ietf/`).
- `04-storage-format.md` §7 (the time model), `11-multi-camera-results.md` (the cameras'
  cadence), `13-audio.md` §5 (the audio's clocks and the measurements of them).
- `crates/iris-ingest/src/sync.rs` (the watches), `crates/iris-ingest/src/regularize.rs`.
- A camera's sender reports one by one: the `iris_ingest::reports` log target at `debug`
  (`crates/iris-ingest/README.md`).
