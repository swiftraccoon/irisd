//! Per-frame camera cadence, for diagnosing a camera that delivers under its
//! declared rate: one debug line per group of pictures with every frame's
//! step in camera time, its arrival step on the recorder's clock, and its size.
//!
//! For placing one stream against another, each group also carries the
//! least delay seen in it (the smallest receipt time less camera time, which
//! estimates where the stream's timestamps sit on the recorder's clock).
//!
//! Nothing is collected unless the `iris_ingest::cadence` target is enabled
//! at `debug`. Each RTCP sender report is logged with where it puts a
//! stream's timestamps on the camera's clock, under a target of its own,
//! `iris_ingest::reports`: a report every few seconds can be logged for
//! days, which every frame cannot.

use std::fmt::Write as _;

use iris_core::id::StreamKey;
use iris_core::time::WallUs;
use tracing::{Level, debug, enabled};

use crate::sync::epoch_us;

/// Frames kept of one group; a longer group is reported by its first frames
/// and its count.
const MAX_FRAMES: usize = 600;

/// One frame: its step in camera ticks and in arrival microseconds since the
/// frame before it (zero for the first of a session), and its size in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Step {
    ticks: i64,
    arrival_us: i64,
    bytes: usize,
}

/// Collects the frames of the group being received.
#[derive(Debug, Default)]
pub(crate) struct Cadence {
    last: Option<(i64, WallUs)>,
    steps: Vec<Step>,
    frames: usize,
    /// The smallest receipt time less camera time of the group, microseconds.
    least_delay_us: Option<i64>,
}

/// Microseconds of `ticks` at 90 kHz.
fn ticks_us(ticks: i64) -> i64 {
    ticks.saturating_mul(100) / 9
}

/// Logs one RTCP sender report of `key`: the instant on the camera's clock at
/// which the stream's RTP timestamp is zero (`epoch_us`, the report's NTP
/// time less its RTP timestamp at `clock_rate`), and how long after the
/// report's NTP time the recorder received it. Two streams of one camera
/// whose epochs are known are placed against each other without the
/// recorder's clock.
pub(crate) fn sender_report(
    key: &StreamKey,
    camera: WallUs,
    rtp: i64,
    clock_rate: u32,
    received: WallUs,
) {
    debug!(
        target: "iris_ingest::reports",
        stream = %key,
        epoch_us = epoch_us(camera, rtp, clock_rate),
        late_us = received.micros_since(camera),
        "sender report"
    );
}

impl Cadence {
    /// Notes one frame and, at a sync frame, reports the group before it.
    pub(crate) fn frame(
        &mut self,
        key: &StreamKey,
        raw_ts: i64,
        wall: WallUs,
        bytes: usize,
        is_sync: bool,
    ) {
        if !enabled!(target: "iris_ingest::cadence", Level::DEBUG) {
            if self.frames != 0 {
                *self = Self::default();
            }
            return;
        }
        if is_sync && self.frames != 0 {
            let (camera_ms, arrival_ms, kib) = columns(&self.steps);
            debug!(
                target: "iris_ingest::cadence",
                stream = %key,
                frames = self.frames,
                camera_ms,
                arrival_ms,
                kib,
                least_delay_us = self.least_delay_us.unwrap_or(0),
                "group"
            );
            self.steps.clear();
            self.frames = 0;
            self.least_delay_us = None;
        }
        let delay_us = wall.get().saturating_sub(ticks_us(raw_ts));
        self.least_delay_us = Some(
            self.least_delay_us
                .map_or(delay_us, |least| least.min(delay_us)),
        );
        let (ticks, arrival_us) = self.last.map_or((0, 0), |(last_ts, last_wall)| {
            (raw_ts.saturating_sub(last_ts), wall.micros_since(last_wall))
        });
        self.last = Some((raw_ts, wall));
        self.frames += 1;
        if self.steps.len() < MAX_FRAMES {
            self.steps.push(Step {
                ticks,
                arrival_us,
                bytes,
            });
        }
    }
}

/// The steps as three space-separated columns: camera milliseconds, arrival
/// milliseconds, and kibibytes, each rounded to the nearest whole number.
fn columns(steps: &[Step]) -> (String, String, String) {
    let mut camera_ms = String::with_capacity(steps.len() * 3);
    let mut arrival_ms = String::with_capacity(steps.len() * 3);
    let mut kib = String::with_capacity(steps.len() * 3);
    for (index, step) in steps.iter().enumerate() {
        let separator = if index == 0 { "" } else { " " };
        // Writing to a String cannot fail.
        let _ = write!(camera_ms, "{separator}{}", (step.ticks + 45).div_euclid(90));
        let _ = write!(
            arrival_ms,
            "{separator}{}",
            (step.arrival_us + 500).div_euclid(1_000)
        );
        let _ = write!(kib, "{separator}{}", (step.bytes + 512) / 1_024);
    }
    (camera_ms, arrival_ms, kib)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_round_each_step_to_whole_units() {
        let steps = [
            Step {
                ticks: 0,
                arrival_us: 0,
                bytes: 213_000,
            },
            Step {
                ticks: 2_700,
                arrival_us: 33_499,
                bytes: 511,
            },
            Step {
                ticks: 8_100,
                arrival_us: 90_500,
                bytes: 512,
            },
            Step {
                ticks: -900,
                arrival_us: -1_400,
                bytes: 1_536,
            },
        ];
        assert_eq!(
            columns(&steps),
            (
                "0 30 90 -10".to_owned(),
                "0 33 91 -1".to_owned(),
                "208 0 1 2".to_owned()
            )
        );
        assert_eq!(columns(&[]), (String::new(), String::new(), String::new()));
    }

    /// Without the debug target nothing is kept, whatever arrives.
    #[test]
    fn nothing_is_collected_while_the_target_is_off() {
        let key = StreamKey {
            camera: "cam1".parse().unwrap(),
            kind: iris_core::id::StreamKind::Main,
        };
        let mut cadence = Cadence::default();
        for index in 0..10 {
            cadence.frame(&key, index * 3_000, WallUs::new(index), 100, index == 0);
        }
        assert_eq!(cadence.frames, 0);
        assert_eq!(cadence.steps, []);
    }

    #[derive(Clone, Default)]
    struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("capture").extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// With the target at debug, a sync frame reports the group before it:
    /// every frame's steps and size, the first frame of the session at zero.
    #[test]
    fn a_group_is_reported_when_the_next_one_starts() {
        let captured = Captured::default();
        let writer = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new(
                "info,iris_ingest::cadence=debug",
            ))
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        let key = StreamKey {
            camera: "cam1".parse().unwrap(),
            kind: iris_core::id::StreamKind::Main,
        };
        tracing::subscriber::with_default(subscriber, || {
            let mut cadence = Cadence::default();
            let frames = [
                (0, 0, 200 * 1_024, true),
                (2_700, 30_000, 4 * 1_024, false),
                (10_800, 125_000, 50 * 1_024, false),
                (13_500, 150_000, 300 * 1_024, true),
                (16_200, 180_000, 1_024, false),
                (19_800, 220_000, 100 * 1_024, true),
            ];
            for (raw_ts, wall, bytes, is_sync) in frames {
                cadence.frame(&key, raw_ts, WallUs::new(wall), bytes, is_sync);
            }
        });
        let log = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines.len(), 2, "{log}");
        assert!(
            lines[0].contains(
                "group stream=cam1/main frames=3 camera_ms=\"0 30 90\" \
                 arrival_ms=\"0 30 95\" kib=\"200 4 50\""
            ),
            "{log}"
        );
        assert!(
            lines[1].contains(
                "group stream=cam1/main frames=2 camera_ms=\"30 30\" arrival_ms=\"25 30\" \
                 kib=\"300 1\""
            ),
            "{log}"
        );
    }

    /// Runs `body` with `target` at debug and returns what it logged.
    fn logged(target: &str, body: impl FnOnce()) -> String {
        let captured = Captured::default();
        let writer = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new(format!(
                "info,iris_ingest::{target}=debug"
            )))
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, body);
        String::from_utf8(captured.0.lock().unwrap().clone()).unwrap()
    }

    /// A group's least delay is the smallest receipt time less camera time
    /// among its frames: the frame that waited least on its way.
    #[test]
    fn the_least_delay_of_a_group_is_that_of_its_promptest_frame() {
        let key = StreamKey {
            camera: "cam1".parse().unwrap(),
            kind: iris_core::id::StreamKind::Main,
        };
        let log = logged("cadence", || {
            let mut cadence = Cadence::default();
            // Camera times 0, 33.333, and 66.666 ms, received 40, 5, and
            // 90 ms after an offset of one second.
            for (raw_ts, wall, is_sync) in [
                (0, 1_040_000, true),
                (3_000, 1_038_333, false),
                (6_000, 1_156_666, false),
                (9_000, 1_200_000, true),
            ] {
                cadence.frame(&key, raw_ts, WallUs::new(wall), 100, is_sync);
            }
        });
        assert!(log.contains("least_delay_us=1005000"), "{log}");
        assert_eq!(log.lines().count(), 1, "{log}");
    }

    /// A sender report places RTP timestamp zero on the camera's clock: its
    /// NTP time less its RTP timestamp at the stream's clock rate.
    #[test]
    fn a_sender_report_is_logged_with_its_epoch_and_lateness() {
        let key = StreamKey {
            camera: "cam1".parse().unwrap(),
            kind: iris_core::id::StreamKind::Audio,
        };
        // Two seconds of a 64 kHz clock, reported at ten seconds and
        // received 20 ms later.
        let report = || {
            sender_report(
                &key,
                WallUs::new(10_000_000),
                128_000,
                64_000,
                WallUs::new(10_020_000),
            );
        };
        let log = logged("reports", report);
        assert!(
            log.contains("sender report stream=cam1/audio epoch_us=8000000 late_us=20000"),
            "{log}"
        );
        // The reports have a target of their own.
        assert_eq!(logged("cadence", report), "");
    }

    /// A group longer than the cap is reported by its count and its first
    /// 600 frames, and what was collected is dropped when the target goes off.
    #[test]
    fn a_long_group_is_capped_and_collection_stops_with_the_target() {
        let key = StreamKey {
            camera: "cam1".parse().unwrap(),
            kind: iris_core::id::StreamKind::Main,
        };
        let mut cadence = Cadence::default();
        let log = logged("cadence", || {
            for index in 0..=700_i64 {
                let is_sync = index == 0 || index == 700;
                cadence.frame(
                    &key,
                    index * 3_000,
                    WallUs::new(index * 33_333),
                    10,
                    is_sync,
                );
            }
        });
        assert!(log.contains("frames=700 "), "{log}");
        let listed = log
            .split("camera_ms=\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .unwrap()
            .split(' ')
            .count();
        assert_eq!(listed, MAX_FRAMES);
        // The frame that closed the group is held; without the target it goes.
        assert_eq!(cadence.frames, 1);
        cadence.frame(&key, 701 * 3_000, WallUs::new(0), 10, false);
        assert_eq!((cadence.frames, cadence.steps.len()), (0, 0));
    }
}
