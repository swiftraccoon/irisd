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
use serde::Deserialize;
use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};

use crate::error::ApiError;
use crate::hls::{
    MAX_FRAGMENT_BYTES, MAX_INIT_BYTES, ValidatedMedia, load_fragments, validate_fragment_record,
};
use crate::media_admission;
use crate::query::parse_time;
use crate::segments::stream_key;
use crate::state::{ApiState, MediaClass, admitted_media_bytes};

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
/// Returns HTTP 400 if the lookback or exclusive range end cannot be represented.
pub(crate) async fn keyframe(
    State(state): State<Arc<ApiState>>,
    Path((camera, kind)): Path<(String, String)>,
    Query(params): Query<KeyframeParams>,
) -> Result<Response, ApiError> {
    let permit = state.stats.admit_media(MediaClass::Bounded)?;
    let key = stream_key(&camera, &kind)?;
    let implicit_newest = params.t.is_none();
    let t = match &params.t {
        Some(raw) => {
            parse_time(raw).ok_or_else(|| ApiError::BadRequest(format!("bad 't' {raw:?}")))?
        }
        None => WallUs::now(),
    };
    let (Some(from), Some(to)) = (t.get().checked_sub(LOOKBACK_US), t.get().checked_add(1)) else {
        return Err(ApiError::BadRequest(
            "'t' is outside the supported keyframe lookup range".into(),
        ));
    };
    let range = TimeRange {
        from: WallUs::new(from),
        to: WallUs::new(to),
    };
    let segments = load_fragments(&state, &key, range, &permit).await?;
    let Some((media, init_len, record)) = segments
        .iter()
        .flat_map(|seg| {
            seg.records
                .iter()
                .map(move |r| (&seg.media, seg.row.init_len, r))
        })
        .filter(|(_, _, r)| {
            r.flags & FLAG_STARTS_WITH_SYNC != 0 && r.wall >= range.from && r.wall <= t
        })
        .max_by_key(|(_, _, r)| r.wall)
    else {
        return Err(ApiError::NotFound);
    };
    let media = media.clone();
    let record = *record;
    let body = media_admission::spawn(
        &permit,
        async move { build(&media, init_len, &record).await },
    )
    .await
    .map_err(io::Error::other)??;
    let body = admitted_media_bytes(body, permit);
    let cache_control = if implicit_newest {
        "private, no-store"
    } else {
        "private, max-age=3600"
    };
    Ok((
        [
            (header::CONTENT_TYPE, "video/mp4".to_owned()),
            (header::CACHE_CONTROL, cache_control.to_owned()),
            (
                "x-iris-wall-us".parse().expect("static header name"),
                record.wall.get().to_string(),
            ),
        ],
        body,
    )
        .into_response())
}

/// Reads the init section and the fragment from the validated recording,
/// reopened beneath its root, and re-encodes the fragment's first sample alone.
async fn build(
    media: &ValidatedMedia,
    init_len: u32,
    record: &IndexRecord,
) -> Result<Bytes, ApiError> {
    if u64::from(init_len) > MAX_INIT_BYTES || u64::from(record.len) > MAX_FRAGMENT_BYTES {
        return Err(ApiError::Unavailable(
            "keyframe source exceeds the serving budget".into(),
        ));
    }
    let mut file = media.open().await?;
    let mut init = Vec::new();
    init.try_reserve_exact(init_len as usize).map_err(|error| {
        ApiError::Unavailable(format!("cannot reserve keyframe initialization: {error}"))
    })?;
    init.resize(init_len as usize, 0);
    file.read_exact(&mut init).await?;
    file.seek(SeekFrom::Start(record.offset)).await?;
    let mut fragment = Vec::new();
    fragment
        .try_reserve_exact(record.len as usize)
        .map_err(|error| {
            ApiError::Unavailable(format!("cannot reserve keyframe fragment: {error}"))
        })?;
    fragment.resize(record.len as usize, 0);
    file.read_exact(&mut fragment).await?;
    let info = validate_fragment_record(&fragment, record)?;
    if !info.starts_with_sync {
        return Err(ApiError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "keyframe source is not a sync fragment",
        )));
    }
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
    init.try_reserve(
        encoded
            .prefix
            .len()
            .saturating_add(sample.data.len())
            .saturating_add(encoded.trailer.len()),
    )
    .map_err(|error| ApiError::Unavailable(format!("cannot reserve keyframe response: {error}")))?;
    let mut out = init;
    out.extend_from_slice(&encoded.prefix);
    out.extend_from_slice(&sample.data);
    out.extend_from_slice(&encoded.trailer);
    Ok(Bytes::from(out))
}
