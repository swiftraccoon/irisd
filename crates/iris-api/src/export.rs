//! Streamed fMP4 export of a time range.

use std::io::{self, SeekFrom};
use std::sync::Arc;
use std::task::Poll;

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use iris_catalog::TimeRange;
use iris_core::time::{MediaTicks, TIMESCALE, WallUs};
use iris_mp4::export::{MOVIE_TIMESCALE, rebase_fragment, trim_fragment_end, with_edit_list};
use iris_mp4::fragment::FragmentHeader;
use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};
use tokio::sync::mpsc;

use crate::error::ApiError;
use crate::hls::{
    MAX_FRAGMENT_BYTES, MAX_INIT_BYTES, SegmentFragments, load_fragments, validate_fragment_record,
};
use crate::media_admission::{self, MediaPermit};
use crate::query::{TimeParams, resolve_range};
use crate::segments::stream_key;
use crate::state::{ApiState, admitted_media_bytes};

pub(crate) async fn export(
    State(state): State<Arc<ApiState>>,
    Path((camera, kind)): Path<(String, String)>,
    Query(params): Query<TimeParams>,
) -> Result<Response, ApiError> {
    let permit = media_admission::shared(
        state
            .stats
            .try_media_response()
            .ok_or_else(|| ApiError::Unavailable("too many concurrent media responses".into()))?,
    );
    let key = stream_key(&camera, &kind)?;
    let range = resolve_range(&params, WallUs::now())?;
    let segments = load_fragments(&state, &key, range, &permit).await?;
    if segments.is_empty() {
        return Err(ApiError::NotFound);
    }
    let init = compatible_init(&segments, &permit).await?;
    let c = range.from.to_civil();
    let filename = format!(
        "{}-{}-{:04}{:02}{:02}T{:02}{:02}{:02}Z.mp4",
        key.camera, key.kind, c.year, c.month, c.day, c.hour, c.minute, c.second
    );
    // One bounded fragment can wait behind the one being delivered. Combined
    // with the fragment-length and process concurrency limits, slow clients
    // cannot multiply an unbounded byte queue.
    let (tx, mut rx) = mpsc::channel::<Result<Bytes, io::Error>>(1);
    let producer = media_admission::spawn(&permit, async move {
        if let Err(e) = stream_segments(segments, range, &init, &tx).await {
            let _ = tx.send(Err(e)).await;
        }
    });
    // The response stream owns one permit reference and the deliberately
    // detached producer owns another until its admitted work has ended.
    drop(producer);
    let stream = futures_util::stream::poll_fn(move |cx| match rx.poll_recv(cx) {
        Poll::Ready(Some(Ok(bytes))) => {
            Poll::Ready(Some(Ok(admitted_media_bytes(bytes, Arc::clone(&permit)))))
        }
        other => other,
    });
    Ok((
        [
            (header::CONTENT_TYPE, "video/mp4".to_owned()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            ),
            (header::CACHE_CONTROL, "private, no-store".to_owned()),
        ],
        Body::from_stream(stream),
    )
        .into_response())
}

/// Checks the actual init bytes because adopted files may lack catalog codec hashes.
/// This finishes before headers are sent, so incompatible files cannot produce a
/// partially streamed MP4 under the first file's sample description.
async fn compatible_init(
    segments: &[SegmentFragments],
    permit: &MediaPermit,
) -> Result<Bytes, ApiError> {
    let mut expected: Option<Vec<u8>> = None;
    for segment in segments {
        if u64::from(segment.row.init_len) > MAX_INIT_BYTES
            || u64::from(segment.row.init_len) > segment.extent
        {
            return Err(ApiError::Unavailable(
                "export initialization exceeds the serving budget or published extent".into(),
            ));
        }
        let path = segment.row.path.clone();
        let init_len = segment.row.init_len;
        let init = media_admission::spawn(permit, async move {
            let mut file = tokio::fs::File::open(path).await?;
            let mut init = Vec::new();
            init.try_reserve_exact(init_len as usize)
                .map_err(|error| io::Error::other(error.to_string()))?;
            init.resize(init_len as usize, 0);
            file.read_exact(&mut init).await?;
            Ok::<_, io::Error>(init)
        })
        .await
        .map_err(io::Error::other)??;
        if expected.as_ref().is_some_and(|expected| expected != &init) {
            return Err(ApiError::BadRequest(
                "export range crosses a codec configuration change; request one configuration at a time"
                    .into(),
            ));
        }
        expected = Some(init);
    }
    expected.map(Bytes::from).ok_or(ApiError::NotFound)
}

/// Writes the first segment's init section with an edit list trimming the
/// export's start, then every in-range fragment rebased onto a continuous
/// timeline. Tail sample bytes outside the range are omitted, with a crossing
/// sample's duration shortened, so end clipping does not depend on the player.
/// The exclusive end rounds up to a media tick to retain a positive tail.
async fn stream_segments(
    segments: Vec<SegmentFragments>,
    range: TimeRange,
    init: &[u8],
    tx: &mpsc::Sender<Result<Bytes, io::Error>>,
) -> io::Result<()> {
    let Some(edit) = edit_list(&segments, range) else {
        return Ok(());
    };
    let Ok(init_slot) = tx.reserve().await else {
        return Ok(());
    };
    let init = with_edit_list(init, edit.media_time, edit.duration_ms).map_err(io::Error::other)?;
    init_slot.send(Ok(Bytes::from(init)));
    let mut sequence = 1u32;
    let mut decode_time = 0u64;
    for seg in segments {
        let mut file = None;
        for record in &seg.records {
            let Ok(fragment_slot) = tx.reserve().await else {
                return Ok(());
            };
            let file = match &mut file {
                Some(file) => file,
                None => file.insert(tokio::fs::File::open(&seg.row.path).await?),
            };
            if u64::from(record.len) > MAX_FRAGMENT_BYTES || record.end() > seg.extent {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "export fragment exceeds serving bounds",
                ));
            }
            file.seek(SeekFrom::Start(record.offset)).await?;
            let mut fragment = Vec::new();
            fragment
                .try_reserve_exact(record.len as usize)
                .map_err(|error| io::Error::other(error.to_string()))?;
            fragment.resize(record.len as usize, 0);
            file.read_exact(&mut fragment).await?;
            validate_fragment_record(&fragment, record)?;
            let until = ticks_through(record.wall, range.to);
            let mut duration = u64::from(record.duration);
            if until < u64::from(record.duration) {
                fragment = trim_fragment_end(
                    fragment,
                    FragmentHeader {
                        sequence,
                        decode_time,
                        wall: record.wall,
                    },
                    MediaTicks::new(i64::try_from(until).map_err(io::Error::other)?),
                )
                .map_err(io::Error::other)?;
                duration = until;
            } else {
                rebase_fragment(&mut fragment, sequence, decode_time).map_err(io::Error::other)?;
            }
            sequence = sequence.wrapping_add(1);
            decode_time += duration;
            fragment_slot.send(Ok(Bytes::from(fragment)));
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
    // Match the serialized tail before deriving the edit: rounding the omitted
    // tail separately can put its end a tick beyond the available media.
    let last_duration = ticks_through(last.wall, range.to).min(u64::from(last.duration));
    let clipped_total = total - u64::from(last.duration) + last_duration;
    let head = ticks_between(first.wall, range.from).min(clipped_total.saturating_sub(1));
    let shown = clipped_total.saturating_sub(head).max(1);
    Some(Edit {
        media_time: head,
        duration_ms: (shown * u64::from(MOVIE_TIMESCALE)).div_ceil(u64::from(TIMESCALE)),
    })
}

/// Media ticks from `from` to `to`, zero when `to` is not after `from`.
fn ticks_between(from: WallUs, to: WallUs) -> u64 {
    let micros = (i128::from(to.get()) - i128::from(from.get())).max(0);
    u64::try_from(micros * i128::from(TIMESCALE) / 1_000_000).unwrap_or(u64::MAX)
}

/// Media ticks through the exclusive end, rounded up so a positive overlap
/// retains the crossing sample with at least one tick of duration.
fn ticks_through(from: WallUs, to: WallUs) -> u64 {
    let micros = (i128::from(to.get()) - i128::from(from.get())).max(0);
    u64::try_from((micros * i128::from(TIMESCALE) + 999_999) / 1_000_000).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use iris_catalog::{SegmentId, SegmentRow};
    use iris_core::id::{CameraId, StreamKey, StreamKind};
    use iris_mp4::index::IndexRecord;

    #[tokio::test]
    async fn export_omits_samples_starting_at_or_after_the_requested_end() {
        use iris_mp4::fragment::{FragmentHeader, Sample, encode_fragment};
        use iris_mp4::init::{SampleEntry, build_init_segment};
        use iris_mp4::reader::parse_fragment;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recording.mp4");
        let entry = SampleEntry::new(
            Bytes::from_static(b"\0\0\0\x10hvc1\0\0\0\0\0\0\0\0"),
            "hvc1".into(),
            320,
            180,
        );
        let init = build_init_segment(&entry);
        let mut bytes = init.to_vec();
        let base = 1_700_000_000_000_000;
        let mut records = Vec::new();
        for second in 1..=4 {
            let samples: Vec<Sample> = (0..20)
                .map(|frame| Sample {
                    data: Bytes::from(vec![0, 0, 0, 2, if frame == 0 { 0x26 } else { 0x02 }, 1]),
                    duration: 4500,
                    is_sync: frame == 0,
                })
                .collect();
            let encoded = encode_fragment(
                &FragmentHeader {
                    sequence: second as u32,
                    decode_time: second * 90_000,
                    wall: WallUs::new(base + second as i64 * 1_000_000),
                },
                &samples,
            )
            .unwrap();
            let fragment: Vec<u8> = encoded
                .io_slices(&samples)
                .iter()
                .flat_map(|s| s.iter().copied())
                .collect();
            let mut record = IndexRecord::from_scan(&parse_fragment(&fragment).unwrap());
            record.offset = bytes.len() as u64;
            records.push(record);
            bytes.extend_from_slice(&fragment);
        }
        std::fs::write(&path, bytes).unwrap();
        let mut segment = segment(records);
        segment.row.path = path;
        let range = TimeRange {
            from: WallUs::new(base + 1_250_000),
            to: WallUs::new(base + 4_750_000),
        };
        let (tx, mut rx) = mpsc::channel(8);
        stream_segments(vec![segment], range, &init, &tx)
            .await
            .unwrap();
        drop(tx);
        let _init = rx.recv().await.unwrap().unwrap();
        let mut total = 0;
        while let Some(fragment) = rx.recv().await {
            let fragment = fragment.unwrap();
            let info = parse_fragment(&fragment).unwrap();
            let mut wall = info.wall.unwrap().get();
            for duration in info.sample_durations {
                assert!(
                    wall < range.to.get(),
                    "export includes sample at {wall}, cutoff {}",
                    range.to.get()
                );
                wall += i64::from(duration) * 1_000_000 / 90_000;
                total += 1;
            }
        }
        assert_eq!(
            total, 75,
            "five leading samples remain as sync preroll, five tail samples are omitted"
        );
    }

    fn record(index: i64) -> IndexRecord {
        IndexRecord {
            wall: WallUs::new(1_700_000_000_000_000 + index * 2_000_000),
            duration: 180_000,
            ..IndexRecord::default()
        }
    }

    fn segment(records: Vec<IndexRecord>) -> SegmentFragments {
        let extent = records.last().map_or(0, IndexRecord::end);
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
            extent,
            discontinuity_before: true,
        }
    }

    #[tokio::test]
    async fn export_reserves_queue_capacity_before_opening_the_next_fragment() {
        use iris_mp4::init::{SampleEntry, build_init_segment};

        let entry = SampleEntry::new(
            Bytes::from_static(b"\0\0\0\x10hvc1\0\0\0\0\0\0\0\0"),
            "hvc1".into(),
            16,
            16,
        );
        let init = build_init_segment(&entry);
        let record = record(0);
        let range = TimeRange {
            from: record.wall,
            to: record.wall.offset(1_000_000),
        };
        let directory = tempfile::tempdir().expect("temporary directory");
        let mut source = segment(vec![record]);
        source.row.path = directory.path().join("missing.mp4");
        let (tx, mut rx) = mpsc::channel(1);
        let mut producer =
            tokio::spawn(async move { stream_segments(vec![source], range, &init, &tx).await });

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut producer)
                .await
                .is_err(),
            "the producer must wait for capacity before opening the missing media file"
        );
        rx.recv()
            .await
            .expect("initialization queued")
            .expect("initialization valid");
        let error = producer
            .await
            .expect("producer task")
            .expect_err("missing media file is opened after capacity is available");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
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
