//! The `Range` request header of a raw recording request, parsed without
//! allocation. Included by the `api_range` fuzz target as well as the crate.

/// What a `Range` header asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RangeRequest {
    /// No `Range` header, or one with a unit other than `bytes`, which an
    /// origin server ignores (RFC 9110 §14.2): the whole extent is served.
    Whole,
    /// One satisfiable byte range, clamped to the extent: `(first, last)`.
    Bytes(u64, u64),
}

/// Why a `Range` header cannot be satisfied: more than one range, a malformed
/// specification, a first byte at or past the extent, or an empty extent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Unsatisfiable;

/// Parses the `Range` header value `raw` against a resource of `extent` bytes.
pub(crate) fn parse_range(raw: Option<&str>, extent: u64) -> Result<RangeRequest, Unsatisfiable> {
    let Some(raw) = raw else {
        return Ok(RangeRequest::Whole);
    };
    let Some(range) = raw.strip_prefix("bytes=") else {
        return Ok(RangeRequest::Whole);
    };
    let range = range.trim();
    if range.contains(',') {
        return Err(Unsatisfiable);
    }
    let (start, end) = range.split_once('-').ok_or(Unsatisfiable)?;
    if extent == 0 {
        return Err(Unsatisfiable);
    }
    if start.is_empty() {
        let suffix: u64 = end.parse().map_err(|_| Unsatisfiable)?;
        if suffix == 0 {
            return Err(Unsatisfiable);
        }
        let start = extent.saturating_sub(suffix);
        return Ok(RangeRequest::Bytes(start, extent - 1));
    }
    let start: u64 = start.parse().map_err(|_| Unsatisfiable)?;
    if start >= extent {
        return Err(Unsatisfiable);
    }
    let end = if end.is_empty() {
        extent - 1
    } else {
        let end: u64 = end.parse().map_err(|_| Unsatisfiable)?;
        if end < start {
            return Err(Unsatisfiable);
        }
        end.min(extent - 1)
    };
    Ok(RangeRequest::Bytes(start, end))
}

/// The number of bytes a `Range` header asks for before the extent is known:
/// `None` when it is absent, malformed, names several ranges, or is open-ended.
pub(crate) fn requested_len(raw: Option<&str>) -> Option<u64> {
    let range = raw?.strip_prefix("bytes=")?.trim();
    if range.contains(',') {
        return None;
    }
    let (start, end) = range.split_once('-')?;
    if start.is_empty() {
        return end.parse::<u64>().ok();
    }
    let start: u64 = start.parse().ok()?;
    let end: u64 = end.parse().ok()?;
    end.checked_sub(start)?.checked_add(1)
}
