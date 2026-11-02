//! The keyframe resource: the init segment plus one single-sample fragment
//! holding the sync sample at or before a time, for client-rendered
//! thumbnails and scrubbing without decoding a whole GOP.

use std::io::{self, SeekFrom};
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use iris_catalog::TimeRange;
use iris_core::time::WallUs;
use iris_mp4::fragment::{FragmentHeader, Sample, encode_fragment};
use iris_mp4::index::{FLAG_STARTS_WITH_SYNC, IndexRecord};
use iris_mp4::reader::parse_fragment;
use serde::Deserialize;
use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};

use crate::error::ApiError;
use crate::hls::load_fragments;
use crate::query::parse_time;
use crate::segments::stream_key;
use crate::state::ApiState;

/// How far back from `t` the sync sample may lie: longer than any fragment
/// the store writes, so a sync sample inside a recording is always found.
const LOOKBACK_US: i64 = 120 * 1_000_000;

/// `t` as it arrives on the query string.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct KeyframeParams {
    t: Option<String>,
}

/// `GET /api/streams/{camera}/{kind}/keyframe?t=`: a playable MP4 of one
/// frame, the sync sample at or before `t` (default: the newest).
pub(crate) async fn keyframe(
    State(state): State<Arc<ApiState>>,
    Path((camera, kind)): Path<(String, String)>,
    Query(params): Query<KeyframeParams>,
) -> Result<Response, ApiError> {
    let key = stream_key(&camera, &kind)?;
    let t = match &params.t {
        Some(raw) => {
            parse_time(raw).ok_or_else(|| ApiError::BadRequest(format!("bad 't' {raw:?}")))?
        }
        None => WallUs::now(),
    };
    let range = TimeRange {
        from: t.offset(-LOOKBACK_US),
        to: t.offset(1),
    };
    let segments = load_fragments(&state, &key, range).await?;
    let Some((path, init_len, record)) = segments
        .iter()
        .flat_map(|seg| {
            seg.records
                .iter()
                .map(move |r| (seg.row.path.clone(), seg.row.init_len, r))
        })
        .filter(|(_, _, r)| r.flags & FLAG_STARTS_WITH_SYNC != 0 && r.wall <= t)
        .max_by_key(|(_, _, r)| r.wall)
    else {
        return Err(ApiError::NotFound);
    };
    let record = *record;
    let body = build(&path, init_len, &record).await?;
    Ok((
        [
            (header::CONTENT_TYPE, "video/mp4".to_owned()),
            (header::CACHE_CONTROL, "private, max-age=3600".to_owned()),
            (
                "x-iris-wall-us".parse().expect("static header name"),
                record.wall.get().to_string(),
            ),
        ],
        body,
    )
        .into_response())
}

/// Reads the init section and the fragment, and re-encodes the fragment's
/// first sample alone.
async fn build(
    path: &std::path::Path,
    init_len: u32,
    record: &IndexRecord,
) -> Result<Bytes, ApiError> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut init = vec![0u8; init_len as usize];
    file.read_exact(&mut init).await?;
    file.seek(SeekFrom::Start(record.offset)).await?;
    let mut fragment = vec![0u8; record.len as usize];
    file.read_exact(&mut fragment).await?;
    let info = parse_fragment(&fragment)
        .ok_or_else(|| ApiError::Io(io::Error::other("unparseable fragment")))?;
    let (Some(&size), Some(&duration)) = (info.sample_sizes.first(), info.sample_durations.first())
    else {
        return Err(ApiError::Io(io::Error::other("fragment without samples")));
    };
    let start = usize::try_from(info.payload_offset).map_err(|_| ApiError::NotFound)?;
    let end = start
        .checked_add(size as usize)
        .filter(|&end| end <= fragment.len())
        .ok_or_else(|| ApiError::Io(io::Error::other("sample past the fragment")))?;
    let sample = Sample {
        data: Bytes::copy_from_slice(&fragment[start..end]),
        duration,
        is_sync: true,
    };
    let header = FragmentHeader {
        sequence: 1,
        decode_time: 0,
        wall: record.wall,
    };
    let encoded = encode_fragment(&header, std::slice::from_ref(&sample))
        .map_err(|e| ApiError::Io(io::Error::other(e.to_string())))?;
    let mut out = init;
    out.extend_from_slice(&encoded.prefix);
    out.extend_from_slice(&sample.data);
    out.extend_from_slice(&encoded.trailer);
    Ok(Bytes::from(out))
}
