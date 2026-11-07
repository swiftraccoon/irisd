//! Time-range query parameters.

use iris_catalog::TimeRange;
use iris_core::time::WallUs;
use serde::Deserialize;

use crate::error::ApiError;

mod time;

pub(crate) use time::parse_time;

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
        None => WallUs::new(to.get().checked_sub(DEFAULT_WINDOW_US).ok_or_else(|| {
            ApiError::BadRequest("'to' is too early for the default window".into())
        })?),
    };
    if to <= from {
        return Err(ApiError::BadRequest("'to' must be after 'from'".into()));
    }
    if to
        .get()
        .checked_sub(from.get())
        .is_none_or(|span| span > MAX_RANGE_US)
    {
        return Err(ApiError::BadRequest("range exceeds 7 days".into()));
    }
    Ok(TimeRange { from, to })
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

    #[test]
    fn rejects_ranges_whose_span_or_default_start_overflows() {
        for params in [
            TimeParams {
                from: Some(i64::MIN.to_string()),
                to: Some(i64::MAX.to_string()),
            },
            TimeParams {
                from: None,
                to: Some(i64::MIN.to_string()),
            },
        ] {
            assert!(matches!(
                resolve_range(&params, WallUs::new(0)),
                Err(ApiError::BadRequest(_))
            ));
        }
        let params = TimeParams {
            from: Some((i64::MIN + 1).to_string()),
            to: Some((i64::MIN + 1 + MAX_RANGE_US).to_string()),
        };
        assert!(resolve_range(&params, WallUs::new(0)).is_ok());
    }
}
