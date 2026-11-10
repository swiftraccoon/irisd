//! Allocation-free parsing of request timestamps, shared with the fuzz target.

use iris_core::time::{self, WallUs};

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
        || !(1..=time::days_in_month(year, month)).contains(&day)
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
    let days = time::days_from_civil(year, month, day);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Seeded instants from 1970 through 2104, to the microsecond: the
    /// parser reads back what `WallUs` formats, with `Z` and behind an offset.
    #[test]
    fn parse_time_reads_back_every_generated_instant_wall_us_formats() {
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        for _ in 0..20_000 {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            let micros = i64::try_from(
                state.wrapping_mul(0x2545_f491_4f6c_dd1d) % (135 * 365 * 86_400 * 1_000_000),
            )
            .unwrap();
            let wall = WallUs::new(micros);
            let text = wall.to_string();
            assert_eq!(parse_time(&text), Some(wall), "{text}");
            let ahead = format!("{}+05:30", text.trim_end_matches('Z'));
            assert_eq!(
                parse_time(&ahead),
                Some(wall.offset(-19_800_000_000)),
                "{ahead}"
            );
        }
    }

    #[test]
    fn parses_micros_and_rfc3339() {
        assert_eq!(
            parse_time("1793466000000000"),
            Some(WallUs::new(1_793_466_000_000_000))
        );
        let t = parse_time("2026-10-31T17:00:00Z").unwrap();
        assert_eq!(t.to_string(), "2026-10-31T17:00:00.000000Z");
        let t = parse_time("2026-10-31T13:00:00.25-04:00").unwrap();
        assert_eq!(t.to_string(), "2026-10-31T17:00:00.250000Z");
        assert_eq!(parse_time("1970-01-01T00:00:00Z"), Some(WallUs::new(0)));
        assert!(parse_time("2026-13-01T00:00:00Z").is_none());
        assert!(parse_time("yesterday").is_none());
    }

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
            // A zone must end the text and keep its colon.
            "2026-01-01T00:00:00Zx",
            "2026-01-01T00:00:00+01:00x",
            "2026-01-01T00:00:00+01-00",
            "2026-01-01T00:00:00+0100",
            "2026-01-01T00:00:00",
            // The shortest valid text is twenty bytes.
            "2026-01-01T00:00:0Z",
        ] {
            assert!(parse_time(raw).is_none(), "accepted {raw:?}");
        }
        // One separator wrong at a time.
        for position in [4, 7, 10, 13, 16] {
            let mut raw = b"2026-01-01T00:00:00Z".to_vec();
            raw[position] = b'x';
            let raw = String::from_utf8(raw).unwrap();
            assert!(parse_time(&raw).is_none(), "accepted {raw:?}");
        }
        // Digits past the sixth of a fraction are read and dropped.
        assert_eq!(
            parse_time("1970-01-01T00:00:00.1234567Z"),
            Some(WallUs::new(123_456))
        );
        assert_eq!(
            parse_time("1970-01-01T00:00:00.000009Z"),
            Some(WallUs::new(9))
        );
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
