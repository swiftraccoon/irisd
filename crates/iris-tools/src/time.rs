//! Timestamps as the daemon logs them: RFC 3339 in UTC with a `Z`.

/// Parses `YYYY-MM-DDTHH:MM:SS[.fraction]Z` (or `T` replaced by a space,
/// or a `+00:00` offset) into seconds since the Unix epoch.
pub fn parse_seconds(text: &str) -> Option<f64> {
    let t = text.trim();
    let b = t.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |s: &str| s.parse::<i64>().ok();
    let year = num(t.get(0..4)?)?;
    let month = num(t.get(5..7)?)?;
    let day = num(t.get(8..10)?)?;
    let hour = num(t.get(11..13)?)?;
    let minute = num(t.get(14..16)?)?;
    let second = num(t.get(17..19)?)?;
    let mut rest = &t[19..];
    let mut fraction = 0.0;
    if let Some(after) = rest.strip_prefix('.') {
        let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
        if !digits.is_empty() {
            fraction = format!("0.{digits}").parse::<f64>().ok()?;
        }
        rest = &after[digits.len()..];
    }
    let offset = match rest {
        "" | "Z" | "z" | "+00:00" | "-00:00" => 0,
        other => {
            let sign = match other.as_bytes().first()? {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let oh = num(other.get(1..3)?)?;
            let om = num(other.get(4..6)?)?;
            sign * (oh * 3600 + om * 60)
        }
    };
    let days = days_from_civil(year, month, day)?;
    Some(((days * 86_400 + hour * 3600 + minute * 60 + second - offset) as f64) + fraction)
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
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
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
}
