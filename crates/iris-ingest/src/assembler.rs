//! Groups frames into fragments and assigns their durations.

use std::sync::Arc;

use bytes::Bytes;
use iris_core::time::WallUs;
use iris_mp4::fragment::{Fragment, Sample};
use iris_mp4::init::SampleEntry;

use crate::regularize::Regularizer;
use crate::source::IngestOptions;

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
/// frame carrying new parameters, or once it spans `max_fragment` ticks of
/// camera time. Its durations are assigned when it closes, because the last
/// frame's duration is only known from the first timestamp of the next
/// fragment. Call [`FragmentAssembler::flush`] before
/// [`FragmentAssembler::mark_discontinuity`] when a session ends.
#[derive(Debug)]
pub struct FragmentAssembler {
    max_fragment: i64,
    regularizer: Regularizer,
    sample_entry: Arc<SampleEntry>,
    pending: Vec<Frame>,
    pending_entry: Arc<SampleEntry>,
    decode_time: u64,
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
            decode_time: 0,
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
    pub fn push(&mut self, frame: Frame) -> Option<Fragment> {
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
            let reached_cap =
                frame.raw_ts - first.raw_ts >= self.max_fragment + self.max_fragment / 4;
            if frame.is_sync || frame.new_params || reached_cap {
                completed = Some(self.close(frame.raw_ts));
            }
        }
        if self.pending.is_empty() {
            self.pending_entry = Arc::clone(&self.sample_entry);
        }
        self.pending.push(frame);
        completed
    }

    /// Marks that the next fragment starts a new run: the decode timeline
    /// restarts at zero and frames before the next sync frame are dropped.
    /// Any pending frames are discarded; flush first to keep them.
    pub fn mark_discontinuity(&mut self) {
        self.pending.clear();
        self.decode_time = 0;
        self.new_run = true;
        self.started = false;
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
        let raw: Vec<i64> = frames.iter().map(|f| f.raw_ts).collect();
        let assignment = self.regularizer.assign(&raw, next_raw);
        if assignment.snapped {
            self.snaps += 1;
        }
        self.holes += u64::from(assignment.holes);
        self.hold_ticks += u64::try_from(assignment.hold_ticks).unwrap_or(0);
        self.last_hold_ticks = assignment.hold_ticks;
        let first = &frames[0];
        let wall = first.wall;
        let raw_rtp = (first.raw_ts & 0xffff_ffff) as u32;
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
        assert_eq!(fragment.wall, WallUs::new(1_000_000 + 2 * 33_333));
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
}
