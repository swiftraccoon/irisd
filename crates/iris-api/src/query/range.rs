//! `from`/`to` query parameters: parsing, defaults, and the range bound.

use iris_catalog::TimeRange;
use iris_core::time::WallUs;
use serde::Deserialize;

use crate::error::ApiError;
use crate::query::parse_time;

/// Longest range a single request may cover.
pub(crate) const MAX_RANGE_US: i64 = 7 * 86_400 * 1_000_000;
/// Default window when `from` is absent.
pub(super) const DEFAULT_WINDOW_US: i64 = 3_600 * 1_000_000;

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
