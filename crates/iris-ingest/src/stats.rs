//! Per-stream counters, readable from any thread.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};

use iris_core::metrics::Peak;
use iris_core::time::WallUs;

use crate::steer::Side;

/// Counters for one ingest stream, shared between the ingest task and the
/// daemon's statistics reporter.
///
/// Every counter is a relaxed atomic, so reading it never contends with the
/// ingest task; the codec description sits behind a mutex held only to copy a
/// short string. Byte counts are access-unit bytes received (4-byte length
/// prefixes included, parameter sets excluded); tick counts are 90 kHz media
/// ticks; peaks reset when taken.
#[derive(Debug)]
pub struct StreamStats {
    frames: AtomicU64,
    fragments: AtomicU64,
    bytes: AtomicU64,
    loss_events: AtomicU64,
    reconnects: AtomicU64,
    beats: AtomicU64,
    snaps: AtomicU64,
    dropped_sink: AtomicU64,
    connected: AtomicBool,
    /// Zero means "no frame yet".
    last_frame_wall: AtomicI64,
    codec: Mutex<Option<String>>,
    /// Camera-dropped frames rendered as holds.
    holds: AtomicU64,
    /// Ticks rendered as holds.
    hold_ticks: AtomicU64,
    /// Nominal frame duration in use; zero while unknown.
    nominal_ticks: AtomicU32,
    /// Assigned timeline minus camera timeline, in ticks.
    error_ticks: AtomicI64,
    fragment_bytes_peak: Peak,
    fragment_ticks_peak: Peak,
    sender_reports: AtomicU64,
    /// Camera clock minus NVR clock at the last sender report; `i64::MIN` while none.
    camera_offset_us: AtomicI64,
    gaps: AtomicU64,
    /// The watches of parted clocks, in microseconds, each from the window
    /// that closed last; `i64::MIN` while none has.
    clock_moved_us: AtomicI64,
    audio_stamps_moved_us: AtomicI64,
    audio_sync_us: AtomicI64,
    audio_sync_moved_us: AtomicI64,
    audio_arrival_moved_us: AtomicI64,
    /// Frames the rule of `docs/research/13-audio.md` §5.3.4 named to be
    /// left out, and frames it named to be recorded twice.
    sound_steps_said: [AtomicU64; 2],
    /// Frames left out of the recording and frames recorded twice by steps.
    sound_steps_made: [AtomicU64; 2],
}

/// Audio frames of steps of the drift rule
/// ([`StreamStats::sound_steps_said`], [`StreamStats::sound_steps_made`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SoundSteps {
    /// Frames left out, where the recording had the sound late.
    pub left_out: u64,
    /// Frames recorded twice, where it had the sound early.
    pub recorded_twice: u64,
}

/// A gauge that reads `None` until it is set.
fn gauge(value: &AtomicI64) -> Option<i64> {
    match value.load(Ordering::Relaxed) {
        i64::MIN => None,
        set => Some(set),
    }
}

impl Default for StreamStats {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamStats {
    /// Creates zeroed counters.
    #[must_use]
    pub fn new() -> Self {
        Self {
            frames: AtomicU64::new(0),
            fragments: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            loss_events: AtomicU64::new(0),
            reconnects: AtomicU64::new(0),
            beats: AtomicU64::new(0),
            snaps: AtomicU64::new(0),
            dropped_sink: AtomicU64::new(0),
            connected: AtomicBool::new(false),
            last_frame_wall: AtomicI64::new(0),
            codec: Mutex::new(None),
            holds: AtomicU64::new(0),
            hold_ticks: AtomicU64::new(0),
            nominal_ticks: AtomicU32::new(0),
            error_ticks: AtomicI64::new(0),
            fragment_bytes_peak: Peak::new(),
            fragment_ticks_peak: Peak::new(),
            sender_reports: AtomicU64::new(0),
            camera_offset_us: AtomicI64::new(i64::MIN),
            clock_moved_us: AtomicI64::new(i64::MIN),
            audio_stamps_moved_us: AtomicI64::new(i64::MIN),
            audio_sync_us: AtomicI64::new(i64::MIN),
            audio_sync_moved_us: AtomicI64::new(i64::MIN),
            audio_arrival_moved_us: AtomicI64::new(i64::MIN),
            sound_steps_said: [AtomicU64::new(0), AtomicU64::new(0)],
            sound_steps_made: [AtomicU64::new(0), AtomicU64::new(0)],
            gaps: AtomicU64::new(0),
        }
    }

    /// Video frames received.
    pub fn frames(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }

    /// Fragments handed to the sink.
    pub fn fragments(&self) -> u64 {
        self.fragments.load(Ordering::Relaxed)
    }

    /// Access-unit bytes received: 4-byte length prefixes included, parameter
    /// sets excluded.
    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    /// Frames that reported RTP packet loss before them.
    pub fn loss_events(&self) -> u64 {
        self.loss_events.load(Ordering::Relaxed)
    }

    /// Sessions that ended with an error and were retried.
    pub fn reconnects(&self) -> u64 {
        self.reconnects.load(Ordering::Relaxed)
    }

    /// How often the stream's task has noted that it is alive: at every
    /// item it reads, every bounded wait on a quiet camera that ends, every
    /// step of a session's set-up, and through every pause between
    /// attempts. A healthy task goes at most a step of set-up and a slice of a
    /// pause without one (20 s); a count that stands still through a whole
    /// check of the daemon's watchdog is a task that is stuck, whatever its
    /// camera does.
    pub fn beats(&self) -> u64 {
        self.beats.load(Ordering::Relaxed)
    }

    /// Fragments whose timing was snapped to the camera timeline.
    pub fn snaps(&self) -> u64 {
        self.snaps.load(Ordering::Relaxed)
    }

    /// Fragments the sink rejected.
    pub fn dropped_sink(&self) -> u64 {
        self.dropped_sink.load(Ordering::Relaxed)
    }

    /// Whether a session is playing.
    pub fn connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// Wall time of the most recent frame.
    pub fn last_frame_wall(&self) -> Option<WallUs> {
        match self.last_frame_wall.load(Ordering::Relaxed) {
            0 => None,
            micros => Some(WallUs::new(micros)),
        }
    }

    /// Codec and dimensions, for example `hvc1.1.6.L150.B0 3840x2160`.
    pub fn codec_description(&self) -> Option<String> {
        self.codec.lock().map_or(None, |guard| guard.clone())
    }

    /// Camera-dropped frames the regulariser rendered as holds.
    pub fn holds(&self) -> u64 {
        self.holds.load(Ordering::Relaxed)
    }

    /// Media ticks rendered as holds.
    pub fn hold_ticks(&self) -> u64 {
        self.hold_ticks.load(Ordering::Relaxed)
    }

    /// The nominal frame duration in use, in ticks, once known.
    pub fn nominal_ticks(&self) -> Option<u32> {
        match self.nominal_ticks.load(Ordering::Relaxed) {
            0 => None,
            ticks => Some(ticks),
        }
    }

    /// Assigned timeline minus camera timeline after the last fragment, in ticks.
    pub fn error_ticks(&self) -> i64 {
        self.error_ticks.load(Ordering::Relaxed)
    }

    /// Largest fragment payload, in bytes, since the peak was last taken.
    pub fn fragment_bytes_peak(&self) -> &Peak {
        &self.fragment_bytes_peak
    }

    /// Longest fragment, in ticks, since the peak was last taken.
    pub fn fragment_ticks_peak(&self) -> &Peak {
        &self.fragment_ticks_peak
    }

    /// RTCP sender reports received.
    pub fn sender_reports(&self) -> u64 {
        self.sender_reports.load(Ordering::Relaxed)
    }

    /// Camera clock minus NVR clock, in microseconds, at the last sender report.
    pub fn camera_offset_us(&self) -> Option<i64> {
        match self.camera_offset_us.load(Ordering::Relaxed) {
            i64::MIN => None,
            micros => Some(micros),
        }
    }

    /// How far the stream's timestamps have moved against the recorder's
    /// clock since the session's first window, in microseconds: the least
    /// receipt time less timestamp of the window that closed last, less
    /// that of the first. It follows the camera's clock rate against the
    /// recorder's and any change in how long a frame takes to arrive.
    pub fn clock_moved_us(&self) -> Option<i64> {
        gauge(&self.clock_moved_us)
    }

    /// How far the timestamps of the audio recorded with this stream have
    /// moved against its frame count since the audio run's first window, in
    /// microseconds, by the least of each window.
    pub fn audio_stamps_moved_us(&self) -> Option<i64> {
        gauge(&self.audio_stamps_moved_us)
    }

    /// How much later the recording has the audio against the video, in
    /// microseconds, over the last window that read it: the lower tenth of
    /// the video's epoch less the audio's by each pair of sender reports,
    /// plus the median video timestamp against its decode time, less the
    /// least audio timestamp against its decode time. `None` while no window has
    /// read it, as for a camera that sends no sender reports for both
    /// streams.
    pub fn audio_sync_us(&self) -> Option<i64> {
        gauge(&self.audio_sync_us)
    }

    /// [`Self::audio_sync_us`] less that of the audio run's first window.
    pub fn audio_sync_moved_us(&self) -> Option<i64> {
        gauge(&self.audio_sync_moved_us)
    }

    /// How far the audio has moved against the video since the run's first
    /// window by when the frames of each arrived, in microseconds: the
    /// least receipt time less decode time of the video's frames, less
    /// that of the audio's. Positive when the sound has become later. It
    /// needs no sender reports.
    pub fn audio_arrival_moved_us(&self) -> Option<i64> {
        gauge(&self.audio_arrival_moved_us)
    }

    /// The audio frames the drift rule named to be left out and to be
    /// recorded twice, whether its steps are made or only said
    /// (`docs/research/13-audio.md` §5.3.6).
    pub fn sound_steps_said(&self) -> SoundSteps {
        SoundSteps {
            left_out: self.sound_steps_said[0].load(Ordering::Relaxed),
            recorded_twice: self.sound_steps_said[1].load(Ordering::Relaxed),
        }
    }

    /// The audio frames that steps left out of the recording and the frames
    /// they recorded twice, counted as each arriving frame is left out or
    /// taken twice; a second record dropped again because the video started
    /// a run before it was cut stays counted. None where the rule only says
    /// its steps
    /// ([`IngestOptions::audio_steps`](crate::IngestOptions::audio_steps)).
    pub fn sound_steps_made(&self) -> SoundSteps {
        SoundSteps {
            left_out: self.sound_steps_made[0].load(Ordering::Relaxed),
            recorded_twice: self.sound_steps_made[1].load(Ordering::Relaxed),
        }
    }

    /// Gaps logged: reconnections with frames before them, camera holds of a
    /// second or more, and the first sink rejection of each loss epoch.
    pub fn gaps(&self) -> u64 {
        self.gaps.load(Ordering::Relaxed)
    }

    pub(crate) fn record_frame(&self, bytes: u64, wall: WallUs, lost: bool) {
        self.frames.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
        self.last_frame_wall.store(wall.get(), Ordering::Relaxed);
        if lost {
            self.loss_events.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn record_fragment(&self, accepted: bool) {
        self.fragments.fetch_add(1, Ordering::Relaxed);
        if !accepted {
            self.dropped_sink.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn add_snaps(&self, n: u64) {
        self.snaps.fetch_add(n, Ordering::Relaxed);
    }

    pub(crate) fn record_reconnect(&self) {
        self.reconnects.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn beat(&self) {
        self.beats.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn set_connected(&self, connected: bool) {
        self.connected.store(connected, Ordering::Relaxed);
    }

    pub(crate) fn set_codec_description(&self, description: String) {
        if let Ok(mut guard) = self.codec.lock() {
            *guard = Some(description);
        }
    }

    /// Adds holds and stores the regulariser's state after a fragment closed.
    pub(crate) fn record_timing(
        &self,
        holes: u64,
        hold_ticks: u64,
        nominal: Option<u32>,
        error_ticks: i64,
    ) {
        self.holds.fetch_add(holes, Ordering::Relaxed);
        self.hold_ticks.fetch_add(hold_ticks, Ordering::Relaxed);
        self.nominal_ticks
            .store(nominal.unwrap_or(0), Ordering::Relaxed);
        self.error_ticks.store(error_ticks, Ordering::Relaxed);
    }

    pub(crate) fn record_fragment_size(&self, bytes: u64, ticks: u64) {
        self.fragment_bytes_peak.observe(bytes);
        self.fragment_ticks_peak.observe(ticks);
    }

    pub(crate) fn record_sender_report(&self, offset_us: i64) {
        self.sender_reports.fetch_add(1, Ordering::Relaxed);
        self.camera_offset_us.store(offset_us, Ordering::Relaxed);
    }

    pub(crate) fn record_clock_moved(&self, micros: i64) {
        self.clock_moved_us.store(micros, Ordering::Relaxed);
    }

    pub(crate) fn record_audio_stamps_moved(&self, micros: i64) {
        self.audio_stamps_moved_us.store(micros, Ordering::Relaxed);
    }

    pub(crate) fn record_audio_sync(&self, late_us: i64, moved_us: i64) {
        self.audio_sync_us.store(late_us, Ordering::Relaxed);
        self.audio_sync_moved_us.store(moved_us, Ordering::Relaxed);
    }

    pub(crate) fn record_audio_arrival_moved(&self, micros: i64) {
        self.audio_arrival_moved_us.store(micros, Ordering::Relaxed);
    }

    pub(crate) fn record_sound_step_said(&self, side: Side, frames: u32) {
        let index = match side {
            Side::Late => 0,
            Side::Early => 1,
        };
        self.sound_steps_said[index].fetch_add(u64::from(frames), Ordering::Relaxed);
    }

    /// Counts one frame left out (`Side::Late`) or recorded twice.
    pub(crate) fn record_sound_step_made(&self, side: Side) {
        let index = match side {
            Side::Late => 0,
            Side::Early => 1,
        };
        self.sound_steps_made[index].fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_gap(&self) {
        self.gaps.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_start_empty() {
        let s = StreamStats::new();
        assert_eq!(s.camera_offset_us(), None);
        assert_eq!(s.nominal_ticks(), None);
        assert_eq!(StreamStats::default().camera_offset_us(), None);
        assert_eq!((s.frames(), s.bytes(), s.gaps()), (0, 0, 0));
        assert!(!s.connected());
    }

    #[test]
    fn timing_accumulates_holds_and_keeps_the_last_regulariser_state() {
        let s = StreamStats::new();
        s.record_timing(2, 6_000, Some(3000), -40);
        s.record_timing(1, 3_000, Some(3000), 10);
        assert_eq!(
            (
                s.holds(),
                s.hold_ticks(),
                s.nominal_ticks(),
                s.error_ticks()
            ),
            (3, 9_000, Some(3000), 10)
        );
        s.record_timing(0, 0, None, 0);
        assert_eq!(s.nominal_ticks(), None);
    }

    #[test]
    fn fragment_peaks_reset_when_taken() {
        let s = StreamStats::new();
        s.record_fragment_size(1_000, 180_000);
        s.record_fragment_size(500, 90_000);
        assert_eq!(
            (s.fragment_bytes_peak().get(), s.fragment_ticks_peak().get()),
            (1_000, 180_000)
        );
        assert_eq!(s.fragment_bytes_peak().take(), 1_000);
        assert_eq!(s.fragment_bytes_peak().get(), 0);
    }

    #[test]
    fn sender_reports_keep_the_last_camera_offset() {
        let s = StreamStats::new();
        s.record_sender_report(-1_500);
        s.record_sender_report(2_500);
        assert_eq!((s.sender_reports(), s.camera_offset_us()), (2, Some(2_500)));
    }

    #[test]
    fn frames_fragments_gaps_and_connection_state_are_recorded() {
        let s = StreamStats::new();
        s.record_frame(100, WallUs::new(7), true);
        s.record_frame(50, WallUs::new(9), false);
        s.record_fragment(true);
        s.record_fragment(false);
        s.record_reconnect();
        s.record_gap();
        s.set_connected(true);
        assert_eq!((s.frames(), s.bytes(), s.loss_events()), (2, 150, 1));
        assert_eq!(s.last_frame_wall(), Some(WallUs::new(9)));
        assert_eq!((s.fragments(), s.dropped_sink()), (2, 1));
        assert_eq!((s.reconnects(), s.gaps()), (1, 1));
        assert!(s.connected());
    }
}
