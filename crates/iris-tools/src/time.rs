//! Timestamps as the daemon logs them: RFC 3339 in UTC with a `Z`. Instants
//! are `f64` seconds since the Unix epoch, the unit the reports compute rates
//! and regressions in.
//!
//! The `tools_protocol` and `tools_logs` fuzz targets include this file by
//! path, so it uses nothing outside the standard library.

use std::time::{SystemTime, UNIX_EPOCH};

/// Parses the RFC 3339 subset the daemon writes,
/// `YYYY-MM-DDTHH:MM:SS[.fraction](Z|+HH:MM|-HH:MM)` with an upper-case `T`
/// and `Z`, into seconds since the Unix epoch. A leap second (`:60`), lower-case
/// designators, surrounding whitespace, and every other form give `None`.
pub fn parse_seconds(text: &str) -> Option<f64> {
    let b = text.as_bytes();
    if b.len() < 20
        || b.get(4) != Some(&b'-')
        || b.get(7) != Some(&b'-')
        || b.get(10) != Some(&b'T')
        || b.get(13) != Some(&b':')
        || b.get(16) != Some(&b':')
    {
        return None;
    }
    let num = |s: &str| {
        s.as_bytes()
            .iter()
            .all(u8::is_ascii_digit)
            .then(|| s.parse::<i64>().ok())
            .flatten()
    };
    let year = num(text.get(0..4)?)?;
    let month = num(text.get(5..7)?)?;
    let day = num(text.get(8..10)?)?;
    let hour = num(text.get(11..13)?)?;
    let minute = num(text.get(14..16)?)?;
    let second = num(text.get(17..19)?)?;
    if !(1..=9999).contains(&year) || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let mut rest = text.get(19..)?;
    let mut fraction = 0.0;
    if let Some(after) = rest.strip_prefix('.') {
        let digits_len = after.bytes().take_while(u8::is_ascii_digit).count();
        if digits_len == 0 {
            return None;
        }
        let digits = after.get(..digits_len)?;
        fraction = format!("0.{digits}").parse::<f64>().ok()?;
        rest = after.get(digits_len..)?;
    }
    let offset = match rest {
        "Z" => 0,
        other => {
            if other.len() != 6 || other.as_bytes().get(3) != Some(&b':') {
                return None;
            }
            let sign = match other.as_bytes().first()? {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let offset_hours = num(other.get(1..3)?)?;
            let offset_minutes = num(other.get(4..6)?)?;
            if offset_hours > 23 || offset_minutes > 59 {
                return None;
            }
            sign * (offset_hours * 3600 + offset_minutes * 60)
        }
    };
    let days = days_from_civil(year, month, day)?;
    let whole = days
        .checked_mul(86_400)?
        .checked_add(hour.checked_mul(3_600)?)?
        .checked_add(minute.checked_mul(60)?)?
        .checked_add(second)?
        .checked_sub(offset)?;
    Some(whole as f64 + fraction)
}

/// Formats seconds since the epoch as `YYYY-MM-DDTHH:MM:SSZ`, rounding down to
/// the whole second.
pub fn iso(seconds: f64) -> String {
    let total = seconds.floor() as i64;
    let days = total.div_euclid(86_400);
    let rem = total.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Days since 1970-01-01 for a proleptic Gregorian date; `None` for an invalid
/// month or day, or when the count overflows.
pub fn days_from_civil(y: i64, m: i64, d: i64) -> Option<i64> {
    if !(1..=12).contains(&m) {
        return None;
    }
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let max_day = match m {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if !(1..=max_day).contains(&d) {
        return None;
    }
    let y = if m <= 2 { y.checked_sub(1)? } else { y };
    let era = y.div_euclid(400);
    let yoe = y.checked_sub(era.checked_mul(400)?)?;
    let mp = (m + 9) % 12;
    let doy = 153i64
        .checked_mul(mp)?
        .checked_add(2)?
        .checked_div(5)?
        .checked_add(d)?
        .checked_sub(1)?;
    let doe = yoe
        .checked_mul(365)?
        .checked_add(yoe / 4)?
        .checked_sub(yoe / 100)?
        .checked_add(doy)?;
    era.checked_mul(146_097)?
        .checked_add(doe)?
        .checked_sub(719_468)
}

/// The civil date of a day count since 1970-01-01. [`iso`] passes at most
/// `i64::MAX / 86_400` days either side of the epoch, far inside the range the
/// arithmetic holds.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// The current time as seconds since the epoch; 0.0 when the clock reads
/// before 1970.
pub fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |elapsed| elapsed.as_secs_f64())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_what_it_parses() {
        let seconds = parse_seconds("2026-11-02T01:44:23Z").unwrap();
        assert_eq!(iso(seconds), "2026-11-02T01:44:23Z");
    }

    #[test]
    fn the_epoch_is_zero() {
        assert_eq!(parse_seconds("1970-01-01T00:00:00Z"), Some(0.0));
    }

    #[test]
    fn a_fraction_adds_to_the_whole_seconds() {
        let whole = parse_seconds("2026-11-02T01:44:23Z").unwrap();
        assert_eq!(parse_seconds("2026-11-02T01:44:23.5Z"), Some(whole + 0.5));
    }

    #[test]
    fn an_offset_converts_to_utc() {
        assert_eq!(
            parse_seconds("2026-11-02T03:44:23+02:00"),
            parse_seconds("2026-11-02T01:44:23Z")
        );
    }

    #[test]
    fn a_leap_day_with_a_negative_offset_parses() {
        assert_eq!(
            parse_seconds("2024-02-29T23:59:59.25-05:30"),
            parse_seconds("2024-03-01T05:29:59.25Z")
        );
    }

    #[test]
    fn rejects_other_forms_and_invalid_calendar_values() {
        for invalid in [
            "nope",
            "2026-11-02 01:44:23Z",
            "2026/11/02T01:44:23Z",
            "2026-02-29T01:44:23Z",
            "2024-02-30T01:44:23Z",
            "2026-11-02T24:00:00Z",
            "2026-11-02T00:60:00Z",
            "2026-11-02T00:00:60Z",
            "2026-11-02T00:00:00",
            "2026-11-02T00:00:00z",
            "2026-11-02t00:00:00Z",
            "2026-11-02T00:00:00+24:00",
            "2026-11-02T00:00:00Zjunk",
            "2026-11-02T00:00:00.Z",
            " 2026-11-02T00:00:00Z",
            "2026-11-02T00:00:00Z ",
        ] {
            assert_eq!(parse_seconds(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn iso_rounds_down_to_the_whole_second() {
        assert_eq!(iso(0.999), "1970-01-01T00:00:00Z");
        assert_eq!(iso(-0.5), "1969-12-31T23:59:59Z");
    }

    #[test]
    fn iso_formats_any_float_without_overflow() {
        for seconds in [f64::MAX, f64::MIN, f64::NAN, f64::INFINITY] {
            assert!(iso(seconds).ends_with('Z'), "{seconds}");
        }
    }
}
