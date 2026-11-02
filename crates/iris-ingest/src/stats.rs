//! Per-stream counters, readable from any thread.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};

use iris_core::metrics::Peak;
use iris_core::time::WallUs;

/// Counters for one ingest stream, shared between the ingest task and the
/// daemon's statistics reporter.
///
/// Every accessor is a relaxed atomic load, so reading them never contends
/// with the ingest task. Byte counts are RTP payload bytes received; tick
/// counts are 90 kHz media ticks; peaks reset when taken.
#[derive(Debug)]
pub struct StreamStats {
    frames: AtomicU64,
    fragments: AtomicU64,
    bytes: AtomicU64,
    loss_events: AtomicU64,
    reconnects: AtomicU64,
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

    /// Payload bytes received.
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

    /// Gaps logged: reconnections with frames before them, and camera holds
    /// of a second or more.
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

    pub(crate) fn record_gap(&self) {
        self.gaps.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timing_peaks_reports_and_gaps_accumulate() {
        let s = StreamStats::new();
        assert_eq!(s.camera_offset_us(), None);
        assert_eq!(s.nominal_ticks(), None);
        assert_eq!(StreamStats::default().camera_offset_us(), None);

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

        s.record_fragment_size(1_000, 180_000);
        s.record_fragment_size(500, 90_000);
        assert_eq!(
            (s.fragment_bytes_peak().get(), s.fragment_ticks_peak().get()),
            (1_000, 180_000)
        );
        assert_eq!(s.fragment_bytes_peak().take(), 1_000);
        assert_eq!(s.fragment_bytes_peak().get(), 0);

        s.record_sender_report(-1_500);
        s.record_sender_report(2_500);
        assert_eq!((s.sender_reports(), s.camera_offset_us()), (2, Some(2_500)));

        s.record_gap();
        assert_eq!(s.gaps(), 1);
    }
}
