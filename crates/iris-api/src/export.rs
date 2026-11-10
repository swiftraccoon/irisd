//! Streamed fMP4 export of a time range.

use std::io::{self, SeekFrom};
use std::os::unix::fs::FileExt as _;
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
use crate::state::{ApiState, MediaClass};

pub(crate) async fn export(
    State(state): State<Arc<ApiState>>,
    Path((camera, kind)): Path<(String, String)>,
    Query(params): Query<TimeParams>,
) -> Result<Response, ApiError> {
    let permit = state.stats.admit_media(MediaClass::Streaming)?;
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
            Poll::Ready(Some(Ok(media_admission::attach(bytes, &permit))))
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
/// partially streamed MP4 under the first file's sample description. Each file is
/// reopened beneath its root and must still be the validated inode.
async fn compatible_init(
    segments: &[SegmentFragments],
    permit: &MediaPermit,
) -> Result<Bytes, ApiError> {
    let mut expected: Option<Vec<u8>> = None;
    for segment in segments {
        if u64::from(segment.row.init_len) > MAX_INIT_BYTES
            || u64::from(segment.row.init_len) > segment.media.extent()
        {
            return Err(ApiError::Unavailable(
                "export initialization exceeds the serving budget or published extent".into(),
            ));
        }
        let media = segment.media.clone();
        let init_len = segment.row.init_len;
        let init = media_admission::blocking(permit, move || {
            let file = media.reopen()?;
            let mut init = Vec::new();
            init.try_reserve_exact(init_len as usize)
                .map_err(io::Error::other)?;
            init.resize(init_len as usize, 0);
            file.read_exact_at(&mut init, 0)?;
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
/// Only the final fragment is trimmed: the wall anchor can step down inside a
/// run, so an earlier fragment's wall interval may reach past the end, and
/// the fragments after it need its last samples to decode.
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
    let last_segment = segments.len() - 1;
    for (segment_index, seg) in segments.into_iter().enumerate() {
        let mut file = None;
        let last_record = seg.records.len().saturating_sub(1);
        for (record_index, record) in seg.records.iter().enumerate() {
            let is_final = segment_index == last_segment && record_index == last_record;
            let Ok(fragment_slot) = tx.reserve().await else {
                return Ok(());
            };
            let file = match &mut file {
                Some(file) => file,
                None => file.insert(seg.media.open().await?),
            };
            if u64::from(record.len) > MAX_FRAGMENT_BYTES || record.end() > seg.media.extent() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "export fragment exceeds serving bounds",
                ));
            }
            file.seek(SeekFrom::Start(record.offset)).await?;
            let mut fragment = Vec::new();
            fragment
                .try_reserve_exact(record.len as usize)
                .map_err(io::Error::other)?;
            fragment.resize(record.len as usize, 0);
            file.read_exact(&mut fragment).await?;
            validate_fragment_record(&fragment, record)?;
            let until = ticks_through(record.wall, range.to);
            let mut duration = u64::from(record.duration);
            if is_final && until < u64::from(record.duration) {
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
    use std::time::Duration;

    use super::*;
    use crate::hls::ValidatedMedia;
    use iris_catalog::{SegmentId, SegmentRow};
    use iris_core::id::{CameraId, StreamKey, StreamKind};
    use iris_mp4::index::IndexRecord;

    const BASE: i64 = 1_700_000_000_000_000;

    /// A recording of one-second fragments of twenty samples, one per entry
    /// of `fragments`: its wall time in microseconds after [`BASE`] and
    /// whether it starts with a sync sample. Returns it with its init bytes.
    fn recording(
        directory: &std::path::Path,
        fragments: &[(i64, bool)],
    ) -> (SegmentFragments, Bytes) {
        use iris_mp4::fragment::{FragmentHeader, Sample, encode_fragment};
        use iris_mp4::init::{SampleEntry, build_init_segment};
        use iris_mp4::reader::parse_fragment;

        let entry = SampleEntry::new(
            Bytes::from_static(b"\0\0\0\x10hvc1\0\0\0\0\0\0\0\0"),
            "hvc1".into(),
            320,
            180,
        );
        let init = build_init_segment(&entry);
        let mut bytes = init.to_vec();
        let mut records = Vec::new();
        for (index, &(wall, sync)) in fragments.iter().enumerate() {
            let samples: Vec<Sample> = (0..20)
                .map(|frame| {
                    let is_sync = sync && frame == 0;
                    Sample {
                        data: Bytes::from(vec![0, 0, 0, 2, if is_sync { 0x26 } else { 0x02 }, 1]),
                        duration: 4500,
                        is_sync,
                    }
                })
                .collect();
            let encoded = encode_fragment(
                &FragmentHeader {
                    sequence: index as u32 + 1,
                    decode_time: index as u64 * 90_000,
                    wall: WallUs::new(BASE + wall),
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
        let mut segment = segment(directory, records);
        let extent = segment.media.extent();
        segment.media = ValidatedMedia::for_test(directory, &bytes, extent);
        segment.row.path = segment.media.path().to_path_buf();
        (segment, init)
    }

    /// Exports `segment` over `range` and returns the parsed fragments.
    async fn exported(
        segment: SegmentFragments,
        init: &[u8],
        range: TimeRange,
    ) -> Vec<iris_mp4::reader::FragmentInfo> {
        // The producer waits for queue room, so it runs beside the reader.
        let (tx, mut rx) = mpsc::channel(1);
        let produce = async move {
            stream_segments(vec![segment], range, init, &tx)
                .await
                .unwrap();
        };
        let consume = async {
            let _init = rx.recv().await.unwrap().unwrap();
            let mut fragments = Vec::new();
            while let Some(fragment) = rx.recv().await {
                fragments.push(iris_mp4::reader::parse_fragment(&fragment.unwrap()).unwrap());
            }
            fragments
        };
        tokio::join!(produce, consume).1
    }

    #[tokio::test]
    async fn export_omits_samples_starting_at_or_after_the_requested_end() {
        let dir = tempfile::tempdir().unwrap();
        let (segment, init) = recording(
            dir.path(),
            &[
                (1_000_000, true),
                (2_000_000, true),
                (3_000_000, true),
                (4_000_000, true),
            ],
        );
        let range = TimeRange {
            from: WallUs::new(BASE + 1_250_000),
            to: WallUs::new(BASE + 4_750_000),
        };
        let mut total = 0;
        for info in exported(segment, &init, range).await {
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

    /// The wall anchor can step down inside a run, so a fragment's wall
    /// interval may reach past the start of the one after it, or begin after
    /// it. Such a fragment before the final one is exported whole and on one
    /// continuous timeline, because the fragments after it depend on it.
    #[tokio::test]
    async fn export_trims_only_the_final_fragment_when_wall_intervals_overlap() {
        // The second fragment covers 2.0 s to 3.0 s and the third, a
        // continuation, starts at 2.5 s; then the second starts after the end.
        for second_wall in [2_000_000, 3_000_000] {
            let dir = tempfile::tempdir().unwrap();
            let (segment, init) = recording(
                dir.path(),
                &[(1_000_000, true), (second_wall, true), (2_500_000, false)],
            );
            let range = TimeRange {
                from: WallUs::new(BASE + 1_000_000),
                to: WallUs::new(BASE + 2_800_000),
            };
            let edit = edit_list(std::slice::from_ref(&segment), range).unwrap();
            let fragments = exported(segment, &init, range).await;
            let samples: Vec<usize> = fragments
                .iter()
                .map(|info| info.sample_durations.len())
                .collect();
            assert_eq!(samples, [20, 20, 6], "second fragment at {second_wall}");
            let mut decode_time = 0;
            for info in &fragments {
                assert_eq!(info.decode_time, decode_time);
                decode_time += u64::from(info.duration);
            }
            assert_eq!(decode_time, 207_000);
            assert_eq!(edit.duration_ms, 2_300);
        }
    }

    /// The initialization section and every fragment must lie inside the
    /// extent that was published with the records; one byte past it is
    /// refused before anything of that part is read.
    #[tokio::test]
    async fn an_export_refuses_parts_past_the_published_extent() {
        let dir = tempfile::tempdir().unwrap();
        let (mut segment, init) = recording(dir.path(), &[(1_000_000, true), (2_000_000, true)]);
        let bytes = std::fs::read(segment.media.path()).unwrap();
        let extent = segment.media.extent();
        let stats = crate::state::ApiStats::new_with_media_limits(1, 1);
        let permit = stats.admit_media(MediaClass::Streaming).unwrap();
        let init_len = u32::try_from(init.len()).unwrap();

        segment.row.init_len = init_len;
        let read = compatible_init(std::slice::from_ref(&segment), &permit)
            .await
            .unwrap();
        assert_eq!(read, init);
        // An initialization length that reaches past the extent, and one
        // past the serving budget under an extent that would admit it.
        segment.media = ValidatedMedia::for_test(dir.path(), &bytes, u64::from(init_len) - 1);
        assert!(matches!(
            compatible_init(std::slice::from_ref(&segment), &permit).await,
            Err(ApiError::Unavailable(_))
        ));
        segment.media = ValidatedMedia::for_test(dir.path(), &bytes, u64::MAX);
        segment.row.init_len = u32::try_from(MAX_INIT_BYTES).unwrap() + 1;
        assert!(matches!(
            compatible_init(std::slice::from_ref(&segment), &permit).await,
            Err(ApiError::Unavailable(_))
        ));

        // The last fragment ends at the extent: one byte less refuses it.
        let range = TimeRange {
            from: WallUs::new(BASE),
            to: WallUs::new(BASE + 10_000_000),
        };
        for (published, accepted) in [(extent, true), (extent - 1, false)] {
            let published = SegmentFragments {
                row: segment.row.clone(),
                records: segment.records.clone(),
                media: ValidatedMedia::for_test(dir.path(), &bytes, published),
                discontinuity_before: true,
            };
            let (tx, mut rx) = mpsc::channel(8);
            let outcome = stream_segments(vec![published], range, &init, &tx).await;
            drop(tx);
            let mut sent = 0;
            while rx.recv().await.is_some() {
                sent += 1;
            }
            if accepted {
                outcome.unwrap();
                assert_eq!(sent, 3);
            } else {
                assert_eq!(outcome.unwrap_err().kind(), io::ErrorKind::InvalidData);
                assert_eq!(sent, 2, "the init and the first fragment");
            }
        }
    }

    /// A small deterministic generator (xorshift64*), so a failure
    /// reproduces from its seed.
    struct Rng(u64);

    impl Rng {
        /// A value in `0..n`.
        fn below(&mut self, n: u64) -> i64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            i64::try_from(self.0.wrapping_mul(0x2545_f491_4f6c_dd1d) % n).unwrap()
        }
    }

    /// Over generated recordings whose wall anchor steps down now and then,
    /// and generated ranges: the export holds every selected fragment once on
    /// a continuous timeline from zero, opens at a sync sample, keeps every
    /// fragment but the last whole, cuts the last where the range ends, and
    /// carries an edit that starts at the range's start and stays inside the
    /// serialized media.
    #[tokio::test]
    async fn generated_exports_are_continuous_and_cut_only_at_the_end() {
        for seed in 1..=150 {
            let mut rng = Rng(seed);
            let count = 2 + rng.below(7);
            let mut step = 0;
            let fragments: Vec<(i64, bool)> = (0..count)
                .map(|index| {
                    if rng.below(3) == 0 {
                        step += rng.below(300_000);
                    }
                    (
                        index * 1_000_000 + 1_000_000 - step,
                        index == 0 || rng.below(5) < 3,
                    )
                })
                .collect();
            let dir = tempfile::tempdir().unwrap();
            let (mut segment, init) = recording(dir.path(), &fragments);
            let from = BASE + rng.below(u64::try_from(count).unwrap() * 1_000_000 + 1_500_000);
            let range = TimeRange {
                from: WallUs::new(from),
                to: WallUs::new(from + 1 + rng.below(4_000_000)),
            };
            segment.records = crate::hls::clip_records(&segment.records, &range);
            let selected = segment.records.clone();
            let Some(last) = selected.last() else {
                continue;
            };
            let context = format!("seed {seed}: {fragments:?} over {range:?}");
            let edit = edit_list(std::slice::from_ref(&segment), range).unwrap();
            let exported = exported(segment, &init, range).await;

            assert_eq!(exported.len(), selected.len(), "{context}");
            assert!(exported[0].starts_with_sync, "{context}");
            let mut decode_time = 0;
            for (index, (info, record)) in exported.iter().zip(&selected).enumerate() {
                assert_eq!(info.sequence as usize, index + 1, "{context}");
                assert_eq!(info.decode_time, decode_time, "{context}");
                assert_eq!(info.wall, Some(record.wall), "{context}");
                decode_time += u64::from(info.duration);
                if index + 1 < selected.len() {
                    assert_eq!(info.sample_durations.len(), 20, "{context}");
                }
            }
            // The last fragment keeps the samples that start before the end.
            let until = ticks_through(last.wall, range.to);
            let kept = usize::try_from(until.div_ceil(4500).min(20)).unwrap();
            let tail = exported.last().unwrap();
            assert_eq!(tail.sample_durations.len(), kept, "{context}");
            assert_eq!(u64::from(tail.duration), until.min(90_000), "{context}");

            // The edit starts where the range does, or at the first frame
            // when the range starts before it, and ends inside the media.
            let head = ticks_between(selected[0].wall, range.from);
            assert_eq!(edit.media_time, head.min(decode_time - 1), "{context}");
            let shown = edit.duration_ms * 90;
            assert!(shown >= 1, "{context}");
            assert!(
                edit.media_time + shown < decode_time + 90,
                "{context}: the edit runs past the media"
            );
        }
    }

    fn record(index: i64) -> IndexRecord {
        IndexRecord {
            wall: WallUs::new(1_700_000_000_000_000 + index * 2_000_000),
            duration: 180_000,
            ..IndexRecord::default()
        }
    }

    /// A segment whose media is an empty validated recording beneath a fresh
    /// root in `directory`, with the extent its records describe.
    fn segment(directory: &std::path::Path, records: Vec<IndexRecord>) -> SegmentFragments {
        let extent = records.last().map_or(0, IndexRecord::end);
        let media = ValidatedMedia::for_test(directory, &[], extent);
        SegmentFragments {
            row: SegmentRow {
                id: SegmentId(1),
                key: StreamKey {
                    camera: CameraId::try_new("cam1").unwrap(),
                    kind: StreamKind::Main,
                },
                path: media.path().to_path_buf(),
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
                sample_entry_id: iris_catalog::SampleEntryId(1),
            },
            records,
            media,
            discontinuity_before: true,
        }
    }

    #[tokio::test(start_paused = true)]
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
        let source = segment(directory.path(), vec![record]);
        std::fs::remove_file(source.media.path()).expect("remove the validated recording");
        let (tx, mut rx) = mpsc::channel(1);
        let mut producer =
            tokio::spawn(async move { stream_segments(vec![source], range, &init, &tx).await });

        // Under paused time the deadline fires as soon as the producer is idle,
        // so this proves it is blocked on the queue and not on time.
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut producer)
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

    fn three_fragments(directory: &std::path::Path) -> Vec<SegmentFragments> {
        vec![
            segment(directory, vec![record(0), record(1)]),
            segment(directory, vec![record(2)]),
        ]
    }

    #[test]
    fn the_edit_list_trims_both_ends_to_the_range() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let segments = three_fragments(directory.path());
        let from = record(0).wall.offset(500_000);
        let to = record(2).wall.offset(1_500_000);
        let edit = edit_list(&segments, TimeRange { from, to }).unwrap();
        assert_eq!(edit.media_time, 45_000);
        assert_eq!(edit.duration_ms, 5_000);
    }

    #[test]
    fn a_range_wider_than_the_recording_trims_nothing() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let segments = three_fragments(directory.path());
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
    }

    #[test]
    fn an_empty_selection_has_no_edit_list() {
        let from = record(0).wall.offset(500_000);
        let to = record(2).wall.offset(1_500_000);
        assert_eq!(edit_list(&[], TimeRange { from, to }), None);
    }
}
