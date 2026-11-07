//! Time units: wall-clock microseconds and 90 kHz media ticks.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// The media timescale used throughout irisd, in ticks per second.
///
/// RFC 7798 requires a 90 kHz RTP clock for HEVC, and every recording uses the
/// same scale so that fragments from different runs compare directly.
pub const TIMESCALE: u32 = 90_000;

/// Seconds between the NTP epoch (1900-01-01) and the Unix epoch (1970-01-01).
const NTP_UNIX_OFFSET_SECS: u64 = 2_208_988_800;

/// A wall-clock instant in microseconds since the Unix epoch, taken from the
/// NVR's `CLOCK_REALTIME`.
///
/// This is the only trustworthy wall clock in the system; camera clocks are
/// recorded for diagnostics but never used for placement or search.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct WallUs(i64);

impl WallUs {
    /// Wraps a microsecond count.
    #[must_use]
    pub const fn new(micros: i64) -> Self {
        Self(micros)
    }

    /// Reads the system clock.
    ///
    /// # Panics
    ///
    /// Panics if the system clock reports a time before 1970, which indicates
    /// a broken host rather than a recoverable condition.
    #[must_use]
    pub fn now() -> Self {
        let since_epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is set before the Unix epoch");
        Self(i64::try_from(since_epoch.as_micros()).expect("microseconds overflow i64"))
    }

    /// Returns the microsecond count.
    #[must_use]
    pub const fn get(self) -> i64 {
        self.0
    }

    /// Returns the value as floating-point seconds since the Unix epoch.
    #[must_use]
    pub fn as_secs_f64(self) -> f64 {
        self.0 as f64 / 1e6
    }

    /// Converts to a 64-bit NTP timestamp (seconds since 1900 in the high 32
    /// bits, binary fraction in the low 32 bits), the format used by RTCP
    /// sender reports and the `prft` box.
    ///
    /// Times before the Unix epoch saturate to zero.
    #[must_use]
    pub fn to_ntp64(self) -> u64 {
        let micros = u64::try_from(self.0).unwrap_or(0);
        let secs = micros / 1_000_000 + NTP_UNIX_OFFSET_SECS;
        let frac_micros = micros % 1_000_000;
        let frac = (frac_micros << 32) / 1_000_000;
        (secs << 32) | frac
    }

    /// Converts a 64-bit NTP timestamp to wall time, the inverse of
    /// [`WallUs::to_ntp64`]. The fraction is rounded to the nearest microsecond,
    /// so a value produced by [`WallUs::to_ntp64`] (which truncates) converts back
    /// exactly. Times before 1970 saturate to zero.
    #[must_use]
    pub fn from_ntp64(ntp: u64) -> Self {
        let Some(secs) = (ntp >> 32).checked_sub(NTP_UNIX_OFFSET_SECS) else {
            return Self(0);
        };
        let frac_micros = ((ntp & 0xffff_ffff) * 1_000_000 + (1 << 31)) >> 32;
        Self(i64::try_from(secs * 1_000_000 + frac_micros).unwrap_or(i64::MAX))
    }

    /// Splits the instant into a UTC civil date and time of day.
    #[must_use]
    pub fn to_civil(self) -> CivilTime {
        let secs = self.0.div_euclid(1_000_000);
        let days = secs.div_euclid(86_400);
        let sod = secs.rem_euclid(86_400);
        let (year, month, day) = civil_from_days(days);
        CivilTime {
            year,
            month,
            day,
            hour: (sod / 3600) as u32,
            minute: ((sod % 3600) / 60) as u32,
            second: (sod % 60) as u32,
            micros: self.0.rem_euclid(1_000_000) as u32,
        }
    }

    /// Adds a signed number of microseconds.
    #[must_use]
    pub const fn offset(self, micros: i64) -> Self {
        Self(self.0 + micros)
    }

    /// Returns `self - other` in microseconds.
    #[must_use]
    pub const fn micros_since(self, other: Self) -> i64 {
        self.0 - other.0
    }
}

/// A UTC civil date and time of day.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CivilTime {
    /// Proleptic Gregorian year.
    pub year: i64,
    /// Month, 1-12.
    pub month: u32,
    /// Day of month, 1-31.
    pub day: u32,
    /// Hour, 0-23.
    pub hour: u32,
    /// Minute, 0-59.
    pub minute: u32,
    /// Second, 0-59.
    pub second: u32,
    /// Microsecond within the second.
    pub micros: u32,
}

impl fmt::Display for WallUs {
    /// Formats as `YYYY-MM-DDTHH:MM:SS.ffffffZ` without pulling in a calendar
    /// crate; the civil-date conversion follows Howard Hinnant's algorithm.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let c = self.to_civil();
        write!(
            f,
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:06}Z",
            c.year, c.month, c.day, c.hour, c.minute, c.second, c.micros
        )
    }
}

/// Converts days since 1970-01-01 to a proleptic Gregorian civil date.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (
        year,
        u32::try_from(m).expect("month fits u32"),
        u32::try_from(d).expect("day fits u32"),
    )
}

/// A duration or position in 90 kHz media ticks ([`TIMESCALE`]).
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct MediaTicks(i64);

impl MediaTicks {
    /// Wraps a tick count.
    #[must_use]
    pub const fn new(ticks: i64) -> Self {
        Self(ticks)
    }

    /// Converts whole seconds to ticks.
    #[must_use]
    pub const fn from_secs(secs: i64) -> Self {
        Self(secs * TIMESCALE as i64)
    }

    /// Converts milliseconds to ticks (exact: one millisecond is 90 ticks).
    #[must_use]
    pub const fn from_millis(millis: i64) -> Self {
        Self(millis * (TIMESCALE as i64 / 1000))
    }

    /// Converts nonnegative milliseconds to ticks, returning `None` when the
    /// input or exact multiplication cannot fit in the signed tick unit.
    #[must_use]
    pub fn checked_from_millis(millis: u64) -> Option<Self> {
        let millis = i64::try_from(millis).ok()?;
        millis.checked_mul(i64::from(TIMESCALE) / 1_000).map(Self)
    }

    /// Returns the tick count.
    #[must_use]
    pub const fn get(self) -> i64 {
        self.0
    }

    /// Returns the value in floating-point seconds.
    #[must_use]
    pub fn as_secs_f64(self) -> f64 {
        self.0 as f64 / f64::from(TIMESCALE)
    }

    /// Returns the value in microseconds, rounded toward negative infinity.
    #[must_use]
    pub const fn as_micros(self) -> i64 {
        // 90 kHz ticks to microseconds: multiply by 1000 / 90 = 100 / 9.
        (self.0 * 100).div_euclid(9)
    }
}

impl std::ops::Add for MediaTicks {
    type Output = Self;

    fn add(self, rhs: Self) -> Self {
        Self(self.0 + rhs.0)
    }
}

impl std::ops::Sub for MediaTicks {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self {
        Self(self.0 - rhs.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pre_unix_epoch_ntp_fractions_saturate_to_zero() {
        for fraction in [0, 1 << 31, 0xffff_ffff] {
            let ntp = ((NTP_UNIX_OFFSET_SECS - 1) << 32) | fraction;
            assert_eq!(WallUs::from_ntp64(ntp), WallUs::new(0));
        }
        assert_eq!(
            WallUs::from_ntp64((NTP_UNIX_OFFSET_SECS << 32) | (1 << 31)),
            WallUs::new(500_000)
        );
        let last_microsecond = WallUs::new(999_999);
        assert_eq!(
            WallUs::from_ntp64(last_microsecond.to_ntp64()),
            last_microsecond
        );
    }

    #[test]
    fn formats_rfc3339_utc() {
        // 2026-10-31T15:35:58.000123Z
        let t = WallUs::new(1_793_460_958_000_123);
        assert_eq!(t.to_string(), "2026-10-31T15:35:58.000123Z");
        assert_eq!(WallUs::new(0).to_string(), "1970-01-01T00:00:00.000000Z");
    }

    #[test]
    fn ntp_conversion_matches_epoch_offset() {
        let unix_epoch = WallUs::new(0).to_ntp64();
        assert_eq!(unix_epoch >> 32, NTP_UNIX_OFFSET_SECS);
        assert_eq!(unix_epoch & 0xffff_ffff, 0);
        let half_second = WallUs::new(500_000).to_ntp64();
        assert_eq!(half_second & 0xffff_ffff, 1 << 31);
    }

    #[test]
    fn every_microsecond_fraction_round_trips_exactly() {
        // Stepping through a second in 997 µs strides covers both residues that
        // truncation used to lose; the two ends are checked explicitly.
        let base = 1_761_955_200_000_000i64;
        for offset in (0..1_000_000).step_by(997).chain([999_999, 379_376]) {
            let wall = WallUs::new(base + offset);
            assert_eq!(WallUs::from_ntp64(wall.to_ntp64()), wall, "offset {offset}");
        }
        assert_eq!(
            WallUs::from_ntp64(0xffff_ffff | (NTP_UNIX_OFFSET_SECS << 32)),
            WallUs::new(1_000_000)
        );
    }

    #[test]
    fn ntp_round_trips() {
        let t = WallUs::new(1_793_460_958_000_123);
        let back = WallUs::from_ntp64(t.to_ntp64());
        assert!((back.get() - t.get()).abs() <= 1, "{back:?} vs {t:?}");
    }

    #[test]
    fn ticks_convert_to_micros_exactly() {
        assert_eq!(MediaTicks::from_secs(1).as_micros(), 1_000_000);
        assert_eq!(MediaTicks::new(90).as_micros(), 1000);
        assert_eq!(MediaTicks::from_millis(33).get(), 2970);
    }

    #[test]
    fn checked_millisecond_conversion_has_an_exact_boundary() {
        let largest = u64::try_from(i64::MAX / 90).unwrap();
        assert_eq!(
            MediaTicks::checked_from_millis(largest),
            Some(MediaTicks::new((i64::MAX / 90) * 90))
        );
        assert_eq!(MediaTicks::checked_from_millis(largest + 1), None);
        assert_eq!(MediaTicks::checked_from_millis(u64::MAX), None);
    }
}
