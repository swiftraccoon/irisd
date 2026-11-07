//! Groups frames into fragments and assigns their durations.

use std::sync::Arc;

use bytes::Bytes;
use iris_core::time::WallUs;
use iris_mp4::fragment::{Fragment, Sample};
use iris_mp4::init::SampleEntry;

use crate::regularize::Regularizer;
use crate::source::IngestOptions;

/// Bounds independent of the camera's timestamp and declared frame rate.
const MAX_PENDING_FRAMES: usize = 4_096;
const MAX_PENDING_BYTES: usize = 64 * 1_024 * 1_024;
/// Maximum upward movement of the wall-minus-media anchor, in parts per million.
const WALL_ANCHOR_UPWARD_PPM: i64 = 500;

#[derive(Debug)]
struct WallAnchor {
    offset_us: i64,
    media_ticks: u64,
}

impl WallAnchor {
    fn update(&mut self, candidate_us: i64, media_ticks: u64) {
        if candidate_us <= self.offset_us {
            self.offset_us = candidate_us;
        } else {
            let elapsed_ticks = media_ticks.saturating_sub(self.media_ticks);
            let elapsed_us = ticks_to_micros(elapsed_ticks);
            let allowed = elapsed_us.saturating_mul(WALL_ANCHOR_UPWARD_PPM) / 1_000_000;
            self.offset_us = self.offset_us.saturating_add(allowed).min(candidate_us);
        }
        self.media_ticks = media_ticks;
    }

    fn wall_at(&self, media_ticks: u64) -> WallUs {
        WallUs::new(self.offset_us.saturating_add(ticks_to_micros(media_ticks)))
    }
}

fn ticks_to_micros(ticks: u64) -> i64 {
    i64::try_from(ticks.saturating_mul(100) / 9).unwrap_or(i64::MAX)
}

/// One received access unit.
#[derive(Clone, Debug)]
pub struct Frame {
    /// Length-prefixed NAL units without parameter sets.
    pub data: Bytes,
    /// Unwrapped camera RTP timestamp in 90 kHz ticks.
    pub raw_ts: i64,
    /// NVR wall time at which the frame's first packet arrived.
    pub wall: WallUs,
    /// Whether the frame is a random access point.
    pub is_sync: bool,
    /// Whether the frame carried new parameter sets.
    pub new_params: bool,
}

/// Builds fragments from frames.
///
/// A fragment opens at a sync frame and closes at the next sync frame, at a
/// frame carrying new parameters, or when camera, receipt, or nominal sample
/// time reaches the cap plus a quarter. Pending payload is limited to 64 MiB
/// and 4,096 frames. Its durations are assigned when it closes, because the last
/// frame's duration is only known from the first timestamp of the next
/// fragment. Call [`FragmentAssembler::flush`] before
/// [`FragmentAssembler::mark_discontinuity`] when a session ends.
#[derive(Debug)]
pub struct FragmentAssembler {
    max_fragment: i64,
    regularizer: Regularizer,
    sample_entry: Arc<SampleEntry>,
    pending: Vec<Frame>,
    pending_bytes: usize,
    pending_entry: Arc<SampleEntry>,
    decode_time: u64,
    wall_anchor: Option<WallAnchor>,
    new_run: bool,
    started: bool,
    snaps: u64,
    holes: u64,
    hold_ticks: u64,
    last_hold_ticks: i64,
}

impl FragmentAssembler {
    /// Creates an assembler for one run.
    #[must_use]
    pub fn new(
        options: &IngestOptions,
        regularizer: Regularizer,
        sample_entry: Arc<SampleEntry>,
    ) -> Self {
        Self {
            max_fragment: options.max_fragment.get().max(1),
            regularizer,
            pending_entry: Arc::clone(&sample_entry),
            sample_entry,
            pending: Vec::new(),
            pending_bytes: 0,
            decode_time: 0,
            wall_anchor: None,
            new_run: true,
            started: false,
            snaps: 0,
            holes: 0,
            hold_ticks: 0,
            last_hold_ticks: 0,
        }
    }

    /// Sets the codec configuration for frames pushed from now on.
    pub fn set_sample_entry(&mut self, entry: Arc<SampleEntry>) {
        self.sample_entry = entry;
    }

    /// Starts a fresh timing epoch at `nominal` without changing the recording timeline.
    pub(crate) fn retarget_timing(&mut self, nominal: u32) {
        self.regularizer.retarget(nominal);
    }

    /// Number of fragments whose timing was snapped to the camera timeline.
    #[must_use]
    pub fn snap_count(&self) -> u64 {
        self.snaps
    }

    /// Whole camera-dropped frames rendered as holds so far.
    #[must_use]
    pub fn holes(&self) -> u64 {
        self.holes
    }

    /// Ticks rendered as holds so far.
    #[must_use]
    pub fn hold_ticks(&self) -> u64 {
        self.hold_ticks
    }

    /// Ticks rendered as holds in the most recently closed fragment.
    #[must_use]
    pub fn last_hold_ticks(&self) -> i64 {
        self.last_hold_ticks
    }

    /// Access to the regulariser (for diagnostics).
    #[must_use]
    pub fn regularizer(&self) -> &Regularizer {
        &self.regularizer
    }

    /// The nominal frame duration in ticks (30 fps until something better is known).
    #[must_use]
    pub fn nominal(&self) -> u32 {
        self.regularizer.nominal().unwrap_or(3000)
    }

    /// The codec configuration applied to frames pushed from now on.
    #[must_use]
    pub fn sample_entry(&self) -> &Arc<SampleEntry> {
        &self.sample_entry
    }

    /// Provisional decode time of the last pushed frame (the pending
    /// fragment's start plus the frame's index times the nominal duration), or
    /// `None` when no frame is pending because the last push was dropped.
    #[must_use]
    pub fn last_provisional_decode_time(&self) -> Option<u64> {
        let index = self.pending.len().checked_sub(1)?;
        Some(self.decode_time + index as u64 * u64::from(self.nominal()))
    }

    /// Adds a frame; returns the fragment it completed, if any.
    ///
    /// A frame larger than 64 MiB flushes any pending fragment and is dropped.
    /// Frames after that are ignored until the next sync frame, which starts
    /// a separate run with its decode timeline at zero.
    pub fn push(&mut self, frame: Frame) -> Option<Fragment> {
        if frame.data.len() > MAX_PENDING_BYTES {
            // An oversized access unit cannot fit the assembly budget. Resume
            // at a sync frame because later frames may reference the dropped one.
            let completed = self.flush();
            self.mark_discontinuity();
            return completed;
        }
        if !self.started {
            if !frame.is_sync {
                return None;
            }
            self.started = true;
        }
        let mut completed = None;
        if let Some(first) = self.pending.first() {
            // A GOP may exceed the cap by a quarter before it is split, so that
            // timestamp jitter never splits a GOP whose nominal length equals the cap.
            let cap = self.max_fragment.saturating_add(self.max_fragment / 4);
            let reached_cap = frame.raw_ts.saturating_sub(first.raw_ts) >= cap;
            // A frozen or backwards RTP clock cannot disable the cap. Receipt
            // time and nominal sample time bound ordinary long GOPs; hard byte
            // and frame limits also cover bogus rate hints or receipt times.
            let nominal_span =
                (self.pending.len() as u64).saturating_mul(u64::from(self.nominal()));
            let received_span = frame.wall.micros_since(first.wall);
            let reached_budget = nominal_span >= u64::try_from(cap).unwrap_or(u64::MAX)
                || received_span >= cap.saturating_mul(100) / 9
                || self.pending.len() >= MAX_PENDING_FRAMES
                || self.pending_bytes.saturating_add(frame.data.len()) > MAX_PENDING_BYTES;
            if frame.is_sync || frame.new_params || reached_cap || reached_budget {
                completed = Some(self.close(frame.raw_ts));
            }
        }
        if self.pending.is_empty() {
            self.pending_entry = Arc::clone(&self.sample_entry);
        }
        self.pending_bytes += frame.data.len();
        self.pending.push(frame);
        completed
    }

    /// Marks that the next fragment starts a new run: the decode timeline
    /// restarts at zero and frames before the next sync frame are dropped.
    /// Any pending frames are discarded; flush first to keep them.
    pub fn mark_discontinuity(&mut self) {
        self.pending.clear();
        self.pending_bytes = 0;
        self.decode_time = 0;
        self.wall_anchor = None;
        self.new_run = true;
        self.started = false;
    }

    /// Ends recording continuity after the sink rejected a closed fragment.
    ///
    /// The first frame of the next fragment has already been pushed. Keep it
    /// only when it is a sync frame that can start an independent run.
    pub(crate) fn mark_sink_rejection(&mut self) {
        let keep_sync = self.pending.first().is_some_and(|frame| frame.is_sync);
        if !keep_sync {
            self.pending.clear();
            self.pending_bytes = 0;
        }
        self.decode_time = 0;
        self.wall_anchor = None;
        self.new_run = true;
        self.started = keep_sync;
    }

    /// Closes the pending fragment with nominal durations.
    pub fn flush(&mut self) -> Option<Fragment> {
        let last = self.pending.last()?;
        let nominal = i64::from(self.regularizer.nominal().unwrap_or(3000));
        let next_raw = last.raw_ts + nominal;
        Some(self.close(next_raw))
    }

    fn close(&mut self, next_raw: i64) -> Fragment {
        let frames = std::mem::take(&mut self.pending);
        self.pending_bytes = 0;
        let raw: Vec<i64> = frames.iter().map(|f| f.raw_ts).collect();
        let assignment = self.regularizer.assign(&raw, next_raw);
        if assignment.snapped {
            self.snaps += 1;
        }
        self.holes += u64::from(assignment.holes);
        self.hold_ticks += u64::try_from(assignment.hold_ticks).unwrap_or(0);
        self.last_hold_ticks = assignment.hold_ticks;
        let first = &frames[0];
        let raw_rtp = (first.raw_ts & 0xffff_ffff) as u32;
        let mut media_ticks = self.decode_time;
        let mut anchor_candidate = i64::MAX;
        for (frame, duration) in frames.iter().zip(&assignment.durations) {
            let candidate = frame
                .wall
                .get()
                .saturating_sub(ticks_to_micros(media_ticks));
            anchor_candidate = anchor_candidate.min(candidate);
            media_ticks = media_ticks.saturating_add(u64::from(*duration));
        }
        let anchor = self.wall_anchor.get_or_insert(WallAnchor {
            offset_us: anchor_candidate,
            media_ticks,
        });
        anchor.update(anchor_candidate, media_ticks);
        let wall = anchor.wall_at(self.decode_time);
        let samples: Vec<Sample> = frames
            .into_iter()
            .zip(assignment.durations)
            .map(|(f, duration)| Sample {
                data: f.data,
                duration,
                is_sync: f.is_sync,
            })
            .collect();
        let total: u64 = samples.iter().map(|s| u64::from(s.duration)).sum();
        let fragment = Fragment {
            samples,
            decode_time: self.decode_time,
            wall,
            raw_rtp,
            new_run: self.new_run,
            sample_entry: Arc::clone(&self.pending_entry),
        };
        self.decode_time += total;
        self.new_run = false;
        fragment
    }
}

#[cfg(test)]
mod tests {
    use iris_core::config::TimestampMode;
    use iris_core::time::MediaTicks;

    use super::*;

    fn entry() -> Arc<SampleEntry> {
        Arc::new(SampleEntry::new(
            Bytes::from_static(b"\0\0\0\x08hvc1"),
            "hvc1".into(),
            16,
            16,
        ))
    }

    fn assembler() -> FragmentAssembler {
        let options = IngestOptions::default();
        let regularizer = Regularizer::new(
            TimestampMode::Regularize,
            Some(3000),
            MediaTicks::from_millis(500),
        );
        FragmentAssembler::new(&options, regularizer, entry())
    }

    fn frame(index: i64, is_sync: bool) -> Frame {
        Frame {
            data: Bytes::from(vec![0, 0, 0, 2, 0x26, index as u8]),
            raw_ts: 5_000 + index * 3000,
            wall: WallUs::new(1_000_000 + index * 33_333),
            is_sync,
            new_params: false,
        }
    }

    #[test]
    fn drops_frames_before_first_sync_and_closes_at_sync() {
        let mut a = assembler();
        assert!(a.push(frame(0, false)).is_none());
        assert!(a.push(frame(1, false)).is_none());
        assert!(a.push(frame(2, true)).is_none());
        for i in 3..62 {
            assert!(a.push(frame(i, false)).is_none());
        }
        let fragment = a
            .push(frame(62, true))
            .expect("closes at the next sync frame");
        assert_eq!(fragment.samples.len(), 60);
        assert!(fragment.new_run);
        assert!(fragment.starts_with_sync());
        assert_eq!(fragment.decode_time, 0);
        assert!(
            fragment
                .wall
                .micros_since(WallUs::new(1_000_000 + 2 * 33_333))
                .abs()
                <= 100
        );
        assert_eq!(fragment.raw_rtp, (5_000 + 2 * 3000) as u32);
        assert!(fragment.samples.iter().all(|s| s.duration == 3000));
        assert_eq!(fragment.duration(), 60 * 3000);
        assert_eq!(fragment.sample_entry.hash, entry().hash);

        for i in 63..122 {
            assert!(a.push(frame(i, false)).is_none());
        }
        let second = a.push(frame(122, true)).expect("second fragment");
        assert!(!second.new_run);
        assert_eq!(second.decode_time, 60 * 3000);
        assert_eq!(a.snap_count(), 0);
    }

    #[test]
    fn frozen_rtp_without_sync_frames_still_closes_fragments() {
        let mut a = assembler();
        let mut completed = 0;
        for index in 0..1_000 {
            let mut incoming = frame(index, index == 0);
            incoming.raw_ts = 5_000;
            if let Some(fragment) = a.push(incoming) {
                assert!(fragment.samples.len() <= 75);
                completed += 1;
            }
        }
        assert!(
            completed > 0,
            "frozen RTP must not retain every received frame"
        );
        assert!(a.pending.len() <= 75);
    }

    #[test]
    fn assembly_has_a_frame_limit_independent_of_all_clocks() {
        let options = IngestOptions {
            max_fragment: MediaTicks::new(i64::MAX),
            ..IngestOptions::default()
        };
        let regularizer = Regularizer::new(
            TimestampMode::Regularize,
            Some(1),
            MediaTicks::from_millis(500),
        );
        let mut a = FragmentAssembler::new(&options, regularizer, entry());
        for index in 0..MAX_PENDING_FRAMES {
            let mut incoming = frame(0, index == 0);
            incoming.raw_ts = 0;
            assert!(a.push(incoming).is_none());
        }
        let fragment = a
            .push(frame(0, false))
            .expect("hard frame budget closes the fragment");
        assert_eq!(fragment.samples.len(), MAX_PENDING_FRAMES);
        assert_eq!(a.pending.len(), 1);
    }

    #[test]
    fn assembly_has_a_byte_limit_and_drops_oversized_access_units() {
        let mut a = assembler();
        let data = Bytes::from(vec![0; MAX_PENDING_BYTES / 2]);
        for index in 0..2 {
            let mut incoming = frame(index, index == 0);
            incoming.data = data.clone();
            assert!(a.push(incoming).is_none());
        }
        let fragment = a
            .push(frame(2, false))
            .expect("byte budget closes the fragment");
        assert_eq!(fragment.payload_len(), MAX_PENDING_BYTES as u64);
        assert!(a.pending_bytes < MAX_PENDING_BYTES);
        let mut oversized = frame(3, false);
        oversized.data = Bytes::from(vec![0; MAX_PENDING_BYTES + 1]);
        assert!(a.push(oversized).is_some());
        assert_eq!(a.pending_bytes, 0);
        assert!(a.push(frame(4, false)).is_none());
        assert!(a.last_provisional_decode_time().is_none());
        assert!(a.push(frame(5, true)).is_none());
        assert!(a.last_provisional_decode_time().is_some());
    }

    #[test]
    fn oversized_access_unit_preserves_pending_frames_and_starts_a_separate_run() {
        for is_sync in [false, true] {
            let mut a = assembler();
            for index in 0..10 {
                assert!(a.push(frame(index, index == 0)).is_none());
            }
            let mut oversized = frame(10, is_sync);
            oversized.data = Bytes::from(vec![0; MAX_PENDING_BYTES + 1]);
            let completed = a.push(oversized).expect("keeps frames before the loss");
            assert_eq!(completed.samples.len(), 10);
            assert!(
                completed
                    .samples
                    .iter()
                    .all(|sample| sample.duration == 3000)
            );
            assert_eq!(completed.duration(), 30_000);
            assert_eq!(completed.decode_time, 0);
            assert!(completed.wall.micros_since(frame(0, true).wall).abs() <= 100);
            assert!(completed.new_run);
            assert_eq!(a.pending_bytes, 0);
            assert!(a.last_provisional_decode_time().is_none());
            for index in 11..60 {
                assert!(a.push(frame(index, false)).is_none());
                assert!(a.last_provisional_decode_time().is_none());
            }
            for index in 60..120 {
                assert!(a.push(frame(index, index == 60)).is_none());
            }
            let resumed = a.push(frame(120, true)).expect("closes the resumed GOP");
            assert!(
                resumed.new_run,
                "discarded frames must leave a run boundary"
            );
            assert_eq!(resumed.decode_time, 0);
            assert_eq!(resumed.samples.len(), 60);
            assert!(resumed.starts_with_sync());
            assert!(resumed.wall.micros_since(frame(60, true).wall).abs() <= 100);
            assert_eq!(resumed.raw_rtp, frame(60, true).raw_ts as u32);
            assert_eq!(resumed.duration(), 180_000);
            let preceding_end = completed.wall.get() + 333_333;
            assert!(resumed.wall.get() > preceding_end);
            assert_eq!((a.holes(), a.hold_ticks(), a.snap_count()), (0, 0, 0));
            for index in 121..180 {
                assert!(a.push(frame(index, false)).is_none());
            }
            let following = a.push(frame(180, true)).expect("closes the following GOP");
            assert!(!following.new_run);
            assert_eq!(following.decode_time, resumed.duration());
        }
    }

    #[test]
    fn a_camera_gap_is_counted_as_holds() {
        let mut a = assembler();
        for i in 0..60 {
            assert!(a.push(frame(i, i == 0)).is_none());
        }
        assert_eq!((a.holes(), a.hold_ticks(), a.last_hold_ticks()), (0, 0, 0));
        // The next sync frame arrives thirty frame periods late.
        let fragment = a.push(frame(90, true)).expect("closes the first GOP");
        assert_eq!(fragment.samples[59].duration, 3000 + 30 * 3000);
        assert_eq!(
            (a.holes(), a.hold_ticks(), a.last_hold_ticks()),
            (30, 90_000, 90_000)
        );
        for i in 91..150 {
            assert!(a.push(frame(i, false)).is_none());
        }
        a.push(frame(150, true)).expect("closes the second GOP");
        assert_eq!(
            (a.holes(), a.hold_ticks(), a.last_hold_ticks()),
            (30, 90_000, 0)
        );
    }

    #[test]
    fn splits_long_gops_at_the_cap() {
        let mut a = assembler();
        assert!(a.push(frame(0, true)).is_none());
        let mut closed = Vec::new();
        for i in 1..300 {
            if let Some(f) = a.push(frame(i, false)) {
                closed.push(f);
            }
        }
        let fragment = a.push(frame(300, true)).unwrap();
        closed.push(fragment);
        // A 2 s cap with a quarter of slack splits a 10 s GOP every 75 frames.
        assert_eq!(closed.len(), 4);
        assert!(closed[0].starts_with_sync());
        assert!(closed[1..].iter().all(|f| !f.starts_with_sync()));
        assert!(closed.iter().all(|f| f.samples.len() == 75));
        let mut expected = 0;
        for f in &closed {
            assert_eq!(f.decode_time, expected);
            expected += f.duration();
        }
    }

    #[test]
    fn flush_and_discontinuity_restart_the_timeline() {
        let mut a = assembler();
        a.push(frame(0, true));
        a.push(frame(1, false));
        let flushed = a.flush().expect("pending frames");
        assert_eq!(flushed.samples.len(), 2);
        assert!(flushed.samples.iter().all(|s| s.duration == 3000));
        assert!(a.flush().is_none());
        a.mark_discontinuity();
        assert!(
            a.push(frame(10, false)).is_none(),
            "non-sync frame after discontinuity is dropped"
        );
        a.push(frame(11, true));
        let next = a.push(frame(12, true)).unwrap();
        assert!(next.new_run);
        assert_eq!(next.decode_time, 0);
    }

    #[test]
    fn new_parameters_close_the_pending_fragment_with_the_old_entry() {
        let mut a = assembler();
        a.push(frame(0, true));
        a.push(frame(1, false));
        let new_entry = Arc::new(SampleEntry::new(
            Bytes::from_static(b"\0\0\0\x08hvc2"),
            "hvc1".into(),
            32,
            32,
        ));
        a.set_sample_entry(Arc::clone(&new_entry));
        let mut f = frame(2, true);
        f.new_params = true;
        let old = a.push(f).unwrap();
        assert_eq!(old.sample_entry.hash, entry().hash);
        let next = a.push(frame(3, true)).unwrap();
        assert_eq!(next.sample_entry.hash, new_entry.hash);
    }

    #[test]
    fn sink_rejection_discards_dependent_frames_until_sync() {
        let mut a = assembler();
        assert!(a.push(frame(0, true)).is_none());
        for index in 1..75 {
            assert!(a.push(frame(index, false)).is_none());
        }
        let rejected = a.push(frame(75, false)).expect("cap split");
        assert!(rejected.starts_with_sync());
        assert!(!a.pending[0].is_sync);

        a.mark_sink_rejection();
        assert!(a.pending.is_empty());
        assert!(a.push(frame(76, false)).is_none());
        assert!(a.pending.is_empty());
        assert!(a.push(frame(77, true)).is_none());
        let resumed = a.push(frame(78, true)).expect("resumed run");
        assert!(resumed.starts_with_sync());
        assert!(resumed.new_run);
        assert_eq!(resumed.decode_time, 0);
    }

    #[test]
    fn sink_rejection_keeps_an_already_pending_sync_frame() {
        let mut a = assembler();
        for index in 0..60 {
            assert!(a.push(frame(index, index == 0)).is_none());
        }
        let rejected = a.push(frame(60, true)).expect("sync boundary");
        assert!(rejected.starts_with_sync());
        a.mark_sink_rejection();
        assert_eq!(a.pending.len(), 1);
        assert!(a.pending[0].is_sync);
        for index in 61..120 {
            assert!(a.push(frame(index, false)).is_none());
        }
        let resumed = a.push(frame(120, true)).expect("resumed run");
        assert_eq!(resumed.samples.len(), 60);
        assert!(resumed.starts_with_sync());
        assert!(resumed.new_run);
        assert_eq!(resumed.decode_time, 0);
    }

    #[test]
    fn wall_anchor_adopts_lower_delay_and_clamps_upward_motion() {
        let mut anchor = WallAnchor {
            offset_us: 1_000_000,
            media_ticks: 90_000,
        };
        anchor.update(2_000_000, 180_000);
        assert_eq!(anchor.offset_us, 1_000_500);
        anchor.update(900_000, 270_000);
        assert_eq!(anchor.offset_us, 900_000);
    }

    #[test]
    fn wall_anchor_keeps_transport_backlog_out_of_fragment_placement() {
        let mut a = assembler();
        assert!(a.push(frame(0, true)).is_none());
        assert!(a.push(frame(1, false)).is_none());
        let mut boundary = frame(2, true);
        boundary.wall = WallUs::new(2_000_000);
        let first = a.push(boundary).expect("first fragment");

        let mut delayed = frame(3, false);
        delayed.wall = WallUs::new(2_000_100);
        assert!(a.push(delayed).is_none());
        let mut next = frame(4, true);
        next.wall = WallUs::new(2_000_200);
        let second = a.push(next).expect("delayed fragment");
        let expected_us = i64::try_from(first.duration()).unwrap() * 100 / 9;
        assert!(
            (second.wall.micros_since(first.wall) - expected_us).abs() <= 100,
            "first={} second={}",
            first.wall,
            second.wall
        );
    }

    #[test]
    fn discontinuity_resets_the_wall_anchor() {
        let mut a = assembler();
        a.push(frame(0, true));
        a.push(frame(1, true)).expect("fragment");
        assert!(a.wall_anchor.is_some());
        a.mark_discontinuity();
        assert!(a.wall_anchor.is_none());
    }
}
