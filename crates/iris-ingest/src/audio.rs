//! Assembles received audio frames into fragments on the video's timeline
//! (`docs/research/13-audio.md` §5.2).
//!
//! Audio frames do not depend on each other and all last the same time, so
//! every sample is a sync sample and time within a run is the count of
//! frames. The camera's timestamps only tell whether the run goes on: cameras
//! stamp audio frames a fraction of a frame early or late, even backwards, so
//! a frame continues the run while its timestamp is within half a second of
//! where the count puts it.
//!
//! An audio run belongs to a video run. Its first frame takes the video media
//! time at which it was received, by the least receipt delay of the frames
//! (the estimate the video path uses for its own wall times), and the count
//! carries the rest. A fragment is the frames that start inside one video
//! fragment's span, so the two streams are cut at the same points within a
//! frame, and a run that starts in the video starts one in the audio.
//!
//! A step moves the sound against the picture by whole frames (§5.3): frames
//! are left out of the recording or recorded twice as they arrive, so the
//! decode times stay a count without a gap while every later frame stands
//! that much earlier or later. The count the camera's timestamps are held
//! against goes on with every frame received.

use std::collections::VecDeque;
use std::sync::Arc;

use bytes::Bytes;
use iris_core::time::WallUs;
use iris_mp4::fragment::{Fragment, FrameSteps, Sample};
use iris_mp4::init::SampleEntry;

use crate::assembler::WallAnchor;
use crate::steer::Side;
use crate::sync::{LeastWatch, Watched};

/// Frames kept while no video fragment has closed over them: about sixteen
/// seconds at 64 kHz. Older frames are dropped; they have no picture.
const MAX_PENDING_FRAMES: usize = 1_024;

/// The length of one window of the stamp watch unless another is set: ten
/// minutes of counted frames, in 90 kHz ticks.
const WATCH_WINDOW_TICKS: u64 = 600 * 90_000;

/// One received audio frame.
#[derive(Clone, Debug)]
pub(crate) struct AudioFrame {
    /// The frame's bytes as the track stores them (raw AAC).
    pub(crate) data: Bytes,
    /// The camera's extended RTP timestamp, in units of the sampling rate.
    pub(crate) raw_ts: i64,
    /// When the recorder received the frame.
    pub(crate) wall: WallUs,
    /// Whether packets were lost before the frame.
    pub(crate) lost: bool,
}

/// The span of one closed video fragment on its run's timeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct VideoSpan {
    /// Decode time of the fragment's first frame, 90 kHz ticks.
    pub(crate) decode_time: u64,
    /// The fragment's duration in ticks.
    pub(crate) duration: u64,
    /// Wall time of the fragment's first frame.
    pub(crate) wall: WallUs,
    /// The low 32 bits of the camera's RTP timestamp of the first frame.
    pub(crate) raw_rtp: u32,
    /// Whether the fragment starts a video run.
    pub(crate) new_run: bool,
}

impl VideoSpan {
    /// The span of a closed video fragment.
    pub(crate) fn of(fragment: &Fragment) -> Self {
        Self {
            decode_time: fragment.decode_time,
            duration: fragment.duration(),
            wall: fragment.wall,
            raw_rtp: fragment.raw_rtp,
            new_run: fragment.new_run,
        }
    }

    fn end(&self) -> u64 {
        self.decode_time.saturating_add(self.duration)
    }
}

/// What a pushed frame did to the run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Pushed {
    /// The frame continues the run, or is the first received.
    Continued,
    /// Packets were lost or the timestamp left the count: the frames held
    /// were dropped and the frame starts over. The video starts a run too,
    /// so that both streams break at the same point.
    Broke,
}

#[derive(Debug)]
struct Pending {
    data: Bytes,
    /// Position on the count clock: frames received since the count began,
    /// in ticks.
    count_ticks: u64,
    /// Position among the frames recorded, in ticks: what the decode time
    /// counts.
    decode_ticks: u64,
    raw_ts: i64,
    /// The step this frame holds: frames left out just before it, or the
    /// frame is the second record of the one before it.
    step: Option<Side>,
}

/// Collects frames of one audio stream into fragments beside a video stream.
#[derive(Debug)]
pub(crate) struct AudioAssembler {
    entry: Arc<SampleEntry>,
    /// A frame's duration in 90 kHz ticks.
    frame_ticks: u32,
    /// A frame's length in units of the camera's timestamp.
    frame_samples: i64,
    /// How far a frame's timestamp may be from the count within a run.
    tolerance: i64,
    /// The raw timestamp the count gives the next frame.
    expected: Option<i64>,
    /// The count clock's position for the next frame.
    count_ticks: u64,
    /// The position of the next frame recorded: the count less the frames
    /// left out, plus those recorded twice.
    decode_ticks: u64,
    /// The frames of a step still to come: how many of the frames received
    /// next are left out (`Side::Late`) or recorded twice (`Side::Early`).
    owed: Option<(Side, u32)>,
    /// Whether frames were left out since the last frame recorded.
    left_out: bool,
    /// How many times the frame pushed last was recorded.
    copies: usize,
    /// Receipt time less count time, by least delay.
    anchor: Option<WallAnchor>,
    pending: VecDeque<Pending>,
    /// Video decode time of count zero, once the run is placed.
    base: Option<i64>,
    /// The video span that closed last, whose audio is still to be cut.
    span: Option<VideoSpan>,
    new_run: bool,
    /// Where placement puts the first frame of the fragment cut last, less where
    /// the count put it, in ticks.
    drift_ticks: i64,
    /// The camera's timestamps against the frame count over the run, window
    /// by window, in ticks. A camera stamps a frame when it packs it, late
    /// by a varying time, so the frame stamped earliest in a window waited
    /// least, and that least is the steadiest sign of where the camera's
    /// clock stands against the count (`docs/research/14-drift.md`). The
    /// watch is reported and decides nothing.
    watch: LeastWatch,
    closed: Option<Watched>,
}

impl AudioAssembler {
    /// An assembler for frames of `frame_samples` samples at `sample_rate`
    /// hertz that last `frame_ticks` ticks each.
    pub(crate) fn new(
        entry: Arc<SampleEntry>,
        sample_rate: u32,
        frame_samples: u32,
        frame_ticks: u32,
    ) -> Self {
        Self {
            entry,
            frame_ticks,
            frame_samples: i64::from(frame_samples),
            tolerance: i64::from(sample_rate / 2),
            expected: None,
            count_ticks: 0,
            decode_ticks: 0,
            owed: None,
            left_out: false,
            copies: 0,
            anchor: None,
            pending: VecDeque::new(),
            base: None,
            span: None,
            new_run: true,
            drift_ticks: 0,
            watch: LeastWatch::new(WATCH_WINDOW_TICKS),
            closed: None,
        }
    }

    /// The same assembler with windows of `ticks` of counted frames in its
    /// stamp watch.
    pub(crate) fn with_watch_window(mut self, ticks: u64) -> Self {
        self.watch = LeastWatch::new(ticks);
        self
    }

    /// The window of the stamp watch that closed since the last call, in
    /// ticks: how far the frames' timestamps stood from the count.
    pub(crate) fn take_stamp_window(&mut self) -> Option<Watched> {
        self.closed.take()
    }

    /// The drift of the fragment cut last: placement less count, in ticks.
    pub(crate) fn drift_ticks(&self) -> i64 {
        self.drift_ticks
    }

    /// The sample entry of the frames.
    pub(crate) fn entry(&self) -> &Arc<SampleEntry> {
        &self.entry
    }

    /// A frame's duration in 90 kHz ticks.
    pub(crate) fn frame_ticks(&self) -> u32 {
        self.frame_ticks
    }

    /// The decode time and the wall time the recording gives the frame
    /// pushed last, on the video run's timeline, once for every time the
    /// frame was recorded: none for a frame a step left out, two for a frame
    /// a step recorded twice. Nothing until the run is placed on that
    /// timeline, which the first video fragment to close after the run's
    /// start does. A frame pushed as the video starts another run keeps the
    /// time of the run that ended.
    pub(crate) fn last_pushed(&self) -> impl Iterator<Item = (u64, WallUs)> + '_ {
        let from = self.pending.len().saturating_sub(self.copies);
        self.pending
            .range(from..)
            .filter_map(|frame| self.recorded_at(frame.decode_ticks))
    }

    fn recorded_at(&self, decode_ticks: u64) -> Option<(u64, WallUs)> {
        let span = self.span?;
        let decode = self.base?.checked_add(i64::try_from(decode_ticks).ok()?)?;
        let into_span = decode.checked_sub(i64::try_from(span.decode_time).ok()?)?;
        Some((
            u64::try_from(decode).ok()?,
            span.wall.offset(ticks_to_micros_signed(into_span)),
        ))
    }

    /// Makes a step of `frames` frames on the frames received next: with the
    /// sound late (`Side::Late`) they are not recorded, and with the sound
    /// early (`Side::Early`) each is recorded twice. Every later frame of
    /// the run then stands that many frames earlier, or later, on the
    /// video's timeline. A step replaces what is left of the step before
    /// it; a break of the run and a run started by the video end it, and
    /// the run that follows is placed by its frames' receipt as any other.
    /// A run that is not placed on the video's timeline takes no step: its
    /// place is still to be read from its frames' receipt.
    pub(crate) fn step(&mut self, side: Side, frames: u32) {
        self.owed = (frames > 0 && self.base.is_some()).then_some((side, frames));
    }

    /// The step made on the frame pushed last: `Side::Late` when the frame
    /// was left out, `Side::Early` when it was recorded twice.
    pub(crate) fn last_step(&self) -> Option<Side> {
        match self.copies {
            0 => Some(Side::Late),
            1 => None,
            _ => Some(Side::Early),
        }
    }

    /// Takes a frame. Frames are held until the video fragment over them has
    /// closed ([`Self::video_fragment`]).
    pub(crate) fn push(&mut self, mut frame: AudioFrame) -> Pushed {
        let on_count = self
            .expected
            .filter(|expected| !frame.lost && (frame.raw_ts - expected).abs() <= self.tolerance);
        let pushed = if on_count.is_none() && self.expected.is_some() {
            self.start_over();
            Pushed::Broke
        } else {
            Pushed::Continued
        };
        if let Some(expected) = on_count {
            let off_ticks =
                (frame.raw_ts - expected) * i64::from(self.frame_ticks) / self.frame_samples;
            if let Some(window) = self.watch.note(self.count_ticks, off_ticks) {
                self.closed = Some(window);
            }
        }
        // Within a run the count advances from where it stood, so stamps
        // that wander do not move it; a run starts from its first stamp.
        self.expected = Some(
            on_count
                .unwrap_or(frame.raw_ts)
                .saturating_add(self.frame_samples),
        );
        let candidate_us = frame
            .wall
            .get()
            .saturating_sub(ticks_to_micros(self.count_ticks));
        match self.anchor.as_mut() {
            Some(anchor) => anchor.update(candidate_us, self.count_ticks),
            None => self.anchor = Some(WallAnchor::new(candidate_us, self.count_ticks)),
        }
        let side = self.owed.map(|(side, _)| side);
        self.owed = self.owed.and_then(|(side, frames)| {
            Some((side, frames.checked_sub(1).filter(|left| *left > 0)?))
        });
        self.copies = match side {
            Some(Side::Late) => 0,
            None => 1,
            Some(Side::Early) => 2,
        };
        self.left_out |= self.copies == 0;
        let data = std::mem::take(&mut frame.data);
        for copy in 0..self.copies {
            if self.pending.len() >= MAX_PENDING_FRAMES {
                self.pending.pop_front();
            }
            let step = if copy > 0 {
                Some(Side::Early)
            } else {
                std::mem::take(&mut self.left_out).then_some(Side::Late)
            };
            self.pending.push_back(Pending {
                data: data.clone(),
                count_ticks: self.count_ticks,
                decode_ticks: self.decode_ticks,
                raw_ts: frame.raw_ts,
                step,
            });
            self.decode_ticks += u64::from(self.frame_ticks);
        }
        self.count_ticks += u64::from(self.frame_ticks);
        pushed
    }

    /// Takes the span of a video fragment that just closed and returns the
    /// audio fragment of the span before it, whose frames have all arrived
    /// by then. A span that starts a video run starts an audio run.
    pub(crate) fn video_fragment(&mut self, span: VideoSpan) -> Option<Fragment> {
        // A run is placed against the fragment of its video run that closed
        // last: this one, unless it starts another run.
        let fragment = self.cut((!span.new_run).then_some(&span));
        if span.new_run {
            self.base = None;
            self.new_run = true;
            self.lay_again();
        }
        self.span = Some(span);
        fragment
    }

    /// The audio of the last video span, at a session's end; call after the
    /// video's last fragment went through [`Self::video_fragment`].
    pub(crate) fn flush(&mut self) -> Option<Fragment> {
        let fragment = self.cut(None);
        self.span = None;
        fragment
    }

    /// Lays the frames held on the count of frames received again, for a
    /// run that is placed anew: no step carries into it. A second record of
    /// a frame goes, and the frames keep decode positions without a gap
    /// that end where the count stands, so every frame received from here
    /// on is recorded where its receipt places it; frames held from before
    /// frames that were left out stand that much late, and the frame after
    /// those left out keeps its mark.
    fn lay_again(&mut self) {
        self.pending.retain(|frame| frame.step != Some(Side::Early));
        let mut at = self.count_ticks;
        for frame in self.pending.iter_mut().rev() {
            at = at.saturating_sub(u64::from(self.frame_ticks));
            frame.decode_ticks = at;
        }
        self.decode_ticks = self.count_ticks;
        self.owed = None;
        self.left_out = false;
        self.copies = self.copies.min(1);
    }

    /// Drops what is held and starts the count over.
    fn start_over(&mut self) {
        self.pending.clear();
        self.count_ticks = 0;
        self.decode_ticks = 0;
        self.owed = None;
        self.left_out = false;
        self.watch = LeastWatch::new(self.watch.span());
        self.anchor = None;
        self.base = None;
        self.span = None;
        self.new_run = true;
    }

    /// Where the placement puts the frame at `count_ticks` on the timeline
    /// of `span`'s run.
    fn placed(&self, span: &VideoSpan, count_ticks: u64) -> Option<i64> {
        let wall = self.anchor.as_ref()?.wall_at(count_ticks);
        let into_span = micros_to_ticks(wall.micros_since(span.wall));
        Some(
            i64::try_from(span.decode_time)
                .ok()?
                .saturating_add(into_span),
        )
    }

    /// The fragment of the held span: the frames that start inside it.
    /// `latest` is the fragment of the same video run that closed last, when
    /// one closed after the span's: a run that is placed here is placed against it, because the
    /// wall time of a session's first video fragment is taken before the
    /// video's estimate has settled (`docs/research/13-audio.md` §5.1a).
    fn cut(&mut self, latest: Option<&VideoSpan>) -> Option<Fragment> {
        let span = self.span?;
        let start = i64::try_from(span.decode_time).ok()?;
        let end = i64::try_from(span.end()).ok()?;
        if self.base.is_none() {
            // Place the run: frames received before the span starts have no
            // picture in this run and are dropped. No step is in force
            // where a run is placed, so the frames received from here on
            // are recorded at their count.
            let against = latest.unwrap_or(&span);
            while let Some(first) = self.pending.front() {
                let placed = self.placed(against, first.count_ticks)?;
                if placed >= start {
                    self.base = Some(placed - i64::try_from(first.count_ticks).ok()?);
                    break;
                }
                self.pending.pop_front();
            }
        }
        let base = self.base?;
        let decode = |decode_ticks: u64| base.saturating_add(decode_ticks as i64);
        let first = self.pending.front()?;
        let first_decode = decode(first.decode_ticks);
        if first_decode >= end {
            return None;
        }
        self.drift_ticks = self.placed(&span, first.count_ticks)? - first_decode;
        let raw_rtp = first.raw_ts as u32;
        let mut samples = Vec::new();
        let mut steps = FrameSteps::default();
        while let Some(frame) = self.pending.front() {
            if decode(frame.decode_ticks) >= end {
                break;
            }
            let frame = self.pending.pop_front()?;
            match frame.step {
                Some(Side::Late) => steps.left_out = true,
                Some(Side::Early) => steps.twice = true,
                None => {}
            }
            samples.push(Sample {
                data: frame.data,
                duration: self.frame_ticks,
                is_sync: true,
            });
        }
        let decode_time = u64::try_from(first_decode).ok()?;
        let wall = span
            .wall
            .offset(ticks_to_micros_signed(first_decode - start));
        let fragment = Fragment {
            samples,
            decode_time,
            wall,
            raw_rtp,
            new_run: self.new_run,
            steps,
            sample_entry: Arc::clone(&self.entry),
        };
        self.new_run = false;
        Some(fragment)
    }
}

fn ticks_to_micros(ticks: u64) -> i64 {
    i64::try_from(ticks.saturating_mul(100) / 9).unwrap_or(i64::MAX)
}

fn ticks_to_micros_signed(ticks: i64) -> i64 {
    ticks.saturating_mul(100) / 9
}

fn micros_to_ticks(micros: i64) -> i64 {
    micros.saturating_mul(9) / 100
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME_TICKS: u64 = 1_440;
    const FRAME_US: i64 = 16_000;
    /// Wall time at which the video run's decode time is zero.
    const VIDEO_EPOCH_US: i64 = 1_000_000_000;

    fn entry() -> Arc<SampleEntry> {
        Arc::new(SampleEntry::audio(
            Bytes::from_static(b"\0\0\0\x08mp4a"),
            "mp4a.40.2".into(),
        ))
    }

    fn assembler() -> AudioAssembler {
        AudioAssembler::new(entry(), 64_000, 1_024, 1_440)
    }

    /// Frame `index` of a stream whose frame zero was captured `lead_us`
    /// after the video epoch, received `late_us` after its capture.
    fn frame(index: i64, lead_us: i64, late_us: i64) -> AudioFrame {
        AudioFrame {
            data: Bytes::from(index.to_be_bytes().to_vec()),
            raw_ts: 77_000 + index * 1_024,
            wall: WallUs::new(VIDEO_EPOCH_US + lead_us + index * FRAME_US + late_us),
            lost: false,
        }
    }

    fn span(decode_time: u64, duration: u64, new_run: bool) -> VideoSpan {
        VideoSpan {
            decode_time,
            duration,
            wall: WallUs::new(VIDEO_EPOCH_US + ticks_to_micros(decode_time)),
            raw_rtp: decode_time as u32,
            new_run,
        }
    }

    fn index_of(sample: &Sample) -> i64 {
        i64::from_be_bytes(sample.data[..].try_into().unwrap())
    }

    /// Receipt delays as cameras produce them: most frames prompt, some held
    /// behind a keyframe for up to 300 ms.
    fn lateness(index: i64) -> i64 {
        match index % 31 {
            0 => 0,
            7..=12 => 300_000 - (index % 31 - 7) * FRAME_US,
            n => (n % 4) * 5_000,
        }
    }

    /// Audio that started 5 ms into the video run, against video fragments
    /// of uneven length: each audio fragment starts within a frame of its
    /// video fragment, the fragments are contiguous by count, no frame is
    /// lost or repeated, and late receipt does not move the placement.
    #[test]
    fn fragments_follow_the_video_spans_within_a_frame() {
        let mut audio = assembler();
        let durations = [180_000_u64, 182_700, 189_000, 180_000, 195_300, 180_000];
        let mut fragments = Vec::new();
        let mut next = 0_i64;
        let mut start = 0_u64;
        for (number, duration) in durations.into_iter().enumerate() {
            // Frames captured up to the end of this span have arrived when
            // the fragment after it closes.
            let end_us = ticks_to_micros(start + duration);
            while 5_000 + next * FRAME_US < end_us + 400_000 {
                assert_eq!(
                    audio.push(frame(next, 5_000, lateness(next))),
                    Pushed::Continued
                );
                next += 1;
            }
            fragments.extend(audio.video_fragment(span(start, duration, number == 0)));
            start += duration;
        }
        fragments.extend(audio.flush());
        assert_eq!(fragments.len(), durations.len());

        let mut span_start = 0_u64;
        let mut expected_index = 0_i64;
        for (number, (fragment, duration)) in fragments.iter().zip(durations).enumerate() {
            let lead = fragment.decode_time as i64 - span_start as i64;
            assert!(
                (0..FRAME_TICKS as i64).contains(&lead),
                "fragment {number} starts {lead} ticks into its span"
            );
            assert_eq!(fragment.new_run, number == 0);
            assert_eq!(
                fragment.wall,
                WallUs::new(VIDEO_EPOCH_US + ticks_to_micros(fragment.decode_time)),
                "fragment {number}"
            );
            for sample in &fragment.samples {
                assert_eq!(index_of(sample), expected_index);
                assert!(sample.is_sync && u64::from(sample.duration) == FRAME_TICKS);
                expected_index += 1;
            }
            if number > 0 {
                let before = &fragments[number - 1];
                assert_eq!(fragment.decode_time, before.decode_time + before.duration());
            }
            span_start += duration;
        }
        // The first frame was captured 5 ms in: 450 ticks.
        assert_eq!(fragments[0].decode_time, 450);
        assert_eq!(index_of(&fragments[0].samples[0]), 0);
        // Within the rounding of microseconds to ticks.
        assert!(audio.drift_ticks().abs() <= 1, "{}", audio.drift_ticks());
    }

    /// Frames captured before the video run starts have no picture and are
    /// dropped; the run starts with the first frame inside the span.
    /// A session's first video fragment takes its wall time before the
    /// video's estimate has settled: here it is 72 ms late, and the
    /// fragments after it are right. The run is placed against the fragment
    /// that closed last, so the audio is where the settled estimate puts it
    /// from its first frame, and no drift shows in the fragments that follow.
    #[test]
    fn a_run_is_placed_against_the_latest_closed_video_fragment() {
        let mut audio = assembler();
        for index in 0..400 {
            assert_eq!(audio.push(frame(index, 0, 0)), Pushed::Continued);
        }
        let unsettled = VideoSpan {
            wall: WallUs::new(VIDEO_EPOCH_US + 72_000),
            ..span(0, 180_000, true)
        };
        assert!(audio.video_fragment(unsettled).is_none());
        let first = audio
            .video_fragment(span(180_000, 180_000, false))
            .expect("the audio of the first video fragment");
        assert_eq!(first.decode_time, 0);
        assert_eq!(index_of(&first.samples[0]), 0);
        assert_eq!(first.samples.len(), 125);
        assert!(first.new_run);
        let second = audio
            .video_fragment(span(360_000, 180_000, false))
            .expect("the audio of the second");
        assert_eq!(second.decode_time, 180_000);
        assert_eq!(audio.drift_ticks(), 0);

        // At a session's end there is no later fragment: the run is placed
        // against the only one it has.
        let mut audio = assembler();
        for index in 0..200 {
            audio.push(frame(index, 0, 0));
        }
        assert!(audio.video_fragment(unsettled).is_none());
        let only = audio.flush().expect("the audio of the only fragment");
        // 72 ms is 6,480 ticks: the frames placed before the fragment's
        // start are dropped, and frame 5 is the first at or after it.
        assert_eq!(index_of(&only.samples[0]), 5);
        assert_eq!(only.decode_time, 720);

        // A fragment that starts another video run is on another timeline
        // and places nothing of the run before it.
        let mut audio = assembler();
        for index in 0..200 {
            audio.push(frame(index, 0, 0));
        }
        assert!(audio.video_fragment(unsettled).is_none());
        let before = audio
            .video_fragment(span(0, 180_000, true))
            .expect("the audio of the run that ended");
        assert_eq!((before.decode_time, index_of(&before.samples[0])), (720, 5));
    }

    #[test]
    fn audio_before_the_video_run_is_dropped() {
        let mut audio = assembler();
        // Frame zero is captured 100 ms before the video epoch.
        for index in 0..200 {
            audio.push(frame(index, -100_000, 0));
        }
        assert!(audio.video_fragment(span(0, 180_000, true)).is_none());
        let fragment = audio.flush().unwrap();
        // Frames 0..=6 start before zero; frame 7 starts 12 ms in.
        assert_eq!(index_of(&fragment.samples[0]), 7);
        assert_eq!(fragment.decode_time, 1_080);
        assert!(fragment.new_run);
    }

    /// A video run that starts mid-stream closes the audio of the span before
    /// it and starts an audio run on that run's timeline.
    #[test]
    fn a_video_run_starts_an_audio_run() {
        let mut audio = assembler();
        for index in 0..400 {
            audio.push(frame(index, 0, 0));
        }
        assert!(audio.video_fragment(span(0, 180_000, true)).is_none());
        let first = audio.video_fragment(span(180_000, 180_000, false)).unwrap();
        assert_eq!((first.decode_time, first.samples.len()), (0, 125));
        // The video starts over at the instant the timeline it leaves read 4 s.
        let restarted = VideoSpan {
            decode_time: 0,
            duration: 180_000,
            wall: WallUs::new(VIDEO_EPOCH_US + 4_000_000),
            raw_rtp: 0,
            new_run: true,
        };
        let second = audio.video_fragment(restarted).unwrap();
        assert_eq!((second.decode_time, second.new_run), (180_000, false));
        let third = audio.flush().unwrap();
        assert!(third.new_run);
        assert_eq!(third.decode_time, 0);
        // 4 s is frame 250 exactly.
        assert_eq!(index_of(&third.samples[0]), 250);
    }

    /// Timestamps that wander around the count, backwards too, keep the run;
    /// one more than half a second off, or packet loss, breaks it: what was
    /// held is dropped and the next fragment starts a run.
    #[test]
    fn a_stamp_off_the_count_or_a_loss_breaks_the_run() {
        let mut audio = assembler();
        for index in 0..50 {
            let mut wandering = frame(index, 0, 0);
            wandering.raw_ts += [0, -384, 640, -1_024][index as usize % 4];
            assert_eq!(audio.push(wandering), Pushed::Continued);
        }
        let mut exact = frame(50, 0, 0);
        exact.raw_ts += 32_000;
        assert_eq!(audio.push(exact), Pushed::Continued, "at the tolerance");

        for (step, lost) in [(32_001, false), (-32_001, false), (0, true)] {
            let mut audio = assembler();
            for index in 0..10 {
                audio.push(frame(index, 0, 0));
            }
            let mut broken = frame(10, 0, 0);
            broken.raw_ts += step;
            broken.lost = lost;
            assert_eq!(audio.push(broken), Pushed::Broke, "{step} {lost}");
            for index in 11..140 {
                let mut after = frame(index, 0, 0);
                after.raw_ts += step;
                assert_eq!(audio.push(after), Pushed::Continued);
            }
            // Frame 10 starts 160 ms into the span.
            assert!(audio.video_fragment(span(0, 180_000, true)).is_none());
            let fragment = audio.flush().unwrap();
            assert!(fragment.new_run);
            assert_eq!(index_of(&fragment.samples[0]), 10);
            assert_eq!(fragment.decode_time, 14_400);
        }
    }

    /// The first frame of a session may follow lost packets without a break:
    /// there is no run yet.
    #[test]
    fn a_loss_before_the_first_frame_is_not_a_break() {
        let mut audio = assembler();
        let mut first = frame(0, 0, 0);
        first.lost = true;
        assert_eq!(audio.push(first), Pushed::Continued);
    }

    /// The watch reports, every ten minutes of counted frames, the frame
    /// stamped earliest and the one stamped last against the count and how
    /// far the earliest has moved since the run's first window; a break starts it
    /// over.
    #[test]
    fn the_stamp_watch_reports_each_window_against_the_runs_first() {
        let mut audio = assembler();
        let frame = |index: i64, late: i64| AudioFrame {
            data: Bytes::from_static(b"a"),
            raw_ts: index * 1_024 + late,
            wall: WallUs::new(VIDEO_EPOCH_US + index * 16_000),
            lost: false,
        };
        // A frame of 1,024 samples is 1,440 ticks: 64 samples are 90 ticks.
        let window = (WATCH_WINDOW_TICKS / 1_440) as i64;
        let mut index = 0;
        let mut windows = Vec::new();
        // Stamps 0 to 192 samples late in the first window, 128 more in the
        // second, and back in the third.
        for extra in [0, 128, 0] {
            for _ in 0..=window {
                audio.push(frame(index, extra + (index % 4) * 64));
                index += 1;
                windows.extend(audio.take_stamp_window());
            }
        }
        // The run's first frame sets the count and is not watched, so each
        // window closes on the first frame of the phase after it.
        audio.push(frame(index, 0));
        index += 1;
        windows.extend(audio.take_stamp_window());
        let seen = |windows: &[Watched]| -> Vec<(i64, i64, i64)> {
            windows
                .iter()
                .map(|w| (w.least, w.greatest, w.moved))
                .collect()
        };
        assert_eq!(seen(&windows), [(0, 270, 0), (180, 450, 180), (0, 270, 0)]);
        assert!(windows.iter().all(|w| w.samples == 37_501), "{windows:?}");

        // A lost packet starts a run, and the watch with it.
        let mut lost = frame(index, 0);
        lost.lost = true;
        assert_eq!(audio.push(lost), Pushed::Broke);
        windows.clear();
        for _ in 0..=window {
            index += 1;
            audio.push(frame(index, 64));
            windows.extend(audio.take_stamp_window());
        }
        assert_eq!(seen(&windows), [(90, 90, 0)]);
    }

    /// An audio clock that runs fast against the video timeline shows as
    /// drift: the count puts a fragment later than its placement does.
    #[test]
    fn a_fast_audio_clock_shows_as_negative_drift() {
        let mut audio = assembler();
        // Frames arrive every 15.984 ms: 1,000 ppm fast.
        let mut next = 0_i64;
        let mut start = 0_u64;
        for number in 0..16 {
            // Each video fragment closes when the frames up to its end, and
            // a little more, have arrived.
            let end_us = ticks_to_micros(start + 180_000);
            while next * 15_984 < end_us + 100_000 {
                audio.push(AudioFrame {
                    data: Bytes::from(next.to_be_bytes().to_vec()),
                    raw_ts: next * 1_024,
                    wall: WallUs::new(VIDEO_EPOCH_US + next * 15_984),
                    lost: false,
                });
                next += 1;
            }
            audio.video_fragment(span(start, 180_000, number == 0));
            start += 180_000;
        }
        // The last fragment cut starts 28 s in, where the count is 28 ms
        // ahead of the placement; the anchor follows a faster clock at
        // once, so the drift is the whole of it.
        let drift_ms = audio.drift_ticks() / 90;
        assert!((-30..=-26).contains(&drift_ms), "{drift_ms} ms");
    }

    /// An assembler that holds frames 0 to 299 of a placed run, whose first
    /// two seconds are cut, and the fragment of those.
    fn two_seconds_cut() -> (AudioAssembler, Fragment) {
        let mut audio = assembler();
        for index in 0..300 {
            assert_eq!(audio.push(frame(index, 0, 0)), Pushed::Continued);
            assert_eq!(audio.last_step(), None);
        }
        assert!(audio.video_fragment(span(0, 180_000, true)).is_none());
        let first = audio.video_fragment(span(180_000, 180_000, false)).unwrap();
        (audio, first)
    }

    fn indexes(fragment: &Fragment) -> Vec<i64> {
        fragment.samples.iter().map(index_of).collect()
    }

    /// The times the recording gives the frame pushed last, as ticks and as
    /// microseconds after the video's epoch.
    fn recorded(audio: &AudioAssembler) -> Vec<(u64, i64)> {
        audio
            .last_pushed()
            .map(|(decode, wall)| (decode, wall.get() - VIDEO_EPOCH_US))
            .collect()
    }

    /// With the sound late, the frames of a step are not recorded: nothing
    /// is published for them, the decode times go on as a count without a
    /// gap, every later frame stands that many frames earlier, and the
    /// fragment that holds the frame after them is marked.
    #[test]
    fn a_step_leaves_frames_out_and_the_decode_times_stay_a_count() {
        let (mut audio, first) = two_seconds_cut();
        assert_eq!(first.steps, FrameSteps::default());
        audio.step(Side::Late, 3);
        for index in 300..600 {
            assert_eq!(audio.push(frame(index, 0, 0)), Pushed::Continued);
            if index < 303 {
                assert_eq!(audio.last_step(), Some(Side::Late), "frame {index}");
                assert_eq!(recorded(&audio), [], "frame {index}");
            } else {
                // Three frames earlier than the count of frames received.
                let at = (index - 3) as u64;
                assert_eq!(audio.last_step(), None, "frame {index}");
                assert_eq!(
                    recorded(&audio),
                    [(at * FRAME_TICKS, at as i64 * FRAME_US)],
                    "frame {index}"
                );
            }
        }
        let second = audio.video_fragment(span(360_000, 180_000, false)).unwrap();
        let third = audio.video_fragment(span(540_000, 180_000, false)).unwrap();
        let fourth = audio.video_fragment(span(720_000, 180_000, false)).unwrap();
        assert_eq!(indexes(&second), (125..250).collect::<Vec<_>>());
        assert_eq!(
            indexes(&third),
            (250..300).chain(303..378).collect::<Vec<_>>()
        );
        assert_eq!(indexes(&fourth), (378..503).collect::<Vec<_>>());
        assert_eq!(
            [second.steps, third.steps, fourth.steps],
            [
                FrameSteps::default(),
                FrameSteps {
                    left_out: true,
                    twice: false
                },
                FrameSteps::default()
            ]
        );
        for (before, after) in [(&first, &second), (&second, &third), (&third, &fourth)] {
            assert_eq!(after.decode_time, before.decode_time + before.duration());
            assert_eq!(
                after.wall,
                WallUs::new(VIDEO_EPOCH_US + ticks_to_micros(after.decode_time))
            );
        }
        // Where the least receipt delay puts the frames, less where they
        // are recorded: the three frames of the step.
        assert_eq!(audio.drift_ticks(), 3 * FRAME_TICKS as i64);
    }

    /// With the sound early, each frame of a step is recorded twice, the
    /// second time a frame after the first: two frames are published for
    /// it, every later frame stands that many frames later, and the
    /// fragment that holds a second record is marked.
    #[test]
    fn a_step_records_frames_twice_and_the_decode_times_stay_a_count() {
        let (mut audio, first) = two_seconds_cut();
        audio.step(Side::Early, 2);
        for index in 300..600 {
            assert_eq!(audio.push(frame(index, 0, 0)), Pushed::Continued);
            let at = |position: i64| (position as u64 * FRAME_TICKS, position * FRAME_US);
            let expected = match index {
                300 => vec![at(300), at(301)],
                301 => vec![at(302), at(303)],
                later => vec![at(later + 2)],
            };
            assert_eq!(recorded(&audio), expected, "frame {index}");
            assert_eq!(
                audio.last_step(),
                (index < 302).then_some(Side::Early),
                "frame {index}"
            );
        }
        let second = audio.video_fragment(span(360_000, 180_000, false)).unwrap();
        let third = audio.video_fragment(span(540_000, 180_000, false)).unwrap();
        let fourth = audio.video_fragment(span(720_000, 180_000, false)).unwrap();
        assert_eq!(
            indexes(&third),
            (250..300)
                .chain([300, 300, 301, 301])
                .chain(302..373)
                .collect::<Vec<_>>()
        );
        assert_eq!(indexes(&fourth), (373..498).collect::<Vec<_>>());
        assert_eq!(
            [second.steps, third.steps, fourth.steps],
            [
                FrameSteps::default(),
                FrameSteps {
                    left_out: false,
                    twice: true
                },
                FrameSteps::default()
            ]
        );
        for (before, after) in [(&first, &second), (&second, &third), (&third, &fourth)] {
            assert_eq!(after.decode_time, before.decode_time + before.duration());
        }
        assert_eq!(audio.drift_ticks(), -2 * (FRAME_TICKS as i64));
    }

    /// A step moves the count of decode times and not the count the
    /// camera's timestamps are held against: frames on their stamps go on
    /// continuing the run, and the stamp watch reads no movement.
    #[test]
    fn a_step_does_not_move_the_count_the_timestamps_are_held_against() {
        for side in [Side::Late, Side::Early] {
            let mut audio = assembler().with_watch_window(100 * FRAME_TICKS);
            let mut windows = Vec::new();
            for index in 0..450 {
                if index == 120 {
                    audio.step(side, 3);
                }
                assert_eq!(audio.push(frame(index, 0, 0)), Pushed::Continued);
                windows.extend(audio.take_stamp_window());
            }
            assert_eq!(audio.count_ticks, 450 * FRAME_TICKS, "{side:?}");
            assert_eq!(windows.len(), 4, "{side:?}");
            assert!(
                windows
                    .iter()
                    .all(|w| (w.least, w.greatest, w.moved) == (0, 0, 0)),
                "{side:?}: {windows:?}"
            );
        }
    }

    /// What is left of a step ends where the run breaks and where the
    /// video starts a run: the audio is placed again there. A step named
    /// while frames of another are owed replaces it.
    #[test]
    fn a_break_and_a_video_run_end_what_is_left_of_a_step() {
        let (mut audio, _) = two_seconds_cut();
        audio.step(Side::Late, 3);
        audio.push(frame(300, 0, 0));
        assert_eq!(audio.last_step(), Some(Side::Late));
        let mut lost = frame(301, 0, 0);
        lost.lost = true;
        assert_eq!(audio.push(lost), Pushed::Broke);
        assert_eq!(audio.last_step(), None);
        audio.push(frame(302, 0, 0));
        assert_eq!(audio.last_step(), None);

        let (mut audio, _) = two_seconds_cut();
        audio.step(Side::Early, 3);
        audio.push(frame(300, 0, 0));
        assert_eq!(audio.last_step(), Some(Side::Early));
        let restarted = VideoSpan {
            decode_time: 0,
            duration: 180_000,
            wall: WallUs::new(VIDEO_EPOCH_US + 4_800_000),
            raw_rtp: 0,
            new_run: true,
        };
        let before = audio.video_fragment(restarted).unwrap();
        assert_eq!(indexes(&before), (125..250).collect::<Vec<_>>());
        audio.push(frame(301, 0, 0));
        assert_eq!(audio.last_step(), None);
        // The run after is placed by its frames' receipt, and the second
        // record of frame 300, which was captured at 4.8 s, is not in it.
        let after = audio.flush().unwrap();
        assert!(after.new_run);
        assert_eq!((after.decode_time, indexes(&after)), (0, vec![300, 301]));
        assert_eq!(after.steps, FrameSteps::default());

        // Frames left out and then a video run from the instant frame 295
        // was captured: the frames received after those left out are
        // recorded where their receipt places them, eight frames in, and
        // the five held from before stand three frames late without a gap.
        let (mut audio, _) = two_seconds_cut();
        audio.step(Side::Late, 3);
        for index in 300..305 {
            audio.push(frame(index, 0, 0));
        }
        let restarted = VideoSpan {
            wall: WallUs::new(VIDEO_EPOCH_US + 295 * FRAME_US),
            ..restarted
        };
        audio.video_fragment(restarted).unwrap();
        audio.push(frame(305, 0, 0));
        let after = audio.flush().unwrap();
        assert_eq!(after.decode_time, 3 * FRAME_TICKS);
        assert_eq!(
            indexes(&after),
            (295..300).chain(303..306).collect::<Vec<_>>()
        );
        assert!(after.steps.left_out && !after.steps.twice);

        let (mut audio, _) = two_seconds_cut();
        audio.step(Side::Late, 3);
        audio.push(frame(300, 0, 0));
        audio.step(Side::Early, 1);
        audio.push(frame(301, 0, 0));
        assert_eq!(audio.last_step(), Some(Side::Early));
        audio.push(frame(302, 0, 0));
        assert_eq!(audio.last_step(), None);
        audio.step(Side::Late, 0);
        audio.push(frame(303, 0, 0));
        assert_eq!(audio.last_step(), None, "a step of no frames is none");

        // A run that is not placed takes no step.
        let mut audio = assembler();
        audio.push(frame(0, 0, 0));
        audio.step(Side::Early, 2);
        audio.push(frame(1, 0, 0));
        assert_eq!(audio.last_step(), None);
        assert!(audio.video_fragment(span(0, 180_000, true)).is_none());
        audio.step(Side::Early, 2);
        audio.push(frame(2, 0, 0));
        assert_eq!(audio.last_step(), None, "the first fragment places nothing");
        assert_eq!(indexes(&audio.flush().unwrap()), [0, 1, 2]);
    }

    #[test]
    fn frames_without_a_video_fragment_are_bounded() {
        let mut audio = assembler();
        for index in 0..5_000 {
            audio.push(frame(index, 0, 0));
        }
        assert_eq!(audio.pending.len(), MAX_PENDING_FRAMES);
        assert!(audio.flush().is_none(), "no span, no fragment");
    }
}
