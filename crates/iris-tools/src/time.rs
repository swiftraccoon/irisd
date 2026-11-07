//! Timestamps as the daemon logs them: RFC 3339 in UTC with a `Z`.

/// Strictly parses RFC 3339 `YYYY-MM-DDTHH:MM:SS[.fraction](Z|+HH:MM)` into
/// seconds since the Unix epoch.
pub fn parse_seconds(text: &str) -> Option<f64> {
    let t = text;
    let b = t.as_bytes();
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
    let year = num(t.get(0..4)?)?;
    let month = num(t.get(5..7)?)?;
    let day = num(t.get(8..10)?)?;
    let hour = num(t.get(11..13)?)?;
    let minute = num(t.get(14..16)?)?;
    let second = num(t.get(17..19)?)?;
    if !(1..=9999).contains(&year) || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let mut rest = t.get(19..)?;
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
            let oh = num(other.get(1..3)?)?;
            let om = num(other.get(4..6)?)?;
            if oh > 23 || om > 59 {
                return None;
            }
            sign * (oh * 3600 + om * 60)
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

/// Formats seconds since the epoch as `YYYY-MM-DDTHH:MM:SSZ`.
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

/// Days since 1970-01-01 for a proleptic Gregorian date.
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

/// The civil date of a day count since 1970-01-01.
pub fn civil_from_days(days: i64) -> (i64, i64, i64) {
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

/// The current time as seconds since the epoch.
pub fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_formats_round_trip() {
        let s = parse_seconds("2026-11-02T01:44:23Z").unwrap();
        assert_eq!(iso(s), "2026-11-02T01:44:23Z");
        assert_eq!(parse_seconds("2026-11-02T01:44:23.5Z"), Some(s + 0.5));
        assert_eq!(parse_seconds("2026-11-02T03:44:23+02:00"), Some(s));
        assert_eq!(parse_seconds("1970-01-01T00:00:00Z"), Some(0.0));
        assert_eq!(parse_seconds("nope"), None);
    }

    #[test]
    fn rejects_non_rfc3339_and_invalid_calendar_values() {
        for invalid in [
            "2026-11-02 01:44:23Z",
            "2026/11/02T01:44:23Z",
            "2026-02-29T01:44:23Z",
            "2024-02-30T01:44:23Z",
            "2026-11-02T24:00:00Z",
            "2026-11-02T00:60:00Z",
            "2026-11-02T00:00:60Z",
            "2026-11-02T00:00:00",
            "2026-11-02T00:00:00z",
            "2026-11-02T00:00:00+24:00",
            "2026-11-02T00:00:00Zjunk",
            "2026-11-02T00:00:00.Z",
            " 2026-11-02T00:00:00Z",
            "2026-11-02T00:00:00Z ",
        ] {
            assert_eq!(parse_seconds(invalid), None, "{invalid}");
        }
        assert!(parse_seconds("2024-02-29T23:59:59.123-05:30").is_some());
    }
}
