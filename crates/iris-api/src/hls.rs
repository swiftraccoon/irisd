//! Byte-range VOD playlists spanning several recording files.

use std::fmt::Write as _;
use std::io;
use std::sync::Arc;

use axum::Extension;
use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use iris_catalog::{SegmentRow, TimeRange};
use iris_core::id::StreamKey;
use iris_core::time::{MediaTicks, TIMESCALE, WallUs};
use iris_mp4::index::{IndexRecord, read_index};

use crate::auth::AuthToken;
use crate::error::ApiError;
use crate::query::{TimeParams, resolve_range};
use crate::segments::stream_key;
use crate::state::ApiState;

/// Fragments per HLS media segment (about six seconds at a two-second GOP).
const FRAGMENTS_PER_MEDIA_SEGMENT: usize = 3;

/// A catalog row plus the fragments of its sidecar that fall in the request range.
#[derive(Debug)]
pub(crate) struct SegmentFragments {
    pub(crate) row: SegmentRow,
    pub(crate) records: Vec<IndexRecord>,
}

/// Loads the segments overlapping `range` and their in-range fragments.
pub(crate) async fn load_fragments(
    state: &ApiState,
    key: &StreamKey,
    range: TimeRange,
) -> Result<Vec<SegmentFragments>, ApiError> {
    let rows = state.catalog.segments(key, range).await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let idx = iris_store::layout::index_path(&row.path);
        let read = tokio::task::spawn_blocking(move || read_index(&idx))
            .await
            .map_err(|e| ApiError::Io(io::Error::other(e)))?;
        let records = match read {
            Ok((_, records)) => records,
            Err(e) => {
                tracing::warn!(path = %row.path.display(), error = %e, "unreadable sidecar; skipping");
                continue;
            }
        };
        let in_range: Vec<IndexRecord> = records
            .into_iter()
            .filter(|r| overlaps(r, &range))
            .collect();
        if !in_range.is_empty() {
            out.push(SegmentFragments {
                row,
                records: in_range,
            });
        }
    }
    Ok(out)
}

fn overlaps(record: &IndexRecord, range: &TimeRange) -> bool {
    let start = record.wall;
    let end = start.offset(MediaTicks::new(i64::from(record.duration)).as_micros());
    end > range.from && start < range.to
}

/// One media segment: a byte range covering whole fragments.
#[derive(Debug)]
struct MediaSegment {
    offset: u64,
    len: u64,
    duration_ticks: u64,
}

/// Groups fragments into media segments that start at sync fragments.
fn group(records: &[IndexRecord]) -> Vec<MediaSegment> {
    let mut groups: Vec<(MediaSegment, usize)> = Vec::new();
    for r in records {
        match groups.last_mut() {
            Some((seg, n)) if !(r.starts_with_sync() && *n >= FRAGMENTS_PER_MEDIA_SEGMENT) => {
                seg.len = r.end() - seg.offset;
                seg.duration_ticks += u64::from(r.duration);
                *n += 1;
            }
            _ => groups.push((
                MediaSegment {
                    offset: r.offset,
                    len: u64::from(r.len),
                    duration_ticks: u64::from(r.duration),
                },
                1,
            )),
        }
    }
    groups.into_iter().map(|(seg, _)| seg).collect()
}

/// Percent-encodes everything outside the unreserved set.
pub(crate) fn percent_encode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for b in raw.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// The file URI for a segment, carrying the caller's token when auth is on.
pub(crate) fn file_uri(segment_id: i64, token: Option<&str>) -> String {
    match token {
        Some(t) => format!(
            "/api/segments/{segment_id}/file.mp4?token={}",
            percent_encode(t)
        ),
        None => format!("/api/segments/{segment_id}/file.mp4"),
    }
}

/// Renders the VOD playlist.
pub(crate) fn vod_playlist(segments: &[SegmentFragments], token: Option<&str>) -> String {
    let mut body = String::new();
    let mut target_ticks = 0u64;
    let mut last_hash: Option<u32> = None;
    for seg in segments {
        let uri = file_uri(seg.row.id.0, token);
        let discontinuity = last_hash.is_some()
            && (seg.row.discontinuity || last_hash != Some(seg.row.sample_entry_hash));
        if discontinuity {
            body.push_str("#EXT-X-DISCONTINUITY\n");
        }
        let _ = writeln!(
            body,
            "#EXT-X-MAP:URI=\"{uri}\",BYTERANGE=\"{}@0\"",
            seg.row.init_len
        );
        for media in group(&seg.records) {
            target_ticks = target_ticks.max(media.duration_ticks);
            let _ = writeln!(
                body,
                "#EXTINF:{:.5},",
                media.duration_ticks as f64 / f64::from(TIMESCALE)
            );
            let _ = writeln!(body, "#EXT-X-BYTERANGE:{}@{}", media.len, media.offset);
            body.push_str(&uri);
            body.push('\n');
        }
        last_hash = Some(seg.row.sample_entry_hash);
    }
    let target = target_ticks.div_ceil(u64::from(TIMESCALE)).max(1);
    format!(
        "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:{target}\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-INDEPENDENT-SEGMENTS\n{body}#EXT-X-ENDLIST\n"
    )
}

pub(crate) async fn vod(
    State(state): State<Arc<ApiState>>,
    Path((camera, kind)): Path<(String, String)>,
    Query(params): Query<TimeParams>,
    Extension(token): Extension<AuthToken>,
) -> Result<Response, ApiError> {
    let key = stream_key(&camera, &kind)?;
    let range = resolve_range(&params, WallUs::now())?;
    let segments = load_fragments(&state, &key, range).await?;
    if segments.is_empty() {
        return Err(ApiError::NotFound);
    }
    let text = vod_playlist(&segments, token.0.as_deref());
    Ok((
        [
            (header::CONTENT_TYPE, "application/vnd.apple.mpegurl"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        text,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use iris_mp4::index::FLAG_STARTS_WITH_SYNC;

    fn record(offset: u64, len: u32, sync: bool) -> IndexRecord {
        IndexRecord {
            offset,
            len,
            duration: 180_000,
            sample_count: 60,
            flags: if sync { FLAG_STARTS_WITH_SYNC } else { 0 },
            ..IndexRecord::default()
        }
    }

    #[test]
    fn groups_three_fragments_per_media_segment_at_sync_points() {
        let records: Vec<IndexRecord> =
            (0..7).map(|i| record(700 + i * 1000, 1000, true)).collect();
        let groups = group(&records);
        assert_eq!(groups.len(), 3);
        assert_eq!((groups[0].offset, groups[0].len), (700, 3000));
        assert_eq!((groups[2].offset, groups[2].len), (6700, 1000));
        assert_eq!(groups[0].duration_ticks, 540_000);
    }

    #[test]
    fn encodes_tokens_in_uris() {
        assert_eq!(
            file_uri(5, Some("a b/c")),
            "/api/segments/5/file.mp4?token=a%20b%2Fc"
        );
        assert_eq!(file_uri(5, None), "/api/segments/5/file.mp4");
    }
}
