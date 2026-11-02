//! Time-range query parameters.

use iris_catalog::TimeRange;
use iris_core::time::WallUs;
use serde::Deserialize;

use crate::error::ApiError;

/// Longest range a single request may cover.
pub(crate) const MAX_RANGE_US: i64 = 7 * 86_400 * 1_000_000;
/// Default window when `from` is absent.
const DEFAULT_WINDOW_US: i64 = 3_600 * 1_000_000;

/// `from`/`to` as they arrive on the query string.
#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct TimeParams {
    pub(crate) from: Option<String>,
    pub(crate) to: Option<String>,
}

/// Resolves the parameters into a half-open range, applying defaults and bounds.
pub(crate) fn resolve_range(params: &TimeParams, now: WallUs) -> Result<TimeRange, ApiError> {
    let to = match &params.to {
        Some(raw) => {
            parse_time(raw).ok_or_else(|| ApiError::BadRequest(format!("bad 'to' {raw:?}")))?
        }
        None => now,
    };
    let from = match &params.from {
        Some(raw) => {
            parse_time(raw).ok_or_else(|| ApiError::BadRequest(format!("bad 'from' {raw:?}")))?
        }
        None => to.offset(-DEFAULT_WINDOW_US),
    };
    if to <= from {
        return Err(ApiError::BadRequest("'to' must be after 'from'".into()));
    }
    if to.micros_since(from) > MAX_RANGE_US {
        return Err(ApiError::BadRequest("range exceeds 7 days".into()));
    }
    Ok(TimeRange { from, to })
}

/// Parses either Unix microseconds or RFC 3339 (`2026-10-31T17:00:00Z`,
/// optional fraction, `Z` or a `±HH:MM` offset).
pub(crate) fn parse_time(raw: &str) -> Option<WallUs> {
    let raw = raw.trim();
    if let Ok(micros) = raw.parse::<i64>() {
        return Some(WallUs::new(micros));
    }
    parse_rfc3339(raw)
}

fn parse_rfc3339(raw: &str) -> Option<WallUs> {
    let bytes = raw.as_bytes();
    if bytes.len() < 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !matches!(bytes[10], b'T' | b't' | b' ')
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let year: i64 = raw[0..4].parse().ok()?;
    let month: u32 = raw[5..7].parse().ok()?;
    let day: u32 = raw[8..10].parse().ok()?;
    let hour: i64 = raw[11..13].parse().ok()?;
    let minute: i64 = raw[14..16].parse().ok()?;
    let second: i64 = raw[17..19].parse().ok()?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let mut pos = 19;
    let mut micros: i64 = 0;
    if bytes.get(pos) == Some(&b'.') {
        pos += 1;
        let start = pos;
        while pos < bytes.len() && bytes[pos].is_ascii_digit() {
            pos += 1;
        }
        let digits = &raw[start..pos];
        if digits.is_empty() {
            return None;
        }
        let scaled = format!("{digits:0<6}");
        micros = scaled[..6].parse().ok()?;
    }
    let offset_secs: i64 = match bytes.get(pos) {
        Some(b'Z' | b'z') if pos + 1 == bytes.len() => 0,
        Some(sign @ (b'+' | b'-')) if pos + 6 == bytes.len() && bytes[pos + 3] == b':' => {
            let oh: i64 = raw[pos + 1..pos + 3].parse().ok()?;
            let om: i64 = raw[pos + 4..pos + 6].parse().ok()?;
            let total = oh * 3600 + om * 60;
            if *sign == b'+' { total } else { -total }
        }
        _ => return None,
    };
    let days = days_from_civil(year, month, day);
    let secs = days * 86_400 + hour * 3600 + minute * 60 + second - offset_secs;
    Some(WallUs::new(secs * 1_000_000 + micros))
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
    fn applies_defaults_and_bounds() {
        let now = WallUs::new(1_793_466_000_000_000);
        let range = resolve_range(&TimeParams::default(), now).unwrap();
        assert_eq!(range.to, now);
        assert_eq!(range.from, now.offset(-DEFAULT_WINDOW_US));
        let too_long = TimeParams {
            from: Some("0".into()),
            to: Some(now.get().to_string()),
        };
        assert!(matches!(
            resolve_range(&too_long, now),
            Err(ApiError::BadRequest(_))
        ));
        let inverted = TimeParams {
            from: Some(now.get().to_string()),
            to: Some("5".into()),
        };
        assert!(matches!(
            resolve_range(&inverted, now),
            Err(ApiError::BadRequest(_))
        ));
    }
}
