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
use iris_core::id::StreamKind;
use iris_core::time::{MediaTicks, TIMESCALE, WallUs};
use iris_mp4::export::{
    ExportError, MOVIE_TIMESCALE, TrackEdit, TrimmedStart, rebase_fragment, retrack_fragment,
    trim_fragment_end, trim_fragment_start, with_edit_lists,
};
use iris_mp4::fragment::FragmentHeader;
use iris_mp4::index::IndexRecord;
use iris_mp4::init::{AV_AUDIO_TRACK_ID, SampleEntry, build_av_init_segment, sample_entry_bytes};
use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};
use tokio::sync::mpsc;

use crate::error::ApiError;
use crate::hls::{
    MAX_FRAGMENT_BYTES, SegmentFragments, load_fragments, read_init, validate_fragment_record,
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
    let sound = if key.kind == StreamKind::Audio {
        Sound::Alone
    } else {
        Sound::None
    };
    Ok(streamed(segments, sound, range, init, &filename, permit))
}

/// `GET /api/cameras/{camera}/export.mp4`: the camera's main stream for a
/// range as one MP4 file, with the camera's recorded audio as a second track
/// when the range holds any (`docs/research/13-audio.md` §9). The video is
/// what the main stream's own export holds; each audio fragment follows the
/// video fragment whose span it starts in, on the same continuous timeline.
pub(crate) async fn camera_export(
    State(state): State<Arc<ApiState>>,
    Path(camera): Path<String>,
    Query(params): Query<TimeParams>,
) -> Result<Response, ApiError> {
    let permit = state.stats.admit_media(MediaClass::Streaming)?;
    let video_key = stream_key(&camera, "main")?;
    let audio_key = stream_key(&camera, "audio")?;
    let range = resolve_range(&params, WallUs::now())?;
    let video = load_fragments(&state, &video_key, range, &permit).await?;
    let Some(first) = video.first() else {
        return Err(ApiError::NotFound);
    };
    let audio = load_fragments(&state, &audio_key, range, &permit).await?;
    let mut init = compatible_init(&video, &permit).await?;
    if let Some(sound) = audio.first() {
        let audio_init = compatible_init(&audio, &permit).await?;
        let unreadable =
            || ApiError::Unavailable("a recording's sample entry is unreadable".into());
        let video_entry = SampleEntry::new(
            Bytes::copy_from_slice(sample_entry_bytes(&init).ok_or_else(unreadable)?),
            first.row.codecs.clone(),
            first.row.width,
            first.row.height,
        );
        let audio_entry = SampleEntry::audio(
            Bytes::copy_from_slice(sample_entry_bytes(&audio_init).ok_or_else(unreadable)?),
            sound.row.codecs.clone(),
        );
        init = build_av_init_segment(&video_entry, &audio_entry);
    }
    let c = range.from.to_civil();
    let filename = format!(
        "{}-{:04}{:02}{:02}T{:02}{:02}{:02}Z.mp4",
        video_key.camera, c.year, c.month, c.day, c.hour, c.minute, c.second
    );
    let sound = if audio.is_empty() {
        Sound::None
    } else {
        Sound::Beside(audio)
    };
    Ok(streamed(video, sound, range, init, &filename, permit))
}

/// What an export holds of sound.
enum Sound {
    /// None: the exported stream is pictures.
    None,
    /// The exported stream is itself sound: its start is cut where the
    /// export starts.
    Alone,
    /// A camera's audio beside its pictures, as a second track: these
    /// fragments, of which there is at least one.
    Beside(Vec<SegmentFragments>),
}

impl Sound {
    /// The edit of each track of the export, given the edit of its start: a
    /// video track's hides what precedes the start, and a sound track,
    /// which is cut there, says its length alone.
    fn edits(&self, start: &Edit) -> Vec<TrackEdit> {
        let hidden = TrackEdit {
            media_time: start.media_time,
            duration_ms: start.duration_ms,
        };
        let cut = TrackEdit {
            media_time: 0,
            duration_ms: start.duration_ms,
        };
        match self {
            Self::None => vec![hidden],
            Self::Alone => vec![cut],
            Self::Beside(_) => vec![hidden, cut],
        }
    }
}

/// The response that streams an export of `video` with `sound`, under
/// `init`.
fn streamed(
    video: Vec<SegmentFragments>,
    sound: Sound,
    range: TimeRange,
    init: Bytes,
    filename: &str,
    permit: MediaPermit,
) -> Response {
    // One bounded fragment can wait behind the one being delivered. Combined
    // with the fragment-length and process concurrency limits, slow clients
    // cannot multiply an unbounded byte queue.
    let (tx, mut rx) = mpsc::channel::<Result<Bytes, io::Error>>(1);
    let producer = media_admission::spawn(&permit, async move {
        if let Err(e) = stream_segments(video, sound, range, &init, &tx).await {
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
    (
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
        .into_response()
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
        let init = read_init(segment, permit).await?;
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

/// Writes the init section with an edit list trimming the export's start,
/// then every in-range fragment rebased onto a continuous timeline. Tail
/// sample bytes outside the range are omitted, with a crossing sample's
/// duration shortened, so end clipping does not depend on the player.
/// The exclusive end rounds up to a media tick to retain a positive tail.
/// Only the final fragment is trimmed: the wall anchor can step down inside a
/// run, so an earlier fragment's wall interval may reach past the end, and
/// the fragments after it need its last samples to decode.
///
/// A sound track's start is cut, not hidden by an edit: its samples that
/// start before the export does are left out and its timeline starts at
/// zero where the export starts, because a player in wide use misreads the
/// media time of an AAC track's edit (`docs/research/13-audio.md` §9). That
/// holds for a stream of sound alone ([`Sound::Alone`]) and for the audio
/// beside a video ([`Sound::Beside`]).
///
/// With audio beside the video, `init` holds two tracks. After a video fragment come the
/// audio fragments that start inside its span, on track
/// [`AV_AUDIO_TRACK_ID`], as far into the export as into the video; audio
/// that starts before the first video fragment or at or after the export's
/// end is left out, and an audio fragment that crosses the end is trimmed
/// there.
async fn stream_segments(
    segments: Vec<SegmentFragments>,
    sound: Sound,
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
    let init = with_edit_lists(init, &sound.edits(&edit)).map_err(io::Error::other)?;
    init_slot.send(Ok(Bytes::from(init)));
    // The wall time at which each video fragment after the first starts:
    // where the audio that follows the fragment before it ends.
    let next_starts: Vec<WallUs> = segments
        .iter()
        .flat_map(|seg| seg.records.iter().map(|record| record.wall))
        .skip(1)
        .collect();
    let mut next_starts = next_starts.into_iter();
    // Ticks of a lone sound track still to leave out before the export starts.
    let mut lead = match sound {
        Sound::Alone => edit.media_time,
        Sound::None | Sound::Beside(_) => 0,
    };
    let beside = match sound {
        Sound::Beside(audio) => audio,
        Sound::None | Sound::Alone => Vec::new(),
    };
    let mut sound = AudioCursor::new(beside, edit.media_time);
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
            let mut fragment = read_fragment(&mut file, &seg, record).await?;
            let mut wall = record.wall;
            let mut duration = u64::from(record.duration);
            if lead > 0 {
                let Some(TrimmedStart {
                    fragment: rest,
                    omitted_ticks: omitted,
                }) = without_start(fragment, lead)?
                else {
                    // No sample of it starts at or after the export's start.
                    decode_time = duration.saturating_sub(lead);
                    lead = lead.saturating_sub(duration);
                    continue;
                };
                fragment = rest;
                wall = wall.offset(ticks_as_micros(omitted)?);
                duration -= omitted;
                decode_time = omitted - lead;
                lead = 0;
            }
            let until = ticks_through(wall, range.to);
            if is_final && until < duration {
                if until == 0 {
                    break;
                }
                fragment = trim_fragment_end(
                    fragment,
                    FragmentHeader {
                        sequence,
                        decode_time,
                        wall,
                    },
                    MediaTicks::new(i64::try_from(until).map_err(io::Error::other)?),
                )
                .map_err(io::Error::other)?;
                duration = until;
            } else {
                rebase_fragment(&mut fragment, sequence, decode_time).map_err(io::Error::other)?;
            }
            sequence = sequence.wrapping_add(1);
            fragment_slot.send(Ok(Bytes::from(fragment)));
            let span = VideoSpan {
                wall: record.wall,
                stored: record.decode_time,
                decode_time,
                end: decode_time + duration,
                next_wall: next_starts.next(),
            };
            if !sound.follow(span, &mut sequence, tx).await? {
                return Ok(());
            }
            decode_time += duration;
        }
    }
    Ok(())
}

/// `fragment` from its first sample that starts at or after `skip` ticks
/// into it, with the ticks left out; `None` when no sample does, and for a
/// fragment that flags a sample as not a sync sample, which cannot lose its
/// start and is left out whole.
fn without_start(fragment: Vec<u8>, skip_ticks: u64) -> io::Result<Option<TrimmedStart>> {
    let skip = MediaTicks::new(i64::try_from(skip_ticks).map_err(io::Error::other)?);
    match trim_fragment_start(fragment, skip) {
        Ok(cut) => Ok(Some(cut)),
        Err(ExportError::InvalidDuration | ExportError::NotAllSync) => Ok(None),
        Err(error) => Err(io::Error::other(error)),
    }
}

/// Media ticks as microseconds of wall time.
fn ticks_as_micros(ticks: u64) -> io::Result<i64> {
    let ticks = i64::try_from(ticks).map_err(io::Error::other)?;
    Ok(MediaTicks::new(ticks).as_micros())
}

/// Reads and validates the stored fragment `record` names, opening the
/// segment's file at its first use.
async fn read_fragment(
    file: &mut Option<tokio::fs::File>,
    seg: &SegmentFragments,
    record: &IndexRecord,
) -> io::Result<Vec<u8>> {
    let file = match file {
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
    Ok(fragment)
}

/// One exported video fragment, for placing the audio that follows it.
#[derive(Clone, Copy, Debug)]
struct VideoSpan {
    /// Wall time of its first frame.
    wall: WallUs,
    /// Its decode time as stored.
    stored: u64,
    /// Its decode time in the export.
    decode_time: u64,
    /// Where it ends in the export, after trimming.
    end: u64,
    /// Wall time at which the next video fragment starts; `None` for the last.
    next_wall: Option<WallUs>,
}

impl VideoSpan {
    /// How far into this fragment `audio` starts, in ticks. Audio is stored
    /// on its video run's timeline, so the difference of the stored decode
    /// times is exact; when that disagrees with the wall times by more than
    /// a millisecond the two are not of one run, and the wall times decide.
    fn offset_of(&self, audio: &IndexRecord) -> u64 {
        let by_wall = ticks_between(self.wall, audio.wall);
        match audio.decode_time.checked_sub(self.stored) {
            Some(by_time) if by_time.abs_diff(by_wall) <= RUN_AGREEMENT_TICKS => by_time,
            _ => by_wall,
        }
    }
}

/// How far the stored decode times of a video fragment and an audio fragment
/// may disagree with their wall times and still be of one run: a millisecond.
const RUN_AGREEMENT_TICKS: u64 = 90;

/// The audio fragments of an export, taken in order as the video advances.
struct AudioCursor {
    segments: Vec<SegmentFragments>,
    segment: usize,
    record: usize,
    file: Option<tokio::fs::File>,
    /// Where the export starts on the video's export timeline, which is
    /// zero on the audio's.
    start: u64,
    /// Where the audio sent so far ends on its own timeline.
    end: u64,
}

impl AudioCursor {
    fn new(segments: Vec<SegmentFragments>, start: u64) -> Self {
        Self {
            segments,
            segment: 0,
            record: 0,
            file: None,
            start,
            end: 0,
        }
    }

    /// The next audio fragment's record, moving to the next file when one
    /// is exhausted.
    fn peek(&mut self) -> Option<IndexRecord> {
        loop {
            let seg = self.segments.get(self.segment)?;
            if let Some(record) = seg.records.get(self.record) {
                return Some(*record);
            }
            self.segment += 1;
            self.record = 0;
            self.file = None;
        }
    }

    /// Sends the audio fragments that start inside `span`, moved to the
    /// audio track, as far into the export as into the video. The start of
    /// a fragment that crosses the export's start is cut there, and where
    /// the video closes a gap between runs the audio follows what was sent
    /// before it. Returns `false` when the receiver is gone.
    async fn follow(
        &mut self,
        span: VideoSpan,
        sequence: &mut u32,
        tx: &mpsc::Sender<Result<Bytes, io::Error>>,
    ) -> io::Result<bool> {
        while let Some(record) = self.peek() {
            if span.next_wall.is_some_and(|next| record.wall >= next) {
                break;
            }
            self.record += 1;
            if record.wall < span.wall {
                // Before the first video fragment.
                continue;
            }
            let mut at = span.decode_time + span.offset_of(&record);
            let mut duration = u64::from(record.duration);
            if at >= span.end || at + duration <= self.start {
                // Past the video it would follow, or before the export starts.
                continue;
            }
            let Ok(slot) = tx.reserve().await else {
                return Ok(false);
            };
            let seg = &self.segments[self.segment];
            let mut fragment = read_fragment(&mut self.file, seg, &record).await?;
            let mut wall = record.wall;
            if at < self.start {
                let Some(TrimmedStart {
                    fragment: rest,
                    omitted_ticks: omitted,
                }) = without_start(fragment, self.start - at)?
                else {
                    continue;
                };
                fragment = rest;
                wall = wall.offset(ticks_as_micros(omitted)?);
                at += omitted;
                duration -= omitted;
                if at >= span.end {
                    continue;
                }
            }
            let decode_time = (at - self.start).max(self.end);
            let room = span.end - at;
            if span.next_wall.is_none() && room < duration {
                fragment = trim_fragment_end(
                    fragment,
                    FragmentHeader {
                        sequence: *sequence,
                        decode_time,
                        wall,
                    },
                    MediaTicks::new(i64::try_from(room).map_err(io::Error::other)?),
                )
                .map_err(io::Error::other)?;
                duration = room;
            } else {
                rebase_fragment(&mut fragment, *sequence, decode_time).map_err(io::Error::other)?;
            }
            retrack_fragment(&mut fragment, AV_AUDIO_TRACK_ID).map_err(io::Error::other)?;
            *sequence = sequence.wrapping_add(1);
            self.end = decode_time + duration;
            slot.send(Ok(Bytes::from(fragment)));
        }
        Ok(true)
    }
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
    use crate::hls::{MAX_INIT_BYTES, ValidatedMedia};
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
            stream_segments(vec![segment], Sound::None, range, init, &tx)
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
            let outcome = stream_segments(vec![published], Sound::None, range, &init, &tx).await;
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
        let mut producer = tokio::spawn(async move {
            stream_segments(vec![source], Sound::None, range, &init, &tx).await
        });

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
