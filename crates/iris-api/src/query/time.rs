//! Allocation-free parsing of request timestamps, shared with the fuzz target.

use iris_core::time::WallUs;

/// Parses Unix microseconds or a Gregorian ASCII RFC 3339 timestamp with an optional fraction.
/// Leap seconds are rejected because the recorder has no leap-second table.
pub(crate) fn parse_time(raw: &str) -> Option<WallUs> {
    if !raw.is_ascii() {
        return None;
    }
    let raw = raw.trim();
    if let Ok(micros) = raw.parse::<i64>() {
        return Some(WallUs::new(micros));
    }
    parse_rfc3339(raw)
}

fn parse_rfc3339(raw: &str) -> Option<WallUs> {
    let bytes = raw.as_bytes();
    if !raw.is_ascii()
        || bytes.len() < 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !matches!(bytes[10], b'T' | b't')
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let year = i64::from(four_digits(bytes, 0)?);
    let month = two_digits(bytes, 5)?;
    let day = two_digits(bytes, 8)?;
    let hour = two_digits(bytes, 11)?;
    let minute = two_digits(bytes, 14)?;
    let second = two_digits(bytes, 17)?;
    if !(1..=12).contains(&month)
        || !(1..=days_in_month(year, month)).contains(&day)
        || hour > 23
        || minute > 59
        // Leap seconds are rejected rather than normalized because this API
        // has no leap-second table with which to identify their valid dates.
        || second > 59
    {
        return None;
    }
    let mut pos = 19;
    let mut micros: i64 = 0;
    if bytes.get(pos) == Some(&b'.') {
        pos += 1;
        let start = pos;
        while pos < bytes.len() && bytes[pos].is_ascii_digit() {
            if pos - start < 6 {
                micros = micros * 10 + i64::from(bytes[pos] - b'0');
            }
            pos += 1;
        }
        if start == pos {
            return None;
        }
        for _ in (pos - start)..6 {
            micros *= 10;
        }
    }
    let offset_secs: i64 = match bytes.get(pos) {
        Some(b'Z' | b'z') if pos + 1 == bytes.len() => 0,
        Some(sign @ (b'+' | b'-')) if pos + 6 == bytes.len() && bytes[pos + 3] == b':' => {
            let oh = i64::from(two_digits(bytes, pos + 1)?);
            let om = i64::from(two_digits(bytes, pos + 4)?);
            if oh > 23 || om > 59 {
                return None;
            }
            let total = oh * 3600 + om * 60;
            if *sign == b'+' { total } else { -total }
        }
        _ => return None,
    };
    let days = days_from_civil(year, month, day);
    let secs = days * 86_400 + i64::from(hour) * 3600 + i64::from(minute) * 60 + i64::from(second)
        - offset_secs;
    Some(WallUs::new(secs * 1_000_000 + micros))
}

fn two_digits(bytes: &[u8], start: usize) -> Option<u32> {
    let tens = *bytes.get(start)?;
    let ones = *bytes.get(start + 1)?;
    if !tens.is_ascii_digit() || !ones.is_ascii_digit() {
        return None;
    }
    Some(u32::from(tens - b'0') * 10 + u32::from(ones - b'0'))
}

fn four_digits(bytes: &[u8], start: usize) -> Option<u32> {
    let high = two_digits(bytes, start)?;
    let low = two_digits(bytes, start + 2)?;
    Some(high * 100 + low)
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

fn is_leap_year(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = i64::from(month);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_multibyte_characters_at_numeric_boundaries() {
        for raw in [
            "2026-01-01T00:00:0éZ",
            "2026-0é-01T00:00:00Z",
            "2026-01-01T00:00:00+0é:00",
            "2026-01-01T00:00:00.éZ",
            "2026-01-01T00:00:0💥Z",
            "\u{2003}2026-01-01T00:00:00Z",
            "123\u{a0}",
        ] {
            assert!(parse_time(raw).is_none(), "accepted {raw:?}");
        }
    }

    #[test]
    fn fractions_are_padded_and_truncated_without_allocation() {
        for (raw, expected) in [
            ("1970-01-01T00:00:00.1Z", 100_000),
            ("1970-01-01T00:00:00.123456789Z", 123_456),
        ] {
            assert_eq!(parse_time(raw), Some(WallUs::new(expected)));
        }
    }

    #[test]
    fn enforces_rfc3339_calendar_and_clock_boundaries() {
        for raw in [
            "0000-01-01T00:00:00Z",
            "2000-02-29T23:59:59Z",
            "2004-02-29t00:00:00z",
            "2026-04-30T12:34:56.000001Z",
            "1970-01-01T23:59:59+23:59",
            "1970-01-01T00:00:00-23:59",
        ] {
            assert!(parse_time(raw).is_some(), "rejected {raw:?}");
        }

        for raw in [
            "2026-01-01 00:00:00Z",
            "2026-01-01T-1:00:00Z",
            "2026-01-01T+1:00:00Z",
            "2026-02-29T00:00:00Z",
            "1900-02-29T00:00:00Z",
            "2026-02-31T00:00:00Z",
            "2026-04-31T00:00:00Z",
            "2026-01-00T00:00:00Z",
            "2026-01-01T24:00:00Z",
            "2026-01-01T00:60:00Z",
            "2026-01-01T00:00:60Z",
            "2026-01-01T00:00:00.Z",
            "2026-01-01T00:00:00+24:00",
            "2026-01-01T00:00:00+23:60",
            "2026-01-01T00:00:00+99:99",
        ] {
            assert!(parse_time(raw).is_none(), "accepted {raw:?}");
        }
    }

    #[test]
    fn offset_boundaries_map_to_the_expected_utc_instant() {
        assert_eq!(
            parse_time("1970-01-01T23:59:59+23:59"),
            Some(WallUs::new(59_000_000))
        );
        assert_eq!(
            parse_time("1970-01-01T00:00:00-23:59"),
            Some(WallUs::new(86_340_000_000))
        );
    }
}
