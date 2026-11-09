//! Fuzzes the timestamp parser used by API query parameters and event retention.
#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../../crates/iris-api/src/query/time.rs"]
mod time;

fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data) {
        let parsed = time::parse_time(text).map(iris_core::time::WallUs::get);
        let trimmed = text.trim();
        let unix_micros = trimmed.parse::<i64>().ok();
        let semantic_timestamp = semantic_rfc3339(trimmed);
        let expected = if text.is_ascii() {
            unix_micros.or(semantic_timestamp)
        } else {
            None
        };
        assert_eq!(parsed, expected);

        if unix_micros.is_none()
            && let Some(value) = time::parse_time(text)
        {
            assert_eq!(time::parse_time(&value.to_string()), Some(value));
        }
    }
});

fn semantic_rfc3339(raw: &str) -> Option<i64> {
    let bytes = raw.as_bytes();
    if !raw.is_ascii()
        || bytes.len() < 20
        || bytes.get(4) != Some(&b'-')
        || bytes.get(7) != Some(&b'-')
        || !matches!(bytes.get(10), Some(b'T' | b't'))
        || bytes.get(13) != Some(&b':')
        || bytes.get(16) != Some(&b':')
    {
        return None;
    }

    let year = digits(bytes, 0, 4)?;
    let month = u32::try_from(digits(bytes, 5, 2)?).ok()?;
    let day = u32::try_from(digits(bytes, 8, 2)?).ok()?;
    let hour = digits(bytes, 11, 2)?;
    let minute = digits(bytes, 14, 2)?;
    let second = digits(bytes, 17, 2)?;
    if !(1..=12).contains(&month)
        || !(1..=month_days(year, month)).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }

    let mut pos = 19;
    let mut micros = 0;
    if bytes.get(pos) == Some(&b'.') {
        pos += 1;
        let start = pos;
        while let Some(&digit) = bytes.get(pos).filter(|byte| byte.is_ascii_digit()) {
            if pos - start < 6 {
                micros = micros * 10 + i64::from(digit - b'0');
            }
            pos += 1;
        }
        if pos == start {
            return None;
        }
        for _ in (pos - start)..6 {
            micros *= 10;
        }
    }

    let offset = match bytes.get(pos) {
        Some(b'Z' | b'z') if pos + 1 == bytes.len() => 0,
        Some(sign @ (b'+' | b'-'))
            if pos + 6 == bytes.len() && bytes.get(pos + 3) == Some(&b':') =>
        {
            let hours = digits(bytes, pos + 1, 2)?;
            let minutes = digits(bytes, pos + 4, 2)?;
            if hours > 23 || minutes > 59 {
                return None;
            }
            let seconds = hours * 3600 + minutes * 60;
            if *sign == b'+' { seconds } else { -seconds }
        }
        _ => return None,
    };

    let days = civil_days(year, month, day);
    Some((days * 86_400 + hour * 3600 + minute * 60 + second - offset) * 1_000_000 + micros)
}

fn digits(bytes: &[u8], start: usize, len: usize) -> Option<i64> {
    let mut value = 0;
    for index in start..start.checked_add(len)? {
        let digit = *bytes.get(index)?;
        if !digit.is_ascii_digit() {
            return None;
        }
        value = value * 10 + i64::from(digit - b'0');
    }
    Some(value)
}

fn month_days(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => 0,
    }
}

fn civil_days(year: i64, month: u32, day: u32) -> i64 {
    let adjusted_year = if month <= 2 { year - 1 } else { year };
    let era = adjusted_year.div_euclid(400);
    let year_of_era = adjusted_year.rem_euclid(400);
    let month = i64::from(month);
    let day_of_year =
        (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}
