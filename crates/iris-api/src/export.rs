//! Streamed fMP4 export of a time range.

use std::io::{self, SeekFrom};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use iris_catalog::TimeRange;
use iris_core::time::{MediaTicks, TIMESCALE, WallUs};
use iris_mp4::export::{MOVIE_TIMESCALE, rebase_fragment, with_edit_list};
use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};
use tokio::sync::mpsc;

use crate::error::ApiError;
use crate::hls::{SegmentFragments, load_fragments};
use crate::query::{TimeParams, resolve_range};
use crate::segments::stream_key;
use crate::state::ApiState;

pub(crate) async fn export(
    State(state): State<Arc<ApiState>>,
    Path((camera, kind)): Path<(String, String)>,
    Query(params): Query<TimeParams>,
) -> Result<Response, ApiError> {
    let key = stream_key(&camera, &kind)?;
    let range = resolve_range(&params, WallUs::now())?;
    let segments = load_fragments(&state, &key, range).await?;
    if segments.is_empty() {
        return Err(ApiError::NotFound);
    }
    let c = range.from.to_civil();
    let filename = format!(
        "{}-{}-{:04}{:02}{:02}T{:02}{:02}{:02}Z.mp4",
        key.camera, key.kind, c.year, c.month, c.day, c.hour, c.minute, c.second
    );
    let (tx, mut rx) = mpsc::channel::<Result<Bytes, io::Error>>(4);
    tokio::spawn(async move {
        if let Err(e) = stream_segments(segments, range, &tx).await {
            let _ = tx.send(Err(e)).await;
        }
    });
    let stream = futures_util::stream::poll_fn(move |cx| rx.poll_recv(cx));
    Ok((
        [
            (header::CONTENT_TYPE, "video/mp4".to_owned()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            ),
            (header::CACHE_CONTROL, "no-store".to_owned()),
        ],
        Body::from_stream(stream),
    )
        .into_response())
}

/// Writes the first segment's init section with an edit list trimming the
/// export to `range`, then every in-range fragment rebased onto a continuous
/// timeline from zero with fresh sequence numbers and trailers.
async fn stream_segments(
    segments: Vec<SegmentFragments>,
    range: TimeRange,
    tx: &mpsc::Sender<Result<Bytes, io::Error>>,
) -> io::Result<()> {
    let Some(edit) = edit_list(&segments, range) else {
        return Ok(());
    };
    let mut first = true;
    let mut sequence = 1u32;
    let mut decode_time = 0u64;
    for seg in segments {
        let mut file = tokio::fs::File::open(&seg.row.path).await?;
        if first {
            let mut init = vec![0u8; seg.row.init_len as usize];
            file.read_exact(&mut init).await?;
            let init = with_edit_list(&init, edit.media_time, edit.duration_ms)
                .map_err(io::Error::other)?;
            if tx.send(Ok(Bytes::from(init))).await.is_err() {
                return Ok(());
            }
            first = false;
        }
        for record in &seg.records {
            file.seek(SeekFrom::Start(record.offset)).await?;
            let mut fragment = vec![0u8; record.len as usize];
            file.read_exact(&mut fragment).await?;
            rebase_fragment(&mut fragment, sequence, decode_time).map_err(io::Error::other)?;
            sequence = sequence.wrapping_add(1);
            decode_time += u64::from(record.duration);
            if tx.send(Ok(Bytes::from(fragment))).await.is_err() {
                return Ok(());
            }
        }
    }
    Ok(())
}

/// The edit list of an export: how far into the first fragment presentation
/// starts, and how long it lasts, so that the frames outside `range` in the
/// first and last fragments are not shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Edit {
    /// Ticks into the media where presentation starts.
    media_time: u64,
    /// Presentation length in the movie timescale (milliseconds).
    duration_ms: u64,
}

/// Computes the edit list from the in-range fragments; `None` when there are
/// none. Media time tracks the wall clock within milliseconds (`04 §7`), so
/// the trims are derived from the fragments' wall times.
fn edit_list(segments: &[SegmentFragments], range: TimeRange) -> Option<Edit> {
    let first = segments.first()?.records.first()?;
    let last = segments.last()?.records.last()?;
    let total: u64 = segments
        .iter()
        .flat_map(|s| s.records.iter())
        .map(|r| u64::from(r.duration))
        .sum();
    let head = ticks_between(first.wall, range.from);
    let last_end = last
        .wall
        .offset(MediaTicks::new(i64::from(last.duration)).as_micros());
    let tail = ticks_between(range.to, last_end);
    let shown = total.saturating_sub(head).saturating_sub(tail).max(1);
    Some(Edit {
        media_time: head.min(total.saturating_sub(1)),
        duration_ms: shown * u64::from(MOVIE_TIMESCALE) / u64::from(TIMESCALE),
    })
}

/// Media ticks from `from` to `to`, zero when `to` is not after `from`.
fn ticks_between(from: WallUs, to: WallUs) -> u64 {
    let micros = to.micros_since(from).max(0);
    u64::try_from(micros).map_or(0, |us| us * u64::from(TIMESCALE) / 1_000_000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use iris_catalog::{SegmentId, SegmentRow};
    use iris_core::id::{CameraId, StreamKey, StreamKind};
    use iris_mp4::index::IndexRecord;

    fn record(index: i64) -> IndexRecord {
        IndexRecord {
            wall: WallUs::new(1_700_000_000_000_000 + index * 2_000_000),
            duration: 180_000,
            ..IndexRecord::default()
        }
    }

    fn segment(records: Vec<IndexRecord>) -> SegmentFragments {
        SegmentFragments {
            row: SegmentRow {
                id: SegmentId(1),
                key: StreamKey {
                    camera: CameraId::try_new("cam1").unwrap(),
                    kind: StreamKind::Main,
                },
                path: "/nonexistent".into(),
                run_seq: 1,
                discontinuity: true,
                wall_start: records[0].wall,
                wall_end: records[0].wall,
                media_start: 0,
                media_end: 0,
                init_len: 0,
                durable_len: 0,
                fragments: 0,
                bytes: 0,
                sealed: true,
                codecs: String::new(),
                width: 0,
                height: 0,
                sample_entry_hash: 0,
            },
            records,
        }
    }

    #[test]
    fn the_edit_list_trims_both_ends_to_the_range() {
        let segments = vec![
            segment(vec![record(0), record(1)]),
            segment(vec![record(2)]),
        ];
        let from = record(0).wall.offset(500_000);
        let to = record(2).wall.offset(1_500_000);
        let edit = edit_list(&segments, TimeRange { from, to }).unwrap();
        assert_eq!(edit.media_time, 45_000);
        assert_eq!(edit.duration_ms, 5_000);
        // A range wider than the recording trims nothing.
        let wide = edit_list(
            &segments,
            TimeRange {
                from: record(0).wall.offset(-10_000_000),
                to: record(2).wall.offset(10_000_000),
            },
        )
        .unwrap();
        assert_eq!(
            wide,
            Edit {
                media_time: 0,
                duration_ms: 6_000
            }
        );
        assert_eq!(edit_list(&[], TimeRange { from, to }), None);
    }
}
